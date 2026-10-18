//! Mentions & Unreads Inbox state ([`InboxState`]) and coordination with
//! [`crate::platform::notifications::DesktopNotifier`].
//!
//! Structure:
//! - [`mod@data`]: `HomeScreen` methods for REST pagination, read-state acks,
//!   live gateway event hooks, jump-to-message navigation, and desktop toasts.
//! - [`mod@view`]: `HomeScreen` rendering methods for the header button, badge,
//!   and virtualized popover panel.

pub mod data;
pub mod view;

use std::collections::{HashMap, HashSet};

use gpui::{Context, Subscription, Task, UniformListScrollHandle, Window};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
};

use crate::discord;
use crate::discord::inbox::{InboxMention, MENTIONS_PAGE_SIZE, MentionsQuery};
use crate::platform::notifications::{DesktopNotifier, NotificationPreferences};
use crate::platform::prefs;
use crate::screens::home::HomeScreen;

/// Maximum number of mention items retained in memory.
pub const MAX_INBOX_MENTIONS: usize = 200;

/// Maximum number of unread channel items retained in the inbox list.
pub const MAX_INBOX_UNREAD_CHANNELS: usize = 100;

/// Maximum number of preview messages retained per unread channel card.
pub const MAX_UNREAD_PREVIEW_MESSAGES: usize = 3;

/// Maximum number of concurrent unread channel preview fetches.
pub const MAX_CONCURRENT_PREVIEW_FETCHES: usize = 4;

/// Which tab is active inside the Inbox popover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InboxTab {
    #[default]
    Mentions,
    Unreads,
}

/// Filter toggles for the Mentions tab (`GET /users/@me/mentions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MentionsFilter {
    /// Include `@everyone` and `@here` mentions (`everyone` query param).
    pub include_everyone: bool,
    /// Include `@role` mentions (`roles` query param).
    pub include_roles: bool,
    /// Restrict mentions to the currently selected guild (`guild_id` query param).
    pub current_guild_only: bool,
}

impl Default for MentionsFilter {
    fn default() -> Self {
        Self {
            include_everyone: true,
            include_roles: true,
            current_guild_only: false,
        }
    }
}

impl MentionsFilter {
    /// Builds a [`MentionsQuery`] for this filter, optionally anchored before
    /// `before` and scoped to `selected_guild`.
    pub fn to_query(
        self,
        before: Option<Id<MessageMarker>>,
        selected_guild: Option<Id<GuildMarker>>,
    ) -> MentionsQuery {
        MentionsQuery {
            before,
            limit: MENTIONS_PAGE_SIZE,
            guild_id: if self.current_guild_only {
                selected_guild
            } else {
                None
            },
            roles: self.include_roles,
            everyone: self.include_everyone,
        }
    }
}

/// One mention item rendered in the Mentions tab.
#[derive(Clone)]
pub struct MentionEntry {
    pub channel_id: Id<ChannelMarker>,
    pub guild_id: Option<Id<GuildMarker>>,
    pub channel_name: String,
    pub guild_name: Option<String>,
    pub guild_icon_url: Option<String>,
    pub message: discord::Message,
    /// True while a `DELETE /users/@me/mentions/{id}` request is in flight.
    pub busy: bool,
}

/// State of the message preview inside an [`UnreadChannelEntry`].
#[derive(Clone)]
pub enum UnreadPreviewState {
    /// Preview has not been requested yet; fetched lazily when the Unreads tab
    /// is viewed.
    NotLoaded,
    /// A preview fetch is currently in flight.
    Loading,
    /// The newest unread messages in this channel (up to
    /// [`MAX_UNREAD_PREVIEW_MESSAGES`]).
    Loaded(Vec<discord::Message>),
    /// Channel history could not be loaded (e.g. deleted channel, revoked
    /// permissions, or network failure).
    Unavailable(String),
}

/// One unread channel or DM conversation rendered in the Unreads tab.
#[derive(Clone)]
pub struct UnreadChannelEntry {
    pub channel_id: Id<ChannelMarker>,
    pub guild_id: Option<Id<GuildMarker>>,
    pub parent_id: Option<Id<ChannelMarker>>,
    pub channel_name: String,
    pub guild_name: Option<String>,
    pub guild_icon_url: Option<String>,
    pub is_dm: bool,
    pub dm_avatar_url: Option<String>,
    pub recipient_id: Option<Id<UserMarker>>,
    pub last_message_id: Id<MessageMarker>,
    pub acked_message_id: Option<Id<MessageMarker>>,
    pub preview: UnreadPreviewState,
    /// True while an ack request for this channel is in flight.
    pub acking: bool,
}

