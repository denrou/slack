//! `@handle` and `#channel` typed in a message become the mentions Slack
//! understands, `<@U…>` and `<#C…>`, when the name cache knows them; any
//! other word stays as typed, and no call is made to find out. `@here`,
//! `@channel` and `@everyone` stay text too: a notification to a whole
//! channel is never implicit.

use crate::names::Names;

/// Link the `@handle` and `#channel` words the cache knows.
pub fn link(text: &str, names: &Names) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['@', '#']) {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let sigil = &from[..1];
        let word = from[1..]
            .split(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | '_' | '-')))
            .next()
            .unwrap_or_default()
            .trim_end_matches(['.', '-', '_']);
        let linked = match (sigil, starts_a_word(out.chars().next_back()), word) {
            (_, _, "") | (_, false, _) => None,
            ("@", true, _) if is_broadcast(word) => None,
            ("@", true, _) => names.find_user(word).map(|id| format!("<@{id}>")),
            (_, true, _) => names.find_channel(word).map(|id| format!("<#{id}>")),
        };
        match linked {
            Some(mention) => {
                out.push_str(&mention);
                rest = &from[1 + word.len()..];
            }
            None => {
                out.push_str(sigil);
                rest = &from[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A sigil mentions something only at the start of a word: `a@b.c` is an
/// address and `x.com/#top` an anchor.
fn starts_a_word(previous: Option<char>) -> bool {
    previous.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\''))
}

fn is_broadcast(word: &str) -> bool {
    word.eq_ignore_ascii_case("here")
        || word.eq_ignore_ascii_case("channel")
        || word.eq_ignore_ascii_case("everyone")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::{Channel, User};

    fn names() -> Names {
        let user = |name: &str, display: &str| User {
            name: name.to_string(),
            display: display.to_string(),
            ..User::default()
        };
        let channel = |name: &str| Channel {
            name: name.to_string(),
            is_private: false,
            is_im: false,
            is_mpim: false,
            user: None,
        };
        Names::with(
            vec![("U1", user("jdoe", "Jane")), ("U2", user("bob.d", "Bob"))],
            vec![("C1", channel("dev-team")), ("D1", channel(""))],
        )
    }

    #[test]
    fn known_handles_and_channels_become_mentions() {
        assert_eq!(
            link("hi @jdoe, see #dev-team (@bob.d.) and @Jane", &names()),
            "hi <@U1>, see <#C1> (<@U2>.) and <@U1>"
        );
    }

    #[test]
    fn anything_else_stays_as_typed() {
        let text = "mail a@b.c, x.com/#top, #123, @nobody, @here @channel, <@U1> <#C1|dev-team>, @ # @@jdoe";
        assert_eq!(link(text, &names()), text);
    }
}
