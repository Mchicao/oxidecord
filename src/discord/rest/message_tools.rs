//! Message search across a guild or one conversation, and a channel's pins.
//!
//! Search has no twilight helper, so the request is built raw: the query string
//! is assembled here from documented parameters only
//! ([docs.discord.food/resources/message](https://docs.discord.food/resources/message),
//! *Search Guild Messages*), with every user-supplied value percent-encoded
//! before it is concatenated. Pins go through twilight's, which targets the
//! current paginated endpoints rather than the deprecated ones.

use std::time::Duration;

use twilight_http::request::{Method, RequestBuilder};
use twilight_http::response::marker::EmptyBody;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
};
use twilight_model::util::Timestamp;

use super::request;
use crate::discord::model::{Message, convert_message};

/// How many hits one search page carries; the API's own page size.
pub const SEARCH_PAGE_SIZE: usize = 25;

/// How many pins one [`fetch_pins`] page carries; the API's maximum.
pub const PINS_PAGE_SIZE: usize = 50;

/// The API refuses offsets past this, so deeper results are unreachable and
/// the UI says the list is complete instead of offering a page that can't load.
pub const MAX_SEARCH_OFFSET: usize = 9975;

/// Longest `content` the search index accepts.
pub const MAX_QUERY_LEN: usize = 1024;

/// Unix millisecond of the Discord snowflake epoch (2015-01-01T00:00:00Z),
/// which `before:`/`after:` dates are converted into.
pub const SNOWFLAKE_EPOCH_MS: i64 = 1_420_070_400_000;

/// Error code the API answers 202 with while a guild's search index is still
/// being built. The body carries `retry_after` in seconds.
pub const INDEX_PENDING_CODE: i64 = 110_000;

/// What `has:` filters messages by. The names are the API's own
/// `has` values; anything else is rejected before a request is built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchHas {
    Image,
    Sound,
    Video,
    File,
    Sticker,
    Embed,
    Link,
    Poll,
    Snapshot,
}

impl SearchHas {
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.to_ascii_lowercase();
        Some(match value.as_str() {
            "image" => Self::Image,
            "sound" => Self::Sound,
            "video" => Self::Video,
            "file" => Self::File,
            "sticker" => Self::Sticker,
            "embed" => Self::Embed,
            "link" => Self::Link,
            "poll" => Self::Poll,
            "snapshot" => Self::Snapshot,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Sound => "sound",
            Self::Video => "video",
            Self::File => "file",
            Self::Sticker => "sticker",
            Self::Embed => "embed",
            Self::Link => "link",
            Self::Poll => "poll",
            Self::Snapshot => "snapshot",
        }
    }
}

/// One search request, as the endpoint wants it. `guild_id` selects the guild
/// endpoint; without it the channel endpoint searches the DM conversation
/// `channel_id` names.
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub guild_id: Option<Id<GuildMarker>>,
    pub channel_id: Id<ChannelMarker>,
    pub content: String,
    pub author_ids: Vec<Id<UserMarker>>,
    /// Guild scope only: the API ignores channel filters on private channels.
    pub channel_ids: Vec<Id<ChannelMarker>>,
    pub has: Vec<SearchHas>,
    /// Snowflake floor: results were sent after this (`after:`).
    pub min_id: Option<u64>,
    /// Snowflake ceiling: results were sent before this (`before:`).
    pub max_id: Option<u64>,
    pub offset: usize,
}

/// One flattened hit: the message plus the conversation it was found in,
/// which a guild-wide search needs to name and jump to.
pub struct SearchHit {
    pub channel_id: Id<ChannelMarker>,
    pub message: Message,
}

pub struct SearchResults {
    pub total_results: u64,
    pub hits: Vec<SearchHit>,
    /// The API flagged a deep historical index running, so results may grow
    /// in behind the panel's back until it finishes.
    pub deep_indexing: bool,
}

/// What a search came back as. `Indexing` is the API's 202: the guild's index
/// isn't ready, and the request may be resumed after `retry_after`.
pub enum SearchOutcome {
    Page(SearchResults),
    Indexing { retry_after: Duration },
}

