//! The gateway websocket connection, which delivers live events.
//!
//! Dispatches are read as raw JSON rather than through twilight's `Event`
//! enum: the app needs a few user-client payloads twilight either models too
//! strictly (a DM call's `VOICE_SERVER_UPDATE` has no guild id) or doesn't
//! model at all. The shard still handles the session itself — heartbeats,
//! identify, resume — since that happens as messages are polled.

use futures::StreamExt as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use twilight_gateway::{Intents, Message as ShardMessage, MessageSender, Shard, ShardId};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
};

use crate::platform::runtime;

use twilight_model::util::Timestamp;

use super::model::{
    ChannelLatest, GuildEmoji, GuildUnreads, MemberListUpdate, Message, MuteSettings,
    PresenceStatus, RawChannelUnread, RawEmoji, RawGuildChannel, RawGuildSettings, RawMember,
    RawMemberListUpdate, RawReadState, RawRole, RawVoiceServer, RawVoiceState, ReactionEmoji,
    ReadState, Role, StreamServerInfo, VoiceServerInfo, VoiceUserState, convert_channel_unreads,
    convert_guild_emoji, convert_guild_unreads, convert_guild_voice_states,
    convert_member_list_update, convert_message, convert_mute_settings, convert_reaction_emoji,
    convert_read_states, convert_role, convert_stream_server, convert_voice_server,
    convert_voice_state, parse_user_settings, versioned_list,
};

/// A message received live over the gateway, tagged with the channel it
/// belongs to so the UI can decide whether it's for the open conversation.
pub struct IncomingMessage {
    pub channel_id: Id<ChannelMarker>,
    /// `None` for a message in a DM or group DM.
    pub guild_id: Option<Id<GuildMarker>>,
    pub message: Message,
    /// The nonce the sender attached, when this is a new message. Matches the
    /// id of the optimistic copy of a message sent from here.
    pub nonce: Option<u64>,
}

/// The live events the app acts on.
pub enum GatewayEvent {
    /// A new session is up. Carries the signed-in user, which voice
    /// connections need to identify themselves, and everyone in voice across
    /// its guilds — the whole of it, replacing whatever an earlier session
    /// knew, since anyone could have left while the app was disconnected.
    Ready {
        user_id: Id<UserMarker>,
        voice_states: Vec<VoiceUserState>,
    },
    Message(IncomingMessage),
    /// A message was edited. Carries the whole message as it now stands.
    MessageUpdate(IncomingMessage),
    /// One message was deleted, or several at once by a moderator.
    MessageDelete {
        channel_id: Id<ChannelMarker>,
        message_ids: Vec<Id<MessageMarker>>,
    },
    /// Someone started typing in a channel. Discord shows it for ten seconds,
    /// and clients re-send it every few while the typing goes on.
    Typing {
        channel_id: Id<ChannelMarker>,
        user_id: Id<UserMarker>,
        /// Their name in the guild. `None` in a DM, which sends no member.
        name: Option<String>,
    },
    /// A message's reactions changed.
    Reaction {
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        change: ReactionChange,
    },
    /// Someone joined, left, or changed their state in a voice channel.
    VoiceState(VoiceUserState),
    /// Everyone in voice in one guild, replacing what was known of it: from
    /// `GUILD_CREATE`, or empty from `GUILD_DELETE`.
    GuildVoiceStates {
        guild_id: Id<GuildMarker>,
        states: Vec<VoiceUserState>,
    },
    /// The voice server assigned to a call the user is joining.
    VoiceServer(VoiceServerInfo),
    /// A stream was created. For the user's own, it names the server the
    /// stream connection identifies to.
    StreamCreate {
        stream_key: String,
        rtc_server_id: String,
    },
    /// The voice server assigned to a stream.
    StreamServer(StreamServerInfo),
    /// A stream ended, or the user stopped watching it. `reason` is
    /// Discord's code for why — `stream_full` and `unauthorized` when a
    /// watch was refused.
    StreamDelete {
        stream_key: String,
        reason: Option<String>,
    },
    /// Every role in a guild, from `READY` or `GUILD_CREATE`. Replaces what
    /// was known before.
    GuildRoles {
        guild_id: Id<GuildMarker>,
        roles: Vec<Role>,
    },
    /// A role was created or changed.
    RoleUpdate {
        guild_id: Id<GuildMarker>,
        role: Role,
    },
    RoleDelete {
        guild_id: Id<GuildMarker>,
        role_id: Id<RoleMarker>,
    },
    /// Every custom emoji in a guild, from `READY`, `GUILD_CREATE`, or
    /// `GUILD_EMOJIS_UPDATE`. Replaces what was known before.
    GuildEmojis {
        guild_id: Id<GuildMarker>,
        emojis: Vec<GuildEmoji>,
    },
    /// A guild member's roles, from `READY`, `GUILD_CREATE`,
    /// `GUILD_MEMBER_UPDATE`, `GUILD_MEMBERS_CHUNK`, or the member a new
    /// message carries for its author. Not only the signed-in user's: other
    /// members' roles colour their names.
    MemberRoles {
        guild_id: Id<GuildMarker>,
        user_id: Id<UserMarker>,
        roles: Vec<Id<RoleMarker>>,
    },
    /// A change to a channel member list the session is subscribed to; see
    /// [`GatewaySender::subscribe_member_list`].
    MemberList(MemberListUpdate),
    /// Other users' statuses: friends' and guild members' from `READY` and
    /// `READY_SUPPLEMENTAL`, then each change from `PRESENCE_UPDATE`.
    Presences(Vec<(Id<UserMarker>, PresenceStatus)>),
    /// The user picked a status, here or on another of their clients.
    StatusSettings(PresenceStatus),
    /// How far the user has read every channel, from `READY`. Follows
    /// `Ready`, which clears what an earlier session knew.
    ReadStates(Vec<ReadState>),
    /// The user read a channel up to a message, here or on another client.
    /// `manual` when the ack went backwards: a message marked unread.
    MessageAck {
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        manual: bool,
    },
    /// A guild's channels that count toward its unread mark, from `READY` or
    /// `GUILD_CREATE`. Replaces what was known of the guild.
    GuildUnreads(GuildUnreads),
    /// Channels' newest messages: the DMs' from `READY`, and channels that
    /// moved in guilds the session isn't sent every message for.
    LatestMessages(Vec<ChannelLatest>),
    /// What the user muted: in every guild from `READY`, in one from
    /// `USER_GUILD_SETTINGS_UPDATE`.
    MuteSettings(Vec<MuteSettings>),
}

