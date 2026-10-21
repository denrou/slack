//! Drawing the TUI from its state: nothing here changes anything.

use jiff::Zoned;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::model::{
    Completion, Draft, Focus, Mode, Picker, Row, Section, State, Viewport, row_of, rows,
};
use crate::history::Message;
use crate::names::Names;
use crate::{emoji, mrkdwn, view};

const LIST_HINTS: &str = "j/k move  l messages  Enter reply  n new  c open any  r read  R read all  o in Slack  Ctrl-R refresh  ? keys  q quit";
const OPENED_HINTS: &str = "j/k move  l messages  Enter reply  n new  x close  c open any  r read  o in Slack  Ctrl-R refresh  ? keys  q quit";
const MESSAGES_HINTS: &str = "j/k move  Enter reply to it  l open the thread  u open link  y copy link  o in Slack  h back  q quit";
const THREAD_HINTS: &str =
    "j/k move  Enter reply in the thread  u open link  y copy link  o in Slack  h back  q quit";

const HELP: &[&str] = &[
    "j, k, arrows    move through the list, or through the messages",
    "l, Right        deeper: the messages, then the highlighted message's thread",
    "h, Left, Esc    back: close the thread, leave the messages",
    "gg, G           the first, the last (Home, End too)",
    "Ctrl-D, Ctrl-U  half a screen down, up; Ctrl-F, Ctrl-B, PgDn, PgUp a whole one",
    "Enter           reply: to the conversation, to the highlighted message, or in the open thread",
    "s               a message to the conversation, wherever you are in it",
    "n               new message: pick a channel or people (Tab marks several as you type), then write",
    "c               open a channel or group you are in, or anyone: type to filter, Enter opens; m in the list shows the muted",
    "@, #            in the reply box, complete a person or a channel: Up/Down move, Tab picks, Esc closes the list",
    "Ctrl-J          a new line in the reply box (Ctrl-Enter, Shift-Enter too where the terminal reports them)",
    "Ctrl-W, Ctrl-U  the last word, the line, back (Option-, Command-Backspace too where the terminal reports them)",
    "x               close an opened conversation",
    "u, y            open, or copy, a link of the highlighted message",
    "+               react to the highlighted message: type to filter, Enter puts it on, or takes yours back",
    "r               mark the selected one as read",
    "R               mark everything listed as read (asks first)",
    "o               open it in Slack, at the highlighted message from the pane",
    "Ctrl-R          refresh",
    "q, Ctrl-C       quit",
];

/// Returns what the keys that move by screens need to know about the
/// messages pane, for `State::viewport`.
pub fn draw(frame: &mut Frame, state: &State, names: &Names, now: &Zoned) -> Viewport {
    let bottom_height = match &state.mode {
        Mode::Compose(draft) => {
            compose_rows(&draft.text, frame.area().width)
                + 2
                + state.completion(draft).as_ref().map_or(0, list_rows)
        }
        Mode::Help => HELP.len() as u16 + 2,
        Mode::Links { links, .. } => links.len() as u16 + 2,
        Mode::React(reacting) => state.emoji_choices(&reacting.query).len() as u16 + 3,
        Mode::Browse | Mode::ConfirmReadAll | Mode::Pick(_) => 1,
    };
    let [main, bottom] = Layout::vertical([Constraint::Min(3), Constraint::Length(bottom_height)])
        .areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).areas(main);
    match &state.mode {
        Mode::Pick(picker) => draw_picker(frame, left, picker),
        _ => draw_list(frame, left, state),
    }
    let viewport = draw_messages(frame, right, state, names, now);
    draw_bottom(frame, bottom, state, names);
    viewport
}

