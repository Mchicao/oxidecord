//! Mentions inbox (`GET`/`DELETE /users/@me/mentions`) and bulk read-state
//! acknowledgements (`POST /read-states/ack-bulk`).
//!
//! These are user-account endpoints documented in `docs.discord.food` with no
//! first-class helpers in `twilight-http`, so requests are built with
//! [`RequestBuilder::raw`] and executed on the shared background Tokio runtime
//! via [`super::request`].

use serde::{Deserialize, Serialize};
use twilight_http::request::{Method, RequestBuilder};
use twilight_http::response::marker::EmptyBody;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker},
};

use super::request;
use crate::discord::model::{Message, convert_message};

/// Default number of mentions requested per page (`1..=100` allowed by Discord).
pub const MENTIONS_PAGE_SIZE: u8 = 25;

/// Maximum number of read states sent in one `POST /read-states/ack-bulk` call.
pub const MAX_BULK_ACK_ENTRIES: usize = 100;

/// Query parameters for `GET /users/@me/mentions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MentionsQuery {
    /// Fetch mentions before this message snowflake (pagination cursor).
    pub before: Option<Id<MessageMarker>>,
    /// Max number of mentions to return (`1..=100`, clamped when building the path).
    pub limit: u8,
    /// Restrict mentions to a single guild when `Some`.
    pub guild_id: Option<Id<GuildMarker>>,
    /// Include messages that mention a role the user holds.
    pub roles: bool,
    /// Include `@everyone` and `@here` mentions.
    pub everyone: bool,
}

impl Default for MentionsQuery {
    fn default() -> Self {
        Self {
            before: None,
            limit: MENTIONS_PAGE_SIZE,
            guild_id: None,
            roles: true,
            everyone: true,
        }
    }
}

impl MentionsQuery {
    /// Builds the relative REST path and query string for [`RequestBuilder::raw`].
    pub fn to_path(self) -> String {
        let limit = self.limit.clamp(1, 100);
        let mut path = format!(
            "users/@me/mentions?limit={limit}&roles={}&everyone={}",
            self.roles, self.everyone
        );
        if let Some(before) = self.before {
            path.push_str(&format!("&before={before}"));
        }
        if let Some(guild_id) = self.guild_id {
            path.push_str(&format!("&guild_id={guild_id}"));
        }
        path
    }
}

/// A message returned by the mentions inbox endpoint, paired with the channel
/// and optional guild where it was posted.
#[derive(Clone)]
pub struct InboxMention {
    pub channel_id: Id<ChannelMarker>,
    pub guild_id: Option<Id<GuildMarker>>,
    pub message: Message,
}

/// One channel read-state entry for `POST /read-states/ack-bulk`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkAckEntry {
    pub channel_id: Id<ChannelMarker>,
    pub message_id: Id<MessageMarker>,
    pub read_state_type: u8,
}