/// How a message's reactions changed.
pub enum ReactionChange {
    /// Someone reacted. Includes the signed-in user, whose own reactions also
    /// come back as dispatches.
    Add {
        user_id: Id<UserMarker>,
        emoji: ReactionEmoji,
    },
    Remove {
        user_id: Id<UserMarker>,
        emoji: ReactionEmoji,
    },
    /// Every reaction with one emoji was cleared by a moderator.
    RemoveEmoji(ReactionEmoji),
    /// Every reaction on the message was cleared by a moderator.
    RemoveAll,
}

/// Sends commands up the gateway from outside the receive loop.
///
/// Cloneable and cheap: commands are queued on a channel the shard drains, so
/// they survive a reconnect rather than failing while one is in flight.
#[derive(Clone)]
pub struct GatewaySender {
    inner: MessageSender,
}

impl GatewaySender {
    /// Sends `VOICE_STATE_UPDATE` (opcode 4): joins `channel_id`, or leaves
    /// the current channel when it's `None`.
    ///
    /// `guild_id` is `None` for a DM call, which Discord accepts as a null
    /// field — twilight's own command type can't express that, so the payload
    /// is built here.
    pub fn update_voice_state(
        &self,
        guild_id: Option<Id<GuildMarker>>,
        channel_id: Option<Id<ChannelMarker>>,
        self_mute: bool,
        self_deaf: bool,
    ) {
        let payload = serde_json::json!({
            "op": 4,
            "d": {
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": self_mute,
                "self_deaf": self_deaf,
                "self_video": false,
            }
        });
        let _ = self.inner.send(payload.to_string());
    }

    /// Sends `PRESENCE_UPDATE` (opcode 3): shows the user as `status` for the
    /// rest of the session. Activities are left empty — the app reports none.
    pub fn update_presence(&self, status: PresenceStatus) {
        let payload = serde_json::json!({
            "op": 3,
            "d": {
                "status": status.as_str(),
                "since": 0,
                "activities": [],
                "afk": false,
            }
        });
        let _ = self.inner.send(payload.to_string());
    }

    /// Sends `STREAM_CREATE` (opcode 18): goes live in the call in
    /// `channel_id`. Discord answers with the stream's server, as dispatches.
    pub fn create_stream(&self, guild_id: Option<Id<GuildMarker>>, channel_id: Id<ChannelMarker>) {
        let payload = serde_json::json!({
            "op": 18,
            "d": {
                "type": if guild_id.is_some() { "guild" } else { "call" },
                "guild_id": guild_id,
                "channel_id": channel_id,
                "preferred_region": null,
            }
        });
        let _ = self.inner.send(payload.to_string());
    }

    /// Sends `STREAM_SET_PAUSED` (opcode 22). A new stream starts paused
    /// until told otherwise.
    pub fn set_stream_paused(&self, stream_key: &str, paused: bool) {
        let payload = serde_json::json!({
            "op": 22,
            "d": { "stream_key": stream_key, "paused": paused }
        });
        let _ = self.inner.send(payload.to_string());
    }

    /// Sends `STREAM_WATCH` (opcode 20): starts watching someone's stream in
    /// the call the user is in. Discord answers with the stream's server, as
    /// dispatches, or with a `STREAM_DELETE` saying why not.
    pub fn watch_stream(&self, stream_key: &str) {
        let payload = serde_json::json!({
            "op": 20,
            "d": { "stream_key": stream_key }
        });
        let _ = self.inner.send(payload.to_string());
    }

    /// Sends `REQUEST_GUILD_MEMBERS` (opcode 8) for specific users, whose
    /// members come back as `GUILD_MEMBERS_CHUNK` dispatches. A user session
    /// is only sent a sliver of each guild's members up front, and fetched
    /// history carries none, so this is how a message's author gets a role
    /// colour. Discord takes at most 100 ids a request.
    pub fn request_members(&self, guild_id: Id<GuildMarker>, user_ids: &[Id<UserMarker>]) {
        for user_ids in user_ids.chunks(100) {
            let payload = serde_json::json!({
                "op": 8,
                "d": {
                    "guild_id": [guild_id],
                    "user_ids": user_ids,
                    "presences": false,
                }
            });
            let _ = self.inner.send(payload.to_string());
        }
    }

    /// Sends `GUILD_SUBSCRIPTIONS_BULK` (opcode 37): subscribes to the
    /// member list of `channel_id`, in the index ranges given (inclusive,
    /// a hundred apiece, as Discord's client asks). Replaces the guild's
    /// earlier channel subscription. Discord answers with a
    /// `GUILD_MEMBER_LIST_UPDATE` syncing each range, then keeps them current.
    ///
    /// Undocumented; the other flags are what Discord's own client sends.
    pub fn subscribe_member_list(
        &self,
        guild_id: Id<GuildMarker>,
        channel_id: Id<ChannelMarker>,
        ranges: &[[usize; 2]],
    ) {
        let payload = serde_json::json!({
            "op": 37,
            "d": {
                "subscriptions": {
                    guild_id.to_string(): {
                        "typing": true,
                        "activities": true,
                        "threads": true,
                        "channels": { channel_id.to_string(): ranges },
                    }
                }
            }
        });
        let _ = self.inner.send(payload.to_string());
    }

    /// Sends `STREAM_DELETE` (opcode 19): stops streaming, or stops watching
    /// a stream that isn't the user's own.
    pub fn delete_stream(&self, stream_key: &str) {
        let payload = serde_json::json!({
            "op": 19,
            "d": { "stream_key": stream_key }
        });
        let _ = self.inner.send(payload.to_string());
    }
}

/// `STREAM_CREATE`, of which only what a connection needs is read.
#[derive(Deserialize)]
struct StreamCreatePayload {
    stream_key: String,
    rtc_server_id: String,
}

/// `STREAM_DELETE`.
#[derive(Deserialize)]
struct StreamDeletePayload {
    stream_key: String,
    #[serde(default)]
    reason: Option<String>,
}

/// `USER_SETTINGS_PROTO_UPDATE`, of which only the `PreloadedUserSettings`
/// kind (type 1) is read. A partial update carries just the fields that
/// changed, so a missing status means it didn't change, not that it's gone.
#[derive(Deserialize)]
struct SettingsProtoUpdatePayload {
    settings: SettingsProtoUpdate,
}

#[derive(Deserialize)]
struct SettingsProtoUpdate {
    #[serde(rename = "type")]
    kind: u8,
    proto: String,
}