fn draw_list(frame: &mut Frame, area: Rect, state: &State) {
    let rows = rows(&state.items);
    let lines: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            Row::Header(section) => ListItem::new(Line::from(section.title().bold().dim())),
            Row::Item(index) => {
                let item = &state.items[*index];
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::raw(item.title.as_str()),
                    Span::raw("  "),
                    Span::styled(item.detail.as_str(), Style::new().dim()),
                ]))
            }
        })
        .collect();
    let title = format!(" slack · {} ", state.items.len());
    let highlight = if state.focus == Focus::List {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    let list = List::new(lines)
        .block(Block::bordered().title(title))
        .highlight_style(highlight);
    let selected = state.selected.and_then(|item| row_of(&rows, item));
    let mut list_state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// The picker takes the list's place: the filter in the title, the
/// matching choices below, the marked people ticked, the selected one
/// highlighted (reversed when the keys move the list, bold while they
/// type the filter).
fn draw_picker(frame: &mut Frame, area: Rect, picker: &Picker) {
    let matches = picker.matches();
    let lines: Vec<ListItem> = matches
        .iter()
        .map(|&index| {
            let choice = &picker.choices[index];
            let tick = if picker.marked.contains(&index) {
                "✓ "
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::raw(tick),
                Span::raw(choice.label.as_str()),
                Span::raw("  "),
                Span::styled(choice.note.as_str(), Style::new().dim()),
            ]))
        })
        .collect();
    let verb = if picker.compose {
        "new message to"
    } else {
        "open"
    };
    let title = if picker.filtering {
        format!(" {verb} › {}▏ ", picker.query)
    } else if picker.query.is_empty() {
        format!(" {verb} ")
    } else {
        format!(" {verb} › {} ", picker.query)
    };
    let highlight = if picker.filtering {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::REVERSED)
    };
    let list = List::new(lines)
        .block(Block::bordered().title(title))
        .highlight_style(highlight);
    let selected = (!matches.is_empty()).then(|| picker.selected.min(matches.len() - 1));
    let mut list_state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_messages(
    frame: &mut Frame,
    area: Rect,
    state: &State,
    names: &Names,
    now: &Zoned,
) -> Viewport {
    let (title, has_more, in_thread, unread_from) = match (&state.thread, state.selected_item()) {
        (Some(thread), _) => {
            let replies = thread.messages.len().saturating_sub(1);
            let count = match replies {
                1 => "1 reply".to_string(),
                n => format!("{n}{} replies", if thread.has_more { "+" } else { "" }),
            };
            (
                format!(" Thread · {} · {count} ", thread.title),
                thread.has_more,
                true,
                None,
            )
        }
        (None, Some(item)) => (
            format!(" {} · {} ", item.title, item.detail),
            item.has_more,
            false,
            item.unread_from.as_deref(),
        ),
        (None, None) => {
            let text = if state.items.is_empty() {
                "All caught up."
            } else {
                ""
            };
            frame.render_widget(Paragraph::new(text).block(Block::bordered()), area);
            return Viewport::default();
        }
    };
    // A conversation's page is a history, read down from its oldest
    // message, so what is missing comes before it; a thread's page is its
    // new replies, and what is missing comes after.
    let history = !in_thread
        && state
            .selected_item()
            .is_some_and(|item| item.thread_ts.is_none());
    let messages = state.pane_messages();
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    let plain = crate::style::Style::plain();
    let times: Vec<String> = messages
        .iter()
        .map(|m| view::format_ts(&m.ts, now))
        .collect();
    let width = times.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    let indent = " ".repeat(width + 2);

    let mut lines: Vec<Line> = Vec::new();
    if has_more && history {
        lines.push(Line::from("earlier messages not shown").dim());
    }
    let mut starts = Vec::with_capacity(messages.len());
    let mut ends = Vec::with_capacity(messages.len());
    let mut separator = None;
    for (index, (message, time)) in messages.iter().zip(times).enumerate() {
        let new = !in_thread && view::is_new(message, unread_from);
        if new && separator.is_none() {
            separator = Some(lines.len());
            lines.push(Line::from(format!("{indent}── new ──")).dim());
        }
        // The read part of a history steps back, so the new part stands
        // out without a colour of its own.
        let read = unread_from.is_some() && !new;
        starts.push(lines.len());
        let mentions_me = state.me.as_deref().is_some_and(|me| message.mentions(me));
        let time_style = if mentions_me {
            Style::new().yellow().bold()
        } else {
            Style::new().dim()
        };
        let mut spans = vec![
            Span::styled(format!("{time:>width$}"), time_style),
            Span::raw("  "),
        ];
        let author = match (&message.user, &message.bot) {
            (Some(user), _) => Some(names.user_label(user)),
            (None, Some(bot)) => Some(format!("{bot} (bot)")),
            (None, None) => None,
        };
        if let Some(author) = author {
            spans.push(Span::styled(author, Style::new().cyan()));
            spans.push(Span::raw(": "));
        }
        let text = mrkdwn::render(&message.text, &user, &channel, &plain);
        let mut text_lines = text.lines();
        spans.push(Span::raw(text_lines.next().unwrap_or("").to_string()));
        let mut notes = Vec::new();
        if in_thread && index == 0 {
            notes.push("root".to_string());
        }
        match message.reply_count {
            0 => {}
            1 => notes.push("1 reply".to_string()),
            n => notes.push(format!("{n} replies")),
        }
        for file in &message.files {
            notes.push(format!("file: {file}"));
        }
        if !notes.is_empty() {
            spans.push(Span::styled(
                format!(" [{}]", notes.join(", ")),
                Style::new().dim(),
            ));
        }
        let style = if read {
            Style::new().dim()
        } else {
            Style::new()
        };
        lines.push(Line::from(spans).style(style));
        for rest in text_lines {
            lines.push(
                Line::from(vec![Span::raw(indent.clone()), Span::raw(rest.to_string())])
                    .style(style),
            );
        }
        // Under the message, as the app has them; yours stand out, since
        // `+` on one of them takes it back.
        if !message.reactions.is_empty() {
            let mut spans = vec![Span::raw(indent.clone())];
            for (at, reaction) in message.reactions.iter().enumerate() {
                if at > 0 {
                    spans.push(Span::raw("  "));
                }
                let mine = state.me.as_deref().is_some_and(|me| reaction.by(me));
                let look = if mine {
                    Style::new().blue().bold()
                } else {
                    Style::new()
                };
                spans.push(Span::styled(view::reaction_label(reaction), look));
            }
            lines.push(Line::from(spans).style(style));
        }
        ends.push(lines.len());
    }
    if has_more && !history {
        let note = if in_thread {
            "more replies not shown"
        } else {
            "newer replies not shown"
        };
        lines.push(Line::from(note).dim());
    }

    // Scrolling counts rows of the screen, and a long message wraps over
    // several, so the row a line lands on is what the wrapper says, not
    // the line's index: `first_row[line]`, and `first_row[lines.len()]`
    // is the end of the text.
    let height = area.height.saturating_sub(2);
    let inner_height = usize::from(height);
    let inner_width = area.width.saturating_sub(2);
    let mut first_row = Vec::with_capacity(lines.len() + 1);
    let mut total = 0;
    first_row.push(total);
    for line in &lines {
        total += if inner_width == 0 {
            1
        } else {
            Paragraph::new(vec![line.clone()])
                .wrap(Wrap { trim: false })
                .line_count(inner_width)
        };
        first_row.push(total);
    }
    let row_of = |line: usize| first_row[line];
    // From the list, a history starts at its new part with a few rows of
    // what came before above it, or at its end when nothing is new, the
    // way a chat scrolls; Ctrl-U shows the rest.
    let anchor = match separator {
        Some(at) => row_of(at).saturating_sub(inner_height / 4),
        None if history => row_of(lines.len()).saturating_sub(inner_height),
        None => 0,
    } as u16;
    // With the focus in the pane, the highlighted message stays in view,
    // around the middle once the pane has scrolled.
    let scroll = if state.focus == Focus::Messages {
        let row = starts.get(state.cursor).map_or(0, |&start| row_of(start));
        if row >= inner_height {
            (row - inner_height / 2) as u16
        } else {
            0
        }
    } else {
        (i32::from(anchor) + state.scroll).max(0) as u16
    };
    // The highlighted message as rows of the pane, every row it wraps to.
    let highlight = (state.focus == Focus::Messages)
        .then(|| starts.get(state.cursor).zip(ends.get(state.cursor)))
        .flatten()
        .map(|(&start, &end)| (row_of(start), row_of(end)));
    let screen = usize::from(scroll)..usize::from(scroll) + inner_height;
    let in_view = starts
        .iter()
        .filter(|&&start| screen.contains(&row_of(start)))
        .count();
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
    // A full-width block over the highlighted message, painted after the
    // text: a line's own style stops where its text does.
    if let Some((from, to)) = highlight {
        let inner = area.inner(Margin::new(1, 1));
        let scroll = usize::from(scroll);
        let top = from.max(scroll);
        let bottom = to.min(scroll + usize::from(inner.height));
        if top < bottom {
            let block = Rect {
                x: inner.x,
                y: inner.y + (top - scroll) as u16,
                width: inner.width,
                height: (bottom - top) as u16,
            };
            frame
                .buffer_mut()
                .set_style(block, Style::new().add_modifier(Modifier::REVERSED));
        }
    }
    Viewport {
        anchor,
        height,
        rows: total,
        in_view,
    }
}

