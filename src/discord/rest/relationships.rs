//! Managing user relationships: friends, incoming/outgoing requests, and blocks.
//!
//! Discord documents relationship endpoints in userdocs (docs.discord.food).
//! All mutations return 204 No Content and are accompanied by live
//! `RELATIONSHIP_ADD` and `RELATIONSHIP_REMOVE` gateway dispatches.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use twilight_http::request::{Method, RequestBuilder};
use twilight_http::response::marker::{EmptyBody, ListBody};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, UserMarker},
};

use super::request;

/// Discord's numeric relationship type classification.
///
/// Any undocumented or newly added Discord type is safely normalized into
/// `RelationshipType::Unknown(code)` rather than failing deserialization of the
/// entire payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RelationshipType {
    None,
    Friend,
    Blocked,
    IncomingRequest,
    OutgoingRequest,
    Implicit,
    Suggestion,
    Unknown(u8),
}

impl RelationshipType {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Friend => 1,
            Self::Blocked => 2,
            Self::IncomingRequest => 3,
            Self::OutgoingRequest => 4,
            Self::Implicit => 5,
            Self::Suggestion => 6,
            Self::Unknown(n) => n,
        }
    }

    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Friend,
            2 => Self::Blocked,
            3 => Self::IncomingRequest,
            4 => Self::OutgoingRequest,
            5 => Self::Implicit,
            6 => Self::Suggestion,
            other => Self::Unknown(other),
        }
    }

    pub fn is_friend(self) -> bool {
        matches!(self, Self::Friend)
    }

    pub fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked)
    }

    pub fn is_pending(self) -> bool {
        matches!(self, Self::IncomingRequest | Self::OutgoingRequest)
    }
}

impl<'de> Deserialize<'de> for RelationshipType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = i64::deserialize(deserializer)?;
        let code = if (0..=255).contains(&raw) {
            raw as u8
        } else {
            255
        };
        Ok(Self::from_u8(code))
    }
}

impl Serialize for RelationshipType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.as_u8())
    }
}

/// The partial user object embedded inside a relationship record or gateway event.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RelationshipUser {
    pub id: Id<UserMarker>,
    pub username: String,
    #[serde(default)]
    pub discriminator: Option<String>,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: bool,
    #[serde(default)]
    pub public_flags: Option<u64>,
}

impl RelationshipUser {
    /// Effective display name: preferred global_name, falling back to username.
    pub fn display_name(&self) -> &str {
        if let Some(global_name) = &self.global_name {
            let trimmed = global_name.trim();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        &self.username
    }

    /// Numerical discriminator (0 for accounts migrated to unique usernames).
    pub fn discriminator_num(&self) -> u16 {
        self.discriminator
            .as_deref()
            .and_then(|d| d.parse::<u16>().ok())
            .unwrap_or(0)
    }

    /// User tag formatted as modern `@username` or legacy `username#1234`.
    pub fn tag(&self) -> String {
        let disc = self.discriminator_num();
        if disc == 0 || self.discriminator.as_deref() == Some("0") {
            format!("@{}", self.username)
        } else {
            format!("{}#{:04}", self.username, disc)
        }
    }

    /// CDN URL for user avatar, or default avatar computed from snowflake/discriminator.
    pub fn avatar_url(&self) -> String {
        let user_id = self.id.get();
        if let Some(hash) = &self.avatar {
            let ext = if hash.starts_with("a_") {
                "gif"
            } else {
                "webp"
            };
            format!("https://cdn.discordapp.com/avatars/{user_id}/{hash}.{ext}?size=80")
        } else {
            let disc = self.discriminator_num();
            let index = if disc == 0 {
                (user_id >> 22) % 6
            } else {
                u64::from(disc % 5)
            };
            format!("https://cdn.discordapp.com/embed/avatars/{index}.png")
        }
    }
}

/// Full relationship record returned by `GET /users/@me/relationships` or `RELATIONSHIP_ADD`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Relationship {
    pub id: Id<UserMarker>,
    #[serde(rename = "type")]
    pub kind: RelationshipType,
    pub user: RelationshipUser,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub user_ignored: bool,
    #[serde(default)]
    pub is_spam_request: bool,
    #[serde(default)]
    pub stranger_request: bool,
    #[serde(default)]
    pub since: Option<String>,
}

