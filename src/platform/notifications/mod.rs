//! Native desktop notifications: opt-in preferences, filtering, bounded
//! deduplication and rate limiting, privacy redaction, and click-to-jump
//! routing back to the conversation pane.
//!
//! Windows uses the WinRT `ToastNotificationManager` API in `windows.rs`;
//! other platforms compile `unsupported.rs`, which reports that native toasts
//! are unavailable without failing the caller.

#[cfg_attr(windows, path = "windows.rs")]
#[cfg_attr(not(windows), path = "unsupported.rs")]
mod backend;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use serde::{Deserialize, Serialize};
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
};

/// Maximum number of message snowflakes remembered for duplicate suppression.
pub const MAX_DEDUP_MESSAGES: usize = 256;

/// Maximum number of channels tracked for per-channel cooldowns.
pub const MAX_COOLDOWN_CHANNELS: usize = 128;

/// Minimum interval between desktop toasts for the same channel.
pub const CHANNEL_TOAST_COOLDOWN: Duration = Duration::from_millis(1500);

/// Sliding window for global burst rate limiting.
pub const GLOBAL_BURST_WINDOW: Duration = Duration::from_secs(10);

/// Maximum number of desktop toasts allowed within [`GLOBAL_BURST_WINDOW`].
pub const MAX_TOASTS_PER_BURST: usize = 5;

/// Maximum number of characters kept in a toast body preview.
pub const MAX_BODY_CHARS: usize = 180;

/// User-configurable desktop notification preferences persisted in `prefs.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationPreferences {
    /// Master opt-in toggle for native desktop toast notifications.
    #[serde(default)]
    pub enabled: bool,
    /// Whether to include author, channel, and message preview text in the
    /// OS toast. When `false`, generic text is shown so private content never
    /// appears on a shared screen or lockscreen.
    #[serde(default = "default_true")]
    pub show_content: bool,
    /// When `true`, guild channels only trigger a desktop toast when the
    /// message mentions the user, one of their roles, or `@everyone`/`@here`.
    /// Direct messages always count as direct notifications unless muted.
    #[serde(default = "default_true")]
    pub only_mentions_in_guilds: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for NotificationPreferences {
    fn default() -> Self {
        Self {
            enabled: false,
            show_content: true,
            only_mentions_in_guilds: true,
        }
    }
}

/// Destination opened when the user clicks a desktop toast notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationTarget {
    pub guild_id: Option<Id<GuildMarker>>,
    pub channel_id: Id<ChannelMarker>,
    pub message_id: Id<MessageMarker>,
}

impl NotificationTarget {
    /// Encodes the jump target into a compact `launch` argument string for the
    /// Windows toast XML payload.
    pub fn encode_launch_arg(self) -> String {
        match self.guild_id {
            Some(guild_id) => format!(
                "oxidecord://jump?guild_id={}&channel_id={}&message_id={}",
                guild_id.get(),
                self.channel_id.get(),
                self.message_id.get()
            ),
            None => format!(
                "oxidecord://jump?channel_id={}&message_id={}",
                self.channel_id.get(),
                self.message_id.get()
            ),
        }
    }

    /// Parses a `launch` argument string back into a [`NotificationTarget`].
    pub fn decode_launch_arg(arg: &str) -> Option<Self> {
        let query = arg.strip_prefix("oxidecord://jump?")?;
        let mut guild_id = None;
        let mut channel_id = None;
        let mut message_id = None;

        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            match key {
                "guild_id" => {
                    guild_id = Some(value.parse::<u64>().ok().and_then(Id::new_checked)?);
                }
                "channel_id" => {
                    channel_id = Some(value.parse::<u64>().ok().and_then(Id::new_checked)?);
                }
                "message_id" => {
                    message_id = Some(value.parse::<u64>().ok().and_then(Id::new_checked)?);
                }
                _ => {}
            }
        }

        Some(Self {
            guild_id,
            channel_id: channel_id?,
            message_id: message_id?,
        })
    }
}

