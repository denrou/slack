//! Drawing the TUI from its state: nothing here changes anything.

use jiff::Zoned;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_image::StatefulImage;
use ratatui_image::protocol::StatefulProtocol;

use super::model::{
    Completion, Draft, Focus, Item, Mode, Picker, Row, Search, Section, State, Target, Viewport,
    row_of, rows,
};
use crate::history::{File, Message};
use crate::names::Names;
use crate::presence::{Person, Presence, Status};
use crate::{emoji, mrkdwn, view};

const LIST_HINTS: &str = "j/k move  Tab section  Enter messages  r reply  e mark read  x select  c compose  o open  / search  O in Slack  Ctrl-R refresh  ? keys  q quit";
const OPENED_HINTS: &str = "j/k move  Tab section  Enter messages  r reply  e close  x select  c compose  o open  / search  O in Slack  Ctrl-R refresh  ? keys  q quit";
const MESSAGES_HINTS: &str = "j/k move  Enter open the thread  r reply to it  R react  u open link or file  y copy link  e mark read  O in Slack  h back  q quit";
const THREAD_HINTS: &str = "j/k move  r reply in the thread  R react  u open link or file  y copy link  O in Slack  h back  q quit";
const CHECKED_HINTS: &str =
    "x select or unselect  e mark them read  Esc unselect  j/k move  q quit";

/// The highlighted row: the terminal theme's own grey, softer than reversing
/// the colours, which flips the whole row to white on a dark theme.
const HIGHLIGHT: Style = Style::new().bg(Color::DarkGray);

const HELP: &[&str] = &[
    "j, k, arrows    move through the list, or through the messages",
    "l, Enter, Right deeper: the messages, then the highlighted message's thread",
    "h, Left, Esc    back: close the thread, leave the messages; Esc at the list unselects",
    "gg, G           the first, the last (Home, End too)",
    "Tab, Shift-Tab  the next section of the list, the previous one",
    "Ctrl-D, Ctrl-U  half a screen down, up; Ctrl-F, Ctrl-B, PgDn, PgUp a whole one",
    "r               reply: to the conversation, to the highlighted message, or in the open thread",
    "s               a message to the conversation, wherever you are in it",
    "c               compose: pick a channel or people (Tab marks several as you type), then write",
    "o               open a channel or group you are in, or anyone: type to filter, Enter opens; m in the list shows the muted",
    "/               search Slack (in:#channel, from:@person, after:2026-10-01 work): Enter searches, then opens the match in its conversation; / again brings the last search back",
    "@, #            in the reply box, complete a person or a channel: Up/Down move, Tab picks, Esc closes the list",
    "Ctrl-J          a new line in the reply box (Ctrl-Enter, Shift-Enter too where the terminal reports them)",
    "Arrows          in the reply box, move the cursor; with Option a word, with Command to the line's start or end (Home, End, Ctrl-A, Ctrl-E too)",
    "Ctrl-W, Ctrl-U  the word, the line before the cursor, back (Option-, Command-Backspace too where the terminal reports them)",
    "Ctrl-G          in the reply box, correct the accents and typos with fix_command (config.toml); again, your text back",
    "e               mark it as read, or close it, unmarked, if you opened it; with a selection, all of the selection",
    "x               select the conversation for e, or unselect it",
    "u, y            open a link or a file of the highlighted message, or copy its link: an image shows in the pane, where u hands it to the viewer",
    "R               react to the highlighted message: type to filter, Enter puts it on, or takes yours back",
    "O               open it in Slack, at the highlighted message from the pane",
    "Ctrl-R          refresh",
    "q, Ctrl-C       quit",
];

/// Returns what the keys that move by screens need to know about the
/// messages pane, for `State::viewport`. `image` is the picture of
/// `Mode::Image`, encoded for the terminal.
pub fn draw(
    frame: &mut Frame,
    state: &State,
    names: &Names,
    now: &Zoned,
    image: Option<&mut StatefulProtocol>,
) -> Viewport {
    let bottom_height = match &state.mode {
        Mode::Compose(draft) => {
            compose_rows(&draft.text, frame.area().width)
                + 2
                + state.completion(draft).as_ref().map_or(0, list_rows)
        }
        Mode::Help => HELP.len() as u16 + 2,
        Mode::Links { links, .. } => links.len() as u16 + 2,
        Mode::React(reacting) => state.emoji_choices(&reacting.query).len() as u16 + 3,
        Mode::Browse | Mode::Pick(_) | Mode::Search(_) | Mode::Image(_) => 1,
    };
    let [main, bottom] = Layout::vertical([Constraint::Min(3), Constraint::Length(bottom_height)])
        .areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).areas(main);
    let viewport = match &state.mode {
        Mode::Search(search) => {
            draw_search(frame, left, search, names, now);
            draw_hit(frame, right, search, names, now);
            Viewport::default()
        }
        Mode::Pick(picker) => {
            draw_picker(frame, left, picker);
            draw_messages(frame, right, state, names, now)
        }
        Mode::Image(file) => {
            draw_list(frame, left, state, names, now);
            let block = Block::bordered().title(format!(" {} ", file.title));
            let inner = block.inner(right);
            frame.render_widget(block, right);
            if let Some(image) = image {
                frame.render_stateful_widget(StatefulImage::default(), inner, image);
            }
            // The keys keep the messages pane's measures while it is hidden.
            state.viewport
        }
        _ => {
            draw_list(frame, left, state, names, now);
            draw_messages(frame, right, state, names, now)
        }
    };
    draw_bottom(frame, bottom, state, names);
    viewport
}

