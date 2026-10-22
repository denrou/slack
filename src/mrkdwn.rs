//! Slack's message markup to terminal text. Pure: names come in through
//! closures, and the text comes out as pieces, each with its look, which
//! the text view paints with `Style`, the screen with its own styles, and
//! the reply box with the markers still in sight. With styling off the
//! markers are left as written so nothing is lost.

use crate::style::Style;

/// A name for an id, without the `@` or `#` sigil; None when unknown.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What a piece of text stands for, besides its emphasis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Text,
    Name,
    Mention,
    Link,
}

/// How a piece of text is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Look {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Text to show, in its look.
    Text(String, Look),
    /// Markup: what was typed, and what a styled view shows in its place,
    /// a bar for a quote, a line break that sets a code block apart, or
    /// nothing.
    Marker { typed: String, shown: String },
}

pub fn render(text: &str, user: Lookup, channel: Lookup, style: &Style) -> String {
    let pieces = pieces(text, user, channel);
    if !style.enabled {
        return pieces
            .iter()
            .map(|piece| match piece {
                Piece::Text(text, _) => text.as_str(),
                Piece::Marker { typed, .. } => typed.as_str(),
            })
            .collect();
    }
    pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(text, look) => paint(text, *look, style),
            Piece::Marker { shown, .. } if shown.trim().is_empty() => shown.clone(),
            Piece::Marker { shown, .. } => style.dim(shown),
        })
        .collect()
}

/// The text as the screen shows it, without its styles: for a title, an
/// excerpt or a notification, where `*` and `_` would only be noise.
pub fn strip(text: &str, user: Lookup, channel: Lookup) -> String {
    pieces(text, user, channel)
        .iter()
        .map(|piece| match piece {
            Piece::Text(text, _) | Piece::Marker { shown: text, .. } => text.as_str(),
        })
        .collect()
}

/// A message's text: tokens expanded, entities decoded, markup read.
pub fn pieces(text: &str, user: Lookup, channel: Lookup) -> Vec<Piece> {
    Markup::new(tokens(text, user, channel)).read()
}

/// A draft as typed: the markup read, nothing else, since names are linked
/// on sending and nothing in it is escaped.
pub fn draft(text: &str) -> Vec<Piece> {
    Markup::new(text.chars().map(Atom::Char).collect()).read()
}

fn paint(text: &str, look: Look, style: &Style) -> String {
    let mut out = match look.kind {
        Kind::Text => text.to_string(),
        Kind::Name => style.name(text),
        Kind::Mention => style.mention(text),
        Kind::Link => style.link(text),
    };
    for (on, paint) in [
        (look.code, Style::code as fn(&Style, &str) -> String),
        (look.bold, Style::bold),
        (look.italic, Style::italic),
        (look.strike, Style::strike),
    ] {
        if on {
            out = paint(style, &out);
        }
    }
    out
}

/// A character of the text, or a token expanded whole: markup goes around
/// a mention or a link, never into it.
enum Atom {
    Char(char),
    Token(Vec<(String, Kind)>),
}

/// Replace Slack's `<…>` tokens: mentions, channel links, special
/// mentions, dates and URLs. Entities are decoded after the split, since a
/// literal `<` arrives as `&lt;`.
fn tokens(text: &str, user: Lookup, channel: Lookup) -> Vec<Atom> {
    fn push(atoms: &mut Vec<Atom>, text: &str) {
        atoms.extend(decode_entities(text).chars().map(Atom::Char));
    }
    let mut atoms = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        push(&mut atoms, &rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            push(&mut atoms, &rest[start..]);
            return atoms;
        };
        match expand_token(&after[..end], user, channel) {
            Some(parts) => atoms.push(Atom::Token(parts)),
            None => push(&mut atoms, &rest[start..start + 1 + end + 1]),
        }
        rest = &after[end + 1..];
    }
    push(&mut atoms, rest);
    atoms
}