/// Context needed to decide whether a live message should trigger a desktop
/// toast notification.
#[derive(Clone, Debug)]
pub struct NotificationCandidate {
    pub target: NotificationTarget,
    pub author_id: Id<UserMarker>,
    pub author_name: String,
    pub channel_name: String,
    pub guild_name: Option<String>,
    pub content: String,
    pub has_attachments_or_embeds: bool,
    /// Whether the message mentions the signed-in user directly, via one of
    /// their guild roles, or via an active `@everyone`/`@here`.
    pub is_mention: bool,
    /// Current user's ID, used to suppress self-authored messages.
    pub self_user_id: Option<Id<UserMarker>>,
    /// Whether the user's status is Do Not Disturb.
    pub is_dnd: bool,
    /// Whether the channel or its parent category is muted.
    pub channel_muted: bool,
    /// Whether the guild is muted.
    pub guild_muted: bool,
    /// Whether the author is blocked or ignored (Feature 7 integration).
    pub author_blocked: bool,
    /// Whether the app window is currently focused/active in the OS.
    pub window_active: bool,
    /// Whether this channel is the currently open conversation and scrolled to
    /// the live bottom.
    pub channel_visible_at_bottom: bool,
}

/// Why a candidate message did not produce a desktop notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuppressionReason {
    Disabled,
    OwnMessage,
    DoNotDisturb,
    BlockedUser,
    GuildMuted,
    ChannelMuted,
    ChannelFocusedAndVisible,
    NotMentionInGuild,
    DuplicateMessage,
    RateLimited,
    PlatformError(String),
}

/// Sanitized, display-ready toast content passed to the platform backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastPayload {
    pub title: String,
    pub body: String,
    pub attribution: Option<String>,
    pub tag: String,
    pub group: String,
    pub launch_arg: String,
    pub target: NotificationTarget,
    /// When `false`, the OS should hide private details on lockscreens.
    pub allow_lockscreen_content: bool,
}

/// Pure evaluation of static filter rules (before stateful deduplication/rate
/// limiting).
pub fn evaluate_filter(
    candidate: &NotificationCandidate,
    prefs: NotificationPreferences,
) -> Result<(), SuppressionReason> {
    if !prefs.enabled {
        return Err(SuppressionReason::Disabled);
    }
    if candidate.self_user_id == Some(candidate.author_id) {
        return Err(SuppressionReason::OwnMessage);
    }
    if candidate.is_dnd {
        return Err(SuppressionReason::DoNotDisturb);
    }
    if candidate.author_blocked {
        return Err(SuppressionReason::BlockedUser);
    }
    if candidate.guild_muted {
        return Err(SuppressionReason::GuildMuted);
    }
    if candidate.channel_muted {
        return Err(SuppressionReason::ChannelMuted);
    }
    if candidate.window_active && candidate.channel_visible_at_bottom {
        return Err(SuppressionReason::ChannelFocusedAndVisible);
    }
    if candidate.target.guild_id.is_some() && prefs.only_mentions_in_guilds && !candidate.is_mention
    {
        return Err(SuppressionReason::NotMentionInGuild);
    }
    Ok(())
}

