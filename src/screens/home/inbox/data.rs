//! Data operations and event hooks for [`super::InboxState`] on [`HomeScreen`]:
//! server-paginated mentions (`GET /users/@me/mentions`), mention deletion
//! (`DELETE /users/@me/mentions/{id}`), single and bulk unread acks
//! (`POST /channels/{id}/messages/{id}/ack`, `POST /read-states/ack-bulk`),
//! lazy unread preview fetching, jump-to-message navigation, and native desktop
//! toast evaluation.

use gpui::*;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
};

use crate::discord;
use crate::discord::inbox::BulkAckEntry;
use crate::platform::notifications::{
    NotificationCandidate, NotificationPreferences, NotificationTarget,
};
use crate::platform::prefs;
use crate::screens::home::{HomeScreen, MessageTarget, SessionEnded};

use super::{
    InboxTab, MAX_CONCURRENT_PREVIEW_FETCHES, MAX_INBOX_UNREAD_CHANNELS,
    MAX_UNREAD_PREVIEW_MESSAGES, TrackedChannelMeta, UnreadChannelEntry, UnreadPreviewState,
    message_mentions_user,
};

impl HomeScreen {
    /// Toggles the Inbox popover open or closed.
    pub(in crate::screens::home) fn toggle_inbox(&mut self, cx: &mut Context<Self>) {
        if self.inbox.open {
            self.close_inbox(cx);
        } else {
            self.open_inbox(cx);
        }
    }

    /// Opens the Inbox popover and triggers an initial load for the active tab
    /// if needed.
    pub(in crate::screens::home) fn open_inbox(&mut self, cx: &mut Context<Self>) {
        self.inbox.open = true;
        self.inbox.action_error = None;
        self.inbox.unread_mentions_badge = 0;

        match self.inbox.active_tab {
            InboxTab::Mentions => {
                if !self.inbox.mentions_loaded && !self.inbox.mentions_loading {
                    self.load_mentions(false, cx);
                }
            }
            InboxTab::Unreads => {
                self.materialize_inbox_unreads(cx);
            }
        }
        cx.notify();
    }

    /// Closes the Inbox popover.
    pub(in crate::screens::home) fn close_inbox(&mut self, cx: &mut Context<Self>) {
        if self.inbox.open {
            self.inbox.open = false;
            self.inbox.unreads_coalesce_task = None;
            self.inbox.action_error = None;
            cx.notify();
        }
    }

    /// Switches between [`InboxTab::Mentions`] and [`InboxTab::Unreads`].
    pub(in crate::screens::home) fn set_inbox_tab(
        &mut self,
        tab: InboxTab,
        cx: &mut Context<Self>,
    ) {
        if self.inbox.active_tab == tab {
            return;
        }
        self.inbox.active_tab = tab;
        self.inbox.action_error = None;
        match tab {
            InboxTab::Mentions => {
                self.inbox.unreads_coalesce_task = None;
                self.inbox.unread_mentions_badge = 0;
                if !self.inbox.mentions_loaded && !self.inbox.mentions_loading {
                    self.load_mentions(false, cx);
                }
            }
            InboxTab::Unreads => {
                self.materialize_inbox_unreads(cx);
            }
        }
        cx.notify();
    }

    /// Toggles the `@everyone` filter in the Mentions tab and refetches from
    /// the server.
    pub(in crate::screens::home) fn toggle_mentions_everyone(&mut self, cx: &mut Context<Self>) {
        self.inbox.filter.include_everyone = !self.inbox.filter.include_everyone;
        self.load_mentions(false, cx);
    }

    /// Toggles the `@roles` filter in the Mentions tab and refetches from the
    /// server.
    pub(in crate::screens::home) fn toggle_mentions_roles(&mut self, cx: &mut Context<Self>) {
        self.inbox.filter.include_roles = !self.inbox.filter.include_roles;
        self.load_mentions(false, cx);
    }

    /// Toggles filtering mentions to the currently selected guild and refetches
    /// from the server.
    pub(in crate::screens::home) fn toggle_mentions_current_guild(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.inbox.filter.current_guild_only = !self.inbox.filter.current_guild_only;
        self.load_mentions(false, cx);
    }

    /// Updates desktop notification preferences at runtime and persists them
    /// to `prefs.json`.
    pub(in crate::screens::home) fn set_notification_preferences(
        &mut self,
        prefs: NotificationPreferences,
        cx: &mut Context<Self>,
    ) {
        self.inbox.notifier.set_preferences(prefs);
        prefs::update(|stored| stored.notifications = prefs);
        cx.notify();
    }

    /// Current desktop notification preferences.
    pub(in crate::screens::home) fn notification_preferences(&self) -> NotificationPreferences {
        self.inbox.notifier.preferences()
    }