/// Known metadata for a channel tracked by the event-driven unread index.
#[derive(Clone, Debug)]
pub struct TrackedChannelMeta {
    pub channel_id: Id<ChannelMarker>,
    pub guild_id: Option<Id<GuildMarker>>,
    pub parent_id: Option<Id<ChannelMarker>>,
    pub name: Option<String>,
    pub is_dm: bool,
    pub dm_avatar_url: Option<String>,
    pub recipient_id: Option<Id<UserMarker>>,
    /// Set when the server reports the channel was deleted or permissions were lost.
    pub unavailable_reason: Option<String>,
}

/// State owned by [`HomeScreen`] (`pub(super) inbox: InboxState`).
pub struct InboxState {
    /// Whether the Inbox popover is currently open.
    pub open: bool,
    /// Active tab inside the popover (`Mentions` or `Unreads`).
    pub active_tab: InboxTab,
    /// Filter settings for the Mentions tab.
    pub filter: MentionsFilter,

    /// Loaded mentions ordered newest-first, bounded to [`MAX_INBOX_MENTIONS`].
    pub mentions: Vec<MentionEntry>,
    /// Whether the initial page of mentions has been fetched for the current
    /// filter.
    pub mentions_loaded: bool,
    /// Whether the first page of mentions is currently loading.
    pub mentions_loading: bool,
    /// Whether an older page of mentions (`before` cursor) is currently loading.
    pub mentions_loading_more: bool,
    /// Whether the server returned fewer than `MENTIONS_PAGE_SIZE` items,
    /// indicating the end of the mentions history.
    pub mentions_complete: bool,
    /// Error from the last `GET /users/@me/mentions` call. Rendered visibly;
    /// never masked as an empty list.
    pub mentions_error: Option<String>,
    /// Monotonic generation counter to discard stale mention fetches when
    /// filters change, reload is triggered, or the user logs out.
    pub mentions_generation: u64,
    pub mentions_task: Option<Task<()>>,

    /// Materialized unread channels ordered newest-message-first, bounded to
    /// [`MAX_INBOX_UNREAD_CHANNELS`]. Built lazily only when the Unreads tab
    /// is open.
    pub unreads: Vec<UnreadChannelEntry>,
    /// True when unread states, channel metadata, mutes, or relationships have
    /// changed while the Unreads tab is not open. Materialized once when the
    /// user opens the Unreads tab.
    pub unreads_dirty: bool,
    /// One-shot task that coalesces burst gateway events (such as `READY`
    /// `GuildUnreads`) when the Unreads tab is open.
    pub unreads_coalesce_task: Option<Task<()>>,
    /// Error from a bulk-ack, single-ack, mention-delete, or jump operation.
    pub action_error: Option<String>,
    /// Whether a "Mark All Read" bulk-ack operation is in flight.
    pub bulk_acking: bool,
    /// Monotonic generation counter for unread preview / channel metadata fetches.
    pub unreads_generation: u64,
    /// Guilds whose channel metadata is currently being fetched for unread rows.
    pub loading_guild_channels: HashSet<Id<GuildMarker>>,
    /// Channels whose preview messages are currently being fetched.
    pub loading_previews: HashSet<Id<ChannelMarker>>,

    /// Known channel metadata indexed by `channel_id`, populated from
    /// `GuildUnreads`, `fetch_channels`, `fetch_dms`, and live messages.
    pub tracked_channels: HashMap<Id<ChannelMarker>, TrackedChannelMeta>,
    /// Cached latest message previews per channel from lazy loads or live
    /// updates while viewing Unreads.
    pub cached_previews: HashMap<Id<ChannelMarker>, Vec<discord::Message>>,
    /// Muted guilds from `MuteSettings` (mirrored here because `Unreads.muted_guilds`
    /// is private to `unread.rs`).
    pub muted_guilds: HashSet<Id<GuildMarker>>,
    /// Local fallback set of blocked users (used alongside `HomeScreen.relationships.is_blocked`).
    pub blocked_users: HashSet<Id<UserMarker>>,

