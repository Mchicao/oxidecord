//! Threads and forum posts: the app-shaped summaries the threads panel
//! renders, the conversions that build them from twilight's channel objects,
//! and the gateway events that keep them current.
//!
//! Discord models a thread as a channel, so listing responses are channels
//! alongside the current user's thread members. As with every other model,
//! the twilight types stay confined to this module and the REST/gateway
//! layers that read them.

use std::collections::HashSet;

use twilight_model::channel::forum::ForumTag as RawForumTag;
use twilight_model::channel::{ChannelFlags, ChannelType, thread::ThreadMember};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, EmojiMarker, GuildMarker, MessageMarker, TagMarker, UserMarker},
};
use twilight_model::util::Timestamp;

use super::channel::ChannelKind;

/// How long a post preview may be before it is cut off. One line of a list
/// row is a couple of hundred pixels; two hundred characters is generous.
const PREVIEW_MAX_CHARS: usize = 200;

/// A tag a forum channel offers for its posts.
#[derive(Clone, PartialEq, Eq)]
pub struct ForumTag {
    pub id: Id<TagMarker>,
    pub name: String,
    /// Only members holding `MANAGE_THREADS` may add or remove this tag.
    pub moderated: bool,
    pub emoji_id: Option<Id<EmojiMarker>>,
    pub emoji_name: Option<String>,
}

/// One thread or forum post, as far as the panel needs it.
///
/// Every field is what the last thing that mentioned the thread said. An
/// update missing an optional field merges against what was known rather
/// than clearing it — see [`ThreadEvent::Upsert`]'s merge rules in the
/// panel's reducer.
#[derive(Clone)]
pub struct ThreadSummary {
    pub id: Id<ChannelMarker>,
    /// The text or forum channel the thread hangs from.
    pub parent_id: Id<ChannelMarker>,
    pub guild_id: Option<Id<GuildMarker>>,
    pub name: String,
    pub kind: ChannelKind,
    pub archived: bool,
    /// A locked thread only takes messages from members holding
    /// `MANAGE_THREADS`, archived or not.
    pub locked: bool,
    /// When the thread's archive status last changed, which is also the
    /// cursor the archived listing pages by.
    pub archive_timestamp: Timestamp,
    pub message_count: Option<u32>,
    /// Discord stops counting at 50.
    pub member_count: Option<i32>,
    /// The user who started the thread.
    pub owner_id: Option<Id<UserMarker>>,
    /// Tags applied to a forum post, at most five.
    pub applied_tags: Vec<Id<TagMarker>>,
    /// Whether the current user is a member. Membership is what a follow
    /// means: it shows the thread in the channel list and carries its
    /// notifications.
    pub joined: bool,
    pub last_message_id: Option<Id<MessageMarker>>,
    /// The post's first message, when the listing carries it (forum search
    /// does). Plain text, cut off for the list row.
    pub preview: Option<String>,
    /// Pinned to the top of its forum channel.
    pub pinned: bool,
}

/// One page of a thread listing.
#[derive(Clone)]
pub struct ThreadPage {
    pub threads: Vec<ThreadSummary>,
    /// Whether another page may exist past this one.
    pub has_more: bool,
}

/// Which slice of a channel's archived threads a listing asks for. The three
/// differ in route, ordering, and permission, more than they share.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchivedScope {
    /// Everyone's archived public threads, by archive time. Needs
    /// `READ_MESSAGE_HISTORY`.
    Public,
    /// Everyone's archived private threads, by archive time. Needs
    /// `READ_MESSAGE_HISTORY` and `MANAGE_THREADS`.
    Private,
    /// The current user's own archived private threads, by thread id. Needs
    /// only `READ_MESSAGE_HISTORY`.
    JoinedPrivate,
}

/// Where the next page of an archived listing starts. Public and private
/// pages run on archive timestamps, joined private ones on thread ids —
/// each route only accepts its own kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchivedCursor {
    Timestamp(Timestamp),
    Id(Id<ChannelMarker>),
}

/// A thread change from the gateway, already converted. The screen's
/// dispatch forwards these to the panel, which applies them to the open
/// parent's lists.
#[derive(Clone)]
pub enum ThreadEvent {
    /// A thread was created or changed. Optional fields the payload left out
    /// (counts, ids, membership) keep their previously known values; core
    /// fields and the archive state come from the payload.
    Upsert(ThreadSummary),
    /// A thread was deleted.
    Delete {
        guild_id: Id<GuildMarker>,
        thread_id: Id<ChannelMarker>,
        parent_id: Id<ChannelMarker>,
    },
    /// A `THREAD_LIST_SYNC`: the server's word on which threads are active.
    /// `parent_ids` empty means a guild-wide sync, which for user accounts
    /// only carries joined threads, so it merges rather than replaces; a
    /// channel-scoped sync is the full active set for those channels.
    Sync {
        guild_id: Id<GuildMarker>,
        parent_ids: Vec<Id<ChannelMarker>>,
        threads: Vec<ThreadSummary>,
    },
    /// A `THREAD_MEMBERS_UPDATE`. `joined` says whether the current user was
    /// added or removed; `None` when the event is about other people.
    Membership {
        guild_id: Id<GuildMarker>,
        thread_id: Id<ChannelMarker>,
        member_count: Option<i32>,
        joined: Option<bool>,
    },
}

