//! State and data management for user relationships (friends, requests, blocked).
//!
//! Owns lazy loading from the server, tab selection, per-row busy/error states,
//! session fences, live gateway event application, and journaled coalescing
//! between live dispatches and in-flight HTTP snapshots so events are never lost,
//! removed friends never resurrect, and journal overflow triggers a safe resync.

use std::collections::{HashMap, HashSet};

use gpui::*;
use gpui_component::input::{InputEvent, InputState};
use twilight_model::id::{Id, marker::UserMarker};

use crate::discord::PresenceStatus;
use crate::discord::relationships::*;
use crate::screens::home::HomeScreen;

/// Upper bound on the pending live events journal during in-flight fetches.
pub const MAX_JOURNAL_SIZE: usize = 256;

/// Active tab on the Friends screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FriendsTab {
    #[default]
    Online,
    All,
    Pending,
    Blocked,
    AddFriend,
}

/// Status feedback resulting from an outgoing friend request submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddFriendStatus {
    Success(String),
    Error(String),
}

/// Outcome of attempting to complete an HTTP fetch against the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchCompletionOutcome {
    /// Snapshot successfully reconciled with in-flight journal and applied.
    Applied,
    /// Live events exceeded journal capacity; snapshot was safely rejected and a fresh resync is needed.
    NeedsResync,
    /// The HTTP request returned an error.
    Error(String),
    /// The completion came from an older generation and was ignored.
    Stale,
}

/// Core production state and reducers, decoupled from GUI window handles so it can be tested directly.
#[derive(Default)]
pub struct RelationshipsStore {
    pub relationships: Vec<Relationship>,
    pub loaded: bool,
    pub loading: bool,
    pub refreshing: bool,
    pub error: Option<String>,
    pub active_tab: FriendsTab,
    pub add_friend_status: Option<AddFriendStatus>,
    pub add_friend_submitting: bool,
    pub busy_users: HashSet<Id<UserMarker>>,
    pub row_errors: HashMap<Id<UserMarker>, String>,
    pub blocked_cache: HashSet<Id<UserMarker>>,
    pub filtered_ids: Vec<Id<UserMarker>>,
    /// Monotonic generation counter for HTTP fetches.
    pub fetch_generation: u64,
    /// Monotonic generation counter for user session (logout / reset).
    pub session_generation: u64,
    /// Whether live events exceeded the journal cap during an in-flight fetch.
    pub journal_overflowed: bool,
    /// Coalescing journal: records live gateway additions/removals while an HTTP fetch is in flight.
    /// `Some(rel)` = added/updated live; `None` = removed live.
    pub pending_event_journal: HashMap<Id<UserMarker>, Option<Relationship>>,
}

impl RelationshipsStore {
    /// Fast O(1) query to check if a user is currently blocked.
    pub fn is_blocked(&self, user_id: Id<UserMarker>) -> bool {
        self.blocked_cache.contains(&user_id)
    }

    /// Recomputes the fast blocked set from current relationships.
    pub fn sync_blocked_cache(&mut self) {
        self.blocked_cache = self
            .relationships
            .iter()
            .filter(|r| r.kind.is_blocked())
            .map(|r| r.id)
            .collect();
    }

    /// Clears all local state and advances generations to fence off pending tasks.
    pub fn reset(&mut self) {
        self.relationships.clear();
        self.blocked_cache.clear();
        self.busy_users.clear();
        self.row_errors.clear();
        self.filtered_ids.clear();
        self.pending_event_journal.clear();
        self.journal_overflowed = false;
        self.loaded = false;
        self.loading = false;
        self.refreshing = false;
        self.error = None;
        self.active_tab = FriendsTab::Online;
        self.add_friend_status = None;
        self.add_friend_submitting = false;
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
        self.session_generation = self.session_generation.wrapping_add(1);
    }

    /// Replaces the relationships list from the initial READY dispatch snapshot.
    pub fn replace_snapshot(&mut self, snapshot: Vec<Relationship>) {
        self.relationships = snapshot;
        self.sync_blocked_cache();
        self.loaded = true;
        self.loading = false;
        self.refreshing = false;
        self.error = None;
        self.pending_event_journal.clear();
        self.journal_overflowed = false;
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
    }

