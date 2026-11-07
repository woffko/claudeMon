//! RECENT screen: agent sessions used in the selected period, whether each
//! one is running, and buttons to resume a stopped one in a new terminal.

use super::{control_group_label, format_age, moment_label, pill, register_right_aligned_targets};
use crate::app::{AppState, RecentRange, UiClickAction, UiHitTarget};
use crate::launch::Placement;
use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::recent::{RecentSession, SessionState};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const DETAILS_HEIGHT: u16 = 7;
const RUN_BUTTON: &str = " RUN ";
const MIDDLE_DOT: &str = "\u{00b7}";

pub(crate) fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let line_area = super::header_line_area(area);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(40)])
        .split(line_area);
    let now = crate::limits::unix_now();
    let visible = state.visible_recent_sessions(now);
    let running = visible
        .iter()
        .filter(|session| session.is_running())
        .count();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "RECENT :: SESSIONS :: {running} RUNNING / {} LISTED",
                visible.len()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        row[0],
    );
    let updated = match state.recent_scanned_at {
        Some(at) => format!("Updated {} ago", format_age(now.saturating_sub(at))),
        None => "Scanning...".to_string(),
    };
    frame.render_widget(Paragraph::new(updated).alignment(Alignment::Right), row[1]);
}

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(DETAILS_HEIGHT),
        ])
        .split(area);
    render_controls(frame, chunks[0], state);
    render_list(frame, chunks[1], state);
    render_details(frame, chunks[2], state);
}

fn render_controls(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let accent = state.accent_text_color();
    let mut segments: Vec<(&str, Option<UiClickAction>)> = vec![(" RANGE ", None)];
    let mut spans = vec![control_group_label("RANGE", accent)];
    for (label, range) in [
        (" DAY ", RecentRange::Day),
        (" WEEK ", RecentRange::Week),
        (" MONTH ", RecentRange::Month),
        (" ALL ", RecentRange::All),
    ] {
        segments.push((label, Some(UiClickAction::SetRecentRange(range))));
        spans.push(pill(range.pill_label(), state.recent_range == range));
    }
    segments.extend([
        (" STYLE ", None),
        (
            " CLASS ",
            Some(UiClickAction::SetDisplayStyle(DisplayStyle::Classic)),
        ),
        (
            " SCOMP ",
            Some(UiClickAction::SetDisplayStyle(DisplayStyle::SystemCompact)),
        ),
        (
            " SFULL ",
            Some(UiClickAction::SetDisplayStyle(DisplayStyle::SystemFull)),
        ),
    ]);
    spans.push(control_group_label("STYLE", accent));
    spans.extend([
        pill("CLASS", state.display_style == DisplayStyle::Classic),
        pill("SCOMP", state.display_style == DisplayStyle::SystemCompact),
        pill("SFULL", state.display_style == DisplayStyle::SystemFull),
    ]);
    register_right_aligned_targets(state, area, &segments);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
        area,
    );
}

fn boxed(title: String) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 1,
            right: 1,
            top: 0,
            bottom: 0,
        })
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Gray),
        ))
}

/// Table columns, widest content first to drop when space runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Status,
    Agent,
    Name,
    Directory,
    LastActive,
    Started,
    Process,
    Session,
    Action,
}

impl Column {
    fn title(self) -> &'static str {
        match self {
            Self::Status => "STATUS",
            Self::Agent => "AGENT",
            Self::Name => "NAME",
            Self::Directory => "DIRECTORY",
            Self::LastActive => "LAST ACTIVE",
            Self::Started => "STARTED",
            Self::Process => "PID / TTY",
            Self::Session => "SESSION",
            Self::Action => "",
        }
    }
}