/// The envelope every gateway payload arrives in. Only dispatches (opcode 0)
/// carry a name and data the app cares about. Both borrow from the frame, so
/// telling what a payload is costs no allocations — which matters, because
/// most of a user account's firehose is dropped unread.
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(default, borrow)]
    t: Option<&'a str>,
    #[serde(default, borrow)]
    d: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ReadyPayload {
    user: ReadyUser,
    /// A user session's guilds arrive whole in `READY`; a bot's only as ids,
    /// with the rest following in `GUILD_CREATE`.
    #[serde(default)]
    guilds: Vec<GatewayGuild>,
    /// The signed-in user's member in each guild, in the same order as
    /// `guilds`, when the session asked for deduplicated payloads. Otherwise
    /// they arrive in each guild's own `members`.
    #[serde(default)]
    merged_members: Vec<Vec<RawMember>>,
    /// A plain list, or versioned when the session asked for it.
    #[serde(default, deserialize_with = "versioned_list")]
    read_state: Vec<RawReadState>,
    #[serde(default, deserialize_with = "versioned_list")]
    user_guild_settings: Vec<RawGuildSettings>,
    /// DMs and group DMs, of which only how far each has got is read here —
    /// the DM list fetches the rest when it's opened.
    #[serde(default, deserialize_with = "skip_invalid")]
    private_channels: Vec<RawChannelUnread>,
    /// Friends' presences, unless the session asked for them merged.
    #[serde(default, deserialize_with = "skip_invalid")]
    presences: Vec<RawPresence>,
    #[serde(default)]
    merged_presences: Option<MergedPresences>,
}

/// Presences gathered in one place, from `READY` or `READY_SUPPLEMENTAL`
/// depending on what the session asked for. `guilds` is in the same order as
/// `READY`'s guilds, which doesn't matter here: a status is the same in all.
#[derive(Deserialize)]
struct MergedPresences {
    #[serde(default, deserialize_with = "skip_invalid")]
    friends: Vec<RawPresence>,
    #[serde(default)]
    guilds: Vec<Vec<Box<RawValue>>>,
}

impl MergedPresences {
    fn into_presences(self) -> impl Iterator<Item = RawPresence> {
        let guilds = self.guilds.into_iter().flatten();
        self.friends
            .into_iter()
            .chain(guilds.filter_map(|presence| serde_json::from_str(presence.get()).ok()))
    }
}

/// A presence, of which only whose it is and their status are read. A
/// `PRESENCE_UPDATE` names the user in a partial `user`; a merged presence by
/// `user_id`.
#[derive(Deserialize)]
struct RawPresence {
    #[serde(default)]
    user: Option<ReadyUser>,
    #[serde(default)]
    user_id: Option<Id<UserMarker>>,
    status: String,
}

/// The presences that name a user and a status the app knows.
fn convert_presences(
    presences: impl IntoIterator<Item = RawPresence>,
) -> Vec<(Id<UserMarker>, PresenceStatus)> {
    presences
        .into_iter()
        .filter_map(|presence| {
            let user_id = presence.user_id.or(presence.user.map(|user| user.id))?;
            Some((user_id, PresenceStatus::from_presence(&presence.status)?))
        })
        .collect()
}

/// `READY_SUPPLEMENTAL`, of which only the presences are read.
#[derive(Deserialize)]
struct ReadySupplementalPayload {
    #[serde(default)]
    merged_presences: Option<MergedPresences>,
}

/// A guild as `READY` and `GUILD_CREATE` send it, of which only what's needed
/// is read: who is already sitting in its voice channels, what a role mention
/// should be called, and which roles the user holds. An unavailable guild
/// arrives with none of these.
#[derive(Deserialize)]
struct GatewayGuild {
    id: Id<GuildMarker>,
    /// Lenient because it sits inside `READY`: one state Discord shapes
    /// unexpectedly shouldn't cost the whole session its roles.
    #[serde(default, deserialize_with = "skip_invalid")]
    voice_states: Vec<RawVoiceState>,
    #[serde(default)]
    roles: Vec<RawRole>,
    #[serde(default)]
    members: Vec<RawMember>,
    /// `None` for an unavailable guild, which says nothing about its emoji,
    /// as opposed to a guild that has none.
    #[serde(default, deserialize_with = "skip_invalid_opt")]
    emojis: Option<Vec<RawEmoji>>,
    #[serde(default, deserialize_with = "skip_invalid")]
    channels: Vec<RawGuildChannel>,
    #[serde(default, deserialize_with = "skip_invalid")]
    presences: Vec<RawPresence>,
    #[serde(default)]
    joined_at: Option<Timestamp>,
    #[serde(default)]
    owner_id: Option<Id<UserMarker>>,
    /// Where the guild's own fields sit when the session asked for them
    /// apart from its lists.
    #[serde(default)]
    properties: Option<GuildProperties>,
}

#[derive(Deserialize)]
struct GuildProperties {
    #[serde(default)]
    owner_id: Option<Id<UserMarker>>,
}

impl GatewayGuild {
    fn owner_id(&self) -> Option<Id<UserMarker>> {
        self.owner_id.or_else(|| self.properties.as_ref()?.owner_id)
    }
}

/// `MESSAGE_ACK`.
#[derive(Deserialize)]
struct MessageAckPayload {
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    #[serde(default)]
    manual: bool,
}

/// `CHANNEL_UNREAD_UPDATE`, `PASSIVE_UPDATE_V1`, and `PASSIVE_UPDATE_V2`,
/// which each carry channels' newest messages under a name of their own.
#[derive(Deserialize)]
struct ChannelUnreadsPayload {
    #[serde(
        default,
        alias = "channels",
        alias = "updated_channels",
        deserialize_with = "skip_invalid"
    )]
    channel_unread_updates: Vec<RawChannelUnread>,
}

/// `GUILD_EMOJIS_UPDATE`: the guild's whole emoji list as it now stands.
#[derive(Deserialize)]
struct EmojisUpdatePayload {
    guild_id: Id<GuildMarker>,
    #[serde(deserialize_with = "skip_invalid")]
    emojis: Vec<RawEmoji>,
}

/// `GUILD_DELETE`: the user left the guild, or it went unavailable.
#[derive(Deserialize)]
struct GuildDeletePayload {
    id: Id<GuildMarker>,
}

/// `GUILD_MEMBER_UPDATE`.
#[derive(Deserialize)]
struct MemberUpdatePayload {
    guild_id: Id<GuildMarker>,
    #[serde(flatten)]
    member: RawMember,
}

/// `GUILD_MEMBERS_CHUNK`, the answer to [`GatewaySender::request_members`].
#[derive(Deserialize)]
struct MembersChunkPayload {
    guild_id: Id<GuildMarker>,
    #[serde(default, deserialize_with = "skip_invalid")]
    members: Vec<RawMember>,
}

/// `GUILD_ROLE_CREATE` and `GUILD_ROLE_UPDATE`.
#[derive(Deserialize)]
struct RoleUpdatePayload {
    guild_id: Id<GuildMarker>,
    role: RawRole,
}