    /// Hook for `GatewayEvent::Ready`: clears session-specific unread tracking
    /// while preserving user notification preferences.
    pub(in crate::screens::home) fn inbox_on_ready(
        &mut self,
        user_id: Id<UserMarker>,
        cx: &mut Context<Self>,
    ) {
        self.self_user_id = Some(user_id);
        self.inbox.unreads.clear();
        self.inbox.unreads_dirty = true;
        self.inbox.unreads_coalesce_task = None;
        self.inbox.tracked_channels.clear();
        self.inbox.cached_previews.clear();
        self.inbox.muted_guilds.clear();
        self.inbox.loading_guild_channels.clear();
        self.inbox.loading_previews.clear();
        self.inbox.unreads_generation = self.inbox.unreads_generation.wrapping_add(1);
        cx.notify();
    }

    /// Hook for `GatewayEvent::ReadStates`: registers channels with read states
    /// and marks the unread list dirty.
    pub(in crate::screens::home) fn inbox_on_read_states(
        &mut self,
        states: &[discord::ReadState],
        cx: &mut Context<Self>,
    ) {
        for state in states {
            self.inbox
                .tracked_channels
                .entry(state.channel_id)
                .or_insert(TrackedChannelMeta {
                    channel_id: state.channel_id,
                    guild_id: None,
                    parent_id: None,
                    name: None,
                    is_dm: false,
                    dm_avatar_url: None,
                    recipient_id: None,
                    unavailable_reason: None,
                });
        }
        self.rebuild_inbox_unreads(cx);
    }

    /// Hook for `GatewayEvent::GuildUnreads`: registers every visible text-like
    /// channel in the guild from `READY` or `GUILD_CREATE` in O(channels)
    /// without materializing or sorting the unread list when closed.
    pub(in crate::screens::home) fn inbox_on_guild_unreads(
        &mut self,
        guild: &discord::GuildUnreads,
        cx: &mut Context<Self>,
    ) {
        for ch in &guild.channels {
            let entry = self
                .inbox
                .tracked_channels
                .entry(ch.id)
                .or_insert(TrackedChannelMeta {
                    channel_id: ch.id,
                    guild_id: Some(guild.guild_id),
                    parent_id: ch.parent_id,
                    name: None,
                    is_dm: false,
                    dm_avatar_url: None,
                    recipient_id: None,
                    unavailable_reason: None,
                });
            entry.guild_id = Some(guild.guild_id);
            entry.parent_id = ch.parent_id;
            entry.is_dm = false;
        }
        self.rebuild_inbox_unreads(cx);
    }

    /// Hook for `GatewayEvent::LatestMessages`: registers private/guild channels
    /// with latest message IDs from `READY` and passive updates.
    pub(in crate::screens::home) fn inbox_on_latest_messages(
        &mut self,
        channels: &[discord::ChannelLatest],
        cx: &mut Context<Self>,
    ) {
        for ch in channels {
            self.inbox
                .tracked_channels
                .entry(ch.channel_id)
                .or_insert(TrackedChannelMeta {
                    channel_id: ch.channel_id,
                    guild_id: None,
                    parent_id: None,
                    name: None,
                    is_dm: true,
                    dm_avatar_url: None,
                    recipient_id: None,
                    unavailable_reason: None,
                });
        }
        self.rebuild_inbox_unreads(cx);
    }

    /// Hook for `GatewayEvent::MuteSettings`: updates guild mute state and
    /// marks unreads dirty.
    pub(in crate::screens::home) fn inbox_on_mute_settings(
        &mut self,
        settings: &[discord::MuteSettings],
        cx: &mut Context<Self>,
    ) {
        for item in settings {
            if let Some(guild_id) = item.guild_id {
                if item.muted {
                    self.inbox.muted_guilds.insert(guild_id);
                } else {
                    self.inbox.muted_guilds.remove(&guild_id);
                }
            }
        }
        self.rebuild_inbox_unreads(cx);
    }

    /// Hook for `GatewayEvent::MessageAck` (and local acks): marks unreads
    /// dirty when a channel is read or marked unread.
    pub(in crate::screens::home) fn inbox_on_message_ack(
        &mut self,
        channel_id: Id<ChannelMarker>,
        _message_id: Id<MessageMarker>,
        _manual: bool,
        cx: &mut Context<Self>,
    ) {
        let _ = channel_id;
        self.rebuild_inbox_unreads(cx);
    }

    /// Hook called when `fetch_guilds` completes so guild names and icons are
    /// filled in on existing mention entries and unreads are marked dirty.
    pub(in crate::screens::home) fn inbox_on_guilds_loaded(&mut self, cx: &mut Context<Self>) {
        for mention in &mut self.inbox.mentions {
            if let Some(guild_id) = mention.guild_id
                && let Some(guild) = self.guilds.iter().find(|g| g.id == guild_id)
            {
                mention.guild_name = Some(guild.name.clone());
                mention.guild_icon_url = guild.icon_url.clone();
            }
        }
        self.rebuild_inbox_unreads(cx);
    }

