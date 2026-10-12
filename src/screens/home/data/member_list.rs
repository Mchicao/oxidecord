//! The member list beside a guild conversation: which channel's list the
//! gateway is subscribed to, and Discord's edits to it as they arrive.

use std::ops::Range;

use gpui::*;

use crate::discord::{self, ChannelKind, MemberListItem, MemberListOp};
use crate::platform::prefs;
use crate::screens::home::state::MemberList;
use crate::screens::home::{HomeScreen, View};

/// How many rows each subscribed range covers, as in Discord's client.
const RANGE_SIZE: usize = 100;

/// The most ranges asked for at once: the first, always, so the top of the
/// list stays live, and the two the view can straddle.
const MAX_RANGES: usize = 3;

impl HomeScreen {
    /// Whether the open conversation shows a member list, hidden or not:
    /// a guild's text or announcement channel.
    pub(in crate::screens::home) fn has_member_list(&self) -> bool {
        self.view == View::Guild
            && self.selected_channel_info().is_some_and(|channel| {
                matches!(channel.kind, ChannelKind::Text | ChannelKind::Announcement)
            })
    }

    /// Subscribes to the open channel's member list when the open channel
    /// has changed, and drops the list when the conversation has none.
    /// Called every render, so every path that changes the selection is
    /// caught; it does nothing unless the channel changed.
    pub(in crate::screens::home) fn sync_member_list(&mut self) {
        let wanted = self
            .has_member_list()
            .then(|| self.selected_guild.zip(self.selected_channel))
            .flatten();
        let Some((guild_id, channel_id)) = wanted else {
            self.member_list = None;
            return;
        };
        match &mut self.member_list {
            Some(list) if list.guild_id == guild_id && list.channel_id == channel_id => return,
            // Another channel in the same guild: what's shown stays up until
            // Discord says which list this channel has.
            Some(list) if list.guild_id == guild_id => {
                list.channel_id = channel_id;
                list.ranges = vec![[0, RANGE_SIZE - 1]];
                list.scroll = UniformListScrollHandle::new();
            }
            list => {
                *list = Some(MemberList {
                    guild_id,
                    channel_id,
                    id: None,
                    groups: Vec::new(),
                    rows: Vec::new(),
                    ranges: vec![[0, RANGE_SIZE - 1]],
                    scroll: UniformListScrollHandle::new(),
                })
            }
        }
        self.resubscribe_member_list();
    }

    /// Sends the open list's subscription again — after a new session, which
    /// starts with none.
    pub(in crate::screens::home) fn resubscribe_member_list(&self) {
        if let (Some(list), Some(gateway)) = (&self.member_list, &self.gateway) {
            gateway.subscribe_member_list(list.guild_id, list.channel_id, &list.ranges);
        }
    }

    /// Follows the list's scroll position with the ranges subscribed to, so
    /// the rows coming into view get synced. Called with the rows the view is
    /// about to draw.
    pub(in crate::screens::home) fn track_member_list_range(&mut self, visible: Range<usize>) {
        let Some(list) = &mut self.member_list else {
            return;
        };
        let first = visible.start / RANGE_SIZE;
        let last = visible.end.saturating_sub(1) / RANGE_SIZE;
        let ranges: Vec<[usize; 2]> = std::iter::once(0)
            .chain((first..=last).filter(|&chunk| chunk > 0))
            .take(MAX_RANGES)
            .map(|chunk| [chunk * RANGE_SIZE, (chunk + 1) * RANGE_SIZE - 1])
            .collect();
        if ranges != list.ranges {
            list.ranges = ranges;
            self.resubscribe_member_list();
        }
    }

    pub(in crate::screens::home) fn apply_member_list_update(
        &mut self,
        update: discord::MemberListUpdate,
        cx: &mut Context<Self>,
    ) {
        // Whatever list they're from, the members' roles and statuses are
        // current, and colour their names and avatars elsewhere too.
        let roles = self.member_roles.entry(update.guild_id).or_default();
        for op in &update.ops {
            let items = match op {
                MemberListOp::Sync { items, .. } => items.as_slice(),
                MemberListOp::Insert { item, .. } | MemberListOp::Update { item, .. } => {
                    std::slice::from_ref(item)
                }
                MemberListOp::Delete { .. } | MemberListOp::Invalidate { .. } => &[],
            };
            for item in items {
                if let MemberListItem::Member(member) = item {
                    roles.insert(member.user_id, member.roles.clone());
                    if Some(member.user_id) != self.self_user_id {
                        self.user_presences.insert(member.user_id, member.status);
                    }
                }
            }
        }

        let Some(list) = self
            .member_list
            .as_mut()
            .filter(|list| list.guild_id == update.guild_id)
        else {
            return;
        };
        if list.id.as_deref() != Some(update.list_id.as_str()) {
            // Edits to a list that isn't the one shown are a channel left
            // behind. A sync of another means the open channel's list has
            // arrived in its place.
            if !update
                .ops
                .iter()
                .any(|op| matches!(op, MemberListOp::Sync { .. }))
            {
                return;
            }
            list.id = Some(update.list_id);
            list.rows.clear();
        }

        let rows = &mut list.rows;
        for op in update.ops {
            match op {
                MemberListOp::Sync { start, items } => {
                    let end = start + items.len();
                    if rows.len() < end {
                        rows.resize(end, None);
                    }
                    for (row, item) in rows[start..end].iter_mut().zip(items) {
                        *row = Some(item);
                    }
                }
                MemberListOp::Insert { index, item } => {
                    if rows.len() < index {
                        rows.resize(index, None);
                    }
                    rows.insert(index, Some(item));
                }
                MemberListOp::Update { index, item } => {
                    if rows.len() <= index {
                        rows.resize(index + 1, None);
                    }
                    rows[index] = Some(item);
                }
                MemberListOp::Delete { index } => {
                    if index < rows.len() {
                        rows.remove(index);
                    }
                }
                MemberListOp::Invalidate { start, end } => {
                    let end = (end + 1).min(rows.len());
                    if start < end {
                        rows[start..end].fill(None);
                    }
                }
            }
        }

        // The groups say how long the list is: a header for each group with
        // anyone in it, then its members. Rows past what's been synced are
        // placeholders until they are.
        list.groups = update.groups;
        let len = list
            .groups
            .iter()
            .filter(|group| group.count > 0)
            .map(|group| group.count + 1)
            .sum();
        list.rows.resize(len, None);
        cx.notify();
    }

    pub(in crate::screens::home) fn toggle_member_list(&mut self, cx: &mut Context<Self>) {
        self.member_list_hidden = !self.member_list_hidden;
        let hidden = self.member_list_hidden;
        prefs::update(|prefs| prefs.member_list_hidden = hidden);
        cx.notify();
    }
}