#[derive(Deserialize)]
struct RoleDeletePayload {
    guild_id: Id<GuildMarker>,
    role_id: Id<RoleMarker>,
}

/// The `nonce` on a `MESSAGE_CREATE`. Discord echoes it as whatever the sender
/// used, a string or a number, and other clients' nonces needn't be numeric.
#[derive(Deserialize)]
struct NoncePayload {
    #[serde(default, deserialize_with = "deserialize_nonce")]
    nonce: Option<u64>,
}

fn deserialize_nonce<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::String(nonce)) => nonce.parse().ok(),
            Some(serde_json::Value::Number(nonce)) => nonce.as_u64(),
            _ => None,
        },
    )
}

/// `MESSAGE_DELETE`.
#[derive(Deserialize)]
struct MessageDeletePayload {
    id: Id<MessageMarker>,
    channel_id: Id<ChannelMarker>,
}

/// `MESSAGE_DELETE_BULK`.
#[derive(Deserialize)]
struct MessageDeleteBulkPayload {
    ids: Vec<Id<MessageMarker>>,
    channel_id: Id<ChannelMarker>,
}

/// `MESSAGE_REACTION_ADD`, `MESSAGE_REACTION_REMOVE`, and the two clears,
/// which share a shape: the clears just leave out who reacted, and clearing
/// all of a message's reactions leaves out the emoji too.
#[derive(Deserialize)]
struct ReactionPayload {
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    #[serde(default)]
    user_id: Option<Id<UserMarker>>,
    #[serde(default)]
    emoji: Option<twilight_model::channel::message::EmojiReactionType>,
}

/// `TYPING_START`.
#[derive(Deserialize)]
struct TypingStartPayload {
    channel_id: Id<ChannelMarker>,
    user_id: Id<UserMarker>,
    #[serde(default)]
    member: Option<RawMember>,
}

#[derive(Deserialize)]
struct ReadyUser {
    id: Id<UserMarker>,
}

/// Deserializes a list item by item, dropping the items that don't fit `T`
/// instead of failing the whole payload. The items are borrowed from the frame
/// first, so the skipping costs no copies.
fn skip_invalid<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let items = Vec::<&'de RawValue>::deserialize(deserializer)?;
    Ok(items
        .into_iter()
        .filter_map(|item| serde_json::from_str(item.get()).ok())
        .collect())
}

fn skip_invalid_opt<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    skip_invalid(deserializer).map(Some)
}

/// Opens a gateway websocket connection and invokes `on_event` for every
/// dispatch the app acts on. Returning `false` from it (the receiving end went
/// away) ends the shard loop and drops the socket.
///
/// The shard reconnects and resumes on its own, so transient errors are
/// skipped rather than treated as fatal.
pub fn connect_gateway(
    token: String,
    mut on_event: impl FnMut(GatewayEvent) -> bool + Send + 'static,
) -> GatewaySender {
    // The shard is built here rather than inside the task because the sender
    // has to come back to the caller — but building one starts the identify
    // queue's timer, so it has to happen inside the runtime all the same.
    let guard = runtime::handle().enter();
    // Discord ignores the intents field for user tokens; a real user client
    // receives every event its account can see. Request all intents so we
    // mirror that and never filter events at this layer (the shard still
    // requires a value in the IDENTIFY payload).
    let mut shard = Shard::new(ShardId::ONE, token, Intents::all());
    let sender = GatewaySender {
        inner: shard.sender(),
    };
    drop(guard);

    runtime::handle().spawn(async move {
        let mut self_id = None;
        while let Some(item) = shard.next().await {
            // Reconnects and resumes are handled by the shard internally; a
            // receive error just means skip this one and keep listening.
            let Ok(ShardMessage::Text(json)) = item else {
                continue;
            };
            let Ok(Envelope {
                t: Some(name),
                d: Some(data),
            }) = serde_json::from_str::<Envelope>(&json)
            else {
                continue;
            };

            // A dispatch the app doesn't handle, or one whose shape doesn't
            // match, yields nothing rather than ending the loop.
            for event in dispatch(name, data, &mut self_id) {
                if !on_event(event) {
                    return;
                }
            }
        }
    });

    sender
}