    /// Hook called when `fetch_channels` completes for `guild_id`: caches
    /// human-readable channel names and marks any vanished channels as
    /// unavailable/permission-lost.
    pub(in crate::screens::home) fn inbox_on_channels_loaded(
        &mut self,
        guild_id: Id<GuildMarker>,
        channels: &[discord::Channel],
        cx: &mut Context<Self>,
    ) {
        let visible_ids: std::collections::HashSet<Id<ChannelMarker>> =
            channels.iter().map(|ch| ch.id).collect();

        for ch in channels {
            if ch.kind == discord::ChannelKind::Category || ch.kind.is_voice() {
                continue;
            }
            let meta = self
                .inbox
                .tracked_channels
                .entry(ch.id)
                .or_insert(TrackedChannelMeta {
                    channel_id: ch.id,
                    guild_id: Some(guild_id),
                    parent_id: ch.parent_id,
                    name: Some(ch.name.clone()),
                    is_dm: false,
                    dm_avatar_url: None,
                    recipient_id: None,
                    unavailable_reason: None,
                });
            meta.guild_id = Some(guild_id);
            meta.parent_id = ch.parent_id;
            meta.name = Some(ch.name.clone());
            meta.is_dm = false;
            meta.unavailable_reason = None;
        }

        for meta in self.inbox.tracked_channels.values_mut() {
            if meta.guild_id == Some(guild_id) && !visible_ids.contains(&meta.channel_id) {
                meta.unavailable_reason =
                    Some("Channel deleted or view permission revoked.".into());
            }
        }

        for mention in &mut self.inbox.mentions {
            if mention.guild_id == Some(guild_id)
                && let Some(ch) = channels.iter().find(|c| c.id == mention.channel_id)
            {
                mention.channel_name = ch.name.clone();
            }
        }

        self.rebuild_inbox_unreads(cx);
    }

    /// Hook called when `fetch_dms` completes: caches DM conversation names,
    /// avatars, and recipient IDs for unread and blocked-user filtering.
    pub(in crate::screens::home) fn inbox_on_dms_loaded(
        &mut self,
        dms: &[discord::DirectMessage],
        cx: &mut Context<Self>,
    ) {
        for dm in dms {
            let meta = self
                .inbox
                .tracked_channels
                .entry(dm.id)
                .or_insert(TrackedChannelMeta {
                    channel_id: dm.id,
                    guild_id: None,
                    parent_id: None,
                    name: Some(dm.name.clone()),
                    is_dm: true,
                    dm_avatar_url: dm.avatar_url.clone(),
                    recipient_id: dm.recipient_id,
                    unavailable_reason: None,
                });
            meta.name = Some(dm.name.clone());
            meta.is_dm = true;
            meta.dm_avatar_url = dm.avatar_url.clone();
            meta.recipient_id = dm.recipient_id;
            meta.unavailable_reason = None;
        }
        self.rebuild_inbox_unreads(cx);
    }

    /// Hook for `GatewayEvent::Message`: records channel metadata in O(1),
    /// inserts real live mentions, and evaluates native desktop toasts only when
    /// opted in — avoiding preview/candidate allocations while closed or opted out.
    pub(in crate::screens::home) fn inbox_on_incoming_message(
        &mut self,
        incoming: &discord::IncomingMessage,
        cx: &mut Context<Self>,
    ) {
        let author_blocked = self.is_inbox_user_blocked(incoming.message.author_id);
        let is_self = self.self_user_id == Some(incoming.message.author_id);

        let meta = self
            .inbox
            .tracked_channels
            .entry(incoming.channel_id)
            .or_insert_with(|| TrackedChannelMeta {
                channel_id: incoming.channel_id,
                guild_id: incoming.guild_id,
                parent_id: None,
                name: (incoming.guild_id.is_none()).then(|| incoming.message.author_name.clone()),
                is_dm: incoming.guild_id.is_none(),
                dm_avatar_url: (incoming.guild_id.is_none())
                    .then(|| incoming.message.author_avatar_url.clone())
                    .flatten(),
                recipient_id: (incoming.guild_id.is_none()).then_some(incoming.message.author_id),
                unavailable_reason: None,
            });
        if incoming.guild_id.is_some() {
            meta.guild_id = incoming.guild_id;
            meta.is_dm = false;
        }

        // Only clone incoming messages into cached_previews when the Unreads tab
        // is actively open or this channel already has a loaded preview cache.
        let keep_preview_live = (self.inbox.open && self.inbox.active_tab == InboxTab::Unreads)
            || self
                .inbox
                .cached_previews
                .contains_key(&incoming.channel_id);
        if !author_blocked && keep_preview_live {
            let preview_list = self
                .inbox
                .cached_previews
                .entry(incoming.channel_id)
                .or_default();
            if !preview_list.iter().any(|m| m.id == incoming.message.id) {
                preview_list.push(incoming.message.clone());
                if preview_list.len() > MAX_UNREAD_PREVIEW_MESSAGES {
                    let excess = preview_list.len() - MAX_UNREAD_PREVIEW_MESSAGES;
                    preview_list.drain(0..excess);
                }
            }
        }

        let user_roles = self.signed_in_roles_for_guild(incoming.guild_id);
        let is_mention = !author_blocked
            && !is_self
            && message_mentions_user(
                &incoming.message,
                self.self_user_id,
                &user_roles,
                true,
                true,
            );
        let notif_enabled = self.inbox.notifier.preferences().enabled;

        // Only resolve human-readable channel/guild names when a real mention
        // arrived or desktop toast notifications are enabled.
        if is_mention || (notif_enabled && !author_blocked && !is_self) {
            let (channel_name, guild_name, guild_icon_url) =
                self.resolve_inbox_location(incoming.channel_id, incoming.guild_id);

            if is_mention {
                let channel_is_unread = self.unreads.is_unread(incoming.channel_id);
                let prev_badge = self.inbox.unread_mentions_badge;
                self.inbox.push_live_mention(
                    incoming,
                    (self.self_user_id, &user_roles),
                    self.selected_guild,
                    author_blocked,
                    (channel_name.clone(), guild_name.clone(), guild_icon_url),
                    channel_is_unread,
                );
                if self.inbox.unread_mentions_badge != prev_badge || self.inbox.open {
                    cx.notify();
                }
            }

            if notif_enabled && !author_blocked && !is_self {
                let parent_id = self.resolve_channel_parent(incoming.channel_id);
                let channel_muted = self.unreads.is_muted(incoming.channel_id, parent_id);
                let guild_muted = incoming
                    .guild_id
                    .is_some_and(|gid| self.inbox.muted_guilds.contains(&gid));
                let is_dnd = self.presence_status == Some(discord::PresenceStatus::DoNotDisturb);
                let channel_visible_at_bottom = self.selected_channel == Some(incoming.channel_id)
                    && !self.messages_loading
                    && self.reached_newest
                    && self.at_bottom;

                let candidate = NotificationCandidate {
                    target: NotificationTarget {
                        guild_id: incoming.guild_id,
                        channel_id: incoming.channel_id,
                        message_id: incoming.message.id,
                    },
                    author_id: incoming.message.author_id,
                    author_name: incoming.message.author_name.clone(),
                    channel_name,
                    guild_name,
                    content: incoming.message.content.clone(),
                    has_attachments_or_embeds: !incoming.message.images.is_empty()
                        || !incoming.message.videos.is_empty()
                        || !incoming.message.embeds.is_empty(),
                    is_mention,
                    self_user_id: self.self_user_id,
                    is_dnd,
                    channel_muted,
                    guild_muted,
                    author_blocked,
                    window_active: self.inbox.window_active,
                    channel_visible_at_bottom,
                };
                let _ = self.inbox.notifier.notify(&candidate);
            }
        }

        self.rebuild_inbox_unreads(cx);
    }

