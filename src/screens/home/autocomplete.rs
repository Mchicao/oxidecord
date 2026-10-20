//! The composer's autocomplete: spotting the `@`, `#` or `:` word being typed
//! at the cursor, ranking what could finish it, and turning the mentions it
//! put in back into the markup Discord expects when the message is sent.
//!
//! What's on offer is gathered by the caller; this module only matches and
//! orders it.

use std::ops::Range;

use twilight_model::id::{
    Id,
    marker::{ChannelMarker, UserMarker},
};

use super::emoji::PickerEmoji;

/// How many suggestions are offered at once, as in Discord.
pub(in crate::screens::home) const LIMIT: usize = 10;

/// Emoji suggestions wait for this many characters after the colon, so a
/// colon in ordinary prose doesn't open the list, as in Discord.
const EMOJI_MIN_QUERY: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::screens::home) enum Trigger {
    User,
    Channel,
    Emoji,
}

/// The word being completed: from its trigger character up to the cursor.
#[derive(PartialEq, Eq, Debug)]
pub(in crate::screens::home) struct Word<'a> {
    pub trigger: Trigger,
    /// Byte range in the text, trigger included.
    pub range: Range<usize>,
    /// What follows the trigger.
    pub query: &'a str,
}

#[derive(Clone)]
pub(in crate::screens::home) enum Suggestion {
    User {
        id: Id<UserMarker>,
        name: String,
        username: Option<String>,
        avatar_url: Option<String>,
    },
    Channel {
        id: Id<ChannelMarker>,
        name: String,
        kind: crate::discord::ChannelKind,
        /// The category it's filed under, shown beside it as in Discord.
        category: Option<String>,
    },
    Broadcast(Broadcast),
    Emoji {
        emoji: PickerEmoji,
        /// The guild a custom emoji is from.
        guild: Option<String>,
    },
}

impl Suggestion {
    /// What tells one suggestion from another, so the highlighted one can stay
    /// highlighted as the list is rebuilt around it.
    pub fn key(&self) -> String {
        match self {
            Self::User { id, .. } => format!("user-{id}"),
            Self::Channel { id, .. } => format!("channel-{id}"),
            Self::Broadcast(broadcast) => broadcast.text().to_string(),
            Self::Emoji { emoji, .. } => format!("emoji-{:?}", emoji.recent_key()),
        }
    }
}

/// `@everyone` and `@here`. Sent as written: Discord reads them from the text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::screens::home) enum Broadcast {
    Everyone,
    Here,
}

impl Broadcast {
    pub const ALL: [Self; 2] = [Self::Everyone, Self::Here];

    pub fn text(self) -> &'static str {
        match self {
            Self::Everyone => "@everyone",
            Self::Here => "@here",
        }
    }

    /// Discord's wording for what it does.
    pub fn description(self) -> &'static str {
        match self {
            Self::Everyone => "Notify everyone who has permission to view this channel.",
            Self::Here => "Notify everyone online who has permission to view this channel.",
        }
    }
}

/// The trigger word ending at `cursor`, if there is one. It has to start the
/// text or follow whitespace, so an email address doesn't count as a mention.
pub(in crate::screens::home) fn word_at(text: &str, cursor: usize) -> Option<Word<'_>> {
    let before = text.get(..cursor)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let word = &before[start..];
    let mut chars = word.chars();
    let trigger = match chars.next()? {
        '@' => Trigger::User,
        '#' => Trigger::Channel,
        ':' => Trigger::Emoji,
        _ => return None,
    };
    let query = chars.as_str();
    if trigger == Trigger::Emoji && (query.chars().count() < EMOJI_MIN_QUERY || query.contains(':'))
    {
        return None;
    }
    Some(Word {
        trigger,
        range: start..cursor,
        query,
    })
}

/// How well `name` matches a lowercased `query`, lower being better: names
/// starting with it, then ones with a later word starting with it, then any
/// containing it. Ties go to the shorter name, the likelier finished. `None`
/// if it doesn't match at all.
pub(in crate::screens::home) fn match_rank(name: &str, query: &str) -> Option<(u8, usize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    let name = name.to_lowercase();
    let rank = if name.starts_with(query) {
        0
    } else if name
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(query))
    {
        1
    } else if name.contains(query) {
        2
    } else {
        return None;
    };
    Some((rank, name.len()))
}

/// The best [`LIMIT`] of the ranked candidates, best first. The sort is
/// stable, so equal matches keep the order they were gathered in.
pub(in crate::screens::home) fn best<T>(
    ranked: impl IntoIterator<Item = ((u8, usize), T)>,
) -> Vec<T> {
    let mut ranked: Vec<_> = ranked.into_iter().collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked
        .into_iter()
        .take(LIMIT)
        .map(|(_, item)| item)
        .collect()
}

/// `text` with each mention the autocomplete put in, as `(shown, markup)`,
/// turned into its markup. Longer names go first, so `@Sam` can't eat the
/// front of an `@Samantha` chosen alongside it.
pub(in crate::screens::home) fn resolve_mentions(
    text: &str,
    mentions: &[(String, String)],
) -> String {
    let mut mentions: Vec<_> = mentions.iter().collect();
    mentions.sort_by_key(|(shown, _)| std::cmp::Reverse(shown.len()));
    let mut text = text.to_string();
    for (shown, markup) in mentions {
        text = text.replace(shown.as_str(), markup);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_word_at_the_cursor() {
        let word = word_at("hey @ali", 8).unwrap();
        assert_eq!(word.trigger, Trigger::User);
        assert_eq!(word.range, 4..8);
        assert_eq!(word.query, "ali");

        assert_eq!(word_at("#", 1).unwrap().trigger, Trigger::Channel);
        // Only up to the cursor.
        assert_eq!(word_at("@alice", 3).unwrap().query, "al");
        // Past the word, nothing's being completed.
        assert_eq!(word_at("@alice ", 7), None);
    }

    #[test]
    fn ignores_triggers_inside_a_word() {
        assert_eq!(word_at("me@example.com", 14), None);
        assert_eq!(word_at("issue#4", 7), None);
    }

    #[test]
    fn emoji_wait_for_two_characters() {
        assert_eq!(word_at("note: x", 7), None);
        assert_eq!(word_at(":s", 2), None);
        assert_eq!(word_at(":sm", 3).unwrap().query, "sm");
        // A finished `:name:` isn't completed again.
        assert_eq!(word_at(":smile:", 7), None);
    }

    #[test]
    fn handles_multibyte_text() {
        let text = "héllo\u{3000}@zoë";
        let word = word_at(text, text.len()).unwrap();
        assert_eq!(word.query, "zoë");
    }

    #[test]
    fn ranks_prefixes_first() {
        let names = ["xsam", "Samantha", "big sam", "Sam"];
        let ranked = best(
            names
                .iter()
                .filter_map(|name| Some((match_rank(name, "sam")?, *name))),
        );
        assert_eq!(ranked, ["Sam", "Samantha", "big sam", "xsam"]);
        assert_eq!(match_rank("Alice", "bob"), None);
    }

    #[test]
    fn resolves_longer_mentions_first() {
        let mentions = [
            ("@Sam".to_string(), "<@1>".to_string()),
            ("@Samantha".to_string(), "<@2>".to_string()),
            ("#general".to_string(), "<#3>".to_string()),
        ];
        assert_eq!(
            resolve_mentions("@Samantha and @Sam in #general", &mentions),
            "<@2> and <@1> in <#3>"
        );
    }
}
