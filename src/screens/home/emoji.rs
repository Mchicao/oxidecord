//! The emoji picker's contents, and turning the `:name:` it inserts for a
//! custom emoji into the `<:name:id>` Discord renders.
//!
//! Which custom emoji the user may send is decided by the caller; this module
//! only lays out and searches what it's handed.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use twilight_model::id::{Id, marker::GuildMarker};

use crate::discord::GuildEmoji;
use crate::platform::prefs::RecentEmoji;

/// Emoji per picker row.
pub(in crate::screens::home) const COLUMNS: usize = 9;

/// How many recently used emoji are kept: three full rows.
pub(in crate::screens::home) const RECENT_LIMIT: usize = COLUMNS * 3;

/// The newest emoji version Windows' Segoe UI Emoji draws. Anything later would
/// show as tofu, so it's left out of the picker rather than offered broken.
const NEWEST_EMOJI: emojis::EmojiVersion = emojis::EmojiVersion::new(15, 0);

#[derive(Clone)]
pub(in crate::screens::home) enum PickerEmoji {
    Unicode(&'static emojis::Emoji),
    Custom {
        emoji: GuildEmoji,
        /// Why it can't be sent here, for one that's shown but locked.
        locked: Option<&'static str>,
    },
}

impl PickerEmoji {
    /// The `:name:` shown under the picker for the hovered emoji.
    pub fn label(&self) -> String {
        match self {
            Self::Unicode(emoji) => format!(":{}:", emoji.shortcode().unwrap_or(emoji.name())),
            Self::Custom { emoji, .. } => format!(":{}:", emoji.name),
        }
    }

    /// How it's remembered among the recently used.
    pub fn recent_key(&self) -> RecentEmoji {
        match self {
            Self::Unicode(emoji) => RecentEmoji::Unicode(emoji.as_str().to_string()),
            Self::Custom { emoji, .. } => RecentEmoji::Custom(emoji.id.get()),
        }
    }
}

/// Which run of emoji a section is, and what stands for it in the picker's
/// category rail. Also how a collapsed section is remembered.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::screens::home) enum SectionKind {
    Recent,
    Guild(Id<GuildMarker>),
    Unicode(emojis::Group),
}

/// A titled run of emoji: one guild's, or one unicode category.
pub(in crate::screens::home) struct PickerSection {
    pub kind: SectionKind,
    pub title: String,
    /// A guild's icon, drawn in the rail and beside the header.
    pub icon: Option<String>,
    pub emojis: Vec<PickerEmoji>,
}

/// A section as laid out: its header, and the list line that header is on.
pub(in crate::screens::home) struct SectionHeader {
    pub kind: SectionKind,
    pub title: String,
    pub icon: Option<String>,
    pub first_row: usize,
    pub collapsed: bool,
}

/// A line of the picker's list. Headers take a whole line so the list stays
/// uniform in height and can be virtualized.
pub(in crate::screens::home) enum PickerRow {
    /// Index into [`PickerLayout::sections`].
    Header(usize),
    Emojis(Vec<PickerEmoji>),
}

#[derive(Default)]
pub(in crate::screens::home) struct PickerLayout {
    pub sections: Vec<SectionHeader>,
    pub rows: Vec<PickerRow>,
}

impl PickerLayout {
    /// The section showing at list line `row`: the last to start at or before it.
    pub fn section_at(&self, row: usize) -> Option<usize> {
        self.sections
            .iter()
            .rposition(|section| section.first_row <= row)
    }
}

/// The unicode categories, filtered to what the system font can draw. Built
/// once: the emoji crate's table is static, so this never changes.
#[allow(clippy::type_complexity)]
static UNICODE: LazyLock<Vec<(emojis::Group, &'static str, Vec<&'static emojis::Emoji>)>> =
    LazyLock::new(|| {
        emojis::Group::iter()
            .map(|group| {
                let emojis = group
                    .emojis()
                    .filter(|emoji| emoji.emoji_version() <= NEWEST_EMOJI)
                    .collect();
                (group, group_title(group), emojis)
            })
            .collect()
    });

/// Every unicode emoji the system font can draw, in picker order.
pub(in crate::screens::home) fn unicode_emojis() -> impl Iterator<Item = &'static emojis::Emoji> {
    UNICODE
        .iter()
        .flat_map(|(_, _, emojis)| emojis.iter().copied())
}

