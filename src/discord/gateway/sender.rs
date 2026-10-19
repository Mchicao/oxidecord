//! Commands sent up the gateway.

use twilight_gateway::MessageSender;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, UserMarker},
};

use crate::discord::model::PresenceStatus;

/// Sends commands up the gateway from outside the receive loop.
///
/// Cloneable and cheap: commands are queued on a channel the shard drains, so
/// they survive a reconnect rather than failing while one is in flight.
#[derive(Clone)]
pub struct GatewaySender {
    pub(super) inner: MessageSender,
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

    /// Sends `REQUEST_CALL_CONNECT` (opcode 13): asks after the call in a DM.
    /// Without the `AUTO_CALL_CONNECT` capability, a call that started before
    /// the session did is only reported on request, as a `CALL_CREATE`;
    /// nothing comes back when there's none.
    pub fn request_call_connect(&self, channel_id: Id<ChannelMarker>) {
        let payload = serde_json::json!({
            "op": 13,
            "d": { "channel_id": channel_id }
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