/// The person of a DM item, None for any other item; Some(None) while
/// nothing is known of them yet.
fn dm_person<'a>(item: &Item, names: &Names, state: &'a State) -> Option<Option<&'a Person>> {
    let user = names
        .dm_user(&item.channel)
        .filter(|_| item.thread_ts.is_none())?;
    Some(state.people.get(user))
}

/// Whether a DM's person is at their screen; a space while not known.
fn presence_dot(presence: Option<Presence>) -> Span<'static> {
    match presence {
        Some(Presence::Active) => Span::styled("● ", Style::new().fg(Color::Green)),
        Some(Presence::Away) => Span::styled("○ ", Style::new().dim()),
        None => Span::raw("  "),
    }
}

fn draw_list(frame: &mut Frame, area: Rect, state: &State, names: &Names, now: &Zoned) {
    let seconds = now.timestamp().as_second();
    let rows = rows(&state.items);
    let lines: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            Row::Header(section) => ListItem::new(Line::from(section.title().bold().dim())),
            Row::Item(index) => {
                let item = &state.items[*index];
                let tick = if state.is_checked(item) { "✓ " } else { "  " };
                let title = if item.muted {
                    Style::new().dim()
                } else {
                    Style::new()
                };
                let mut spans = vec![Span::raw(tick)];
                // A DM's person: whether they are there, their status's
                // emoji, their paused notifications.
                let person = dm_person(item, names, state);
                if let Some(person) = person {
                    spans.push(presence_dot(person.and_then(|person| person.presence)));
                }
                spans.push(Span::styled(item.title.as_str(), title));
                if let Some(person) = person.flatten() {
                    // A custom emoji's `:name:` would crowd the row.
                    let glyph = person.status(seconds).and_then(Status::character);
                    if let Some(glyph) = glyph {
                        spans.push(Span::raw(format!(" {glyph}")));
                    }
                    if person.paused_until(seconds).is_some() {
                        spans.push(Span::raw(" 🔕"));
                    }
                }
                spans.push(Span::raw("  "));
                spans.push(Span::styled(item.detail.as_str(), Style::new().dim()));
                if !state
                    .typists(&item.channel, item.thread_ts.as_deref())
                    .is_empty()
                {
                    spans.push(Span::styled("  ✎", Style::new().dim()));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();
    // What needs you, and what was opened: the recent ones are always there.
    let listed = state
        .items
        .iter()
        .filter(|item| !item.section.is_recent())
        .count();
    let title = match state.checked.len() {
        0 => format!(" slack · {listed} "),
        n => format!(" slack · {listed} · {n} selected "),
    };
    let highlight = if state.focus == Focus::List {
        HIGHLIGHT
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
/// highlighted (on grey when the keys move the list, bold while they
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
        HIGHLIGHT
    };
    let list = List::new(lines)
        .block(Block::bordered().title(title))
        .highlight_style(highlight);
    let selected = (!matches.is_empty()).then(|| picker.selected.min(matches.len() - 1));
    let mut list_state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// The search takes the list's place, as the picker does: the query in
/// the title, each hit on two lines below (where, who, when, then its
/// first line), highlighted as the picker's choices are.
fn draw_search(frame: &mut Frame, area: Rect, search: &Search, names: &Names, now: &Zoned) {
    let title = if search.typing {
        format!(" search › {}▏ ", search.query)
    } else {
        format!(" search › {} ", search.query)
    };
    let block = Block::bordered().title(title);
    if search.searched.is_some() && search.hits.is_empty() {
        frame.render_widget(Paragraph::new("Nothing matches.").dim().block(block), area);
        return;
    }
    let lines: Vec<ListItem> = search
        .hits
        .iter()
        .map(|hit| {
            let mut head = vec![
                Span::raw(names.channel_label(&hit.channel)),
                Span::raw("  "),
            ];
            if let Some(author) = author(&hit.message, names) {
                head.push(Span::styled(author, Style::new().cyan()));
                head.push(Span::raw("  "));
            }
            head.push(Span::styled(
                view::format_ts(&hit.message.ts, now),
                Style::new().dim(),
            ));
            if hit.message.is_reply() {
                head.push(Span::styled("  in thread", Style::new().dim()));
            }
            // A bot's post is often an attachment alone.
            let message = &hit.message;
            let text = Some(plain_text(message, names))
                .filter(|text| !text.trim().is_empty())
                .or_else(|| message.attachments.first().cloned())
                .or_else(|| message.files.first().map(|file| file.title.clone()))
                .unwrap_or_default();
            ListItem::new(vec![
                Line::from(head),
                Line::from(format!("  {}", view::excerpt(&text, 200))),
            ])
        })
        .collect();
    let highlight = if search.typing {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        HIGHLIGHT
    };
    let list = List::new(lines).block(block).highlight_style(highlight);
    let selected = (!search.hits.is_empty()).then(|| search.selected.min(search.hits.len() - 1));
    let mut list_state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// The highlighted hit whole, in the messages pane, before it is opened.
fn draw_hit(frame: &mut Frame, area: Rect, search: &Search, names: &Names, now: &Zoned) {
    let Some(hit) = search.hits.get(search.selected) else {
        frame.render_widget(Block::bordered(), area);
        return;
    };
    let mut title = vec![names.channel_label(&hit.channel)];
    title.extend(author(&hit.message, names));
    title.push(view::format_ts(&hit.message.ts, now));
    let mut lines = message_lines(&hit.message, names);
    let notes = hit.message.files.iter().map(file_note).chain(
        hit.message
            .attachments
            .iter()
            .map(|a| format!("attachment: {a}")),
    );
    for note in notes {
        lines.push(Line::from(note).dim());
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" {} ", title.join(" · ")))),
        area,
    );
}

/// Who wrote a message, as the pane names them.
fn author(message: &Message, names: &Names) -> Option<String> {
    match (&message.user, &message.bot) {
        (Some(user), _) => Some(names.user_label(user)),
        (None, Some(bot)) => Some(format!("{bot} (bot)")),
        (None, None) => None,
    }
}

fn plain_text(message: &Message, names: &Names) -> String {
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    mrkdwn::strip(&message.text, &user, &channel)
}

/// A message's text as lines, its markup styled.
fn message_lines(message: &Message, names: &Names) -> Vec<Line<'static>> {
    let user = |id: &str| names.user_name(id);
    let channel = |id: &str| names.channel_name(id);
    let mut lines = styled_lines(&mrkdwn::pieces(&message.text, &user, &channel), false);
    if lines.len() > 1 && lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    lines
}

