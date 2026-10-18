//! Opening a conversation, at the first unread message when there is one, and
//! paging through its history either way from there.

use gpui::*;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker},
};

use crate::discord::{self, MessagePage};
use crate::screens::home::navigation::NavigationEntry;
use crate::screens::home::{HomeScreen, MessageTarget, NavigateBack, NavigateForward, View};

impl HomeScreen {
    pub(in crate::screens::home) fn select_channel(
        &mut self,
        channel_id: Id<ChannelMarker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_message_target = None;
        self.apply_channel_selection(channel_id, true, window, cx);
    }

    pub(in crate::screens::home) fn jump_to_message(
        &mut self,
        target: MessageTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target.guild_id {
            Some(guild_id) => {
                self.pending_message_target = Some(target);
                self.open_guild_channel(guild_id, Some(target.channel_id), true, window, cx);
            }
            None => {
                if self.view != View::DirectMessages {
                    self.apply_direct_messages_home(true, window, cx);
                }
                self.pending_message_target = Some(target);
                self.apply_channel_selection(target.channel_id, true, window, cx);
            }
        }
        if self.selected_channel == Some(target.channel_id)
            && self.pending_message_target == Some(target)
        {
            self.load_messages(window, cx);
        }
    }

    /// Selects `channel_id` in the active view and loads its messages. When
    /// `record_history` is `true`, the destination is pushed onto the
    /// navigation stack; back/forward traversal passes `false` so stepping
    /// through history doesn't record a fresh visit or truncate forward entries.
    pub(in crate::screens::home) fn apply_channel_selection(
        &mut self,
        channel_id: Id<ChannelMarker>,
        record_history: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view == View::Guild
            && let Some(thread) = self.threads.get(channel_id).cloned()
        {
            self.remember_thread(&thread);
        }
        if self.selected_channel == Some(channel_id) {
            return;
        }
        self.save_collapsed_categories();
        self.selected_channel = Some(channel_id);
        if self
            .channel_info(channel_id)
            .is_some_and(|channel| channel.kind.is_thread())
        {
            self.threads_open = false;
        }
        self.sync_thread_parent(window, cx);
        self.message_tools_selection_changed(cx);
        let entry = match self.view {
            View::DirectMessages => Some(NavigationEntry::DirectMessage { channel_id }),
            View::Guild => self
                .selected_guild
                .map(|guild_id| NavigationEntry::GuildChannel {
                    guild_id,
                    channel_id,
                }),
        };
        if let Some(entry) = entry {
            if record_history {
                self.navigation_history.push(entry);
            } else {
                self.navigation_history.replace_current(entry);
            }
        }
        self.load_messages(window, cx);
    }

    pub(in crate::screens::home) fn on_navigate_back(
        &mut self,
        _: &NavigateBack,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_back(window, cx);
    }

    pub(in crate::screens::home) fn on_navigate_forward(
        &mut self,
        _: &NavigateForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_forward(window, cx);
    }

    pub(in crate::screens::home) fn navigate_back(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.step_history(false) else {
            return;
        };
        self.restore_navigation_entry(entry, window, cx);
    }

    pub(in crate::screens::home) fn navigate_forward(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.step_history(true) else {
            return;
        };
        self.restore_navigation_entry(entry, window, cx);
    }

    fn step_history(&mut self, forward: bool) -> Option<NavigationEntry> {
        let guilds = &self.guilds;
        let selected_guild = self.selected_guild;
        let channels_loading = self.channels_loading;
        let channel_groups = &self.channel_groups;
        let thread_channels = &self.thread_channels;
        let dms_loaded = self.dms_loaded;
        let dms = &self.dms;

        let is_valid = |entry: NavigationEntry| match entry {
            NavigationEntry::DirectMessagesHome => true,
            NavigationEntry::DirectMessage { channel_id } => {
                !dms_loaded || dms.iter().any(|dm| dm.id == channel_id)
            }
            NavigationEntry::GuildChannel {
                guild_id,
                channel_id,
            } => {
                guilds.iter().any(|guild| guild.id == guild_id)
                    && (selected_guild != Some(guild_id)
                        || channels_loading
                        || channel_groups
                            .iter()
                            .flat_map(|group| &group.channels)
                            .any(|channel| channel.id == channel_id && !channel.kind.is_voice())
                        || thread_channels
                            .get(&channel_id)
                            .is_some_and(|(guild, _)| *guild == guild_id))
            }
        };

        if forward {
            self.navigation_history.navigate_forward(is_valid)
        } else {
            self.navigation_history.navigate_back(is_valid)
        }
    }

