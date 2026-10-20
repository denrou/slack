//! Drawing the TUI from its state: nothing here changes anything.

use jiff::Zoned;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::model::{Mode, Row, State, rows};
use crate::names::Names;
use crate::{mrkdwn, view};

const HINTS: &str = "j/k move  J/K scroll  Enter reply  r read  R read all  o open  g refresh  a channels  ? keys  q quit";

const HELP: &[&str] = &[
    "j, k, arrows   move through the list",
    "J, K, PgDn/Up  scroll the messages",
    "Enter, s       reply to the selected conversation or thread",
    "r              mark the selected one as read",
    "R              mark everything listed as read (asks first)",
    "o              open it in Slack",
    "g              refresh",
    "a              show or hide the channels without a mention",
    "q, Esc         quit",
];

pub fn draw(frame: &mut Frame, state: &State, names: &Names, now: &Zoned) {
    let bottom_height = match state.mode {
        Mode::Compose(_) => 3,
        Mode::Help => HELP.len() as u16 + 2,
        Mode::Browse | Mode::ConfirmReadAll => 1,
    };
    let [main, bottom] = Layout::vertical([Constraint::Min(3), Constraint::Length(bottom_height)])
        .areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).areas(main);
    draw_list(frame, left, state);
    draw_messages(frame, right, state, names, now);
    draw_bottom(frame, bottom, state);
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
    let list = List::new(lines)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let selected = state
        .selected
        .and_then(|item| super::model::row_of(&rows, item));
    let mut list_state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_messages(frame: &mut Frame, area: Rect, state: &State, names: &Names, now: &Zoned) {
    let Some(item) = state.selected_item() else {
        let text = if state.items.is_empty() {
            "All caught up."
        } else {
            ""
        };
        frame.render_widget(Paragraph::new(text).block(Block::bordered()), area);
        return;
    };
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    let plain = crate::style::Style::plain();
    let times: Vec<String> = item
        .messages
        .iter()
        .map(|m| view::format_ts(&m.ts, now))
        .collect();
    let width = times.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    let indent = " ".repeat(width + 2);

    let mut lines: Vec<Line> = Vec::new();
    for (message, time) in item.messages.iter().zip(times) {
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
        lines.push(Line::from(spans));
        for rest in text_lines {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::raw(rest.to_string()),
            ]));
        }
    }
    if item.has_more {
        lines.push(Line::from("earlier unread messages not shown").dim());
    }
    let title = format!(" {} · {} ", item.title, item.detail);
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_bottom(frame: &mut Frame, area: Rect, state: &State) {
    match &state.mode {
        Mode::Compose(text) => {
            let to = state
                .selected_item()
                .map_or_else(String::new, |item| item.title.clone());
            let title = format!(" Reply to {to} · Enter sends, Esc cancels ");
            let line = Line::from(vec![
                Span::raw(text.as_str()),
                Span::styled("▏", Style::new().dim()),
            ]);
            frame.render_widget(
                Paragraph::new(line).block(Block::bordered().title(title)),
                area,
            );
        }
        Mode::ConfirmReadAll => {
            let text = format!("Mark all {} as read? (y/n)", state.items.len());
            frame.render_widget(Paragraph::new(text).yellow(), area);
        }
        Mode::Help => {
            let lines: Vec<Line> = HELP.iter().map(|line| Line::from(*line)).collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" keys · any key closes ")),
                area,
            );
        }
        Mode::Browse => {
            let text = state.status.as_deref().unwrap_or(HINTS);
            frame.render_widget(Paragraph::new(text).dim(), area);
        }
    }
}
