//! The message tools: full search across a server or conversation, and the
//! channel's pinned messages, both docked in a panel beside the chat.
//!
//! [`MessageToolsState`] holds everything; the methods that need the rest of
//! the screen live in [`data`] as [`HomeScreen`] extensions, and [`view`]
//! renders the panel. Requests are one-shot tasks guarded by an epoch, so a
//! response can never land in a search or pin list that has moved on — typing,
//! retrying, or switching conversations cancels what was in flight instead of
//! queuing it.

mod data;
mod view;

use std::collections::HashSet;
use std::time::Duration;

use gpui::{Context, ListAlignment, ListState, Pixels, Task, Window, px};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker},
};

use crate::discord::message_tools::{PinEntry, SearchHit, SearchRequest};
use crate::screens::home::HomeScreen;

/// How long typing in the header search box waits before it searches.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// How many times one query may be resumed after an "index not ready" answer
/// before the panel gives up offering it.
const MAX_SEARCH_RETRIES: u8 = 3;

/// How many search hits are kept in memory. Paging stops here and the panel
/// asks for a narrower query, instead of growing toward a ten-thousand-hit
/// response set.
pub(super) const MAX_SEARCH_HITS: usize = 500;

/// Same ceiling for the pin list.
pub(super) const MAX_PINS: usize = 500;

/// How far past the viewport a virtualized list pre-renders, in pixels.
const LIST_OVERSCAN: Pixels = px(512.);

/// What the docked panel is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolsPanel {
    Search,
    Pins,
}

/// Where a search looks. A DM conversation only has the channel scope.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchScope {
    Channel,
    Guild,
}

pub(super) struct MessageToolsState {
    pub(super) panel: Option<ToolsPanel>,
    pub(super) search: SearchState,
    pub(super) pins: PinsState,
}

pub(super) struct SearchState {
    pub(super) scope: SearchScope,
    /// The query as last submitted — what the current results answer to.
    pub(super) query: String,
    /// The request the current results (or error) answer to, kept so "load
    /// more" and "retry" re-run exactly it instead of re-resolving the box.
    pub(super) last_request: Option<SearchRequest>,
    pub(super) hits: Vec<SearchHit>,
    /// Virtualized row list over `hits`; kept in sync by reset/splice in the
    /// data layer, so the view builds only the rows in the viewport instead
    /// of every hit on hover.
    pub(super) list: ListState,
    pub(super) total_results: u64,
    pub(super) next_offset: usize,
    /// No further page exists, or none can be reached through the API's
    /// offset ceiling or the in-memory hit cap.
    pub(super) complete: bool,
    /// The hit cap was reached: what's shown is everything this query gets,
    /// and the panel says so.
    pub(super) capped: bool,
    pub(super) loading: bool,
    pub(super) loading_more: bool,
    pub(super) error: Option<String>,
    /// The API answered 202: the guild's index isn't ready yet. The search
    /// may be resumed, at most `MAX_SEARCH_RETRIES` times per query.
    pub(super) indexing: bool,
    pub(super) retries_used: u8,
    /// The API said the guild is running a deep historical index, so results
    /// may be incomplete until it finishes.
    pub(super) deep_indexing: bool,
    pub(super) epoch: u64,
    /// The pending re-search after the debounce; replaced (and so cancelled)
    /// by every keystroke.
    pub(super) debounce: Option<Task<()>>,
    pub(super) _task: Option<Task<()>>,
}

pub(super) struct PinsState {
    /// Which conversation's pins are loaded; reset on every switch.
    pub(super) channel_id: Option<Id<ChannelMarker>>,
    pub(super) entries: Vec<PinEntry>,
    /// Virtualized row list over `entries`; same contract as the search one.
    pub(super) list: ListState,
    /// The last page came back with more behind it, paged by `pinned_at`.
    pub(super) has_more: bool,
    /// The pin cap was reached: the newest pins are shown, older ones aren't
    /// fetched, and the panel says so.
    pub(super) capped: bool,
    pub(super) loading: bool,
    pub(super) loading_more: bool,
    pub(super) error: Option<String>,
    /// Pin/unpin requests in flight, by message. Nothing is applied
    /// optimistically: the list and the menu's pin state only move when the
    /// server has answered, so a failure leaves nothing to revert.
    pub(super) busy: HashSet<Id<MessageMarker>>,
    pub(super) epoch: u64,
    pub(super) _task: Option<Task<()>>,
    pub(super) _toggle_task: Option<Task<()>>,
}

/// What the message menu should offer for a message, decided from its known
/// pin state. Having written the message is irrelevant: pinning anyone's
/// message — one's own included — needs the channel permission.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PinMenuAction {
    Pin,
    Unpin,
}

impl MessageToolsState {
    pub(super) fn new(_window: &mut Window, _cx: &mut Context<HomeScreen>) -> Self {
        Self {
            panel: None,
            search: SearchState {
                scope: SearchScope::Channel,
                query: String::new(),
                last_request: None,
                hits: Vec::new(),
                list: ListState::new(0, ListAlignment::Top, LIST_OVERSCAN),
                total_results: 0,
                next_offset: 0,
                complete: false,
                capped: false,
                loading: false,
                loading_more: false,
                error: None,
                indexing: false,
                retries_used: 0,
                deep_indexing: false,
                epoch: 0,
                debounce: None,
                _task: None,
            },
            pins: PinsState {
                channel_id: None,
                entries: Vec::new(),
                list: ListState::new(0, ListAlignment::Top, LIST_OVERSCAN),
                has_more: false,
                capped: false,
                loading: false,
                loading_more: false,
                error: None,
                busy: HashSet::new(),
                epoch: 0,
                _task: None,
                _toggle_task: None,
            },
        }
    }
}