/// Formats a [`NotificationCandidate`] into a [`ToastPayload`], applying
/// lockscreen/preview redaction when `prefs.show_content` is `false`.
pub fn build_toast_payload(
    candidate: &NotificationCandidate,
    prefs: NotificationPreferences,
) -> ToastPayload {
    let tag = format!("msg-{}", candidate.target.message_id.get());
    let group = format!("ch-{}", candidate.target.channel_id.get());
    let launch_arg = candidate.target.encode_launch_arg();

    if !prefs.show_content {
        let body = if candidate.target.guild_id.is_none() {
            "New direct message".to_string()
        } else if candidate.is_mention {
            "You were mentioned in a message".to_string()
        } else {
            "New message received".to_string()
        };
        return ToastPayload {
            title: "Oxidecord".to_string(),
            body,
            attribution: None,
            tag,
            group,
            launch_arg,
            target: candidate.target,
            allow_lockscreen_content: false,
        };
    }

    let (title, attribution) = match &candidate.guild_name {
        Some(guild_name) => {
            let channel_label = if candidate.channel_name.is_empty() {
                guild_name.clone()
            } else {
                format!("#{} \u{2022} {}", candidate.channel_name, guild_name)
            };
            (candidate.author_name.clone(), Some(channel_label))
        }
        None => {
            let attribution = (!candidate.channel_name.is_empty()
                && candidate.channel_name != candidate.author_name)
                .then(|| candidate.channel_name.clone());
            (candidate.author_name.clone(), attribution)
        }
    };

    let collapsed = collapse_whitespace(&candidate.content);
    let body = if !collapsed.is_empty() {
        truncate_chars(&collapsed, MAX_BODY_CHARS)
    } else if candidate.has_attachments_or_embeds {
        "Sent an attachment".to_string()
    } else {
        "Sent a message".to_string()
    };

    ToastPayload {
        title,
        body,
        attribution,
        tag,
        group,
        launch_arg,
        target: candidate.target,
        allow_lockscreen_content: true,
    }
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_BODY_CHARS + 16));
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.len() > MAX_BODY_CHARS * 4 {
            break;
        }
    }
    out
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}\u{2026}")
    } else {
        prefix
    }
}

/// Escapes text for safe inclusion inside a Windows Toast XML document.
pub fn escape_xml(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => escaped.push(c),
        }
    }
    escaped
}

/// Renders the Windows Toast XML template for a [`ToastPayload`].
pub fn render_toast_xml(payload: &ToastPayload) -> String {
    let launch = escape_xml(&payload.launch_arg);
    let title = escape_xml(&payload.title);
    let body = escape_xml(&payload.body);
    let lockscreen_attr = if payload.allow_lockscreen_content {
        ""
    } else {
        " hint-lockDetailedStatus1=\"\""
    };
    let attribution = payload
        .attribution
        .as_deref()
        .map_or_else(String::new, |attr| {
            format!(
                "<text placement=\"attribution\">{}</text>",
                escape_xml(attr)
            )
        });

    format!(
        "<toast launch=\"{launch}\" activationType=\"foreground\">\
            <visual>\
                <binding template=\"ToastGeneric\"{lockscreen_attr}>\
                    <text>{title}</text>\
                    <text>{body}</text>\
                    {attribution}\
                </binding>\
            </visual>\
        </toast>"
    )
}

/// Bounded deduplication and flood-control gate for desktop notifications.
pub struct NotificationGate {
    seen_order: VecDeque<Id<MessageMarker>>,
    seen_set: HashSet<Id<MessageMarker>>,
    channel_order: VecDeque<Id<ChannelMarker>>,
    channel_last_sent: HashMap<Id<ChannelMarker>, Instant>,
    burst_timestamps: VecDeque<Instant>,
    max_dedup: usize,
    max_channels: usize,
    channel_cooldown: Duration,
    burst_window: Duration,
    max_burst: usize,
}

impl Default for NotificationGate {
    fn default() -> Self {
        Self::new(
            MAX_DEDUP_MESSAGES,
            MAX_COOLDOWN_CHANNELS,
            CHANNEL_TOAST_COOLDOWN,
            GLOBAL_BURST_WINDOW,
            MAX_TOASTS_PER_BURST,
        )
    }
}

impl NotificationGate {
    pub fn new(
        max_dedup: usize,
        max_channels: usize,
        channel_cooldown: Duration,
        burst_window: Duration,
        max_burst: usize,
    ) -> Self {
        let max_dedup = max_dedup.max(1);
        let max_channels = max_channels.max(1);
        let max_burst = max_burst.max(1);
        Self {
            seen_order: VecDeque::with_capacity(max_dedup),
            seen_set: HashSet::with_capacity(max_dedup),
            channel_order: VecDeque::with_capacity(max_channels),
            channel_last_sent: HashMap::with_capacity(max_channels),
            burst_timestamps: VecDeque::with_capacity(max_burst),
            max_dedup,
            max_channels,
            channel_cooldown,
            burst_window,
            max_burst,
        }
    }