/// Column widths for `width` cells: optional columns are dropped and the
/// session id shortened until the rest fits; name and directory share what
/// is left.
fn layout_columns(width: usize) -> Vec<(Column, usize)> {
    const GAP: usize = 1;
    let fixed = |session: usize, started: bool, process: bool, agent: bool| {
        let mut columns = vec![(Column::Status, 9)];
        if agent {
            columns.push((Column::Agent, 8));
        }
        columns.push((Column::Name, 0));
        columns.push((Column::Directory, 0));
        columns.push((Column::LastActive, 11));
        if started {
            columns.push((Column::Started, 14));
        }
        if process {
            columns.push((Column::Process, 16));
        }
        if session > 0 {
            columns.push((Column::Session, session));
        }
        columns.push((Column::Action, RUN_BUTTON.len()));
        columns
    };
    let candidates = [
        fixed(36, true, true, true),
        fixed(8, true, true, true),
        fixed(8, false, true, true),
        fixed(8, false, false, true),
        fixed(0, false, false, false),
    ];
    let min_flexible = 30;
    let mut chosen = candidates[candidates.len() - 1].clone();
    for columns in candidates {
        let used: usize = columns.iter().map(|(_, width)| width + GAP).sum();
        if used + min_flexible <= width {
            chosen = columns;
            break;
        }
    }
    let used: usize = chosen.iter().map(|(_, width)| width + GAP).sum();
    let flexible = width.saturating_sub(used).max(12);
    let name = (flexible * 2 / 5).clamp(10, 32);
    let directory = flexible.saturating_sub(name).max(6);
    for (column, width) in &mut chosen {
        match column {
            Column::Name => *width = name,
            Column::Directory => *width = directory,
            _ => {}
        }
    }
    chosen
}

