//! Voice states: who is in which voice channel, what they've silenced, and
//! who is streaming. Also the streams themselves, which are voice
//! connections of their own.
//!
//! These arrive as raw gateway dispatches rather than through twilight's
//! models, because a DM call's `VOICE_SERVER_UPDATE` carries no guild id and
//! twilight's model requires one.

use std::collections::HashMap;

use serde::Deserialize;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, UserMarker},
};

use super::guild::RawMember;

/// One user's presence in a voice channel, as the gateway last reported it.
#[derive(Clone)]
pub struct VoiceUserState {
    pub user_id: Id<UserMarker>,
    pub guild_id: Option<Id<GuildMarker>>,
    /// The channel they're in, or `None` when they've left voice entirely.
    pub channel_id: Option<Id<ChannelMarker>>,
    /// Identifies this voice session to the voice server; needed to connect.
    pub session_id: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    /// Muted or deafened by a moderator, which the user can't undo themselves.
    pub mute: bool,
    pub deaf: bool,
    pub suppress: bool,
    /// Going live: sharing their screen into the call.
    pub self_stream: bool,
    /// Display name and avatar, carried by dispatches that include the member.
    /// Absent ones fall back to whatever the app already knows about the user.
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

impl VoiceUserState {
    pub fn server_restricted(&self) -> bool {
        self.mute || self.deaf || self.suppress
    }
}

/// Where a call's voice websocket lives, from `VOICE_SERVER_UPDATE`.
#[derive(Clone)]
pub struct VoiceServerInfo {
    /// `None` while Discord is reallocating the call to another server; the
    /// next update carries the new one.
    pub endpoint: Option<String>,
    pub token: String,
    /// The guild for a channel call, absent for a DM call.
    pub guild_id: Option<Id<GuildMarker>>,
    pub channel_id: Option<Id<ChannelMarker>>,
}

#[derive(Deserialize)]
pub(in crate::discord) struct RawVoiceState {
    user_id: Id<UserMarker>,
    #[serde(default)]
    guild_id: Option<Id<GuildMarker>>,
    #[serde(default)]
    channel_id: Option<Id<ChannelMarker>>,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    self_mute: bool,
    #[serde(default)]
    self_deaf: bool,
    #[serde(default)]
    mute: bool,
    #[serde(default)]
    deaf: bool,
    #[serde(default)]
    suppress: bool,
    #[serde(default)]
    self_stream: bool,
    #[serde(default)]
    member: Option<RawMember>,
}

#[derive(Deserialize)]
pub(in crate::discord) struct RawVoiceServer {
    #[serde(default)]
    endpoint: Option<String>,
    token: String,
    #[serde(default)]
    guild_id: Option<Id<GuildMarker>>,
    #[serde(default)]
    channel_id: Option<Id<ChannelMarker>>,
}

/// A `VOICE_STATE_UPDATE`, which carries its own member when it has one.
pub(in crate::discord) fn convert_voice_state(raw: RawVoiceState) -> VoiceUserState {
    convert(raw, None, None)
}

/// Every voice state bundled into a guild object (`READY`'s guilds,
/// `GUILD_CREATE`). Those states carry neither the guild id nor a member, so
/// the guild comes from the object and the name from its `members`, when the
/// user is among them.
pub(in crate::discord) fn convert_guild_voice_states<'a>(
    guild_id: Id<GuildMarker>,
    states: Vec<RawVoiceState>,
    members: impl IntoIterator<Item = &'a RawMember>,
) -> Vec<VoiceUserState> {
    if states.is_empty() {
        return Vec::new();
    }
    let members: HashMap<_, _> = members
        .into_iter()
        .filter_map(|member| Some((member.user_id()?, member)))
        .collect();
    states
        .into_iter()
        .map(|state| {
            let member = members.get(&state.user_id).copied();
            convert(state, Some(guild_id), member)
        })
        .collect()
}

