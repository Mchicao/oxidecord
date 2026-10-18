//! Bounded in-memory navigation history for conversations and the DM home view.
//!
//! Each entry names a destination the user opened: a guild text channel, a DM
//! conversation, or the DM list with no conversation selected. Navigating back
//! or forward moves the cursor through the stack without recording a new visit,
//! while a fresh visit after stepping back drops the forward branch.

use std::collections::VecDeque;

use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker},
};

/// Maximum number of destinations kept in memory. When full, pushing a new
/// destination drops the oldest one from the back stack.
pub(super) const MAX_NAVIGATION_HISTORY: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MessageTarget {
    pub guild_id: Option<Id<GuildMarker>>,
    pub channel_id: Id<ChannelMarker>,
    pub message_id: Id<MessageMarker>,
}

/// A place in the home screen that back/forward navigation can return to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NavigationEntry {
    /// The Direct Messages view with no conversation open.
    DirectMessagesHome,
    /// A specific DM conversation in the Direct Messages view.
    DirectMessage { channel_id: Id<ChannelMarker> },
    /// A text-like channel inside a guild.
    GuildChannel {
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
    },
}

/// Bounded back/forward stack over [`NavigationEntry`] values.
#[derive(Debug)]
pub(super) struct NavigationHistory {
    entries: VecDeque<NavigationEntry>,
    /// Index of the active destination in `entries`, when the history is non-empty.
    current: Option<usize>,
    capacity: usize,
}

impl Default for NavigationHistory {
    fn default() -> Self {
        Self::with_capacity(MAX_NAVIGATION_HISTORY)
    }
}

