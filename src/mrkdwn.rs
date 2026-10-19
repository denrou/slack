//! Slack's message markup to terminal text. Pure: names come in through
//! closures, styling through `Style`, and with styling off the markers are
//! left as written so nothing is lost.

use crate::style::Style;

/// A name for an id, without the `@` or `#` sigil; None when unknown.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn render(text: &str, user: Lookup, channel: Lookup, style: &Style) -> String {
    let expanded = expand_tokens(text, user, channel, style);
    let decoded = decode_entities(&expanded);
    if style.enabled {
        style_markup(&decoded, style)
    } else {
        decoded
    }
}

/// Replace Slack's `<…>` tokens: mentions, channel links, special
/// mentions, dates and URLs. Entities are decoded afterwards, since a
/// literal `<` arrives as `&lt;`.
fn expand_tokens(text: &str, user: Lookup, channel: Lookup, style: &Style) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = &after[..end];
        match expand_token(token, user, channel, style) {
            Some(expanded) => out.push_str(&expanded),
            None => out.push_str(&rest[start..start + 1 + end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn expand_token(token: &str, user: Lookup, channel: Lookup, style: &Style) -> Option<String> {
    let (target, label) = match token.split_once('|') {
        Some((target, label)) => (target, Some(label)),
        None => (token, None),
    };
    if let Some(id) = target.strip_prefix('@') {
        let name = user(id)
            .or_else(|| label.map(str::to_string))
            .unwrap_or_else(|| id.to_string());
        return Some(style.name(&format!("@{name}")));
    }
    if let Some(id) = target.strip_prefix('#') {
        let name = label
            .filter(|label| !label.is_empty())
            .map(str::to_string)
            .or_else(|| channel(id))
            .unwrap_or_else(|| id.to_string());
        return Some(style.name(&format!("#{name}")));
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
        return Some(style.mention(&text));
    }
    if target.contains("://") || target.starts_with("mailto:") || target.starts_with("tel:") {
        let url = target
            .strip_prefix("mailto:")
            .or_else(|| target.strip_prefix("tel:"))
            .unwrap_or(target);
        return Some(
            match label.filter(|label| !label.is_empty() && !repeats_url(label, url)) {
                Some(label) => format!("{} ({})", label, style.link(url)),
                None => style.link(url),
            },
        );
    }
    None
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

/// Bold, italic, strike and code. Code blocks and spans are styled whole
/// and shield their contents from the other markers.
fn style_markup(text: &str, style: &Style) -> String {
    let mut out = String::with_capacity(text.len());
    let mut segments = text.split("```");
    if let Some(first) = segments.next() {
        out.push_str(&style_spans(first, style));
    }
    let mut in_block = true;
    for segment in segments {
        if in_block {
            out.push_str(&style.code(segment.trim_matches('\n')));
        } else {
            out.push_str(&style_spans(segment, style));
        }
        in_block = !in_block;
    }
    out
}

fn style_spans(text: &str, style: &Style) -> String {
    let mut out = String::with_capacity(text.len());
    let mut segments = text.split('`');
    if let Some(first) = segments.next() {
        out.push_str(&style_pairs(first, style));
    }
    let mut in_code = true;
    for segment in segments {
        if in_code {
            out.push_str(&style.code(segment));
        } else {
            out.push_str(&style_pairs(segment, style));
        }
        in_code = !in_code;
    }
    out
}

/// `*bold*`, `_italic_`, `~strike~`, each on a word boundary so that
/// `snake_case_names` and `2*3*4` stay as they are.
fn style_pairs(text: &str, style: &Style) -> String {
    let mut out = text.to_string();
    for (marker, paint) in [
        ('*', Style::bold as fn(&Style, &str) -> String),
        ('_', Style::italic),
        ('~', Style::strike),
    ] {
        out = style_pair(&out, marker, |inner| paint(style, inner));
    }
    out
}

fn style_pair(text: &str, marker: char, paint: impl Fn(&str) -> String) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == marker && opens_at(&chars, i) {
            if let Some(close) =
                (i + 2..chars.len()).find(|&j| chars[j] == marker && closes_at(&chars, j))
            {
                let inner: String = chars[i + 1..close].iter().collect();
                if !inner.contains('\n') {
                    out.push_str(&paint(&inner));
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn opens_at(chars: &[char], i: usize) -> bool {
    let before_ok = i == 0 || !chars[i - 1].is_alphanumeric();
    let after_ok = chars
        .get(i + 1)
        .is_some_and(|c| !c.is_whitespace() && *c != chars[i]);
    before_ok && after_ok
}

fn closes_at(chars: &[char], j: usize) -> bool {
    let before_ok = j > 0 && !chars[j - 1].is_whitespace() && chars[j - 1] != chars[j];
    let after_ok = chars.get(j + 1).is_none_or(|c| !c.is_alphanumeric());
    before_ok && after_ok
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
        assert_eq!(block, "see \x1b[35mlet x = 1;\x1b[0m");
    }
}