fn group_title(group: emojis::Group) -> &'static str {
    use emojis::Group;
    match group {
        Group::SmileysAndEmotion => "Smileys & Emotion",
        Group::PeopleAndBody => "People & Body",
        Group::AnimalsAndNature => "Animals & Nature",
        Group::FoodAndDrink => "Food & Drink",
        Group::TravelAndPlaces => "Travel & Places",
        Group::Activities => "Activities",
        Group::Objects => "Objects",
        Group::Symbols => "Symbols",
        Group::Flags => "Flags",
    }
}

/// The picker's lines: the recently used, the custom sections handed in, then
/// every unicode category, all narrowed to `query` (matched against names and
/// shortcodes, ignoring case). Sections left empty by the search are dropped,
/// and the recently used are left out of a search, which would only repeat
/// them. A `collapsed` section keeps only its header, except during a search,
/// where that would hide matches behind a header that says nothing of them.
pub(in crate::screens::home) fn picker_rows(
    recent: Vec<PickerEmoji>,
    custom: Vec<PickerSection>,
    query: &str,
    collapsed: &HashSet<SectionKind>,
) -> PickerLayout {
    let query = query.trim().to_lowercase();
    let recent = query.is_empty().then(|| PickerSection {
        kind: SectionKind::Recent,
        title: "Recently Used".into(),
        icon: None,
        emojis: recent,
    });
    let unicode = UNICODE.iter().map(|(group, title, emojis)| PickerSection {
        kind: SectionKind::Unicode(*group),
        title: title.to_string(),
        icon: None,
        emojis: emojis
            .iter()
            .filter(|emoji| unicode_matches(emoji, &query))
            .map(|&emoji| PickerEmoji::Unicode(emoji))
            .collect(),
    });

    let mut layout = PickerLayout::default();
    for mut section in recent.into_iter().chain(custom).chain(unicode) {
        if !query.is_empty() {
            section.emojis.retain(|emoji| match emoji {
                PickerEmoji::Custom { emoji, .. } => emoji.name.to_lowercase().contains(&query),
                // Already filtered above.
                PickerEmoji::Unicode(_) => true,
            });
        }
        if section.emojis.is_empty() {
            continue;
        }
        let is_collapsed = query.is_empty() && collapsed.contains(&section.kind);
        layout.rows.push(PickerRow::Header(layout.sections.len()));
        layout.sections.push(SectionHeader {
            kind: section.kind,
            title: section.title,
            icon: section.icon,
            first_row: layout.rows.len() - 1,
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            layout.rows.extend(
                section
                    .emojis
                    .chunks(COLUMNS)
                    .map(|chunk| PickerRow::Emojis(chunk.to_vec())),
            );
        }
    }
    layout
}

fn unicode_matches(emoji: &emojis::Emoji, query: &str) -> bool {
    query.is_empty()
        || emoji.name().contains(query)
        || emoji.shortcodes().any(|code| code.contains(query))
}

/// Custom emoji the user may send here, by name, for [`resolve_custom_emoji`].
/// The first of a name wins, so callers list the open guild's own first.
pub(in crate::screens::home) fn emoji_names<'a>(
    usable: impl IntoIterator<Item = &'a GuildEmoji>,
) -> HashMap<&'a str, &'a GuildEmoji> {
    let mut names = HashMap::new();
    for emoji in usable {
        names.entry(emoji.name.as_str()).or_insert(emoji);
    }
    names
}

