//! Slack's message markup to terminal text, and to the rich text blocks a
//! scheduled message is written in. Pure: names come in through closures,
//! and the text comes out as pieces, each with its look, which the text
//! view paints with `Style`. With styling off the markers are left as
//! written so nothing is lost.

use serde_json::{Value, json};

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

/// A line break inside a `>>>` quote, which the bar follows.
const QUOTED_BREAK: &str = "\n▎ ";

/// A message's text: tokens expanded, entities decoded, markup read.
pub fn pieces(text: &str, user: Lookup, channel: Lookup) -> Vec<Piece> {
    Markup::new(tokens(text, user, channel))
        .read(true)
        .into_pieces()
}

/// Text as typed for `slack send` (its mentions already `<@U…>`) as the
/// `blocks` of a draft: bold, italic, strike and code, the mentions, the
/// channels and the links, read by the same rules as a message received.
/// Deliberate, as slackcli found with `drafts.create`: one section with
/// its line breaks inside the text, since Slack runs several sections
/// together on one line; and a `>` quote stays as typed.
pub fn rich_text(text: &str) -> Value {
    let mut elements: Vec<Value> = Vec::new();
    let markup = Markup::new(elements_of(text)).read(false);
    let atoms = markup
        .atoms
        .into_iter()
        .zip(markup.looks)
        .zip(markup.markers);
    for ((atom, look), marker) in atoms {
        let style = style_of(look);
        match (atom, marker) {
            (Atom::Char(_), Some("")) => {}
            (Atom::Char(_), Some(shown)) => push_text(&mut elements, shown, &style),
            (Atom::Char(c), None) => push_text(&mut elements, c.encode_utf8(&mut [0; 4]), &style),
            (Atom::Token(mut element), _) => {
                if let Some(style) = style {
                    element["style"] = style;
                }
                elements.push(element);
            }
        }
    }
    if elements.is_empty() {
        elements.push(json!({ "type": "text", "text": "" }));
    }
    json!([{
        "type": "rich_text",
        "elements": [{ "type": "rich_text_section", "elements": elements }],
    }])
}

fn style_of(look: Look) -> Option<Value> {
    let mut style = serde_json::Map::new();
    for (on, name) in [
        (look.bold, "bold"),
        (look.italic, "italic"),
        (look.strike, "strike"),
        (look.code, "code"),
    ] {
        if on {
            style.insert(name.to_string(), Value::Bool(true));
        }
    }
    (!style.is_empty()).then_some(Value::Object(style))
}

fn push_text(elements: &mut Vec<Value>, text: &str, style: &Option<Value>) {
    if let Some(last) = elements.last_mut()
        && last["type"] == "text"
        && last.get("style") == style.as_ref()
        && let Some(Value::String(so_far)) = last.get_mut("text")
    {
        so_far.push_str(text);
        return;
    }
    let mut element = json!({ "type": "text", "text": text });
    if let Some(style) = style {
        element["style"] = style.clone();
    }
    elements.push(element);
}

/// The text with Slack's `<…>` tokens as rich text elements; any other
/// `<` stays a character. Nothing is decoded: this text was typed, not
/// received.
fn elements_of(text: &str) -> Vec<Atom<Value>> {
    let mut atoms = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        atoms.extend(rest[..start].chars().map(Atom::Char));
        match element_of(&after[..end]) {
            Some(element) => atoms.push(Atom::Token(element)),
            None => atoms.extend(rest[start..start + 1 + end + 1].chars().map(Atom::Char)),
        }
        rest = &after[end + 1..];
    }
    atoms.extend(rest.chars().map(Atom::Char));
    atoms
}

fn element_of(token: &str) -> Option<Value> {
    let (target, label) = match token.split_once('|') {
        Some((target, label)) => (target, Some(label).filter(|label| !label.is_empty())),
        None => (token, None),
    };
    let is_id = |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric());
    if let Some(id) = target.strip_prefix('@').filter(|id| is_id(id)) {
        return Some(json!({ "type": "user", "user_id": id }));
    }
    if let Some(id) = target.strip_prefix('#').filter(|id| is_id(id)) {
        return Some(json!({ "type": "channel", "channel_id": id }));
    }
    if let Some(id) = target.strip_prefix("!subteam^").filter(|id| is_id(id)) {
        return Some(json!({ "type": "usergroup", "usergroup_id": id }));
    }
    if let Some(range) = target
        .strip_prefix('!')
        .filter(|range| matches!(*range, "here" | "channel" | "everyone"))
    {
        return Some(json!({ "type": "broadcast", "range": range }));
    }
    if is_url(target) && !target.contains(char::is_whitespace) {
        let mut link = json!({ "type": "link", "url": target });
        if let Some(label) = label {
            link["text"] = json!(label);
        }
        return Some(link);
    }
    None
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

/// A character of the text, or a token kept whole: markup goes around a
/// mention or a link, never into it. The token is its expansion for the
/// text view, its element for rich text.
enum Atom<T> {
    Char(char),
    Token(T),
}

/// A token as the text view shows it.
type Shown = Vec<(String, Kind)>;