fn render_list(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let accent = state.accent_text_color();
    // RUN opens a tab next to claudemon, or a window when there are no tabs.
    let run_placement = if state.recent_launch_support.1 {
        Placement::Tab
    } else {
        Placement::Window
    };
    let formatter = state.formatter();
    let now = crate::limits::unix_now();
    let block = boxed(format!(
        "SESSIONS {MIDDLE_DOT} {}",
        state.recent_range.label()
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 || inner.width < 20 {
        return;
    }

    let columns = layout_columns(inner.width as usize);
    let header: Vec<Span<'static>> = columns
        .iter()
        .map(|(column, width)| {
            Span::styled(
                format!("{} ", fit(column.title(), *width)),
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(header)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let rows_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    let page_rows = rows_area.height as usize;
    let (rows, targets) = {
        let visible = state.visible_recent_sessions(now);
        if visible.is_empty() {
            let message = if state.recent_scanned_at.is_none() {
                "Scanning sessions...".to_string()
            } else {
                match state.recent_range {
                    RecentRange::All => "No sessions found.".to_string(),
                    range => format!("No sessions in the {}.", range.label()),
                }
            };
            frame.render_widget(
                Paragraph::new(Span::styled(message, Style::default().fg(Color::Gray))),
                rows_area,
            );
            state.recent_page_rows = page_rows;
            return;
        }
        let selected = state.selected_recent_position(&visible);
        let mut scroll = state.recent_scroll.min(visible.len().saturating_sub(1));
        if selected < scroll {
            scroll = selected;
        } else if selected >= scroll + page_rows {
            scroll = selected + 1 - page_rows;
        }
        let mut rows = Vec::new();
        let mut targets = Vec::new();
        for (offset, session) in visible.iter().enumerate().skip(scroll).take(page_rows) {
            let y = rows_area.y + (offset - scroll) as u16;
            let (line, action_x) = row_line(
                session,
                &columns,
                offset == selected,
                now,
                accent,
                formatter,
            );
            if let Some(x) = action_x {
                targets.push(UiHitTarget {
                    area: Rect::new(rows_area.x + x as u16, y, RUN_BUTTON.len() as u16, 1),
                    action: UiClickAction::LaunchRecent(offset, run_placement),
                });
            }
            targets.push(UiHitTarget {
                area: Rect::new(rows_area.x, y, rows_area.width, 1),
                action: UiClickAction::SelectRecent(offset),
            });
            rows.push(line);
        }
        state.recent_scroll = scroll;
        (rows, targets)
    };
    state.recent_page_rows = page_rows;
    state.ui_hit_targets.extend(targets);
    frame.render_widget(Paragraph::new(Text::from(rows)), rows_area);
}

/// One table row, and the x offset of its RUN button when it has one.
fn row_line(
    session: &RecentSession,
    columns: &[(Column, usize)],
    selected: bool,
    now: i64,
    accent: Color,
    formatter: DisplayFormatter<'_>,
) -> (Line<'static>, Option<usize>) {
    let base = if selected {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default()
    };
    let mut spans = Vec::with_capacity(columns.len());
    let mut x = 0;
    let mut action_x = None;
    for (column, width) in columns {
        let (text, style) = match column {
            Column::Status => {
                let (text, color) = status_cell(&session.state, accent);
                (text, base.fg(color))
            }
            Column::Agent => (session.agent.label().to_string(), base.fg(Color::Gray)),
            Column::Name => (session.name.clone(), base.add_modifier(Modifier::BOLD)),
            Column::Directory => (
                session
                    .cwd
                    .as_deref()
                    .map(home_display)
                    .unwrap_or_else(|| "--".to_string()),
                base,
            ),
            Column::LastActive => (
                if session.last_active > 0 {
                    format!(
                        "{} ago",
                        format_age(now.saturating_sub(session.last_active))
                    )
                } else {
                    "--".to_string()
                },
                base.fg(Color::Gray),
            ),
            Column::Started => (
                session
                    .started_at
                    .and_then(|at| moment_label(at, formatter))
                    .unwrap_or_else(|| "--".to_string()),
                base.fg(Color::Gray),
            ),
            Column::Process => (
                match &session.state {
                    SessionState::Running(process) => match process.tty.as_deref() {
                        Some(tty) => format!("{} {tty}", process.pid),
                        None => process.pid.to_string(),
                    },
                    _ => String::new(),
                },
                base.fg(Color::Gray),
            ),
            Column::Session => (session.session_id.clone(), base.fg(Color::Gray)),
            Column::Action => {
                if !session.is_running() && session.resume.is_some() {
                    action_x = Some(x);
                    (
                        RUN_BUTTON.to_string(),
                        Style::default()
                            .fg(Color::Black)
                            .bg(accent)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (String::new(), base)
                }
            }
        };
        let cell = if *column == Column::Directory {
            fit_middle(&text, *width)
        } else {
            fit(&text, *width)
        };
        spans.push(Span::styled(cell, style));
        spans.push(Span::styled(" ", base));
        x += width + 1;
    }
    (Line::from(spans), action_x)
}

fn status_cell(state: &SessionState, accent: Color) -> (String, Color) {
    match state {
        SessionState::Running(process) => {
            let status = process.status.as_deref().unwrap_or("running");
            let color = match status {
                "busy" => accent,
                "waiting" => Color::Yellow,
                "shell" => Color::Cyan,
                _ => Color::Green,
            };
            (format!("\u{25cf} {status}"), color)
        }
        SessionState::Stopped => ("\u{25cb} stopped".to_string(), Color::DarkGray),
        SessionState::Unknown => ("? unknown".to_string(), Color::Gray),
    }
}

fn render_details(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let accent = state.accent_text_color();
    let gray = Style::default().fg(Color::Gray);
    let now = crate::limits::unix_now();
    let block = boxed("SESSION".to_string());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut buttons: Vec<(String, UiClickAction)> = Vec::new();
    {
        let visible = state.visible_recent_sessions(now);
        let position = state.selected_recent_position(&visible);
        if let Some(session) = visible.get(position) {
            let mut heading = vec![
                Span::styled(session.agent.label().to_string(), gray),
                Span::raw(format!("  {MIDDLE_DOT}  ")),
                Span::styled(
                    session.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(title) = session
                .title
                .as_deref()
                .filter(|title| *title != session.name)
            {
                heading.push(Span::raw(format!("  {MIDDLE_DOT}  ")));
                heading.push(Span::raw(title.to_string()));
            }
            lines.push(Line::from(heading));
            let command = session.resume.as_ref().map(|resume| {
                let text = resume.text();
                match session.cwd.as_deref() {
                    Some(cwd) => crate::read::tui::cd_command(cwd, &text),
                    None => text,
                }
            });
            lines.push(Line::from(Span::styled(
                command.unwrap_or_else(|| "No resume command for this session.".to_string()),
                Style::default().fg(accent),
            )));
            match &session.state {
                SessionState::Running(process) => {
                    let mut text = format!("Running: pid {}", process.pid);
                    if let Some(tty) = process.tty.as_deref() {
                        text.push_str(&format!(" on {tty}"));
                    }
                    if let Some(status) = process.status.as_deref() {
                        text.push_str(&format!("  {MIDDLE_DOT}  {status}"));
                    }
                    if let Some(waiting) = process.waiting_for.as_deref() {
                        text.push_str(&format!(" ({waiting})"));
                    }
                    lines.push(Line::from(Span::styled(text, gray)));
                }
                SessionState::Stopped | SessionState::Unknown if session.resume.is_some() => {
                    let (window, tab) = state.recent_launch_support;
                    if window {
                        buttons.push((
                            " NEW WINDOW ".to_string(),
                            UiClickAction::LaunchRecent(position, Placement::Window),
                        ));
                    }
                    if tab {
                        buttons.push((
                            " NEW TAB ".to_string(),
                            UiClickAction::LaunchRecent(position, Placement::Tab),
                        ));
                    }
                    buttons.push((" COPY ".to_string(), UiClickAction::CopyRecentCommand));
                }
                _ => {}
            }
        } else {
            lines.push(Line::from(Span::styled("No session selected.", gray)));
        }
    }

    if !buttons.is_empty() {
        let y = inner.y + lines.len() as u16;
        let mut spans = Vec::new();
        let mut x = inner.x;
        for (label, action) in &buttons {
            let key = match action {
                UiClickAction::LaunchRecent(_, Placement::Window) => "Enter",
                UiClickAction::LaunchRecent(_, Placement::Tab) => "t",
                _ => "y",
            };
            let width = UnicodeWidthStr::width(label.as_str()) as u16;
            if y < inner.y + inner.height {
                state.ui_hit_targets.push(UiHitTarget {
                    area: Rect::new(x, y, width, 1),
                    action: *action,
                });
            }
            spans.push(Span::styled(
                label.clone(),
                Style::default()
                    .fg(Color::Black)
                    .bg(accent)
                    .add_modifier(Modifier::BOLD),
            ));
            let hint = format!(" {key}   ");
            x += width + UnicodeWidthStr::width(hint.as_str()) as u16;
            spans.push(Span::styled(hint, gray));
        }
        lines.push(Line::from(spans));
    }
    if let Some(notice) = state.recent_notice.as_deref() {
        lines.push(Line::from(Span::styled(
            notice.to_string(),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// `~/project` for paths under the home directory.
fn home_display(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home
        .as_deref()
        .filter(|home| *home != Path::new("/"))
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `text` cut to `width` display cells (with `...`) and padded to it.
fn fit(text: &str, width: usize) -> String {
    let clean: String = text.chars().filter(|ch| !ch.is_control()).collect();
    if UnicodeWidthStr::width(clean.as_str()) <= width {
        return pad(clean, width);
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in clean.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width + 3 > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    if width >= 3 {
        out.push_str("...");
    }
    pad(out, width)
}

/// Like `fit`, but keeps the end of the text (the last path components).
fn fit_middle(text: &str, width: usize) -> String {
    let clean: String = text.chars().filter(|ch| !ch.is_control()).collect();
    if UnicodeWidthStr::width(clean.as_str()) <= width || width < 8 {
        return fit(&clean, width);
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in clean.chars().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width + 3 > width {
            break;
        }
        tail.push(ch);
        used += ch_width;
    }
    tail.reverse();
    pad(
        format!("...{}", tail.into_iter().collect::<String>()),
        width,
    )
}

fn pad(text: String, width: usize) -> String {
    let used = UnicodeWidthStr::width(text.as_str());
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_the_width_and_drop_optional_ones_when_narrow() {
        for width in [60, 100, 140, 200] {
            let columns = layout_columns(width);
            let used: usize = columns.iter().map(|(_, w)| w + 1).sum();
            assert!(used <= width.max(80), "width {width}: {columns:?}");
            assert!(columns.iter().any(|(c, _)| *c == Column::Action));
        }
        let wide = layout_columns(200);
        assert!(wide.contains(&(Column::Session, 36)));
        let narrow = layout_columns(60);
        assert!(!narrow.iter().any(|(c, _)| *c == Column::Process));
    }

    #[test]
    fn cells_are_cut_and_padded_to_their_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdefgh", 6), "abc...");
        assert_eq!(fit_middle("/home/user/projects/app", 12), "...jects/app");
        assert_eq!(
            UnicodeWidthStr::width(fit("\u{4e2d}\u{6587}\u{5b57}", 5).as_str()),
            5
        );
    }
}