/// The reply box grows with its text, as wrapped at the width of the
/// screen, up to this many rows: Ctrl-J or a paste brings several lines.
const COMPOSE_ROWS: u16 = 6;

/// The completion list shows this many entries at most.
const COMPLETION_ROWS: u16 = 6;

/// The rows the completion list takes above the reply box, border
/// included.
fn list_rows(completion: &Completion) -> u16 {
    u16::try_from(completion.matches.len())
        .unwrap_or(u16::MAX)
        .min(COMPLETION_ROWS)
        + 2
}

/// The completion list, right above the reply box: what the typed word
/// begins, the highlighted entry reversed.
fn draw_completion(frame: &mut Frame, area: Rect, state: &State, completion: &Completion) {
    let candidates = state.candidates.as_deref().unwrap_or_default();
    let lines: Vec<ListItem> = completion
        .matches
        .iter()
        .filter_map(|&index| candidates.get(index))
        .map(|candidate| {
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                Span::raw(candidate.insert.as_str()),
                Span::raw("  "),
                Span::styled(candidate.detail.as_str(), Style::new().dim()),
            ]))
        })
        .collect();
    let channels = completion
        .matches
        .first()
        .and_then(|&index| candidates.get(index))
        .is_some_and(|candidate| candidate.insert.starts_with('#'));
    let kind = match (channels, completion.matches.len()) {
        (true, 1) => "channel",
        (true, _) => "channels",
        (false, 1) => "person",
        (false, _) => "people",
    };
    let title = format!(
        " {} {kind} · Up/Down move, Tab or Enter picks, Esc closes ",
        completion.matches.len()
    );
    let list = List::new(lines)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default().with_selected(Some(completion.pick));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn compose_rows(text: &str, width: u16) -> u16 {
    let inner = width.saturating_sub(2);
    if inner == 0 {
        return 1;
    }
    let rows = Paragraph::new(format!("{text}▏"))
        .wrap(Wrap { trim: false })
        .line_count(inner);
    u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .clamp(1, COMPOSE_ROWS)
}