fn convert(
    raw: RawVoiceState,
    guild_id: Option<Id<GuildMarker>>,
    member: Option<&RawMember>,
) -> VoiceUserState {
    let member = raw.member.as_ref().or(member);
    let name = member.and_then(RawMember::display_name);
    let avatar_url = member.and_then(RawMember::avatar_url);

    VoiceUserState {
        user_id: raw.user_id,
        guild_id: raw.guild_id.or(guild_id),
        channel_id: raw.channel_id,
        session_id: raw.session_id,
        self_mute: raw.self_mute,
        self_deaf: raw.self_deaf,
        mute: raw.mute,
        deaf: raw.deaf,
        suppress: raw.suppress,
        self_stream: raw.self_stream,
        name,
        avatar_url,
    }
}

pub(in crate::discord) fn convert_voice_server(raw: RawVoiceServer) -> VoiceServerInfo {
    VoiceServerInfo {
        endpoint: raw.endpoint.map(secure_endpoint),
        token: raw.token,
        guild_id: raw.guild_id,
        channel_id: raw.channel_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(flags: serde_json::Value) -> RawVoiceState {
        let mut value = serde_json::json!({"user_id":"1", "channel_id":"2"});
        value
            .as_object_mut()
            .expect("voice state")
            .extend(flags.as_object().expect("flags").clone());
        serde_json::from_value(value).expect("raw voice state")
    }

    #[test]
    fn omitted_flags_default_false_and_live_suppression_preserves_mute_semantics() {
        let initial = convert_voice_state(raw(serde_json::json!({})));
        assert!(!initial.suppress && !initial.mute && !initial.deaf);
        assert!(!initial.server_restricted());
        let suppressed = convert_voice_state(raw(serde_json::json!({"suppress":true})));
        assert!(suppressed.suppress && suppressed.server_restricted());
        assert!(!suppressed.mute && !suppressed.deaf);
        let lifted = convert_voice_state(raw(serde_json::json!({"suppress":false})));
        assert!(!lifted.server_restricted());
        for flag in ["mute", "deaf"] {
            let restricted = convert_voice_state(raw(serde_json::json!({(flag):true})));
            assert!(restricted.server_restricted());
            assert!(!restricted.suppress);
        }
    }

    #[test]
    fn initial_guild_voice_states_keep_suppression_separate_from_server_mute() {
        let states = convert_guild_voice_states(
            Id::new(10),
            vec![
                raw(serde_json::json!({"suppress":true})),
                raw(serde_json::json!({})),
            ],
            std::iter::empty(),
        );
        assert_eq!(states[0].guild_id, Some(Id::new(10)));
        assert!(states[0].suppress && states[0].server_restricted());
        assert!(!states[0].mute && !states[0].deaf);
        assert!(!states[1].server_restricted());
    }
}

/// Where a stream's voice websocket lives, from `STREAM_SERVER_UPDATE`.
#[derive(Clone, Deserialize)]
pub struct StreamServerInfo {
    pub stream_key: String,
    /// Absent while Discord is reallocating the stream, like a call's.
    #[serde(default)]
    pub endpoint: Option<String>,
    pub token: String,
}

pub(in crate::discord) fn convert_stream_server(raw: StreamServerInfo) -> StreamServerInfo {
    StreamServerInfo {
        endpoint: raw.endpoint.map(secure_endpoint),
        ..raw
    }
}

/// The voice websocket is spoken over TLS, but Discord has handed out
/// endpoints with a plaintext `:80` on them; dropping it leaves the default
/// 443 the connection actually wants.
fn secure_endpoint(endpoint: String) -> String {
    endpoint
        .strip_suffix(":80")
        .map_or(endpoint.clone(), str::to_owned)
}

/// The key Discord names a user's stream by: the call it's in, and whose it
/// is. A DM call has no guild, and says so in the prefix.
pub fn stream_key(
    guild_id: Option<Id<GuildMarker>>,
    channel_id: Id<ChannelMarker>,
    user_id: Id<UserMarker>,
) -> String {
    match guild_id {
        Some(guild_id) => format!("guild:{guild_id}:{channel_id}:{user_id}"),
        None => format!("call:{channel_id}:{user_id}"),
    }
}
