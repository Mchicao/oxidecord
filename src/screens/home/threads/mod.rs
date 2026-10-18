//! The threads panel for the open channel: a text channel's threads or a
//! forum channel's posts, active and archived, plus the forms that create
//! and join them.
//!
//! [`HomeScreen`] owns a plain `ThreadState` field; [`data`] mutates it (the
//! fetches, the gateway reducer, the actions) and [`view`] renders it. Every
//! fetch is generation-guarded — opening another parent bumps the
//! generation, drops the in-flight tasks, and stale results are discarded —
//! and the lists it keeps are capped, since a long-lived forum can page in
//! thousands of archived posts.

pub(super) mod data;
pub(super) mod view;

use gpui::{AppContext as _, Context, Entity, Task, Window};
use gpui_component::input::InputState;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, TagMarker},
};

use crate::discord::{ArchivedScope, ForumTag, Permissions, ThreadSummary};

use super::HomeScreen;

/// The most threads kept per list. Past it, "load more" stops offering more
/// pages: the panel is a picker, not an archive browser, and an unbounded
/// Vec of summaries is the one cost that only grows with use.
const MAX_THREADS_PER_LIST: usize = 250;

/// Which list the panel shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Active,
    Archived,
}

/// What a create form is aimed at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CreateSource {
    /// A thread on the open channel itself.
    Channel,
    /// A thread spun off a message, from the message menu. The form floats
    /// over the conversation until it is sent or dismissed.
    Message(Id<ChannelMarker>, Id<MessageMarker>),
}

pub(super) struct ThreadState {
    /// The open parent: its guild and channel. `None` when no channel with
    /// threads is open, which empties the panel.
    pub(super) parent: Option<(Id<GuildMarker>, Id<ChannelMarker>)>,
    /// Whether the open parent is a thread-only channel (forum or media),
    /// which is what makes a post the create target instead of a thread.
    pub(super) forum: bool,
    /// The parent's forum tags, as the create form offers them.
    pub(super) forum_tags: Vec<ForumTag>,
    /// The parent requires a tag on every new post.
    pub(super) requires_tag: bool,
    /// The open parent's resolved permissions, refreshed by the screen. The
    /// panel never sends on its own authority: the server re-checks
    /// everything, and its errors surface in the panel.
    pub(super) permissions: Permissions,
    pub(super) tab: Tab,
    pub(super) scope: ArchivedScope,
    lists: data::ThreadLists,
    pub(super) active_loading: bool,
    pub(super) archived_loading: bool,
    /// Whether the visible list is a continuation rather than the first
    /// page, which keeps the skeleton to first loads.
    pub(super) loading_more: bool,
    pub(super) error: Option<String>,
    pub(super) has_more_active: bool,
    pub(super) has_more_archived: bool,
    /// The search endpoint's offset cursor, which deduplication makes safe
    /// against posts arriving between pages.
    pub(super) active_offset: usize,
    /// Bumped whenever the parent changes; results from older generations
    /// are dropped, and their tasks dropped to cancel them outright.
    generation: u64,
    /// The create form, open or not. `None` keeps the panel's "New Thread"
    /// button in its place.
    pub(super) creating: Option<CreateSource>,
    /// Set until the first successful load under a scope, so the archived
    /// tab knows to fetch on first show and after every scope switch.
    scope_loaded: Option<ArchivedScope>,
    /// A join/leave/archive in flight, whose row's buttons stay disabled
    /// until the server answers.
    pub(super) mutating: Option<Id<ChannelMarker>>,
    /// Whether the create form's request is in flight.
    pub(super) submitting: bool,
    /// Shown inside the create form; separate from the list's banner.
    pub(super) create_error: Option<String>,
    pub(super) name_input: Entity<InputState>,
    /// The first message of a forum post. Unused for plain threads, whose
    /// composer is the conversation's own.
    pub(super) body_input: Entity<InputState>,
    pub(super) selected_tags: Vec<Id<TagMarker>>,
    /// Whether the create form targets a private thread (text channels
    /// only; forum posts are always public).
    pub(super) private_thread: bool,
    pub(super) _active_task: Option<Task<()>>,
    pub(super) _archived_task: Option<Task<()>>,
    pub(super) _mutate_task: Option<Task<()>>,
}