    /// Hook for `GatewayEvent::MessageUpdate`: updates edited message content in
    /// mentions and cached unread previews.
    pub(in crate::screens::home) fn inbox_on_message_update(
        &mut self,
        incoming: &discord::IncomingMessage,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for entry in &mut self.inbox.mentions {
            if entry.message.id == incoming.message.id {
                entry.message.apply_edit(incoming.message.clone());
                changed = true;
            }
        }
        if let Some(cached) = self.inbox.cached_previews.get_mut(&incoming.channel_id) {
            for msg in cached {
                if msg.id == incoming.message.id {
                    msg.apply_edit(incoming.message.clone());
                    changed = true;
                }
            }
        }
        if changed {
            self.rebuild_inbox_unreads(cx);
            if self.inbox.open {
                cx.notify();
            }
        }
    }

    /// Hook for `GatewayEvent::MessageDelete`: removes deleted messages from
    /// mentions and unread previews.
    pub(in crate::screens::home) fn inbox_on_message_delete(
        &mut self,
        channel_id: Id<ChannelMarker>,
        message_ids: &[Id<MessageMarker>],
        cx: &mut Context<Self>,
    ) {
        if self.inbox.remove_deleted_messages(channel_id, message_ids) {
            self.rebuild_inbox_unreads(cx);
            if self.inbox.open {
                cx.notify();
            }
        }
    }

    /// Hook for logout: resets all inbox state, cancels in-flight fetches, and
    /// clears desktop notifications.
    pub(in crate::screens::home) fn inbox_on_logout(&mut self, cx: &mut Context<Self>) {
        self.inbox.reset_on_logout();
        cx.notify();
    }