    /// O(1) count of unread mentions since session start or inbox check, shown
    /// on the header badge.
    pub unread_mentions_badge: usize,

    /// Whether the application window currently has OS focus. Updated via
    /// `cx.observe_window_activation`.
    pub window_active: bool,
    /// Native desktop toast dispatcher and filter gate.
    pub notifier: DesktopNotifier,

    /// Scroll handles for the virtualized lists.
    pub mentions_scroll: UniformListScrollHandle,
    pub unreads_scroll: UniformListScrollHandle,

    pub _window_activation: Option<Subscription>,
    pub _toast_click_task: Option<Task<()>>,
}

impl InboxState {
    /// Creates the [`InboxState`] and wires window-activation tracking plus the
    /// foreground listener for native toast click navigation.
    pub fn new(window: &mut Window, cx: &mut Context<HomeScreen>) -> Self {
        let initial_prefs = load_notification_prefs();
        let (notifier, mut click_rx) = DesktopNotifier::new(initial_prefs);
        let window_active = window.is_window_active();

        let window_activation = cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            if this.inbox.window_active != active {
                this.inbox.window_active = active;
                if active {
                    this.ack_open_conversation(cx);
                }
                cx.notify();
            }
        });

        let toast_click_task = cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(target) = click_rx.next().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.handle_notification_click(target, window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            open: false,
            active_tab: InboxTab::Mentions,
            filter: MentionsFilter::default(),
            mentions: Vec::new(),
            mentions_loaded: false,
            mentions_loading: false,
            mentions_loading_more: false,
            mentions_complete: false,
            mentions_error: None,
            mentions_generation: 0,
            mentions_task: None,
            unreads: Vec::new(),
            unreads_dirty: true,
            unreads_coalesce_task: None,
            action_error: None,
            bulk_acking: false,
            unreads_generation: 0,
            loading_guild_channels: HashSet::new(),
            loading_previews: HashSet::new(),
            tracked_channels: HashMap::new(),
            cached_previews: HashMap::new(),
            muted_guilds: HashSet::new(),
            blocked_users: HashSet::new(),
            unread_mentions_badge: 0,
            window_active,
            notifier,
            mentions_scroll: UniformListScrollHandle::new(),
            unreads_scroll: UniformListScrollHandle::new(),
            _window_activation: Some(window_activation),
            _toast_click_task: Some(toast_click_task),
        }
    }

    /// Creates a headless [`InboxState`] for deterministic unit tests without
    /// requiring a GPUI window.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let (notifier, _click_rx) = DesktopNotifier::new(NotificationPreferences::default());
        Self {
            open: false,
            active_tab: InboxTab::Mentions,
            filter: MentionsFilter::default(),
            mentions: Vec::new(),
            mentions_loaded: false,
            mentions_loading: false,
            mentions_loading_more: false,
            mentions_complete: false,
            mentions_error: None,
            mentions_generation: 0,
            mentions_task: None,
            unreads: Vec::new(),
            unreads_dirty: true,
            unreads_coalesce_task: None,
            action_error: None,
            bulk_acking: false,
            unreads_generation: 0,
            loading_guild_channels: HashSet::new(),
            loading_previews: HashSet::new(),
            tracked_channels: HashMap::new(),
            cached_previews: HashMap::new(),
            muted_guilds: HashSet::new(),
            blocked_users: HashSet::new(),
            unread_mentions_badge: 0,
            window_active: false,
            notifier,
            mentions_scroll: UniformListScrollHandle::new(),
            unreads_scroll: UniformListScrollHandle::new(),
            _window_activation: None,
            _toast_click_task: None,
        }
    }

    /// Marks the unread channel list dirty in O(1) and returns `true` only when
    /// the Inbox popover is open on the Unreads tab (meaning a coalesced UI
    /// rebuild should be scheduled).
    pub fn mark_unreads_dirty(&mut self) -> bool {
        self.unreads_dirty = true;
        self.open && self.active_tab == InboxTab::Unreads
    }

    /// Cancels in-flight mention fetches by bumping `mentions_generation` and
    /// dropping the active task.
    pub fn cancel_mentions_fetch(&mut self) -> u64 {
        self.mentions_generation = self.mentions_generation.wrapping_add(1);
        self.mentions_task = None;
        self.mentions_loading = false;
        self.mentions_loading_more = false;
        self.mentions_generation
    }

    /// Applies a page of fetched mentions if `generation` matches the current
    /// `mentions_generation`, filtering out blocked users and deduplicating by
    /// message ID within [`MAX_INBOX_MENTIONS`].
    pub fn apply_mentions_page(
        &mut self,
        generation: u64,
        append: bool,
        page: Result<Vec<InboxMention>, String>,
        self_user_id: Option<Id<UserMarker>>,
        user_context: impl Fn(Option<Id<GuildMarker>>, Id<UserMarker>) -> (bool, Vec<Id<RoleMarker>>),
        resolve_names: impl Fn(
            Id<ChannelMarker>,
            Option<Id<GuildMarker>>,
        ) -> (String, Option<String>, Option<String>),
    ) -> bool {
        if generation != self.mentions_generation {
            return false;
        }
        self.mentions_loading = false;
        self.mentions_loading_more = false;
        self.mentions_task = None;

        match page {
            Ok(items) => {
                self.mentions_error = None;
                self.mentions_loaded = true;
                if items.len() < usize::from(MENTIONS_PAGE_SIZE) {
                    self.mentions_complete = true;
                }
                if !append {
                    self.mentions.clear();
                }
                let mut seen: HashSet<Id<MessageMarker>> =
                    self.mentions.iter().map(|entry| entry.message.id).collect();

                for item in items {
                    let (blocked, roles) = user_context(item.guild_id, item.message.author_id);
                    if blocked {
                        continue;
                    }
                    if !matches_mention_filter(
                        &item.message,
                        self_user_id,
                        &roles,
                        self.filter,
                        true,
                    ) {
                        continue;
                    }
                    if seen.insert(item.message.id) {
                        let (channel_name, guild_name, guild_icon_url) =
                            resolve_names(item.channel_id, item.guild_id);
                        self.mentions.push(MentionEntry {
                            channel_id: item.channel_id,
                            guild_id: item.guild_id,
                            channel_name,
                            guild_name,
                            guild_icon_url,
                            message: item.message,
                            busy: false,
                        });
                    }
                }

                self.mentions
                    .sort_by_key(|entry| std::cmp::Reverse(entry.message.id));
                if self.mentions.len() > MAX_INBOX_MENTIONS {
                    self.mentions.truncate(MAX_INBOX_MENTIONS);
                    self.mentions_complete = true;
                }
                true
            }
            Err(err) => {
                self.mentions_error = Some(err);
                true
            }
        }
    }

    /// Inserts a newly arrived live mention at the top of the list if it
    /// matches the current filter and isn't from self or a blocked user.
    pub fn push_live_mention(
        &mut self,
        incoming: &discord::IncomingMessage,
        (self_user_id, user_roles): (Option<Id<UserMarker>>, &[Id<RoleMarker>]),
        selected_guild: Option<Id<GuildMarker>>,
        is_blocked: bool,
        (channel_name, guild_name, guild_icon_url): (String, Option<String>, Option<String>),
        channel_is_unread: bool,
    ) -> bool {
        if is_blocked || self_user_id == Some(incoming.message.author_id) {
            return false;
        }
        if !message_mentions_user(&incoming.message, self_user_id, user_roles, true, true) {
            return false;
        }

        if channel_is_unread && !self.open {
            self.unread_mentions_badge = self.unread_mentions_badge.saturating_add(1);
        }

        if self.filter.current_guild_only
            && selected_guild.is_some()
            && incoming.guild_id != selected_guild
        {
            return true;
        }
        if !matches_mention_filter(
            &incoming.message,
            self_user_id,
            user_roles,
            self.filter,
            false,
        ) {
            return true;
        }

        if self
            .mentions
            .iter()
            .any(|entry| entry.message.id == incoming.message.id)
        {
            return false;
        }

        if self.mentions_loaded {
            self.mentions.insert(
                0,
                MentionEntry {
                    channel_id: incoming.channel_id,
                    guild_id: incoming.guild_id,
                    channel_name,
                    guild_name,
                    guild_icon_url,
                    message: incoming.message.clone(),
                    busy: false,
                },
            );
            if self.mentions.len() > MAX_INBOX_MENTIONS {
                self.mentions.truncate(MAX_INBOX_MENTIONS);
            }
        }
        true
    }

    /// Removes deleted messages from both `mentions` and cached unread previews.
    pub fn remove_deleted_messages(
        &mut self,
        channel_id: Id<ChannelMarker>,
        message_ids: &[Id<MessageMarker>],
    ) -> bool {
        let before_len = self.mentions.len();
        self.mentions.retain(|entry| {
            !(entry.channel_id == channel_id && message_ids.contains(&entry.message.id))
        });
        let mut changed = self.mentions.len() != before_len;

        if let Some(cached) = self.cached_previews.get_mut(&channel_id) {
            let prev = cached.len();
            cached.retain(|msg| !message_ids.contains(&msg.id));
            changed |= cached.len() != prev;
        }
        for unread in &mut self.unreads {
            if unread.channel_id == channel_id
                && let UnreadPreviewState::Loaded(messages) = &mut unread.preview
            {
                let prev = messages.len();
                messages.retain(|msg| !message_ids.contains(&msg.id));
                changed |= messages.len() != prev;
            }
        }
        changed
    }

    /// Resets all session state, cancels in-flight tasks, and clears active
    /// desktop notifications on logout or session invalidation.
    pub fn reset_on_logout(&mut self) {
        self.open = false;
        self.active_tab = InboxTab::Mentions;
        self.filter = MentionsFilter::default();
        self.cancel_mentions_fetch();
        self.mentions.clear();
        self.mentions_loaded = false;
        self.mentions_complete = false;
        self.mentions_error = None;
        self.unreads.clear();
        self.unreads_dirty = false;
        self.unreads_coalesce_task = None;
        self.action_error = None;
        self.bulk_acking = false;
        self.unreads_generation = self.unreads_generation.wrapping_add(1);
        self.loading_guild_channels.clear();
        self.loading_previews.clear();
        self.tracked_channels.clear();
        self.cached_previews.clear();
        self.muted_guilds.clear();
        self.blocked_users.clear();
        self.unread_mentions_badge = 0;
        self.notifier.clear_on_logout();
    }

    /// O(1) unread mentions count shown on the Inbox header badge while closed.
    pub fn badge_count(&self) -> usize {
        self.unread_mentions_badge
    }
}

