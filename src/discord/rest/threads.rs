//! The thread routes behind the threads panel: listing a channel's active
//! and archived threads, creating threads and forum posts, and joining and
//! archiving them.
//!
//! The active-thread listing routes are bot-only (docs.discord.food marks
//! `/guilds/{id}/threads/active` and `/channels/{id}/threads/active` "not
//! usable by user accounts"), so the active list comes from the thread
//! search endpoint instead.

use std::collections::HashMap;

use serde::Deserialize;
use twilight_http::request::channel::thread::create_forum_thread::ForumThread;
use twilight_http::request::{Method, RequestBuilder};
use twilight_model::channel::thread::{AutoArchiveDuration, ThreadMember};
use twilight_model::channel::{Channel, ChannelType};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker, TagMarker},
};

use super::request;
use crate::discord::model::threads::{
    ArchivedCursor, ArchivedScope, ThreadPage, ThreadSummary, convert_thread, joined_thread_ids,
    preview_text,
};

/// How many threads one [`fetch_active_threads`] page requests, the most the
/// search endpoint allows.
pub const THREAD_PAGE_SIZE: usize = 25;

/// How many threads one [`fetch_archived_threads`] page requests; a response
/// with `has_more` false means the archive is exhausted.
pub const ARCHIVED_PAGE_SIZE: usize = 50;

/// The search endpoint answers `202 Accepted` with a `retry_after` until its
/// index is built, so one bounded retry follows the server's own delay
/// before the error reaches the UI.
const SEARCH_ATTEMPTS: usize = 2;

/// The search endpoint's response body. Every field is defaulted so the
/// `202` index-pending body, which carries none of them, still parses —
/// `retry_after` tells the two apart.
#[derive(Deserialize)]
struct ThreadSearchResponse {
    #[serde(default)]
    threads: Vec<Channel>,
    #[serde(default)]
    members: Vec<ThreadMember>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    first_messages: Option<Vec<twilight_model::channel::Message>>,
    #[serde(default)]
    retry_after: Option<f64>,
}

/// Fetches a page of the channel's active threads, newest activity first.
///
/// `offset` is the search endpoint's own cursor: a count of threads to skip,
/// which deduplication against the loaded list makes safe to reuse.
pub async fn fetch_active_threads(
    channel_id: Id<ChannelMarker>,
    offset: usize,
) -> Result<ThreadPage, String> {
    request(move |client| async move {
        // The index is usually built already; one retry covers the rare
        // first hit on a cold channel without chaining delays.
        for attempt in 0..SEARCH_ATTEMPTS {
            let search = RequestBuilder::raw(
                Method::Get,
                format!(
                    "channels/{channel_id}/threads/search?archived=false\
                     &limit={THREAD_PAGE_SIZE}&offset={offset}"
                ),
            )
            .build()?;
            let response: ThreadSearchResponse = client
                .request::<ThreadSearchResponse>(search)
                .await?
                .model()
                .await?;

            match response.retry_after {
                // At most one bounded wait, capped so a stuck index surfaces
                // as an error instead of a hung request.
                Some(seconds) if attempt + 1 < SEARCH_ATTEMPTS => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        (seconds * 1000.0).clamp(250.0, 5_000.0) as u64,
                    ))
                    .await;
                }
                Some(_) => {
                    return Err(anyhow::anyhow!(
                        "This channel's thread index isn't ready yet. Try again in a moment."
                    ));
                }
                None => return Ok(search_page(response)),
            }
        }
        unreachable!("the loop returns on every branch")
    })
    .await
}

/// Turns a search response into a page, attaching each post's first message
/// as its preview.
fn search_page(response: ThreadSearchResponse) -> ThreadPage {
    let joined = joined_thread_ids(&response.members);
    let previews: HashMap<u64, String> = response
        .first_messages
        .unwrap_or_default()
        .into_iter()
        .filter_map(|message| {
            preview_text(&message.content).map(|preview| (message.id.get(), preview))
        })
        .collect();

    let threads = response
        .threads
        .into_iter()
        .filter_map(|channel| {
            let joined = joined.contains(&channel.id).then_some(true);
            let mut summary = convert_thread(channel, joined)?;
            if let Some(preview) = previews.get(&summary.id.get()) {
                summary.preview = Some(preview.clone());
            }
            Some(summary)
        })
        .collect();

    ThreadPage {
        threads,
        has_more: response.has_more,
    }
}

/// Fetches a page of the channel's archived threads under `scope`, newest
/// first. `before` picks up where the last page stopped, in the cursor kind
/// the scope's route accepts.
pub async fn fetch_archived_threads(
    channel_id: Id<ChannelMarker>,
    scope: ArchivedScope,
    before: Option<ArchivedCursor>,
) -> Result<ThreadPage, String> {
    request(move |client| async move {
        // Each scope is a different route with a different builder type, so
        // await per branch and share only the conversion.
        let listing = match scope {
            ArchivedScope::Public => {
                let mut request = client
                    .public_archived_threads(channel_id)
                    .limit(ARCHIVED_PAGE_SIZE as u64);
                // The formatted cursor must outlive the request it is baked
                // into, so it binds outside the borrow of the builder.
                let before = match before {
                    Some(ArchivedCursor::Timestamp(timestamp)) => {
                        Some(timestamp.iso_8601().to_string())
                    }
                    _ => None,
                };
                if let Some(before) = &before {
                    request = request.before(before);
                }
                request.await?.model().await?
            }
            ArchivedScope::Private => {
                let mut request = client
                    .private_archived_threads(channel_id)
                    .limit(ARCHIVED_PAGE_SIZE as u64);
                let before = match before {
                    Some(ArchivedCursor::Timestamp(timestamp)) => {
                        Some(timestamp.iso_8601().to_string())
                    }
                    _ => None,
                };
                if let Some(before) = &before {
                    request = request.before(before);
                }
                request.await?.model().await?
            }
            ArchivedScope::JoinedPrivate => {
                let mut request = client
                    .joined_private_archived_threads(channel_id)
                    .limit(ARCHIVED_PAGE_SIZE as u64);
                if let Some(ArchivedCursor::Id(id)) = before {
                    request = request.before(id);
                }
                request.await?.model().await?
            }
        };

        let joined = joined_thread_ids(&listing.members);
        let threads = listing
            .threads
            .into_iter()
            .filter_map(|channel| {
                let joined = joined.contains(&channel.id).then_some(true);
                convert_thread(channel, joined)
            })
            .collect();

        Ok(ThreadPage {
            threads,
            has_more: listing.has_more.unwrap_or(false),
        })
    })
    .await
}