fn expand_token(token: &str, user: Lookup, channel: Lookup) -> Option<Vec<(String, Kind)>> {
    let (target, label) = match token.split_once('|') {
        Some((target, label)) => (target, Some(label)),
        None => (token, None),
    };
    if let Some(id) = target.strip_prefix('@') {
        let name = user(id)
            .or_else(|| label.map(str::to_string))
            .unwrap_or_else(|| id.to_string());
        return Some(vec![(decode_entities(&format!("@{name}")), Kind::Name)]);
    }
    if let Some(id) = target.strip_prefix('#') {
        let name = label
            .filter(|label| !label.is_empty())
            .map(str::to_string)
            .or_else(|| channel(id))
            .unwrap_or_else(|| id.to_string());
        return Some(vec![(decode_entities(&format!("#{name}")), Kind::Name)]);
    }
    if let Some(special) = target.strip_prefix('!') {
        let text = match special.split('^').next().unwrap_or(special) {
            "here" | "channel" | "everyone" => {
                format!("@{}", special.split('^').next().unwrap_or(special))
            }
            "subteam" => label
                .map(|label| label.trim_start_matches('@'))
                .map_or_else(|| "@group".to_string(), |label| format!("@{label}")),
            "date" => label
                .map(str::to_string)
                .unwrap_or_else(|| special.to_string()),
            _ => label
                .map(str::to_string)
                .unwrap_or_else(|| format!("@{special}")),
        };
        return Some(vec![(decode_entities(&text), Kind::Mention)]);
    }
    if is_url(target) {
        let url = decode_entities(shown_url(target));
        return Some(
            match label.filter(|label| !label.is_empty() && !repeats_url(label, shown_url(target)))
            {
                Some(label) => vec![
                    (format!("{} (", decode_entities(label)), Kind::Text),
                    (url, Kind::Link),
                    (")".to_string(), Kind::Text),
                ],
                None => vec![(url, Kind::Link)],
            },
        );
    }
    None
}

/// The atoms of a text and what the markup makes of each.
struct Markup {
    atoms: Vec<Atom>,
    looks: Vec<Look>,
    /// Some for an atom that is markup, with what a styled view shows for
    /// it.
    markers: Vec<Option<&'static str>>,
}

impl Markup {
    fn new(atoms: Vec<Atom>) -> Markup {
        let len = atoms.len();
        Markup {
            atoms,
            looks: vec![Look::default(); len],
            markers: vec![None; len],
        }
    }

    /// Code first, since it shields what it holds from the other markers,
    /// then quotes, then bold, italic and strike, which may nest.
    fn read(mut self) -> Vec<Piece> {
        self.code_blocks();
        self.code_spans();
        self.quotes();
        self.pairs('*', |look| look.bold = true);
        self.pairs('_', |look| look.italic = true);
        self.pairs('~', |look| look.strike = true);
        self.into_pieces()
    }

    fn char(&self, at: usize) -> Option<char> {
        match self.atoms.get(at) {
            Some(Atom::Char(c)) => Some(*c),
            _ => None,
        }
    }

    /// `c` at `at`, and not already code or markup.
    fn is(&self, at: usize, c: char) -> bool {
        self.char(at) == Some(c) && !self.looks[at].code && self.markers[at].is_none()
    }

    /// A token reads as a word: it is a name or a link.
    fn is_word(&self, at: usize) -> bool {
        self.char(at).is_none_or(char::is_alphanumeric)
    }

    fn is_space(&self, at: usize) -> bool {
        self.char(at).is_some_and(char::is_whitespace)
    }

    fn line_starts_at(&self, at: usize) -> bool {
        at == 0 || self.char(at - 1) == Some('\n')
    }

    fn mark(&mut self, range: std::ops::Range<usize>, shown: &'static str) {
        let first = range.start;
        for at in range {
            self.markers[at] = Some(if at == first { shown } else { "" });
        }
    }

    fn fence_at(&self, at: usize) -> bool {
        (at..at + 3).all(|at| self.is(at, '`'))
    }

    /// A block between ``` fences, set on lines of its own; the line
    /// breaks right inside the fences belong to them.
    fn code_blocks(&mut self) {
        let len = self.atoms.len();
        let mut at = 0;
        while at + 3 <= len {
            let close = if self.fence_at(at) {
                (at + 3..=len - 3).find(|&close| self.fence_at(close))
            } else {
                None
            };
            let Some(close) = close else {
                at += 1;
                continue;
            };
            let mut from = at + 3;
            if self.char(from) == Some('\n') {
                from += 1;
            }
            let mut to = close;
            if to > from && self.char(to - 1) == Some('\n') {
                to -= 1;
            }
            let end = close + 3;
            let before = if self.line_starts_at(at) { "" } else { "\n" };
            let after = if end == len || self.char(end) == Some('\n') {
                ""
            } else {
                "\n"
            };
            self.mark(at..from, before);
            self.mark(to..end, after);
            for look in &mut self.looks[from..to] {
                look.code = true;
            }
            at = end;
        }
    }

