//! Slack's emoji names to the characters a terminal draws. Slack names
//! its standard emoji the way gemoji does, mostly; where it does not, the
//! Unicode name usually matches (`thinking_face`, `star-struck`), and a
//! few are listed. A workspace's custom emoji have no character and stay
//! `:name:`.

use std::collections::HashMap;
use std::sync::OnceLock;

use emojis::{Emoji, SkinTone};

/// Slack names that neither gemoji nor the Unicode name know.
const ALIASES: &[(&str, &str)] = &[
    ("hugging_face", "hugs"),
    ("robot_face", "robot"),
    ("the_horns", "metal"),
    ("white_frowning_face", "frowning_face"),
    ("simple_smile", "slightly_smiling_face"),
];

/// The character of an emoji name, `+1::skin-tone-3` included; None for
/// a name it does not know, a custom emoji most likely.
pub fn glyph(name: &str) -> Option<&'static str> {
    let (base, tone) = match name.split_once("::skin-tone-") {
        Some((base, tone)) => (base, skin_tone(tone)),
        None => (name, None),
    };
    let emoji = lookup(base)?;
    Some(
        tone.and_then(|tone| emoji.with_skin_tone(tone))
            .unwrap_or(emoji)
            .as_str(),
    )
}

/// What to draw for an emoji name: its character, or `:name:`.
pub fn label(name: &str) -> String {
    glyph(name).map_or_else(|| format!(":{name}:"), str::to_string)
}

/// Whether a typed part picks out a name: it begins the name or one of
/// its words, as `check` finds `white_check_mark`.
pub fn begins(name: &str, typed: &str) -> bool {
    name.starts_with(typed) || name.split(['_', '-']).any(|word| word.starts_with(typed))
}

/// The standard emoji names the typed part `begins`, in gemoji's order,
/// which keeps the faces first.
pub fn matching(typed: &str) -> impl Iterator<Item = &'static str> {
    emojis::iter()
        .flat_map(Emoji::shortcodes)
        .filter(move |code| begins(code, typed))
}

/// What a typed name means: `:+1:` and `+1` are the same.
pub fn normalize(typed: &str) -> String {
    typed.trim().trim_matches(':').to_lowercase()
}

/// The Slack name of what was typed: a name, or the character itself
/// when it has one, `👍🏻` as `+1::skin-tone-2`.
pub fn name_of(typed: &str) -> String {
    let Some(emoji) = emojis::get(typed.trim()) else {
        return normalize(typed);
    };
    let tone = emoji.skin_tone().and_then(tone_number);
    let plain = emoji.with_skin_tone(SkinTone::Default).unwrap_or(emoji);
    match (plain.shortcode(), tone) {
        (Some(code), Some(tone)) => format!("{code}::skin-tone-{tone}"),
        (Some(code), None) => code.to_string(),
        (None, _) => normalize(typed),
    }
}

/// Whether two names are one emoji, as `thumbsup` is Slack's `+1`.
pub fn same(a: &str, b: &str) -> bool {
    a == b || glyph(a).is_some_and(|drawn| glyph(b) == Some(drawn))
}

/// The other names to try when Slack refuses one: the names come from
/// gemoji, and Slack calls a few otherwise (`thinking` is
/// `thinking_face`), its alias first, then the Unicode name.
pub fn other_names(name: &str) -> Vec<String> {
    let (base, tone) = match name.split_once("::skin-tone-") {
        Some((base, tone)) => (base, Some(tone)),
        None => (name, None),
    };
    let Some(emoji) = lookup(base) else {
        return Vec::new();
    };
    let aliases = ALIASES
        .iter()
        .filter(|(_, code)| emoji.shortcodes().any(|known| known == *code))
        .map(|(alias, _)| (*alias).to_string());
    let mut names: Vec<String> = Vec::new();
    for other in aliases
        .chain(std::iter::once(emoji.name().replace(' ', "_")))
        .chain(emoji.shortcodes().map(str::to_string))
    {
        if other != base && !names.contains(&other) {
            names.push(other);
        }
    }
    match tone {
        Some(tone) => names
            .into_iter()
            .map(|other| format!("{other}::skin-tone-{tone}"))
            .collect(),
        None => names,
    }
}