/// Checks whether `message` mentions `self_user_id` directly, via `user_roles`,
/// or via `@everyone`/`@here`.
pub fn message_mentions_user(
    message: &discord::Message,
    self_user_id: Option<Id<UserMarker>>,
    user_roles: &[Id<RoleMarker>],
    include_everyone: bool,
    include_roles: bool,
) -> bool {
    if let Some(self_id) = self_user_id
        && message.mentions.iter().any(|user| user.id == self_id)
    {
        return true;
    }
    if include_everyone && message.mention_everyone {
        return true;
    }
    if include_roles
        && message
            .mention_roles
            .iter()
            .any(|role_id| user_roles.contains(role_id))
    {
        return true;
    }
    false
}

/// Checks whether a message satisfies [`MentionsFilter`]. When `from_server_endpoint`
/// is `true`, Discord's `/users/@me/mentions` endpoint has already verified that
/// the message mentions the user or one of their roles in that guild, so role
/// mentions remain accepted even if the guild's `member_roles` haven't been
/// synced over the gateway yet.
pub fn matches_mention_filter(
    message: &discord::Message,
    self_user_id: Option<Id<UserMarker>>,
    user_roles: &[Id<RoleMarker>],
    filter: MentionsFilter,
    from_server_endpoint: bool,
) -> bool {
    if let Some(self_id) = self_user_id
        && message.mentions.iter().any(|user| user.id == self_id)
    {
        return true;
    }
    if filter.include_everyone && message.mention_everyone {
        return true;
    }
    if filter.include_roles {
        if !user_roles.is_empty() {
            if message
                .mention_roles
                .iter()
                .any(|role_id| user_roles.contains(role_id))
            {
                return true;
            }
        } else if from_server_endpoint && !message.mention_roles.is_empty() {
            return true;
        }
    }
    // If the server returned a direct mention before `self_user_id` resolved,
    // keep it when it isn't solely an excluded `@everyone` or `@role` ping.
    if from_server_endpoint && self_user_id.is_none() {
        if !filter.include_everyone && message.mention_everyone && message.mentions.is_empty() {
            return false;
        }
        if !filter.include_roles && !message.mention_roles.is_empty() && message.mentions.is_empty()
        {
            return false;
        }
        return true;
    }
    false
}

