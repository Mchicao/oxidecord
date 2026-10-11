//! Guild channels and direct-message conversations.

use twilight_model::channel::ChannelType;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker, UserMarker},
};

use super::cdn;
use super::markdown::{self, Inline};
use super::user::small_avatar_url;

/// The subset of Discord channel types the app knows how to display.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Text,
    Announcement,
    Voice,
    Stage,
    Forum,
    Category,
}

impl ChannelKind {
    pub fn is_voice(self) -> bool {
        matches!(self, Self::Voice | Self::Stage)
    }
}

#[derive(Clone)]
pub struct Channel {
    pub id: Id<ChannelMarker>,
    pub name: String,
    pub kind: ChannelKind,
    pub parent_id: Option<Id<ChannelMarker>>,
    pub position: i32,
    /// Parsed for the header's one line, so its newlines are already run
    /// together.
    pub topic: Option<Vec<Inline>>,
    /// Whether the current user may post here, resolved from the channel's
    /// overwrites when the channel list is fetched.
    pub can_send: bool,
    /// Whether the current user holds `MANAGE_MESSAGES` here, which is what
    /// lets them delete other people's messages.
    pub can_manage_messages: bool,
    /// Whether the current user holds `USE_EXTERNAL_EMOJIS` here. Nitro is
    /// needed as well before another guild's emoji can be sent.
    pub can_use_external_emoji: bool,
    /// The newest message, which decides whether the channel is unread.
    pub last_message_id: Option<Id<MessageMarker>>,
}

/// A 1:1 or group direct-message conversation from the user's DM list.
#[derive(Clone)]
pub struct DirectMessage {
    pub id: Id<ChannelMarker>,
    pub name: String,
    pub avatar_url: Option<String>,
    /// The other user in a 1:1 conversation, whose status the row shows.
    /// `None` for a group DM.
    pub recipient_id: Option<Id<UserMarker>>,
    /// Snowflake of the last message, used only to order conversations by
    /// recency like Discord does. `None` (no messages yet) sorts last.
    last_message_id: Option<u64>,
}

impl DirectMessage {
    pub fn last_message_id(&self) -> Option<Id<MessageMarker>> {
        self.last_message_id.and_then(Id::new_checked)
    }

    /// Records a newly arrived message, so the conversation sorts as the most
    /// recently active.
    pub fn record_message(&mut self, message_id: u64) {
        self.last_message_id = self.last_message_id.max(Some(message_id));
    }
}

/// Converts a guild channel, dropping the types the UI can't display (threads,
/// directories, ...).
pub(in crate::discord) fn convert_channel(
    channel: twilight_model::channel::Channel,
    can_send: bool,
    can_manage_messages: bool,
    can_use_external_emoji: bool,
) -> Option<Channel> {
    let kind = match channel.kind {
        ChannelType::GuildText => ChannelKind::Text,
        ChannelType::GuildAnnouncement => ChannelKind::Announcement,
        ChannelType::GuildVoice => ChannelKind::Voice,
        ChannelType::GuildStageVoice => ChannelKind::Stage,
        ChannelType::GuildForum => ChannelKind::Forum,
        ChannelType::GuildCategory => ChannelKind::Category,
        _ => return None,
    };

    Some(Channel {
        id: channel.id,
        name: channel.name.unwrap_or_default(),
        kind,
        parent_id: channel.parent_id,
        position: channel.position.unwrap_or(0),
        topic: channel
            .topic
            .filter(|topic| !topic.trim().is_empty())
            .map(|topic| markdown::parse_preview(&topic)),
        can_send,
        can_manage_messages,
        can_use_external_emoji,
        last_message_id: channel.last_message_id.map(Id::cast),
    })
}

/// Converts the raw private-channel list into DM conversations, ordered
/// most-recently-active first.
pub(in crate::discord) fn convert_dms(
    channels: Vec<twilight_model::channel::Channel>,
) -> Vec<DirectMessage> {
    let mut dms: Vec<DirectMessage> = channels.into_iter().filter_map(convert_dm).collect();
    dms.sort_by_key(|dm| std::cmp::Reverse(dm.last_message_id));
    dms
}

fn convert_dm(channel: twilight_model::channel::Channel) -> Option<DirectMessage> {
    let last_message_id = channel.last_message_id.map(|id| id.get());
    match channel.kind {
        ChannelType::Private => {
            let user = channel.recipients?.into_iter().next()?;
            Some(DirectMessage {
                id: channel.id,
                avatar_url: small_avatar_url(&user),
                recipient_id: Some(user.id),
                name: user.global_name.unwrap_or(user.name),
                last_message_id,
            })
        }
        ChannelType::Group => Some(DirectMessage {
            id: channel.id,
            avatar_url: channel
                .icon
                .map(|hash| cdn::group_dm_icon_url(channel.id, &hash.to_string())),
            name: group_dm_name(channel.name, channel.recipients.as_deref()),
            recipient_id: None,
            last_message_id,
        }),
        _ => None,
    }
}

/// A group DM's own title, or the recipients' names joined the way Discord
/// labels an unnamed group.
fn group_dm_name(
    name: Option<String>,
    recipients: Option<&[twilight_model::user::User]>,
) -> String {
    name.filter(|name| !name.is_empty())
        .or_else(|| {
            let joined = recipients?
                .iter()
                .map(|user| {
                    user.global_name
                        .clone()
                        .unwrap_or_else(|| user.name.clone())
                })
                .collect::<Vec<_>>()
                .join(", ");
            (!joined.is_empty()).then_some(joined)
        })
        .unwrap_or_else(|| "Group DM".to_string())
}