/// Replace Slack's `<…>` tokens: mentions, channel links, special
/// mentions, dates and URLs. Entities are decoded after the split, since a
/// literal `<` arrives as `&lt;`.
fn tokens(text: &str, user: Lookup, channel: Lookup) -> Vec<Atom<Shown>> {
    fn push(atoms: &mut Vec<Atom<Shown>>, text: &str) {
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
struct Markup<T> {
    atoms: Vec<Atom<T>>,
    looks: Vec<Look>,
    /// Some for an atom that is markup, with what a styled view shows for
    /// it.
    markers: Vec<Option<&'static str>>,
}

impl<T> Markup<T> {
    fn new(atoms: Vec<Atom<T>>) -> Self {
        let len = atoms.len();
        Markup {
            atoms,
            looks: vec![Look::default(); len],
            markers: vec![None; len],
        }
    }

    /// Code first, since it shields what it holds from the other markers,
    /// then quotes when asked, then bold, italic and strike, which may
    /// nest.
    fn read(mut self, with_quotes: bool) -> Self {
        self.code_blocks();
        self.code_spans();
        if with_quotes {
            self.quotes();
        }
        self.pairs('*', |look| look.bold = true);
        self.pairs('_', |look| look.italic = true);
        self.pairs('~', |look| look.strike = true);
        self
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

    /// `>` at the start of a line quotes it, shown as a bar, and `>>>`
    /// quotes the rest of the message, a bar after each line break.
    fn quotes(&mut self) {
        let len = self.atoms.len();
        let mut at = 0;
        while at < len {
            if !(self.is(at, '>') && self.line_starts_at(at)) {
                at += 1;
                continue;
            }
            let rest = (at..at + 3).all(|at| self.is(at, '>'));
            let mut from = if rest { at + 3 } else { at + 1 };
            if self.char(from) == Some(' ') {
                from += 1;
            }
            self.mark(at..from, "▎ ");
            if rest {
                for at in from..len {
                    if self.is(at, '\n') {
                        self.mark(at..at + 1, QUOTED_BREAK);
                    }
                }
                return;
            }
            at = from;
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
}

impl Markup<Shown> {
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

/// A mail or phone link reads better without its scheme.
fn shown_url(target: &str) -> &str {
    target
        .strip_prefix("mailto:")
        .or_else(|| target.strip_prefix("tel:"))
        .unwrap_or(target)
}

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

    /// The elements of the one section `rich_text` writes.
    fn rich(text: &str) -> Value {
        rich_text(text)[0]["elements"][0]["elements"].clone()
    }

    #[test]
    fn plain_text_is_the_blocks_the_slack_app_sends() {
        // As the Slack app wrote it in a scheduled draft, 2026-11-04.
        assert_eq!(
            rich_text("test"),
            serde_json::json!([{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"test"}]}]}])
        );
        assert_eq!(rich(""), json!([{ "type": "text", "text": "" }]));
    }

    #[test]
    fn markup_becomes_styles_by_the_reading_rules() {
        assert_eq!(
            rich("a *b _c_* ~d~ `e *f*` snake_case_name 2*3*4"),
            json!([
                { "type": "text", "text": "a " },
                { "type": "text", "text": "b ", "style": { "bold": true } },
                { "type": "text", "text": "c", "style": { "bold": true, "italic": true } },
                { "type": "text", "text": " " },
                { "type": "text", "text": "d", "style": { "strike": true } },
                { "type": "text", "text": " " },
                { "type": "text", "text": "e *f*", "style": { "code": true } },
                { "type": "text", "text": " snake_case_name 2*3*4" },
            ])
        );
    }

    #[test]
    fn line_breaks_stay_in_the_text_and_a_quote_as_typed() {
        assert_eq!(
            rich("one\n> two\n```\nx = 1\n```"),
            json!([
                { "type": "text", "text": "one\n> two\n" },
                { "type": "text", "text": "x = 1", "style": { "code": true } },
            ])
        );
    }

    #[test]
    fn tokens_become_elements_and_keep_the_style_around_them() {
        assert_eq!(
            rich(
                "*hi <@U1>*, see <#C1> <!here> <!subteam^S1|@sre> <https://x.y/z|the doc> <https://x.y/>"
            ),
            json!([
                { "type": "text", "text": "hi ", "style": { "bold": true } },
                { "type": "user", "user_id": "U1", "style": { "bold": true } },
                { "type": "text", "text": ", see " },
                { "type": "channel", "channel_id": "C1" },
                { "type": "text", "text": " " },
                { "type": "broadcast", "range": "here" },
                { "type": "text", "text": " " },
                { "type": "usergroup", "usergroup_id": "S1" },
                { "type": "text", "text": " " },
                { "type": "link", "url": "https://x.y/z", "text": "the doc" },
                { "type": "text", "text": " " },
                { "type": "link", "url": "https://x.y/" },
            ])
        );
    }

    #[test]
    fn anything_else_in_brackets_stays_text_as_typed() {
        assert_eq!(
            rich("a <b> & c &lt; <@> <unclosed"),
            json!([{ "type": "text", "text": "a <b> & c &lt; <@> <unclosed" }])
        );
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
            pieces("<https://x.test/_a_>", &user, &channel),
            [text(
                "https://x.test/_a_",
                look(false, false, false, Kind::Link)
            )]
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
    fn three_quote_marks_quote_the_rest_of_the_message() {
        let quoted = "&gt;&gt;&gt; one\n&gt; two\nthree";
        assert_eq!(
            pieces(quoted, &user, &channel),
            [
                marker(">>> ", "▎ "),
                text("one", Look::default()),
                marker("\n", "\n▎ "),
                text("> two", Look::default()),
                marker("\n", "\n▎ "),
                text("three", Look::default()),
            ]
        );
    }

    #[test]
    fn render_plain_gives_back_every_character_typed() {
        for typed in [
            "*",
            "**",
            "a*b*c",
            "```oops",
            "````",
            "`````",
            "``````",
            "```\nlet x\n```",
            "```a``` and ```b```",
            "see ```x = 1``` done",
            ">",
            "> ",
            "> *x*\n> y",
            ">>> a\n*b*\n",
            "_ _",
            "~/a and ~/b",
            "*été* _à_ ~ça~",
        ] {
            assert_eq!(plain(typed), typed);
        }
    }
}