/// Loads persisted notification preferences from `prefs::load().notifications`.
pub fn load_notification_prefs() -> NotificationPreferences {
    prefs::load().notifications
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::discord::{Delivery, IncomingMessage, Markdown, MentionedUser, Message};

    fn make_message(
        id: u64,
        author_id: u64,
        mentioned_users: &[u64],
        mention_roles: &[u64],
        mention_everyone: bool,
    ) -> Message {
        let content = format!("msg {id}");
        Message {
            id: Id::new(id),
            author_id: Id::new(author_id),
            author_name: format!("user-{author_id}"),
            author_avatar_url: None,
            markdown: Arc::new(Markdown::parse(&content)),
            content,
            mentions: mentioned_users
                .iter()
                .map(|&uid| MentionedUser {
                    id: Id::new(uid),
                    name: format!("user-{uid}"),
                    avatar_url: None,
                })
                .collect(),
            mention_everyone,
            mention_roles: mention_roles.iter().map(|&rid| Id::new(rid)).collect(),
            timestamp: 1_700_000_000,
            edited: None,
            images: Vec::new(),
            videos: Vec::new(),
            embeds: Vec::new(),
            reply: None,
            forward: None,
            reactions: Vec::new(),
            pinned: false,
            delivery: Delivery::Sent,
        }
    }

    #[test]
    fn mention_filters_respect_everyone_roles_and_direct_pings() {
        let self_id = Some(Id::new(42));
        let user_roles = vec![Id::new(7)];

        let direct = make_message(1, 10, &[42], &[], false);
        let everyone = make_message(2, 10, &[], &[], true);
        let role_ping = make_message(3, 10, &[], &[7], false);
        let other_role = make_message(4, 10, &[], &[99], false);

        let all_on = MentionsFilter::default();
        assert!(matches_mention_filter(
            &direct,
            self_id,
            &user_roles,
            all_on,
            true
        ));
        assert!(matches_mention_filter(
            &everyone,
            self_id,
            &user_roles,
            all_on,
            true
        ));
        assert!(matches_mention_filter(
            &role_ping,
            self_id,
            &user_roles,
            all_on,
            true
        ));
        assert!(!matches_mention_filter(
            &other_role,
            self_id,
            &user_roles,
            all_on,
            true
        ));

        let no_everyone = MentionsFilter {
            include_everyone: false,
            include_roles: true,
            current_guild_only: false,
        };
        assert!(!matches_mention_filter(
            &everyone,
            self_id,
            &user_roles,
            no_everyone,
            true
        ));
        assert!(matches_mention_filter(
            &direct,
            self_id,
            &user_roles,
            no_everyone,
            true
        ));

        let no_roles = MentionsFilter {
            include_everyone: true,
            include_roles: false,
            current_guild_only: false,
        };
        assert!(!matches_mention_filter(
            &role_ping,
            self_id,
            &user_roles,
            no_roles,
            true
        ));
    }

    #[test]
    fn apply_mentions_page_cancels_stale_fetch_dedupes_filters_blocked_and_bounds() {
        let mut inbox = InboxState::new_for_test();
        let self_id = Some(Id::new(42));
        let blocked_author = Id::new(666);

        let gen1 = inbox.cancel_mentions_fetch();
        let gen2 = inbox.cancel_mentions_fetch();
        assert_ne!(gen1, gen2);

        // Stale generation (gen1) is rejected and does not touch state.
        let stale_item = InboxMention {
            channel_id: Id::new(10),
            guild_id: Some(Id::new(1)),
            message: make_message(100, 20, &[42], &[], false),
        };
        assert!(!inbox.apply_mentions_page(
            gen1,
            false,
            Ok(vec![stale_item]),
            self_id,
            |_, _| (false, Vec::new()),
            |_, _| ("general".into(), Some("Guild".into()), None),
        ));
        assert!(!inbox.mentions_loaded);
        assert!(inbox.mentions.is_empty());

        // API error on current generation sets mentions_error without marking loaded.
        assert!(inbox.apply_mentions_page(
            gen2,
            false,
            Err("503 Service Unavailable".into()),
            self_id,
            |_, _| (false, Vec::new()),
            |_, _| ("general".into(), None, None),
        ));
        assert_eq!(
            inbox.mentions_error.as_deref(),
            Some("503 Service Unavailable")
        );
        assert!(!inbox.mentions_loaded);

        // Valid page on gen3 filters out blocked user, deduplicates IDs, and bounds to MAX_INBOX_MENTIONS.
        let gen3 = inbox.cancel_mentions_fetch();
        let mut big_page = Vec::new();
        // Add a blocked user's mention (should be skipped).
        big_page.push(InboxMention {
            channel_id: Id::new(10),
            guild_id: Some(Id::new(1)),
            message: make_message(9999, blocked_author.get(), &[42], &[], false),
        });
        // Add duplicate and > MAX_INBOX_MENTIONS valid mentions.
        for i in 1..=(MAX_INBOX_MENTIONS as u64 + 10) {
            let item = InboxMention {
                channel_id: Id::new(10),
                guild_id: Some(Id::new(1)),
                message: make_message(i, 20, &[42], &[], false),
            };
            big_page.push(item.clone());
            if i == 5 {
                big_page.push(item); // Duplicate
            }
        }

        assert!(inbox.apply_mentions_page(
            gen3,
            false,
            Ok(big_page),
            self_id,
            |_, uid| (uid == blocked_author, Vec::new()),
            |_, _| ("general".into(), Some("Guild".into()), None),
        ));
        assert!(inbox.mentions_error.is_none());
        assert!(inbox.mentions_loaded);
        assert_eq!(inbox.mentions.len(), MAX_INBOX_MENTIONS);
        assert!(
            inbox
                .mentions
                .iter()
                .all(|m| m.message.author_id != blocked_author)
        );
        // Sorted newest-first.
        assert_eq!(
            inbox.mentions[0].message.id,
            Id::new(MAX_INBOX_MENTIONS as u64 + 10)
        );
    }

    #[test]
    fn live_mentions_lazy_unreads_dirty_and_logout_reset_behave_deterministically() {
        let mut inbox = InboxState::new_for_test();
        let self_id = Some(Id::new(42));
        let generation = inbox.cancel_mentions_fetch();
        inbox.apply_mentions_page(
            generation,
            false,
            Ok(Vec::new()),
            self_id,
            |_, _| (false, Vec::new()),
            |_, _| ("general".into(), None, None),
        );

        // Marking unreads dirty while closed is O(1) and reports false (do not materialize).
        inbox.unreads_dirty = false;
        assert!(!inbox.mark_unreads_dirty());
        assert!(inbox.unreads_dirty);
        assert!(inbox.unreads.is_empty());

        // When open on Unreads tab, mark_unreads_dirty reports true so a coalesced rebuild runs.
        inbox.open = true;
        inbox.active_tab = InboxTab::Unreads;
        assert!(inbox.mark_unreads_dirty());
        inbox.open = false;
        inbox.active_tab = InboxTab::Mentions;

        let incoming = IncomingMessage {
            channel_id: Id::new(50),
            guild_id: Some(Id::new(5)),
            message: make_message(500, 99, &[42], &[], false),
            nonce: None,
        };
        assert!(inbox.push_live_mention(
            &incoming,
            (self_id, &[]),
            Some(Id::new(5)),
            false,
            ("dev".into(), Some("Rust".into()), None),
            true,
        ));
        assert_eq!(inbox.mentions.len(), 1);
        assert_eq!(inbox.unread_mentions_badge, 1);
        assert_eq!(inbox.badge_count(), 1);

        // Deleting message removes it from mentions.
        assert!(inbox.remove_deleted_messages(Id::new(50), &[Id::new(500)]));
        assert!(inbox.mentions.is_empty());

        // Logout clears everything and bumps generations.
        let prev_gen = inbox.mentions_generation;
        inbox.reset_on_logout();
        assert_eq!(inbox.unread_mentions_badge, 0);
        assert_eq!(inbox.badge_count(), 0);
        assert!(!inbox.unreads_dirty);
        assert!(!inbox.mentions_loaded);
        assert_ne!(inbox.mentions_generation, prev_gen);
    }
}