/// Turns one dispatch into the events the app acts on. `READY` and
/// `GUILD_CREATE` fan out into several.
///
/// `self_id` is the signed-in user, remembered from `READY` for the
/// dispatches that need it and don't say.
fn dispatch(
    name: &str,
    data: &RawValue,
    self_id: &mut Option<Id<UserMarker>>,
) -> Vec<GatewayEvent> {
    let data = data.get();
    match name {
        "READY" => serde_json::from_str::<ReadyPayload>(data)
            .map(|ready| {
                *self_id = Some(ready.user.id);
                let mut voice_states = Vec::new();
                let mut roles = Vec::new();
                let mut unreads = vec![
                    GatewayEvent::ReadStates(convert_read_states(ready.read_state)),
                    GatewayEvent::MuteSettings(
                        ready
                            .user_guild_settings
                            .into_iter()
                            .map(convert_mute_settings)
                            .collect(),
                    ),
                    GatewayEvent::LatestMessages(convert_channel_unreads(ready.private_channels)),
                ];
                let mut presences = ready.presences;
                presences.extend(
                    ready
                        .merged_presences
                        .into_iter()
                        .flat_map(MergedPresences::into_presences),
                );
                let mut merged = ready.merged_members.into_iter();
                for mut guild in ready.guilds {
                    presences.append(&mut guild.presences);
                    let merged = merged.next().unwrap_or_default();
                    let owner_id = guild.owner_id();
                    unreads.extend(guild_unreads(
                        guild.id,
                        ready.user.id,
                        owner_id,
                        guild.joined_at,
                        &guild.roles,
                        guild.members.iter().chain(&merged),
                        guild.channels,
                    ));
                    voice_states.extend(convert_guild_voice_states(
                        guild.id,
                        guild.voice_states,
                        guild.members.iter().chain(&merged),
                    ));
                    roles.extend(member_roles(guild.id, merged));
                    roles.extend(guild_emojis(guild.id, guild.emojis));
                    roles.extend(guild_roles(guild.id, guild.roles, guild.members));
                }
                // `Ready` goes first: the member roles that follow are only
                // kept for the signed-in user, whom it names.
                let ready = GatewayEvent::Ready {
                    user_id: ready.user.id,
                    voice_states,
                };
                let presences = GatewayEvent::Presences(convert_presences(presences));
                std::iter::once(ready)
                    .chain(unreads)
                    .chain(std::iter::once(presences))
                    .chain(roles)
                    .collect()
            })
            .unwrap_or_default(),
        "MESSAGE_CREATE" => serde_json::from_str::<twilight_model::channel::Message>(data)
            .map(|message| {
                // twilight's message model has no nonce, so it's read apart.
                let nonce = serde_json::from_str::<NoncePayload>(data)
                    .ok()
                    .and_then(|payload| payload.nonce);
                let roles = author_roles(&message);
                std::iter::once(GatewayEvent::Message(IncomingMessage {
                    channel_id: message.channel_id,
                    guild_id: message.guild_id,
                    message: convert_message(message),
                    nonce,
                }))
                .chain(roles)
                .collect()
            })
            .unwrap_or_default(),
        // twilight models this dispatch as a partial message, because Discord
        // hasn't always sent the whole thing. Parsing it as a full one takes
        // the updates that do and skips any that don't.
        "MESSAGE_UPDATE" => serde_json::from_str::<twilight_model::channel::Message>(data)
            .map(|message| {
                let roles = author_roles(&message);
                std::iter::once(GatewayEvent::MessageUpdate(IncomingMessage {
                    channel_id: message.channel_id,
                    guild_id: message.guild_id,
                    message: convert_message(message),
                    nonce: None,
                }))
                .chain(roles)
                .collect()
            })
            .unwrap_or_default(),
        "MESSAGE_DELETE" => serde_json::from_str::<MessageDeletePayload>(data)
            .map(|delete| {
                vec![GatewayEvent::MessageDelete {
                    channel_id: delete.channel_id,
                    message_ids: vec![delete.id],
                }]
            })
            .unwrap_or_default(),
        "MESSAGE_DELETE_BULK" => serde_json::from_str::<MessageDeleteBulkPayload>(data)
            .map(|delete| {
                vec![GatewayEvent::MessageDelete {
                    channel_id: delete.channel_id,
                    message_ids: delete.ids,
                }]
            })
            .unwrap_or_default(),
        "MESSAGE_REACTION_ADD"
        | "MESSAGE_REACTION_REMOVE"
        | "MESSAGE_REACTION_REMOVE_EMOJI"
        | "MESSAGE_REACTION_REMOVE_ALL" => serde_json::from_str::<ReactionPayload>(data)
            .ok()
            .and_then(|payload| {
                let emoji = payload.emoji.map(convert_reaction_emoji);
                let change = match name {
                    "MESSAGE_REACTION_ADD" => ReactionChange::Add {
                        user_id: payload.user_id?,
                        emoji: emoji?,
                    },
                    "MESSAGE_REACTION_REMOVE" => ReactionChange::Remove {
                        user_id: payload.user_id?,
                        emoji: emoji?,
                    },
                    "MESSAGE_REACTION_REMOVE_EMOJI" => ReactionChange::RemoveEmoji(emoji?),
                    _ => ReactionChange::RemoveAll,
                };
                Some(vec![GatewayEvent::Reaction {
                    channel_id: payload.channel_id,
                    message_id: payload.message_id,
                    change,
                }])
            })
            .unwrap_or_default(),
        "TYPING_START" => serde_json::from_str::<TypingStartPayload>(data)
            .map(|typing| {
                vec![GatewayEvent::Typing {
                    channel_id: typing.channel_id,
                    user_id: typing.user_id,
                    name: typing.member.and_then(|member| member.display_name()),
                }]
            })
            .unwrap_or_default(),
        "VOICE_STATE_UPDATE" => serde_json::from_str::<RawVoiceState>(data)
            .map(|state| vec![GatewayEvent::VoiceState(convert_voice_state(state))])
            .unwrap_or_default(),
        "VOICE_SERVER_UPDATE" => serde_json::from_str::<RawVoiceServer>(data)
            .map(|server| vec![GatewayEvent::VoiceServer(convert_voice_server(server))])
            .unwrap_or_default(),
        "STREAM_CREATE" => serde_json::from_str::<StreamCreatePayload>(data)
            .map(|stream| {
                vec![GatewayEvent::StreamCreate {
                    stream_key: stream.stream_key,
                    rtc_server_id: stream.rtc_server_id,
                }]
            })
            .unwrap_or_default(),
        "STREAM_SERVER_UPDATE" => serde_json::from_str::<StreamServerInfo>(data)
            .map(|server| vec![GatewayEvent::StreamServer(convert_stream_server(server))])
            .unwrap_or_default(),
        "STREAM_DELETE" => serde_json::from_str::<StreamDeletePayload>(data)
            .map(|stream| {
                vec![GatewayEvent::StreamDelete {
                    stream_key: stream.stream_key,
                    reason: stream.reason,
                }]
            })
            .unwrap_or_default(),
        "GUILD_CREATE" => serde_json::from_str::<GatewayGuild>(data)
            .map(|guild| {
                let owner_id = guild.owner_id();
                let unreads = self_id.and_then(|self_id| {
                    guild_unreads(
                        guild.id,
                        self_id,
                        owner_id,
                        guild.joined_at,
                        &guild.roles,
                        &guild.members,
                        guild.channels,
                    )
                });
                let voice = GatewayEvent::GuildVoiceStates {
                    guild_id: guild.id,
                    states: convert_guild_voice_states(
                        guild.id,
                        guild.voice_states,
                        &guild.members,
                    ),
                };
                let presences = GatewayEvent::Presences(convert_presences(guild.presences));
                std::iter::once(voice)
                    .chain(std::iter::once(presences))
                    .chain(unreads)
                    .chain(guild_emojis(guild.id, guild.emojis))
                    .chain(guild_roles(guild.id, guild.roles, guild.members))
                    .collect()
            })
            .unwrap_or_default(),
        "GUILD_DELETE" => serde_json::from_str::<GuildDeletePayload>(data)
            .map(|guild| {
                vec![GatewayEvent::GuildVoiceStates {
                    guild_id: guild.id,
                    states: Vec::new(),
                }]
            })
            .unwrap_or_default(),
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
            serde_json::from_str::<RoleUpdatePayload>(data)
                .map(|update| {
                    vec![GatewayEvent::RoleUpdate {
                        guild_id: update.guild_id,
                        role: convert_role(update.role),
                    }]
                })
                .unwrap_or_default()
        }
        "GUILD_ROLE_DELETE" => serde_json::from_str::<RoleDeletePayload>(data)
            .map(|delete| {
                vec![GatewayEvent::RoleDelete {
                    guild_id: delete.guild_id,
                    role_id: delete.role_id,
                }]
            })
            .unwrap_or_default(),
        "GUILD_EMOJIS_UPDATE" => serde_json::from_str::<EmojisUpdatePayload>(data)
            .map(|update| {
                guild_emojis(update.guild_id, Some(update.emojis))
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default(),
        "GUILD_MEMBER_UPDATE" => serde_json::from_str::<MemberUpdatePayload>(data)
            .map(|update| member_roles(update.guild_id, vec![update.member]).collect())
            .unwrap_or_default(),
        "GUILD_MEMBERS_CHUNK" => serde_json::from_str::<MembersChunkPayload>(data)
            .map(|chunk| member_roles(chunk.guild_id, chunk.members).collect())
            .unwrap_or_default(),
        "GUILD_MEMBER_LIST_UPDATE" => serde_json::from_str::<RawMemberListUpdate>(data)
            .map(|update| vec![GatewayEvent::MemberList(convert_member_list_update(update))])
            .unwrap_or_default(),
        "READY_SUPPLEMENTAL" => serde_json::from_str::<ReadySupplementalPayload>(data)
            .ok()
            .and_then(|ready| ready.merged_presences)
            .map(|merged| {
                vec![GatewayEvent::Presences(convert_presences(
                    merged.into_presences(),
                ))]
            })
            .unwrap_or_default(),
        "PRESENCE_UPDATE" => serde_json::from_str::<RawPresence>(data)
            .map(|presence| vec![GatewayEvent::Presences(convert_presences([presence]))])
            .unwrap_or_default(),
        "USER_SETTINGS_PROTO_UPDATE" => serde_json::from_str::<SettingsProtoUpdatePayload>(data)
            .ok()
            .filter(|update| update.settings.kind == 1)
            .and_then(|update| parse_user_settings(&update.settings.proto).ok()?.status)
            .map(|status| vec![GatewayEvent::StatusSettings(status)])
            .unwrap_or_default(),
        "MESSAGE_ACK" => serde_json::from_str::<MessageAckPayload>(data)
            .map(|ack| {
                vec![GatewayEvent::MessageAck {
                    channel_id: ack.channel_id,
                    message_id: ack.message_id,
                    manual: ack.manual,
                }]
            })
            .unwrap_or_default(),
        "CHANNEL_UNREAD_UPDATE" | "PASSIVE_UPDATE_V1" | "PASSIVE_UPDATE_V2" => {
            serde_json::from_str::<ChannelUnreadsPayload>(data)
                .map(|update| convert_channel_unreads(update.channel_unread_updates))
                .ok()
                .filter(|channels| !channels.is_empty())
                .map(|channels| vec![GatewayEvent::LatestMessages(channels)])
                .unwrap_or_default()
        }
        "USER_GUILD_SETTINGS_UPDATE" => serde_json::from_str::<RawGuildSettings>(data)
            .map(|settings| {
                vec![GatewayEvent::MuteSettings(vec![convert_mute_settings(
                    settings,
                )])]
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A guild's channels as an unread event, unless it arrived without any —
/// an unavailable guild, which says nothing about them. `members` is
/// searched for the signed-in user's own, whose roles decide what they see.
fn guild_unreads<'a>(
    guild_id: Id<GuildMarker>,
    self_id: Id<UserMarker>,
    owner_id: Option<Id<UserMarker>>,
    joined_at: Option<Timestamp>,
    roles: &[RawRole],
    members: impl IntoIterator<Item = &'a RawMember>,
    channels: Vec<RawGuildChannel>,
) -> Option<GatewayEvent> {
    if channels.is_empty() {
        return None;
    }
    let member_roles = members
        .into_iter()
        .find(|member| member.user_id() == Some(self_id))
        .map(|member| member.roles.as_slice());
    Some(GatewayEvent::GuildUnreads(convert_guild_unreads(
        guild_id,
        self_id,
        owner_id,
        joined_at,
        roles,
        member_roles,
        channels,
    )))
}

/// A guild's emoji as an event, unless the guild arrived without its list.
fn guild_emojis(guild_id: Id<GuildMarker>, emojis: Option<Vec<RawEmoji>>) -> Option<GatewayEvent> {
    Some(GatewayEvent::GuildEmojis {
        guild_id,
        emojis: emojis?.into_iter().map(convert_guild_emoji).collect(),
    })
}

/// Each member's roles as an event, skipping any that arrived without a user.
fn member_roles(
    guild_id: Id<GuildMarker>,
    members: Vec<RawMember>,
) -> impl Iterator<Item = GatewayEvent> {
    members.into_iter().filter_map(move |member| {
        let user_id = member.user_id()?;
        Some(GatewayEvent::MemberRoles {
            guild_id,
            user_id,
            roles: member.roles,
        })
    })
}

/// The roles of a guild message's author, from the member Discord sends along
/// with it. Webhooks aren't members, so their messages have none.
fn author_roles(message: &twilight_model::channel::Message) -> Option<GatewayEvent> {
    Some(GatewayEvent::MemberRoles {
        guild_id: message.guild_id?,
        user_id: message.author.id,
        roles: message.member.as_ref()?.roles.clone(),
    })
}

/// A guild's roles and its members' as events. An unavailable guild in
/// `READY` arrives with neither, and yields nothing.
fn guild_roles(
    guild_id: Id<GuildMarker>,
    roles: Vec<RawRole>,
    members: Vec<RawMember>,
) -> impl Iterator<Item = GatewayEvent> {
    let roles_event = (!roles.is_empty()).then(|| GatewayEvent::GuildRoles {
        guild_id,
        roles: roles.into_iter().map(convert_role).collect(),
    });
    roles_event
        .into_iter()
        .chain(member_roles(guild_id, members))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::{MemberGroupKind, MemberListGroup, MemberListItem, MemberListOp};

    fn dispatch_json(name: &str, json: &str) -> Vec<GatewayEvent> {
        dispatch(
            name,
            &RawValue::from_string(json.to_owned()).unwrap(),
            &mut Some(Id::new(1)),
        )
    }

    fn ready_voice_states(events: &[GatewayEvent]) -> &[VoiceUserState] {
        match events.first() {
            Some(GatewayEvent::Ready { voice_states, .. }) => voice_states,
            _ => panic!("READY should lead with a Ready event"),
        }
    }

    #[test]
    fn ready_names_voice_states_from_guild_members() {
        let events = dispatch_json(
            "READY",
            r#"{
                "user": {"id": "1"},
                "guilds": [{
                    "id": "10",
                    "voice_states": [
                        {"user_id": "2", "channel_id": "100", "self_mute": true},
                        {"user_id": "3", "channel_id": "100"}
                    ],
                    "members": [{
                        "nick": "Nick",
                        "user": {"id": "2", "username": "user2", "global_name": "Global"}
                    }]
                }]
            }"#,
        );

        let states = ready_voice_states(&events);
        assert_eq!(states.len(), 2);
        assert!(
            states
                .iter()
                .all(|state| state.guild_id == Some(Id::new(10)))
        );
        let named = states.iter().find(|s| s.user_id == Id::new(2)).unwrap();
        assert_eq!(named.name.as_deref(), Some("Nick"));
        assert!(named.self_mute);
        // Not among the members: left for the app to name.
        let unnamed = states.iter().find(|s| s.user_id == Id::new(3)).unwrap();
        assert_eq!(unnamed.name, None);
    }

    #[test]
    fn ready_names_voice_states_from_merged_members() {
        let events = dispatch_json(
            "READY",
            r#"{
                "user": {"id": "1"},
                "guilds": [{
                    "id": "10",
                    "voice_states": [{"user_id": "2", "channel_id": "100"}]
                }],
                "merged_members": [[{"user_id": "2", "nick": "Merged"}]]
            }"#,
        );

        let states = ready_voice_states(&events);
        assert_eq!(states[0].name.as_deref(), Some("Merged"));
    }

    #[test]
    fn ready_skips_a_malformed_voice_state_and_keeps_the_rest() {
        let events = dispatch_json(
            "READY",
            r#"{
                "user": {"id": "1"},
                "guilds": [{
                    "id": "10",
                    "voice_states": [
                        {"channel_id": "100"},
                        {"user_id": "2", "channel_id": "100"}
                    ],
                    "roles": [{"id": "20", "name": "role"}]
                }]
            }"#,
        );

        assert_eq!(ready_voice_states(&events).len(), 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, GatewayEvent::GuildRoles { .. }))
        );
    }

    #[test]
    fn guild_create_replaces_the_guilds_voice_states() {
        let events = dispatch_json(
            "GUILD_CREATE",
            r#"{
                "id": "10",
                "voice_states": [{"user_id": "2", "channel_id": "100"}],
                "members": [{
                    "nick": "",
                    "user": {"id": "2", "username": "user2", "global_name": "Global"}
                }]
            }"#,
        );

        let Some(GatewayEvent::GuildVoiceStates { guild_id, states }) = events.first() else {
            panic!("GUILD_CREATE should yield the guild's voice states");
        };
        assert_eq!(*guild_id, Id::new(10));
        // A cleared nickname falls through to the global name.
        assert_eq!(states[0].name.as_deref(), Some("Global"));
    }

    #[test]
    fn guild_emojis_arrive_with_the_guild_and_their_updates() {
        let events = dispatch_json(
            "GUILD_CREATE",
            r#"{
                "id": "10",
                "emojis": [
                    {"id": "5", "name": "wave", "animated": true, "roles": ["20"]},
                    {"id": null, "name": "broken"}
                ]
            }"#,
        );
        let emojis = events.iter().find_map(|event| match event {
            GatewayEvent::GuildEmojis { emojis, .. } => Some(emojis),
            _ => None,
        });
        let [emoji] = emojis
            .expect("GUILD_CREATE should carry its emoji")
            .as_slice()
        else {
            panic!("the malformed emoji should be skipped");
        };
        assert!(emoji.animated && emoji.available);
        assert_eq!(emoji.roles, [Id::new(20)]);

        let events = dispatch_json("GUILD_EMOJIS_UPDATE", r#"{"guild_id": "10", "emojis": []}"#);
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::GuildEmojis { emojis, .. }] if emojis.is_empty()
        ));
    }

    #[test]
    fn guild_delete_clears_the_guilds_voice_states() {
        let events = dispatch_json("GUILD_DELETE", r#"{"id": "10", "unavailable": true}"#);

        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::GuildVoiceStates { guild_id, states }]
                if *guild_id == Id::new(10) && states.is_empty()
        ));
    }

    #[test]
    fn voice_state_update_reads_its_own_member() {
        let events = dispatch_json(
            "VOICE_STATE_UPDATE",
            r#"{
                "user_id": "2",
                "guild_id": "10",
                "channel_id": "100",
                "session_id": "abc",
                "member": {"user": {"id": "2", "username": "user2", "avatar": "hash"}}
            }"#,
        );

        let [GatewayEvent::VoiceState(state)] = events.as_slice() else {
            panic!("VOICE_STATE_UPDATE should yield one voice state");
        };
        assert_eq!(state.guild_id, Some(Id::new(10)));
        assert_eq!(state.name.as_deref(), Some("user2"));
        assert!(state.avatar_url.is_some());
    }

    #[test]
    fn ready_carries_read_states_and_the_channels_the_user_can_see() {
        let events = dispatch_json(
            "READY",
            r#"{
                "user": {"id": "1"},
                "read_state": [{"id": "100", "last_message_id": "40"}],
                "private_channels": [{"id": "200", "type": 1, "last_message_id": "9"}],
                "guilds": [{
                    "id": "10",
                    "joined_at": "2020-01-01T00:00:00+00:00",
                    "roles": [
                        {"id": "10", "name": "@everyone", "permissions": "0"},
                        {"id": "20", "name": "member", "permissions": "1024"}
                    ],
                    "channels": [
                        {"id": "100", "type": 0, "last_message_id": "50"}
                    ]
                }],
                "merged_members": [[{"user_id": "1", "roles": ["20"]}]]
            }"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            GatewayEvent::ReadStates(states) if states.len() == 1
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            GatewayEvent::LatestMessages(channels)
                if channels[0].channel_id == Id::new(200)
        )));
        // Visible through the member's role, not `@everyone`.
        let Some(GatewayEvent::GuildUnreads(guild)) = events
            .iter()
            .find(|event| matches!(event, GatewayEvent::GuildUnreads(_)))
        else {
            panic!("READY should carry the guild's channels");
        };
        assert_eq!(guild.channels.len(), 1);
        assert!(guild.joined.is_some());
    }

    #[test]
    fn presences_arrive_from_ready_supplemental_and_updates() {
        let events = dispatch_json(
            "READY_SUPPLEMENTAL",
            r#"{
                "merged_presences": {
                    "friends": [{"user_id": "2", "status": "idle"}],
                    "guilds": [[{"user_id": "3", "status": "dnd"}, {"user_id": "4", "status": "streaming"}]]
                }
            }"#,
        );
        let [GatewayEvent::Presences(presences)] = events.as_slice() else {
            panic!("READY_SUPPLEMENTAL should yield its presences");
        };
        // An unknown status is skipped rather than guessed at.
        assert_eq!(
            presences.as_slice(),
            [
                (Id::new(2), PresenceStatus::Idle),
                (Id::new(3), PresenceStatus::DoNotDisturb)
            ]
        );

        let events = dispatch_json(
            "PRESENCE_UPDATE",
            r#"{"user": {"id": "2"}, "status": "offline", "activities": [], "guild_id": "10"}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::Presences(presences)]
                if presences.as_slice() == [(Id::new(2), PresenceStatus::Invisible)]
        ));
    }

    #[test]
    fn message_deletes_arrive_singly_and_in_bulk() {
        let events = dispatch_json("MESSAGE_DELETE", r#"{"id": "5", "channel_id": "100"}"#);
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::MessageDelete { channel_id, message_ids }]
                if *channel_id == Id::new(100) && message_ids == &[Id::new(5)]
        ));

        let events = dispatch_json(
            "MESSAGE_DELETE_BULK",
            r#"{"ids": ["5", "6"], "channel_id": "100", "guild_id": "10"}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::MessageDelete { message_ids, .. }] if message_ids.len() == 2
        ));
    }

    #[test]
    fn reactions_read_unicode_and_custom_emoji() {
        let events = dispatch_json(
            "MESSAGE_REACTION_ADD",
            r#"{
                "user_id": "2", "channel_id": "100", "message_id": "5",
                "emoji": {"id": null, "name": "👍"}, "burst": false, "type": 0
            }"#,
        );
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::Reaction {
                change: ReactionChange::Add { user_id, emoji: ReactionEmoji::Unicode(name) },
                ..
            }] if *user_id == Id::new(2) && name == "👍"
        ));

        let events = dispatch_json(
            "MESSAGE_REACTION_REMOVE_EMOJI",
            r#"{
                "channel_id": "100", "message_id": "5",
                "emoji": {"id": "7", "name": "wave", "animated": true}
            }"#,
        );
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::Reaction {
                change: ReactionChange::RemoveEmoji(ReactionEmoji::Custom { animated: true, .. }),
                ..
            }]
        ));

        let events = dispatch_json(
            "MESSAGE_REACTION_REMOVE_ALL",
            r#"{"channel_id": "100", "message_id": "5"}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::Reaction {
                change: ReactionChange::RemoveAll,
                ..
            }]
        ));

        // An add without its reactor is malformed, not a clear.
        let events = dispatch_json(
            "MESSAGE_REACTION_ADD",
            r#"{"channel_id": "100", "message_id": "5", "emoji": {"id": null, "name": "👍"}}"#,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn member_roles_arrive_from_member_chunks_and_messages() {
        let events = dispatch_json(
            "GUILD_MEMBERS_CHUNK",
            r#"{
                "guild_id": "10",
                "members": [{"user": {"id": "2"}, "roles": ["20", "30"]}],
                "chunk_index": 0,
                "chunk_count": 1,
                "not_found": ["3"]
            }"#,
        );
        let [
            GatewayEvent::MemberRoles {
                guild_id,
                user_id,
                roles,
            },
        ] = events.as_slice()
        else {
            panic!("GUILD_MEMBERS_CHUNK should yield each member's roles");
        };
        assert_eq!((*guild_id, *user_id), (Id::new(10), Id::new(2)));
        assert_eq!(roles, &[Id::new(20), Id::new(30)]);

        let events = dispatch_json(
            "MESSAGE_CREATE",
            r#"{
                "id": "500",
                "channel_id": "100",
                "guild_id": "10",
                "author": {"id": "4", "username": "user4", "discriminator": "0", "avatar": null},
                "member": {"roles": ["20"], "joined_at": "2020-01-01T00:00:00+00:00", "deaf": false, "mute": false, "flags": 0},
                "content": "hi",
                "timestamp": "2020-01-01T00:00:00+00:00",
                "edited_timestamp": null,
                "tts": false,
                "mention_everyone": false,
                "mentions": [],
                "mention_roles": [],
                "attachments": [],
                "embeds": [],
                "pinned": false,
                "type": 0
            }"#,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            GatewayEvent::MemberRoles { user_id, roles, .. }
                if *user_id == Id::new(4) && roles == &[Id::new(20)]
        )));
    }

    #[test]
    fn member_list_update_reads_groups_members_and_edits() {
        let events = dispatch_json(
            "GUILD_MEMBER_LIST_UPDATE",
            r#"{
                "guild_id": "10",
                "id": "everyone",
                "member_count": 3,
                "online_count": 2,
                "groups": [{"id": "20", "count": 1}, {"id": "online", "count": 1}, {"id": "offline", "count": 1}],
                "ops": [
                    {"op": "SYNC", "range": [0, 99], "items": [
                        {"group": {"id": "20"}},
                        {"member": {
                            "user": {"id": "2", "username": "user2", "global_name": "Two", "avatar": null, "bot": true},
                            "nick": null,
                            "roles": ["20"],
                            "presence": {"status": "dnd", "activities": [
                                {"type": 0, "name": "Chess"},
                                {"type": 4, "name": "Custom Status", "state": "busy"}
                            ]}
                        }},
                        {"group": {"id": "offline"}},
                        {"member": {"user": {"id": "3", "username": "user3"}, "roles": []}}
                    ]},
                    {"op": "UPDATE", "index": 3, "item": {"member": {
                        "user": {"id": "3", "username": "user3"}, "roles": [],
                        "presence": {"status": "online", "activities": [{"type": 2, "name": "Spotify"}]}
                    }}},
                    {"op": "DELETE", "index": 1},
                    {"op": "INVALIDATE", "range": [100, 199]}
                ]
            }"#,
        );
        let [GatewayEvent::MemberList(update)] = events.as_slice() else {
            panic!("GUILD_MEMBER_LIST_UPDATE should yield the update");
        };
        assert_eq!(update.guild_id, Id::new(10));
        assert_eq!(update.list_id, "everyone");
        assert!(matches!(
            update.groups.as_slice(),
            [
                MemberListGroup { kind: MemberGroupKind::Role(role), count: 1 },
                MemberListGroup { kind: MemberGroupKind::Online, .. },
                MemberListGroup { kind: MemberGroupKind::Offline, .. },
            ] if *role == Id::new(20)
        ));

        let [
            MemberListOp::Sync { start: 0, items },
            MemberListOp::Update {
                index: 3,
                item: MemberListItem::Member(updated),
            },
            MemberListOp::Delete { index: 1 },
            MemberListOp::Invalidate {
                start: 100,
                end: 199,
            },
        ] = update.ops.as_slice()
        else {
            panic!("each op should be read in order");
        };
        let [
            MemberListItem::Group(_),
            MemberListItem::Member(two),
            MemberListItem::Group(_),
            MemberListItem::Member(three),
        ] = items.as_slice()
        else {
            panic!("the sync should hold two headers and two members");
        };
        assert_eq!(two.name, "Two");
        assert!(two.bot);
        assert_eq!(two.roles, &[Id::new(20)]);
        assert_eq!(two.status, PresenceStatus::DoNotDisturb);
        // The custom status wins over the game.
        assert_eq!(two.activity.as_deref(), Some("busy"));
        // No presence reads as offline.
        assert_eq!(three.status, PresenceStatus::Invisible);
        assert_eq!(updated.activity.as_deref(), Some("Listening to Spotify"));
    }
}