/// Text pieces as lines of spans. A draft keeps its markers as typed,
/// dimmed, so what is deleted is what is seen.
fn styled_lines(pieces: &[mrkdwn::Piece], draft: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    for piece in pieces {
        let (text, style) = match piece {
            mrkdwn::Piece::Text(text, look) => (text, look_style(*look)),
            mrkdwn::Piece::Marker { typed, .. } if draft => (typed, Style::new().dim()),
            mrkdwn::Piece::Marker { shown, .. } => (shown, Style::new().dim()),
        };
        for (at, part) in text.split('\n').enumerate() {
            if at > 0 {
                lines.push(Line::default());
            }
            if !part.is_empty()
                && let Some(line) = lines.last_mut()
            {
                line.push_span(Span::styled(part.to_string(), style));
            }
        }
    }
    lines
}

/// The pane's colours: names cyan and mentions yellow, as in its headers.
fn look_style(look: mrkdwn::Look) -> Style {
    let mut style = match look.kind {
        mrkdwn::Kind::Text => Style::new(),
        mrkdwn::Kind::Name => Style::new().cyan(),
        mrkdwn::Kind::Mention => Style::new().yellow().bold(),
        mrkdwn::Kind::Link => Style::new().blue().underlined(),
    };
    if look.code {
        style = style.magenta();
    }
    if look.bold {
        style = style.bold();
    }
    if look.italic {
        style = style.italic();
    }
    if look.strike {
        style = style.crossed_out();
    }
    style
}