fn draw_bottom(frame: &mut Frame, area: Rect, state: &State, names: &Names) {
    match &state.mode {
        Mode::Compose(draft) => {
            let completion = state.completion(draft);
            let [above, area] = Layout::vertical([
                Constraint::Length(completion.as_ref().map_or(0, list_rows)),
                Constraint::Fill(1),
            ])
            .areas(area);
            if let Some(completion) = &completion {
                draw_completion(frame, above, state, completion);
            }
            let Draft {
                text, thread_ts, ..
            } = draft;
            let to = state
                .selected_item()
                .map_or_else(String::new, |item| item.title.clone());
            let title = match thread_ts {
                Some(_) if state.thread.is_some() => " Reply in the thread".to_string(),
                // A message of the pane: say whose, the way the pane shows it.
                Some(ts)
                    if state
                        .selected_item()
                        .is_some_and(|item| item.thread_ts.is_none()) =>
                {
                    let who = state
                        .pane_messages()
                        .iter()
                        .find(|m| m.thread_ts.as_deref().unwrap_or(&m.ts) == ts.as_str())
                        .map_or_else(|| "the message".to_string(), |m| describe(m, names));
                    format!(" Reply to {who}")
                }
                Some(_) => format!(" Reply to {to}"),
                None => format!(" Message to {to}"),
            };
            // An error from a send, or a load under way, takes the hint's place.
            let title = match &state.status {
                Some(status) => format!("{title} · {status} "),
                None if completion.is_some() => {
                    format!("{title} · Tab picks, Esc closes the list ")
                }
                None => format!("{title} · Enter sends, Ctrl-J a new line, Esc cancels "),
            };
            let mut lines: Vec<Line> = text.split('\n').map(Line::from).collect();
            if let Some(last) = lines.last_mut() {
                last.push_span(Span::styled("▏", Style::new().dim()));
            }
            // The last rows stay in view once the text outgrows the box.
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            let inner = area.inner(Margin::new(1, 1));
            let rows = if inner.width == 0 {
                0
            } else {
                paragraph.line_count(inner.width)
            };
            let scroll = rows.saturating_sub(usize::from(inner.height));
            frame.render_widget(
                paragraph
                    .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
                    .block(Block::bordered().title(title)),
                area,
            );
        }
        Mode::ConfirmReadAll => {
            let text = format!(
                "Mark all {} as read? y, or any other key to cancel",
                state.items.len()
            );
            frame.render_widget(Paragraph::new(text).yellow(), area);
        }
        Mode::Links { links, copy } => {
            let title = if *copy {
                " Copy which link? · a digit picks, any other key cancels "
            } else {
                " Open which link? · a digit picks, any other key cancels "
            };
            let lines: Vec<Line> = links
                .iter()
                .enumerate()
                .map(|(index, link)| {
                    let mut spans = vec![
                        Span::styled(format!("{}  ", index + 1), Style::new().bold()),
                        Span::raw(link.url.as_str()),
                    ];
                    if let Some(label) = &link.label {
                        spans.push(Span::styled(format!("  {label}"), Style::new().dim()));
                    }
                    Line::from(spans)
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(title)),
                area,
            );
        }
        Mode::React(reacting) => {
            let who = state
                .highlighted()
                .map_or_else(|| "the message".to_string(), |m| describe(m, names));
            let title = match &state.status {
                Some(status) => format!(" React to {who} · {status} "),
                None => {
                    format!(" React to {who} · Enter puts it on or takes yours back, Esc cancels ")
                }
            };
            let mut lines = vec![Line::from(vec![
                Span::raw(reacting.query.as_str()),
                Span::styled("▏", Style::new().dim()),
            ])];
            let choices = state.emoji_choices(&reacting.query);
            let pick = reacting.pick.min(choices.len().saturating_sub(1));
            let on_message = |name: &str| {
                state
                    .highlighted()
                    .and_then(|m| m.reactions.iter().find(|r| r.name == name))
                    .map(|r| r.count)
            };
            for (at, name) in choices.iter().enumerate() {
                let note = match (on_message(name), state.reacted_with(name)) {
                    (Some(_), true) => "  yours, Enter takes it back".to_string(),
                    (Some(count), false) => format!("  {count} on it"),
                    (None, _) => String::new(),
                };
                let line = Line::from(vec![
                    // A custom emoji has no character: its name alone.
                    Span::raw(format!(" {}  {name}", emoji::glyph(name).unwrap_or("  "))),
                    Span::styled(note, Style::new().dim()),
                ]);
                lines.push(if at == pick {
                    line.style(Style::new().add_modifier(Modifier::REVERSED))
                } else {
                    line
                });
            }
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(title)),
                area,
            );
        }
        Mode::Help => {
            let lines: Vec<Line> = HELP.iter().map(|line| Line::from(*line)).collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" keys · any key closes ")),
                area,
            );
        }
        Mode::Pick(picker) => {
            let enter = match (picker.enter_lists(), picker.compose) {
                (true, _) => "to the list",
                (false, false) => "open",
                (false, true) => "write",
            };
            let esc = if picker.query.is_empty() {
                "close"
            } else {
                "clear"
            };
            let keys = match (picker.filtering, picker.compose) {
                (true, _) => {
                    format!("type to filter  Tab mark a person  Enter {enter}  Esc {esc}")
                }
                (false, false) => {
                    "j/k move  / filter  Space mark people  m muted  Enter open  Esc cancel".into()
                }
                (false, true) => {
                    "j/k move  / filter  Space mark people  m muted  Enter write  Esc cancel".into()
                }
            };
            let marked: Vec<&str> = picker
                .marked
                .iter()
                .map(|&index| picker.choices[index].label.as_str())
                .collect();
            let to = if marked.is_empty() {
                String::new()
            } else {
                format!(" · to {}", marked.join(", "))
            };
            let hidden = match picker.hidden_muted() {
                0 => String::new(),
                n => format!(" · {n} muted hidden"),
            };
            let text = format!(
                "{} of {}{hidden}{to}  {keys}",
                picker.matches().len(),
                picker.choices.len()
            );
            frame.render_widget(Paragraph::new(text).dim(), area);
        }
        Mode::Browse => {
            let opened = state
                .selected_item()
                .is_some_and(|item| item.section == Section::Opened);
            let hints = match (state.focus, &state.thread) {
                (Focus::List, _) if opened => OPENED_HINTS,
                (Focus::List, _) => LIST_HINTS,
                (Focus::Messages, None) => MESSAGES_HINTS,
                (Focus::Messages, Some(_)) => THREAD_HINTS,
            };
            let text = state.status.as_deref().unwrap_or(hints);
            frame.render_widget(Paragraph::new(text).dim(), area);
        }
    }
}

