//! The threads panel's behaviour: the lists and their gateway reducer, the
//! fetches behind them, and the actions the view calls.
//!
//! The list mutations are pure functions on [`ThreadLists`], so the merge,
//! dedup, scope, and permission rules are testable without gpui; the
//! `impl ThreadState` methods wrap them in tasks that bridge the REST calls
//! back onto the screen. Dropping a stored task cancels it, and every task
//! re-checks the parent and its generation before touching state, so
//! switching channels can't land an old response in a new panel.

use std::future::Future;
use std::pin::Pin;

use gpui::{Context, Window};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, TagMarker},
};

use crate::discord::{
    self, ArchivedCursor, ArchivedScope, Permissions, ThreadEvent, ThreadSummary,
};

use super::{CreateSource, HomeScreen, MAX_THREADS_PER_LIST, Tab, ThreadState};

/// The most threads the active search pages through before the panel stops
/// offering more. The endpoint itself refuses offsets past this.
const SEARCH_OFFSET_LIMIT: usize = 9_975;

/// How many tags a forum post may carry.
const MAX_APPLIED_TAGS: usize = 5;

/// Discord's thread-name ceiling, in characters.
const MAX_NAME_LENGTH: usize = 100;

/// The API's default archive duration, in minutes, used for every created
/// thread until a picker is worth wiring into the forms.
const DEFAULT_ARCHIVE_MINUTES: u16 = 4320;

/// The two lists the panel keeps: the open channel's active threads, and
/// the page of the archive it has loaded so far.
#[derive(Default)]
pub(super) struct ThreadLists {
    pub(super) active: Vec<ThreadSummary>,
    pub(super) archived: Vec<ThreadSummary>,
}

impl ThreadLists {
    pub(super) fn get(&self, thread_id: Id<ChannelMarker>) -> Option<&ThreadSummary> {
        self.active
            .iter()
            .chain(&self.archived)
            .find(|thread| thread.id == thread_id)
    }

    /// Inserts a fresh summary, merging it into what was known. A thread
    /// whose archived flag changed moves between the lists. Returns whether
    /// anything changed.
    pub(super) fn upsert(&mut self, incoming: ThreadSummary, cap: usize) -> bool {
        let archived = incoming.archived;
        let id = incoming.id;
        match self.position(id) {
            // Same list: merge in place, keeping the API's order.
            Some((index, true)) if archived => {
                let merged = merge(&self.archived[index], incoming);
                self.archived[index] = merged;
                true
            }
            Some((index, false)) if !archived => {
                let merged = merge(&self.active[index], incoming);
                self.active[index] = merged;
                true
            }
            // The archive state flipped: move what was known into the other
            // list, at its top where the newest-archived ordering would put
            // a freshly archived thread anyway.
            Some((index, was_archived)) => {
                let merged = if was_archived {
                    merge(&self.archived[index], incoming)
                } else {
                    merge(&self.active[index], incoming)
                };
                if was_archived {
                    self.archived.remove(index);
                } else {
                    self.active.remove(index);
                }
                if archived {
                    self.archived.insert(0, merged);
                } else {
                    self.active.insert(0, merged);
                }
                true
            }
            None if self.archived.len() >= cap && archived => false,
            None if self.active.len() >= cap && !archived => false,
            None if archived => {
                self.archived.insert(0, incoming);
                true
            }
            None => {
                self.active.insert(0, incoming);
                true
            }
        }
    }

    /// Where a thread sits: its index, and whether it is in the archived
    /// list.
    fn position(&self, id: Id<ChannelMarker>) -> Option<(usize, bool)> {
        self.archived
            .iter()
            .position(|thread| thread.id == id)
            .map(|index| (index, true))
            .or_else(|| {
                self.active
                    .iter()
                    .position(|thread| thread.id == id)
                    .map(|index| (index, false))
            })
    }