impl NavigationHistory {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity),
            current: None,
            capacity,
        }
    }

    /// Records a user-initiated visit to `entry`.
    ///
    /// Selecting the active destination again is a no-op. Visiting a new
    /// destination after stepping back truncates the forward branch, and
    /// exceeding the capacity evicts the oldest entry from the front.
    pub(super) fn push(&mut self, entry: NavigationEntry) {
        if self.current() == Some(entry) {
            return;
        }
        if let Some(cur) = self.current {
            self.entries.truncate(cur + 1);
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.current = Some(self.entries.len() - 1);
    }

    pub(super) fn replace_current(&mut self, entry: NavigationEntry) {
        if let Some(current) = self.current {
            self.entries[current] = entry;
        } else {
            self.push(entry);
        }
    }

    /// The active destination, if any visit has been recorded.
    pub(super) fn current(&self) -> Option<NavigationEntry> {
        self.current.and_then(|ix| self.entries.get(ix).copied())
    }

    /// Moves the cursor to the newest reachable entry before `current`,
    /// skipping destinations that `is_valid` rejects. Leaves the cursor
    /// untouched if no valid earlier entry exists.
    pub(super) fn navigate_back(
        &mut self,
        mut is_valid: impl FnMut(NavigationEntry) -> bool,
    ) -> Option<NavigationEntry> {
        let cur = self.current?;
        for ix in (0..cur).rev() {
            let entry = self.entries[ix];
            if is_valid(entry) {
                self.current = Some(ix);
                return Some(entry);
            }
        }
        None
    }

    /// Moves the cursor to the oldest reachable entry after `current`,
    /// skipping destinations that `is_valid` rejects. Leaves the cursor
    /// untouched if no valid later entry exists.
    pub(super) fn navigate_forward(
        &mut self,
        mut is_valid: impl FnMut(NavigationEntry) -> bool,
    ) -> Option<NavigationEntry> {
        let cur = self.current?;
        for ix in (cur + 1)..self.entries.len() {
            let entry = self.entries[ix];
            if is_valid(entry) {
                self.current = Some(ix);
                return Some(entry);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guild_channel(guild: u64, channel: u64) -> NavigationEntry {
        NavigationEntry::GuildChannel {
            guild_id: Id::new(guild),
            channel_id: Id::new(channel),
        }
    }

    fn dm(channel: u64) -> NavigationEntry {
        NavigationEntry::DirectMessage {
            channel_id: Id::new(channel),
        }
    }

    #[test]
    fn back_and_forward_walk_recorded_visits_without_duplicating() {
        let mut history = NavigationHistory::default();
        let a = guild_channel(1, 10);
        let b = guild_channel(1, 20);
        let c = dm(30);

        history.push(a);
        history.push(b);
        history.push(c);
        assert_eq!(history.current(), Some(c));

        assert_eq!(history.navigate_back(|_| true), Some(b));
        assert_eq!(history.current(), Some(b));

        assert_eq!(history.navigate_back(|_| true), Some(a));
        assert_eq!(history.current(), Some(a));

        // At the oldest entry, going back again does nothing.
        assert_eq!(history.navigate_back(|_| true), None);
        assert_eq!(history.current(), Some(a));

        assert_eq!(history.navigate_forward(|_| true), Some(b));
        assert_eq!(history.navigate_forward(|_| true), Some(c));
        assert_eq!(history.navigate_forward(|_| true), None);
        assert_eq!(history.current(), Some(c));
    }

    #[test]
    fn new_visit_after_stepping_back_truncates_forward_branch() {
        let mut history = NavigationHistory::default();
        let a = guild_channel(1, 10);
        let b = guild_channel(1, 20);
        let c = guild_channel(2, 30);
        let d = NavigationEntry::DirectMessagesHome;

        history.push(a);
        history.push(b);
        history.push(c);

        assert_eq!(history.navigate_back(|_| true), Some(b));
        assert_eq!(history.navigate_back(|_| true), Some(a));

        history.push(d);
        assert_eq!(history.current(), Some(d));
        assert_eq!(history.navigate_forward(|_| true), None);
        assert_eq!(history.navigate_back(|_| true), Some(a));
    }

    #[test]
    fn duplicate_selection_and_capacity_bounds_preserve_invariants() {
        let mut history = NavigationHistory::with_capacity(3);
        let a = guild_channel(1, 10);
        let b = guild_channel(1, 20);
        let c = guild_channel(1, 30);
        let d = guild_channel(1, 40);

        history.push(a);
        history.push(a);
        assert_eq!(history.navigate_back(|_| true), None);

        history.push(b);
        history.push(b);
        history.push(c);
        history.push(d);

        // `a` was evicted when `d` exceeded the capacity of 3.
        assert_eq!(history.current(), Some(d));
        assert_eq!(history.navigate_back(|_| true), Some(c));
        assert_eq!(history.navigate_back(|_| true), Some(b));
        assert_eq!(history.navigate_back(|_| true), None);
    }

    #[test]
    fn invalid_destinations_are_skipped_without_moving_cursor_when_none_remain() {
        let mut history = NavigationHistory::default();
        let a = guild_channel(1, 10);
        let deleted = guild_channel(1, 20);
        let c = dm(30);

        history.push(a);
        history.push(deleted);
        history.push(c);

        // `deleted` is skipped, landing straight on `a`.
        assert_eq!(history.navigate_back(|entry| entry != deleted), Some(a));
        assert_eq!(history.current(), Some(a));

        // If every forward entry is invalid, the cursor stays at `a`.
        assert_eq!(history.navigate_forward(|_| false), None);
        assert_eq!(history.current(), Some(a));

        // Once `c` is valid again, forward skips `deleted` and lands on `c`.
        assert_eq!(history.navigate_forward(|entry| entry != deleted), Some(c));
        assert_eq!(history.current(), Some(c));
    }
    #[test]
    fn restoring_a_fallback_preserves_the_forward_branch() {
        let mut history = NavigationHistory::default();
        let deleted = guild_channel(1, 10);
        let fallback = guild_channel(1, 20);
        let next = dm(30);
        history.push(deleted);
        history.push(next);
        assert_eq!(history.navigate_back(|_| true), Some(deleted));
        history.replace_current(fallback);
        assert_eq!(history.current(), Some(fallback));
        assert_eq!(history.navigate_forward(|_| true), Some(next));
        assert_eq!(history.navigate_back(|_| true), Some(fallback));
    }
}