/// `@author: the first words`, for a prompt that names a message.
fn describe(message: &Message, names: &Names) -> String {
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    let text = mrkdwn::render(
        &message.text,
        &user,
        &channel,
        &crate::style::Style::plain(),
    );
    let author = match (&message.user, &message.bot) {
        (Some(user), _) => names.user_label(user),
        (None, Some(bot)) => bot.clone(),
        (None, None) => "someone".to_string(),
    };
    format!("{author}: {}", view::excerpt(&text, 30))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::model::{Candidate, Item, Section};
    use super::*;

    fn message(ts: &str, text: &str) -> Message {
        Message {
            ts: ts.to_string(),
            user: None,
            bot: None,
            text: text.to_string(),
            subtype: None,
            thread_ts: None,
            reply_count: 0,
            files: Vec::new(),
            attachments: Vec::new(),
            reactions: Vec::new(),
        }
    }

    fn ts(n: usize) -> String {
        format!("1700000000.{n:06}")
    }

    /// One channel with `texts` as its messages, in order; the read cursor
    /// sits before message number `first_new`, counted from 1.
    fn history(texts: &[impl AsRef<str>], first_new: Option<usize>) -> State {
        let messages: Vec<Message> = texts
            .iter()
            .enumerate()
            .map(|(i, text)| message(&ts(i + 1), text.as_ref()))
            .collect();
        let item = Item {
            section: Section::Channels,
            channel: "C1".to_string(),
            thread_ts: None,
            title: "#c1".to_string(),
            detail: String::new(),
            latest: messages.last().map(|m| m.ts.clone()),
            messages,
            has_more: false,
            unread_from: first_new.map(|n| ts(n - 1)),
        };
        State::new(vec![item], None)
    }

    fn fifty() -> Vec<String> {
        (1..=50).map(|n| format!("message {n}")).collect()
    }

    fn render(state: &State, width: u16, height: u16) -> (Buffer, Viewport) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        let mut viewport = Viewport::default();
        terminal
            .draw(|frame| viewport = draw(frame, state, &Names::default(), &Zoned::now()))
            .expect("a draw");
        (terminal.backend().buffer().clone(), viewport)
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn reactions_sit_under_their_message_and_plus_offers_them() {
        let mut state = history(&["hello", "world"], None);
        state.me = Some("U7".to_string());
        state.items[0].messages[0].reactions = vec![
            crate::history::Reaction {
                name: "+1".to_string(),
                count: 2,
                users: vec!["U2".to_string(), "U7".to_string()],
            },
            crate::history::Reaction {
                name: "yolo".to_string(),
                count: 1,
                users: vec!["U2".to_string()],
            },
        ];
        let (buffer, _) = render(&state, 100, 20);
        let rows: Vec<String> = (0..20).map(|y| row(&buffer, y)).collect();
        let hello = rows
            .iter()
            .position(|r| r.contains("hello"))
            .expect("hello");
        // A wide character fills two cells, the second one blank here.
        assert!(
            rows[hello + 1].contains("👍  2  :yolo: 1"),
            "{}",
            rows[hello + 1]
        );
        assert!(rows[hello + 2].contains("world"));

        state.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        state.key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
        let (buffer, _) = render(&state, 100, 20);
        let rows: Vec<String> = (0..20).map(|y| row(&buffer, y)).collect();
        assert!(rows.iter().any(|r| r.contains("React to")));
        assert!(
            rows.iter()
                .any(|r| r.contains("👍   +1  yours, Enter takes it back"))
        );
        assert!(rows.iter().any(|r| r.contains("│     yolo  1 on it")));
    }

    fn row_with(buffer: &Buffer, needle: &str) -> Option<u16> {
        (0..buffer.area.height).find(|&y| row(buffer, y).contains(needle))
    }

    fn column_of(buffer: &Buffer, y: u16, needle: &str) -> Option<u16> {
        let symbols: Vec<&str> = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        (0..symbols.len())
            .find(|&x| symbols[x..].concat().starts_with(needle))
            .map(|x| x as u16)
    }

    fn has(buffer: &Buffer, x: u16, y: u16, modifier: Modifier) -> bool {
        buffer[(x, y)].style().add_modifier.contains(modifier)
    }

    /// The pane reaches the right edge: its last inner column is one in.
    fn reversed(buffer: &Buffer, y: u16) -> bool {
        has(buffer, buffer.area.width - 2, y, Modifier::REVERSED)
    }

    #[test]
    fn from_the_list_a_history_opens_a_little_above_its_new_line() {
        let mut state = history(&fifty(), Some(41));
        let (buffer, viewport) = render(&state, 100, 30);
        // 30 rows, one for the bottom line, two borders: 27 inside the pane.
        let separator = row_with(&buffer, "── new ──").expect("a separator");
        assert_eq!(separator, 1 + 27 / 4, "a quarter down the pane");
        assert_eq!(
            viewport.anchor,
            40 - 27 / 4,
            "the line of the separator, less that"
        );
        assert_eq!(
            (viewport.height, viewport.rows, viewport.in_view),
            (27, 51, 16),
            "fifty messages and the new line; six read messages above it, ten new below"
        );
        let above = separator - 1;
        let x = column_of(&buffer, above, "message 40").expect("the last read message above");
        assert!(
            has(&buffer, x, above, Modifier::DIM),
            "the read part steps back"
        );
        let below = separator + 1;
        let x = column_of(&buffer, below, "message 41").expect("the first new message below");
        assert!(!has(&buffer, x, below, Modifier::DIM));

        state.scroll = -100;
        let (buffer, _) = render(&state, 100, 30);
        assert!(
            row(&buffer, 1).contains("message 1 "),
            "Ctrl-U stops at the top"
        );
    }

    #[test]
    fn a_read_history_ends_on_its_newest_message() {
        let state = history(&fifty(), None);
        let (buffer, viewport) = render(&state, 100, 30);
        assert!(row_with(&buffer, "── new ──").is_none());
        assert!(
            row(&buffer, 27).contains("message 50"),
            "the last row inside the pane"
        );
        assert_eq!(viewport.anchor, 50 - 27);
        assert_eq!(viewport.in_view, 27);
    }

    #[test]
    fn the_completion_list_sits_above_the_reply_box() {
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let candidate = |insert: &str, detail: &str, search: &str| Candidate {
            insert: insert.to_string(),
            detail: detail.to_string(),
            search: search.to_string(),
        };
        let mut state = history(&["hello"], None);
        state.candidates = Some(vec![
            candidate("@jdoe", "Jane Doe", "jdoe jane doe"),
            candidate("@jsmith", "John Smith · SRE", "jsmith john smith"),
        ]);
        state.key(press(KeyCode::Enter));
        for c in "hi @j".chars() {
            state.key(press(KeyCode::Char(c)));
        }
        let (buffer, _) = render(&state, 100, 30);
        let list = row_with(&buffer, "@jdoe").expect("the list");
        let reply = row_with(&buffer, "hi @j").expect("the box");
        assert!(list < reply, "the list sits above the box");
        assert!(row(&buffer, list).contains("Jane Doe"));
        assert!(row_with(&buffer, " 2 people · ").is_some());
        assert!(row_with(&buffer, "Tab picks, Esc closes the list").is_some());
        let x = column_of(&buffer, list, "@jdoe").expect("the entry");
        assert!(
            has(&buffer, x, list, Modifier::REVERSED),
            "the first is highlighted"
        );
        assert!(!has(&buffer, x, list + 1, Modifier::REVERSED));

        state.key(press(KeyCode::Esc));
        let (buffer, _) = render(&state, 100, 30);
        assert!(row_with(&buffer, "@jdoe").is_none(), "Esc closes the list");
        assert!(row_with(&buffer, "Enter sends, Ctrl-J a new line, Esc cancels").is_some());
        state.status = Some("Error: channel_not_found".to_string());
        let (buffer, _) = render(&state, 100, 30);
        assert!(
            row_with(&buffer, "· Error: channel_not_found ").is_some(),
            "a status takes the hint's place in the title"
        );
    }

    #[test]
    fn the_highlight_covers_the_whole_message_and_stops_at_the_new_line() {
        let long = "xy ".repeat(70);
        let mut state = history(&["one", "two", long.as_str(), "four", "five"], Some(4));
        state.focus = Focus::Messages;
        state.cursor = 2;
        let (buffer, _) = render(&state, 100, 30);
        let first = row_with(&buffer, "xy xy").expect("the long message");
        let separator = row_with(&buffer, "── new ──").expect("a separator");
        assert!(separator > first + 1, "the message wraps over several rows");
        for y in first..separator {
            assert!(
                reversed(&buffer, y),
                "row {y} of the message is highlighted"
            );
        }
        assert!(!reversed(&buffer, first - 1));
        assert!(!reversed(&buffer, separator));
        assert!(!reversed(&buffer, separator + 1));
    }
}