/// A file under its message: `u` opens either kind, an image in the pane.
fn file_note(file: &File) -> String {
    let kind = if file.is_image() { "image" } else { "file" };
    format!("{kind}: {}", file.title)
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
            {
                let mut parts = vec![item.title.clone()];
                if !item.detail.is_empty() {
                    parts.push(item.detail.clone());
                }
                let person = dm_person(item, names, state).flatten();
                parts.extend(person.map(|person| person.notes(now)).unwrap_or_default());
                format!(" {} ", parts.join(" · "))
            },
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
    // message, so what is missing comes before it; a thread's page starts
    // at its root, and what is missing comes after.
    let history = !in_thread
        && state
            .selected_item()
            .is_some_and(|item| item.thread_ts.is_none());
    let root_ts = match (&state.thread, state.selected_item()) {
        (Some(thread), _) => Some(thread.thread_ts.as_str()),
        (None, item) => item.and_then(|item| item.thread_ts.as_deref()),
    };
    let messages = state.pane_messages();
    let times: Vec<String> = messages
        .iter()
        .map(|m| view::format_ts(&m.ts, now))
        .collect();
    let width = times.iter().map(|t| t.chars().count()).max().unwrap_or(0);
    let indent = " ".repeat(width + 2);

    let mut lines: Vec<Line> = Vec::new();
    if !in_thread
        && messages.is_empty()
        && let Some(item) = state
            .selected_item()
            .filter(|item| item.section.is_recent() && !item.loaded)
    {
        let text = if state.preview_failed.as_ref() == Some(&item.channel) {
            "Could not load this conversation; Ctrl-R tries again."
        } else {
            "Loading…"
        };
        lines.push(Line::from(text).dim());
    }
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
        if let Some(author) = author(message, names) {
            spans.push(Span::styled(author, Style::new().cyan()));
            spans.push(Span::raw(": "));
        }
        let mut text_lines = message_lines(message, names).into_iter();
        spans.extend(text_lines.next().map(|line| line.spans).unwrap_or_default());
        let mut notes = Vec::new();
        if index == 0 && root_ts == Some(message.ts.as_str()) {
            notes.push("root".to_string());
        }
        match message.reply_count {
            0 => {}
            1 => notes.push("1 reply".to_string()),
            n => notes.push(format!("{n} replies")),
        }
        for file in &message.files {
            notes.push(file_note(file));
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
            let mut spans = vec![Span::raw(indent.clone())];
            spans.extend(rest.spans);
            lines.push(Line::from(spans).style(style));
        }
        // Under the message, as the app has them; yours stand out, since
        // `R` on one of them takes it back.
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
    // A thread is read from its new replies, which can put the root, what
    // the thread is about, above the pane: a thread taller than the pane
    // keeps its top row for the root. Kept before the scroll is worked
    // out, so the height is the same from one frame to the next.
    let keeps_root_row = inner_height > 1
        && total > inner_height
        && messages
            .first()
            .is_some_and(|root| root_ts == Some(root.ts.as_str()));
    let height = height - u16::from(keeps_root_row);
    let inner_height = usize::from(height);
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
    // Once the whole root is above, its first line, as drawn, fills that
    // row; until then the row shows the text like the others.
    let pinned = (keeps_root_row
        && ends
            .first()
            .is_some_and(|&end| row_of(end) <= usize::from(scroll)))
    .then(|| lines[starts[0]].clone().dim());
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
    let shown = match (&state.thread, state.selected_item()) {
        (Some(thread), _) => Some((thread.channel.as_str(), Some(thread.thread_ts.as_str()))),
        (None, item) => item.map(|item| (item.channel.as_str(), item.thread_ts.as_deref())),
    };
    let mut block = Block::bordered().title(title);
    if let Some(typing) = shown
        .and_then(|(channel, thread_ts)| typing_line(&state.typists(channel, thread_ts), names))
    {
        block = block.title_bottom(Line::from(typing).dim());
    }
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(pinned) = pinned {
        frame.render_widget(Paragraph::new(pinned), Rect { height: 1, ..inner });
        inner.y += 1;
        inner.height -= 1;
    }
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, inner);
    // A full-width block over the highlighted message, painted after the
    // text: a line's own style stops where its text does.
    if let Some((from, to)) = highlight {
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
            frame.buffer_mut().set_style(block, HIGHLIGHT);
        }
    }
    Viewport {
        anchor,
        height,
        rows: total,
        in_view,
    }
}