    /// Starts an initial load, advancing fetch generation and clearing journal.
    pub fn start_load(&mut self) -> Option<u64> {
        if self.loaded || self.loading {
            return None;
        }
        self.loading = true;
        self.error = None;
        self.pending_event_journal.clear();
        self.journal_overflowed = false;
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
        Some(self.fetch_generation)
    }

    /// Starts a background refresh, advancing fetch generation and clearing journal.
    pub fn start_refresh(&mut self) -> Option<u64> {
        if self.refreshing {
            return None;
        }
        self.refreshing = true;
        self.pending_event_journal.clear();
        self.journal_overflowed = false;
        self.fetch_generation = self.fetch_generation.wrapping_add(1);
        Some(self.fetch_generation)
    }

    /// Applies a live gateway event (Add/Update or Remove).
    /// Always updates existing IDs in-place without consuming extra cap slots.
    /// If a brand new ID arrives while at cap, marks `journal_overflowed` so the unsafe snapshot is rejected.
    pub fn apply_event(&mut self, event: &RelationshipEvent) {
        match event {
            RelationshipEvent::Add(rel) => {
                if let Some(pos) = self.relationships.iter().position(|r| r.id == rel.id) {
                    self.relationships[pos] = rel.clone();
                } else {
                    self.relationships.push(rel.clone());
                }

                if self.loading || self.refreshing {
                    if let Some(entry) = self.pending_event_journal.get_mut(&rel.id) {
                        *entry = Some(rel.clone());
                    } else if self.pending_event_journal.len() < MAX_JOURNAL_SIZE {
                        self.pending_event_journal.insert(rel.id, Some(rel.clone()));
                    } else {
                        self.journal_overflowed = true;
                    }
                }
            }
            RelationshipEvent::Remove(rem) => {
                self.relationships.retain(|r| r.id != rem.id);
                self.row_errors.remove(&rem.id);

                if self.loading || self.refreshing {
                    if let Some(entry) = self.pending_event_journal.get_mut(&rem.id) {
                        *entry = None;
                    } else if self.pending_event_journal.len() < MAX_JOURNAL_SIZE {
                        self.pending_event_journal.insert(rem.id, None);
                    } else {
                        self.journal_overflowed = true;
                    }
                }
            }
        }
        self.sync_blocked_cache();
    }

    /// Completes an HTTP fetch with coalescing of live events.
    /// If overflow occurred, safely drops the snapshot and requests a clean resync.
    pub fn handle_fetch_completion(
        &mut self,
        fetch_gen: u64,
        result: Result<Vec<Relationship>, String>,
    ) -> FetchCompletionOutcome {
        if self.fetch_generation != fetch_gen {
            return FetchCompletionOutcome::Stale;
        }

        self.loading = false;
        self.refreshing = false;

        if self.journal_overflowed {
            self.pending_event_journal.clear();
            self.journal_overflowed = false;
            return FetchCompletionOutcome::NeedsResync;
        }

        match result {
            Ok(mut snapshot) => {
                for (id, live_action) in self.pending_event_journal.drain() {
                    match live_action {
                        None => {
                            snapshot.retain(|r| r.id != id);
                        }
                        Some(live_rel) => {
                            if let Some(pos) = snapshot.iter().position(|r| r.id == id) {
                                snapshot[pos] = live_rel;
                            } else {
                                snapshot.push(live_rel);
                            }
                        }
                    }
                }
                self.relationships = snapshot;
                self.sync_blocked_cache();
                self.loaded = true;
                self.error = None;
                FetchCompletionOutcome::Applied
            }
            Err(err) => {
                self.pending_event_journal.clear();
                self.error = Some(err.clone());
                FetchCompletionOutcome::Error(err)
            }
        }
    }

    /// Completes a mutation with session generation fencing.
    /// Returns `true` if accepted, or `false` if rejected due to session reset.
    pub fn handle_mutation_completion(
        &mut self,
        session_gen: u64,
        user_id: Id<UserMarker>,
        result: Result<(), String>,
    ) -> bool {
        if self.session_generation != session_gen {
            return false;
        }
        self.busy_users.remove(&user_id);
        match result {
            Ok(()) => {}
            Err(err) => {
                self.row_errors.insert(user_id, err);
            }
        }
        true
    }