    /// Fetches the first or next page of recent mentions from Discord
    /// (`GET /users/@me/mentions`).
    pub(in crate::screens::home) fn load_mentions(&mut self, append: bool, cx: &mut Context<Self>) {
        let (generation, before) = if append {
            if self.inbox.mentions_loading
                || self.inbox.mentions_loading_more
                || self.inbox.mentions_complete
            {
                return;
            }
            let Some(oldest_id) = self.inbox.mentions.last().map(|entry| entry.message.id) else {
                return;
            };
            self.inbox.mentions_loading_more = true;
            self.inbox.mentions_error = None;
            (self.inbox.mentions_generation, Some(oldest_id))
        } else {
            let generation = self.inbox.cancel_mentions_fetch();
            self.inbox.mentions_loading = true;
            self.inbox.mentions_error = None;
            self.inbox.mentions_complete = false;
            (generation, None)
        };

        let query = self.inbox.filter.to_query(before, self.selected_guild);
        cx.notify();

        self.inbox.mentions_task = Some(cx.spawn(async move |this, cx| {
            let result = discord::inbox::fetch_mentions(query).await;
            let _ = this.update(cx, |this, cx| {
                if result.is_err() && discord::load_token().is_none() {
                    cx.emit(SessionEnded);
                    return;
                }
                let self_id = this.self_user_id;
                let member_roles = this.member_roles.clone();
                let guilds = this.guilds.clone();
                let channel_groups = this
                    .channel_groups
                    .iter()
                    .flat_map(|g| g.channels.iter().map(|c| (c.id, c.name.clone())))
                    .collect::<std::collections::HashMap<_, _>>();
                let dms = this
                    .dms
                    .iter()
                    .map(|d| (d.id, d.name.clone()))
                    .collect::<std::collections::HashMap<_, _>>();
                let tracked = this.inbox.tracked_channels.clone();
                let blocked_fallback = this.inbox.blocked_users.clone();

                this.inbox.apply_mentions_page(
                    generation,
                    append,
                    result,
                    self_id,
                    |guild_id, author_id| {
                        let roles = guild_id
                            .zip(self_id)
                            .and_then(|(gid, uid)| member_roles.get(&gid)?.get(&uid))
                            .cloned()
                            .unwrap_or_default();
                        (blocked_fallback.contains(&author_id), roles)
                    },
                    |channel_id, guild_id| {
                        let ch_name = channel_groups
                            .get(&channel_id)
                            .cloned()
                            .or_else(|| dms.get(&channel_id).cloned())
                            .or_else(|| tracked.get(&channel_id).and_then(|t| t.name.clone()))
                            .unwrap_or_else(|| format!("channel-{}", channel_id.get()));
                        let (g_name, g_icon) = guild_id
                            .and_then(|gid| guilds.iter().find(|g| g.id == gid))
                            .map(|g| (Some(g.name.clone()), g.icon_url.clone()))
                            .unwrap_or((None, None));
                        (ch_name, g_name, g_icon)
                    },
                );

                // Filter out any users blocked in RelationshipsState as well.
                let blocked_ids: Vec<Id<UserMarker>> = this
                    .inbox
                    .mentions
                    .iter()
                    .map(|m| m.message.author_id)
                    .filter(|&uid| this.is_inbox_user_blocked(uid))
                    .collect();
                if !blocked_ids.is_empty() {
                    this.inbox
                        .mentions
                        .retain(|m| !blocked_ids.contains(&m.message.author_id));
                }

                // Trigger channel metadata fetches for any guild mention whose
                // channel name hasn't been resolved yet.
                this.resolve_missing_mention_channels(cx);
                cx.notify();
            });
        }));
    }