    /// `code` within a line.
    fn code_spans(&mut self) {
        let len = self.atoms.len();
        let mut at = 0;
        while at < len {
            if self.is(at, '`')
                && let Some(close) = (at + 1..len)
                    .take_while(|&j| self.char(j) != Some('\n') && !self.looks[j].code)
                    .find(|&j| self.is(j, '`'))
                && close > at + 1
            {
                self.mark(at..at + 1, "");
                self.mark(close..close + 1, "");
                for look in &mut self.looks[at + 1..close] {
                    look.code = true;
                }
                at = close + 1;
            } else {
                at += 1;
            }
        }
    }

    /// `>` at the start of a line quotes it, shown as a bar.
    fn quotes(&mut self) {
        let len = self.atoms.len();
        for at in 0..len {
            if !(self.is(at, '>') && self.line_starts_at(at)) {
                continue;
            }
            let from = if self.char(at + 1) == Some(' ') {
                at + 2
            } else {
                at + 1
            };
            self.mark(at..from, "▎ ");
        }
    }

    /// `*bold*`, `_italic_`, `~strike~`, each on a word boundary so that
    /// `snake_case_names` and `2*3*4` stay as they are, and within a line.
    fn pairs(&mut self, marker: char, set: fn(&mut Look)) {
        let len = self.atoms.len();
        let mut at = 0;
        while at < len {
            if self.is(at, marker)
                && self.opens_at(at)
                && let Some(close) = (at + 2..len)
                    .take_while(|&j| self.char(j) != Some('\n'))
                    .find(|&j| self.is(j, marker) && self.closes_at(j))
            {
                self.mark(at..at + 1, "");
                self.mark(close..close + 1, "");
                self.looks[at + 1..close].iter_mut().for_each(set);
                at = close + 1;
            } else {
                at += 1;
            }
        }
    }

    fn opens_at(&self, at: usize) -> bool {
        let before = at == 0 || !self.is_word(at - 1);
        let after = at + 1 < self.atoms.len()
            && !self.is_space(at + 1)
            && self.char(at + 1) != self.char(at);
        before && after
    }

    fn closes_at(&self, at: usize) -> bool {
        let before = at > 0 && !self.is_space(at - 1) && self.char(at - 1) != self.char(at);
        let after = at + 1 >= self.atoms.len() || !self.is_word(at + 1);
        before && after
    }

    fn into_pieces(self) -> Vec<Piece> {
        fn text(pieces: &mut Vec<Piece>, text: &str, look: Look) {
            match pieces.last_mut() {
                Some(Piece::Text(last, last_look)) if *last_look == look => last.push_str(text),
                _ => pieces.push(Piece::Text(text.to_string(), look)),
            }
        }
        let mut pieces = Vec::new();
        let atoms = self.atoms.into_iter().zip(self.looks).zip(self.markers);
        for ((atom, look), marker) in atoms {
            match (atom, marker) {
                (Atom::Char(c), Some(shown)) => match pieces.last_mut() {
                    Some(Piece::Marker { typed, shown: last }) => {
                        typed.push(c);
                        last.push_str(shown);
                    }
                    _ => pieces.push(Piece::Marker {
                        typed: c.to_string(),
                        shown: shown.to_string(),
                    }),
                },
                (Atom::Char(c), None) => text(&mut pieces, c.encode_utf8(&mut [0; 4]), look),
                (Atom::Token(parts), _) => {
                    for (part, kind) in parts {
                        text(&mut pieces, &part, Look { kind, ..look });
                    }
                }
            }
        }
        pieces
    }
}

fn is_url(target: &str) -> bool {
    target.contains("://") || target.starts_with("mailto:") || target.starts_with("tel:")
}

fn is_openable(target: &str) -> bool {
    ["https://", "http://", "mailto:", "tel:"]
        .iter()
        .any(|scheme| target.starts_with(scheme))
}