    /// Count of friends who are currently active (Online, Idle, or DoNotDisturb).
    pub fn online_count(&self, presences: &HashMap<Id<UserMarker>, PresenceStatus>) -> usize {
        self.relationships
            .iter()
            .filter(|r| {
                r.kind.is_friend()
                    && matches!(
                        presences.get(&r.id),
                        Some(
                            PresenceStatus::Online
                                | PresenceStatus::Idle
                                | PresenceStatus::DoNotDisturb
                        )
                    )
            })
            .count()
    }

    /// Count of all friends.
    pub fn all_friends_count(&self) -> usize {
        self.relationships
            .iter()
            .filter(|r| r.kind.is_friend())
            .count()
    }

    /// Count of pending incoming and outgoing requests.
    pub fn pending_count(&self) -> usize {
        self.relationships
            .iter()
            .filter(|r| r.kind.is_pending())
            .count()
    }

    /// Count of blocked users.
    pub fn blocked_count(&self) -> usize {
        self.relationships
            .iter()
            .filter(|r| r.kind.is_blocked())
            .count()
    }

    /// Recomputes the cached list of user IDs matching the active tab and search query.
    pub fn recompute_filtered(
        &mut self,
        search_query: &str,
        presences: &HashMap<Id<UserMarker>, PresenceStatus>,
    ) {
        let query_lower = search_query.trim().to_lowercase();
        self.filtered_ids = self
            .relationships
            .iter()
            .filter(|r| {
                let matches_tab = match self.active_tab {
                    FriendsTab::Online => {
                        r.kind.is_friend()
                            && matches!(
                                presences.get(&r.id),
                                Some(
                                    PresenceStatus::Online
                                        | PresenceStatus::Idle
                                        | PresenceStatus::DoNotDisturb
                                )
                            )
                    }
                    FriendsTab::All => r.kind.is_friend(),
                    FriendsTab::Pending => r.kind.is_pending(),
                    FriendsTab::Blocked => r.kind.is_blocked(),
                    FriendsTab::AddFriend => false,
                };

                if !matches_tab {
                    return false;
                }

                if query_lower.is_empty() {
                    return true;
                }

                let name_match = r.user.display_name().to_lowercase().contains(&query_lower);
                let username_match = r.user.username.to_lowercase().contains(&query_lower);
                let nick_match = r
                    .nickname
                    .as_deref()
                    .map(|n| n.to_lowercase().contains(&query_lower))
                    .unwrap_or(false);

                name_match || username_match || nick_match
            })
            .map(|r| r.id)
            .collect();
    }
}

/// Relationship management state held inside [`HomeScreen`].
pub struct RelationshipsState {
    pub store: RelationshipsStore,
    pub search_input: Entity<InputState>,
    pub add_friend_input: Entity<InputState>,
    pub scroll: UniformListScrollHandle,
    pub load_task: Option<Task<()>>,
    pub _search_sub: Subscription,
    pub _add_friend_sub: Subscription,
}

impl std::ops::Deref for RelationshipsState {
    type Target = RelationshipsStore;
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl std::ops::DerefMut for RelationshipsState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.store
    }
}