/// One entry of a channel's pin list.
pub struct PinEntry {
    /// When the message was pinned, in Unix seconds. Pagination walks these.
    pub pinned_at: i64,
    pub message: Message,
}

pub struct PinPage {
    pub entries: Vec<PinEntry>,
    pub has_more: bool,
}

/// Runs a search, on whichever endpoint the request names.
pub async fn search_messages(req: SearchRequest) -> Result<SearchOutcome, String> {
    let guild_id = req.guild_id;
    let channel_id = req.channel_id;
    let query = build_search_query(&req)?;
    request(move |client| async move {
        let path = match guild_id {
            Some(guild_id) => format!("guilds/{guild_id}/messages/search?{query}"),
            None => format!("channels/{channel_id}/messages/search?{query}"),
        };
        let response = client
            .request::<EmptyBody>(RequestBuilder::raw(Method::Get, path).build()?)
            .await?;
        let body = response.bytes().await?;
        decode_search_response(&body).map_err(anyhow::Error::msg)
    })
    .await
}

/// Builds the endpoint's query string: documented parameters only, each value
/// percent-encoded, arrays as repeated keys.
pub fn build_search_query(req: &SearchRequest) -> Result<String, String> {
    if req.guild_id.is_none() && !req.channel_ids.is_empty() {
        return Err("channel filters only apply when searching a server".into());
    }
    if req.content.chars().count() > MAX_QUERY_LEN {
        return Err(format!("queries are limited to {MAX_QUERY_LEN} characters"));
    }
    if req.author_ids.len() > 100 {
        return Err("at most 100 authors can be filtered at once".into());
    }
    if req.channel_ids.len() > 500 {
        return Err("at most 500 channels can be filtered at once".into());
    }
    if req.offset > MAX_SEARCH_OFFSET {
        return Err("this search has more results than Discord will page through".into());
    }

    let mut params: Vec<(&str, String)> = Vec::new();
    if !req.content.is_empty() {
        params.push(("content", req.content.clone()));
    }
    for author in &req.author_ids {
        params.push(("author_id", author.get().to_string()));
    }
    for channel in &req.channel_ids {
        params.push(("channel_id", channel.get().to_string()));
    }
    for has in &req.has {
        params.push(("has", has.as_str().to_string()));
    }
    if let Some(min_id) = req.min_id {
        params.push(("min_id", min_id.to_string()));
    }
    if let Some(max_id) = req.max_id {
        params.push(("max_id", max_id.to_string()));
    }
    if req.offset > 0 {
        params.push(("offset", req.offset.to_string()));
    }
    params.push(("limit", SEARCH_PAGE_SIZE.to_string()));

    Ok(params
        .into_iter()
        .map(|(key, value)| format!("{key}={}", encode_query_component(&value)))
        .collect::<Vec<_>>()
        .join("&"))
}

/// Percent-encodes one query-string component: everything outside the
/// unreserved set is escaped, UTF-8 included, spaces as `%20`.
pub fn encode_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Turns a Unix millisecond timestamp into the snowflake that marks it, so
/// date filters can ride `min_id`/`max_id`.
pub fn snowflake_from_unix_ms(ms: i64) -> u64 {
    ((ms - SNOWFLAKE_EPOCH_MS).max(0) as u64) << 22
}

/// Parses a search response. A 202-style body — the API answers success codes
/// with an error-shaped one while the guild is being indexed — comes back as
/// [`SearchOutcome::Indexing`] instead of a decode failure.
pub fn decode_search_response(body: &[u8]) -> Result<SearchOutcome, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|err| format!("unreadable search response: {err}"))?;
    if value.get("code").and_then(serde_json::Value::as_i64) == Some(INDEX_PENDING_CODE) {
        let retry_after = value
            .get("retry_after")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let retry_after = Duration::try_from_secs_f64(retry_after)
            .map_err(|_| "invalid search indexing retry delay".to_string())?;
        if retry_after > Duration::from_secs(60) {
            return Err("Search indexing is taking longer than expected. Try again later.".into());
        }
        return Ok(SearchOutcome::Indexing { retry_after });
    }

    #[derive(serde::Deserialize)]
    struct RawPage {
        #[serde(default)]
        total_results: u64,
        #[serde(default)]
        messages: Vec<Vec<twilight_model::channel::Message>>,
        #[serde(default, rename = "doing_deep_historical_index")]
        deep_indexing: bool,
    }

    let raw: RawPage = serde_json::from_value(value)
        .map_err(|err| format!("unreadable search response: {err}"))?;
    let hits = raw
        .messages
        // The hits arrive as nested arrays — a former "surrounding context"
        // wrapper the API no longer fills in.
        .into_iter()
        .flatten()
        .map(|message| SearchHit {
            channel_id: message.channel_id,
            message: convert_message(message),
        })
        .collect();
    Ok(SearchOutcome::Page(SearchResults {
        total_results: raw.total_results,
        hits,
        deep_indexing: raw.deep_indexing,
    }))
}