/// Starts a thread on a text or announcement channel: public unless
/// `private`. Creating either needs the matching `CREATE_*_THREADS`
/// permission, which the caller checks first.
pub async fn create_thread(
    channel_id: Id<ChannelMarker>,
    name: String,
    private: bool,
    archive_minutes: u16,
) -> Result<ThreadSummary, String> {
    request(move |client| async move {
        let kind = if private {
            ChannelType::PrivateThread
        } else {
            ChannelType::PublicThread
        };
        let mut thread = client.create_thread(channel_id, &name, kind);
        if let Some(duration) = archive_duration(archive_minutes) {
            thread = thread.auto_archive_duration(duration);
        }
        let channel = thread.await?.model().await?;
        convert_thread(channel, None)
            .ok_or_else(|| anyhow::anyhow!("Discord's reply was not a thread"))
    })
    .await
}

/// Spins a thread off a message of a text or announcement channel. The
/// thread's id becomes the message's, so a message can only ever start one.
pub async fn create_thread_from_message(
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    name: String,
    archive_minutes: u16,
) -> Result<ThreadSummary, String> {
    request(move |client| async move {
        let mut thread = client.create_thread_from_message(channel_id, message_id, &name);
        if let Some(duration) = archive_duration(archive_minutes) {
            thread = thread.auto_archive_duration(duration);
        }
        let channel = thread.await?.model().await?;
        convert_thread(channel, None)
            .ok_or_else(|| anyhow::anyhow!("Discord's reply was not a thread"))
    })
    .await
}

/// Creates a forum post: the thread's name, its first message, and any tags
/// the channel requires or the user picked. In a thread-only channel this
/// needs `SEND_MESSAGES` alone — `CREATE_PUBLIC_THREADS` is ignored there.
pub async fn create_forum_post(
    channel_id: Id<ChannelMarker>,
    name: String,
    content: String,
    applied_tags: Vec<Id<TagMarker>>,
) -> Result<ThreadSummary, String> {
    request(move |client| async move {
        let mut post = client.create_forum_thread(channel_id, &name);
        if !applied_tags.is_empty() {
            post = post.applied_tags(&applied_tags);
        }
        let ForumThread { channel, .. } = post.message().content(&content).await?.model().await?;
        convert_thread(channel, None)
            .ok_or_else(|| anyhow::anyhow!("Discord's reply was not a thread"))
    })
    .await
}

/// Arcives or unarchives a thread. Unarchiving a non-locked thread only
/// needs `SEND_MESSAGES`; everything else needs `MANAGE_THREADS` or the
/// thread's creator, which the server decides.
pub async fn set_thread_archived(
    thread_id: Id<ChannelMarker>,
    archived: bool,
) -> Result<ThreadSummary, String> {
    request(move |client| async move {
        let channel = client
            .update_thread(thread_id)
            .archived(archived)
            .await?
            .model()
            .await?;
        convert_thread(channel, None)
            .ok_or_else(|| anyhow::anyhow!("Discord's reply was not a thread"))
    })
    .await
}

/// Joins the thread as the current user. Needs `VIEW_CHANNEL` and a
/// non-archived thread; joining is what turns on its notifications.
pub async fn join_thread(thread_id: Id<ChannelMarker>) -> Result<(), String> {
    request(move |client| async move {
        client.join_thread(thread_id).await?;
        Ok(())
    })
    .await
}

/// Leaves the thread. Notifications stop; nothing is deleted.
pub async fn leave_thread(thread_id: Id<ChannelMarker>) -> Result<(), String> {
    request(move |client| async move {
        client.leave_thread(thread_id).await?;
        Ok(())
    })
    .await
}

/// Maps the four archive durations Discord accepts, in minutes; anything
/// else leaves the field off and the server's default stands.
fn archive_duration(minutes: u16) -> Option<AutoArchiveDuration> {
    match minutes {
        60 => Some(AutoArchiveDuration::Hour),
        1440 => Some(AutoArchiveDuration::Day),
        4320 => Some(AutoArchiveDuration::ThreeDays),
        10080 => Some(AutoArchiveDuration::Week),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_durations_map_only_the_four_known_values() {
        assert!(matches!(
            archive_duration(60),
            Some(AutoArchiveDuration::Hour)
        ));
        assert!(matches!(
            archive_duration(1440),
            Some(AutoArchiveDuration::Day)
        ));
        assert!(matches!(
            archive_duration(4320),
            Some(AutoArchiveDuration::ThreeDays)
        ));
        assert!(matches!(
            archive_duration(10080),
            Some(AutoArchiveDuration::Week)
        ));
        assert_eq!(archive_duration(0), None);
        assert_eq!(archive_duration(1234), None);
    }
}