/// On the pane's bottom border, where it moves nothing in the pane.
fn typing_line(typists: &[&str], names: &Names) -> Option<String> {
    let name = |id: &str| names.user_name(id).unwrap_or_else(|| "someone".to_string());
    match typists {
        [] => None,
        [one] => Some(format!(" {} is typing… ", name(one))),
        [one, two] => Some(format!(" {} and {} are typing… ", name(one), name(two))),
        _ => Some(" several people are typing… ".to_string()),
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
/// begins, the highlighted entry on grey.
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
        .highlight_style(HIGHLIGHT);
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

/// The reply box's caret, before the `column`th character of the line.
fn put_caret(line: &mut Line<'static>, column: usize) {
    let caret = Span::styled("▏", Style::new().dim());
    // Characters left to pass before the caret, None once it is placed.
    let mut left = Some(column);
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    for span in line.spans.drain(..) {
        let count = span.content.chars().count();
        match left {
            Some(n) if n < count => {
                let split = span.content.char_indices().nth(n).map_or(0, |(at, _)| at);
                let (head, tail) = span.content.split_at(split);
                if !head.is_empty() {
                    spans.push(Span::styled(head.to_string(), span.style));
                }
                spans.push(caret.clone());
                spans.push(Span::styled(tail.to_string(), span.style));
                left = None;
            }
            Some(n) => {
                left = Some(n - count);
                spans.push(span);
            }
            None => spans.push(span),
        }
    }
    if left.is_some() {
        spans.push(caret);
    }
    line.spans = spans;
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
                text,
                cursor,
                thread_ts,
                ..
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
                None if state.fixes.is_ok() => format!(
                    "{title} · Enter sends, Ctrl-G corrects, Ctrl-J a new line, Esc cancels "
                ),
                None => format!("{title} · Enter sends, Ctrl-J a new line, Esc cancels "),
            };
            let before = &text[..*cursor];
            let mut lines = styled_lines(&mrkdwn::draft(text), true);
            if let Some(line) = lines.get_mut(before.matches('\n').count()) {
                let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count());
                put_caret(line, column);
            }
            // The caret's row stays in view once the text outgrows the box.
            // It is counted with the rest of its word, which the wrap moves
            // to the next row along with it.
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            let inner = area.inner(Margin::new(1, 1));
            let after = &text[*cursor..];
            let word_rest = &after[..after.find(char::is_whitespace).unwrap_or(after.len())];
            let rows = if inner.width == 0 {
                0
            } else {
                Paragraph::new(format!("{before}▏{word_rest}"))
                    .wrap(Wrap { trim: false })
                    .line_count(inner.width)
            };
            let scroll = rows.saturating_sub(usize::from(inner.height));
            frame.render_widget(
                paragraph
                    .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
                    .block(Block::bordered().title(title)),
                area,
            );
        }
        Mode::Links { links, copy } => {
            let title = if *copy {
                " Copy which link? · a digit picks, any other key cancels "
            } else {
                " Open which one? · a digit picks, any other key cancels "
            };
            let lines: Vec<Line> = links
                .iter()
                .enumerate()
                .map(|(index, target)| {
                    let mut spans = vec![Span::styled(
                        format!("{}  ", index + 1),
                        Style::new().bold(),
                    )];
                    match target {
                        Target::Link(link) => {
                            spans.push(Span::raw(link.url.as_str()));
                            if let Some(label) = &link.label {
                                spans.push(Span::styled(format!("  {label}"), Style::new().dim()));
                            }
                        }
                        Target::File(file) => spans.push(Span::raw(file_note(file))),
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
                    line.style(HIGHLIGHT)
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
        Mode::Search(search) => {
            let found = match &search.searched {
                Some(searched) if !search.hits.is_empty() => {
                    format!(
                        "{} of {} for `{searched}` · ",
                        search.hits.len(),
                        search.total
                    )
                }
                _ => String::new(),
            };
            let keys = if !search.typing {
                "j/k move  Enter open it  O in Slack  y copy link  / edit the search  Esc close"
            } else if search.enter_hits() {
                "Ctrl-U clears for a new search  Enter to the matches  Esc close"
            } else {
                "type a query (in:#channel from:@person after:2026-10-01)  Enter search  Esc close"
            };
            let text = match &state.status {
                Some(status) => status.clone(),
                None => format!("{found}{keys}"),
            };
            frame.render_widget(Paragraph::new(text).dim(), area);
        }
        Mode::Image(_) => {
            let hints = "u open it in the viewer  y copy link  any other key closes";
            let text = state.status.as_deref().unwrap_or(hints);
            frame.render_widget(Paragraph::new(text).dim(), area);
        }
        Mode::Browse => {
            let opened = state
                .selected_item()
                .is_some_and(|item| item.section == Section::Opened);
            let hints = match (state.focus, &state.thread) {
                (Focus::List, _) if !state.checked.is_empty() => CHECKED_HINTS,
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
    let text = mrkdwn::strip(&message.text, &user, &channel);
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
            loaded: true,
            muted: false,
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
            .draw(|frame| viewport = draw(frame, state, &Names::default(), &Zoned::now(), None))
            .expect("a draw");
        (terminal.backend().buffer().clone(), viewport)
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// The cell where `text` starts on the first row showing it.
    fn find(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
        (0..buffer.area.height).find_map(|y| {
            let row = row(buffer, y);
            let at = row.find(text)?;
            Some((u16::try_from(row[..at].chars().count()).ok()?, y))
        })
    }

    #[test]
    fn a_dm_shows_whether_its_person_is_there() {
        use crate::names::{Channel, User};
        use crate::presence::Profile;

        let mut state = history(&["hello"], None);
        state.items[0].channel = "D1".to_string();
        state.items[0].title = "@jane".to_string();
        state.people.insert(
            "U1".to_string(),
            Person {
                presence: Some(Presence::Active),
                profile: Some(Profile {
                    status: Some(Status {
                        emoji: "running".to_string(),
                        unicode: None,
                        text: "At the gym".to_string(),
                        expires: None,
                    }),
                    tz: Some("Europe/Brussels".to_string()),
                }),
                dnd: None,
            },
        );
        let dm = Channel {
            name: "U1".to_string(),
            is_private: false,
            is_im: true,
            is_mpim: false,
            user: Some("U1".to_string()),
        };
        let names = Names::with(vec![("U1", User::default())], vec![("D1", dm)]);
        let now: Zoned = "2026-10-23T10:00:00-04:00[America/Toronto]"
            .parse()
            .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(140, 10)).expect("a test terminal");
        terminal
            .draw(|frame| {
                draw(frame, &state, &names, &now, None);
            })
            .expect("a draw");
        let buffer = terminal.backend().buffer().clone();
        let (x, y) = find(&buffer, "● @jane 🏃").expect("the dot and the status's emoji");
        assert_eq!(buffer[(x, y)].fg, Color::Green);
        // The emoji's second cell reads as a blank here.
        let title = row(&buffer, 0);
        assert!(
            title.contains("@jane · 🏃") && title.contains("At the gym · 16:00 in Brussels"),
            "the pane's title: {title}"
        );
    }

    #[test]
    fn the_pane_styles_the_markup_and_hides_its_markers() {
        let state = history(&["*done* with `code`\n&gt; quoted"], None);
        let (buffer, _) = render(&state, 100, 12);
        let (x, y) = find(&buffer, "done with code").expect("the text, markers gone");
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(x + 5, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(x + 10, y)].fg, ratatui::style::Color::Magenta);
        let (_, quote) = find(&buffer, "▎ quoted").expect("the quote, with its bar");
        assert_eq!(quote, y + 1);
    }

    #[test]
    fn the_reply_box_styles_the_markup_and_keeps_its_markers() {
        let mut state = history(&["hello"], None);
        let mut draft = Draft::new(None);
        draft.text = "say *hi*".to_string();
        state.mode = Mode::Compose(draft);
        let (buffer, _) = render(&state, 100, 12);
        let (x, y) = find(&buffer, "say *hi*").expect("the draft as typed");
        assert!(buffer[(x + 4, y)].modifier.contains(Modifier::DIM));
        assert!(buffer[(x + 5, y)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(x, y)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_reply_box_draws_its_caret_at_the_cursor() {
        let mut state = history(&["hello"], None);
        let mut draft = Draft::new(None);
        draft.text = "say *hi*\nthere".to_string();
        draft.cursor = "say *h".len();
        state.mode = Mode::Compose(draft);
        let (buffer, _) = render(&state, 100, 12);
        let (x, y) = find(&buffer, "say *h▏i*").expect("the caret inside the word");
        assert!(
            buffer[(x + 7, y)].modifier.contains(Modifier::BOLD),
            "styled after it too"
        );
        assert!(find(&buffer, "there▏").is_none());

        let Mode::Compose(draft) = &mut state.mode else {
            unreachable!()
        };
        draft.cursor = draft.text.len();
        let (buffer, _) = render(&state, 100, 12);
        assert!(
            find(&buffer, "there▏").is_some(),
            "at the end of the last line"
        );
    }

    #[test]
    fn the_reply_box_keeps_the_caret_in_view_when_its_word_wraps() {
        let mut state = history(&["hello"], None);
        let mut draft = Draft::new(None);
        draft.text = "1\n2\n3\n4\n5\n6\naaa bbbccc".to_string();
        draft.cursor = draft.text.len() - "ccc".len();
        state.mode = Mode::Compose(draft);
        // Eight columns inside the box: `aaa bbb▏` would fit on a row,
        // `aaa bbb▏ccc` does not, and the caret goes down with its word.
        let (buffer, _) = render(&state, 10, 14);
        assert!(
            find(&buffer, "bbb▏ccc").is_some(),
            "the caret's row is shown"
        );
    }

    #[test]
    fn a_recent_conversation_says_when_its_history_failed() {
        let mut state = history(&[] as &[&str], None);
        state.items[0].section = Section::RecentChannels;
        state.items[0].loaded = false;
        let shows = |state: &State, text: &str| {
            let (buffer, _) = render(state, 100, 12);
            (0..12).any(|y| row(&buffer, y).contains(text))
        };
        assert!(shows(&state, "Loading…"));

        state.preview_failed = Some("C1".to_string());
        assert!(shows(&state, "Could not load this conversation"));
        assert!(!shows(&state, "Loading…"));
    }

    #[test]
    fn typing_marks_the_conversation_and_signs_under_its_pane() {
        let mut state = history(&["hello"], None);
        let (buffer, _) = render(&state, 100, 12);
        let rows: Vec<String> = (0..12).map(|y| row(&buffer, y)).collect();
        assert!(!rows.iter().any(|r| r.contains('✎') || r.contains("typing")));

        state.typing.insert(
            ("C1".to_string(), None),
            [("U2".to_string(), std::time::Instant::now())].into(),
        );
        let (buffer, _) = render(&state, 100, 12);
        let rows: Vec<String> = (0..12).map(|y| row(&buffer, y)).collect();
        assert!(
            rows[2].contains("#c1") && rows[2].contains('✎'),
            "{}",
            rows[2]
        );
        // The pane's bottom border, above the status line.
        assert!(rows[10].contains("someone is typing…"), "{}", rows[10]);
    }

    #[test]
    fn reactions_sit_under_their_message_and_r_offers_them() {
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
        state.key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        let (buffer, _) = render(&state, 100, 20);
        let rows: Vec<String> = (0..20).map(|y| row(&buffer, y)).collect();
        assert!(rows.iter().any(|r| r.contains("React to")));
        assert!(
            rows.iter()
                .any(|r| r.contains("👍   +1  yours, Enter takes it back"))
        );
        assert!(rows.iter().any(|r| r.contains("│     yolo  1 on it")));
    }

    #[test]
    fn a_thread_read_from_its_new_replies_keeps_its_root_on_top() {
        let mut texts = fifty();
        texts[0] = "why did the build break".to_string();
        let mut state = history(&texts, Some(45));
        state.items[0].section = Section::Threads;
        state.items[0].thread_ts = Some(ts(1));
        let roots = |buffer: &Buffer| {
            (0..20)
                .filter(|&y| row(buffer, y).contains("why did the build break"))
                .count()
        };

        let (buffer, viewport) = render(&state, 100, 20);
        let pinned = row_with(&buffer, "why did the build break [root]").expect("the root");
        assert_eq!(pinned, 1, "right under the border");
        assert!(row_with(&buffer, "── new ──").is_some_and(|new| new > pinned));
        assert_eq!(roots(&buffer), 1);
        let thread_height = viewport.height;

        state.focus = Focus::Messages;
        state.cursor = 0;
        let (buffer, viewport) = render(&state, 100, 20);
        assert_eq!(roots(&buffer), 1, "the root in view is not pinned again");
        assert_eq!(viewport.height, thread_height, "the same height either way");

        // The thread pane, its last reply highlighted: still on screen
        // under the pinned root.
        let mut root = message(&ts(1), "");
        root.files = vec![file("diagram.png", None)];
        let mut messages = vec![root];
        messages.extend((2..=40).map(|n| message(&ts(n), &format!("reply {n}"))));
        state.open_thread(crate::tui::model::ThreadPane {
            channel: "C1".to_string(),
            thread_ts: ts(1),
            title: "#c1".to_string(),
            messages,
            has_more: false,
            from_message: 0,
        });
        state.cursor = 39;
        let (buffer, _) = render(&state, 100, 20);
        assert_eq!(
            row_with(&buffer, "[root, file: diagram.png]"),
            Some(1),
            "a root without text still says what it is"
        );
        let last = row_with(&buffer, "reply 40").expect("the highlighted reply");
        let x = column_of(&buffer, last, "reply 40").unwrap();
        assert!(highlighted(&buffer, x, last));

        state.thread = None;
        state.items[0].section = Section::Channels;
        state.items[0].thread_ts = None;
        state.focus = Focus::List;
        let (buffer, viewport) = render(&state, 100, 20);
        assert_eq!(roots(&buffer), 0, "a channel pins nothing");
        assert_eq!(viewport.height, thread_height + 1, "the root keeps its row");
    }

    fn file(name: &str, mimetype: Option<&str>) -> File {
        File {
            id: "F1".to_string(),
            title: name.to_string(),
            name: name.to_string(),
            mimetype: mimetype.map(str::to_string),
            url: None,
            thumb: None,
            permalink: None,
        }
    }

    #[test]
    fn an_image_takes_the_messages_pane_and_says_how_to_leave() {
        let mut state = history(&["look"], None);
        let shot = file("shot.png", Some("image/png"));
        state.items[0].messages[0].files = vec![shot.clone()];
        state.focus = Focus::Messages;
        let (buffer, _) = render(&state, 100, 12);
        assert!(
            row_with(&buffer, "[image: shot.png]").is_some(),
            "the note says image"
        );

        state.mode = Mode::Links {
            links: vec![
                Target::Link(crate::mrkdwn::Link {
                    url: "https://a.b/1".to_string(),
                    label: None,
                }),
                Target::File(shot.clone()),
            ],
            copy: false,
        };
        let (buffer, _) = render(&state, 100, 12);
        assert!(row_with(&buffer, "Open which one?").is_some());
        assert!(row_with(&buffer, "2  image: shot.png").is_some());

        state.mode = Mode::Image(shot);
        let (buffer, _) = render(&state, 100, 12);
        let title = row_with(&buffer, " shot.png ").expect("the pane is titled with the file");
        assert_eq!(title, 0);
        assert!(
            row_with(&buffer, "look").is_none(),
            "the image hides the messages"
        );
        assert!(row(&buffer, 11).contains("any other key closes"));
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

    fn highlighted(buffer: &Buffer, x: u16, y: u16) -> bool {
        buffer[(x, y)].bg == Color::DarkGray
    }

    /// The pane reaches the right edge: its last inner column is one in.
    fn highlighted_row(buffer: &Buffer, y: u16) -> bool {
        highlighted(buffer, buffer.area.width - 2, y)
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
        state.key(press(KeyCode::Char('r')));
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
        assert!(highlighted(&buffer, x, list), "the first is highlighted");
        assert!(!highlighted(&buffer, x, list + 1));

        state.key(press(KeyCode::Esc));
        let (buffer, _) = render(&state, 100, 30);
        assert!(row_with(&buffer, "@jdoe").is_none(), "Esc closes the list");
        assert!(row_with(&buffer, "Enter sends, Ctrl-J a new line, Esc cancels").is_some());

        // The correction key is offered only when a command is set.
        state.fixes = Ok(());
        let (buffer, _) = render(&state, 120, 30);
        assert!(row_with(&buffer, "Enter sends, Ctrl-G corrects, Ctrl-J").is_some());
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
                highlighted_row(&buffer, y),
                "row {y} of the message is highlighted"
            );
        }
        assert!(!highlighted_row(&buffer, first - 1));
        assert!(!highlighted_row(&buffer, separator));
        assert!(!highlighted_row(&buffer, separator + 1));
    }

    #[test]
    fn the_search_lists_its_hits_and_shows_the_highlighted_one_whole() {
        let mut state = history(&["hello"], None);
        let mut reply = message(&ts(2), "the deploy failed\nthen it worked");
        reply.thread_ts = Some(ts(1));
        let hits = vec![
            crate::search::Hit {
                channel: "C1".to_string(),
                message: message(&ts(3), "deploy done"),
                permalink: String::new(),
            },
            crate::search::Hit {
                channel: "C2".to_string(),
                message: reply,
                permalink: String::new(),
            },
        ];
        state.mode = Mode::Search(Search {
            query: "deploy".to_string(),
            searched: Some("deploy".to_string()),
            hits,
            total: 7,
            selected: 1,
            typing: false,
        });
        let (buffer, _) = render(&state, 100, 12);
        let rows: Vec<String> = (0..12).map(|y| row(&buffer, y)).collect();
        assert!(rows[0].contains("search › deploy"), "{}", rows[0]);
        assert!(
            rows[1].contains("C1") && rows[2].contains("deploy done"),
            "{}",
            rows[1]
        );
        assert!(
            rows[3].contains("C2") && rows[3].contains("in thread"),
            "{}",
            rows[3]
        );
        assert!(
            highlighted(&buffer, 1, 3) && highlighted(&buffer, 1, 4),
            "the hit is highlighted, both its lines"
        );
        assert!(!highlighted(&buffer, 1, 1));
        assert!(
            rows[2].contains("then it worked"),
            "the pane has it whole: {}",
            rows[2]
        );
        assert!(rows[11].contains("2 of 7 for `deploy`"), "{}", rows[11]);

        state.mode = Mode::Search(Search {
            query: "zzz".to_string(),
            searched: Some("zzz".to_string()),
            typing: true,
            ..Search::default()
        });
        let (buffer, _) = render(&state, 100, 12);
        assert!(row(&buffer, 0).contains("search › zzz▏"));
        assert!(row(&buffer, 1).contains("Nothing matches."));
    }
}