impl BulkAckEntry {
    pub const fn channel(channel_id: Id<ChannelMarker>, message_id: Id<MessageMarker>) -> Self {
        Self {
            channel_id,
            message_id,
            read_state_type: 0,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BulkAckPayload {
    read_states: Vec<BulkAckEntry>,
}

/// Converts a raw `twilight_model::channel::Message` into an [`InboxMention`].
pub fn convert_inbox_mention(raw: twilight_model::channel::Message) -> InboxMention {
    let channel_id = raw.channel_id;
    let guild_id = raw.guild_id.or_else(|| {
        raw.reference
            .as_ref()
            .and_then(|reference| reference.guild_id)
    });
    InboxMention {
        channel_id,
        guild_id,
        message: convert_message(raw),
    }
}

/// Converts a page of raw `twilight_model::channel::Message` items into
/// [`InboxMention`] entries ordered newest-first.
pub fn convert_mentions_list(
    raw_messages: Vec<twilight_model::channel::Message>,
) -> Vec<InboxMention> {
    let mut mentions: Vec<InboxMention> = raw_messages
        .into_iter()
        .map(convert_inbox_mention)
        .collect();
    mentions.sort_by_key(|item| std::cmp::Reverse(item.message.id));
    mentions
}

/// Decodes a raw JSON array from `GET /users/@me/mentions` into [`InboxMention`]
/// items, ordered newest first as the inbox displays them.
#[cfg(test)]
pub fn decode_mentions_payload(raw_json: &str) -> Result<Vec<InboxMention>, String> {
    let raw_messages: Vec<twilight_model::channel::Message> =
        serde_json::from_str(raw_json).map_err(|err| err.to_string())?;
    Ok(convert_mentions_list(raw_messages))
}

/// Builds the JSON payload for `POST /read-states/ack-bulk`.
pub fn encode_bulk_ack_payload(entries: &[BulkAckEntry]) -> Result<serde_json::Value, String> {
    serde_json::to_value(BulkAckPayload {
        read_states: entries.to_vec(),
    })
    .map_err(|err| err.to_string())
}

/// Fetches a page of recent mentions for the signed-in user
/// (`GET /users/@me/mentions`), newest first.
pub async fn fetch_mentions(query: MentionsQuery) -> Result<Vec<InboxMention>, String> {
    request(move |client| async move {
        let request = RequestBuilder::raw(Method::Get, query.to_path()).build()?;
        let response = client
            .request::<Vec<twilight_model::channel::Message>>(request)
            .await?;
        let raw_messages = response.model().await?;
        Ok(convert_mentions_list(raw_messages))
    })
    .await
}

/// Removes a message from the user's recent mentions inbox
/// (`DELETE /users/@me/mentions/{message.id}`).
pub async fn delete_mention(message_id: Id<MessageMarker>) -> Result<(), String> {
    request(move |client| async move {
        let request =
            RequestBuilder::raw(Method::Delete, format!("users/@me/mentions/{message_id}"))
                .build()?;
        client.request::<EmptyBody>(request).await?;
        Ok(())
    })
    .await
}

/// Acknowledges multiple channels' newest messages in bulk
/// (`POST /read-states/ack-bulk`), chunked to [`MAX_BULK_ACK_ENTRIES`].
pub async fn ack_bulk(entries: Vec<BulkAckEntry>) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    request(move |client| async move {
        for chunk in entries.chunks(MAX_BULK_ACK_ENTRIES) {
            let body = encode_bulk_ack_payload(chunk).map_err(anyhow::Error::msg)?;
            let request = RequestBuilder::raw(Method::Post, "read-states/ack-bulk".to_string())
                .json(&body)
                .build()?;
            client.request::<EmptyBody>(request).await?;
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        BulkAckEntry, MENTIONS_PAGE_SIZE, MentionsQuery, decode_mentions_payload,
        encode_bulk_ack_payload,
    };
    use twilight_model::id::Id;

    #[test]
    fn mentions_query_builds_expected_path_and_clamps_limits() {
        let default_q = MentionsQuery::default();
        assert_eq!(
            default_q.to_path(),
            format!("users/@me/mentions?limit={MENTIONS_PAGE_SIZE}&roles=true&everyone=true")
        );

        let filtered = MentionsQuery {
            before: Some(Id::new(999_888_777)),
            limit: 250,
            guild_id: Some(Id::new(111_222_333)),
            roles: false,
            everyone: true,
        };
        assert_eq!(
            filtered.to_path(),
            "users/@me/mentions?limit=100&roles=false&everyone=true&before=999888777&guild_id=111222333"
        );

        let zero_limit = MentionsQuery {
            limit: 0,
            ..MentionsQuery::default()
        };
        assert!(
            zero_limit
                .to_path()
                .starts_with("users/@me/mentions?limit=1&")
        );
    }

    #[test]
    fn decodes_representative_mentions_payload_fixture() {
        let fixture = r#"[
            {
                "id": "1200000000000000010",
                "channel_id": "1100000000000000001",
                "guild_id": "1000000000000000001",
                "author": {
                    "id": "900000000000000001",
                    "username": "ada",
                    "discriminator": "0",
                    "global_name": "Ada Lovelace",
                    "avatar": null
                },
                "member": {
                    "nick": "Countess Ada",
                    "roles": ["800000000000000001"],
                    "flags": 0,
                    "deaf": false,
                    "mute": false
                },
                "content": "Hey <@900000000000000099>, check the engine notes!",
                "timestamp": "2026-10-18T10:15:30.000000+00:00",
                "edited_timestamp": null,
                "tts": false,
                "mention_everyone": false,
                "mentions": [
                    {
                        "id": "900000000000000099",
                        "username": "babbage",
                        "discriminator": "0",
                        "global_name": "Charles Babbage",
                        "public_flags": 0,
                        "avatar": null
                    }
                ],
                "mention_roles": ["800000000000000001"],
                "attachments": [],
                "embeds": [],
                "pinned": false,
                "type": 0
            },
            {
                "id": "1200000000000000025",
                "channel_id": "1100000000000000002",
                "author": {
                    "id": "900000000000000002",
                    "username": "grace",
                    "discriminator": "0",
                    "global_name": null,
                    "avatar": null
                },
                "content": "@here compiler build finished",
                "timestamp": "2026-10-18T11:00:00.000000+00:00",
                "edited_timestamp": "2026-10-18T11:01:00.000000+00:00",
                "tts": false,
                "mention_everyone": true,
                "mentions": [],
                "mention_roles": [],
                "attachments": [],
                "embeds": [],
                "pinned": false,
                "type": 0
            }
        ]"#;

        let decoded = decode_mentions_payload(fixture).expect("fixture should decode");
        assert_eq!(decoded.len(), 2);

        // Sorted newest-first by snowflake id.
        assert_eq!(decoded[0].message.id, Id::new(1_200_000_000_000_000_025));
        assert_eq!(decoded[0].channel_id, Id::new(1_100_000_000_000_000_002));
        assert_eq!(decoded[0].guild_id, None);
        assert_eq!(decoded[0].message.author_name, "grace");
        assert!(decoded[0].message.mention_everyone);
        assert!(decoded[0].message.edited.is_some());

        // Guild mention uses member nick over global_name.
        assert_eq!(decoded[1].message.id, Id::new(1_200_000_000_000_000_010));
        assert_eq!(decoded[1].channel_id, Id::new(1_100_000_000_000_000_001));
        assert_eq!(
            decoded[1].guild_id,
            Some(Id::new(1_000_000_000_000_000_001))
        );
        assert_eq!(decoded[1].message.author_name, "Countess Ada");
        assert_eq!(decoded[1].message.mentions.len(), 1);
        assert_eq!(decoded[1].message.mentions[0].name, "babbage");
        assert_eq!(
            decoded[1].message.mention_roles,
            vec![Id::new(800_000_000_000_000_001)]
        );
    }

    #[test]
    fn bulk_ack_payload_matches_discord_schema() {
        let entries = vec![
            BulkAckEntry::channel(Id::new(10), Id::new(100)),
            BulkAckEntry::channel(Id::new(20), Id::new(200)),
        ];
        let json = encode_bulk_ack_payload(&entries).expect("encode bulk ack");
        assert_eq!(
            json,
            serde_json::json!({
                "read_states": [
                    {
                        "channel_id": "10",
                        "message_id": "100",
                        "read_state_type": 0
                    },
                    {
                        "channel_id": "20",
                        "message_id": "200",
                        "read_state_type": 0
                    }
                ]
            })
        );
    }
}