impl ThreadState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<HomeScreen>) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Thread name"));
        let body_input = cx.new(|cx| InputState::new(window, cx).placeholder("Post text"));

        Self {
            parent: None,
            forum: false,
            forum_tags: Vec::new(),
            requires_tag: false,
            permissions: Permissions::empty(),
            tab: Tab::Active,
            scope: ArchivedScope::Public,
            lists: data::ThreadLists::default(),
            active_loading: false,
            archived_loading: false,
            loading_more: false,
            error: None,
            has_more_active: false,
            has_more_archived: false,
            active_offset: 0,
            generation: 0,
            creating: None,
            scope_loaded: None,
            mutating: None,
            submitting: false,
            create_error: None,
            name_input,
            body_input,
            selected_tags: Vec::new(),
            private_thread: false,
            _active_task: None,
            _archived_task: None,
            _mutate_task: None,
        }
    }

    /// Opens a channel as the panel's parent, refetching unless it is
    /// already the one open. The screen's channel selection calls this for
    /// every text-like channel, so the panel is warm before its first
    /// render.
    pub(super) fn open_parent(
        &mut self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        window: &mut Window,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.parent == Some((guild_id, channel_id)) {
            return;
        }

        // Cancels the old parent's fetches: dropping a gpui task stops it.
        self.generation += 1;
        self._active_task = None;
        self._archived_task = None;
        self.parent = Some((guild_id, channel_id));
        self.lists = data::ThreadLists::default();
        self.active_offset = 0;
        self.active_loading = false;
        self.archived_loading = false;
        self.loading_more = false;
        self.error = None;
        self.has_more_active = false;
        self.has_more_archived = false;
        self.tab = Tab::Active;
        self.scope = ArchivedScope::Public;
        self.scope_loaded = None;
        self.mutating = None;
        self.close_create(window, cx);
        self.load_active(cx);
    }

    /// Clears the panel when the screen leaves the open channel. The parent
    /// is kept so an open thread's summary still resolves for the
    /// conversation header until another channel opens.
    pub(super) fn close_parent(&mut self, window: &mut Window, cx: &mut Context<HomeScreen>) {
        self.generation += 1;
        self._active_task = None;
        self._archived_task = None;
        self.active_loading = false;
        self.archived_loading = false;
        self.mutating = None;
        self.close_create(window, cx);
        cx.notify();
    }

    /// The open parent's shape, from the screen's channel record: whether it
    /// takes forum posts, the tags it offers, and whether it demands one.
    /// The screen calls this beside `open_parent`.
    pub(super) fn set_parent_kind(
        &mut self,
        forum: bool,
        tags: Vec<ForumTag>,
        requires_tag: bool,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.forum == forum && self.forum_tags == tags && self.requires_tag == requires_tag {
            return;
        }
        self.forum = forum;
        self.forum_tags = tags;
        self.requires_tag = requires_tag;
        cx.notify();
    }

    /// The open parent's resolved permissions. Refreshed by the screen
    /// whenever its own permission picture changes.
    pub(super) fn set_permissions(
        &mut self,
        permissions: Permissions,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.permissions == permissions {
            return;
        }
        self.permissions = permissions;
        cx.notify();
    }

    /// The summary of a known thread, for the conversation header of an
    /// open thread and its composer gating.
    pub(super) fn get(&self, thread_id: Id<ChannelMarker>) -> Option<&ThreadSummary> {
        self.lists.get(thread_id)
    }

    /// The list the panel is currently showing.
    pub(super) fn visible(&self) -> &[ThreadSummary] {
        match self.tab {
            Tab::Active => &self.lists.active,
            Tab::Archived => &self.lists.archived,
        }
    }

    /// Whether the current user may talk in `thread_id`: `None` for an
    /// unknown thread, which the conversation header treats as not theirs to
    /// gate on.
    pub(super) fn can_send_in(&self, thread_id: Id<ChannelMarker>) -> Option<bool> {
        Some(data::can_reply(self.permissions, self.get(thread_id)?))
    }

    /// Applies a gateway event. Events for other guilds or other parents are
    /// ignored — the panel only ever holds the open parent's threads.
    pub(super) fn apply_event(
        &mut self,
        event: crate::discord::ThreadEvent,
        cx: &mut Context<HomeScreen>,
    ) {
        let Some((guild_id, channel_id)) = self.parent else {
            return;
        };
        if self
            .lists
            .apply_event(guild_id, channel_id, &event, MAX_THREADS_PER_LIST)
        {
            cx.notify();
        }
    }
}