    /// Folds a fetched page into its list: known threads are refreshed where
    /// they sit, new ones appended in the API's newest-first order, and the
    /// list stays capped. Returns whether anything changed.
    pub(super) fn merge_page(
        &mut self,
        page: Vec<ThreadSummary>,
        archived: bool,
        cap: usize,
    ) -> bool {
        let mut changed = false;
        for summary in page {
            match self.position(summary.id) {
                // Refresh in place.
                Some((index, true)) if archived => {
                    let merged = merge(&self.archived[index], summary);
                    self.archived[index] = merged;
                }
                Some((index, false)) if !archived => {
                    let merged = merge(&self.active[index], summary);
                    self.active[index] = merged;
                }
                // The other list's page refreshed our copy's counterpart
                // without moving it between lists.
                Some((index, was_archived)) => {
                    if was_archived {
                        let merged = merge(&self.archived[index], summary);
                        self.archived[index] = merged;
                    } else {
                        let merged = merge(&self.active[index], summary);
                        self.active[index] = merged;
                    }
                }
                None if archived => {
                    if self.archived.len() < cap {
                        self.archived.push(summary);
                        changed = true;
                    }
                }
                None => {
                    if self.active.len() < cap {
                        self.active.push(summary);
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    pub(super) fn remove(&mut self, thread_id: Id<ChannelMarker>) -> bool {
        let before = self.active.len() + self.archived.len();
        self.active.retain(|thread| thread.id != thread_id);
        self.archived.retain(|thread| thread.id != thread_id);
        before != self.active.len() + self.archived.len()
    }

    pub(super) fn set_joined(&mut self, thread_id: Id<ChannelMarker>, joined: bool) -> bool {
        let mut changed = false;
        for thread in self.active.iter_mut().chain(self.archived.iter_mut()) {
            if thread.id == thread_id && thread.joined != joined {
                thread.joined = joined;
                changed = true;
            }
        }
        changed
    }

    /// Undoes an optimistic join/leave, unless a gateway event already moved
    /// the flag past what was optimistically set.
    pub(super) fn rollback_joined(&mut self, thread_id: Id<ChannelMarker>, joined: bool) {
        for thread in self.active.iter_mut().chain(self.archived.iter_mut()) {
            if thread.id == thread_id && thread.joined == joined {
                thread.joined = !joined;
            }
        }
    }

    /// Applies one gateway event, scoped to the open parent. Events about
    /// other guilds or parents never touch these lists, which is what keeps
    /// a guild-wide sync from wiping threads the panel fetched itself.
    pub(super) fn apply_event(
        &mut self,
        guild_id: Id<GuildMarker>,
        parent_id: Id<ChannelMarker>,
        event: &ThreadEvent,
        cap: usize,
    ) -> bool {
        match event {
            ThreadEvent::Upsert(summary) => {
                if summary.guild_id != Some(guild_id) || summary.parent_id != parent_id {
                    return false;
                }
                self.upsert(summary.clone(), cap)
            }
            ThreadEvent::Delete {
                guild_id: event_guild,
                thread_id,
                parent_id: event_parent,
            } => {
                if *event_guild != guild_id || *event_parent != parent_id {
                    return false;
                }
                self.remove(*thread_id)
            }
            ThreadEvent::Sync {
                guild_id: event_guild,
                parent_ids,
                threads,
            } => {
                if *event_guild != guild_id {
                    return false;
                }
                // A guild-wide sync carries only the user's joined threads,
                // so it merges; a channel-scoped one is the full active set
                // for the named channels, so it replaces.
                let scoped = !parent_ids.is_empty();
                if scoped && !parent_ids.contains(&parent_id) {
                    return false;
                }
                let mut changed = false;
                let incoming: Vec<_> = threads
                    .iter()
                    .filter(|thread| thread.parent_id == parent_id)
                    .cloned()
                    .collect();
                if scoped {
                    let kept: Vec<_> = incoming.iter().map(|thread| thread.id).collect();
                    let before = self.active.len();
                    self.active.retain(|thread| {
                        thread.parent_id != parent_id || kept.contains(&thread.id)
                    });
                    changed |= before != self.active.len();
                }
                for thread in incoming {
                    changed |= self.upsert(thread, cap);
                }
                changed
            }
            ThreadEvent::Membership {
                guild_id: event_guild,
                thread_id,
                member_count,
                joined,
            } => {
                if *event_guild != guild_id {
                    return false;
                }
                let mut changed = false;
                for thread in self.active.iter_mut().chain(self.archived.iter_mut()) {
                    if thread.id != *thread_id {
                        continue;
                    }
                    if let Some(count) = member_count
                        && thread.member_count != Some(*count)
                    {
                        thread.member_count = Some(*count);
                        changed = true;
                    }
                    if let Some(joined) = joined
                        && thread.joined != *joined
                    {
                        thread.joined = *joined;
                        changed = true;
                    }
                }
                changed
            }
        }
    }
}

/// Folds a fresh summary into a known one. Core fields and the archive state
/// come from the payload, which always carries them; the optional counters
/// keep their known values when the payload omits them, tags survive an
/// update that lists none, and membership is never revoked by a mere update
/// — leaves arrive as explicit membership events.
fn merge(existing: &ThreadSummary, incoming: ThreadSummary) -> ThreadSummary {
    ThreadSummary {
        message_count: incoming.message_count.or(existing.message_count),
        member_count: incoming.member_count.or(existing.member_count),
        owner_id: incoming.owner_id.or(existing.owner_id),
        last_message_id: incoming.last_message_id.or(existing.last_message_id),
        preview: incoming.preview.or(existing.preview.clone()),
        joined: incoming.joined || existing.joined,
        applied_tags: if incoming.applied_tags.is_empty() && !existing.applied_tags.is_empty() {
            existing.applied_tags.clone()
        } else {
            incoming.applied_tags
        },
        ..incoming
    }
}

/// Where the next archived page starts, in the cursor kind the scope's
/// route accepts: archive timestamps for public and private, thread ids for
/// the user's own private threads.
pub(super) fn archived_cursor(
    scope: ArchivedScope,
    list: &[ThreadSummary],
) -> Option<ArchivedCursor> {
    let last = list.last()?;
    Some(match scope {
        ArchivedScope::Public | ArchivedScope::Private => {
            ArchivedCursor::Timestamp(last.archive_timestamp)
        }
        ArchivedScope::JoinedPrivate => ArchivedCursor::Id(last.id),
    })
}

/// Whether the panel may show anything at all.
pub(super) fn can_view(permissions: Permissions) -> bool {
    permissions.contains(Permissions::VIEW_CHANNEL)
}

/// Whether a new thread or post may be started here. Forum and media
/// channels take `SEND_MESSAGES` alone — the thread-only exception — while
/// text channels split public and private between the two `CREATE_*_THREADS`
/// permissions.
pub(super) fn can_create_thread(permissions: Permissions, forum: bool, private: bool) -> bool {
    if forum {
        permissions.contains(Permissions::SEND_MESSAGES)
    } else if private {
        permissions.contains(Permissions::CREATE_PRIVATE_THREADS)
    } else {
        permissions.contains(Permissions::CREATE_PUBLIC_THREADS)
    }
}

/// Whether the current user may send in a thread: nobody talks in an
/// archived one, and every thread takes `SEND_MESSAGES_IN_THREADS` —
/// `SEND_MESSAGES` has no effect inside threads. A locked one needs that
/// bit *and* `MANAGE_THREADS`, the same pair the conversation's own guard
/// demands.
pub(super) fn can_reply(permissions: Permissions, thread: &ThreadSummary) -> bool {
    if thread.archived {
        return false;
    }
    if thread.locked {
        permissions.contains(Permissions::MANAGE_THREADS)
            && permissions.contains(Permissions::SEND_MESSAGES_IN_THREADS)
    } else {
        permissions.contains(Permissions::SEND_MESSAGES_IN_THREADS)
    }
}

/// Whether the current user may read a channel's history at all, which the
/// archived listing routes all require before their scope-specific bits.
pub(super) fn can_list_archived(permissions: Permissions, scope: ArchivedScope) -> bool {
    if !permissions.contains(Permissions::READ_MESSAGE_HISTORY) {
        return false;
    }
    match scope {
        ArchivedScope::Public | ArchivedScope::JoinedPrivate => true,
        ArchivedScope::Private => permissions.contains(Permissions::MANAGE_THREADS),
    }
}

/// Whether the join/leave button applies: archived threads take no new
/// members and keep the ones they have.
pub(super) fn can_join(thread: &ThreadSummary) -> bool {
    !thread.archived
}

/// Whether the archive/unarchive button may show. Archiving is a moderator
/// or the thread's own creator; unarchiving a locked thread needs
/// `MANAGE_THREADS` like anything else on it, and an unlocked one is open
/// to its members — the server still has the final word either way.
pub(super) fn can_archive(
    permissions: Permissions,
    thread: &ThreadSummary,
    archive: bool,
    is_owner: bool,
) -> bool {
    if archive {
        permissions.contains(Permissions::MANAGE_THREADS) || is_owner
    } else if thread.locked {
        permissions.contains(Permissions::MANAGE_THREADS)
    } else {
        true
    }
}

impl ThreadState {
    /// Whether a fetch that left with this parent and generation may still
    /// touch the panel. Every task re-checks before touching state, so a
    /// response can never land in the panel of a channel that changed while
    /// it was in flight — busy flags included.
    fn fetch_stale(
        &self,
        parent: Option<(Id<GuildMarker>, Id<ChannelMarker>)>,
        generation: u64,
    ) -> bool {
        self.generation != generation || self.parent != parent
    }

    /// Fetches the active list, at the panel's offset cursor. Doubles as the
    /// retry of a failed first page.
    pub(super) fn load_active(&mut self, cx: &mut Context<HomeScreen>) {
        let Some(parent) = self.parent else {
            return;
        };
        if self.active_loading {
            return;
        }
        self.active_loading = true;
        self.loading_more = self.active_offset > 0;
        self.error = None;
        let generation = self.generation;
        let offset = self.active_offset;
        cx.notify();

        self._active_task = Some(cx.spawn(async move |screen, cx| {
            let result = discord::threads::fetch_active_threads(parent.1, offset).await;
            let _ = screen.update(cx, |screen, cx| {
                let threads = &mut screen.threads;
                if threads.fetch_stale(Some(parent), generation) {
                    return;
                }
                threads.active_loading = false;
                threads.loading_more = false;
                match result {
                    Ok(page) => {
                        threads.has_more_active = page.has_more;
                        threads.active_offset = offset + page.threads.len();
                        threads
                            .lists
                            .merge_page(page.threads, false, MAX_THREADS_PER_LIST);
                    }
                    Err(err) => threads.error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    /// Pages the active list further, while the endpoint still has more and
    /// its cursor has room left.
    pub(super) fn load_more_active(&mut self, cx: &mut Context<HomeScreen>) {
        if !self.has_more_active
            || self.active_loading
            || self.active_offset >= SEARCH_OFFSET_LIMIT
            || self.lists.active.len() >= MAX_THREADS_PER_LIST
        {
            return;
        }
        self.load_active(cx);
    }

    /// Pages the archived list further, from the cursor of its last thread.
    pub(super) fn load_more_archived(&mut self, cx: &mut Context<HomeScreen>) {
        if !self.has_more_archived
            || self.archived_loading
            || self.lists.archived.len() >= MAX_THREADS_PER_LIST
        {
            return;
        }
        self.load_archived(true, cx);
    }

    /// Fetches the archived list under the current scope. `more` continues
    /// from the last loaded thread; otherwise the list starts over.
    fn load_archived(&mut self, more: bool, cx: &mut Context<HomeScreen>) {
        let Some(parent) = self.parent else {
            return;
        };
        if self.archived_loading {
            return;
        }
        if more && (!self.has_more_archived || self.lists.archived.len() >= MAX_THREADS_PER_LIST) {
            return;
        }
        let scope = self.scope;
        let before = more
            .then(|| archived_cursor(scope, &self.lists.archived))
            .flatten();
        if !more {
            self.lists.archived.clear();
            self.has_more_archived = false;
        }
        self.archived_loading = true;
        self.loading_more = more;
        self.error = None;
        let generation = self.generation;
        cx.notify();

        self._archived_task = Some(cx.spawn(async move |screen, cx| {
            let result = discord::threads::fetch_archived_threads(parent.1, scope, before).await;
            let _ = screen.update(cx, |screen, cx| {
                let threads = &mut screen.threads;
                if threads.scope != scope || threads.fetch_stale(Some(parent), generation) {
                    return;
                }
                threads.archived_loading = false;
                threads.loading_more = false;
                // Settled under this scope either way, so a failed load
                // doesn't refetch itself in a loop; the banner's retry does.
                threads.scope_loaded = Some(scope);
                match result {
                    Ok(page) => {
                        threads.has_more_archived = page.has_more;
                        threads
                            .lists
                            .merge_page(page.threads, true, MAX_THREADS_PER_LIST);
                    }
                    Err(err) => threads.error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    /// Shows the other tab, fetching the archived list the first time it is
    /// shown and after every scope change.
    pub(super) fn select_tab(&mut self, tab: Tab, cx: &mut Context<HomeScreen>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.error = None;
        if tab == Tab::Archived && self.scope_loaded != Some(self.scope) {
            self.load_archived(false, cx);
        } else {
            cx.notify();
        }
    }

    /// Switches which slice of the archive the tab lists. Each scope is its
    /// own route and permission, so the list starts over.
    pub(super) fn select_scope(&mut self, scope: ArchivedScope, cx: &mut Context<HomeScreen>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.scope_loaded = None;
        self.error = None;
        if self.tab == Tab::Archived {
            self.load_archived(false, cx);
        } else {
            cx.notify();
        }
    }

    /// Retries whichever load failed into the banner.
    pub(super) fn retry(&mut self, cx: &mut Context<HomeScreen>) {
        match self.tab {
            Tab::Active => self.load_active(cx),
            Tab::Archived => self.load_archived(false, cx),
        }
    }

    /// Opens the panel's create form, when the current user may create here
    /// at all: a forum takes `SEND_MESSAGES`, a text channel either of the
    /// two thread-creation permissions.
    pub(super) fn begin_create(&mut self, window: &mut Window, cx: &mut Context<HomeScreen>) {
        if self.creating.is_some() || self.parent.is_none() || !can_view(self.permissions) {
            return;
        }
        let any_kind = self.forum
            || can_create_thread(self.permissions, false, false)
            || can_create_thread(self.permissions, false, true);
        if !any_kind {
            return;
        }
        self.creating = Some(CreateSource::Channel);
        self.create_error = None;
        self.name_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Opens the create form aimed at one message. The screen's message menu
    /// calls this; its content hook renders the form while it is open.
    pub(in crate::screens::home) fn begin_create_from_message(
        &mut self,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        window: &mut Window,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.parent.is_none() {
            return;
        }
        self.creating = Some(CreateSource::Message(channel_id, message_id));
        self.create_error = None;
        self.name_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn close_create(&mut self, window: &mut Window, cx: &mut Context<HomeScreen>) {
        if self.creating.is_none() {
            return;
        }
        self.creating = None;
        self.submitting = false;
        self.create_error = None;
        self.selected_tags.clear();
        self.private_thread = false;
        self.name_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.body_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// Picks or unpicks a forum tag, at most the five Discord applies.
    pub(super) fn toggle_tag(&mut self, tag_id: Id<TagMarker>, cx: &mut Context<HomeScreen>) {
        if let Some(at) = self.selected_tags.iter().position(|tag| tag == &tag_id) {
            self.selected_tags.remove(at);
        } else if self.selected_tags.len() < MAX_APPLIED_TAGS {
            self.selected_tags.push(tag_id);
        }
        cx.notify();
    }

    pub(super) fn set_private(&mut self, private: bool, cx: &mut Context<HomeScreen>) {
        if self.private_thread != private {
            self.private_thread = private;
            cx.notify();
        }
    }

    /// Sends the create form. The route follows the target: a forum post
    /// with its first message and tags, a thread spun off a message, or a
    /// bare thread on a text channel. Success lands the thread in the active
    /// list joined, as Discord adds its creator; a refusal keeps the form
    /// open with the server's word in it, since that is almost always a
    /// permission or ratelimit the caller should read.
    pub(super) fn submit_create(&mut self, window: &mut Window, cx: &mut Context<HomeScreen>) {
        let Some(source) = self.creating else {
            return;
        };
        if self.submitting {
            return;
        }
        let Some((_, parent_channel_id)) = self.parent else {
            return;
        };

        let name = self.name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.create_error = Some("Give the thread a name.".to_string());
            cx.notify();
            return;
        }
        if name.chars().count() > MAX_NAME_LENGTH {
            self.create_error = Some("Thread names can be at most 100 characters.".to_string());
            cx.notify();
            return;
        }

        let request: Pin<Box<dyn Future<Output = Result<ThreadSummary, String>> + Send>> =
            match (self.forum, source) {
                (true, _) => {
                    let content = self.body_input.read(cx).value().trim().to_string();
                    if content.is_empty() {
                        self.create_error = Some("Forum posts need a first message.".to_string());
                        cx.notify();
                        return;
                    }
                    if self.requires_tag && self.selected_tags.is_empty() {
                        self.create_error =
                            Some("This channel requires a tag on every post.".to_string());
                        cx.notify();
                        return;
                    }
                    let target = match source {
                        CreateSource::Channel => parent_channel_id,
                        CreateSource::Message(channel_id, _) => channel_id,
                    };
                    Box::pin(discord::threads::create_forum_post(
                        target,
                        name,
                        content,
                        self.selected_tags.clone(),
                    ))
                }
                (false, CreateSource::Channel) => Box::pin(discord::threads::create_thread(
                    parent_channel_id,
                    name,
                    self.private_thread,
                    DEFAULT_ARCHIVE_MINUTES,
                )),
                (false, CreateSource::Message(channel_id, message_id)) => {
                    Box::pin(discord::threads::create_thread_from_message(
                        channel_id,
                        message_id,
                        name,
                        DEFAULT_ARCHIVE_MINUTES,
                    ))
                }
            };

        self.submitting = true;
        self.create_error = None;
        let parent = self.parent;
        let generation = self.generation;
        cx.notify();

        self._mutate_task = Some(cx.spawn_in(window, async move |screen, cx| {
            let result = request.await;
            let _ = screen.update_in(cx, |screen, window, cx| {
                let threads = &mut screen.threads;
                if threads.fetch_stale(parent, generation) {
                    return;
                }
                threads.submitting = false;
                let created = match result {
                    Ok(mut summary) => {
                        summary.joined = true;
                        let created = summary.id;
                        threads.lists.upsert(summary, MAX_THREADS_PER_LIST);
                        // A no-op when the user cancelled the form mid-flight;
                        // the thread exists either way, so the upsert stood.
                        threads.close_create(window, cx);
                        Some(created)
                    }
                    // A refusal of a form the user already dismissed is not
                    // worth a banner in the list.
                    Err(err) if threads.creating.is_some() => {
                        threads.create_error = Some(err);
                        None
                    }
                    Err(_) => None,
                };
                cx.notify();
                // Past the last use of `threads`, so the screen borrow is
                // free again: a created thread opens as its conversation,
                // whose summary and permissions the screen already resolves.
                if let Some(created) = created {
                    screen.select_channel(created, window, cx);
                }
            });
        }));
    }

    /// Joins or leaves a thread, optimistically, rolling back if the server
    /// refuses. Leaving deletes nothing; the gateway's membership event
    /// confirms whichever way it goes.
    pub(super) fn toggle_membership(
        &mut self,
        thread_id: Id<ChannelMarker>,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.mutating.is_some() {
            return;
        }
        let Some(summary) = self.lists.get(thread_id) else {
            return;
        };
        if !can_join(summary) {
            return;
        }
        let join = !summary.joined;
        self.lists.set_joined(thread_id, join);
        self.mutating = Some(thread_id);
        let parent = self.parent;
        let generation = self.generation;
        cx.notify();

        self._mutate_task = Some(cx.spawn(async move |screen, cx| {
            let result = if join {
                discord::threads::join_thread(thread_id).await
            } else {
                discord::threads::leave_thread(thread_id).await
            };
            let _ = screen.update(cx, |screen, cx| {
                let threads = &mut screen.threads;
                // A panel that changed while the request flew owns its own
                // error banner and membership; nothing here may touch it.
                if threads.fetch_stale(parent, generation) {
                    return;
                }
                if threads.mutating == Some(thread_id) {
                    threads.mutating = None;
                }
                if let Err(err) = result {
                    // Only undoes the optimistic write; a gateway membership
                    // event that arrived meanwhile is left standing.
                    threads.lists.rollback_joined(thread_id, join);
                    threads.error = Some(err);
                }
                cx.notify();
            });
        }));
    }

    /// Archives or unarchives a thread on the server's word; the returned
    /// thread lands as an upsert, moving it between the lists.
    pub(super) fn set_archived(
        &mut self,
        thread_id: Id<ChannelMarker>,
        archive: bool,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.mutating.is_some() || self.lists.get(thread_id).is_none() {
            return;
        }
        self.mutating = Some(thread_id);
        let parent = self.parent;
        let generation = self.generation;
        cx.notify();

        self._mutate_task = Some(cx.spawn(async move |screen, cx| {
            let result = discord::threads::set_thread_archived(thread_id, archive).await;
            let _ = screen.update(cx, |screen, cx| {
                let threads = &mut screen.threads;
                if threads.fetch_stale(parent, generation) {
                    return;
                }
                if threads.mutating == Some(thread_id) {
                    threads.mutating = None;
                }
                match result {
                    Ok(summary) => {
                        threads.lists.upsert(summary, MAX_THREADS_PER_LIST);
                    }
                    Err(err) => threads.error = Some(err),
                }
                cx.notify();
            });
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;
    use twilight_model::util::Timestamp;

    fn archive_time() -> Timestamp {
        Timestamp::from_str("2024-01-01T00:00:05+00:00").unwrap()
    }

    fn summary(id: u64, parent: u64, archived: bool) -> ThreadSummary {
        ThreadSummary {
            id: Id::new(id),
            parent_id: Id::new(parent),
            guild_id: Some(Id::new(1)),
            name: format!("thread-{id}"),
            kind: crate::discord::ChannelKind::PublicThread,
            archived,
            locked: false,
            archive_timestamp: archive_time(),
            message_count: Some(3),
            member_count: Some(2),
            owner_id: None,
            applied_tags: Vec::new(),
            joined: false,
            last_message_id: Some(Id::new(900)),
            preview: Some("hello".to_string()),
            pinned: false,
        }
    }

    fn in_guild(mut summary: ThreadSummary, guild: u64) -> ThreadSummary {
        summary.guild_id = Some(Id::new(guild));
        summary
    }

    #[test]
    fn upsert_merges_partials_without_clearing_known_fields() {
        let mut lists = ThreadLists::default();
        lists.upsert(summary(10, 100, false), 50);

        // An update carrying none of the counters must not blank them, and
        // a bare update must not unjoin the thread.
        let update = ThreadSummary {
            message_count: None,
            member_count: None,
            last_message_id: None,
            preview: None,
            joined: true,
            name: "renamed".to_string(),
            ..summary(10, 100, false)
        };
        lists.upsert(update, 50);

        let thread = lists.get(Id::new(10)).unwrap();
        assert_eq!(thread.name, "renamed");
        assert!(thread.joined);
        assert_eq!(thread.message_count, Some(3));
        assert_eq!(thread.member_count, Some(2));
        assert_eq!(thread.last_message_id, Some(Id::new(900)));
        assert_eq!(thread.preview.as_deref(), Some("hello"));
    }

    #[test]
    fn upsert_moves_threads_between_the_active_and_archived_lists() {
        let mut lists = ThreadLists::default();
        lists.upsert(summary(10, 100, false), 50);
        lists.upsert(summary(11, 100, true), 50);
        assert_eq!(lists.active.len(), 1);
        assert_eq!(lists.archived.len(), 1);

        let archived = ThreadSummary {
            archived: true,
            ..summary(10, 100, false)
        };
        lists.upsert(archived, 50);
        assert!(lists.active.is_empty());
        assert_eq!(lists.archived.len(), 2);
        // The merge rode along: the moved thread kept its counts.
        assert_eq!(lists.get(Id::new(10)).unwrap().message_count, Some(3));

        let unarchived = ThreadSummary {
            archived: false,
            ..summary(10, 100, false)
        };
        lists.upsert(unarchived, 50);
        assert_eq!(lists.active.len(), 1);
        assert_eq!(lists.archived.len(), 1);
    }

    #[test]
    fn events_outside_the_open_guild_or_parent_are_ignored() {
        let mut lists = ThreadLists::default();
        let guild = Id::new(1);
        let parent = Id::new(100);

        assert!(!lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Upsert(in_guild(summary(10, 999, false), 1)),
            50
        ));
        assert!(!lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Upsert(in_guild(summary(10, 100, false), 2)),
            50
        ));
        assert!(lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Upsert(in_guild(summary(10, 100, false), 1)),
            50
        ));

        assert!(!lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Delete {
                guild_id: guild,
                thread_id: Id::new(10),
                parent_id: Id::new(999),
            },
            50
        ));
        assert!(!lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Delete {
                guild_id: Id::new(2),
                thread_id: Id::new(10),
                parent_id: parent,
            },
            50
        ));
        assert!(lists.get(Id::new(10)).is_some());

        assert!(lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Delete {
                guild_id: guild,
                thread_id: Id::new(10),
                parent_id: parent,
            },
            50
        ));
        assert!(lists.get(Id::new(10)).is_none());
    }

    #[test]
    fn channel_scoped_sync_replaces_but_guild_wide_sync_only_merges() {
        let mut lists = ThreadLists::default();
        let guild = Id::new(1);
        let parent = Id::new(100);
        lists.upsert(in_guild(summary(10, 100, false), 1), 50);
        lists.upsert(in_guild(summary(11, 100, false), 1), 50);

        // A guild-wide sync for a user account only carries joined threads;
        // it must not wipe the fetched, not-joined ones.
        let joined = ThreadSummary {
            joined: true,
            ..in_guild(summary(11, 100, false), 1)
        };
        lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Sync {
                guild_id: guild,
                parent_ids: Vec::new(),
                threads: vec![joined],
            },
            50,
        );
        assert!(lists.get(Id::new(10)).is_some());
        assert!(lists.get(Id::new(11)).unwrap().joined);

        // A channel-scoped sync is the whole active set for the channel:
        // thread 10 is gone from it, so it goes from the list.
        lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Sync {
                guild_id: guild,
                parent_ids: vec![parent],
                threads: vec![in_guild(summary(12, 100, false), 1)],
            },
            50,
        );
        assert!(lists.get(Id::new(10)).is_none());
        assert!(lists.get(Id::new(11)).is_none());
        assert!(lists.get(Id::new(12)).is_some());

        // A sync naming a different parent doesn't touch this one.
        lists.apply_event(
            guild,
            parent,
            &ThreadEvent::Sync {
                guild_id: guild,
                parent_ids: vec![Id::new(200)],
                threads: vec![in_guild(summary(13, 200, false), 1)],
            },
            50,
        );
        assert!(lists.get(Id::new(13)).is_none());
    }

    #[test]
    fn membership_events_update_counts_and_membership() {
        let mut lists = ThreadLists::default();
        let guild = Id::new(1);
        lists.upsert(in_guild(summary(10, 100, false), 1), 50);

        lists.apply_event(
            guild,
            Id::new(100),
            &ThreadEvent::Membership {
                guild_id: guild,
                thread_id: Id::new(10),
                member_count: Some(7),
                joined: Some(true),
            },
            50,
        );
        let thread = lists.get(Id::new(10)).unwrap();
        assert_eq!(thread.member_count, Some(7));
        assert!(thread.joined);

        // A removal the server reports for the current user outranks the
        // merge's never-unjoin rule.
        lists.apply_event(
            guild,
            Id::new(100),
            &ThreadEvent::Membership {
                guild_id: guild,
                thread_id: Id::new(10),
                member_count: Some(6),
                joined: Some(false),
            },
            50,
        );
        assert!(!lists.get(Id::new(10)).unwrap().joined);

        // An event for another thread changes nothing here.
        assert!(!lists.apply_event(
            guild,
            Id::new(100),
            &ThreadEvent::Membership {
                guild_id: guild,
                thread_id: Id::new(99),
                member_count: Some(1),
                joined: None,
            },
            50,
        ));
    }

    #[test]
    fn merge_page_dedupes_appends_and_respects_the_cap() {
        let mut lists = ThreadLists::default();
        lists.merge_page(
            vec![summary(10, 100, true), summary(11, 100, true)],
            true,
            3,
        );
        // A second page that repeats a thread doesn't duplicate it.
        lists.merge_page(
            vec![summary(11, 100, true), summary(12, 100, true)],
            true,
            3,
        );
        assert_eq!(
            lists
                .archived
                .iter()
                .map(|thread| thread.id.get())
                .collect::<Vec<_>>(),
            vec![10, 11, 12]
        );

        // At the cap, new pages stop landing.
        lists.merge_page(vec![summary(13, 100, true)], true, 3);
        assert_eq!(lists.archived.len(), 3);

        // Pages of the other list refresh known entries without moving them.
        let mut lists = ThreadLists::default();
        lists.merge_page(vec![summary(10, 100, false)], false, 50);
        lists.merge_page(vec![summary(10, 100, true)], true, 50);
        assert_eq!(lists.active.len(), 1);
        assert!(lists.get(Id::new(10)).unwrap().archived);
    }

    #[test]
    fn archived_cursors_follow_the_scope_route() {
        let list = vec![summary(10, 100, true), summary(11, 100, true)];
        assert_eq!(
            archived_cursor(ArchivedScope::Public, &list),
            Some(ArchivedCursor::Timestamp(list[1].archive_timestamp))
        );
        assert_eq!(
            archived_cursor(ArchivedScope::Private, &list),
            Some(ArchivedCursor::Timestamp(list[1].archive_timestamp))
        );
        assert_eq!(
            archived_cursor(ArchivedScope::JoinedPrivate, &list),
            Some(ArchivedCursor::Id(list[1].id))
        );
        assert_eq!(archived_cursor(ArchivedScope::Public, &[]), None);
    }

    #[test]
    fn optimistic_membership_rolls_back_only_its_own_write() {
        let mut lists = ThreadLists::default();
        lists.upsert(summary(10, 100, false), 50);

        lists.set_joined(Id::new(10), true);
        assert!(lists.get(Id::new(10)).unwrap().joined);
        lists.rollback_joined(Id::new(10), true);
        assert!(!lists.get(Id::new(10)).unwrap().joined);

        // A gateway event that already settled the flag outranks the
        // rollback of the stale optimistic write.
        lists.set_joined(Id::new(10), true);
        lists.set_joined(Id::new(10), false);
        lists.rollback_joined(Id::new(10), true);
        assert!(!lists.get(Id::new(10)).unwrap().joined);
    }

    #[test]
    fn reply_and_create_gates_follow_the_permission_tables() {
        let base = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES;
        let mut locked = summary(10, 100, false);
        locked.locked = true;
        let archived = summary(11, 100, true);

        // Threads take SEND_MESSAGES_IN_THREADS; SEND_MESSAGES alone is not
        // enough, locked or not.
        assert!(!can_reply(base, &summary(10, 100, false)));
        assert!(can_reply(
            base | Permissions::SEND_MESSAGES_IN_THREADS,
            &summary(10, 100, false)
        ));
        // A locked thread answers to the pair: SEND_MESSAGES_IN_THREADS for
        // the thread context, MANAGE_THREADS for the lock — either alone is
        // not enough.
        assert!(!can_reply(
            base | Permissions::SEND_MESSAGES_IN_THREADS,
            &locked
        ));
        assert!(!can_reply(Permissions::MANAGE_THREADS, &locked));
        assert!(can_reply(
            base | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::MANAGE_THREADS,
            &locked
        ));
        // Archived threads take no messages at all.
        assert!(!can_reply(
            base | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::MANAGE_THREADS,
            &archived
        ));

        // Creating: forums take SEND_MESSAGES; text channels split the two
        // CREATE_*_THREADS bits.
        assert!(!can_create_thread(Permissions::VIEW_CHANNEL, true, false));
        assert!(can_create_thread(base, true, false));
        assert!(can_create_thread(Permissions::SEND_MESSAGES, true, false));
        assert!(!can_create_thread(Permissions::SEND_MESSAGES, false, false));
        assert!(!can_create_thread(Permissions::SEND_MESSAGES, false, true));
        assert!(can_create_thread(
            Permissions::CREATE_PUBLIC_THREADS,
            false,
            false
        ));
        assert!(can_create_thread(
            Permissions::CREATE_PRIVATE_THREADS,
            false,
            true
        ));

        // Joining follows the archive state, not a permission.
        assert!(can_join(&summary(10, 100, false)));
        assert!(!can_join(&archived));

        // Archived listings all take READ_MESSAGE_HISTORY, and the private
        // one takes MANAGE_THREADS on top.
        let history = Permissions::READ_MESSAGE_HISTORY;
        assert!(!can_list_archived(
            Permissions::VIEW_CHANNEL,
            ArchivedScope::Public
        ));
        assert!(can_list_archived(history, ArchivedScope::Public));
        assert!(can_list_archived(history, ArchivedScope::JoinedPrivate));
        assert!(!can_list_archived(history, ArchivedScope::Private));
        assert!(can_list_archived(
            history | Permissions::MANAGE_THREADS,
            ArchivedScope::Private
        ));

        // Archiving is a moderator or the creator; unarchiving a locked
        // thread needs MANAGE_THREADS, an unlocked one is open to members.
        assert!(can_archive(
            Permissions::MANAGE_THREADS,
            &locked,
            true,
            false
        ));
        assert!(can_archive(
            Permissions::empty(),
            &summary(10, 100, false),
            true,
            true
        ));
        assert!(!can_archive(
            Permissions::empty(),
            &summary(10, 100, false),
            true,
            false
        ));
        assert!(can_archive(
            Permissions::empty(),
            &summary(10, 100, false),
            false,
            false
        ));
        assert!(!can_archive(Permissions::empty(), &locked, false, false));
        assert!(can_archive(
            Permissions::MANAGE_THREADS,
            &locked,
            false,
            false
        ));
    }
}