    /// Deletes a mention from the server (`DELETE /users/@me/mentions/{id}`)
    /// and acknowledges the channel up to that message if unread.
    pub(in crate::screens::home) fn dismiss_mention(
        &mut self,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .inbox
            .mentions
            .iter_mut()
            .find(|m| m.message.id == message_id)
        else {
            return;
        };
        if entry.busy {
            return;
        }
        entry.busy = true;
        self.inbox.action_error = None;
        let should_ack = self.unreads.is_unread(channel_id);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let delete_res = discord::inbox::delete_mention(message_id).await;
            if delete_res.is_ok() && should_ack {
                let _ = discord::ack_message(channel_id, message_id, false).await;
            }
            let _ = this.update(cx, |this, cx| {
                match delete_res {
                    Ok(()) => {
                        this.inbox.mentions.retain(|m| m.message.id != message_id);
                        if should_ack {
                            this.unreads.ack(channel_id, message_id);
                            this.pending_acks.remove(&channel_id);
                            this.rebuild_inbox_unreads(cx);
                        }
                    }
                    Err(_) if discord::load_token().is_none() => {
                        cx.emit(SessionEnded);
                        return;
                    }
                    Err(err) => {
                        if let Some(entry) = this
                            .inbox
                            .mentions
                            .iter_mut()
                            .find(|m| m.message.id == message_id)
                        {
                            entry.busy = false;
                        }
                        this.inbox.action_error = Some(format!("Could not remove mention: {err}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Marks a single unread channel read up to `message_id` via
    /// `POST /channels/{id}/messages/{id}/ack`.
    pub(in crate::screens::home) fn ack_unread_channel(
        &mut self,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self
            .inbox
            .unreads
            .iter_mut()
            .find(|u| u.channel_id == channel_id)
        {
            if entry.acking {
                return;
            }
            entry.acking = true;
        }
        self.inbox.action_error = None;
        let previous_acked = self.unreads.acked(channel_id);
        self.unreads.ack(channel_id, message_id);
        self.pending_acks.remove(&channel_id);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = discord::ack_message(channel_id, message_id, false).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.materialize_inbox_unreads(cx);
                    }
                    Err(_) if discord::load_token().is_none() => {
                        cx.emit(SessionEnded);
                        return;
                    }
                    Err(err) => {
                        if let Some(prev) = previous_acked {
                            this.unreads.set_acked(channel_id, prev);
                        }
                        this.materialize_inbox_unreads(cx);
                        this.inbox.action_error =
                            Some(format!("Could not mark channel read: {err}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Marks all currently listed unread channels as read via
    /// `POST /read-states/ack-bulk`.
    pub(in crate::screens::home) fn ack_all_unreads(&mut self, cx: &mut Context<Self>) {
        if self.inbox.bulk_acking || self.inbox.unreads.is_empty() {
            return;
        }
        let entries: Vec<BulkAckEntry> = self
            .inbox
            .unreads
            .iter()
            .map(|u| BulkAckEntry::channel(u.channel_id, u.last_message_id))
            .collect();

        self.inbox.bulk_acking = true;
        self.inbox.action_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = discord::inbox::ack_bulk(entries.clone()).await;
            let _ = this.update(cx, |this, cx| {
                this.inbox.bulk_acking = false;
                match result {
                    Ok(()) => {
                        for item in entries {
                            this.unreads.ack(item.channel_id, item.message_id);
                            this.pending_acks.remove(&item.channel_id);
                        }
                        this.inbox.unread_mentions_badge = 0;
                        this.materialize_inbox_unreads(cx);
                    }
                    Err(_) if discord::load_token().is_none() => {
                        cx.emit(SessionEnded);
                        return;
                    }
                    Err(err) => {
                        this.inbox.action_error =
                            Some(format!("Could not mark all channels read: {err}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Navigates to the conversation and specific message from an Inbox item.
    /// Delegates channel/thread resolution directly to [`Self::jump_to_message`].
    pub(in crate::screens::home) fn jump_to_inbox_target(
        &mut self,
        guild_id: Option<Id<GuildMarker>>,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(gid) = guild_id
            && !self.loading
            && !self.guilds.iter().any(|g| g.id == gid)
        {
            self.inbox.action_error =
                Some("That server is no longer available on this account.".into());
            cx.notify();
            return;
        }

        self.inbox.open = false;
        self.inbox.unreads_coalesce_task = None;
        self.inbox.action_error = None;
        self.jump_to_message(
            MessageTarget {
                guild_id,
                channel_id,
                message_id,
            },
            window,
            cx,
        );
    }

    /// Handles a user clicking a native desktop toast notification: activates
    /// the window and jumps directly to the target message.
    pub(in crate::screens::home) fn handle_notification_click(
        &mut self,
        target: NotificationTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.activate_window();
        self.inbox.window_active = true;
        self.jump_to_inbox_target(
            target.guild_id,
            target.channel_id,
            target.message_id,
            window,
            cx,
        );
    }

    /// Marks the Inbox unread list dirty in O(1). When the Inbox popover is
    /// closed or showing the Mentions tab, returns immediately without
    /// iterating, cloning, or sorting `tracked_channels`. When the Unreads tab
    /// is open, coalesces burst events (such as `READY`) into a single one-shot
    /// foreground rebuild.
    pub(in crate::screens::home) fn rebuild_inbox_unreads(&mut self, cx: &mut Context<Self>) {
        if !self.inbox.mark_unreads_dirty() {
            return;
        }
        if self.inbox.unreads_coalesce_task.is_some() {
            return;
        }
        self.inbox.unreads_coalesce_task = Some(cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                this.inbox.unreads_coalesce_task = None;
                if this.inbox.unreads_dirty
                    && this.inbox.open
                    && this.inbox.active_tab == InboxTab::Unreads
                {
                    this.materialize_inbox_unreads(cx);
                }
            });
        }));
    }

    /// Materializes `self.inbox.unreads` once when the Unreads tab is opened
    /// or after a coalesced batch of updates while viewing the Unreads tab.
    pub(in crate::screens::home) fn materialize_inbox_unreads(&mut self, cx: &mut Context<Self>) {
        self.inbox.unreads_dirty = false;
        self.inbox.unreads_coalesce_task = None;

        // Sync currently loaded sidebar channels and DMs into tracked_channels
        // so names are always fresh when the Unreads tab is viewed.
        if let Some(selected_guild) = self.selected_guild {
            for group in &self.channel_groups {
                for ch in &group.channels {
                    if ch.kind.is_voice() || ch.kind == discord::ChannelKind::Category {
                        continue;
                    }
                    let meta =
                        self.inbox
                            .tracked_channels
                            .entry(ch.id)
                            .or_insert(TrackedChannelMeta {
                                channel_id: ch.id,
                                guild_id: Some(selected_guild),
                                parent_id: ch.parent_id,
                                name: Some(ch.name.clone()),
                                is_dm: false,
                                dm_avatar_url: None,
                                recipient_id: None,
                                unavailable_reason: None,
                            });
                    meta.guild_id = Some(selected_guild);
                    meta.parent_id = ch.parent_id;
                    meta.name = Some(ch.name.clone());
                }
            }
        }
        for dm in &self.dms {
            let meta = self
                .inbox
                .tracked_channels
                .entry(dm.id)
                .or_insert(TrackedChannelMeta {
                    channel_id: dm.id,
                    guild_id: None,
                    parent_id: None,
                    name: Some(dm.name.clone()),
                    is_dm: true,
                    dm_avatar_url: dm.avatar_url.clone(),
                    recipient_id: dm.recipient_id,
                    unavailable_reason: None,
                });
            meta.name = Some(dm.name.clone());
            meta.is_dm = true;
            meta.dm_avatar_url = dm.avatar_url.clone();
            meta.recipient_id = dm.recipient_id;
        }

        let tracked_snapshot: Vec<TrackedChannelMeta> =
            self.inbox.tracked_channels.values().cloned().collect();

        let mut unread_entries = Vec::new();
        for meta in tracked_snapshot {
            if !self.unreads.is_unread(meta.channel_id) {
                continue;
            }
            if let Some(guild_id) = meta.guild_id
                && self.inbox.muted_guilds.contains(&guild_id)
            {
                continue;
            }
            if self.unreads.is_muted(meta.channel_id, meta.parent_id) {
                continue;
            }
            if let Some(recipient_id) = meta.recipient_id
                && self.is_inbox_user_blocked(recipient_id)
            {
                continue;
            }
            let Some(last_message_id) = self.unreads.latest(meta.channel_id) else {
                continue;
            };
            let acked_message_id = self.unreads.acked(meta.channel_id);

            let (channel_name, guild_name, guild_icon_url) =
                self.resolve_inbox_location(meta.channel_id, meta.guild_id);

            let preview = if let Some(reason) = meta.unavailable_reason {
                UnreadPreviewState::Unavailable(reason)
            } else if let Some(messages) = self.inbox.cached_previews.get(&meta.channel_id) {
                let filtered: Vec<discord::Message> = messages
                    .iter()
                    .filter(|msg| {
                        !self.is_inbox_user_blocked(msg.author_id)
                            && acked_message_id.is_none_or(|acked| msg.id > acked)
                    })
                    .cloned()
                    .collect();
                if filtered.is_empty() && !messages.is_empty() {
                    let fallback: Vec<discord::Message> = messages
                        .iter()
                        .filter(|msg| !self.is_inbox_user_blocked(msg.author_id))
                        .rev()
                        .take(MAX_UNREAD_PREVIEW_MESSAGES)
                        .cloned()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    UnreadPreviewState::Loaded(fallback)
                } else {
                    UnreadPreviewState::Loaded(filtered)
                }
            } else if self.inbox.loading_previews.contains(&meta.channel_id) {
                UnreadPreviewState::Loading
            } else {
                UnreadPreviewState::NotLoaded
            };

            unread_entries.push(UnreadChannelEntry {
                channel_id: meta.channel_id,
                guild_id: meta.guild_id,
                parent_id: meta.parent_id,
                channel_name,
                guild_name,
                guild_icon_url,
                is_dm: meta.is_dm,
                dm_avatar_url: meta.dm_avatar_url,
                recipient_id: meta.recipient_id,
                last_message_id,
                acked_message_id,
                preview,
                acking: false,
            });
        }

        unread_entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_message_id));
        if unread_entries.len() > MAX_INBOX_UNREAD_CHANNELS {
            unread_entries.truncate(MAX_INBOX_UNREAD_CHANNELS);
        }
        self.inbox.unreads = unread_entries;

        if self.inbox.open && self.inbox.active_tab == InboxTab::Unreads {
            self.ensure_unread_metadata_and_previews(cx);
        }
        cx.notify();
    }

    /// Lazily resolves missing guild channel names, DM metadata, and unread
    /// message previews for the top visible unread items when the Unreads tab
    /// is open.
    pub(in crate::screens::home) fn ensure_unread_metadata_and_previews(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let generation = self.inbox.unreads_generation;

        // If any unread entry is a DM and DMs haven't been loaded yet, load them.
        if self.inbox.unreads.iter().any(|u| u.is_dm) && !self.dms_loaded && !self.dms_loading {
            self.load_dms(cx);
        }

        // Resolve channel names for guilds with unread channels whose channel
        // list hasn't been fetched yet.
        let guilds_needing_names: Vec<Id<GuildMarker>> = self
            .inbox
            .unreads
            .iter()
            .filter_map(|u| {
                let gid = u.guild_id?;
                let has_name = self
                    .inbox
                    .tracked_channels
                    .get(&u.channel_id)
                    .and_then(|m| m.name.as_ref())
                    .is_some();
                (!has_name && !self.inbox.loading_guild_channels.contains(&gid)).then_some(gid)
            })
            .collect();

        for guild_id in guilds_needing_names.into_iter().take(3) {
            let Some(guild) = self.guilds.iter().find(|g| g.id == guild_id) else {
                continue;
            };
            let base_permissions = guild.permissions;
            let owner = guild.owner;
            self.inbox.loading_guild_channels.insert(guild_id);

            cx.spawn(async move |this, cx| {
                let result = discord::fetch_channels(guild_id, base_permissions, owner).await;
                let _ = this.update(cx, |this, cx| {
                    if this.inbox.unreads_generation != generation {
                        return;
                    }
                    this.inbox.loading_guild_channels.remove(&guild_id);
                    if let Ok(channels) = result {
                        this.inbox_on_channels_loaded(guild_id, &channels, cx);
                    }
                });
            })
            .detach();
        }

        // Lazily fetch message previews for unread channels at the top of the
        // list, bounded by MAX_CONCURRENT_PREVIEW_FETCHES.
        if self.inbox.loading_previews.len() >= MAX_CONCURRENT_PREVIEW_FETCHES {
            return;
        }
        let available_slots = MAX_CONCURRENT_PREVIEW_FETCHES - self.inbox.loading_previews.len();
        let channels_to_fetch: Vec<Id<ChannelMarker>> = self
            .inbox
            .unreads
            .iter()
            .filter(|u| matches!(u.preview, UnreadPreviewState::NotLoaded))
            .map(|u| u.channel_id)
            .take(available_slots)
            .collect();

        for channel_id in channels_to_fetch {
            self.inbox.loading_previews.insert(channel_id);
            if let Some(entry) = self
                .inbox
                .unreads
                .iter_mut()
                .find(|u| u.channel_id == channel_id)
            {
                entry.preview = UnreadPreviewState::Loading;
            }

            cx.spawn(async move |this, cx| {
                let result =
                    discord::fetch_messages(channel_id, discord::MessagePage::Latest).await;
                let _ = this.update(cx, |this, cx| {
                    if this.inbox.unreads_generation != generation {
                        return;
                    }
                    this.inbox.loading_previews.remove(&channel_id);
                    match result {
                        Ok(messages) => {
                            let start = messages.len().saturating_sub(MAX_UNREAD_PREVIEW_MESSAGES);
                            this.inbox
                                .cached_previews
                                .insert(channel_id, messages[start..].to_vec());
                        }
                        Err(err) => {
                            if let Some(meta) = this.inbox.tracked_channels.get_mut(&channel_id) {
                                meta.unavailable_reason = Some(format!(
                                    "Cannot read channel (deleted or missing permissions): {err}"
                                ));
                            }
                        }
                    }
                    this.rebuild_inbox_unreads(cx);
                });
            })
            .detach();
        }
    }

    fn resolve_missing_mention_channels(&mut self, cx: &mut Context<Self>) {
        let generation = self.inbox.unreads_generation;
        let guilds_to_load: Vec<Id<GuildMarker>> = self
            .inbox
            .mentions
            .iter()
            .filter_map(|m| {
                let gid = m.guild_id?;
                let unknown = m.channel_name.starts_with("channel-");
                (unknown && !self.inbox.loading_guild_channels.contains(&gid)).then_some(gid)
            })
            .collect();

        for guild_id in guilds_to_load.into_iter().take(2) {
            let Some(guild) = self.guilds.iter().find(|g| g.id == guild_id) else {
                continue;
            };
            let base_permissions = guild.permissions;
            let owner = guild.owner;
            self.inbox.loading_guild_channels.insert(guild_id);

            cx.spawn(async move |this, cx| {
                let result = discord::fetch_channels(guild_id, base_permissions, owner).await;
                let _ = this.update(cx, |this, cx| {
                    if this.inbox.unreads_generation != generation {
                        return;
                    }
                    this.inbox.loading_guild_channels.remove(&guild_id);
                    if let Ok(channels) = result {
                        this.inbox_on_channels_loaded(guild_id, &channels, cx);
                    }
                });
            })
            .detach();
        }
    }

    /// Checks whether `user_id` is blocked, consulting both Feature 7's
    /// `self.relationships.is_blocked(user_id)` and `self.inbox.blocked_users`.
    pub(in crate::screens::home) fn is_inbox_user_blocked(&self, user_id: Id<UserMarker>) -> bool {
        self.relationships.is_blocked(user_id) || self.inbox.blocked_users.contains(&user_id)
    }

    fn signed_in_roles_for_guild(&self, guild_id: Option<Id<GuildMarker>>) -> Vec<Id<RoleMarker>> {
        let (Some(gid), Some(uid)) = (guild_id, self.self_user_id) else {
            return Vec::new();
        };
        self.member_roles
            .get(&gid)
            .and_then(|roles| roles.get(&uid))
            .cloned()
            .unwrap_or_default()
    }

    fn resolve_channel_parent(&self, channel_id: Id<ChannelMarker>) -> Option<Id<ChannelMarker>> {
        for group in &self.channel_groups {
            for ch in &group.channels {
                if ch.id == channel_id {
                    return ch.parent_id;
                }
            }
        }
        self.inbox
            .tracked_channels
            .get(&channel_id)
            .and_then(|meta| meta.parent_id)
    }

    fn resolve_inbox_location(
        &self,
        channel_id: Id<ChannelMarker>,
        guild_id: Option<Id<GuildMarker>>,
    ) -> (String, Option<String>, Option<String>) {
        let channel_name = self
            .channel_groups
            .iter()
            .flat_map(|g| &g.channels)
            .find(|c| c.id == channel_id)
            .map(|c| c.name.clone())
            .or_else(|| {
                self.dms
                    .iter()
                    .find(|d| d.id == channel_id)
                    .map(|d| d.name.clone())
            })
            .or_else(|| {
                self.inbox
                    .tracked_channels
                    .get(&channel_id)
                    .and_then(|t| t.name.clone())
            })
            .unwrap_or_else(|| format!("channel-{}", channel_id.get()));

        let (guild_name, guild_icon_url) = guild_id
            .and_then(|gid| self.guilds.iter().find(|g| g.id == gid))
            .map(|g| (Some(g.name.clone()), g.icon_url.clone()))
            .unwrap_or((None, None));

        (channel_name, guild_name, guild_icon_url)
    }
}
