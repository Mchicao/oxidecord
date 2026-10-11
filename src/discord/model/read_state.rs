//! Read states: how far the user has read each channel, how far each channel
//! has got, and which are muted — what the unread marks are worked out from.
//!
//! None of this is in twilight, since bots have no read state. Discord's own
//! client works the marks out itself from the same three things: a channel is
//! unread when its newest message is past the one the user last acked.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use twilight_model::channel::ChannelType;
use twilight_model::channel::permission_overwrite::PermissionOverwrite;
use twilight_model::guild::Permissions;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
};
use twilight_model::util::Timestamp;
use twilight_util::permission_calculator::PermissionCalculator;

use super::RawRole;

/// Milliseconds from the Unix epoch to Discord's, which snowflakes count from.
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// How far the user has read one channel.
pub struct ReadState {
    pub channel_id: Id<ChannelMarker>,
    pub last_message_id: Id<MessageMarker>,
}

/// The newest message in a channel.
pub struct ChannelLatest {
    pub channel_id: Id<ChannelMarker>,
    pub last_message_id: Id<MessageMarker>,
}

/// A guild's channels as far as its unread mark goes: the ones the user can
/// see that hold messages the app can show.
pub struct GuildUnreads {
    pub guild_id: Id<GuildMarker>,
    /// When the user joined, as a snowflake. A channel they've never opened
    /// has no read state, and Discord counts it read up to here.
    pub joined: Option<Id<MessageMarker>>,
    pub channels: Vec<UnreadChannel>,
}

pub struct UnreadChannel {
    pub id: Id<ChannelMarker>,
    /// The category, whose mute carries down to the channel.
    pub parent_id: Option<Id<ChannelMarker>>,
    pub last_message_id: Option<Id<MessageMarker>>,
}

/// What the user has muted in one guild, or across their DMs when `guild_id`
/// is `None`.
pub struct MuteSettings {
    pub guild_id: Option<Id<GuildMarker>>,
    pub muted: bool,
    pub muted_channels: Vec<Id<ChannelMarker>>,
}

/// One entry of `READY`'s `read_state`. Only channel read states (type 0) are
/// kept; the rest are guild events, the notification center, and the like,
/// whose `id` isn't a channel.
#[derive(Deserialize)]
pub(in crate::discord) struct RawReadState {
    id: Id<ChannelMarker>,
    #[serde(default)]
    read_state_type: u8,
    #[serde(default, deserialize_with = "snowflake")]
    last_message_id: Option<Id<MessageMarker>>,
}

/// A channel's newest message, as `CHANNEL_UNREAD_UPDATE` and the passive
/// updates send it.
#[derive(Deserialize)]
pub(in crate::discord) struct RawChannelUnread {
    id: Id<ChannelMarker>,
    #[serde(default, deserialize_with = "snowflake")]
    last_message_id: Option<Id<MessageMarker>>,
}

/// A channel in a gateway guild, of which only what decides its unread mark
/// is read.
#[derive(Deserialize)]
pub(in crate::discord) struct RawGuildChannel {
    id: Id<ChannelMarker>,
    #[serde(rename = "type")]
    kind: ChannelType,
    #[serde(default)]
    parent_id: Option<Id<ChannelMarker>>,
    #[serde(default, deserialize_with = "snowflake")]
    last_message_id: Option<Id<MessageMarker>>,
    #[serde(default)]
    permission_overwrites: Vec<PermissionOverwrite>,
}

/// One entry of `user_guild_settings`, or a `USER_GUILD_SETTINGS_UPDATE`.
#[derive(Deserialize)]
pub(in crate::discord) struct RawGuildSettings {
    #[serde(default)]
    guild_id: Option<Id<GuildMarker>>,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    mute_config: Option<RawMuteConfig>,
    #[serde(default, deserialize_with = "versioned_list")]
    channel_overrides: Vec<RawChannelOverride>,
}

#[derive(Deserialize)]
struct RawChannelOverride {
    channel_id: Id<ChannelMarker>,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    mute_config: Option<RawMuteConfig>,
}