impl Relationship {
    /// Custom nickname if set by current user, else user's display name.
    pub fn display_name(&self) -> &str {
        if let Some(nick) = &self.nickname {
            let trimmed = nick.trim();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        self.user.display_name()
    }

    pub fn avatar_url(&self) -> String {
        self.user.avatar_url()
    }
}

/// Partial relationship record delivered by `RELATIONSHIP_REMOVE`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RelationshipRemove {
    pub id: Id<UserMarker>,
    #[serde(rename = "type")]
    pub kind: RelationshipType,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub user_ignored: bool,
}

/// Live relationship events from gateway dispatch.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum RelationshipEvent {
    Add(Relationship),
    Remove(RelationshipRemove),
}

/// Parses user tag input into `(username, Option<discriminator>)`.
/// Handles `@username`, `username`, and legacy `username#1234`.
pub fn parse_friend_tag(input: &str) -> (String, Option<String>) {
    let trimmed = input.trim().trim_start_matches('@');
    if let Some((name, disc)) = trimmed.split_once('#') {
        let disc_trimmed = disc.trim();
        if disc_trimmed.len() == 4 && disc_trimmed.chars().all(|c| c.is_ascii_digit()) {
            return (name.trim().to_owned(), Some(disc_trimmed.to_owned()));
        }
    }
    (trimmed.to_owned(), None)
}

/// Fetches all relationships of the current user (`GET /users/@me/relationships`).
pub async fn fetch_relationships() -> Result<Vec<Relationship>, String> {
    request(|client| async move {
        let request =
            RequestBuilder::raw(Method::Get, "users/@me/relationships".to_owned()).build()?;
        let response = client.request::<ListBody<Relationship>>(request).await?;
        let relationships = response.models().await?;
        Ok(relationships)
    })
    .await
}

/// Sends a friend request by username and optional discriminator (`POST /users/@me/relationships`).
/// Modern accounts omit or set discriminator to null.
pub async fn send_friend_request(
    username: &str,
    discriminator: Option<&str>,
    note: Option<&str>,
) -> Result<(), String> {
    let username = username.trim().trim_start_matches('@').to_owned();
    let discriminator = discriminator.map(|d| d.trim().to_owned());
    let note = note.map(|n| n.trim().to_owned());

    request(move |client| async move {
        let mut body = serde_json::json!({
            "username": username,
            "discriminator": discriminator,
        });
        if let Some(note) = note {
            body["note"] = serde_json::Value::String(note);
        }
        let request = RequestBuilder::raw(Method::Post, "users/@me/relationships".to_owned())
            .json(&body)
            .build()?;
        client.request::<EmptyBody>(request).await?;
        Ok(())
    })
    .await
}

/// Accepts an incoming friend request or sends a friend request by user snowflake (`PUT /users/@me/relationships/{user_id}`).
pub async fn accept_friend_request(user_id: Id<UserMarker>) -> Result<(), String> {
    request(move |client| async move {
        let request =
            RequestBuilder::raw(Method::Put, format!("users/@me/relationships/{user_id}"))
                .json(&serde_json::json!({ "type": -1 }))
                .build()?;
        client.request::<EmptyBody>(request).await?;
        Ok(())
    })
    .await
}

/// Generic relationship deletion (`DELETE /users/@me/relationships/{user_id}`).
/// Used for cancelling outgoing requests, declining incoming requests, removing friends, or unblocking.
pub async fn delete_relationship(user_id: Id<UserMarker>) -> Result<(), String> {
    request(move |client| async move {
        let request =
            RequestBuilder::raw(Method::Delete, format!("users/@me/relationships/{user_id}"))
                .build()?;
        client.request::<EmptyBody>(request).await?;
        Ok(())
    })
    .await
}

/// Declines an incoming friend request (`DELETE /users/@me/relationships/{user_id}`).
pub async fn decline_friend_request(user_id: Id<UserMarker>) -> Result<(), String> {
    delete_relationship(user_id).await
}

/// Cancels an outgoing pending friend request (`DELETE /users/@me/relationships/{user_id}`).
pub async fn cancel_friend_request(user_id: Id<UserMarker>) -> Result<(), String> {
    delete_relationship(user_id).await
}

/// Removes a user from friends (`DELETE /users/@me/relationships/{user_id}`).
pub async fn remove_friend(user_id: Id<UserMarker>) -> Result<(), String> {
    delete_relationship(user_id).await
}