/// Fetches a page of the channel's pins, newest pin first, starting before
/// `before` (a `pinned_at` from an earlier page). Empty for a first page.
pub async fn fetch_pins(
    channel_id: Id<ChannelMarker>,
    before: Option<i64>,
) -> Result<PinPage, String> {
    request(move |client| async move {
        let mut pins = client.pins(channel_id).limit(PINS_PAGE_SIZE as u16);
        if let Some(before) = before {
            let timestamp =
                Timestamp::from_secs(before).map_err(|err| anyhow::Error::msg(err.to_string()))?;
            pins = pins.before(timestamp);
        }
        let listing = pins.await?.model().await?;
        Ok(PinPage {
            has_more: listing.has_more,
            entries: listing
                .items
                .into_iter()
                .map(|pin| PinEntry {
                    pinned_at: pin.pinned_at.as_secs(),
                    message: convert_message(pin.message),
                })
                .collect(),
        })
    })
    .await
}

/// Pins a message. Guild channels require the pin permission server-side;
/// this only builds the call.
pub async fn pin_message(
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) -> Result<(), String> {
    request(move |client| async move {
        client.create_pin(channel_id, message_id).await?;
        Ok(())
    })
    .await
}

/// Unpins a message.
pub async fn unpin_message(
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) -> Result<(), String> {
    request(move |client| async move {
        client.delete_pin(channel_id, message_id).await?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example response from docs.discord.food, *Search Guild Messages*.
    fn example_page() -> Vec<u8> {
        serde_json::json!({
            "analytics_id": "67ab1d6e97c631025fbba32d4eb82a16",
            "doing_deep_historical_index": false,
            "total_results": 1,
            "messages": [[{
                "id": "1076986676557131916",
                "type": 0,
                "content": "domainreaction fear",
                "channel_id": "885895521909227581",
                "author": {
                    "id": "545581357812678656",
                    "username": "alien",
                    "global_name": "Alien",
                    "avatar": "60387de43133809b083fb0f7458d2708",
                    "avatar_decoration_data": null,
                    "discriminator": "0",
                    "public_flags": 4194432,
                    "primary_guild": null
                },
                "attachments": [],
                "embeds": [],
                "mentions": [],
                "mention_roles": [],
                "pinned": false,
                "mention_everyone": false,
                "tts": false,
                "timestamp": "2023-02-19T22:00:33.136000+00:00",
                "edited_timestamp": null,
                "flags": 0,
                "components": [],
                "hit": true
            }]]
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn encodes_query_components() {
        assert_eq!(encode_query_component("plain"), "plain");
        assert_eq!(encode_query_component("two words"), "two%20words");
        assert_eq!(encode_query_component("héllo"), "h%C3%A9llo");
        assert_eq!(encode_query_component("a&b=c/d"), "a%26b%3Dc%2Fd");
        assert_eq!(encode_query_component("-._~"), "-._~");
    }

    #[test]
    fn builds_query_from_documented_params() {
        let req = SearchRequest {
            guild_id: Some(Id::new(1)),
            channel_id: Id::new(2),
            content: "ping pong".into(),
            author_ids: vec![Id::new(30), Id::new(31)],
            channel_ids: vec![Id::new(40)],
            has: vec![SearchHas::File, SearchHas::Image],
            min_id: Some(50),
            max_id: Some(60),
            offset: 25,
        };
        assert_eq!(
            build_search_query(&req).unwrap(),
            "content=ping%20pong&author_id=30&author_id=31&channel_id=40&has=file&has=image\
             &min_id=50&max_id=60&offset=25&limit=25"
        );
    }

    #[test]
    fn omits_empty_params() {
        let req = SearchRequest {
            guild_id: None,
            channel_id: Id::new(2),
            content: String::new(),
            author_ids: Vec::new(),
            channel_ids: Vec::new(),
            has: Vec::new(),
            min_id: None,
            max_id: None,
            offset: 0,
        };
        assert_eq!(build_search_query(&req).unwrap(), "limit=25");
    }

    #[test]
    fn rejects_channel_filter_on_private_channels() {
        let req = SearchRequest {
            guild_id: None,
            channel_id: Id::new(2),
            content: String::new(),
            author_ids: Vec::new(),
            channel_ids: vec![Id::new(40)],
            has: Vec::new(),
            min_id: None,
            max_id: None,
            offset: 0,
        };
        assert!(build_search_query(&req).is_err());
    }

    #[test]
    fn rejects_oversized_query() {
        let req = SearchRequest {
            guild_id: Some(Id::new(1)),
            channel_id: Id::new(2),
            content: "x".repeat(MAX_QUERY_LEN + 1),
            author_ids: Vec::new(),
            channel_ids: Vec::new(),
            has: Vec::new(),
            min_id: None,
            max_id: None,
            offset: 0,
        };
        assert!(build_search_query(&req).is_err());
    }

    #[test]
    fn decodes_documented_example_page() {
        let outcome = decode_search_response(&example_page()).unwrap();
        let SearchOutcome::Page(page) = outcome else {
            panic!("expected a page");
        };
        assert_eq!(page.total_results, 1);
        assert_eq!(page.hits.len(), 1);
        assert!(!page.deep_indexing);
        assert_eq!(page.hits[0].channel_id.get(), 885895521909227581);
        assert_eq!(page.hits[0].message.content, "domainreaction fear");
        assert_eq!(page.hits[0].message.author_name, "Alien");
        // Search results carry no reactions; conversion must not mind.
        assert!(page.hits[0].message.reactions.is_empty());
    }

    #[test]
    fn decodes_index_pending_with_retry_after() {
        let body = serde_json::json!({
            "message": "Index not yet available. Try again later",
            "code": 110000,
            "documents_indexed": 0,
            "retry_after": 2
        })
        .to_string()
        .into_bytes();
        match decode_search_response(&body).unwrap() {
            SearchOutcome::Indexing { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(2))
            }
            _ => panic!("expected indexing"),
        }
    }

    #[test]
    fn decodes_fractional_retry_after() {
        let body = serde_json::json!({ "code": 110000, "retry_after": 0.5, "message": "x" })
            .to_string()
            .into_bytes();
        match decode_search_response(&body).unwrap() {
            SearchOutcome::Indexing { retry_after } => {
                assert_eq!(retry_after, Duration::from_millis(500))
            }
            _ => panic!("expected indexing"),
        }
    }

    #[test]
    fn invalid_or_excessive_index_delays_do_not_panic_or_suspend_the_search() {
        for delay in [-1., 61., 1e308] {
            let body = serde_json::json!({ "code": 110000, "retry_after": delay }).to_string();
            assert!(decode_search_response(body.as_bytes()).is_err());
        }
    }

    #[test]
    fn rejects_garbage_body() {
        assert!(decode_search_response(b"not json").is_err());
    }

    #[test]
    fn dates_become_snowflakes_at_the_documented_epoch() {
        assert_eq!(snowflake_from_unix_ms(SNOWFLAKE_EPOCH_MS), 0);
        // The example message's timestamp, derived backwards from its id: the
        // high bits of a snowflake are (ms - epoch), so rebuilding them from
        // the id's timestamp must reproduce the id's own high bits.
        let id: u64 = 1076986676557131916;
        let ms = (id >> 22) as i64 + SNOWFLAKE_EPOCH_MS;
        assert_eq!(snowflake_from_unix_ms(ms), id >> 22 << 22);
    }
}