/// Converts a forum tag as offered by a forum channel.
pub(in crate::discord) fn convert_forum_tag(tag: RawForumTag) -> ForumTag {
    ForumTag {
        id: tag.id,
        name: tag.name,
        moderated: tag.moderated,
        emoji_id: tag.emoji_id,
        emoji_name: tag.emoji_name,
    }
}

/// Converts a thread channel into its summary, or `None` for anything that
/// isn't a thread (or carries no thread metadata, which a real one always
/// does).
///
/// `joined` is the caller's word on the current user's membership: `Some`
/// for listing responses, which carry a member per joined thread, and `None`
/// when the channel object alone decides (its `member` field is the current
/// user's membership, present only on some endpoints).
pub(in crate::discord) fn convert_thread(
    channel: twilight_model::channel::Channel,
    joined: Option<bool>,
) -> Option<ThreadSummary> {
    let kind = match channel.kind {
        ChannelType::AnnouncementThread => ChannelKind::AnnouncementThread,
        ChannelType::PrivateThread => ChannelKind::PrivateThread,
        ChannelType::PublicThread => ChannelKind::PublicThread,
        _ => return None,
    };
    let metadata = channel.thread_metadata?;

    Some(ThreadSummary {
        id: channel.id,
        parent_id: channel.parent_id?,
        guild_id: channel.guild_id,
        name: channel.name.unwrap_or_default(),
        kind,
        archived: metadata.archived,
        locked: metadata.locked,
        archive_timestamp: metadata.archive_timestamp,
        message_count: channel.message_count,
        member_count: channel.member_count.map(i32::from),
        owner_id: channel.owner_id,
        applied_tags: channel.applied_tags.unwrap_or_default(),
        joined: joined.unwrap_or(channel.member.is_some()),
        last_message_id: channel.last_message_id.map(Id::cast),
        preview: None,
        pinned: channel
            .flags
            .is_some_and(|flags| flags.intersects(ChannelFlags::PINNED)),
    })
}

/// The threads the current user has joined, from a listing response's member
/// array, which carries one thread member per joined thread.
pub(in crate::discord) fn joined_thread_ids(
    members: &[ThreadMember],
) -> HashSet<Id<ChannelMarker>> {
    members
        .iter()
        .filter_map(|member| member.id)
        .collect::<HashSet<_>>()
}

/// Cuts a first message's text down to the preview a list row shows.
pub(in crate::discord) fn preview_text(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let end = trimmed
        .char_indices()
        .nth(PREVIEW_MAX_CHARS)
        .map_or(trimmed.len(), |(index, _)| index);
    let mut preview = trimmed[..end].trim_end().to_string();
    if end < trimmed.len() {
        preview.push('…');
    }
    Some(preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_channel() -> twilight_model::channel::Channel {
        serde_json::from_value(serde_json::json!({
            "id": "41771983423143937",
            "guild_id": "41771983423143937",
            "parent_id": "41771983423143930",
            "owner_id": "850034322064678913",
            "name": "don't buy dota-2",
            "type": 11,
            "last_message_id": "155117677105512449",
            "message_count": 1,
            "member_count": 5,
            "thread_metadata": {
                "archived": false,
                "auto_archive_duration": 1440,
                "archive_timestamp": "2021-04-12T23:40:39.855793+00:00",
                "locked": false
            },
            "applied_tags": []
        }))
        .unwrap()
    }

    #[test]
    fn thread_channels_convert_with_their_metadata() {
        let summary = convert_thread(thread_channel(), Some(true)).unwrap();
        assert_eq!(summary.kind, ChannelKind::PublicThread);
        assert_eq!(summary.name, "don't buy dota-2");
        assert!(!summary.archived);
        assert!(!summary.locked);
        assert_eq!(summary.member_count, Some(5));
        assert!(summary.joined);
        assert!(summary.last_message_id.is_some());
    }

    #[test]
    fn non_threads_and_metadataless_channels_do_not_convert() {
        assert_eq!(
            convert_thread(thread_channel(), None).map(|s| s.joined),
            Some(false)
        );

        let text = serde_json::from_value::<twilight_model::channel::Channel>(serde_json::json!({
            "id": "41771983423143930",
            "type": 0,
            "name": "general"
        }))
        .unwrap();
        assert!(convert_thread(text, Some(true)).is_none());

        let bare = serde_json::from_value::<twilight_model::channel::Channel>(serde_json::json!({
            "id": "41771983423143937",
            "type": 11,
            "name": "no metadata"
        }))
        .unwrap();
        assert!(convert_thread(bare, Some(true)).is_none());
    }

    #[test]
    fn previews_cut_on_char_boundaries() {
        assert_eq!(preview_text("  "), None);
        assert_eq!(preview_text(" hi "), Some("hi".to_string()));
        let long = "é".repeat(PREVIEW_MAX_CHARS + 10);
        let preview = preview_text(&long).unwrap();
        assert!(preview.ends_with('…'));
        // Each `é` is two bytes, so a naive byte cut would panic mid-char.
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
    }
}