#[derive(Deserialize)]
struct RawMuteConfig {
    #[serde(default)]
    end_time: Option<Timestamp>,
}

/// Reads an id Discord may send as `0` or `"0"` for "none", which `Id`
/// itself refuses.
fn snowflake<'de, D, T>(deserializer: D) -> Result<Option<Id<T>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::String(id)) => id.parse().ok().and_then(Id::new_checked),
            Some(serde_json::Value::Number(id)) => id.as_u64().and_then(Id::new_checked),
            _ => None,
        },
    )
}

/// Reads a list that may come as a plain array, as a versioned
/// `{entries, partial, version}` object, or keyed by id, depending on the
/// session's capabilities. Items that don't fit are dropped.
pub(in crate::discord) fn versioned_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let items = match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(mut object) => match object.remove("entries") {
            Some(serde_json::Value::Array(items)) => items,
            _ => object.into_iter().map(|(_, item)| item).collect(),
        },
        _ => Vec::new(),
    };
    Ok(items
        .into_iter()
        .filter_map(|item| serde_json::from_value(item).ok())
        .collect())
}

pub(in crate::discord) fn convert_read_states(states: Vec<RawReadState>) -> Vec<ReadState> {
    states
        .into_iter()
        .filter(|state| state.read_state_type == 0)
        .filter_map(|state| {
            Some(ReadState {
                channel_id: state.id,
                last_message_id: state.last_message_id?,
            })
        })
        .collect()
}

pub(in crate::discord) fn convert_channel_unreads(
    channels: Vec<RawChannelUnread>,
) -> Vec<ChannelLatest> {
    channels
        .into_iter()
        .filter_map(|channel| {
            Some(ChannelLatest {
                channel_id: channel.id,
                last_message_id: channel.last_message_id?,
            })
        })
        .collect()
}

/// The guild's channels that count toward its unread mark: text and
/// announcement channels the user can view. Voice channels' text chats and
/// forum posts are left out, since the app can't open them to read them.
///
/// Visibility is worked out the way the channel list's fetch does, from the
/// roles in the same payload. `member_roles` is `None` when the user's member
/// didn't come with it, which leaves only what `@everyone` can see.
pub(in crate::discord) fn convert_guild_unreads(
    guild_id: Id<GuildMarker>,
    user_id: Id<UserMarker>,
    owner_id: Option<Id<UserMarker>>,
    joined_at: Option<Timestamp>,
    roles: &[RawRole],
    member_roles: Option<&[Id<RoleMarker>]>,
    channels: Vec<RawGuildChannel>,
) -> GuildUnreads {
    let role_permissions = |id: Id<RoleMarker>| {
        roles
            .iter()
            .find(|role| role.id == id)
            .and_then(|role| role.permissions)
            .unwrap_or_else(Permissions::empty)
    };
    // `@everyone` shares the guild's id.
    let everyone = role_permissions(guild_id.cast());
    let member_roles: Vec<(Id<RoleMarker>, Permissions)> = member_roles
        .unwrap_or_default()
        .iter()
        .filter(|id| id.get() != guild_id.get())
        .map(|&id| (id, role_permissions(id)))
        .collect();

    let channels = channels
        .into_iter()
        .filter(|channel| {
            matches!(
                channel.kind,
                ChannelType::GuildText | ChannelType::GuildAnnouncement
            )
        })
        .filter(|channel| {
            let mut calculator =
                PermissionCalculator::new(guild_id, user_id, everyone, &member_roles);
            if owner_id == Some(user_id) {
                calculator = calculator.owner_id(user_id);
            }
            calculator
                .in_channel(channel.kind, &channel.permission_overwrites)
                .contains(Permissions::VIEW_CHANNEL)
        })
        .map(|channel| UnreadChannel {
            id: channel.id,
            parent_id: channel.parent_id,
            last_message_id: channel.last_message_id,
        })
        .collect();

    GuildUnreads {
        guild_id,
        joined: joined_at.and_then(timestamp_snowflake),
        channels,
    }
}

