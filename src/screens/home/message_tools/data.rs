//! The message tools' behaviour: turning the search box into API requests,
//! paging results and pins, and pinning from the message menu.
//!
//! Everything here is driven by discrete events — a keystroke's debounce, a
//! click, a conversation switch. Nothing polls. Each request carries the epoch
//! it was started under and is dropped at arrival when the state has moved on.

use std::collections::HashSet;

// Explicit types: a `gpui::*` glob here would leak gpui's `test` macro into
// this file's `use super::*` tests, shadowing the builtin attribute.
use gpui::{Context, Window};
use gpui_component::input::InputEvent;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
};

use crate::discord::MemberListItem;
use crate::discord::Permissions;
use crate::discord::message_tools::{
    MAX_SEARCH_OFFSET, SEARCH_PAGE_SIZE, SearchHas, SearchOutcome, SearchRequest, fetch_pins,
    pin_message, search_messages, snowflake_from_unix_ms, unpin_message,
};
use crate::screens::home::state::View;
use crate::screens::home::{HomeScreen, MessageTarget};

use super::{
    MAX_PINS, MAX_SEARCH_HITS, MAX_SEARCH_RETRIES, PinMenuAction, PinsState, SEARCH_DEBOUNCE,
    SearchScope, SearchState, ToolsPanel,
};

/// The permission bits that let a member pin in a guild channel: the current
/// `PIN_MESSAGES`, and `MANAGE_MESSAGES` for guilds that haven't been through
/// the pin-permission migration yet. Either is accepted, like Discord's own
/// client does while the migration rolls out.
const CAN_PIN: Permissions = Permissions::PIN_MESSAGES.union(Permissions::MANAGE_MESSAGES);

/// What the screen contributes to resolving a query: who the user is, and the
/// names `from:`/`in:` may be matched against. All resolution is local — the
/// API takes ids, never names.
pub(super) struct SearchResolveCtx {
    pub self_id: Option<Id<UserMarker>>,
    /// Sidebar channels of the open guild, for `in:` names.
    pub channels: Vec<(Id<ChannelMarker>, String)>,
    /// Names `from:` may match: the open conversation's authors in a DM, the
    /// loaded member list in a guild.
    pub authors: Vec<(Id<UserMarker>, String)>,
}

/// The search box's query split into operators and free text, before any name
/// is resolved to an id.
#[derive(Default)]
struct ParsedQuery {
    content: String,
    from: Vec<String>,
    in_: Vec<String>,
    has: Vec<SearchHas>,
    invalid_has: Vec<String>,
    before: Option<String>,
    after: Option<String>,
    unknown: Vec<String>,
}

/// Splits `raw` into `from:`/`in:`/`has:`/`before:`/`after:` operators and the
/// remaining free text. Operator names are case-insensitive, like Discord's.
fn parse_query(raw: &str) -> ParsedQuery {
    let mut parsed = ParsedQuery::default();
    let mut content = Vec::new();
    for token in raw.split_whitespace() {
        let Some((operator, value)) = token.split_once(':') else {
            content.push(token);
            continue;
        };
        let value = value.trim_matches('"');
        match operator.to_ascii_lowercase().as_str() {
            "from" => parsed.from.push(value.to_string()),
            "in" => parsed.in_.push(value.to_string()),
            "has" => match SearchHas::parse(value) {
                Some(has) => parsed.has.push(has),
                None => parsed.invalid_has.push(value.to_string()),
            },
            "before" => parsed.before = Some(value.to_string()),
            "after" => parsed.after = Some(value.to_string()),
            _ => parsed.unknown.push(operator.to_string()),
        }
    }
    parsed.content = content.join(" ");
    parsed
}