impl RelationshipsState {
    /// Constructs a new [`RelationshipsState`].
    pub fn new(window: &mut Window, cx: &mut Context<HomeScreen>) -> Self {
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let add_friend_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("You can add friends with their Discord username.")
        });

        let search_sub = cx.subscribe(&search_input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let query = this.relationships.search_input.read(cx).value().to_string();
                this.relationships
                    .store
                    .recompute_filtered(&query, &this.user_presences);
                cx.notify();
            }
        });

        let add_friend_sub = cx.subscribe(&add_friend_input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                if this.relationships.add_friend_status.is_some() {
                    this.relationships.add_friend_status = None;
                }
                cx.notify();
            }
        });

        Self {
            store: RelationshipsStore::default(),
            search_input,
            add_friend_input,
            scroll: UniformListScrollHandle::new(),
            load_task: None,
            _search_sub: search_sub,
            _add_friend_sub: add_friend_sub,
        }
    }

    /// Fast O(1) query to check if a user is currently blocked.
    pub fn is_blocked(&self, user_id: Id<UserMarker>) -> bool {
        self.store.is_blocked(user_id)
    }

    /// Clears all local state and cancels pending tasks (called on logout or session reset).
    pub fn reset(&mut self) {
        self.load_task = None;
        self.store.reset();
    }

    /// Replaces the relationships list from the initial READY dispatch snapshot.
    pub fn replace_snapshot(&mut self, snapshot: Vec<Relationship>, cx: &mut Context<HomeScreen>) {
        self.load_task = None;
        self.store.replace_snapshot(snapshot);
        let query = self.search_input.read(cx).value().to_string();
        self.store.recompute_filtered(&query, &HashMap::new());
        cx.notify();
    }

    /// Switches the active friends tab.
    pub fn set_tab(
        &mut self,
        tab: FriendsTab,
        presences: &HashMap<Id<UserMarker>, PresenceStatus>,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.active_tab != tab {
            self.active_tab = tab;
            let query = self.search_input.read(cx).value().to_string();
            self.store.recompute_filtered(&query, presences);
            cx.notify();
        }
    }

    /// Lazily fetches the server relationships list if not already loaded or in flight.
    pub fn load(&mut self, cx: &mut Context<HomeScreen>) {
        let Some(current_gen) = self.store.start_load() else {
            return;
        };
        let session_gen = self.session_generation;

        let task = cx.spawn(async move |this, cx| {
            let result = fetch_relationships().await;
            let _ = this.update(cx, |this, cx| {
                if this.relationships.session_generation != session_gen {
                    return;
                }
                let outcome = this
                    .relationships
                    .store
                    .handle_fetch_completion(current_gen, result);

                match outcome {
                    FetchCompletionOutcome::Applied => {
                        let query = this.relationships.search_input.read(cx).value().to_string();
                        this.relationships
                            .store
                            .recompute_filtered(&query, &this.user_presences);
                    }
                    FetchCompletionOutcome::NeedsResync => {
                        this.relationships.refresh(cx);
                    }
                    FetchCompletionOutcome::Error(_) | FetchCompletionOutcome::Stale => {}
                }
                cx.notify();
            });
        });

        self.load_task = Some(task);
        cx.notify();
    }

    /// Safely refetches relationships in the background without clearing current UI elements.
    pub fn refresh(&mut self, cx: &mut Context<HomeScreen>) {
        let Some(current_gen) = self.store.start_refresh() else {
            return;
        };
        let session_gen = self.session_generation;

        cx.spawn(async move |this, cx| {
            let result = fetch_relationships().await;
            let _ = this.update(cx, |this, cx| {
                if this.relationships.session_generation != session_gen {
                    return;
                }
                let outcome = this
                    .relationships
                    .store
                    .handle_fetch_completion(current_gen, result);

                match outcome {
                    FetchCompletionOutcome::Applied => {
                        let query = this.relationships.search_input.read(cx).value().to_string();
                        this.relationships
                            .store
                            .recompute_filtered(&query, &this.user_presences);
                    }
                    FetchCompletionOutcome::NeedsResync => {
                        this.relationships.refresh(cx);
                    }
                    FetchCompletionOutcome::Error(_) | FetchCompletionOutcome::Stale => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Applies live gateway events (`RELATIONSHIP_ADD` or `RELATIONSHIP_REMOVE`).
    pub fn apply_event(&mut self, event: &RelationshipEvent, cx: &mut Context<HomeScreen>) {
        self.store.apply_event(event);
        cx.notify();
    }

    /// Sends an outgoing friend request based on the current text in `add_friend_input`.
    pub fn send_friend_request(&mut self, window: &mut Window, cx: &mut Context<HomeScreen>) {
        if self.add_friend_submitting {
            return;
        }

        let raw_tag = self.add_friend_input.read(cx).value().to_string();
        let (username, discriminator) = parse_friend_tag(&raw_tag);
        if username.trim().is_empty() {
            return;
        }

        self.add_friend_submitting = true;
        self.add_friend_status = None;
        self.add_friend_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        cx.notify();

        let display_name = raw_tag.trim().to_string();
        let session_gen = self.session_generation;

        cx.spawn(async move |this, cx| {
            let result = send_friend_request(&username, discriminator.as_deref(), None).await;
            let _ = this.update(cx, |this, cx| {
                if this.relationships.session_generation != session_gen {
                    return;
                }
                this.relationships.add_friend_submitting = false;
                match result {
                    Ok(()) => {
                        this.relationships.add_friend_status = Some(AddFriendStatus::Success(
                            format!("Success! Your friend request to {display_name} was sent."),
                        ));
                        this.relationships.refresh(cx);
                    }
                    Err(err) => {
                        this.relationships.add_friend_status = Some(AddFriendStatus::Error(err));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Accepts an incoming friend request.
    pub fn accept_request(&mut self, user_id: Id<UserMarker>, cx: &mut Context<HomeScreen>) {
        if self.busy_users.contains(&user_id) {
            return;
        }

        self.busy_users.insert(user_id);
        self.row_errors.remove(&user_id);
        cx.notify();

        let session_gen = self.session_generation;

        cx.spawn(async move |this, cx| {
            let result = accept_friend_request(user_id).await;
            let _ = this.update(cx, |this, cx| {
                let accepted = this.relationships.store.handle_mutation_completion(
                    session_gen,
                    user_id,
                    result,
                );
                if accepted {
                    if let Some(rel) = this
                        .relationships
                        .relationships
                        .iter_mut()
                        .find(|r| r.id == user_id)
                    {
                        rel.kind = RelationshipType::Friend;
                    }
                    this.relationships.sync_blocked_cache();
                    this.relationships.refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Declines an incoming friend request.
    pub fn decline_request(&mut self, user_id: Id<UserMarker>, cx: &mut Context<HomeScreen>) {
        self.perform_deletion(user_id, decline_friend_request(user_id), cx);
    }

    /// Cancels an outgoing friend request.
    pub fn cancel_request(&mut self, user_id: Id<UserMarker>, cx: &mut Context<HomeScreen>) {
        self.perform_deletion(user_id, cancel_friend_request(user_id), cx);
    }

    /// Removes a user from the friends list.
    pub fn remove_friend(&mut self, user_id: Id<UserMarker>, cx: &mut Context<HomeScreen>) {
        self.perform_deletion(user_id, remove_friend(user_id), cx);
    }

    /// Blocks a target user.
    pub fn block_user(&mut self, user_id: Id<UserMarker>, cx: &mut Context<HomeScreen>) {
        if self.busy_users.contains(&user_id) {
            return;
        }

        self.busy_users.insert(user_id);
        self.row_errors.remove(&user_id);
        cx.notify();

        let session_gen = self.session_generation;

        cx.spawn(async move |this, cx| {
            let result = block_user(user_id).await;
            let _ = this.update(cx, |this, cx| {
                let accepted = this.relationships.store.handle_mutation_completion(
                    session_gen,
                    user_id,
                    result,
                );
                if accepted {
                    if let Some(rel) = this
                        .relationships
                        .relationships
                        .iter_mut()
                        .find(|r| r.id == user_id)
                    {
                        rel.kind = RelationshipType::Blocked;
                    }
                    this.relationships.sync_blocked_cache();
                    this.relationships.refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Unblocks a previously blocked user.
    pub fn unblock_user(&mut self, user_id: Id<UserMarker>, cx: &mut Context<HomeScreen>) {
        self.perform_deletion(user_id, unblock_user(user_id), cx);
    }

    fn perform_deletion(
        &mut self,
        user_id: Id<UserMarker>,
        future: impl Future<Output = Result<(), String>> + Send + 'static,
        cx: &mut Context<HomeScreen>,
    ) {
        if self.busy_users.contains(&user_id) {
            return;
        }

        self.busy_users.insert(user_id);
        self.row_errors.remove(&user_id);
        cx.notify();

        let session_gen = self.session_generation;

        cx.spawn(async move |this, cx| {
            let result = future.await;
            let _ = this.update(cx, |this, cx| {
                let accepted = this.relationships.store.handle_mutation_completion(
                    session_gen,
                    user_id,
                    result,
                );
                if accepted {
                    this.relationships.relationships.retain(|r| r.id != user_id);
                    this.relationships.sync_blocked_cache();
                    this.relationships.refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}