/// The lowest snowflake minted at `timestamp`.
fn timestamp_snowflake(timestamp: Timestamp) -> Option<Id<MessageMarker>> {
    let ms = timestamp.as_micros() / 1000 - DISCORD_EPOCH_MS;
    u64::try_from(ms)
        .ok()
        .and_then(|ms| Id::new_checked(ms << 22))
}

/// What's muted, with mutes whose time ran out already dropped. One that
/// runs out during the session stays in force until the settings next change.
pub(in crate::discord) fn convert_mute_settings(settings: RawGuildSettings) -> MuteSettings {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let in_force = |muted: bool, config: Option<&RawMuteConfig>| {
        muted
            && config
                .and_then(|config| config.end_time)
                .is_none_or(|end| end.as_secs() > now)
    };

    MuteSettings {
        guild_id: settings.guild_id,
        muted: in_force(settings.muted, settings.mute_config.as_ref()),
        muted_channels: settings
            .channel_overrides
            .iter()
            .filter(|channel| in_force(channel.muted, channel.mute_config.as_ref()))
            .map(|channel| channel.channel_id)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_states_come_plain_or_versioned_and_skip_other_kinds() {
        #[derive(Deserialize)]
        struct Ready {
            #[serde(deserialize_with = "versioned_list")]
            read_state: Vec<RawReadState>,
        }

        let plain: Ready = serde_json::from_str(
            r#"{"read_state": [
                {"id": "100", "last_message_id": "500"},
                {"id": "101", "last_message_id": 0},
                {"id": "1", "read_state_type": 2, "last_acked_id": "7"}
            ]}"#,
        )
        .unwrap();
        let states = convert_read_states(plain.read_state);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].last_message_id, Id::new(500));

        let versioned: Ready = serde_json::from_str(
            r#"{"read_state": {"entries": [{"id": "100", "last_message_id": "500"}],
                "partial": false, "version": 3}}"#,
        )
        .unwrap();
        assert_eq!(convert_read_states(versioned.read_state).len(), 1);
    }

    #[test]
    fn joining_converts_to_a_snowflake() {
        // 2015-01-01T00:00:01Z is one second past Discord's epoch.
        let joined = Timestamp::parse("2015-01-01T00:00:01.000000+00:00").unwrap();
        assert_eq!(timestamp_snowflake(joined), Id::new_checked(1000 << 22));
    }

    #[test]
    fn hidden_channels_are_left_out() {
        let roles: Vec<RawRole> =
            serde_json::from_str(r#"[{"id": "10", "name": "@everyone", "permissions": "1024"}]"#)
                .unwrap();
        let channels: Vec<RawGuildChannel> = serde_json::from_str(
            r#"[
                {"id": "100", "type": 0, "last_message_id": "5"},
                {"id": "101", "type": 0, "permission_overwrites": [
                    {"id": "10", "type": 0, "allow": "0", "deny": "1024"}
                ]},
                {"id": "102", "type": 2},
                {"id": "103", "type": 4}
            ]"#,
        )
        .unwrap();

        let unreads =
            convert_guild_unreads(Id::new(10), Id::new(1), None, None, &roles, None, channels);
        let ids: Vec<_> = unreads.channels.iter().map(|channel| channel.id).collect();
        assert_eq!(ids, [Id::new(100)]);
    }

    #[test]
    fn expired_mutes_are_dropped() {
        let settings: RawGuildSettings = serde_json::from_str(
            r#"{
                "guild_id": "10",
                "muted": true,
                "mute_config": {"end_time": "2000-01-01T00:00:00+00:00"},
                "channel_overrides": [
                    {"channel_id": "100", "muted": true, "mute_config": null},
                    {"channel_id": "101", "muted": false}
                ]
            }"#,
        )
        .unwrap();
        let mutes = convert_mute_settings(settings);
        assert!(!mutes.muted);
        assert_eq!(mutes.muted_channels, [Id::new(100)]);
    }
}
