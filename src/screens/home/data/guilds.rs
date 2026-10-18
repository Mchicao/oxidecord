//! Loading the signed-in user, their guilds, and a guild's channels, and
//! logging them out.

use gpui::*;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker},
};

use crate::discord::{self, Channel};
use crate::platform::prefs;
use crate::screens::home::channels::build_channel_groups;
use crate::screens::home::folders::build_rail_entries;
use crate::screens::home::{HomeScreen, SessionEnded, View};

impl HomeScreen {
    pub(in crate::screens::home) fn refresh_relationship_filter(&mut self, cx: &mut Context<Self>) {
        let query = self.relationships.search_input.read(cx).value().to_string();
        self.relationships
            .store
            .recompute_filtered(&query, &self.user_presences);
        cx.notify();
    }
    /// Logs out and hands back to login. A call is left properly first, so
    /// Discord doesn't keep showing the user in the channel until the dropped
    /// connection times out.
    pub(in crate::screens::home) fn log_out(&mut self, cx: &mut Context<Self>) {
        if self.voice.is_some() {
            self.leave_voice(cx);
        }
        self.relationships.reset();
        self.inbox_on_logout(cx);
        discord::log_out();
        cx.emit(SessionEnded);
    }

    pub(in crate::screens::home) fn load_guilds(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = discord::fetch_guilds().await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(guilds) => {
                        let first = guilds.first().map(|guild| guild.id);
                        this.guilds = guilds;
                        this.inbox_on_guilds_loaded(cx);
                        this.error = None;
                        this.rebuild_rail();
                        if let Some(guild_id) = first {
                            this.select_guild(guild_id, window, cx);
                        }
                    }
                    // A rejected token is forgotten by the request itself.
                    Err(_) if discord::load_token().is_none() => {
                        cx.emit(SessionEnded);
                        return;
                    }
                    Err(err) => this.error = Some(err),
                }
                this.loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Loads the user's settings: the folders that decide the rail's order,
    /// and their chosen status. Best-effort: until it lands the rail uses the
    /// plain guild list.
    pub(in crate::screens::home) fn load_user_settings(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = discord::fetch_user_settings().await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(settings) => {
                        this.guild_folders = Some(settings.guild_folders);
                        // A status picked while this was in flight wins.
                        if this.presence_status.is_none() {
                            this.apply_presence_status(settings.status.unwrap_or_default());
                        }
                    }
                    Err(err) => {
                        eprintln!("failed to load user settings: {err}");
                        return;
                    }
                }
                this.rebuild_rail();
                cx.notify();
            });
        })
        .detach();
    }

    fn rebuild_rail(&mut self) {
        self.rail_entries = build_rail_entries(&self.guilds, self.guild_folders.as_ref());
    }

    /// Loads the signed-in user for the sidebar account panel. Best-effort:
    /// on failure the panel just stays empty.
    pub(in crate::screens::home) fn load_current_user(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let Ok(user) = discord::fetch_current_user().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.self_user_id.is_some_and(|id| id != user.id) {
                    return;
                }
                // Whichever of this and the gateway's READY lands first fills
                // in the id a voice connection identifies itself with.
                this.self_user_id.get_or_insert(user.id);
                this.hydrate_peer_audio(user.id.get());
                this.current_user = Some(user);
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::screens::home) fn select_guild(
        &mut self,
        guild_id: Id<GuildMarker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_message_target = None;
        self.open_guild_channel(guild_id, None, true, window, cx);
    }

    /// Opens `guild_id` and selects `target_channel` (or the guild's first
    /// text channel when `None`). When `record_history` is `false`, the
    /// resulting channel selection restores a history entry without pushing a
    /// new visit.
    pub(in crate::screens::home) fn open_guild_channel(
        &mut self,
        guild_id: Id<GuildMarker>,
        target_channel: Option<Id<ChannelMarker>>,
        record_history: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same_guild = self.selected_guild == Some(guild_id);
        if self.view == View::Guild && same_guild {
            if self.channels_loading {
                self.pending_channel_selection = target_channel.map(|id| (id, record_history));
                return;
            }
            if let Some(channel_id) = target_channel {
                if self.is_selectable_guild_channel(channel_id) {
                    self.apply_channel_selection(channel_id, record_history, window, cx);
                } else {
                    self.fetch_thread_destination(guild_id, channel_id, record_history, window, cx);
                }
            }
            return;
        }
        self.save_collapsed_categories();
        self.view = View::Guild;
        // Returning from the DM view to the guild that's already loaded: switch
        // back to its channels without refetching them, reopening a channel
        // since the DM view cleared the previous selection.
        if same_guild && !self.channel_groups.is_empty() {
            if let Some(channel_id) =
                target_channel.filter(|&id| !self.is_selectable_guild_channel(id))
            {
                self.fetch_thread_destination(guild_id, channel_id, record_history, window, cx);
                return;
            }
            let chosen = target_channel
                .filter(|&id| self.is_selectable_guild_channel(id))
                .or_else(|| self.first_text_channel());
            if let Some(channel_id) = chosen {
                self.apply_channel_selection(channel_id, record_history, window, cx);
            }
            cx.notify();
            return;
        }
        self.selected_guild = Some(guild_id);
        self.channel_groups.clear();
        self.selected_channel = None;
        self.reset_conversation(None, window, cx);
        self.message_tools_selection_changed(cx);
        self.sync_thread_parent(window, cx);
        self.load_collapsed_categories();
        self.channels_error = None;
        self.channels_loading = true;
        self.channel_load_generation = self.channel_load_generation.wrapping_add(1);
        let generation = self.channel_load_generation;
        self.pending_channel_selection = target_channel.map(|id| (id, record_history));
        cx.notify();

        // The guild list already carries the user's guild-wide permissions, so
        // channel visibility only needs the member object on top of them.
        let (base_permissions, owner) = self
            .guilds
            .iter()
            .find(|guild| guild.id == guild_id)
            .map(|guild| (guild.permissions, guild.owner))
            .unwrap_or((discord::Permissions::empty(), false));

        cx.spawn_in(window, async move |this, cx| {
            let result = discord::fetch_channels(guild_id, base_permissions, owner).await;
            let _ = this.update_in(cx, |this, window, cx| {
                // The user may have clicked another guild while this request
                // was in flight; drop the stale response.
                if this.view != View::Guild
                    || this.selected_guild != Some(guild_id)
                    || this.channel_load_generation != generation
                {
                    return;
                }
                match result {
                    Ok(channels) => {
                        this.inbox_on_channels_loaded(guild_id, &channels, cx);
                        for channel in &channels {
                            if let Some(latest) = channel.last_message_id {
                                this.unreads.note_latest(channel.id, latest);
                            }
                        }
                        this.channel_groups = build_channel_groups(channels);
                        let (target_channel, record_history) = this
                            .pending_channel_selection
                            .take()
                            .map_or((target_channel, record_history), |(id, record)| {
                                (Some(id), record)
                            });
                        if let Some(channel_id) =
                            target_channel.filter(|&id| !this.is_selectable_guild_channel(id))
                        {
                            this.fetch_thread_destination(
                                guild_id,
                                channel_id,
                                record_history,
                                window,
                                cx,
                            );
                            this.channels_loading = false;
                            return;
                        }
                        let chosen = target_channel
                            .filter(|&id| this.is_selectable_guild_channel(id))
                            .or_else(|| this.first_text_channel());
                        if let Some(channel_id) = chosen {
                            this.apply_channel_selection(channel_id, record_history, window, cx);
                        }
                    }
                    Err(err) => this.channels_error = Some(err),
                }
                this.channels_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn load_collapsed_categories(&mut self) {
        let Some(guild_id) = self.selected_guild else {
            return;
        };
        self.collapsed_categories = prefs::load()
            .collapsed_categories
            .remove(&guild_id.get())
            .unwrap_or_default()
            .into_iter()
            .filter_map(Id::new_checked)
            .collect();
    }

    /// Writes the selected guild's collapsed categories to the prefs file.
    /// Called on channel and guild switches rather than on every toggle, so
    /// clicking through categories doesn't rewrite the file each time.
    pub(in crate::screens::home) fn save_collapsed_categories(&self) {
        let Some(guild_id) = self.selected_guild else {
            return;
        };
        let mut collapsed: Vec<u64> = self
            .collapsed_categories
            .iter()
            .map(|id| id.get())
            .collect();
        collapsed.sort_unstable();

        let mut prefs = prefs::load();
        let stored = prefs.collapsed_categories.get(&guild_id.get());
        if stored.map_or(collapsed.is_empty(), |stored| *stored == collapsed) {
            return;
        }
        if collapsed.is_empty() {
            prefs.collapsed_categories.remove(&guild_id.get());
        } else {
            prefs.collapsed_categories.insert(guild_id.get(), collapsed);
        }
        prefs::save(&prefs);
    }

    pub(in crate::screens::home) fn selected_channel_info(&self) -> Option<&Channel> {
        self.channel_info(self.selected_channel?)
    }

    pub(in crate::screens::home) fn can_send_to_selected(&self) -> bool {
        self.view == View::DirectMessages
            || self.selected_channel_info().is_some_and(|channel| {
                channel.can_send
                    && (!channel.kind.is_thread()
                        || self.threads.can_send_in(channel.id).unwrap_or(true))
            })
    }

    pub(in crate::screens::home) fn channel_info(&self, id: Id<ChannelMarker>) -> Option<&Channel> {
        self.channel_groups
            .iter()
            .flat_map(|group| &group.channels)
            .find(|channel| channel.id == id)
            .or_else(|| {
                self.thread_channels
                    .get(&id)
                    .filter(|(guild, _)| Some(*guild) == self.selected_guild)
                    .map(|(_, channel)| channel)
            })
    }

    pub(in crate::screens::home) fn remember_thread(
        &mut self,
        thread: &discord::ThreadSummary,
    ) -> bool {
        let Some(parent) = self.channel_info(thread.parent_id).cloned() else {
            return false;
        };
        let Some(guild_id) = self
            .selected_guild
            .filter(|id| thread.guild_id.is_none_or(|guild| guild == *id))
        else {
            return false;
        };
        let channel = Channel {
            id: thread.id,
            name: thread.name.clone(),
            kind: thread.kind,
            parent_id: Some(thread.parent_id),
            position: parent.position,
            topic: None,
            can_send: !thread.archived
                && parent
                    .permissions
                    .contains(discord::Permissions::SEND_MESSAGES_IN_THREADS)
                && (!thread.locked
                    || parent
                        .permissions
                        .contains(discord::Permissions::MANAGE_THREADS)),
            can_manage_messages: parent.can_manage_messages,
            can_use_external_emoji: parent.can_use_external_emoji,
            permissions: parent.permissions,
            forum_tags: Vec::new(),
            requires_tag: false,
            last_message_id: thread.last_message_id,
        };
        if self.thread_channels.len() >= 64
            && !self.thread_channels.contains_key(&thread.id)
            && let Some(old) = self
                .thread_channels
                .keys()
                .copied()
                .find(|id| Some(*id) != self.selected_channel)
        {
            self.thread_channels.remove(&old);
        }
        self.thread_channels.insert(thread.id, (guild_id, channel));
        true
    }

    pub(in crate::screens::home) fn sync_thread_parent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let channel = self.selected_channel_info().cloned();
        let parent = channel.as_ref().and_then(|channel| {
            if channel.kind.is_thread() {
                self.channel_info(channel.parent_id?).cloned()
            } else {
                Some(channel.clone())
            }
        });
        if self.view == View::Guild
            && let (Some(guild_id), Some(parent)) = (self.selected_guild, parent)
            && !parent.kind.is_voice()
            && (self.threads_open
                || parent.kind == discord::ChannelKind::Forum
                || channel.is_some_and(|channel| channel.kind.is_thread()))
        {
            self.threads.set_parent_kind(
                parent.kind == discord::ChannelKind::Forum,
                parent.forum_tags,
                parent.requires_tag,
                cx,
            );
            self.threads.set_permissions(parent.permissions, cx);
            self.threads.open_parent(guild_id, parent.id, window, cx);
        } else {
            self.threads.close_parent(window, cx);
        }
    }

    pub(in crate::screens::home) fn toggle_threads_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.threads_open = !self.threads_open;
        self.sync_thread_parent(window, cx);
        cx.notify();
    }

    fn fetch_thread_destination(
        &mut self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        record: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.message_load_generation = self.message_load_generation.wrapping_add(1);
        let generation = self.message_load_generation;
        cx.spawn_in(window, async move |this, cx| {
            let result = discord::fetch_thread(channel_id).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.view != View::Guild
                    || this.selected_guild != Some(guild_id)
                    || this.message_load_generation != generation
                {
                    return;
                }
                match result {
                    Ok(thread) if this.remember_thread(&thread) => {
                        this.apply_channel_selection(channel_id, record, window, cx);
                    }
                    _ => {
                        this.pending_message_target = None;
                        this.messages_error = Some(
                            "That channel is unavailable, or you cannot read its history.".into(),
                        );
                        this.channels_error = this.messages_error.clone();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(in crate::screens::home) fn is_selectable_guild_channel(
        &self,
        id: Id<ChannelMarker>,
    ) -> bool {
        self.channel_info(id)
            .is_some_and(|channel| !channel.kind.is_voice())
    }

    /// The first text channel in display order, used as the default selection
    /// when entering a guild.
    fn first_text_channel(&self) -> Option<Id<ChannelMarker>> {
        self.channel_groups
            .iter()
            .flat_map(|group| &group.channels)
            .find(|channel| !channel.kind.is_voice())
            .map(|channel| channel.id)
    }
}
