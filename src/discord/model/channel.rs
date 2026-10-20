//! Guild channels and direct-message conversations.

use twilight_model::channel::ChannelType;
use twilight_model::guild::Permissions;
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
    /// overwrites when the channel list is built.
    pub can_send: bool,
    /// Whether the current user holds `MANAGE_MESSAGES` here, which is what
    /// lets them delete other people's messages.
    pub can_manage_messages: bool,
    /// Whether the current user holds `USE_EXTERNAL_EMOJIS` here. Nitro is
    /// needed as well before another guild's emoji can be sent.
    pub can_use_external_emoji: bool,
    /// Whether the current user holds `MENTION_EVERYONE` here, without which
    /// `@everyone` and `@here` don't notify anyone and aren't offered.
    pub can_mention_everyone: bool,
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

/// Converts a guild channel fetched over REST, dropping the types the UI
/// can't display (threads, directories, ...). `permissions` are the user's in
/// it, overwrites applied.
pub(in crate::discord) fn convert_channel(
    channel: twilight_model::channel::Channel,
    permissions: Permissions,
) -> Option<Channel> {
    guild_channel(GuildChannelFields {
        id: channel.id,
        kind: channel.kind,
        name: channel.name,
        parent_id: channel.parent_id,
        position: channel.position,
        topic: channel.topic,
        last_message_id: channel.last_message_id.map(Id::cast),
        permissions,
    })
}

/// A guild channel's fields, wherever they were read from: a REST channel or
/// one inside a gateway guild.
pub(super) struct GuildChannelFields {
    pub id: Id<ChannelMarker>,
    pub kind: ChannelType,
    pub name: Option<String>,
    pub parent_id: Option<Id<ChannelMarker>>,
    pub position: Option<i32>,
    pub topic: Option<String>,
    pub last_message_id: Option<Id<MessageMarker>>,
    pub permissions: Permissions,
}

pub(super) fn guild_channel(fields: GuildChannelFields) -> Option<Channel> {
    let kind = match fields.kind {
        ChannelType::GuildText => ChannelKind::Text,
        ChannelType::GuildAnnouncement => ChannelKind::Announcement,
        ChannelType::GuildVoice => ChannelKind::Voice,
        ChannelType::GuildStageVoice => ChannelKind::Stage,
        ChannelType::GuildForum => ChannelKind::Forum,
        ChannelType::GuildCategory => ChannelKind::Category,
        _ => return None,
    };
    let permissions = fields.permissions;

    Some(Channel {
        id: fields.id,
        name: fields.name.unwrap_or_default(),
        kind,
        parent_id: fields.parent_id,
        position: fields.position.unwrap_or(0),
        topic: fields
            .topic
            .filter(|topic| !topic.trim().is_empty())
            .map(|topic| markdown::parse_preview(&topic)),
        can_send: permissions.contains(Permissions::SEND_MESSAGES),
        can_manage_messages: permissions.contains(Permissions::MANAGE_MESSAGES),
        can_use_external_emoji: permissions.contains(Permissions::USE_EXTERNAL_EMOJIS),
        can_mention_everyone: permissions.contains(Permissions::MENTION_EVERYONE),
        last_message_id: fields.last_message_id,
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