/// Blocks a user (`PUT /users/@me/relationships/{user_id}` with `type: 2`).
pub async fn block_user(user_id: Id<UserMarker>) -> Result<(), String> {
    request(move |client| async move {
        let request =
            RequestBuilder::raw(Method::Put, format!("users/@me/relationships/{user_id}"))
                .json(&serde_json::json!({ "type": 2 }))
                .build()?;
        client.request::<EmptyBody>(request).await?;
        Ok(())
    })
    .await
}

/// Unblocks a user (`DELETE /users/@me/relationships/{user_id}`).
pub async fn unblock_user(user_id: Id<UserMarker>) -> Result<(), String> {
    delete_relationship(user_id).await
}

/// Creates or retrieves an existing 1-on-1 direct-message conversation (`POST /users/@me/channels`).
pub async fn create_or_open_dm(user_id: Id<UserMarker>) -> Result<Id<ChannelMarker>, String> {
    request(move |client| async move {
        let request = RequestBuilder::raw(Method::Post, "users/@me/channels".to_owned())
            .json(&serde_json::json!({ "recipient_id": user_id.to_string() }))
            .build()?;
        let channel = client
            .request::<twilight_model::channel::Channel>(request)
            .await?
            .model()
            .await?;
        Ok(channel.id)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unknown_relationship_type_deserialization() {
        let json_unknown =
            r#"{"id":"123456789","type":99,"user":{"id":"123456789","username":"mystery"}}"#;
        let rel: Relationship = serde_json::from_str(json_unknown).unwrap();
        assert_eq!(rel.kind, RelationshipType::Unknown(99));
        assert!(!rel.kind.is_friend());
        assert!(!rel.kind.is_blocked());
        assert!(!rel.kind.is_pending());

        let json_friend =
            r#"{"id":"123456789","type":1,"user":{"id":"123456789","username":"pal"}}"#;
        let rel_friend: Relationship = serde_json::from_str(json_friend).unwrap();
        assert_eq!(rel_friend.kind, RelationshipType::Friend);
        assert!(rel_friend.kind.is_friend());

        let json_blocked =
            r#"{"id":"123456789","type":2,"user":{"id":"123456789","username":"spammer"}}"#;
        let rel_blocked: Relationship = serde_json::from_str(json_blocked).unwrap();
        assert_eq!(rel_blocked.kind, RelationshipType::Blocked);
        assert!(rel_blocked.kind.is_blocked());

        let json_incoming =
            r#"{"id":"123456789","type":3,"user":{"id":"123456789","username":"someone"}}"#;
        let rel_inc: Relationship = serde_json::from_str(json_incoming).unwrap();
        assert_eq!(rel_inc.kind, RelationshipType::IncomingRequest);
        assert_eq!(rel_inc.kind, RelationshipType::IncomingRequest);
        assert!(rel_inc.kind.is_pending());
    }

    #[test]
    fn test_relationship_user_display_name_and_tag() {
        let u1 = RelationshipUser {
            id: Id::new(12345),
            username: "wumpus".to_owned(),
            discriminator: None,
            global_name: Some("Sir Wumpus".to_owned()),
            avatar: None,
            bot: false,
            public_flags: None,
        };
        assert_eq!(u1.display_name(), "Sir Wumpus");
        assert_eq!(u1.tag(), "@wumpus");

        let u2 = RelationshipUser {
            id: Id::new(12345),
            username: "classic_user".to_owned(),
            discriminator: Some("0042".to_owned()),
            global_name: None,
            avatar: None,
            bot: false,
            public_flags: None,
        };
        assert_eq!(u2.display_name(), "classic_user");
        assert_eq!(u2.tag(), "classic_user#0042");
    }

    #[test]
    fn test_parse_friend_tag() {
        assert_eq!(parse_friend_tag("@wumpus"), ("wumpus".to_owned(), None));
        assert_eq!(
            parse_friend_tag("wumpus#0001"),
            ("wumpus".to_owned(), Some("0001".to_owned()))
        );
        assert_eq!(
            parse_friend_tag("  @clyde#9999  "),
            ("clyde".to_owned(), Some("9999".to_owned()))
        );
        assert_eq!(
            parse_friend_tag("just_a_name"),
            ("just_a_name".to_owned(), None)
        );
    }
}
