//! Drawing the TUI from its state: nothing here changes anything.

use jiff::Zoned;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::model::{Focus, Mode, Picker, Row, Section, State, row_of, rows};
use crate::history::Message;
use crate::names::Names;
use crate::{mrkdwn, view};

const LIST_HINTS: &str = "j/k move  l messages  Enter reply  n new  c open any  r read  R read all  o in Slack  g refresh  ? keys  q quit";
const OPENED_HINTS: &str = "j/k move  l messages  Enter reply  n new  x close  c open any  r read  o in Slack  g refresh  ? keys  q quit";
const MESSAGES_HINTS: &str =
    "j/k move  Enter reply to it  l open the thread  u open link  y copy link  h back  q quit";
const THREAD_HINTS: &str =
    "j/k move  Enter reply in the thread  u open link  y copy link  h back  q quit";

const HELP: &[&str] = &[
    "j, k, arrows   move through the list, or through the messages",
    "l, Right, Tab  deeper: the messages, then the highlighted message's thread",
    "h, Left, Esc   back: close the thread, leave the messages, quit",
    "Enter          reply: to the conversation, to the highlighted message, or in the open thread",
    "s              a message to the conversation, wherever you are in it",
    "n              new message: pick a channel or people (Space marks several), then write",
    "c              open any channel, group or person: j/k move, / filters, Space marks people",
    "x              close an opened conversation",
    "u, y           open, or copy, a link of the highlighted message",
    "J, K, PgDn/Up  scroll the messages, from the list",
    "r              mark the selected one as read",
    "R              mark everything listed as read (asks first)",
    "o              open it in Slack",
    "g              refresh",
    "q              quit",
];

/// Returns where the messages pane starts on its own in list focus, for
/// `State::anchor`.
pub fn draw(frame: &mut Frame, state: &State, names: &Names, now: &Zoned) -> u16 {
    let bottom_height = match &state.mode {
        Mode::Compose { text, .. } => compose_rows(text, frame.area().width) + 2,
        Mode::Help => HELP.len() as u16 + 2,
        Mode::Links { links, .. } => links.len() as u16 + 2,
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
    let anchor = draw_messages(frame, right, state, names, now);
    draw_bottom(frame, bottom, state, names);
    anchor
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

fn draw_messages(frame: &mut Frame, area: Rect, state: &State, names: &Names, now: &Zoned) -> u16 {
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
            return 0;
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
        ends.push(lines.len());
    }
    if has_more && !history {
        let note = if in_thread {
            "more replies not shown"
        } else {
            "earlier new replies not shown"
        };
        lines.push(Line::from(note).dim());
    }

    // Scrolling counts rows of the screen, and a long message wraps over
    // several, so the row a line lands on is what the wrapper says, not
    // the line's index.
    let inner_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(2);
    let row_of = |line: usize| -> usize {
        if inner_width == 0 {
            return line;
        }
        Paragraph::new(lines[..line].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(inner_width)
    };
    // From the list, a history starts at its new part with a few rows of
    // what came before above it, or at its end when nothing is new, the
    // way a chat scrolls; `K` shows the rest.
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
    anchor
}

/// The reply box grows with its text, as wrapped at the width of the
/// screen, up to this many rows; a paste can bring several lines.
const COMPOSE_ROWS: u16 = 6;

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
        Mode::Compose { text, thread_ts } => {
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
            let title = format!("{title} · Enter sends, Esc cancels ");
            let mut lines: Vec<Line> = text.split('\n').map(Line::from).collect();
            if let Some(last) = lines.last_mut() {
                last.push_span(Span::styled("▏", Style::new().dim()));
            }
            // The last rows stay in view once the text outgrows the box.
            let inner = area.inner(Margin::new(1, 1));
            let rows = if inner.width == 0 {
                0
            } else {
                Paragraph::new(lines.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(inner.width)
            };
            let scroll = rows.saturating_sub(usize::from(inner.height));
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
                    .block(Block::bordered().title(title)),
                area,
            );
        }
        Mode::ConfirmReadAll => {
            let text = format!("Mark all {} as read? (y/n)", state.items.len());
            frame.render_widget(Paragraph::new(text).yellow(), area);
        }
        Mode::Links { links, copy } => {
            let title = if *copy {
                " Copy which link? · a digit picks, Esc cancels "
            } else {
                " Open which link? · a digit picks, Esc cancels "
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
        Mode::Help => {
            let lines: Vec<Line> = HELP.iter().map(|line| Line::from(*line)).collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" keys · any key closes ")),
                area,
            );
        }
        Mode::Pick(picker) => {
            let keys = match (picker.filtering, picker.compose) {
                (true, _) => "type to filter  Enter back to the list  Esc clear",
                (false, false) => "j/k move  / filter  Space mark people  Enter open  Esc cancel",
                (false, true) => "j/k move  / filter  Space mark people  Enter write  Esc cancel",
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
            let text = format!(
                "{} of {}{to}  {keys}",
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