/// Resolves a parsed query into a request, or a reason it can't be one. Every
/// error here is shown to the user as-is, so they read like advice.
fn resolve_request(
    parsed: ParsedQuery,
    scope: SearchScope,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
    ctx: &SearchResolveCtx,
) -> Result<SearchRequest, String> {
    if !parsed.unknown.is_empty() {
        return Err(format!(
            "unsupported filter{}: {} — supported: from:, in:, has:, before:, after:",
            if parsed.unknown.len() == 1 { "" } else { "s" },
            parsed
                .unknown
                .iter()
                .map(|op| format!("{op}:"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(value) = parsed.invalid_has.first() {
        return Err(format!(
            "has:{value} is not a filter — has: takes image, sound, video, file, \
             sticker, embed, link, poll or snapshot"
        ));
    }
    if parsed.content.is_empty()
        && parsed.from.is_empty()
        && parsed.in_.is_empty()
        && parsed.has.is_empty()
        && parsed.before.is_none()
        && parsed.after.is_none()
    {
        return Err("type what to search for".into());
    }

    let mut author_ids = Vec::new();
    for from in &parsed.from {
        author_ids.push(resolve_from(from, ctx)?);
    }
    let mut channel_ids = Vec::new();
    if scope == SearchScope::Guild {
        for in_ in &parsed.in_ {
            channel_ids.push(resolve_in(in_, ctx)?);
        }
    } else if !parsed.in_.is_empty() {
        return Err("in: applies when searching a server, not a conversation".into());
    }

    Ok(SearchRequest {
        guild_id: (scope == SearchScope::Guild).then_some(guild_id).flatten(),
        channel_id,
        content: parsed.content,
        author_ids,
        channel_ids,
        has: parsed.has,
        min_id: parsed
            .after
            .as_deref()
            .map(|value| anchor_id(value, false))
            .transpose()?,
        max_id: parsed
            .before
            .as_deref()
            .map(|value| anchor_id(value, true))
            .transpose()?,
        offset: 0,
    })
}

/// `from:` — `me`, a user id, or a name among the authors already known.
fn resolve_from(value: &str, ctx: &SearchResolveCtx) -> Result<Id<UserMarker>, String> {
    if value.eq_ignore_ascii_case("me") {
        return ctx
            .self_id
            .ok_or_else(|| "your session hasn't loaded your user yet".into());
    }
    if let Some(id) = value.parse::<u64>().ok().and_then(Id::new_checked) {
        return Ok(id);
    }
    ctx.authors
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(value))
        .map(|(id, _)| *id)
        .ok_or_else(|| format!("from:{value} matched nobody loaded — use a user id or me"))
}

/// `in:` — a channel id or a name among the guild's channels.
fn resolve_in(value: &str, ctx: &SearchResolveCtx) -> Result<Id<ChannelMarker>, String> {
    if let Some(id) = value.parse::<u64>().ok().and_then(Id::new_checked) {
        return Ok(id);
    }
    ctx.channels
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(value))
        .map(|(id, _)| *id)
        .ok_or_else(|| format!("in:{value} matched no channel in this server"))
}

/// `before:`/`after:` — a snowflake, or a YYYY-MM-DD date converted into the
/// snowflake that marks it: the day's start for `after`, the final millisecond
/// of the preceding day for `before`.
fn anchor_id(value: &str, before: bool) -> Result<u64, String> {
    if let Some(id) = value
        .parse::<u64>()
        .ok()
        .and_then(Id::<MessageMarker>::new_checked)
    {
        return Ok(id.get());
    }
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("{value} is not a message id or a YYYY-MM-DD date"))?;
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis();
    Ok(snowflake_from_unix_ms(if before {
        midnight - 1
    } else {
        midnight
    }))
}

/// Whether a page of `page_len` hits ends a search of `total` results.
fn page_complete(page_len: usize, total: u64, next_offset: usize) -> bool {
    page_len < SEARCH_PAGE_SIZE || next_offset as u64 >= total || next_offset > MAX_SEARCH_OFFSET
}

/// Turns a REST failure into what the pin panel says. A 404 after an unpin is
/// the pin already being gone — which is where the user wanted it.
fn pin_error_note(err: &str) -> String {
    if err.contains("404") || err.contains("Unknown") {
        "That message is no longer pinned.".into()
    } else if err.contains("403") || err.contains("Missing Permissions") {
        "You don't have permission to pin messages here.".into()
    } else {
        err.to_string()
    }
}

/// Extends an in-memory list with one page, cut to its ceiling. Returns the
/// list length afterwards, how many rows to announce to its virtualized list
/// (`splice(old..old, added)`), and whether the ceiling cut results off.
fn extend_capped(old_len: usize, page_len: usize, max: usize) -> (usize, usize, bool) {
    let requested = old_len.saturating_add(page_len);
    let len = requested.min(max);
    (len, len.saturating_sub(old_len), requested > max)
}

/// A message's pin state by id: the loaded conversation's copy when it's in
/// it, otherwise the pin list itself — a pin of far history isn't in the
/// loaded page, and its presence there *is* the state. `None` when neither
/// knows the message, which is the only case toggling refuses. `messages`
/// carries `(id, pinned)` of the loaded page, `pin_ids` the pin list's ids.
fn pinned_state(
    messages: &[(Id<MessageMarker>, bool)],
    pin_ids: &[Id<MessageMarker>],
    message_id: Id<MessageMarker>,
) -> Option<bool> {
    messages
        .iter()
        .find(|(id, _)| *id == message_id)
        .map(|&(_, pinned)| pinned)
        .or_else(|| pin_ids.contains(&message_id).then_some(true))
}

/// Claims the one pin-mutation slot for `message_id`. Mutations are
/// serialized — at most one in flight, so the single task slot can never
/// cancel an older toggle and strand its busy mark. Returns whether the
/// toggle may run.
fn begin_pin_toggle(busy: &mut HashSet<Id<MessageMarker>>, message_id: Id<MessageMarker>) -> bool {
    if !busy.is_empty() {
        return false;
    }
    busy.insert(message_id)
}

/// Whether the open conversation allows pinning at all. DM conversations
/// always do; a guild channel needs the pin permission, resolved with its
/// overwrites when the channel list was fetched.
fn can_pin(screen: &HomeScreen) -> bool {
    let Some(channel_id) = screen.selected_channel else {
        return false;
    };
    if screen.view == View::DirectMessages {
        return true;
    }
    screen
        .channel_info(channel_id)
        .is_some_and(|channel| channel.permissions.intersects(CAN_PIN))
}

impl HomeScreen {
    /// Header search box events: enter searches at once, typing searches after
    /// a short debounce so a burst of keystrokes costs one request.
    pub(in crate::screens::home) fn on_search_event(
        &mut self,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.submit_message_search(cx),
            InputEvent::Change => {
                let debounce = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SEARCH_DEBOUNCE).await;
                    let _ = this.update(cx, |this, cx| this.submit_message_search(cx));
                });
                self.message_tools.search.debounce = Some(debounce);
            }
            _ => {}
        }
    }

    /// Runs the header search box's current text, opening the results panel.
    pub(in crate::screens::home) fn submit_message_search(&mut self, cx: &mut Context<Self>) {
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        let dm = self.view == View::DirectMessages;
        if dm {
            self.message_tools.search.scope = SearchScope::Channel;
        }
        let scope = self.message_tools.search.scope;
        let query = self.search_input.read(cx).value().trim().to_string();
        let ctx = SearchResolveCtx {
            self_id: self.self_user_id,
            channels: self
                .channel_groups
                .iter()
                .flat_map(|group| group.channels.iter().map(|c| (c.id, c.name.clone())))
                .collect(),
            authors: if dm {
                self.messages
                    .iter()
                    .map(|m| (m.author_id, m.author_name.clone()))
                    .collect()
            } else {
                self.member_list
                    .as_ref()
                    .map(|list| {
                        list.rows
                            .iter()
                            .flatten()
                            .filter_map(|item| match item {
                                MemberListItem::Member(member) => {
                                    Some((member.user_id, member.name.clone()))
                                }
                                MemberListItem::Group(_) => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            },
        };
        let guild_id = (!dm).then_some(self.selected_guild).flatten();

        let tools = &mut self.message_tools;
        tools.panel = Some(ToolsPanel::Search);
        // A fresh query gets its own retry budget; resubmitting the same one
        // (a scope flip, say) keeps counting against the old one's.
        if tools.search.query != query {
            tools.search.retries_used = 0;
        }
        match resolve_request(parse_query(&query), scope, channel_id, guild_id, &ctx) {
            Ok(request) => tools.search.run(request, query, cx),
            Err(note) => tools.search.invalidate(note, cx),
        }
        cx.notify();
    }

    /// Fetches the next page of results for the query already shown.
    pub(in crate::screens::home) fn load_more_message_results(&mut self, cx: &mut Context<Self>) {
        let search = &mut self.message_tools.search;
        if search.loading || search.loading_more || search.complete {
            return;
        }
        let Some(mut request) = search.last_request.clone() else {
            return;
        };
        request.offset = search.next_offset;
        search.load_more(request, cx);
    }

    /// Resumes a search the API answered 202 to, while attempts remain. The
    /// bound is per query: a fresh query starts its retries over.
    pub(in crate::screens::home) fn retry_message_search(&mut self, cx: &mut Context<Self>) {
        let search = &mut self.message_tools.search;
        if !search.indexing || search.retries_used >= MAX_SEARCH_RETRIES {
            return;
        }
        let Some(request) = search.last_request.clone() else {
            return;
        };
        let query = search.query.clone();
        search.retries_used += 1;
        search.run(request, query, cx);
    }

    pub(in crate::screens::home) fn set_message_search_scope(
        &mut self,
        scope: SearchScope,
        cx: &mut Context<Self>,
    ) {
        if self.view == View::DirectMessages || self.message_tools.search.scope == scope {
            return;
        }
        self.message_tools.search.scope = scope;
        // Re-run what's in the box so the panel follows the scope immediately.
        if !self.search_input.read(cx).value().trim().is_empty() {
            self.submit_message_search(cx);
        }
        cx.notify();
    }

    /// Jumps to a result's message. The guild is the open one: a channel-scope
    /// result is in this conversation, a guild-scope one in this server.
    pub(in crate::screens::home) fn open_message_search_result(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hit) = self.message_tools.search.hits.get(index) else {
            return;
        };
        let guild_id = (self.view == View::Guild)
            .then_some(self.selected_guild)
            .flatten();
        let target = MessageTarget {
            guild_id,
            channel_id: hit.channel_id,
            message_id: hit.message.id,
        };
        self.jump_to_message(target, window, cx);
    }

    /// Jumps to a pinned message from the pins panel. Pins are always the open
    /// conversation's, so the guild follows the same rule a channel-scope
    /// search result does.
    pub(in crate::screens::home) fn open_pinned_message(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pins = &self.message_tools.pins;
        let Some(pin) = pins.entries.get(index) else {
            return;
        };
        let message_id = pin.message.id;
        let Some(channel_id) = pins.channel_id else {
            return;
        };
        let guild_id = (self.view == View::Guild)
            .then_some(self.selected_guild)
            .flatten();
        let target = MessageTarget {
            guild_id,
            channel_id,
            message_id,
        };
        self.jump_to_message(target, window, cx);
    }

    pub(in crate::screens::home) fn toggle_pin_panel(&mut self, cx: &mut Context<Self>) {
        let channel_id = self.selected_channel;
        let tools = &mut self.message_tools;
        if tools.panel == Some(ToolsPanel::Pins) {
            tools.panel = None;
        } else {
            tools.panel = Some(ToolsPanel::Pins);
            if let Some(channel_id) = channel_id {
                tools.pins.load(channel_id, None, false, cx);
            }
        }
        cx.notify();
    }

    pub(in crate::screens::home) fn close_message_tools_panel(&mut self, cx: &mut Context<Self>) {
        if self.message_tools.panel.take().is_some() {
            cx.notify();
        }
    }

    pub(in crate::screens::home) fn load_more_pins(&mut self, cx: &mut Context<Self>) {
        let pins = &mut self.message_tools.pins;
        if pins.loading || pins.loading_more || !pins.has_more {
            return;
        }
        let Some(channel_id) = pins.channel_id else {
            return;
        };
        let before = pins.entries.last().map(|entry| entry.pinned_at);
        pins.load(channel_id, before, true, cx);
    }

    /// Pin or unpin a message from its menu. The menu only offers this where
    /// the channel permission allows it, and the pin state moves only once the
    /// server has answered — a failure leaves the message exactly as it was
    /// and the panel surfaces why.
    pub(in crate::screens::home) fn toggle_pin_message(
        &mut self,
        message_id: Id<MessageMarker>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        if !can_pin(self) {
            let pins = &mut self.message_tools.pins;
            pins.error = Some("You don't have permission to pin messages here.".into());
            self.message_tools.panel = Some(ToolsPanel::Pins);
            cx.notify();
            return;
        }
        // The conversation's copy of the message decides — or, for a pin of
        // far history that isn't in the loaded page, the pin list itself,
        // which is what the row was showing.
        let history: Vec<_> = self
            .messages
            .iter()
            .map(|message| (message.id, message.pinned))
            .collect();
        let pin_ids: Vec<_> = self
            .message_tools
            .pins
            .entries
            .iter()
            .map(|entry| entry.message.id)
            .collect();
        let Some(pinned) = pinned_state(&history, &pin_ids, message_id) else {
            return;
        };

        let pins = &mut self.message_tools.pins;
        // Serialized: a second toggle while one runs is dropped, rather than
        // replacing (and cancelling) the first request's task.
        if !begin_pin_toggle(&mut pins.busy, message_id) {
            return;
        }
        pins.error = None;
        let epoch = pins.epoch;
        pins._toggle_task = Some(cx.spawn(async move |this, cx| {
            let result = if pinned {
                unpin_message(channel_id, message_id).await
            } else {
                pin_message(channel_id, message_id).await
            };
            let _ = this.update(cx, |this, cx| {
                let pins = &mut this.message_tools.pins;
                // A channel switch while this was in flight reloaded the list
                // already; nothing here applies to it — busy included, which
                // belongs to whichever conversation owns this epoch.
                if pins.epoch == epoch {
                    match result {
                        Ok(()) => {
                            if let Some(message) = this
                                .messages
                                .iter_mut()
                                .find(|message| message.id == message_id)
                            {
                                message.pinned = !pinned;
                            }
                        }
                        Err(err) => pins.error = Some(pin_error_note(&err)),
                    }
                    // Busy is cleared only under this epoch: on a stale
                    // arrival it belongs to the new conversation, which
                    // already cleared it.
                    pins.busy.remove(&message_id);
                    this.refresh_pins_if_open(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The gateway reported a pins change in `channel_id`
    /// (CHANNEL_PINS_UPDATE). Only the open conversation's panel list is
    /// live: it reloads from scratch, fencing any fetch or toggle still in
    /// flight for the old state.
    pub(in crate::screens::home) fn on_channel_pins_update(
        &mut self,
        channel_id: Id<ChannelMarker>,
        cx: &mut Context<Self>,
    ) {
        if self.message_tools.panel != Some(ToolsPanel::Pins)
            || self.selected_channel != Some(channel_id)
            || self.message_tools.pins.channel_id != Some(channel_id)
        {
            return;
        }
        let pins = &mut self.message_tools.pins;
        // A toggle in flight is stale by definition once the server pushed a
        // change itself; cancel it and release its mutation slot so the
        // reload isn't blocked by a busy mark nobody will clear.
        pins._toggle_task = None;
        pins.busy.clear();
        pins.load(channel_id, None, false, cx);
        cx.notify();
    }

    /// What the message menu offers for `message`, if anything. The message's
    /// own pin flag decides which way the action goes; being the author is
    /// irrelevant — pinning anyone's message, one's own included, needs the
    /// channel permission.
    pub(in crate::screens::home) fn pin_menu_action(
        &self,
        message: &crate::discord::Message,
    ) -> Option<PinMenuAction> {
        if !can_pin(self) {
            return None;
        }
        Some(if message.pinned {
            PinMenuAction::Unpin
        } else {
            PinMenuAction::Pin
        })
    }

    /// A different conversation was opened: everything shown belonged to the
    /// old one. Cancels in-flight work, clears what it would have filled, and
    /// reloads pins if the panel is open on the new conversation.
    pub(in crate::screens::home) fn message_tools_selection_changed(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let channel_id = self.selected_channel;
        let dm = self.view == View::DirectMessages;
        {
            let tools = &mut self.message_tools;
            let search = &mut tools.search;
            search.epoch += 1;
            search.debounce = None;
            search._task = None;
            search.query.clear();
            search.last_request = None;
            search.hits.clear();
            search.list.reset(0);
            search.total_results = 0;
            search.next_offset = 0;
            search.complete = false;
            search.capped = false;
            search.loading = false;
            search.loading_more = false;
            search.error = None;
            search.indexing = false;
            search.retries_used = 0;
            search.deep_indexing = false;
            if dm {
                search.scope = SearchScope::Channel;
            }
            let pins = &mut tools.pins;
            pins.epoch += 1;
            pins._task = None;
            pins._toggle_task = None;
            pins.channel_id = channel_id;
            pins.entries.clear();
            pins.list.reset(0);
            pins.has_more = false;
            pins.capped = false;
            pins.loading = false;
            pins.loading_more = false;
            pins.error = None;
            pins.busy.clear();
        }
        if self.message_tools.panel == Some(ToolsPanel::Pins)
            && let Some(channel_id) = channel_id
        {
            self.message_tools.pins.load(channel_id, None, false, cx);
        }
        cx.notify();
    }

    /// Reloads the pin list, when the panel is open on the conversation the
    /// pins were just changed in.
    fn refresh_pins_if_open(&mut self, cx: &mut Context<Self>) {
        if self.message_tools.panel != Some(ToolsPanel::Pins) {
            return;
        }
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        if self.message_tools.pins.channel_id == Some(channel_id) {
            self.message_tools.pins.load(channel_id, None, false, cx);
        }
    }
}

impl SearchState {
    /// Starts a fresh search, invalidating whatever this state showed before.
    fn run(&mut self, request: SearchRequest, query: String, cx: &mut Context<HomeScreen>) {
        self.epoch += 1;
        let epoch = self.epoch;
        self.query = query;
        self.last_request = Some(request.clone());
        self.hits.clear();
        self.list.reset(0);
        self.total_results = 0;
        self.next_offset = 0;
        self.complete = false;
        self.capped = false;
        self.loading = true;
        self.loading_more = false;
        self.error = None;
        self.indexing = false;
        self.deep_indexing = false;
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = search_messages(request).await;
            if let Ok(SearchOutcome::Indexing { retry_after }) = &result {
                cx.background_executor().timer(*retry_after).await;
            }
            let _ = this.update(cx, |this, cx| {
                let search = &mut this.message_tools.search;
                // Typing, retrying, or a channel switch started something else;
                // this page belongs to a search nobody asked for any more.
                if search.epoch != epoch {
                    return;
                }
                search.loading = false;
                match result {
                    Ok(SearchOutcome::Page(page)) => {
                        search.total_results = page.total_results;
                        search.next_offset = page.hits.len();
                        search.complete =
                            page_complete(page.hits.len(), page.total_results, search.next_offset);
                        search.deep_indexing = page.deep_indexing;
                        search.hits = page.hits;
                        // A fresh answer replaces the list wholesale; the
                        // reset also sends the viewport back to the top.
                        search.list.reset(search.hits.len());
                    }
                    Ok(SearchOutcome::Indexing { .. }) => {
                        search.indexing = true;
                    }
                    Err(err) => search.error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    /// Appends a page to the results already shown, still under the same epoch.
    fn load_more(&mut self, request: SearchRequest, cx: &mut Context<HomeScreen>) {
        let epoch = self.epoch;
        self.loading_more = true;
        cx.notify();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = search_messages(request).await;
            if let Ok(SearchOutcome::Indexing { retry_after }) = &result {
                cx.background_executor().timer(*retry_after).await;
            }
            let _ = this.update(cx, |this, cx| {
                let search = &mut this.message_tools.search;
                if search.epoch != epoch {
                    return;
                }
                search.loading_more = false;
                match result {
                    Ok(SearchOutcome::Page(page)) => {
                        search.total_results = page.total_results;
                        search.next_offset += page.hits.len();
                        search.complete =
                            page_complete(page.hits.len(), page.total_results, search.next_offset);
                        let old_len = search.hits.len();
                        let page_len = page.hits.len();
                        search.hits.extend(page.hits);
                        let (len, added, cut) = extend_capped(old_len, page_len, MAX_SEARCH_HITS);
                        search.hits.truncate(len);
                        // Appending at the end of a top-aligned list leaves
                        // the viewport where it is; the list just learns the
                        // new row count.
                        search.list.splice(old_len..old_len, added);
                        if cut {
                            search.complete = true;
                            search.capped = true;
                        }
                    }
                    // The page couldn't load; the button comes back so it can
                    // be tried again.
                    Ok(SearchOutcome::Indexing { .. }) => {
                        search.error =
                            Some("the search index went away mid-search — try again".into());
                    }
                    Err(err) => search.error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    /// Replaces everything with a resolution error — the box couldn't become a
    /// request, so nothing shown answers any request any more.
    fn invalidate(&mut self, note: String, cx: &mut Context<HomeScreen>) {
        self.epoch += 1;
        self._task = None;
        self.hits.clear();
        self.list.reset(0);
        self.total_results = 0;
        self.next_offset = 0;
        self.complete = false;
        self.capped = false;
        self.loading = false;
        self.loading_more = false;
        self.indexing = false;
        self.error = Some(note);
        cx.notify();
    }
}

impl PinsState {
    /// Fetches a page of pins for `channel_id`, the first when `more` is off
    /// and the page behind `before` (an earlier entry's `pinned_at`) otherwise.
    fn load(
        &mut self,
        channel_id: Id<ChannelMarker>,
        before: Option<i64>,
        more: bool,
        cx: &mut Context<HomeScreen>,
    ) {
        self.epoch += 1;
        let epoch = self.epoch;
        if more {
            self.loading_more = true;
        } else {
            self.loading = true;
            self.error = None;
        }
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = fetch_pins(channel_id, before).await;
            let _ = this.update(cx, |this, cx| {
                let pins = &mut this.message_tools.pins;
                if pins.epoch != epoch || pins.channel_id != Some(channel_id) {
                    return;
                }
                pins.loading = false;
                pins.loading_more = false;
                match result {
                    Ok(page) => {
                        if !more {
                            pins.capped = false;
                            pins.entries.clear();
                        }
                        let old_len = pins.entries.len();
                        let page_len = page.entries.len();
                        pins.entries.extend(page.entries);
                        let (len, added, cut) = extend_capped(old_len, page_len, MAX_PINS);
                        pins.entries.truncate(len);
                        if !more {
                            pins.list.reset(len);
                        } else {
                            pins.list.splice(old_len..old_len, added);
                        }
                        pins.has_more = page.has_more;
                        if cut {
                            pins.has_more = false;
                            pins.capped = true;
                        }
                    }
                    Err(err) => pins.error = Some(err),
                }
                cx.notify();
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(channels: &[(&str, u64)], authors: &[(&str, u64)]) -> SearchResolveCtx {
        SearchResolveCtx {
            self_id: Some(Id::new(1)),
            channels: channels
                .iter()
                .map(|(name, id)| (Id::new(*id), name.to_string()))
                .collect(),
            authors: authors
                .iter()
                .map(|(name, id)| (Id::new(*id), name.to_string()))
                .collect(),
        }
    }

    #[test]
    fn splits_operators_from_content() {
        let parsed = parse_query("hello world from:alice has:file from:me");
        assert_eq!(parsed.content, "hello world");
        assert_eq!(parsed.from, vec!["alice", "me"]);
        assert_eq!(parsed.has, vec![SearchHas::File]);
        assert!(parsed.unknown.is_empty());
    }

    #[test]
    fn operator_names_are_case_insensitive() {
        let parsed = parse_query("FROM:me Before:2026-01-01");
        assert_eq!(parsed.from, vec!["me"]);
        assert_eq!(parsed.before.as_deref(), Some("2026-01-01"));
    }

    #[test]
    fn unknown_operators_are_reported() {
        let parsed = parse_query("nice ping:2");
        assert_eq!(parsed.content, "nice");
        assert_eq!(parsed.unknown, vec!["ping"]);
    }

    #[test]
    fn invalid_has_value_is_reported() {
        let parsed = parse_query("has:sound has:vibes");
        assert!(parsed.invalid_has.contains(&"vibes".to_string()));
        assert_eq!(parsed.has, vec![SearchHas::Sound]);
    }

    #[test]
    fn resolves_names_and_me_locally() {
        let ctx = ctx(&[("general", 40)], &[("Alice", 30)]);
        let request = resolve_request(
            parse_query("from:Alice in:General has:link"),
            SearchScope::Guild,
            Id::new(2),
            Some(Id::new(9)),
            &ctx,
        )
        .unwrap();
        assert_eq!(request.author_ids, vec![Id::new(30)]);
        assert_eq!(request.channel_ids, vec![Id::new(40)]);
        assert_eq!(request.guild_id, Some(Id::new(9)));
        assert_eq!(request.has, vec![SearchHas::Link]);
    }

    #[test]
    fn resolves_me_to_the_session_user() {
        let ctx = ctx(&[], &[]);
        let request = resolve_request(
            parse_query("from:me"),
            SearchScope::Channel,
            Id::new(2),
            None,
            &ctx,
        )
        .unwrap();
        assert_eq!(request.author_ids, vec![Id::new(1)]);
        assert_eq!(request.guild_id, None);
    }

    #[test]
    fn unresolved_names_are_honest_errors() {
        let ctx = ctx(&[("general", 40)], &[]);
        let err = resolve_request(
            parse_query("from:ghost"),
            SearchScope::Guild,
            Id::new(2),
            Some(Id::new(9)),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("from:ghost"));
    }

    #[test]
    fn in_is_rejected_outside_guild_scope() {
        let ctx = ctx(&[("general", 40)], &[]);
        let err = resolve_request(
            parse_query("in:general"),
            SearchScope::Channel,
            Id::new(2),
            None,
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("in:"));
    }

    #[test]
    fn dates_become_snowflake_anchors() {
        let ctx = ctx(&[], &[]);
        let request = resolve_request(
            parse_query("after:2026-01-02 before:2026-01-03"),
            SearchScope::Channel,
            Id::new(2),
            None,
            &ctx,
        )
        .unwrap();
        // The upper date is excluded, leaving only January 2.
        let span = request.max_id.unwrap() - request.min_id.unwrap();
        assert_eq!(span >> 22, 86_400_000 - 1);
    }

    #[test]
    fn bare_snowflake_anchors_pass_through() {
        let ctx = ctx(&[], &[]);
        let request = resolve_request(
            parse_query("before:1076986676557131916"),
            SearchScope::Channel,
            Id::new(2),
            None,
            &ctx,
        )
        .unwrap();
        assert_eq!(request.max_id, Some(1076986676557131916));
    }

    #[test]
    fn empty_queries_are_rejected() {
        let ctx = ctx(&[], &[]);
        assert!(
            resolve_request(
                parse_query(""),
                SearchScope::Channel,
                Id::new(2),
                None,
                &ctx
            )
            .is_err()
        );
    }

    #[test]
    fn a_short_page_ends_the_search() {
        assert!(page_complete(10, 500, 10));
        assert!(!page_complete(SEARCH_PAGE_SIZE, 500, SEARCH_PAGE_SIZE));
        assert!(page_complete(SEARCH_PAGE_SIZE, 500, MAX_SEARCH_OFFSET + 1));
    }

    #[test]
    fn pin_failures_read_like_state() {
        assert_eq!(
            pin_error_note("error 404 Unknown Message"),
            "That message is no longer pinned."
        );
        assert_eq!(
            pin_error_note("403 Missing Permissions"),
            "You don't have permission to pin messages here."
        );
        assert_eq!(pin_error_note("connection reset"), "connection reset");
    }

    #[test]
    fn pin_toggles_are_serialized() {
        let mut busy = HashSet::new();
        let first = Id::<MessageMarker>::new(10);
        let second = Id::<MessageMarker>::new(11);
        // The first toggle claims the slot.
        assert!(begin_pin_toggle(&mut busy, first));
        assert_eq!(busy, HashSet::from([first]));
        // A second one — either message — is refused while it runs, so the
        // single task slot can never cancel it and strand a busy mark.
        assert!(!begin_pin_toggle(&mut busy, second));
        assert!(!begin_pin_toggle(&mut busy, first));
        assert_eq!(busy, HashSet::from([first]));
        // Once the in-flight toggle clears its mark, the next runs.
        busy.clear();
        assert!(begin_pin_toggle(&mut busy, second));
    }

    #[test]
    fn capped_extension_announces_the_rows_it_keeps() {
        // A page that fits appends whole.
        assert_eq!(extend_capped(0, 25, 500), (25, 25, false));
        assert_eq!(extend_capped(475, 25, 500), (500, 25, false));
        // A page straddling the cap is cut, and the virtualized list learns
        // only the rows actually kept — never more than are in memory.
        assert_eq!(extend_capped(480, 25, 500), (500, 20, true));
        assert_eq!(extend_capped(501, 25, 500), (500, 0, true));
    }

    #[test]
    fn pin_state_resolves_without_loaded_history() {
        let history = [(Id::<MessageMarker>::new(1), false)];
        let pin_ids = [Id::<MessageMarker>::new(2), Id::<MessageMarker>::new(3)];

        // In the loaded page: its own flag decides.
        assert_eq!(pinned_state(&history, &pin_ids, Id::new(1)), Some(false));
        // Far history, known only through the pin list: pinned, so a real
        // unpin is offered instead of a silent no-op.
        assert_eq!(pinned_state(&[], &pin_ids, Id::new(2)), Some(true));
        assert_eq!(pinned_state(&history, &pin_ids, Id::new(2)), Some(true));
        // Known nowhere: the only case toggling refuses.
        assert_eq!(pinned_state(&[], &pin_ids, Id::new(9)), None);
    }
}