    /// Evaluates both static filters and stateful deduplication/rate-limiting
    /// at `now`, returning the ready-to-display [`ToastPayload`] when allowed.
    pub fn evaluate(
        &mut self,
        candidate: &NotificationCandidate,
        prefs: NotificationPreferences,
        now: Instant,
    ) -> Result<ToastPayload, SuppressionReason> {
        evaluate_filter(candidate, prefs)?;

        let message_id = candidate.target.message_id;
        if self.seen_set.contains(&message_id) {
            return Err(SuppressionReason::DuplicateMessage);
        }

        let channel_id = candidate.target.channel_id;
        if let Some(&last_sent) = self.channel_last_sent.get(&channel_id)
            && now.saturating_duration_since(last_sent) < self.channel_cooldown
        {
            return Err(SuppressionReason::RateLimited);
        }

        while let Some(&oldest) = self.burst_timestamps.front() {
            if now.saturating_duration_since(oldest) >= self.burst_window {
                self.burst_timestamps.pop_front();
            } else {
                break;
            }
        }
        if self.burst_timestamps.len() >= self.max_burst {
            return Err(SuppressionReason::RateLimited);
        }

        // Record message in bounded dedup set.
        if self.seen_len() >= self.max_dedup
            && let Some(evicted) = self.seen_order.pop_front()
        {
            self.seen_set.remove(&evicted);
        }
        self.seen_order.push_back(message_id);
        self.seen_set.insert(message_id);

        // Record channel cooldown timestamp in bounded map.
        if !self.channel_last_sent.contains_key(&channel_id) {
            if self.tracked_channels_len() >= self.max_channels
                && let Some(evicted_ch) = self.channel_order.pop_front()
            {
                self.channel_last_sent.remove(&evicted_ch);
            }
            self.channel_order.push_back(channel_id);
        }
        self.channel_last_sent.insert(channel_id, now);
        self.burst_timestamps.push_back(now);

        Ok(build_toast_payload(candidate, prefs))
    }

    /// Resets all remembered message IDs and rate-limit state (e.g. on logout).
    pub fn clear(&mut self) {
        self.seen_order.clear();
        self.seen_set.clear();
        self.channel_order.clear();
        self.channel_last_sent.clear();
        self.burst_timestamps.clear();
    }

    pub fn seen_len(&self) -> usize {
        self.seen_set.len()
    }

    pub fn tracked_channels_len(&self) -> usize {
        self.channel_last_sent.len()
    }
}

/// High-level desktop notification dispatcher owned by the home screen.
pub struct DesktopNotifier {
    prefs: NotificationPreferences,
    gate: NotificationGate,
    click_tx: UnboundedSender<NotificationTarget>,
    backend: backend::PlatformNotifier,
    last_error: Option<String>,
}

impl DesktopNotifier {
    /// Creates a new desktop notifier and returns both the dispatcher and the
    /// stream of click events emitted when the user clicks a toast.
    pub fn new(prefs: NotificationPreferences) -> (Self, UnboundedReceiver<NotificationTarget>) {
        let (click_tx, click_rx) = mpsc::unbounded();
        let mut backend = backend::PlatformNotifier::new();
        let mut last_error = None;
        if prefs.enabled
            && let Err(err) = backend.ensure_opt_in_ready()
        {
            last_error = Some(err);
        }
        (
            Self {
                prefs,
                gate: NotificationGate::default(),
                click_tx,
                backend,
                last_error,
            },
            click_rx,
        )
    }

    /// Whether the current platform supports native desktop toast notifications.
    pub fn is_supported(&self) -> bool {
        self.backend.is_supported()
    }

    pub fn preferences(&self) -> NotificationPreferences {
        self.prefs
    }