/// A mail or phone link reads better without its scheme.
fn shown_url(target: &str) -> &str {
    target
        .strip_prefix("mailto:")
        .or_else(|| target.strip_prefix("tel:"))
        .unwrap_or(target)
}

/// A link of a message: what the opener gets, and the words it was
/// written under when they say more than the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub label: Option<String>,
}

/// The links of a message, in order, each URL once: Slack writes one as
/// `<url|label>` or `<url>`. Web, mail and phone links only: the system
/// opener takes those, and would hand any other scheme to whatever
/// claims it.
pub fn links(text: &str) -> Vec<Link> {
    let mut links: Vec<Link> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        let token = &after[..end];
        rest = &after[end + 1..];
        let (target, label) = match token.split_once('|') {
            Some((target, label)) => (target, Some(label)),
            None => (token, None),
        };
        if !is_openable(target) {
            continue;
        }
        let url = decode_entities(target);
        if links.iter().any(|link| link.url == url) {
            continue;
        }
        links.push(Link {
            url,
            label: label
                .filter(|label| !label.is_empty() && !repeats_url(label, shown_url(target)))
                .map(decode_entities),
        });
    }
    links
}

/// Slack labels most links with the URL itself, minus the scheme and often
/// shortened with an ellipsis in the middle; showing both says it twice.
fn repeats_url(label: &str, url: &str) -> bool {
    let bare = |s: &str| {
        s.trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.")
            .trim_end_matches('/')
            .to_string()
    };
    let (label, url) = (bare(label), bare(url));
    let head = label.split('…').next().unwrap_or(&label);
    label == url || (!head.is_empty() && url.starts_with(head))
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> Option<String> {
        (id == "U1").then(|| "jane".to_string())
    }

    fn channel(id: &str) -> Option<String> {
        (id == "C1").then(|| "general".to_string())
    }

    fn plain(text: &str) -> String {
        render(text, &user, &channel, &Style::plain())
    }

    #[test]
    fn mentions_and_channels_are_named() {
        assert_eq!(
            plain("hi <@U1>, see <#C1> and <#C2|random>"),
            "hi @jane, see #general and #random"
        );
        assert_eq!(plain("<@U9> <@U9|bob> <#C9>"), "@U9 @bob #C9");
    }

    #[test]
    fn special_mentions_and_dates_read_naturally() {
        assert_eq!(
            plain("<!here> <!channel> <!subteam^S1|@sre> <!date^1700000000^{date}|Nov 14>"),
            "@here @channel @sre Nov 14"
        );
    }

    #[test]
    fn links_are_listed_once_each_with_the_label_that_says_more() {
        let found = links(
            "see <https://x.y/z|the doc> and <https://x.y/z> then \
             <https://x.y/a?b=1&amp;c=2|x.y/a> <@U1|jane> <mailto:a@b.c|a@b.c>",
        );
        assert_eq!(
            found,
            [
                Link {
                    url: "https://x.y/z".into(),
                    label: Some("the doc".into()),
                },
                Link {
                    url: "https://x.y/a?b=1&c=2".into(),
                    label: None,
                },
                Link {
                    url: "mailto:a@b.c".into(),
                    label: None,
                },
            ]
        );
        assert!(links("nothing <here").is_empty());
    }

    #[test]
    fn links_keep_their_label_and_url() {
        assert_eq!(
            plain("<https://x.y/z|the doc> <https://x.y/> <mailto:a@b.c|a@b.c>"),
            "the doc (https://x.y/z) https://x.y/ a@b.c"
        );
        assert_eq!(
            plain("<https://x.y/z/1|x.y/z/1> <https://x.y/a/b/c|x.y/a/…/c> <https://www.x.y/|x.y>"),
            "https://x.y/z/1 https://x.y/a/b/c https://www.x.y/"
        );
    }

    #[test]
    fn entities_are_decoded_after_tokens() {
        assert_eq!(plain("a &lt;b&gt; &amp; c"), "a <b> & c");
    }

    #[test]
    fn unknown_tokens_and_markers_survive_without_styling() {
        assert_eq!(
            plain("<weird> *bold* _it_ ~no~ `code`"),
            "<weird> *bold* _it_ ~no~ `code`"
        );
        assert_eq!(plain("unclosed <token"), "unclosed <token");
    }

    #[test]
    fn styling_wraps_markup_on_word_boundaries() {
        let style = Style { enabled: true };
        let styled = render(
            "*bold* and snake_case_name, 2*3*4 ok `x_y`",
            &user,
            &channel,
            &style,
        );
        assert_eq!(
            styled,
            "\x1b[1mbold\x1b[0m and snake_case_name, 2*3*4 ok \x1b[35mx_y\x1b[0m"
        );
        let block = render("see ```\nlet x = 1;\n```", &user, &channel, &style);
        assert_eq!(block, "see \n\x1b[35mlet x = 1;\x1b[0m");
    }

    fn look(bold: bool, italic: bool, code: bool, kind: Kind) -> Look {
        Look {
            bold,
            italic,
            code,
            kind,
            ..Look::default()
        }
    }

    fn text(text: &str, look: Look) -> Piece {
        Piece::Text(text.to_string(), look)
    }

    fn marker(typed: &str, shown: &str) -> Piece {
        Piece::Marker {
            typed: typed.to_string(),
            shown: shown.to_string(),
        }
    }

    #[test]
    fn markers_nest_and_go_around_a_mention() {
        assert_eq!(
            pieces("*_both_* *hi <@U1>*", &user, &channel),
            [
                marker("*_", ""),
                text("both", look(true, true, false, Kind::Text)),
                marker("_*", ""),
                text(" ", Look::default()),
                marker("*", ""),
                text("hi ", look(true, false, false, Kind::Text)),
                text("@jane", look(true, false, false, Kind::Name)),
                marker("*", ""),
            ]
        );
    }

    #[test]
    fn code_shields_its_contents_and_a_lone_backtick_is_text() {
        assert_eq!(
            pieces("`*a_b*` it`s", &user, &channel),
            [
                marker("`", ""),
                text("*a_b*", look(false, false, true, Kind::Text)),
                marker("`", ""),
                text(" it`s", Look::default()),
            ]
        );
        // A link's own underscores are not markup.
        assert_eq!(
            strip("<https://x.test/_a_>", &user, &channel),
            "https://x.test/_a_"
        );
    }

    #[test]
    fn a_code_block_sits_on_lines_of_its_own() {
        let found = pieces("run ```a\nb``` now", &user, &channel);
        assert_eq!(
            found,
            [
                text("run ", Look::default()),
                marker("```", "\n"),
                text("a\nb", look(false, false, true, Kind::Text)),
                marker("```", "\n"),
                text(" now", Look::default()),
            ]
        );
        assert_eq!(strip("```\nx\n```", &user, &channel), "x");
    }

    #[test]
    fn a_quote_is_a_bar_before_its_line() {
        assert_eq!(
            pieces("&gt; said\nreply &gt; no", &user, &channel),
            [
                marker("> ", "▎ "),
                text("said\nreply > no", Look::default()),
            ]
        );
    }

    #[test]
    fn a_draft_keeps_its_markers_and_its_brackets() {
        assert_eq!(
            draft("*hi* <@U1> &amp;"),
            [
                marker("*", ""),
                text("hi", look(true, false, false, Kind::Text)),
                marker("*", ""),
                text(" <@U1> &amp;", Look::default()),
            ]
        );
    }

    #[test]
    fn strip_drops_the_markers_render_plain_keeps_them() {
        let text = "*Release* is _today_ in <#C1>";
        assert_eq!(strip(text, &user, &channel), "Release is today in #general");
        assert_eq!(plain(text), "*Release* is _today_ in #general");
    }

    #[test]
    fn a_link_with_an_entity_is_listed_once_and_decoded() {
        let found = links("<https://x.test/?a=1&amp;b=2> then <https://x.test/?a=1&amp;b=2|again>");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url, "https://x.test/?a=1&b=2");
    }

    #[test]
    fn only_web_mail_and_phone_links_are_openable() {
        let found = links(
            "<smb://host/share> <file:///etc/hosts> <https://x.test> <mailto:a@x.test> <tel:+33123>",
        );
        let urls: Vec<&str> = found.iter().map(|link| link.url.as_str()).collect();
        assert_eq!(urls, ["https://x.test", "mailto:a@x.test", "tel:+33123"]);
    }
}