fn lookup(name: &str) -> Option<&'static Emoji> {
    let name = ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, code)| code);
    let known = |name: &str| {
        emojis::get_by_shortcode(name).or_else(|| by_unicode_name().get(name).copied())
    };
    known(name).or_else(|| {
        // Slack writes the gendered ones `man-shrugging`, `male-detective`.
        let name = name.replace('-', "_");
        let gendered = match name.split_once('_') {
            Some(("male", rest)) => Some(format!("man_{rest}")),
            Some(("female", rest)) => Some(format!("woman_{rest}")),
            _ => None,
        };
        known(&name).or_else(|| known(&gendered?))
    })
}

/// The emoji by their Unicode name written the Slack way, `thinking_face`.
fn by_unicode_name() -> &'static HashMap<String, &'static Emoji> {
    static NAMES: OnceLock<HashMap<String, &'static Emoji>> = OnceLock::new();
    NAMES.get_or_init(|| {
        emojis::iter()
            .map(|emoji| (emoji.name().replace(' ', "_"), emoji))
            .collect()
    })
}

fn tone_number(tone: SkinTone) -> Option<u8> {
    match tone {
        SkinTone::Light => Some(2),
        SkinTone::MediumLight => Some(3),
        SkinTone::Medium => Some(4),
        SkinTone::MediumDark => Some(5),
        SkinTone::Dark => Some(6),
        _ => None,
    }
}

fn skin_tone(tone: &str) -> Option<SkinTone> {
    match tone {
        "2" => Some(SkinTone::Light),
        "3" => Some(SkinTone::MediumLight),
        "4" => Some(SkinTone::Medium),
        "5" => Some(SkinTone::MediumDark),
        "6" => Some(SkinTone::Dark),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_names_draw_as_characters() {
        assert_eq!(glyph("+1"), Some("👍"));
        assert_eq!(glyph("white_check_mark"), Some("✅"));
        assert_eq!(glyph("thinking_face"), Some("🤔"));
        assert_eq!(glyph("face_with_rolling_eyes"), Some("🙄"));
        assert_eq!(glyph("hugging_face"), Some("🤗"));
        assert_eq!(glyph("man-shrugging"), Some("🤷‍♂️"));
        assert_eq!(glyph("male-police-officer"), Some("👮‍♂️"));
        assert_eq!(glyph("female-technologist"), Some("👩‍💻"));
        assert_eq!(glyph("+1::skin-tone-2"), Some("👍🏻"));
        // A tone the emoji cannot take keeps the plain one.
        assert_eq!(glyph("eyes::skin-tone-4"), Some("👀"));
    }

    #[test]
    fn a_custom_emoji_keeps_its_name() {
        assert_eq!(glyph("yolo"), None);
        assert_eq!(label("yolo"), ":yolo:");
        assert_eq!(label("tada"), "🎉");
    }

    #[test]
    fn typed_names_are_completed_and_normalized() {
        assert!(matching("thumbs").any(|code| code == "thumbsup"));
        assert!(matching("+").any(|code| code == "+1"));
        assert!(matching("check").any(|code| code == "white_check_mark"));
        assert!(!begins("white_check_mark", "heck"));
        assert_eq!(normalize(" :Tada: "), "tada");
        assert_eq!(name_of("👍"), "+1");
        assert_eq!(name_of(":yolo:"), "yolo");
        assert_eq!(name_of("👍🏻"), "+1::skin-tone-2");
    }

    #[test]
    fn one_emoji_under_several_names() {
        assert!(same("+1", "thumbsup"));
        assert!(same("hugging_face", "hugs"));
        assert!(same("yolo", "yolo"));
        assert!(!same("+1", "+1::skin-tone-2"));
        assert!(!same("yolo", "party_parrot"), "two custom emoji");
    }

    #[test]
    fn a_refused_name_has_slack_names_to_try() {
        assert_eq!(other_names("hugs")[0], "hugging_face");
        assert!(other_names("thinking").contains(&"thinking_face".to_string()));
        assert!(other_names("+1::skin-tone-3").contains(&"thumbsup::skin-tone-3".to_string()));
        assert!(!other_names("+1").contains(&"+1".to_string()));
        assert!(other_names("yolo").is_empty());
    }
}