    /// Most recent platform/AUMID error encountered while enabling or showing
    /// desktop notifications, surfaced visibly in the UI.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn set_preferences(&mut self, prefs: NotificationPreferences) {
        let was_enabled = self.prefs.enabled;
        self.prefs = prefs;
        if prefs.enabled {
            match self.backend.ensure_opt_in_ready() {
                Ok(()) => self.last_error = None,
                Err(err) => self.last_error = Some(err),
            }
        } else {
            self.last_error = None;
            if was_enabled {
                self.backend.clear();
            }
        }
    }

    /// Evaluates `candidate` against user preferences, mute/DND/block/focus
    /// rules, and the bounded deduplication/rate-limit gate. Shows a native OS
    /// toast only when all checks pass.
    pub fn notify(&mut self, candidate: &NotificationCandidate) -> Result<(), SuppressionReason> {
        let payload = self.gate.evaluate(candidate, self.prefs, Instant::now())?;
        if let Err(err) = self.backend.show(&payload, self.click_tx.clone()) {
            self.last_error = Some(err.clone());
            return Err(SuppressionReason::PlatformError(err));
        }
        self.last_error = None;
        Ok(())
    }

    /// Clears active toasts and resets deduplication state on session end / logout.
    pub fn clear_on_logout(&mut self) {
        self.gate.clear();
        self.backend.clear();
        self.last_error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_candidate() -> NotificationCandidate {
        NotificationCandidate {
            target: NotificationTarget {
                guild_id: Some(Id::new(100)),
                channel_id: Id::new(200),
                message_id: Id::new(300),
            },
            author_id: Id::new(400),
            author_name: "Ada Lovelace".to_string(),
            channel_name: "general".to_string(),
            guild_name: Some("Analytical Engine".to_string()),
            content: "Hello @babbage, check the latest commits!".to_string(),
            has_attachments_or_embeds: false,
            is_mention: true,
            self_user_id: Some(Id::new(999)),
            is_dnd: false,
            channel_muted: false,
            guild_muted: false,
            author_blocked: false,
            window_active: false,
            channel_visible_at_bottom: false,
        }
    }

    fn enabled_prefs() -> NotificationPreferences {
        NotificationPreferences {
            enabled: true,
            show_content: true,
            only_mentions_in_guilds: true,
        }
    }

    #[test]
    fn launch_arg_round_trips_for_guild_and_dm_targets() {
        let guild_target = NotificationTarget {
            guild_id: Some(Id::new(123)),
            channel_id: Id::new(456),
            message_id: Id::new(789),
        };
        let encoded = guild_target.encode_launch_arg();
        assert_eq!(
            NotificationTarget::decode_launch_arg(&encoded),
            Some(guild_target)
        );

        let dm_target = NotificationTarget {
            guild_id: None,
            channel_id: Id::new(555),
            message_id: Id::new(666),
        };
        let encoded_dm = dm_target.encode_launch_arg();
        assert_eq!(
            NotificationTarget::decode_launch_arg(&encoded_dm),
            Some(dm_target)
        );
    }

    #[test]
    fn filter_respects_opt_in_dnd_mutes_own_blocked_and_focus() {
        let base = sample_candidate();

        // Opt-in disabled by default.
        assert_eq!(
            evaluate_filter(&base, NotificationPreferences::default()),
            Err(SuppressionReason::Disabled)
        );

        let prefs = enabled_prefs();
        assert_eq!(evaluate_filter(&base, prefs), Ok(()));

        // Own message.
        let mut own = base.clone();
        own.author_id = Id::new(999);
        assert_eq!(
            evaluate_filter(&own, prefs),
            Err(SuppressionReason::OwnMessage)
        );

        // Do Not Disturb.
        let mut dnd = base.clone();
        dnd.is_dnd = true;
        assert_eq!(
            evaluate_filter(&dnd, prefs),
            Err(SuppressionReason::DoNotDisturb)
        );

        // Blocked user (Feature 7 integration).
        let mut blocked = base.clone();
        blocked.author_blocked = true;
        assert_eq!(
            evaluate_filter(&blocked, prefs),
            Err(SuppressionReason::BlockedUser)
        );

        // Guild muted & channel muted.
        let mut guild_muted = base.clone();
        guild_muted.guild_muted = true;
        assert_eq!(
            evaluate_filter(&guild_muted, prefs),
            Err(SuppressionReason::GuildMuted)
        );

        let mut ch_muted = base.clone();
        ch_muted.channel_muted = true;
        assert_eq!(
            evaluate_filter(&ch_muted, prefs),
            Err(SuppressionReason::ChannelMuted)
        );

        // Focused visible channel at bottom suppresses toast; unfocused window does not.
        let mut focused = base.clone();
        focused.window_active = true;
        focused.channel_visible_at_bottom = true;
        assert_eq!(
            evaluate_filter(&focused, prefs),
            Err(SuppressionReason::ChannelFocusedAndVisible)
        );

        focused.window_active = false;
        assert_eq!(evaluate_filter(&focused, prefs), Ok(()));

        // Guild non-mention when only_mentions_in_guilds is true.
        let mut non_mention = base.clone();
        non_mention.is_mention = false;
        assert_eq!(
            evaluate_filter(&non_mention, prefs),
            Err(SuppressionReason::NotMentionInGuild)
        );

        // DM non-mention is still notified.
        let mut dm = non_mention;
        dm.target.guild_id = None;
        assert_eq!(evaluate_filter(&dm, prefs), Ok(()));
    }

    #[test]
    fn privacy_option_redacts_lockscreen_and_preview_content() {
        let candidate = sample_candidate();
        let private_prefs = NotificationPreferences {
            enabled: true,
            show_content: false,
            only_mentions_in_guilds: true,
        };
        let payload = build_toast_payload(&candidate, private_prefs);
        assert_eq!(payload.title, "Oxidecord");
        assert_eq!(payload.body, "You were mentioned in a message");
        assert_eq!(payload.attribution, None);
        assert!(!payload.allow_lockscreen_content);
        assert!(!payload.body.contains("babbage"));
    }

    #[test]
    fn gate_enforces_dedup_cooldown_burst_bounds_and_logout_clear() {
        let mut gate =
            NotificationGate::new(3, 2, Duration::from_secs(2), Duration::from_secs(10), 4);
        let prefs = enabled_prefs();
        let t0 = Instant::now();

        let mut c1 = sample_candidate();
        c1.target.message_id = Id::new(1);
        c1.target.channel_id = Id::new(10);
        assert!(gate.evaluate(&c1, prefs, t0).is_ok());

        // Duplicate message ID is rejected even after cooldown.
        assert_eq!(
            gate.evaluate(&c1, prefs, t0 + Duration::from_secs(5)),
            Err(SuppressionReason::DuplicateMessage)
        );

        // Same channel within cooldown is rejected.
        let mut c2 = sample_candidate();
        c2.target.message_id = Id::new(2);
        c2.target.channel_id = Id::new(10);
        assert_eq!(
            gate.evaluate(&c2, prefs, t0 + Duration::from_millis(500)),
            Err(SuppressionReason::RateLimited)
        );

        // Different channels within burst window succeed up to max_burst (4),
        // while respecting bounded seen_len (3) and tracked_channels_len (2).
        for i in 2..=4 {
            let mut ci = sample_candidate();
            ci.target.message_id = Id::new(i);
            ci.target.channel_id = Id::new(10 + i);
            assert!(
                gate.evaluate(&ci, prefs, t0 + Duration::from_millis(100 * i))
                    .is_ok()
            );
        }
        assert_eq!(gate.seen_len(), 3);
        assert_eq!(gate.tracked_channels_len(), 2);

        // 5th toast within the 10s burst window is rate-limited.
        let mut c5 = sample_candidate();
        c5.target.message_id = Id::new(5);
        c5.target.channel_id = Id::new(99);
        assert_eq!(
            gate.evaluate(&c5, prefs, t0 + Duration::from_secs(1)),
            Err(SuppressionReason::RateLimited)
        );

        // Logout clear resets all state.
        gate.clear();
        assert_eq!(gate.seen_len(), 0);
        assert_eq!(gate.tracked_channels_len(), 0);
        assert!(
            gate.evaluate(&c1, prefs, t0 + Duration::from_secs(1))
                .is_ok()
        );
    }
}