/// Replaces each `:name:` naming a custom emoji in `names` with the markup
/// Discord renders it from, which is what its own client does on send. Code
/// spans and blocks are left alone, as is markup that's already resolved.
pub(in crate::screens::home) fn resolve_custom_emoji(
    text: &str,
    names: &HashMap<&str, &GuildEmoji>,
) -> String {
    if names.is_empty() {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                // A run of n backticks is closed by the next run of exactly n.
                let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                i += run;
                if let Some(end) = closing_backticks(&bytes[i..], run) {
                    i += end + run;
                }
            }
            b'<' => {
                // `<:name:id>` and `<a:name:id>` are already emoji; skipping
                // them keeps their inner `:name:` from being resolved again.
                i += 1 + existing_markup_len(&bytes[i + 1..]).unwrap_or(0);
            }
            b':' => {
                let len = bytes[i + 1..]
                    .iter()
                    .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_')
                    .count();
                let close = i + 1 + len;
                let emoji = (len >= 2 && bytes.get(close) == Some(&b':'))
                    .then(|| names.get(&text[i + 1..close]))
                    .flatten();
                match emoji {
                    Some(emoji) => {
                        out.push_str(&text[copied..i]);
                        out.push_str(&emoji.markdown());
                        i = close + 1;
                        copied = i;
                    }
                    // The colon may yet open a name that follows it.
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Where a run of exactly `run` backticks starts in `bytes`.
fn closing_backticks(bytes: &[u8], run: usize) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if len == run {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// The length of `a:name:id>` or `:name:id>` at the start of `bytes`.
fn existing_markup_len(bytes: &[u8]) -> Option<usize> {
    let mut i = usize::from(bytes.first() == Some(&b'a'));
    (bytes.get(i) == Some(&b':')).then_some(())?;
    i += 1;
    let name = bytes[i..]
        .iter()
        .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_')
        .count();
    i += name;
    (name > 0 && bytes.get(i) == Some(&b':')).then_some(())?;
    i += 1;
    let id = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
    i += id;
    (id > 0 && bytes.get(i) == Some(&b'>')).then_some(i + 1)
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    fn emoji(id: u64, name: &str, animated: bool) -> GuildEmoji {
        GuildEmoji {
            id: Id::new(id),
            name: name.to_string(),
            animated,
            available: true,
            roles: Vec::new(),
        }
    }

    fn resolve(text: &str) -> String {
        let emojis = [emoji(1, "wave", false), emoji(2, "party", true)];
        resolve_custom_emoji(text, &emoji_names(&emojis))
    }

    #[test]
    fn resolves_known_names() {
        assert_eq!(resolve("hi :wave:"), "hi <:wave:1>");
        assert_eq!(resolve(":wave::party:"), "<:wave:1><a:party:2>");
        assert_eq!(resolve("a: :wave:"), "a: <:wave:1>");
    }

    #[test]
    fn leaves_unknown_names_and_code_alone() {
        assert_eq!(resolve(":nope: 12:30"), ":nope: 12:30");
        assert_eq!(resolve("`:wave:` :wave:"), "`:wave:` <:wave:1>");
        assert_eq!(resolve("```\n:wave:\n```"), "```\n:wave:\n```");
        assert_eq!(resolve("`` ` :wave: ``"), "`` ` :wave: ``");
    }

    #[test]
    fn leaves_existing_markup_alone() {
        assert_eq!(resolve("<:wave:99> :wave:"), "<:wave:99> <:wave:1>");
        assert_eq!(resolve("<a:wave:99>"), "<a:wave:99>");
    }

    #[test]
    fn first_name_wins() {
        let emojis = [emoji(1, "wave", false), emoji(2, "wave", false)];
        let names = emoji_names(&emojis);
        assert_eq!(resolve_custom_emoji(":wave:", &names), "<:wave:1>");
    }

    #[test]
    fn search_drops_empty_sections() {
        let custom = vec![PickerSection {
            kind: SectionKind::Guild(Id::new(1)),
            title: "Guild".into(),
            icon: None,
            emojis: vec![PickerEmoji::Custom {
                emoji: emoji(1, "wave", false),
                locked: None,
            }],
        }];
        let layout = picker_rows(Vec::new(), custom, "WAVE", &HashSet::new());
        assert!(matches!(layout.rows[0], PickerRow::Header(0)));
        assert_eq!(layout.sections[0].title, "Guild");
        // The unicode waving hand matches too.
        assert!(layout.rows.len() > 2);
        let none = picker_rows(Vec::new(), Vec::new(), "zzzzqqq", &HashSet::new());
        assert!(none.rows.is_empty());
    }

    #[test]
    fn collapsed_sections_keep_their_header() {
        let smileys = SectionKind::Unicode(emojis::Group::SmileysAndEmotion);
        let layout = picker_rows(Vec::new(), Vec::new(), "", &HashSet::from([smileys]));
        assert!(layout.sections[0].collapsed);
        assert_eq!(layout.sections[1].first_row, 1);
        assert_eq!(layout.section_at(0), Some(0));
        assert_eq!(layout.section_at(2), Some(1));
        // A search shows its matches regardless.
        let search = picker_rows(Vec::new(), Vec::new(), "grin", &HashSet::from([smileys]));
        assert!(!search.sections[0].collapsed);
    }
}