    fn restore_navigation_entry(
        &mut self,
        entry: NavigationEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_message_target = None;
        match entry {
            NavigationEntry::DirectMessagesHome => {
                self.apply_direct_messages_home(false, window, cx);
            }
            NavigationEntry::DirectMessage { channel_id } => {
                if self.view != View::DirectMessages {
                    self.save_collapsed_categories();
                    self.view = View::DirectMessages;
                    self.selected_channel = None;
                    if !self.dms_loaded && !self.dms_loading {
                        self.load_dms(cx);
                    }
                }
                self.apply_channel_selection(channel_id, false, window, cx);
            }
            NavigationEntry::GuildChannel {
                guild_id,
                channel_id,
            } => {
                self.open_guild_channel(guild_id, Some(channel_id), false, window, cx);
            }
        }
    }

    fn load_messages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        let target = self.pending_message_target.take().filter(|target| {
            target.channel_id == channel_id
                && match self.view {
                    View::DirectMessages => target.guild_id.is_none(),
                    View::Guild => target.guild_id == self.selected_guild,
                }
        });
        let placeholder = match self.view {
            View::DirectMessages => self
                .selected_dm_info()
                .map(|dm| format!("Message @{}", dm.name)),
            View::Guild => self
                .selected_channel_info()
                .map(|channel| format!("Message #{}", channel.name)),
        };
        // Where the user had read up to, taken before opening acks it.
        let marker = self
            .unreads
            .is_unread(channel_id)
            .then(|| self.unreads.acked(channel_id))
            .flatten();
        self.reset_conversation(placeholder, window, cx);
        if self.view == View::Guild
            && self
                .selected_channel_info()
                .is_some_and(|channel| channel.kind == discord::ChannelKind::Forum)
        {
            cx.notify();
            return;
        }
        self.unread_marker = marker;
        self.messages_loading = true;
        if let Some(target) = target {
            self.fetch_message_target(target, window, cx);
        } else {
            self.fetch_opening_page(channel_id, marker, window, cx);
        }
    }

    /// Leaves history opened further back for the newest page, as Discord's
    /// "Jump To Present" does.
    pub(in crate::screens::home) fn jump_to_present(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        if self.messages_loading {
            return;
        }
        self.message_load_generation = self.message_load_generation.wrapping_add(1);
        self.highlighted_message = None;
        self.messages.clear();
        self.messages_list.reset(0);
        self.reactors.clear();
        self.older_loading = false;
        self.newer_loading = false;
        self.at_bottom = true;
        self.messages_loading = true;
        self.fetch_opening_page(channel_id, None, window, cx);
        cx.notify();
    }

    /// Fetches the page a conversation opens on: the newest, scrolled to the
    /// "new" divider when `unread_after` — the last message read — falls in
    /// it. When it falls further back, the unread run is longer than a page,
    /// and the conversation opens where the user left off instead, as
    /// Discord's does, to be paged forward from there.
    fn fetch_opening_page(
        &mut self,
        channel_id: Id<ChannelMarker>,
        unread_after: Option<Id<MessageMarker>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.message_load_generation;
        cx.spawn_in(window, async move |this, cx| {
            let page_size = discord::MESSAGE_PAGE_SIZE;
            let mut result = discord::fetch_messages(channel_id, MessagePage::Latest)
                .await
                .map(|page| (page, true));
            if let (Ok((latest, _)), Some(marker)) = (&result, unread_after)
                && latest.len() == page_size
                && latest.first().is_some_and(|first| first.id > marker)
            {
                result = discord::fetch_messages(channel_id, MessagePage::After(marker))
                    .await
                    .map(|page| (page, false));
            }
            let _ = this.update_in(cx, |this, window, cx| {
                // The user may have clicked another channel while this request
                // was in flight; drop the stale response.
                if this.selected_channel != Some(channel_id)
                    || this.message_load_generation != generation
                {
                    return;
                }
                this.messages_loading = false;
                match result {
                    Ok((messages, latest)) => {
                        let full = messages.len() == page_size;
                        this.reached_oldest = latest && !full;
                        this.reached_newest = latest || !full;
                        this.messages = messages;
                        this.request_author_members();
                        // Resetting also snaps the bottom-aligned list to the
                        // newest message.
                        this.messages_list.reset(this.messages.len());
                        // Opened just past the last read message, the read
                        // ones above it give the divider something to sit
                        // under.
                        if !latest {
                            this.load_older_messages(cx);
                        }
                        if unread_after.is_some() {
                            this.scroll_to_unread(window, cx);
                        }
                    }
                    Err(err) => this.messages_error = Some(err),
                }
                this.ack_open_conversation(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_message_target(
        &mut self,
        target: MessageTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.message_load_generation;
        cx.spawn_in(window, async move |this, cx| {
            let result =
                discord::fetch_messages(target.channel_id, MessagePage::Around(target.message_id))
                    .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.selected_channel != Some(target.channel_id)
                    || this.message_load_generation != generation
                {
                    return;
                }
                this.messages_loading = false;
                match result {
                    Ok(messages) => {
                        if let Some(index) = messages
                            .iter()
                            .position(|message| message.id == target.message_id)
                        {
                            let complete = messages.len() < discord::MESSAGE_PAGE_SIZE;
                            this.reached_oldest = complete;
                            this.reached_newest = complete;
                            this.messages = messages;
                            this.request_author_members();
                            this.messages_list.reset(this.messages.len());
                            this.messages_list.scroll_to(ListOffset {
                                item_ix: index,
                                offset_in_item: px(0.),
                            });
                            this.highlighted_message = Some(target.message_id);
                            this.at_bottom = false;
                        } else {
                            this.messages_error = Some(
                                "That message is unavailable, or you cannot read its history."
                                    .into(),
                            );
                        }
                    }
                    Err(error) => this.messages_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Puts the first unread message at the top of the view.
    ///
    /// The list stops short of scrolling past its end, so when the unread
    /// all fit on screen this lands at the bottom after all, and they've been
    /// read. That's only known once the list has been laid out, so the check
    /// waits for it: next-frame callbacks run before the frame they're queued
    /// for is drawn, hence two of them.
    fn scroll_to_unread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = match self.unread_divider_index() {
            Some(ix) => ix,
            None if !self.reached_newest => 0,
            None => return,
        };
        self.messages_list.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(0.),
        });
        self.at_bottom = false;

        let screen = cx.entity().downgrade();
        let generation = self.message_load_generation;
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, cx| {
                let _ = screen.update(cx, |this, cx| {
                    if this.message_load_generation != generation {
                        return;
                    }
                    let list = &this.messages_list;
                    // Pinned to the bottom, the list reports its top as past
                    // the last item.
                    if !this.at_bottom && list.logical_scroll_top().item_ix >= list.item_count() {
                        this.at_bottom = true;
                        this.ack_open_conversation(cx);
                    }
                });
            });
        });
    }

    /// Empties the message pane and everything tied to the conversation that
    /// was open, ready for the next one (or none).
    pub(in crate::screens::home) fn reset_conversation(
        &mut self,
        placeholder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.message_load_generation = self.message_load_generation.wrapping_add(1);
        self.highlighted_message = None;
        self.messages.clear();
        self.messages_list.reset(0);
        self.reactors.clear();
        // Release the previous conversation's decoded images instead of letting
        // them accumulate; the next one repopulates the cache as it renders.
        self.image_cache
            .update(cx, |cache, cx| cache.clear(window, cx));
        self.stop_video(window, cx);
        self.messages_error = None;
        self.older_loading = false;
        self.reached_oldest = false;
        self.newer_loading = false;
        self.reached_newest = true;
        // A freshly opened conversation starts pinned to its newest message, so
        // live messages should follow along until the user scrolls up.
        self.at_bottom = true;
        self.send_error = None;
        self.replying_to = None;
        self.editing = None;
        self.revealed_spoilers.clear();
        self.unread_marker = None;
        self.marked_unread = false;
        for attachment in self.pending_attachments.drain(..) {
            attachment.release_preview(cx);
        }
        let placeholder = placeholder.unwrap_or_else(|| "Send a message".into());
        self.message_input.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx);
        });
        cx.notify();
    }

    /// Fetches the page of messages older than the oldest loaded one and
    /// prepends it, keeping the current scroll position.
    pub(in crate::screens::home) fn load_older_messages(&mut self, cx: &mut Context<Self>) {
        if self.messages_loading || self.older_loading || self.reached_oldest {
            return;
        }
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        let Some(oldest_id) = self.messages.first().map(|message| message.id) else {
            return;
        };
        self.older_loading = true;
        let generation = self.message_load_generation;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = discord::fetch_messages(channel_id, MessagePage::Before(oldest_id)).await;
            let _ = this.update(cx, |this, cx| {
                // Drop the response if the channel changed or the messages
                // were reloaded while this request was in flight.
                if this.selected_channel != Some(channel_id)
                    || this.message_load_generation != generation
                    || this.messages.first().map(|message| message.id) != Some(oldest_id)
                {
                    return;
                }
                this.older_loading = false;
                // A failed fetch leaves the messages on screen; it retries the
                // next time the user scrolls near the top.
                if let Ok(older) = result {
                    if older.len() < discord::MESSAGE_PAGE_SIZE {
                        this.reached_oldest = true;
                    }
                    let count = older.len();
                    if count > 0 {
                        this.messages.splice(0..0, older);
                        this.request_author_members();
                        this.messages_list.splice(0..0, count);
                        // The divider may now belong above what was the
                        // oldest row, which was measured without it.
                        if this.unread_divider_index() == Some(count) {
                            this.messages_list.splice(count..count + 1, 1);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches the page newer than the newest loaded message and appends it,
    /// while the conversation is open further back than its present.
    pub(in crate::screens::home) fn load_newer_messages(&mut self, cx: &mut Context<Self>) {
        if self.messages_loading || self.newer_loading || self.reached_newest {
            return;
        }
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        let Some(newest_id) = self.messages.last().map(|message| message.id) else {
            return;
        };
        self.newer_loading = true;
        let generation = self.message_load_generation;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = discord::fetch_messages(channel_id, MessagePage::After(newest_id)).await;
            let _ = this.update(cx, |this, cx| {
                if this.selected_channel != Some(channel_id)
                    || this.message_load_generation != generation
                    || this.messages.last().map(|message| message.id) != Some(newest_id)
                {
                    return;
                }
                this.newer_loading = false;
                // A failed fetch retries the next time the user scrolls near
                // the end.
                if let Ok(newer) = result {
                    if newer.len() < discord::MESSAGE_PAGE_SIZE {
                        this.reached_newest = true;
                    }
                    let count = newer.len();
                    if count > 0 {
                        // Scrolled right to the end, the list is pinned to its
                        // bottom and would ride the new rows down past every
                        // one of them. Pin it where it stands instead: a
                        // viewport up from the end.
                        let list = &this.messages_list;
                        if list.logical_scroll_top().item_ix >= list.item_count() {
                            list.scroll_by(-list.viewport_bounds().size.height);
                        }
                        let ix = this.messages.len();
                        this.messages.extend(newer);
                        this.request_author_members();
                        this.splice_messages(ix..ix, count);
                        this.at_bottom = false;
                    }
                }
                this.ack_open_conversation(cx);
                cx.notify();
            });
        })
        .detach();
    }
}
