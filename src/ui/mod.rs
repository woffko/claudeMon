pub(crate) mod apistat;
pub(crate) mod cost;

use crate::app::{
    AccentTheme, ActiveScreen, AppState, BarFillMode, ChartOrientation, UiClickAction, UiHitTarget,
};
use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::usage::{
    format_compact_kmb, format_count, format_duration_compact, format_tokens_compact,
    format_tokens_overview, ChartRange, ProjectActivity, UsageDay, UsageMetric, UsageZone,
    ACTIVITY_TIMELINE_WEEKS,
};
use anyhow::Result;
use chrono::{
    Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday,
};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Frame, Terminal,
};
use std::collections::BTreeMap;
use std::io::{self, Stdout};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const WEEKLY_PACE_ORANGE_BG: Color = Color::Rgb(160, 80, 0);

const ACTIVITY_PROJECT_HEIGHT: u16 = 9;
const ACTIVITY_PROJECT_STRIDE: u16 = 10;
const ACTIVITY_WEEKDAY_LABEL_WIDTH: u16 = 4;
const ACTIVITY_BASE_COLORS: [Color; 3] =
    [Color::Indexed(23), Color::Indexed(30), Color::Indexed(37)];
const ACTIVITY_COLOR_LEVELS: usize = ACTIVITY_BASE_COLORS.len() + 1;
const TEXTURED_BAR_GLYPH: char = '\u{2591}';
const SOLID_BAR_GLYPH: char = '\u{2588}';
const WEEKLY_GAUGE_DAYS: usize = 7;
const LIMIT_GAUGE_FILL: &str = "\u{2588}";
const LIMIT_GAUGE_DIVIDER: &str = "\u{2595}";
const LIMIT_GAUGE_FILLED_DIVIDER: &str = "\u{2589}";
const USAGE_HEADER_HEIGHT: u16 = 3;

#[derive(Clone)]
struct BarFill {
    glyph: char,
    cell_style: Style,
    value_style: Style,
}

fn alternating_bar_fill(
    index: usize,
    accent_color: Color,
    accent_bright_color: Color,
    mode: BarFillMode,
) -> BarFill {
    match (mode, index.is_multiple_of(2)) {
        (BarFillMode::Semigraphic, true) => BarFill {
            glyph: TEXTURED_BAR_GLYPH,
            cell_style: Style::default().fg(accent_bright_color),
            value_style: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        },
        (BarFillMode::Semigraphic, false) => BarFill {
            glyph: SOLID_BAR_GLYPH,
            cell_style: Style::default().fg(accent_bright_color),
            value_style: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        },
        (BarFillMode::DualColorBackground, true) => BarFill {
            glyph: ' ',
            cell_style: Style::default().bg(accent_color),
            value_style: Style::default()
                .fg(Color::Black)
                .bg(accent_color)
                .add_modifier(Modifier::BOLD),
        },
        (BarFillMode::DualColorBackground, false) => BarFill {
            glyph: ' ',
            cell_style: Style::default().bg(accent_bright_color),
            value_style: Style::default()
                .fg(Color::Black)
                .bg(accent_bright_color)
                .add_modifier(Modifier::BOLD),
        },
    }
}

pub fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Compact duration for ages and countdowns: "45s", "12m", "3h 5m", "2d 4h".
pub(crate) fn format_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// Places `text` on the terminal clipboard with an OSC 52 escape sequence.
pub(crate) fn copy_to_terminal_clipboard(text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))?;
    stdout.flush()?;
    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * index)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn format_updated_label(updated_at: Instant) -> String {
    let secs = updated_at.elapsed().as_secs();
    if secs < 10 {
        "Updated just now".to_string()
    } else if secs < 60 {
        format!("Updated {secs}s ago")
    } else {
        let mins = secs / 60;
        if mins < 60 {
            format!("Updated {mins}m ago")
        } else {
            let hours = mins / 60;
            format!("Updated {hours}h ago")
        }
    }
}

/// Cache hit rate: cache reads as a share of all prompt-side tokens.
fn cache_hit_rate_label(
    prompt_tokens: i64,
    cache_read_tokens: i64,
    formatter: DisplayFormatter<'_>,
) -> String {
    let rate = if prompt_tokens > 0 {
        ((cache_read_tokens as f64) / (prompt_tokens as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };
    format!("Hit Rate {}%", formatter.format_one_decimal(rate))
}

pub fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    state.ui_hit_targets.clear();
    state.usage_scroll_area = None;
    state.activity_scroll_area = None;
    let area = frame.area();
    let (navigation, navigation_targets) = navigation_title(area, state.active_screen);
    state.ui_hit_targets.extend(navigation_targets);
    let (quit, quit_targets) =
        quit_title(area, state.quit_confirm_open, state.skip_quit_confirmation);
    state.ui_hit_targets.extend(quit_targets);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title(Span::styled(
            app_title(),
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .title_top(navigation.right_aligned())
        .title_bottom(quit.right_aligned());
    frame.render_widget(outer, area);

    let inner = apply_margin(
        area,
        Margin {
            vertical: 1,
            horizontal: 2,
        },
    );

    match state.active_screen {
        ActiveScreen::Usage => {
            let footer_height = footer_height(inner.width, state);
            let chunks = usage_screen_layout(inner, footer_height);

            render_header(frame, chunks[0], state);
            render_usage(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area, state.active_screen);
            }
            if state.no_sessions_confirm_open {
                render_no_sessions_overlay(frame, area, state);
            }
        }
        ActiveScreen::Activity => {
            let footer_height = footer_height(inner.width, state);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(footer_height),
                ])
                .split(inner);

            render_activity_header(frame, chunks[0], state);
            render_activity(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area, state.active_screen);
            }
        }
        ActiveScreen::ApiStat => {
            let footer_height = footer_height(inner.width, state);
            let chunks = usage_screen_layout(inner, footer_height);

            apistat::render_header(frame, chunks[0], state);
            apistat::render(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area, state.active_screen);
            }
        }
        ActiveScreen::Cost => {
            let footer_height = footer_height(inner.width, state);
            let chunks = usage_screen_layout(inner, footer_height);

            cost::render_header(frame, chunks[0], state);
            cost::render(frame, chunks[1], state);
            render_footer(frame, chunks[2], state);

            if state.show_help {
                render_help_overlay(frame, area, state.active_screen);
            }
        }
        ActiveScreen::Read => {
            let system_locale = state.system_locale.clone();
            let formatter = DisplayFormatter::new(state.display_style, &system_locale);
            let accent_text_color = state.accent_text_color();
            crate::read::tui::render(
                frame,
                inner,
                &mut state.read_browser,
                state.usage.as_ref(),
                state.usage_error.as_deref(),
                formatter,
                accent_text_color,
            );
            render_history_style_controls(frame, inner, state);
        }
    }

    if state.history_catalog_scan_prompt {
        render_history_catalog_scan_confirmation(frame, area, state);
    } else if state.quit_confirm_open {
        render_quit_confirmation(frame, area, state);
    } else if state.quit_preference_prompt.is_some() {
        render_quit_preference_confirmation(frame, area, state);
    }
}

fn app_title() -> String {
    format!(" claudemon :: {} ", env!("CARGO_PKG_VERSION"))
}

fn usage_screen_layout(area: Rect, footer_height: u16) -> [Rect; 3] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(USAGE_HEADER_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(footer_height),
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2]]
}

fn navigation_title(area: Rect, active_screen: ActiveScreen) -> (Line<'static>, Vec<UiHitTarget>) {
    let title_area = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        1,
    );
    let segments = [
        (" ", None),
        (
            " USAGE ",
            Some(UiClickAction::SetScreen(ActiveScreen::Usage)),
        ),
        (
            " APISTAT ",
            Some(UiClickAction::SetScreen(ActiveScreen::ApiStat)),
        ),
        (" COST ", Some(UiClickAction::SetScreen(ActiveScreen::Cost))),
        (
            " ACTIVITY ",
            Some(UiClickAction::SetScreen(ActiveScreen::Activity)),
        ),
        (
            " HISTORY ",
            Some(UiClickAction::SetScreen(ActiveScreen::Read)),
        ),
    ];
    let app_title_width = UnicodeWidthStr::width(app_title().as_str());
    let navigation_width = segments
        .iter()
        .map(|(text, _)| UnicodeWidthStr::width(*text))
        .sum::<usize>();
    if app_title_width
        .saturating_add(1)
        .saturating_add(navigation_width)
        > title_area.width as usize
    {
        return (Line::default(), Vec::new());
    }

    let line = Line::from(vec![
        Span::raw(" "),
        pill("USAGE", active_screen == ActiveScreen::Usage),
        pill("APISTAT", active_screen == ActiveScreen::ApiStat),
        pill("COST", active_screen == ActiveScreen::Cost),
        pill("ACTIVITY", active_screen == ActiveScreen::Activity),
        pill("HISTORY", active_screen == ActiveScreen::Read),
    ]);
    (line, right_aligned_targets(title_area, &segments))
}

fn quit_title(
    area: Rect,
    active: bool,
    skip_confirmation: bool,
) -> (Line<'static>, Vec<UiHitTarget>) {
    let title_area = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width.saturating_sub(2),
        1,
    );
    let checkbox = if skip_confirmation { " [x] " } else { " [ ] " };
    let segments = [
        (
            checkbox,
            Some(UiClickAction::PromptQuitConfirmationPreference),
        ),
        (" QUIT ", Some(UiClickAction::PromptQuit)),
        (" ", None),
    ];
    let line = Line::from(vec![
        checkbox_span(skip_confirmation, None),
        pill("QUIT", active),
        Span::raw(" "),
    ]);
    (line, right_aligned_targets(title_area, &segments))
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let line_area = header_line_area(area);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(40)])
        .split(line_area);

    let title = "USAGE_SNAPSHOT :: LOCAL";
    let left = Paragraph::new(Line::from(Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Left);
    frame.render_widget(left, row[0]);

    let updated = usage_scan_status_label(state).unwrap_or_else(|| "Updated --".to_string());

    let right = Paragraph::new(updated).alignment(Alignment::Right);
    frame.render_widget(right, row[1]);
}

fn header_line_area(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y.saturating_add(area.height.min(2).saturating_sub(1)),
        width: area.width,
        height: area.height.min(1),
    }
}

fn render_activity_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let line_area = header_line_area(area);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(40)])
        .split(line_area);

    let left = Paragraph::new(Line::from(Span::styled(
        "PROJECT_ACTIVITY",
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Left);
    frame.render_widget(left, row[0]);

    let updated = usage_scan_status_label(state).unwrap_or_else(|| "Updated --".to_string());
    let right = Paragraph::new(updated).alignment(Alignment::Right);
    frame.render_widget(right, row[1]);
}

fn usage_scan_status_label(state: &AppState) -> Option<String> {
    let updated = state
        .usage_updated_label()
        .or_else(|| state.limits_updated_label())?;
    let Some(snapshot) = state.usage.as_ref() else {
        return Some(updated);
    };
    let status = if snapshot.scan_pending_files == 0 {
        "COMPLETE"
    } else {
        "PARTIAL"
    };
    Some(format!(
        "{updated} | {status} {}/{}",
        snapshot.scan_indexed_files, snapshot.scan_total_files
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartScrollbarOwner {
    Usage,
    Activity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartScrollbarAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChartScrollbarViewport {
    total: usize,
    visible: usize,
    offset_from_newest: usize,
}

fn reserve_chart_scrollbar(
    area: Rect,
    axis: ChartScrollbarAxis,
    show: bool,
) -> (Rect, Option<Rect>) {
    if !show {
        return (area, None);
    }
    match axis {
        ChartScrollbarAxis::Vertical if area.width > 2 && area.height >= 3 => (
            Rect::new(area.x, area.y, area.width - 2, area.height),
            Some(Rect::new(
                area.x.saturating_add(area.width - 1),
                area.y,
                1,
                area.height,
            )),
        ),
        ChartScrollbarAxis::Horizontal if area.height > 2 && area.width >= 3 => (
            Rect::new(area.x, area.y, area.width, area.height - 2),
            Some(Rect::new(
                area.x,
                area.y.saturating_add(area.height - 1),
                area.width,
                1,
            )),
        ),
        _ => (area, None),
    }
}

fn render_chart_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    axis: ChartScrollbarAxis,
    viewport: ChartScrollbarViewport,
    owner: ChartScrollbarOwner,
    state: &mut AppState,
) {
    let ChartScrollbarViewport {
        total,
        visible,
        offset_from_newest,
    } = viewport;
    if total <= visible || visible == 0 {
        return;
    }
    let length = match axis {
        ChartScrollbarAxis::Horizontal => area.width,
        ChartScrollbarAxis::Vertical => area.height,
    };
    if length < 3 {
        return;
    }

    let max_offset = total.saturating_sub(visible);
    let track_length = usize::from(length.saturating_sub(2));
    let thumb_length = track_length
        .saturating_mul(visible)
        .saturating_add(total.saturating_sub(1))
        .checked_div(total.max(1))
        .unwrap_or(1)
        .clamp(1, track_length);
    let thumb_travel = track_length.saturating_sub(thumb_length);
    let window_start = max_offset.saturating_sub(offset_from_newest.min(max_offset));
    let thumb_start = window_start
        .saturating_mul(thumb_travel)
        .checked_div(max_offset)
        .unwrap_or(0);

    let (older_char, newer_char) = match axis {
        ChartScrollbarAxis::Horizontal => ('<', '>'),
        ChartScrollbarAxis::Vertical => ('^', 'v'),
    };
    let point_at = |position: u16| match axis {
        ChartScrollbarAxis::Horizontal => (area.x.saturating_add(position), area.y),
        ChartScrollbarAxis::Vertical => (area.x, area.y.saturating_add(position)),
    };
    let rect_at = |position: u16| {
        let (x, y) = point_at(position);
        Rect::new(x, y, 1, 1)
    };
    let (older_action, newer_action) = match owner {
        ChartScrollbarOwner::Usage => (
            UiClickAction::ScrollUsageOlder,
            UiClickAction::ScrollUsageNewer,
        ),
        ChartScrollbarOwner::Activity => (
            UiClickAction::ScrollActivityOlder,
            UiClickAction::ScrollActivityNewer,
        ),
    };
    state.ui_hit_targets.push(UiHitTarget {
        area: rect_at(0),
        action: older_action,
    });
    state.ui_hit_targets.push(UiHitTarget {
        area: rect_at(length - 1),
        action: newer_action,
    });

    let buf = frame.buffer_mut();
    for (position, ch, style) in [
        (
            0,
            older_char,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        (
            length - 1,
            newer_char,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ] {
        let (x, y) = point_at(position);
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch).set_style(style);
        }
    }

    for index in 0..track_length {
        let position = index as u16 + 1;
        let in_thumb = index >= thumb_start && index < thumb_start.saturating_add(thumb_length);
        let (x, y) = point_at(position);
        if let Some(cell) = buf.cell_mut((x, y)) {
            let (ch, style) = chart_scrollbar_cell(in_thumb);
            cell.set_char(ch).set_style(style);
        }
        let logical_start = if track_length <= 1 {
            0
        } else {
            index.saturating_mul(max_offset) / track_length.saturating_sub(1)
        };
        let offset = max_offset.saturating_sub(logical_start);
        let action = match owner {
            ChartScrollbarOwner::Usage => UiClickAction::SetUsageScrollOffset(offset),
            ChartScrollbarOwner::Activity => UiClickAction::SetActivityScrollOffset(offset),
        };
        state.ui_hit_targets.push(UiHitTarget {
            area: rect_at(position),
            action,
        });
    }
}

fn chart_scrollbar_cell(in_thumb: bool) -> (char, Style) {
    if !in_thumb {
        return ('.', Style::default().fg(Color::DarkGray));
    }
    (' ', Style::default().bg(Color::White))
}

fn viewport_bounds(
    total: usize,
    visible_capacity: usize,
    offset_from_newest: usize,
) -> (usize, usize) {
    let visible = total.min(visible_capacity.max(1));
    let max_offset = total.saturating_sub(visible);
    let offset = offset_from_newest.min(max_offset);
    let start = max_offset.saturating_sub(offset);
    (start, start.saturating_add(visible).min(total))
}

fn viewport_label(
    total: usize,
    start: usize,
    end: usize,
    formatter: DisplayFormatter<'_>,
) -> String {
    if total == 0 || (start == 0 && end == total) {
        return formatter.format_usize(total);
    }
    let older = if start > 0 { "<" } else { " " };
    let newer = if end < total { ">" } else { " " };
    format!(
        "{older} {}-{} / {} {newer}",
        formatter.format_usize(start.saturating_add(1)),
        formatter.format_usize(end),
        formatter.format_usize(total)
    )
}

fn usage_horizontal_bar_height(total: usize, height: u16) -> u16 {
    if total == 0 {
        return 1;
    }
    let max_per_bar = height / (total as u16).max(1);
    if max_per_bar >= 5 {
        5
    } else if max_per_bar >= 4 {
        4
    } else if max_per_bar >= 3 {
        3
    } else {
        1
    }
}

fn update_usage_viewport(
    state: &mut AppState,
    area: Rect,
    total: usize,
    visible_capacity: usize,
) -> (usize, usize) {
    let (start, end) = viewport_bounds(total, visible_capacity, state.usage_period_offset);
    state.usage_period_offset = total.saturating_sub(end);
    state.usage_total_periods = total;
    state.usage_visible_periods = end.saturating_sub(start);
    state.usage_scroll_area = Some(area);
    (start, end)
}

fn footer_hint(screen: ActiveScreen) -> &'static str {
    match screen {
        ActiveScreen::Usage => {
            "Usage: Statistic [tab] (tokens/time/runs), Group [g/w] (day/week/month), Layout [f] (horizontal/vertical), Zone [z/F6] (local/UTC), Scroll [wheel/arrows/PgUp/PgDn/Home/End], Refresh [r/F5], Switch [s/F2], Help [?], Quit [q]"
        }
        ActiveScreen::Activity => {
            "Activity: Statistic [tab] (tokens/time/runs), Projects [+/-], Scroll [wheel/left/right/PgUp/PgDn/Home/End], Refresh [r/F5], Switch [s/F2], Help [?], Quit [q]"
        }
        ActiveScreen::ApiStat => {
            "APISTAT: View [v/tab] (limits/models), Dates [d] (all/7d/30d), Copy models [ctrl+s], Refresh limits [r/F5], Switch [s/F2], Help [?], Quit [q]"
        }
        ActiveScreen::Cost => {
            "Cost: Dates [d] (all/7d/30d), Zone [z on USAGE], Refresh [r/F5], Switch [s/F2], Help [?], Quit [q]"
        }
        ActiveScreen::Read => "",
    }
}

fn footer_error(state: &AppState) -> String {
    let err = match state.active_screen {
        ActiveScreen::Usage => state
            .usage_error
            .as_deref()
            .or(state.limits_error.as_deref()),
        ActiveScreen::Activity => state.usage_error.as_deref(),
        ActiveScreen::ApiStat => state.limits_error.as_deref(),
        ActiveScreen::Cost => state.usage_error.as_deref(),
        ActiveScreen::Read => None,
    }
    .unwrap_or("");
    if err.is_empty() {
        String::new()
    } else {
        truncate_middle(err, 80)
    }
}

fn footer_text(state: &AppState) -> String {
    let hint = footer_hint(state.active_screen);
    let style = format!("Style [n]: {}", state.formatter().style_label());
    let err = footer_error(state);
    if err.is_empty() {
        format!("{hint}  {style}")
    } else {
        format!("{hint}  {style}  {err}")
    }
}

fn footer_height(width: u16, state: &AppState) -> u16 {
    wrapped_line_count(&footer_text(state), width).max(1)
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let err = footer_error(state);
    let style = format!("Style [n]: {}", state.formatter().style_label());
    let line = Line::from(vec![
        Span::styled(
            footer_hint(state.active_screen),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(style, Style::default().fg(Color::Gray)),
        Span::raw(if err.is_empty() { "" } else { "  " }),
        Span::styled(err, Style::default().fg(Color::Red)),
    ]);

    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), area);
}

fn render_usage(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let cards_height = usage_cards_height(state, area.width);
    let chunks = usage_layout(area, 1, cards_height);

    render_usage_controls(frame, chunks[0], state);
    let weekly_hover = render_usage_cards(frame, chunks[1], state);
    render_usage_chart(frame, chunks[2], state);
    render_top_models(frame, chunks[3], state);
    if let Some(hover) = weekly_hover {
        render_weekly_pace_tooltip(frame, area, hover.mouse, &hover.text);
    }
}

fn usage_layout(area: Rect, controls_height: u16, cards_height: u16) -> [Rect; 4] {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(controls_height),
            Constraint::Length(cards_height),
            // Cards must keep their measured height. `Min` has higher priority than `Length`
            // in ratatui, so a short terminal would otherwise collapse the cards to borders in
            // order to preserve the chart's minimum height. Let the chart consume the remaining
            // space and degrade first instead.
            Constraint::Fill(1),
            Constraint::Length(3),
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2], chunks[3]]
}

fn render_activity(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let cards_height = usage_cards_height(state, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(cards_height),
            Constraint::Min(1),
        ])
        .split(area);

    render_activity_controls(frame, chunks[0], state);
    let weekly_hover = render_usage_cards(frame, chunks[1], state);
    render_activity_heatmaps(frame, chunks[2], state);
    if let Some(hover) = weekly_hover {
        render_weekly_pace_tooltip(frame, area, hover.mouse, &hover.text);
    }
}

fn render_activity_controls(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let workspace_label = state
        .workspace_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "All workspaces".to_string());

    let tokens = pill("TOKENS", state.metric == UsageMetric::Tokens);
    let time = pill("TIME", state.metric == UsageMetric::Time);
    let runs = pill("RUNS", state.metric == UsageMetric::Runs);
    let project_count = state.activity_project_limit.to_string();

    render_flat_activity_controls(
        frame,
        area,
        state,
        &workspace_label,
        vec![tokens, time, runs],
        project_count,
    );
}

fn render_flat_activity_controls(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut AppState,
    workspace_label: &str,
    metric_pills: Vec<Span<'static>>,
    project_count: String,
) {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Min(0)])
        .split(area);
    render_workspace_label(frame, row[0], workspace_label);

    register_right_aligned_targets(
        state,
        row[1],
        &[
            (" VIEW ", None),
            (
                " TOKENS ",
                Some(UiClickAction::SetMetric(UsageMetric::Tokens)),
            ),
            (" TIME ", Some(UiClickAction::SetMetric(UsageMetric::Time))),
            (" RUNS ", Some(UiClickAction::SetMetric(UsageMetric::Runs))),
            (" PROJECTS ", None),
            (" + ", Some(UiClickAction::IncreaseProjects)),
            (&project_count, None),
            (" -", Some(UiClickAction::DecreaseProjects)),
        ],
    );

    let accent_color = state.accent_text_color();
    let mut spans = vec![control_group_label("VIEW", accent_color)];
    spans.extend(metric_pills);
    spans.push(control_group_label("PROJECTS", accent_color));
    spans.extend(activity_project_control_spans(project_count));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
        row[1],
    );
}

fn activity_project_control_spans(project_count: String) -> [Span<'static>; 3] {
    [
        pill("+", false),
        Span::styled(project_count, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(" -", Style::default().fg(Color::Gray)),
    ]
}

fn render_activity_heatmaps(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let metric_label = match state.metric {
        UsageMetric::Tokens => "TOKENS",
        UsageMetric::Time => "TIME",
        UsageMetric::Runs => "RUNS",
    };
    let inner = inset_with_border_and_padding(
        area,
        Padding {
            left: 2,
            right: 2,
            top: 1,
            bottom: 0,
        },
    );
    if let Some((indexed, total)) = state
        .usage
        .as_ref()
        .filter(|snapshot| snapshot.scan_pending_files > 0)
        .map(|snapshot| (snapshot.scan_indexed_files, snapshot.scan_total_files))
    {
        state.activity_scroll_area = Some(area);
        state.activity_total_weeks = 0;
        state.activity_visible_weeks = 0;
        state.activity_week_offset = 0;
        let formatter = state.formatter();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .title_top(
                Line::from(Span::styled(
                    format!(
                        " INDEXING {} / {} ",
                        formatter.format_usize(indexed),
                        formatter.format_usize(total)
                    ),
                    Style::default().fg(Color::Gray),
                ))
                .left_aligned(),
            )
            .title_top(
                Line::from(Span::styled(
                    format!(" {metric_label} "),
                    Style::default().add_modifier(Modifier::BOLD),
                ))
                .right_aligned(),
            );
        frame.render_widget(block, area);
        render_activity_message(frame, inner, "Indexing local sessions. Please wait.");
        return;
    }
    let grid_width = inner.width.saturating_sub(ACTIVITY_WEEKDAY_LABEL_WIDTH);
    let cell_width = activity_day_cell_width(grid_width, ACTIVITY_TIMELINE_WEEKS);
    let weeks_visible = ACTIVITY_TIMELINE_WEEKS.min((grid_width / cell_width.max(1)) as usize);
    let (chart_inner, scrollbar_area) = reserve_chart_scrollbar(
        inner,
        ChartScrollbarAxis::Horizontal,
        weeks_visible < ACTIVITY_TIMELINE_WEEKS,
    );
    state.activity_scroll_area = Some(area);
    state.activity_total_weeks = ACTIVITY_TIMELINE_WEEKS;
    state.activity_visible_weeks = weeks_visible;
    let max_offset = ACTIVITY_TIMELINE_WEEKS.saturating_sub(weeks_visible);
    state.activity_week_offset = state.activity_week_offset.min(max_offset);
    let older = if state.activity_week_offset < max_offset {
        "<"
    } else {
        " "
    };
    let newer = if state.activity_week_offset > 0 {
        ">"
    } else {
        " "
    };
    let range_title = if max_offset == 0 {
        format!(" Last {ACTIVITY_TIMELINE_WEEKS} weeks ")
    } else {
        format!(
            " {older} Weeks {}-{} of {ACTIVITY_TIMELINE_WEEKS} {newer} ",
            max_offset
                .saturating_sub(state.activity_week_offset)
                .saturating_add(1),
            max_offset
                .saturating_sub(state.activity_week_offset)
                .saturating_add(weeks_visible)
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title_top(
            Line::from(Span::styled(range_title, Style::default().fg(Color::Gray))).left_aligned(),
        )
        .title_top(
            Line::from(Span::styled(
                format!(" {metric_label} "),
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    frame.render_widget(block, area);
    if let Some(scrollbar_area) = scrollbar_area {
        render_chart_scrollbar(
            frame,
            scrollbar_area,
            ChartScrollbarAxis::Horizontal,
            ChartScrollbarViewport {
                total: ACTIVITY_TIMELINE_WEEKS,
                visible: weeks_visible,
                offset_from_newest: state.activity_week_offset,
            },
            ChartScrollbarOwner::Activity,
            state,
        );
    }
    let inner = chart_inner;

    let Some(snapshot) = state.usage.as_ref() else {
        render_activity_message(frame, inner, "Loading activity...");
        return;
    };
    if snapshot.project_activity.is_empty() {
        render_activity_message(frame, inner, "No project activity found.");
        return;
    }
    if inner.width <= ACTIVITY_WEEKDAY_LABEL_WIDTH || inner.height < ACTIVITY_PROJECT_HEIGHT {
        render_activity_message(frame, inner, "Activity view needs more space.");
        return;
    }

    let fit_by_height = ((inner.height.saturating_add(1)) / ACTIVITY_PROJECT_STRIDE).max(1);
    let visible_projects = state
        .activity_project_limit
        .min(snapshot.project_activity.len())
        .min(fit_by_height as usize);

    let accent_color = state.accent_colors().0;
    let mut y = inner.y;
    for project in snapshot.project_activity.iter().take(visible_projects) {
        let slot = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: ACTIVITY_PROJECT_HEIGHT
                .min(inner.y.saturating_add(inner.height).saturating_sub(y)),
        };
        render_project_activity_heatmap(
            frame,
            slot,
            project,
            state.metric,
            snapshot.activity_first_weekday,
            state.formatter(),
            state.activity_week_offset,
            accent_color,
        );
        y = y.saturating_add(ACTIVITY_PROJECT_STRIDE);
        if y >= inner.y.saturating_add(inner.height) {
            break;
        }
    }
}

fn render_activity_message(frame: &mut Frame<'_>, area: Rect, message: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(Color::Gray),
        )))
        .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_project_activity_heatmap(
    frame: &mut Frame<'_>,
    area: Rect,
    project: &ProjectActivity,
    metric: UsageMetric,
    first_weekday: Weekday,
    formatter: DisplayFormatter<'_>,
    week_offset_from_newest: usize,
    accent_color: Color,
) {
    if area.height < ACTIVITY_PROJECT_HEIGHT || area.width <= ACTIVITY_WEEKDAY_LABEL_WIDTH {
        return;
    }

    let weeks_total = project.days.len() / 7;
    if weeks_total == 0 {
        return;
    }
    let grid_width = area.width.saturating_sub(ACTIVITY_WEEKDAY_LABEL_WIDTH);
    let cell_width = activity_day_cell_width(grid_width, weeks_total);
    let weeks_visible = weeks_total.min((grid_width / cell_width) as usize);
    if weeks_visible == 0 {
        return;
    }
    let max_offset = weeks_total.saturating_sub(weeks_visible);
    let week_offset = max_offset.saturating_sub(week_offset_from_newest.min(max_offset));
    let grid_x = area.x.saturating_add(ACTIVITY_WEEKDAY_LABEL_WIDTH);

    let header = format_activity_project_header(project, metric, area.width as usize, formatter);
    let buf = frame.buffer_mut();
    write_text(
        buf,
        area.x,
        area.y,
        area.width,
        &header,
        Style::default().add_modifier(Modifier::BOLD),
    );

    render_activity_month_labels(
        buf,
        project,
        week_offset,
        weeks_visible,
        (grid_x, area.y + 1),
        cell_width,
        formatter,
    );

    let max_value = project
        .days
        .iter()
        .map(|day| activity_day_value(day, metric))
        .max()
        .unwrap_or(0);
    for row in 0..7usize {
        let y = area.y.saturating_add(2 + row as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        write_text(
            buf,
            area.x,
            y,
            ACTIVITY_WEEKDAY_LABEL_WIDTH,
            &activity_weekday_label(first_weekday, row, formatter),
            Style::default().fg(Color::Gray),
        );
        for week in 0..weeks_visible {
            let day_idx = (week_offset + week) * 7 + row;
            let Some(day) = project.days.get(day_idx) else {
                continue;
            };
            let value = activity_day_value(day, metric);
            let level = activity_color_level(value, max_value);
            let x = grid_x.saturating_add((week as u16).saturating_mul(cell_width));
            write_activity_cell(buf, x, y, cell_width, level, accent_color);
        }
    }
}

fn render_activity_month_labels(
    buf: &mut ratatui::buffer::Buffer,
    project: &ProjectActivity,
    week_offset: usize,
    weeks_visible: usize,
    origin: (u16, u16),
    cell_width: u16,
    formatter: DisplayFormatter<'_>,
) {
    let (grid_x, y) = origin;
    let mut next_free_x = grid_x;
    let grid_end = grid_x.saturating_add((weeks_visible as u16).saturating_mul(cell_width));
    for week in 0..weeks_visible {
        let absolute_week = week_offset + week;
        let Some(label) =
            activity_month_label_for_week(project, absolute_week, week == 0, formatter)
        else {
            continue;
        };
        let x = grid_x.saturating_add((week as u16).saturating_mul(cell_width));
        if x < next_free_x || x >= grid_end {
            continue;
        }
        let available = grid_end.saturating_sub(x);
        write_text(
            buf,
            x,
            y,
            (UnicodeWidthStr::width(label.as_str()) as u16).min(available),
            &label,
            Style::default().fg(Color::Gray),
        );
        next_free_x = x
            .saturating_add(UnicodeWidthStr::width(label.as_str()) as u16)
            .saturating_add(1);
    }
}

fn activity_month_label_for_week(
    project: &ProjectActivity,
    week: usize,
    force: bool,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    let start = week.saturating_mul(7);
    let end = start.saturating_add(7).min(project.days.len());
    let days = project.days.get(start..end)?;
    if force {
        let date = parse_activity_date(&days.first()?.day)?;
        return Some(formatter.abbreviated_month(date.month()));
    }
    for day in days {
        let date = parse_activity_date(&day.day)?;
        if date.day() == 1 {
            return Some(formatter.abbreviated_month(date.month()));
        }
    }
    None
}

fn parse_activity_date(day: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()
}

fn format_activity_project_header(
    project: &ProjectActivity,
    metric: UsageMetric,
    max_width: usize,
    formatter: DisplayFormatter<'_>,
) -> String {
    let name = activity_project_name(&project.display_path);
    let active_days = project
        .days
        .iter()
        .filter(|day| activity_day_value(day, metric) > 0)
        .count();
    let total = activity_metric_total_label(project, metric, formatter);
    let last = project
        .last_activity_day
        .as_deref()
        .map(|day| format_activity_date_short(day, formatter))
        .unwrap_or_else(|| "--".to_string());
    truncate_middle(
        &format!(
            "[{name}]  {} days  {total}  last {last}",
            formatter.format_usize(active_days)
        ),
        max_width,
    )
}

fn activity_project_name(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn activity_metric_total_label(
    project: &ProjectActivity,
    metric: UsageMetric,
    formatter: DisplayFormatter<'_>,
) -> String {
    match metric {
        UsageMetric::Tokens => {
            let total = project.total_tokens.max(0) as u64;
            let out_of_cache = (project.total_tokens - project.cache_read_tokens).max(0) as u64;
            let pair = format_horizontal_value(
                total,
                Some(out_of_cache),
                UsageMetric::Tokens,
                20,
                formatter,
            );
            format!("{pair} tokens")
        }
        UsageMetric::Time => format_duration_compact(project.agent_time_ms),
        UsageMetric::Runs => format!("{} runs", format_count(project.agent_runs, formatter)),
    }
}

fn format_activity_date_short(day: &str, formatter: DisplayFormatter<'_>) -> String {
    let Some(date) = parse_activity_date(day) else {
        return day.to_string();
    };
    formatter.format_short_date(date)
}

fn activity_day_value(day: &crate::usage::UsageDay, metric: UsageMetric) -> i64 {
    match metric {
        UsageMetric::Tokens => day.total_tokens.max(0),
        UsageMetric::Time => day.agent_time_ms.max(0),
        UsageMetric::Runs => day.agent_runs.max(0),
    }
}

fn activity_color_level(value: i64, max_value: i64) -> usize {
    if value <= 0 || max_value <= 0 {
        return 0;
    }
    let level = ((value as f64 / max_value as f64) * ACTIVITY_COLOR_LEVELS as f64).ceil() as usize;
    level.clamp(1, ACTIVITY_COLOR_LEVELS)
}

fn activity_day_cell_width(grid_width: u16, weeks_total: usize) -> u16 {
    if weeks_total > 0 && usize::from(grid_width) >= weeks_total.saturating_mul(2) {
        2
    } else {
        1
    }
}

fn activity_weekday_label(
    first_weekday: Weekday,
    row: usize,
    formatter: DisplayFormatter<'_>,
) -> String {
    let monday_row = activity_weekday_row(first_weekday, Weekday::Mon);
    if row < monday_row || !(row - monday_row).is_multiple_of(2) {
        return String::new();
    }
    match activity_weekday_for_row(first_weekday, row) {
        day @ (Weekday::Mon | Weekday::Wed | Weekday::Fri | Weekday::Sun) => {
            formatter.abbreviated_weekday(day)
        }
        _ => String::new(),
    }
}

fn activity_weekday_for_row(first_weekday: Weekday, row: usize) -> Weekday {
    weekday_from_monday_index((first_weekday.num_days_from_monday() as usize + row) % 7)
}

fn activity_weekday_row(first_weekday: Weekday, weekday: Weekday) -> usize {
    let first = first_weekday.num_days_from_monday() as usize;
    let day = weekday.num_days_from_monday() as usize;
    (7 + day - first) % 7
}

fn weekday_from_monday_index(index: usize) -> Weekday {
    match index % 7 {
        0 => Weekday::Mon,
        1 => Weekday::Tue,
        2 => Weekday::Wed,
        3 => Weekday::Thu,
        4 => Weekday::Fri,
        5 => Weekday::Sat,
        _ => Weekday::Sun,
    }
}

fn write_activity_cell(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    width: u16,
    level: usize,
    accent_color: Color,
) {
    let style = if level > 0 {
        Style::default().bg(ACTIVITY_BASE_COLORS
            .get(level.saturating_sub(1))
            .copied()
            .unwrap_or(accent_color))
    } else {
        Style::default()
    };
    for dx in 0..width {
        if let Some(cell) = buf.cell_mut((x.saturating_add(dx), y)) {
            cell.set_char(' ').set_style(style);
        }
    }
}

fn write_text(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    max_width: u16,
    text: &str,
    style: Style,
) {
    if max_width == 0 {
        return;
    }
    for (idx, ch) in text.chars().take(max_width as usize).enumerate() {
        if let Some(cell) = buf.cell_mut((x.saturating_add(idx as u16), y)) {
            cell.set_char(ch).set_style(style);
        }
    }
}

fn render_usage_controls(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let workspace_label = state
        .workspace_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "All workspaces".to_string());

    let tokens = pill("TOKENS", state.metric == UsageMetric::Tokens);
    let time = pill("TIME", state.metric == UsageMetric::Time);
    let runs = pill("RUNS", state.metric == UsageMetric::Runs);
    let day = pill("DAY", state.range == ChartRange::Day);
    let week = pill("WEEK", state.range == ChartRange::Week);
    let month = pill("MONTH", state.range == ChartRange::Month);
    let vert = pill("VERT", state.orientation == ChartOrientation::Vertical);
    let horz = pill("HORZ", state.orientation == ChartOrientation::Horizontal);
    let local = pill("LOCAL", state.usage_zone == UsageZone::Local);
    let utc = pill("UTC", state.usage_zone == UsageZone::Utc);
    let classic = pill("CLASS", state.display_style == DisplayStyle::Classic);
    let system_compact = pill("SCOMP", state.display_style == DisplayStyle::SystemCompact);
    let system_full = pill("SFULL", state.display_style == DisplayStyle::SystemFull);

    render_flat_usage_controls(
        frame,
        area,
        state,
        &workspace_label,
        vec![
            tokens,
            time,
            runs,
            day,
            week,
            month,
            vert,
            horz,
            local,
            utc,
            classic,
            system_compact,
            system_full,
        ],
    );
}

fn render_workspace_label(frame: &mut Frame<'_>, area: Rect, workspace_label: &str) {
    let left = Paragraph::new(Line::from(vec![
        Span::styled("WORKSPACE", Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled(
            truncate_middle(workspace_label, 48),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(left, area);
}

fn render_flat_usage_controls(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut AppState,
    workspace_label: &str,
    pills: Vec<Span<'static>>,
) {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(116)])
        .split(area);
    render_workspace_label(frame, row[0], workspace_label);

    register_right_aligned_targets(
        state,
        row[1],
        &[
            (" VIEW ", None),
            (
                " TOKENS ",
                Some(UiClickAction::SetMetric(UsageMetric::Tokens)),
            ),
            (" TIME ", Some(UiClickAction::SetMetric(UsageMetric::Time))),
            (" RUNS ", Some(UiClickAction::SetMetric(UsageMetric::Runs))),
            (" GRAPH ", None),
            (" DAY ", Some(UiClickAction::SetRange(ChartRange::Day))),
            (" WEEK ", Some(UiClickAction::SetRange(ChartRange::Week))),
            (" MONTH ", Some(UiClickAction::SetRange(ChartRange::Month))),
            (" BARS ", None),
            (
                " VERT ",
                Some(UiClickAction::SetOrientation(ChartOrientation::Vertical)),
            ),
            (
                " HORZ ",
                Some(UiClickAction::SetOrientation(ChartOrientation::Horizontal)),
            ),
            (" ZONE ", None),
            (
                " LOCAL ",
                Some(UiClickAction::SetUsageZone(UsageZone::Local)),
            ),
            (" UTC ", Some(UiClickAction::SetUsageZone(UsageZone::Utc))),
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
        ],
    );

    let accent_color = state.accent_text_color();
    let mut spans = vec![control_group_label("VIEW", accent_color)];
    spans.extend(pills[0..3].iter().cloned());
    spans.push(control_group_label("GRAPH", accent_color));
    spans.extend(pills[3..6].iter().cloned());
    spans.push(control_group_label("BARS", accent_color));
    spans.extend(pills[6..8].iter().cloned());
    spans.push(control_group_label("ZONE", accent_color));
    spans.extend(pills[8..10].iter().cloned());
    spans.push(control_group_label("STYLE", accent_color));
    spans.extend(pills[10..13].iter().cloned());
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
        row[1],
    );
}

fn control_group_label(label: &'static str, accent_color: Color) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(Color::Black)
            .bg(accent_color)
            .add_modifier(Modifier::BOLD),
    )
}

fn render_history_style_controls(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let controls_area = crate::read::tui::history_style_controls_area(area);
    if controls_area.width == 0 {
        return;
    }
    use crate::read::catalog::ProjectViewMode;
    let mode = state.read_browser.project_mode();
    if controls_area.width >= 68 {
        let segments = [
            (" PROJECTS ", None),
            (
                " STRICT ",
                Some(UiClickAction::SetHistoryProjectMode(
                    ProjectViewMode::Strict,
                )),
            ),
            (
                " DEEP  ",
                Some(UiClickAction::SetHistoryProjectMode(ProjectViewMode::Deep)),
            ),
            (
                " FULL ",
                Some(UiClickAction::SetHistoryProjectMode(ProjectViewMode::Full)),
            ),
            (
                " CUSTOM ",
                Some(UiClickAction::SetHistoryProjectMode(
                    ProjectViewMode::Custom,
                )),
            ),
            (" ", None),
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
        ];
        let spans = vec![
            control_group_label("PROJECTS", state.accent_text_color()),
            pill("STRICT", mode == ProjectViewMode::Strict),
            pill("DEEP ", mode == ProjectViewMode::Deep),
            pill("FULL", mode == ProjectViewMode::Full),
            pill("CUSTOM", mode == ProjectViewMode::Custom),
            Span::raw(" "),
            control_group_label("STYLE", state.accent_text_color()),
            pill("CLASS", state.display_style == DisplayStyle::Classic),
            pill("SCOMP", state.display_style == DisplayStyle::SystemCompact),
            pill("SFULL", state.display_style == DisplayStyle::SystemFull),
        ];
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
            controls_area,
        );
        state
            .ui_hit_targets
            .extend(right_aligned_targets(controls_area, &segments));

        if mode == ProjectViewMode::Deep {
            let depth_area = crate::read::tui::history_depth_controls_area(area);
            let depth = format!(" {} ", state.read_browser.deep_depth());
            let depth_segments = [
                (" DEPTH ", Some(UiClickAction::HistoryDepthWheel)),
                (" - ", Some(UiClickAction::DecreaseHistoryDepth)),
                (depth.as_str(), Some(UiClickAction::HistoryDepthWheel)),
                (" + ", Some(UiClickAction::IncreaseHistoryDepth)),
            ];
            let depth_spans = vec![
                control_group_label("DEPTH", state.accent_text_color()),
                pill("-", false),
                pill(&state.read_browser.deep_depth().to_string(), true),
                pill("+", false),
            ];
            frame.render_widget(Paragraph::new(Line::from(depth_spans)), depth_area);
            state
                .ui_hit_targets
                .extend(right_aligned_targets(depth_area, &depth_segments));
        }
    } else {
        let segments = [
            ("  ", None),
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
        ];
        let spans = vec![
            Span::raw("  "),
            control_group_label("STYLE", state.accent_text_color()),
            pill("CLASS", state.display_style == DisplayStyle::Classic),
            pill("SCOMP", state.display_style == DisplayStyle::SystemCompact),
            pill("SFULL", state.display_style == DisplayStyle::SystemFull),
        ];
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Right),
            controls_area,
        );
        state
            .ui_hit_targets
            .extend(right_aligned_targets(controls_area, &segments));
    }
}

#[derive(Debug)]
struct CardSpec {
    lines: Vec<String>,
    reserved_rows: u16,
}

// The LIMITS card paints its gauge after the text, then keeps the normal bottom spacer.
const LIMITS_CARD_RESERVED_ROWS: u16 = 1;

impl CardSpec {
    fn new(value: String, captions: Vec<String>) -> Self {
        Self::new_with_reserved_rows(value, captions, 0)
    }

    fn limits(value: String, captions: Vec<String>) -> Self {
        Self::new_with_reserved_rows(value, captions, LIMITS_CARD_RESERVED_ROWS)
    }

    fn new_with_reserved_rows(value: String, captions: Vec<String>, reserved_rows: u16) -> Self {
        let mut lines = Vec::with_capacity(1 + captions.len());
        lines.push(value);
        for caption in captions {
            if !caption.trim().is_empty() {
                lines.push(caption);
            }
        }
        Self {
            lines,
            reserved_rows,
        }
    }

    fn required_height(&self, card_width: u16) -> u16 {
        const CARD_VERTICAL_CHROME: u16 = 3;
        const CARD_BOTTOM_SPACER: u16 = 1;
        const CARD_MIN_HEIGHT: u16 = 6;
        let content_width = card_width.saturating_sub(5).max(1);
        let mut lines = 0_u16;
        for line in &self.lines {
            lines = lines.saturating_add(wrapped_line_count(line, content_width));
        }
        CARD_VERTICAL_CHROME
            .saturating_add(CARD_BOTTOM_SPACER)
            .saturating_add(lines)
            .saturating_add(self.reserved_rows)
            .max(CARD_MIN_HEIGHT)
    }
}

#[derive(Debug, Clone, Copy)]
struct UsageCardLayout {
    columns: usize,
    min_card_width: u16,
}

fn usage_card_layout(width: u16) -> UsageCardLayout {
    if width >= 180 {
        // The final two cards use 16% of the row, so use that width for safe measurement.
        UsageCardLayout {
            columns: 6,
            min_card_width: width.saturating_mul(16) / 100,
        }
    } else {
        // The smaller 3-card rows use 34% / 33% / 33%; measure against the narrowest card.
        UsageCardLayout {
            columns: 3,
            min_card_width: width.saturating_mul(33) / 100,
        }
    }
}

fn uses_compact_limit_lines(card_width: u16) -> bool {
    // Full reset labels need about 33 cells after the card's border and horizontal padding.
    card_width < 38
}

fn limits_card_content(state: &AppState, compact: bool) -> (String, Vec<String>) {
    let formatter = state.formatter();
    let (value, caption1, caption2, caption3) = if !state.limits_enabled {
        let msg = state
            .limits_error
            .as_deref()
            .or(state.limits_notice.as_deref())
            .unwrap_or("Limits unavailable.");
        ("Unavailable".to_string(), Some(msg.to_string()), None, None)
    } else if let Some(limits) = state.limits.as_ref() {
        format_limits_compact_card_lines(limits, compact, formatter)
    } else {
        ("Loading...".to_string(), None, None, None)
    };

    let captions = [caption1, caption2, caption3]
        .into_iter()
        .flatten()
        .collect();
    (value, captions)
}

fn usage_card_specs(state: &AppState, card_width: u16) -> Vec<CardSpec> {
    let formatter = state.formatter();
    let snapshot = state.usage.as_ref();
    let totals = snapshot
        .map(|snapshot| snapshot.totals_view_for_zone(state.metric, formatter, state.usage_zone));
    let today = snapshot.and_then(|snapshot| snapshot.days_for_zone(state.usage_zone).last());
    let (limits_value, limits_captions) =
        limits_card_content(state, uses_compact_limit_lines(card_width));

    if let Some(pending) = snapshot.filter(|snapshot| snapshot.scan_pending_files > 0) {
        let progress = format!(
            "{} / {} files",
            formatter.format_usize(pending.scan_indexed_files),
            formatter.format_usize(pending.scan_total_files)
        );
        let mut cards = Vec::with_capacity(6);
        cards.push(CardSpec::limits(limits_value, limits_captions));
        for _ in 0..5 {
            cards.push(CardSpec::new(
                "INDEXING".to_string(),
                vec![progress.clone(), "Please wait".to_string()],
            ));
        }
        return cards;
    }

    let today_value = today
        .map(|day| {
            format!(
                "{} tokens",
                format_tokens_overview(day.total_tokens, formatter)
            )
        })
        .unwrap_or_else(|| "--".to_string());
    let today_captions = vec![
        today
            .map(|day| cache_hit_rate_label(day.prompt_tokens(), day.cache_read_tokens, formatter))
            .unwrap_or_default(),
        today
            .map(|day| format!("Runs {}", format_count(day.agent_runs, formatter)))
            .unwrap_or_default(),
        today
            .map(|day| format!("Time {}", format_duration_words(day.agent_time_ms)))
            .unwrap_or_default(),
    ];

    let last7_runs = snapshot
        .map(|snapshot| {
            snapshot
                .last7_days_for_zone(state.usage_zone)
                .iter()
                .map(|day| day.agent_runs)
                .sum::<i64>()
        })
        .map(|runs| format!("Runs {}", format_count(runs, formatter)));
    let last30_runs = snapshot
        .map(|snapshot| {
            snapshot
                .days_for_zone(state.usage_zone)
                .iter()
                .map(|day| day.agent_runs)
                .sum::<i64>()
        })
        .map(|runs| format!("Runs {}", format_count(runs, formatter)));

    let mut cards = Vec::with_capacity(6);
    cards.push(CardSpec::limits(limits_value, limits_captions));
    cards.push(CardSpec::new(today_value, today_captions));

    match state.metric {
        UsageMetric::Tokens => {
            let last7 = totals
                .as_ref()
                .map(|totals| totals.last7_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let avg = totals
                .as_ref()
                .map(|totals| totals.avg_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let last30 = totals
                .as_ref()
                .map(|totals| totals.last30_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let cache = totals
                .as_ref()
                .map(|totals| totals.cache_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak = totals
                .as_ref()
                .map(|totals| totals.peak_day_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak_sub = totals
                .as_ref()
                .map(|totals| totals.peak_sub_label.clone())
                .unwrap_or_default();
            let total = totals
                .as_ref()
                .map(|totals| totals.total_label.clone())
                .unwrap_or_else(|| "--".to_string());

            cards.push(CardSpec::new(
                last7,
                vec![
                    format!("Avg {avg} / day"),
                    last7_runs.clone().unwrap_or_default(),
                ],
            ));
            cards.push(CardSpec::new(
                last30,
                vec![format!("Total {total}"), last30_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(
                cache,
                vec!["Last 7 days".to_string(), last7_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(peak, vec![peak_sub]));
        }
        UsageMetric::Time => {
            let last7 = totals
                .as_ref()
                .map(|totals| totals.last7_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let avg = totals
                .as_ref()
                .map(|totals| totals.avg_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let last30 = totals
                .as_ref()
                .map(|totals| totals.last30_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let runs = totals
                .as_ref()
                .map(|totals| totals.runs_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak = totals
                .as_ref()
                .map(|totals| totals.peak_day_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak_sub = totals
                .as_ref()
                .map(|totals| totals.peak_sub_label.clone())
                .unwrap_or_default();
            let total = totals
                .as_ref()
                .map(|totals| totals.total_label.clone())
                .unwrap_or_else(|| "--".to_string());

            cards.push(CardSpec::new(
                last7,
                vec![format!("Avg {avg} / day"), last7_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(
                last30,
                vec![format!("Total {total}"), last30_runs.unwrap_or_default()],
            ));
            cards.push(CardSpec::new(runs, vec!["Last 7 days".to_string()]));
            cards.push(CardSpec::new(peak, vec![peak_sub]));
        }
        UsageMetric::Runs => {
            let last7 = totals
                .as_ref()
                .map(|totals| totals.last7_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let avg = totals
                .as_ref()
                .map(|totals| totals.avg_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let last30 = totals
                .as_ref()
                .map(|totals| totals.last30_primary_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak = totals
                .as_ref()
                .map(|totals| totals.peak_day_label.clone())
                .unwrap_or_else(|| "--".to_string());
            let peak_sub = totals
                .as_ref()
                .map(|totals| totals.peak_sub_label.clone())
                .unwrap_or_default();
            let avg30 = snapshot
                .filter(|snapshot| !snapshot.days_for_zone(state.usage_zone).is_empty())
                .map(|snapshot| {
                    let days = snapshot.days_for_zone(state.usage_zone);
                    let total_runs = days.iter().map(|day| day.agent_runs).sum::<i64>();
                    format_count(
                        (total_runs as f64 / days.len() as f64).round() as i64,
                        formatter,
                    )
                })
                .unwrap_or_else(|| "--".to_string());
            let tokens7 = snapshot
                .map(|snapshot| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(
                            snapshot.totals_for_zone(state.usage_zone).last7_days_tokens,
                            formatter,
                        )
                    )
                })
                .unwrap_or_else(|| "--".to_string());
            let tokens30 = snapshot
                .map(|snapshot| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(
                            snapshot
                                .totals_for_zone(state.usage_zone)
                                .last30_days_tokens,
                            formatter,
                        )
                    )
                })
                .unwrap_or_else(|| "--".to_string());
            let time7 = snapshot
                .map(|snapshot| {
                    let total_ms = snapshot
                        .last7_days_for_zone(state.usage_zone)
                        .iter()
                        .map(|day| day.agent_time_ms)
                        .sum::<i64>();
                    format_duration_compact(total_ms)
                })
                .unwrap_or_else(|| "--".to_string());

            cards.push(CardSpec::new(
                last7,
                vec![format!("Avg {avg} / day"), tokens7],
            ));
            cards.push(CardSpec::new(
                last30,
                vec![format!("Avg {avg30} / day"), tokens30],
            ));
            cards.push(CardSpec::new(time7, vec!["Last 7 days".to_string()]));
            cards.push(CardSpec::new(peak, vec![peak_sub]));
        }
    }

    cards
}

fn usage_card_row_heights(state: &AppState, width: u16) -> Vec<u16> {
    let layout = usage_card_layout(width);
    let card_width = layout.min_card_width.max(1);
    let cards = usage_card_specs(state, card_width);
    let mut row_heights = Vec::with_capacity(cards.len().div_ceil(layout.columns));
    for row in cards.chunks(layout.columns) {
        row_heights.push(card_row_height(row, card_width));
    }
    row_heights
}

fn card_row_height(cards: &[CardSpec], card_width: u16) -> u16 {
    let mut height = 0_u16;
    for card in cards {
        height = height.max(card.required_height(card_width));
    }
    height
}

fn usage_cards_height(state: &AppState, width: u16) -> u16 {
    let mut total = 0_u16;
    for height in usage_card_row_heights(state, width) {
        total = total.saturating_add(height);
    }
    total
}

fn render_usage_cards(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
) -> Option<WeeklyPaceHover> {
    let mut weekly_hover: Option<WeeklyPaceHover> = None;
    let formatter = state.formatter();
    let today_now = match state.usage_zone {
        UsageZone::Local => Local::now().naive_local(),
        UsageZone::Utc => Utc::now().naive_utc(),
    };
    let today_title = today_card_title(formatter, today_now);
    let card_layout = usage_card_layout(area.width);
    let row_heights = usage_card_row_heights(state, area.width);
    let two_rows = row_heights.len() > 1;
    let (row1, row2) = if two_rows {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(row_heights[0]),
                Constraint::Length(row_heights[1]),
            ])
            .split(area);
        (Some(rows[0]), Some(rows[1]))
    } else {
        (Some(area), None)
    };

    let snapshot = state.usage.as_ref();
    let totals =
        snapshot.map(|s| s.totals_view_for_zone(state.metric, formatter, state.usage_zone));
    let today = snapshot.and_then(|s| s.days_for_zone(state.usage_zone).last());

    if let Some(pending) = state
        .usage
        .as_ref()
        .filter(|snapshot| snapshot.scan_pending_files > 0)
    {
        let progress = format!(
            "{} / {} files",
            formatter.format_usize(pending.scan_indexed_files),
            formatter.format_usize(pending.scan_total_files)
        );
        let aux_title = match state.metric {
            UsageMetric::Tokens => "CACHE_HIT_RATE",
            UsageMetric::Time => "RUNS",
            UsageMetric::Runs => "TIME",
        };
        let render_pending = |frame: &mut Frame<'_>, title: &str, target: Rect| {
            frame.render_widget(
                card(title, "INDEXING", Some(&progress), Some("Please wait")),
                target,
            );
        };
        if let Some(row1) = row1 {
            if row2.is_none() {
                let cards = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(17),
                        Constraint::Percentage(17),
                        Constraint::Percentage(17),
                        Constraint::Percentage(17),
                        Constraint::Percentage(16),
                        Constraint::Percentage(16),
                    ])
                    .split(row1);
                if let Some(hover) = render_limits_card(
                    frame,
                    cards[0],
                    state,
                    uses_compact_limit_lines(card_layout.min_card_width),
                ) {
                    weekly_hover = Some(hover);
                };
                frame.render_widget(
                    card(
                        &today_title,
                        "INDEXING",
                        Some(&progress),
                        Some("Please wait"),
                    ),
                    cards[1],
                );
                for (title, target) in [
                    ("LAST_7_DAYS", cards[2]),
                    ("LAST_30_DAYS", cards[3]),
                    (aux_title, cards[4]),
                    ("PEAK_DAY", cards[5]),
                ] {
                    render_pending(frame, title, target);
                }
            } else {
                let top = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(34),
                        Constraint::Percentage(33),
                        Constraint::Percentage(33),
                    ])
                    .split(row1);
                if let Some(hover) = render_limits_card(
                    frame,
                    top[0],
                    state,
                    uses_compact_limit_lines(card_layout.min_card_width),
                ) {
                    weekly_hover = Some(hover);
                };
                frame.render_widget(
                    card(
                        &today_title,
                        "INDEXING",
                        Some(&progress),
                        Some("Please wait"),
                    ),
                    top[1],
                );
                render_pending(frame, "LAST_7_DAYS", top[2]);
            }
        }
        if let Some(row2) = row2 {
            let bottom = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(34),
                    Constraint::Percentage(33),
                    Constraint::Percentage(33),
                ])
                .split(row2);
            render_pending(frame, "LAST_30_DAYS", bottom[0]);
            render_pending(frame, aux_title, bottom[1]);
            render_pending(frame, "PEAK_DAY", bottom[2]);
        }
        return weekly_hover;
    }

    // TODAY card always shows Tokens / Hit Rate / Runs / Time.
    let today_value = today
        .map(|d| {
            format!(
                "{} tokens",
                format_tokens_overview(d.total_tokens, formatter)
            )
        })
        .unwrap_or_else(|| "--".to_string());
    let today_caption1 = today
        .map(|d| format!("Runs {}", format_count(d.agent_runs, formatter)))
        .unwrap_or_default();
    let today_caption2 = today
        .map(|d| format!("Time {}", format_duration_words(d.agent_time_ms)))
        .unwrap_or_default();

    let last7_runs_sum = snapshot
        .map(|s| {
            s.last7_days_for_zone(state.usage_zone)
                .iter()
                .map(|d| d.agent_runs)
                .sum::<i64>()
        })
        .unwrap_or(0);
    let last30_runs_sum = snapshot
        .map(|s| {
            s.days_for_zone(state.usage_zone)
                .iter()
                .map(|d| d.agent_runs)
                .sum::<i64>()
        })
        .unwrap_or(0);
    let last7_runs_caption = if snapshot.is_some() {
        Some(format!("Runs {}", format_count(last7_runs_sum, formatter)))
    } else {
        None
    };
    let last30_runs_caption = if snapshot.is_some() {
        Some(format!("Runs {}", format_count(last30_runs_sum, formatter)))
    } else {
        None
    };

    match state.metric {
        UsageMetric::Tokens => {
            let last7 = totals
                .as_ref()
                .map(|t| t.last7_primary_label.clone())
                .unwrap_or("--".into());
            let avg = totals
                .as_ref()
                .map(|t| t.avg_primary_label.clone())
                .unwrap_or("--".into());
            let last30 = totals
                .as_ref()
                .map(|t| t.last30_primary_label.clone())
                .unwrap_or("--".into());
            let cache = totals
                .as_ref()
                .map(|t| t.cache_label.clone())
                .unwrap_or("--".into());
            let peak = totals
                .as_ref()
                .map(|t| t.peak_day_label.clone())
                .unwrap_or("--".into());
            let peak_sub = totals
                .as_ref()
                .map(|t| t.peak_sub_label.clone())
                .unwrap_or("".into());
            let total = totals
                .as_ref()
                .map(|t| t.total_label.clone())
                .unwrap_or("--".into());

            if let Some(row1) = row1 {
                if row2.is_none() {
                    let cards = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(16),
                            Constraint::Percentage(16),
                        ])
                        .split(row1);
                    if let Some(hover) = render_limits_card(
                        frame,
                        cards[0],
                        state,
                        uses_compact_limit_lines(card_layout.min_card_width),
                    ) {
                        weekly_hover = Some(hover);
                    };
                    render_today_card(
                        frame,
                        cards[1],
                        state,
                        &today_title,
                        &today_value,
                        Some(&today_caption1),
                        Some(&today_caption2),
                    );
                    let runs7 = last7_runs_caption.as_deref();
                    let runs30 = last30_runs_caption.as_deref();
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        cards[2],
                    );
                    frame.render_widget(
                        card(
                            "LAST_30_DAYS",
                            &last30,
                            Some(&format!("Total {total}")),
                            runs30,
                        ),
                        cards[3],
                    );
                    frame.render_widget(
                        card("CACHE_HIT_RATE", &cache, Some("Last 7 days"), runs7),
                        cards[4],
                    );
                    frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), cards[5]);
                } else {
                    let top = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(34),
                            Constraint::Percentage(33),
                            Constraint::Percentage(33),
                        ])
                        .split(row1);
                    if let Some(hover) = render_limits_card(
                        frame,
                        top[0],
                        state,
                        uses_compact_limit_lines(card_layout.min_card_width),
                    ) {
                        weekly_hover = Some(hover);
                    };
                    let runs7 = last7_runs_caption.as_deref();
                    render_today_card(
                        frame,
                        top[1],
                        state,
                        &today_title,
                        &today_value,
                        Some(&today_caption1),
                        Some(&today_caption2),
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        top[2],
                    );
                }
            }
            if let Some(row2) = row2 {
                let bottom = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(34),
                        Constraint::Percentage(33),
                        Constraint::Percentage(33),
                    ])
                    .split(row2);
                let runs7 = last7_runs_caption.as_deref();
                let runs30 = last30_runs_caption.as_deref();
                frame.render_widget(
                    card(
                        "LAST_30_DAYS",
                        &last30,
                        Some(&format!("Total {total}")),
                        runs30,
                    ),
                    bottom[0],
                );
                frame.render_widget(
                    card("CACHE_HIT_RATE", &cache, Some("Last 7 days"), runs7),
                    bottom[1],
                );
                frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), bottom[2]);
            }
        }
        UsageMetric::Time => {
            let last7 = totals
                .as_ref()
                .map(|t| t.last7_primary_label.clone())
                .unwrap_or("--".into());
            let avg = totals
                .as_ref()
                .map(|t| t.avg_primary_label.clone())
                .unwrap_or("--".into());
            let last30 = totals
                .as_ref()
                .map(|t| t.last30_primary_label.clone())
                .unwrap_or("--".into());
            let runs = totals
                .as_ref()
                .map(|t| t.runs_label.clone())
                .unwrap_or("--".into());
            let peak = totals
                .as_ref()
                .map(|t| t.peak_day_label.clone())
                .unwrap_or("--".into());
            let peak_sub = totals
                .as_ref()
                .map(|t| t.peak_sub_label.clone())
                .unwrap_or("".into());
            let total = totals
                .as_ref()
                .map(|t| t.total_label.clone())
                .unwrap_or("--".into());

            if let Some(row1) = row1 {
                if row2.is_none() {
                    let cards = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(16),
                            Constraint::Percentage(16),
                        ])
                        .split(row1);
                    if let Some(hover) = render_limits_card(
                        frame,
                        cards[0],
                        state,
                        uses_compact_limit_lines(card_layout.min_card_width),
                    ) {
                        weekly_hover = Some(hover);
                    };
                    render_today_card(
                        frame,
                        cards[1],
                        state,
                        &today_title,
                        &today_value,
                        Some(&today_caption1),
                        Some(&today_caption2),
                    );
                    let runs7 = last7_runs_caption.as_deref();
                    let runs30 = last30_runs_caption.as_deref();
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        cards[2],
                    );
                    frame.render_widget(
                        card(
                            "LAST_30_DAYS",
                            &last30,
                            Some(&format!("Total {total}")),
                            runs30,
                        ),
                        cards[3],
                    );
                    frame.render_widget(card("RUNS", &runs, Some("Last 7 days"), None), cards[4]);
                    frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), cards[5]);
                } else {
                    let top = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(34),
                            Constraint::Percentage(33),
                            Constraint::Percentage(33),
                        ])
                        .split(row1);
                    if let Some(hover) = render_limits_card(
                        frame,
                        top[0],
                        state,
                        uses_compact_limit_lines(card_layout.min_card_width),
                    ) {
                        weekly_hover = Some(hover);
                    };
                    let runs7 = last7_runs_caption.as_deref();
                    render_today_card(
                        frame,
                        top[1],
                        state,
                        &today_title,
                        &today_value,
                        Some(&today_caption1),
                        Some(&today_caption2),
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            runs7,
                        ),
                        top[2],
                    );
                }
            }
            if let Some(row2) = row2 {
                let bottom = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(34),
                        Constraint::Percentage(33),
                        Constraint::Percentage(33),
                    ])
                    .split(row2);
                let runs30 = last30_runs_caption.as_deref();
                frame.render_widget(
                    card(
                        "LAST_30_DAYS",
                        &last30,
                        Some(&format!("Total {total}")),
                        runs30,
                    ),
                    bottom[0],
                );
                frame.render_widget(card("RUNS", &runs, Some("Last 7 days"), None), bottom[1]);
                frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), bottom[2]);
            }
        }
        UsageMetric::Runs => {
            let last7 = totals
                .as_ref()
                .map(|t| t.last7_primary_label.clone())
                .unwrap_or("--".into());
            let avg = totals
                .as_ref()
                .map(|t| t.avg_primary_label.clone())
                .unwrap_or("--".into());
            let last30 = totals
                .as_ref()
                .map(|t| t.last30_primary_label.clone())
                .unwrap_or("--".into());
            let peak = totals
                .as_ref()
                .map(|t| t.peak_day_label.clone())
                .unwrap_or("--".into());
            let peak_sub = totals
                .as_ref()
                .map(|t| t.peak_sub_label.clone())
                .unwrap_or("".into());

            let avg30 = snapshot
                .map(|s| {
                    let days = s.days_for_zone(state.usage_zone);
                    if days.is_empty() {
                        0
                    } else {
                        let total_runs = days.iter().map(|d| d.agent_runs).sum::<i64>();
                        (total_runs as f64 / days.len() as f64).round() as i64
                    }
                })
                .unwrap_or(0);
            let avg30_label = if snapshot.is_some() {
                format_count(avg30, formatter)
            } else {
                "--".into()
            };

            let tokens7 = snapshot
                .map(|s| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(
                            s.totals_for_zone(state.usage_zone).last7_days_tokens,
                            formatter,
                        )
                    )
                })
                .unwrap_or_else(|| "--".into());
            let tokens30 = snapshot
                .map(|s| {
                    format!(
                        "Tokens {}",
                        format_tokens_compact(
                            s.totals_for_zone(state.usage_zone).last30_days_tokens,
                            formatter,
                        )
                    )
                })
                .unwrap_or_else(|| "--".into());
            let time7 = snapshot
                .map(|s| {
                    let ms = s
                        .last7_days_for_zone(state.usage_zone)
                        .iter()
                        .map(|d| d.agent_time_ms)
                        .sum::<i64>();
                    format_duration_compact(ms)
                })
                .unwrap_or_else(|| "--".into());

            if let Some(row1) = row1 {
                if row2.is_none() {
                    let cards = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(17),
                            Constraint::Percentage(16),
                            Constraint::Percentage(16),
                        ])
                        .split(row1);
                    if let Some(hover) = render_limits_card(
                        frame,
                        cards[0],
                        state,
                        uses_compact_limit_lines(card_layout.min_card_width),
                    ) {
                        weekly_hover = Some(hover);
                    };
                    render_today_card(
                        frame,
                        cards[1],
                        state,
                        &today_title,
                        &today_value,
                        Some(&today_caption1),
                        Some(&today_caption2),
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            Some(&tokens7),
                        ),
                        cards[2],
                    );
                    frame.render_widget(
                        card(
                            "LAST_30_DAYS",
                            &last30,
                            Some(&format!("Avg {avg30_label} / day")),
                            Some(&tokens30),
                        ),
                        cards[3],
                    );
                    frame.render_widget(card("TIME", &time7, Some("Last 7 days"), None), cards[4]);
                    frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), cards[5]);
                } else {
                    let top = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(34),
                            Constraint::Percentage(33),
                            Constraint::Percentage(33),
                        ])
                        .split(row1);
                    if let Some(hover) = render_limits_card(
                        frame,
                        top[0],
                        state,
                        uses_compact_limit_lines(card_layout.min_card_width),
                    ) {
                        weekly_hover = Some(hover);
                    };
                    render_today_card(
                        frame,
                        top[1],
                        state,
                        &today_title,
                        &today_value,
                        Some(&today_caption1),
                        Some(&today_caption2),
                    );
                    frame.render_widget(
                        card(
                            "LAST_7_DAYS",
                            &last7,
                            Some(&format!("Avg {avg} / day")),
                            Some(&tokens7),
                        ),
                        top[2],
                    );
                }
            }
            if let Some(row2) = row2 {
                let bottom = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(34),
                        Constraint::Percentage(33),
                        Constraint::Percentage(33),
                    ])
                    .split(row2);
                frame.render_widget(
                    card(
                        "LAST_30_DAYS",
                        &last30,
                        Some(&format!("Avg {avg30_label} / day")),
                        Some(&tokens30),
                    ),
                    bottom[0],
                );
                frame.render_widget(card("TIME", &time7, Some("Last 7 days"), None), bottom[1]);
                frame.render_widget(card("PEAK_DAY", &peak, Some(&peak_sub), None), bottom[2]);
            }
        }
    }
    weekly_hover
}

fn aggregate_usage_days(days: &[UsageDay], grouping: ChartRange) -> Vec<UsageDay> {
    let mut grouped = BTreeMap::<NaiveDate, UsageDay>::new();
    for day in days {
        let Ok(date) = NaiveDate::parse_from_str(&day.day, "%Y-%m-%d") else {
            continue;
        };
        let start = usage_period_start(date, grouping);
        let entry = grouped.entry(start).or_insert_with(|| UsageDay {
            day: start.format("%Y-%m-%d").to_string(),
            ..UsageDay::default()
        });
        entry.input_tokens = entry.input_tokens.saturating_add(day.input_tokens);
        entry.cache_write_tokens = entry
            .cache_write_tokens
            .saturating_add(day.cache_write_tokens);
        entry.cache_read_tokens = entry
            .cache_read_tokens
            .saturating_add(day.cache_read_tokens);
        entry.output_tokens = entry.output_tokens.saturating_add(day.output_tokens);
        entry.total_tokens = entry.total_tokens.saturating_add(day.total_tokens);
        entry.agent_time_ms = entry.agent_time_ms.saturating_add(day.agent_time_ms);
        entry.agent_runs = entry.agent_runs.saturating_add(day.agent_runs);
    }
    grouped.into_values().collect()
}

fn usage_period_start(date: NaiveDate, grouping: ChartRange) -> NaiveDate {
    match grouping {
        ChartRange::Day => date,
        ChartRange::Week => {
            date - ChronoDuration::days(date.weekday().num_days_from_monday() as i64)
        }
        ChartRange::Month => NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date),
    }
}

fn usage_period_end(start: NaiveDate, grouping: ChartRange) -> NaiveDate {
    match grouping {
        ChartRange::Day => start,
        ChartRange::Week => start + ChronoDuration::days(6),
        ChartRange::Month => {
            let (year, month) = if start.month() == 12 {
                (start.year().saturating_add(1), 1)
            } else {
                (start.year(), start.month() + 1)
            };
            NaiveDate::from_ymd_opt(year, month, 1)
                .map(|next| next - ChronoDuration::days(1))
                .unwrap_or(start)
        }
    }
}

fn format_usage_period_label(
    start: NaiveDate,
    grouping: ChartRange,
    formatter: DisplayFormatter<'_>,
    compact: bool,
) -> String {
    match grouping {
        ChartRange::Day if compact => format!("{:02}", start.day()),
        ChartRange::Day => {
            format_day_label_weekday_mmdd(&start.format("%Y-%m-%d").to_string(), formatter)
        }
        ChartRange::Week if compact => format!("W{:02}", start.iso_week().week()),
        ChartRange::Week => {
            let end = usage_period_end(start, grouping);
            let range = if start.month() == end.month() {
                format!(
                    "{} {}-{}",
                    formatter.abbreviated_month(start.month()),
                    start.day(),
                    end.day()
                )
            } else {
                format!(
                    "{} {}-{} {}",
                    formatter.abbreviated_month(start.month()),
                    start.day(),
                    formatter.abbreviated_month(end.month()),
                    end.day()
                )
            };
            format!("W{:02} ({range})", start.iso_week().week())
        }
        ChartRange::Month => format!(
            "{} {}",
            formatter.abbreviated_month(start.month()),
            start.year()
        ),
    }
}

fn format_usage_period_tooltip(
    start: NaiveDate,
    grouping: ChartRange,
    formatter: DisplayFormatter<'_>,
) -> String {
    if grouping == ChartRange::Day {
        formatter.format_full_date(start)
    } else {
        format!(
            "{} - {}",
            formatter.format_full_date(start),
            formatter.format_full_date(usage_period_end(start, grouping))
        )
    }
}

fn render_usage_chart(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    // Note: We draw bars manually to control label placement and padding.
    let (accent_color, accent_bright_color) = state.accent_colors();
    let range_label = match state.range {
        ChartRange::Day => "Usage by day",
        ChartRange::Week => "Usage by ISO week",
        ChartRange::Month => "Usage by month",
    };
    // Inner area: account for borders and provide padding (top padding = 1 line).
    let inner = inset_with_border_and_padding(
        area,
        Padding {
            left: 2,
            right: 2,
            top: 1,
            bottom: 0,
        },
    );

    if let Some(pending) = state
        .usage
        .as_ref()
        .filter(|snapshot| snapshot.scan_pending_files > 0)
    {
        state.usage_period_offset = 0;
        state.usage_visible_periods = 0;
        state.usage_total_periods = 0;
        state.usage_scroll_area = Some(area);
        let formatter = state.formatter();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .title_top(
                Line::from(Span::styled(
                    format!(" {range_label} "),
                    Style::default().fg(Color::Gray),
                ))
                .left_aligned(),
            )
            .title_top(
                Line::from(Span::styled(
                    format!(
                        " INDEXING {} / {} ",
                        formatter.format_usize(pending.scan_indexed_files),
                        formatter.format_usize(pending.scan_total_files)
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                ))
                .right_aligned(),
            );
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "INDEXING LOCAL SESSIONS",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from("Please wait. Values appear when the local snapshot is complete."),
            ]))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let all_days = state
        .usage
        .as_ref()
        .map(|snapshot| aggregate_usage_days(snapshot.days_for_zone(state.usage_zone), state.range))
        .unwrap_or_default();
    let visible_capacity = match state.orientation {
        ChartOrientation::Vertical => vertical_bar_capacity(inner.width),
        ChartOrientation::Horizontal => {
            let bar_height = usage_horizontal_bar_height(all_days.len(), inner.height);
            usize::from((inner.height / bar_height).max(1))
        }
    };
    let scrollbar_axis = match state.orientation {
        ChartOrientation::Vertical => ChartScrollbarAxis::Horizontal,
        ChartOrientation::Horizontal => ChartScrollbarAxis::Vertical,
    };
    let (chart_inner, scrollbar_area) =
        reserve_chart_scrollbar(inner, scrollbar_axis, all_days.len() > visible_capacity);
    let (start, end) = update_usage_viewport(state, area, all_days.len(), visible_capacity);
    let days = &all_days[start..end];
    let formatter = state.formatter();
    let metric_label = usage_chart_metric_label(state.metric);
    let range_title = if all_days.len() == days.len() {
        range_label.to_string()
    } else {
        format!(
            "{range_label} :: {}",
            viewport_label(all_days.len(), start, end, formatter)
        )
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title_top(
            Line::from(Span::styled(
                format!(" {range_title} "),
                Style::default().fg(Color::Gray),
            ))
            .left_aligned(),
        );
    // Only show TOKENS/TIME in horizontal mode, per request.
    if state.orientation == ChartOrientation::Horizontal {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" {metric_label} "),
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    }
    frame.render_widget(block, area);

    let inner = chart_inner;

    if inner.height < 2 {
        return;
    }

    let mut labels: Vec<String> = Vec::with_capacity(days.len());
    let mut tooltip_labels: Vec<String> = Vec::with_capacity(days.len());
    let mut values: Vec<u64> = Vec::with_capacity(days.len());
    let mut token_columns: Vec<TokenColumns> = Vec::with_capacity(days.len());
    for day in days {
        let date = NaiveDate::parse_from_str(&day.day, "%Y-%m-%d").ok();
        let label = date
            .map(|date| {
                format_usage_period_label(
                    date,
                    state.range,
                    formatter,
                    state.orientation == ChartOrientation::Vertical,
                )
            })
            .unwrap_or_else(|| day.day.clone());
        let tooltip = date
            .map(|date| format_usage_period_tooltip(date, state.range, formatter))
            .unwrap_or_else(|| day.day.clone());
        labels.push(label);
        tooltip_labels.push(tooltip);
    }
    for day in days {
        let value = match state.metric {
            UsageMetric::Tokens => day.total_tokens.max(0) as u64,
            UsageMetric::Time => (day.agent_time_ms.max(0) as u64) / 60_000,
            UsageMetric::Runs => day.agent_runs.max(0) as u64,
        };
        values.push(value);
        token_columns.push(usage_day_token_columns(day));
    }

    match state.orientation {
        ChartOrientation::Vertical => {
            if values.is_empty() || inner.width < 3 || inner.height < 3 {
                return;
            }

            // 1 line for labels at the bottom, remaining for bars.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(2), Constraint::Length(1)])
                .split(inner);
            let bars_area = chunks[0];
            let labels_area = chunks[1];

            let (bar_width, bar_gap) = compute_bar_layout(bars_area, values.len() as u16);
            let bw = bar_width.max(1);

            let max_value = values.iter().copied().max().unwrap_or(0).max(1);
            let hovered_bar = hovered_vertical_bar_index(
                state.mouse_position,
                bars_area,
                bw,
                bar_gap,
                &values,
                max_value,
            );
            let hover_tooltip = hovered_bar.and_then(|index| {
                state.mouse_position.map(|mouse| {
                    (
                        mouse,
                        format_vertical_bar_tooltip(
                            &tooltip_labels[index],
                            values[index],
                            state.metric,
                            formatter,
                        ),
                    )
                })
            });
            let buf = frame.buffer_mut();

            // Bars are laid left-to-right.
            for (i, (label, value)) in labels.iter().zip(values.iter()).enumerate() {
                let x0 = bars_area
                    .x
                    .saturating_add((i as u16).saturating_mul(bw.saturating_add(bar_gap)));
                if x0 >= bars_area.x.saturating_add(bars_area.width) {
                    break;
                }
                let w = bw.min(
                    bars_area
                        .x
                        .saturating_add(bars_area.width)
                        .saturating_sub(x0),
                );
                if w == 0 {
                    break;
                }

                let bar_fill =
                    alternating_bar_fill(i, accent_color, accent_bright_color, state.bar_fill_mode);

                let ratio = (*value as f64) / (max_value as f64);
                let filled_h = ((bars_area.height as f64) * ratio.clamp(0.0, 1.0)).round() as u16;

                // Fill bar area with background for filled portion.
                let bottom_y = bars_area
                    .y
                    .saturating_add(bars_area.height)
                    .saturating_sub(1);
                let top_filled_y = bottom_y.saturating_sub(filled_h.saturating_sub(1));
                for yy in bars_area.y..bars_area.y.saturating_add(bars_area.height) {
                    for xx in x0..x0.saturating_add(w) {
                        if let Some(cell) = buf.cell_mut((xx, yy)) {
                            cell.set_char(' ');
                            if filled_h > 0 && yy >= top_filled_y {
                                cell.set_char(bar_fill.glyph)
                                    .set_style(bar_fill.cell_style.clone());
                            }
                        }
                    }
                }

                // Value text inside bar, one line above the bottom.
                if bars_area.height >= 3 && filled_h >= 2 {
                    let text_y = bottom_y.saturating_sub(1); // 1-line bottom space
                    let text = match state.metric {
                        UsageMetric::Tokens => format_vertical_token_value(*value, w, formatter),
                        UsageMetric::Time => {
                            let raw = format_minutes_hhmm(*value);
                            if raw.len() <= w as usize {
                                raw
                            } else if w >= 4 {
                                // fallback: compact minutes
                                format_compact_kmb(*value, w, formatter)
                            } else {
                                String::new()
                            }
                        }
                        UsageMetric::Runs => {
                            let raw = formatter.format_u64(*value);
                            if raw.len() <= w as usize {
                                raw
                            } else {
                                format_compact_kmb(*value, w, formatter)
                            }
                        }
                    };
                    let text = truncate_middle(&text, w as usize);
                    let start_x = x0.saturating_add((w.saturating_sub(text.len() as u16)) / 2);
                    for (j, ch) in text.chars().enumerate() {
                        if j as u16 >= w {
                            break;
                        }
                        if let Some(cell) = buf.cell_mut((start_x + j as u16, text_y)) {
                            cell.set_char(ch).set_style(bar_fill.value_style.clone());
                        }
                    }
                }

                // Label under the bar (centered).
                let label_text = truncate_middle(label, w as usize);
                let label_start_x =
                    x0.saturating_add((w.saturating_sub(label_text.len() as u16)) / 2);
                for (j, ch) in label_text.chars().enumerate() {
                    if j as u16 >= w {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((label_start_x + j as u16, labels_area.y)) {
                        cell.set_char(ch)
                            .set_style(Style::default().fg(Color::Gray));
                    }
                }
            }

            if let Some((mouse, tooltip)) = hover_tooltip {
                render_chart_tooltip(frame, inner, mouse, &tooltip);
            }
        }
        ChartOrientation::Horizontal => {
            // Multi-line horizontal bars, values outside to the right.
            //
            // Behavior:
            // - Prefer bar height 5, else 3, else 1, depending on how many bars fit for the current range.
            // - If even height=1 can't fit all bars, show the most recent subset that fits.
            // - Label is on the left, bar is in the middle, and the value is outside the bar with 1 space after bar.

            let count = values.len();
            if count == 0 || inner.width < 10 || inner.height < 1 {
                return;
            }

            // Decide bar height based on available space and selected range.
            let max_per_bar = inner.height / (count as u16).max(1);
            let preferred_h = if max_per_bar >= 5 {
                5
            } else if max_per_bar >= 4 {
                4
            } else if max_per_bar >= 3 {
                3
            } else {
                1
            };
            let bar_h = preferred_h.clamp(1, 5);

            let fit_bars = (inner.height / bar_h).max(1) as usize;
            let start = count.saturating_sub(fit_bars);

            let visible_labels = &labels[start..];
            let visible_values = &values[start..];
            let visible_token_columns = &token_columns[start..];

            let max_value = visible_values.iter().copied().max().unwrap_or(0).max(1);

            // Compute label column width based on label lengths (clamped).
            let mut max_label_len = 0usize;
            for l in visible_labels {
                max_label_len = max_label_len.max(l.len());
            }
            let label_w = (max_label_len as u16)
                .saturating_add(1)
                .min(inner.width.saturating_sub(12).max(4))
                .max(4);

            let value_gap: u16 = 1; // spaces between bar and value
            let min_bar_w: u16 = 10;

            let available = inner.width.saturating_sub(label_w);
            if available <= value_gap + 1 {
                return;
            }

            // Reserve the space needed by independently aligned token columns.
            let desired_value_w = if state.metric == UsageMetric::Tokens {
                horizontal_token_columns_widths(visible_token_columns, u16::MAX, formatter)
                    .map(|widths| {
                        widths
                            .iter()
                            .sum::<usize>()
                            .saturating_add(TOKEN_COLUMN_SEPARATORS_WIDTH)
                    })
                    .unwrap_or(1)
            } else {
                let mut max_len = 0usize;
                for v in visible_values {
                    let s = format_horizontal_value(*v, None, state.metric, u16::MAX, formatter);
                    max_len = max_len.max(s.len());
                }
                max_len
            };
            let desired_value_w = if state.metric == UsageMetric::Tokens {
                (desired_value_w as u16).max(1)
            } else {
                (desired_value_w as u16).clamp(6, 32)
            };

            let mut value_w = desired_value_w.min(available.saturating_sub(value_gap).max(1));
            if available > min_bar_w.saturating_add(value_gap) {
                value_w = value_w.min(
                    available
                        .saturating_sub(value_gap)
                        .saturating_sub(min_bar_w),
                );
            }
            // Keep at least 1 cell for the bar; if we're extremely cramped, shrink values.
            value_w = value_w.clamp(1, available.saturating_sub(value_gap).max(1));
            let bar_w = available
                .saturating_sub(value_gap)
                .saturating_sub(value_w)
                .max(1);
            let token_column_widths = if state.metric == UsageMetric::Tokens {
                horizontal_token_columns_widths(visible_token_columns, value_w, formatter)
            } else {
                None
            };

            let buf = frame.buffer_mut();

            for (idx, (label, value)) in
                visible_labels.iter().zip(visible_values.iter()).enumerate()
            {
                let y0 = inner.y.saturating_add((idx as u16).saturating_mul(bar_h));
                if y0 >= inner.y.saturating_add(inner.height) {
                    break;
                }
                let row_area = Rect {
                    x: inner.x,
                    y: y0,
                    width: inner.width,
                    height: bar_h.min(inner.y.saturating_add(inner.height).saturating_sub(y0)),
                };

                let label_area = Rect {
                    x: row_area.x,
                    y: row_area.y,
                    width: label_w.min(row_area.width),
                    height: row_area.height,
                };
                let bar_area = Rect {
                    x: row_area.x.saturating_add(label_area.width),
                    y: row_area.y,
                    width: bar_w.min(
                        row_area
                            .width
                            .saturating_sub(label_area.width)
                            .saturating_sub(value_gap)
                            .saturating_sub(value_w),
                    ),
                    height: row_area.height,
                };
                let value_area = Rect {
                    x: inner.x.saturating_add(inner.width).saturating_sub(value_w),
                    y: row_area.y,
                    width: value_w,
                    height: row_area.height,
                };

                // Center line for label/value.
                let mid_y = row_area.y.saturating_add(row_area.height / 2);

                // Render label on the middle line.
                let label_text = truncate_middle(label, label_area.width as usize);
                for (i, ch) in label_text.chars().enumerate() {
                    if i as u16 >= label_area.width {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((label_area.x + i as u16, mid_y)) {
                        cell.set_char(ch)
                            .set_style(Style::default().fg(Color::Gray));
                    }
                }

                // Fill bar background according to ratio.
                let ratio = (*value as f64) / (max_value as f64);
                let filled = ((bar_area.width as f64) * ratio.clamp(0.0, 1.0)).round() as u16;
                let bar_fill = alternating_bar_fill(
                    idx,
                    accent_color,
                    accent_bright_color,
                    state.bar_fill_mode,
                );
                for yy in bar_area.y..bar_area.y.saturating_add(bar_area.height) {
                    for xx in bar_area.x..bar_area.x.saturating_add(bar_area.width) {
                        if let Some(cell) = buf.cell_mut((xx, yy)) {
                            cell.set_char(' ');
                            if xx < bar_area.x.saturating_add(filled) {
                                cell.set_char(bar_fill.glyph)
                                    .set_style(bar_fill.cell_style.clone());
                            }
                        }
                    }
                }

                // Value outside the bar, on the middle line, with 1 leading space.
                let value_max_width = value_area.width;
                let value_text = if state.metric == UsageMetric::Tokens {
                    format_horizontal_token_columns(
                        visible_token_columns[idx],
                        value_max_width,
                        token_column_widths,
                        formatter,
                    )
                } else {
                    format_horizontal_value(*value, None, state.metric, value_max_width, formatter)
                };
                // Right-align numeric values within the dedicated value column.
                let value_text_width =
                    u16::try_from(UnicodeWidthStr::width(value_text.as_str())).unwrap_or(u16::MAX);
                let start_x = value_area
                    .x
                    .saturating_add(value_area.width)
                    .saturating_sub(value_text_width);
                let mut cell_offset = 0u16;
                for ch in value_text.chars() {
                    if cell_offset >= value_area.width {
                        break;
                    }
                    if let Some(cell) = buf.cell_mut((start_x + cell_offset, mid_y)) {
                        cell.set_char(ch)
                            .set_style(Style::default().fg(Color::Gray));
                    }
                    cell_offset = cell_offset.saturating_add(
                        UnicodeWidthChar::width(ch)
                            .unwrap_or(0)
                            .min(u16::MAX as usize) as u16,
                    );
                }
            }
        }
    }
    if let Some(scrollbar_area) = scrollbar_area {
        render_chart_scrollbar(
            frame,
            scrollbar_area,
            scrollbar_axis,
            ChartScrollbarViewport {
                total: all_days.len(),
                visible: end.saturating_sub(start),
                offset_from_newest: state.usage_period_offset,
            },
            ChartScrollbarOwner::Usage,
            state,
        );
    }
}

fn usage_chart_metric_label(metric: UsageMetric) -> &'static str {
    match metric {
        UsageMetric::Tokens => "TOKENS (INPUT / CACHE-WRITE / CACHE-READ / OUTPUT)",
        UsageMetric::Time => "TIME",
        UsageMetric::Runs => "RUNS",
    }
}

/// Token columns of one chart row: uncached input, cache writes, cache
/// reads, and output.
type TokenColumns = [u64; 4];

/// Width of the three " / " separators between token columns.
const TOKEN_COLUMN_SEPARATORS_WIDTH: usize = 9;

fn usage_day_token_columns(day: &UsageDay) -> TokenColumns {
    [
        day.input_tokens.max(0) as u64,
        day.cache_write_tokens.max(0) as u64,
        day.cache_read_tokens.max(0) as u64,
        day.output_tokens.max(0) as u64,
    ]
}

fn hovered_vertical_bar_index(
    mouse_position: Option<(u16, u16)>,
    bars_area: Rect,
    bar_width: u16,
    bar_gap: u16,
    values: &[u64],
    max_value: u64,
) -> Option<usize> {
    let (mouse_x, mouse_y) = mouse_position?;
    let right = bars_area.x.saturating_add(bars_area.width);
    let bottom = bars_area.y.saturating_add(bars_area.height);
    if mouse_x < bars_area.x
        || mouse_x >= right
        || mouse_y < bars_area.y
        || mouse_y >= bottom
        || bar_width == 0
    {
        return None;
    }

    let stride = bar_width.saturating_add(bar_gap);
    if stride == 0 {
        return None;
    }
    let offset_x = mouse_x.saturating_sub(bars_area.x);
    let index = (offset_x / stride) as usize;
    let value = *values.get(index)?;
    let bar_x = bars_area
        .x
        .saturating_add((index as u16).saturating_mul(stride));
    let actual_width = bar_width.min(right.saturating_sub(bar_x));
    if mouse_x >= bar_x.saturating_add(actual_width) {
        return None;
    }

    let ratio = (value as f64) / (max_value.max(1) as f64);
    let filled_height = ((bars_area.height as f64) * ratio.clamp(0.0, 1.0)).round() as u16;
    if filled_height == 0 {
        return None;
    }
    let bottom_y = bottom.saturating_sub(1);
    let top_filled_y = bottom_y.saturating_sub(filled_height.saturating_sub(1));
    (mouse_y >= top_filled_y).then_some(index)
}

fn format_vertical_bar_tooltip(
    day: &str,
    value: u64,
    metric: UsageMetric,
    formatter: DisplayFormatter<'_>,
) -> String {
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map(|date| formatter.format_full_date(date))
        .unwrap_or_else(|_| day.to_string());
    let value = match metric {
        UsageMetric::Tokens => format!("{} tokens", formatter.format_u64(value)),
        UsageMetric::Time => format_duration_words(
            value.min((i64::MAX as u64) / 60_000).saturating_mul(60_000) as i64,
        ),
        UsageMetric::Runs => format!("{} runs", formatter.format_u64(value)),
    };
    format!("{date} | {value}")
}

fn render_chart_tooltip(frame: &mut Frame<'_>, bounds: Rect, mouse: (u16, u16), text: &str) {
    render_hover_tooltip(frame, bounds, mouse, text);
}

fn render_hover_tooltip(frame: &mut Frame<'_>, bounds: Rect, mouse: (u16, u16), text: &str) {
    render_hover_tooltip_with_horizontal_padding(frame, bounds, mouse, text, 0);
}

fn render_weekly_pace_tooltip(frame: &mut Frame<'_>, bounds: Rect, mouse: (u16, u16), text: &str) {
    render_hover_tooltip_with_horizontal_padding(frame, bounds, mouse, text, 1);
}

fn render_hover_tooltip_with_horizontal_padding(
    frame: &mut Frame<'_>,
    bounds: Rect,
    mouse: (u16, u16),
    text: &str,
    horizontal_padding: u16,
) {
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        return;
    }
    let content_width = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(*line))
        .max()
        .unwrap_or(0)
        .min(u16::MAX as usize) as u16;
    let content_height = lines.len().min(u16::MAX as usize) as u16;
    let Some(area) = hover_tooltip_rect_with_horizontal_padding(
        bounds,
        mouse,
        content_width,
        content_height,
        horizontal_padding,
    ) else {
        return;
    };
    let inner_width = area
        .width
        .saturating_sub(2)
        .saturating_sub(horizontal_padding.saturating_mul(2))
        .max(1) as usize;
    let styled_lines: Vec<Line<'static>> = lines
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                truncate_middle(line, inner_width),
                Style::default().add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(Color::White))
        .padding(Padding {
            left: horizontal_padding,
            right: horizontal_padding,
            top: 0,
            bottom: 0,
        })
        .style(Style::default().bg(Color::Black));
    let alignment = if content_height <= 1 {
        Alignment::Center
    } else {
        Alignment::Left
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(styled_lines))
            .alignment(alignment)
            .block(block),
        area,
    );
}

#[cfg(test)]
fn chart_tooltip_rect(bounds: Rect, mouse: (u16, u16), content_width: u16) -> Option<Rect> {
    hover_tooltip_rect_with_horizontal_padding(bounds, mouse, content_width, 1, 0)
}

fn hover_tooltip_rect_with_horizontal_padding(
    bounds: Rect,
    mouse: (u16, u16),
    content_width: u16,
    content_height: u16,
    horizontal_padding: u16,
) -> Option<Rect> {
    let horizontal_frame = 2u16.saturating_add(horizontal_padding.saturating_mul(2));
    let min_width = horizontal_frame.saturating_add(1);
    if bounds.width < min_width || bounds.height < 3 {
        return None;
    }
    let width = content_width
        .saturating_add(horizontal_frame)
        .clamp(min_width, bounds.width);
    let height = content_height.saturating_add(2).clamp(3, bounds.height);
    let right = bounds.x.saturating_add(bounds.width);
    let bottom = bounds.y.saturating_add(bounds.height);
    let max_x = right.saturating_sub(width);
    let max_y = bottom.saturating_sub(height);

    let preferred_x = if mouse.0.saturating_add(1).saturating_add(width) <= right {
        mouse.0.saturating_add(1)
    } else {
        mouse.0.saturating_sub(width)
    };
    let preferred_y = if mouse.1 >= bounds.y.saturating_add(height) {
        mouse.1.saturating_sub(height)
    } else {
        mouse.1.saturating_add(1)
    };

    Some(Rect::new(
        preferred_x.clamp(bounds.x, max_x),
        preferred_y.clamp(bounds.y, max_y),
        width,
        height,
    ))
}

fn format_vertical_token_value(
    value: u64,
    max_width: u16,
    formatter: DisplayFormatter<'_>,
) -> String {
    let preferred = match formatter.style() {
        DisplayStyle::Classic => value.to_string(),
        DisplayStyle::SystemCompact => format_tokens_compact(value as i64, formatter),
        DisplayStyle::SystemFull => formatter.format_u64(value),
    };
    if preferred.len() <= max_width as usize {
        preferred
    } else {
        format_compact_kmb(value, max_width, formatter)
    }
}

fn format_day_label_weekday_mmdd(day: &str, formatter: DisplayFormatter<'_>) -> String {
    // Input: YYYY-MM-DD
    // Output: Mon 02/02
    let parsed = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok();
    let Some(date) = parsed else {
        return day.to_string();
    };
    formatter.format_chart_day(date)
}

fn format_horizontal_value(
    value: u64,
    out_of_cache_tokens: Option<u64>,
    metric: UsageMetric,
    max_width: u16,
    formatter: DisplayFormatter<'_>,
) -> String {
    if max_width == 0 {
        return String::new();
    }

    if metric == UsageMetric::Tokens {
        if let Some(out_of_cache) = out_of_cache_tokens {
            let compact = format!(
                "{} / {}",
                format_tokens_overview(value as i64, formatter),
                format_tokens_overview(out_of_cache as i64, formatter)
            );
            if compact.len() <= max_width as usize {
                return compact;
            }

            if formatter.style() == DisplayStyle::SystemFull {
                return compact;
            }

            let pair_width = max_width.saturating_sub(1);
            if pair_width >= 2 {
                let mut left_w = ((pair_width as usize * 2) / 3) as u16;
                left_w = left_w.clamp(1, pair_width.saturating_sub(1));
                let right_w = pair_width.saturating_sub(left_w);
                return format!(
                    "{} / {}",
                    format_compact_kmb(value, left_w, formatter),
                    format_compact_kmb(out_of_cache, right_w, formatter),
                );
            }
        }
    }

    let full = match metric {
        UsageMetric::Tokens => format_tokens_overview(value as i64, formatter),
        UsageMetric::Time => format_minutes_hhmm(value),
        UsageMetric::Runs => format_count(value as i64, formatter),
    };
    if full.len() <= max_width as usize {
        return full;
    }
    if metric == UsageMetric::Tokens && formatter.style() == DisplayStyle::SystemFull {
        return full;
    }

    // If we can't fit the full value, try compact with the same suffix.
    // (No suffix for horizontal values; the chart header indicates the unit.)

    // Final fallback: compact only.
    format_compact_kmb(value, max_width, formatter)
}

fn horizontal_token_columns_widths(
    rows: &[TokenColumns],
    max_width: u16,
    formatter: DisplayFormatter<'_>,
) -> Option<[usize; 4]> {
    let mut widths = [0usize; 4];
    for row in rows {
        for (idx, value) in row.iter().enumerate() {
            let formatted = format_tokens_overview(*value as i64, formatter);
            widths[idx] = widths[idx].max(UnicodeWidthStr::width(formatted.as_str()));
        }
    }

    let full_width = widths
        .iter()
        .sum::<usize>()
        .saturating_add(TOKEN_COLUMN_SEPARATORS_WIDTH);
    if full_width <= max_width as usize {
        return Some(widths);
    }
    if usize::from(max_width) < TOKEN_COLUMN_SEPARATORS_WIDTH + widths.len() {
        return None;
    }

    let digits_width = usize::from(max_width).saturating_sub(TOKEN_COLUMN_SEPARATORS_WIDTH);
    let mut compact_widths = [0usize; 4];
    for row in rows {
        for (idx, value) in row.iter().enumerate() {
            let compact = format_compact_kmb(*value, 5, formatter);
            compact_widths[idx] = compact_widths[idx].max(UnicodeWidthStr::width(compact.as_str()));
        }
    }
    if compact_widths.iter().sum::<usize>() <= digits_width {
        return Some(compact_widths);
    }

    let base = digits_width / 4;
    let remainder = digits_width % 4;
    Some(std::array::from_fn(|idx| {
        base + usize::from(idx < remainder)
    }))
}

fn format_horizontal_token_columns(
    row: TokenColumns,
    max_width: u16,
    columns: Option<[usize; 4]>,
    formatter: DisplayFormatter<'_>,
) -> String {
    let Some(widths) = columns else {
        return "\u{2026}".to_string();
    };
    let cells: [String; 4] = std::array::from_fn(|idx| {
        let cell = format_token_column_cell(row[idx], widths[idx], formatter);
        right_align_token_cell(&cell, widths[idx])
    });
    let value = cells.join(" / ");
    if UnicodeWidthStr::width(value.as_str()) <= max_width as usize {
        value
    } else {
        "\u{2026}".to_string()
    }
}

fn format_token_column_cell(value: u64, width: usize, formatter: DisplayFormatter<'_>) -> String {
    let full = format_tokens_overview(value as i64, formatter);
    if UnicodeWidthStr::width(full.as_str()) <= width {
        return full;
    }

    let compact = format_compact_kmb(value, width.min(u16::MAX as usize) as u16, formatter);
    let has_magnitude_suffix = compact.ends_with('K')
        || compact.ends_with('M')
        || compact.ends_with('B')
        || compact.ends_with('T');
    if value >= 1_000 && has_magnitude_suffix && UnicodeWidthStr::width(compact.as_str()) <= width {
        compact
    } else {
        "\u{2026}".to_string()
    }
}

fn right_align_token_cell(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(value));
    format!("{}{}", " ".repeat(padding), value)
}

fn format_duration_words(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;

    if hours > 0 {
        let hour_label = if hours == 1 { "hour" } else { "hours" };
        if mins > 0 {
            format!("{hours} {hour_label} {mins} min")
        } else {
            format!("{hours} {hour_label}")
        }
    } else {
        format!("{mins} min")
    }
}

fn format_minutes_hhmm(total_minutes: u64) -> String {
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 99 {
        return "99:59".to_string();
    }
    format!("{:02}:{:02}", hours, minutes)
}

fn render_top_models(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let formatter = state.formatter();
    let snapshot = state
        .usage
        .as_ref()
        .filter(|snapshot| snapshot.scan_pending_files == 0);
    let models = snapshot
        .map(|s| s.top_models_for_zone(state.usage_zone).to_vec())
        .unwrap_or_default();

    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "TOP_MODELS  ",
        Style::default().fg(Color::Gray),
    )];
    if models.is_empty() {
        if state
            .usage
            .as_ref()
            .is_some_and(|snapshot| snapshot.scan_pending_files > 0)
        {
            spans.push(Span::raw("INDEXING... PLEASE WAIT"));
        } else {
            spans.push(Span::raw("--"));
        }
    } else {
        for (idx, m) in models.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!(
                    "{} {}%",
                    truncate_middle(&crate::usage::model_display_name(&m.model), 18),
                    formatter.format_one_decimal(m.share_percent)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
    let mode_selector_width = 2u16;
    let swatch_width = 2u16;
    let selector_width = mode_selector_width.saturating_add(
        u16::try_from(AccentTheme::ALL.len())
            .unwrap_or(u16::MAX)
            .saturating_mul(swatch_width),
    );
    let selector_visible = area.width > selector_width;
    let models_area = Rect {
        width: area.width.saturating_sub(if selector_visible {
            selector_width.saturating_add(1)
        } else {
            0
        }),
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), models_area);

    if selector_visible {
        let selector_area = Rect::new(
            area.x
                .saturating_add(area.width)
                .saturating_sub(selector_width),
            area.y,
            selector_width,
            1,
        );
        let mode_area = Rect::new(selector_area.x, selector_area.y, 1, 1);
        state.ui_hit_targets.push(UiHitTarget {
            area: mode_area,
            action: UiClickAction::ToggleBarFillMode,
        });
        let mode_style = match state.bar_fill_mode {
            BarFillMode::Semigraphic => Style::default()
                .fg(state.accent_text_color())
                .add_modifier(Modifier::BOLD),
            BarFillMode::DualColorBackground => Style::default()
                .fg(Color::White)
                .bg(state.accent_text_color())
                .add_modifier(Modifier::BOLD),
        };
        let mut swatches =
            Vec::with_capacity(AccentTheme::ALL.len().saturating_mul(2).saturating_add(2));
        swatches.push(Span::styled("#", mode_style));
        swatches.push(Span::raw(" "));
        for (index, theme) in AccentTheme::ALL.iter().copied().enumerate() {
            let swatch_x = selector_area
                .x
                .saturating_add(mode_selector_width)
                .saturating_add((index as u16).saturating_mul(swatch_width));
            let swatch_area = Rect::new(swatch_x, selector_area.y, swatch_width, 1);
            let (accent_color, accent_bright_color) = theme.colors();
            state.ui_hit_targets.push(UiHitTarget {
                area: swatch_area,
                action: UiClickAction::SetAccentTheme(theme),
            });
            swatches.push(Span::styled(" ", Style::default().bg(accent_color)));
            swatches.push(Span::styled(" ", Style::default().bg(accent_bright_color)));
        }
        frame.render_widget(Paragraph::new(Line::from(swatches)), selector_area);
    }
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect, screen: ActiveScreen) {
    let w = area.width.min(60);
    let h = area.height.min(17);
    let popup = centered_rect(w, h, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            " Help ",
            Style::default().add_modifier(Modifier::BOLD),
        ));

    let text = match screen {
        ActiveScreen::Usage => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  Tab  - toggle statistic (Tokens/Time/Runs)"),
            Line::from("  g/w  - group by day/ISO week/month"),
            Line::from("  f    - toggle layout (Horz/Vert)"),
            Line::from("  z/F6 - toggle calendar zone (Local/UTC)"),
            Line::from("  Wheel/arrows/PgUp/PgDn/Home/End - scroll periods"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  c    - cycle color theme; click # to toggle chart fill"),
            Line::from("  Mouse - click tabs/controls/format/quit"),
            Line::from("  r/F5 - refresh usage + limits"),
            Line::from("  s/F2 - switch screen"),
            Line::from("  ?    - toggle help"),
            Line::from("  q    - quit (confirm)"),
        ]),
        ActiveScreen::Activity => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  Tab  - toggle statistic (Tokens/Time/Runs)"),
            Line::from("  +/=  - show more projects"),
            Line::from("  -    - show fewer projects"),
            Line::from("  Left/Right or wheel - scroll weeks"),
            Line::from("  PgUp/PgDn/Home/End - jump through weeks"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  c    - cycle color theme; click # to toggle chart fill"),
            Line::from("  Mouse - click tabs/view/projects/quit"),
            Line::from("  r/F5 - refresh usage + limits"),
            Line::from("  s/F2 - switch screen"),
            Line::from("  ?    - toggle help"),
            Line::from("  q    - quit (confirm)"),
        ]),
        ActiveScreen::ApiStat => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  v/Tab - switch LIMITS / MODELS view"),
            Line::from("  d    - cycle Models dates (all / 7 days / 30 days)"),
            Line::from("  ctrl+s - copy the Models table (terminal clipboard)"),
            Line::from("  r/F5 - refresh live limits"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  c    - cycle color theme; click # to toggle chart fill"),
            Line::from("  Mouse - click tabs, views, and date ranges"),
            Line::from("  s/F2 - switch screen"),
            Line::from("  ?    - toggle help"),
            Line::from("  q    - quit (confirm)"),
        ]),
        ActiveScreen::Cost => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  d    - cycle dates (all / 7 days / 30 days)"),
            Line::from("  r/F5 - refresh usage"),
            Line::from("  Prices: `pricing` in config.json (USD per million tokens)"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  c    - cycle color theme"),
            Line::from("  s/F2 - switch screen"),
            Line::from("  ?    - toggle help"),
            Line::from("  q    - quit (confirm)"),
        ]),
        ActiveScreen::Read => Text::from(vec![
            Line::from("Keys:"),
            Line::from("  n    - cycle display style (Classic/System Compact/Full)"),
            Line::from("  c    - cycle color theme; click # to toggle chart fill"),
            Line::from("  r/F5 - discover/refresh repositories (asks first)"),
            Line::from("  Mouse - click tabs/quit"),
            Line::from("  q    - quit (confirm)"),
        ]),
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn render_no_sessions_overlay(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let w = area.width.min(74);
    let h = area.height.min(11);
    let popup = centered_rect(w, h, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            " Warning! ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));

    let project = state
        .workspace_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "--".to_string());

    let text = Text::from(vec![
        Line::from("No Claude Code session files were found for this project."),
        Line::from(Span::styled(
            truncate_middle(&project, 64),
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from("Usage will stay empty until you run Claude Code in this repo."),
        Line::from("Continue anyway?"),
        Line::from(""),
        Line::from(Span::styled(
            "Press Enter/Y to continue. ESC/Q to quit",
            Style::default().fg(Color::Gray),
        )),
    ]);

    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn render_history_catalog_scan_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut AppState,
) {
    state.ui_hit_targets.clear();

    let desired_height =
        u16::try_from(state.history_project_roots.len().saturating_add(12)).unwrap_or(u16::MAX);
    let max_height = area.height.saturating_sub(2).max(8);
    let popup = centered_rect(area.width.min(92), desired_height.min(max_height), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .title(Span::styled(
                " Repository scan warning ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        popup,
    );

    let mut lines = vec![
        Line::from(Span::styled(
            "ClaudeMon will scan these folders for Git repositories.",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            "macOS may ask to access protected folders.",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            "Continue only if you trust these folders.",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            "Configured roots:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    for root in &state.history_project_roots {
        lines.push(Line::from(format!("- {}", root.display())));
    }
    lines.extend([
        Line::from(Span::styled(
            format!(
                "Search depth: {}; maximum folders: {}.",
                state.history_catalog_max_depth, state.history_catalog_max_directories
            ),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "Enter/Y: scan. Esc/N: cancel.",
            Style::default().fg(Color::Gray),
        )),
    ]);

    let message_area = Rect::new(
        popup.x.saturating_add(2),
        popup.y.saturating_add(1),
        popup.width.saturating_sub(4),
        popup.height.saturating_sub(4),
    );
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        message_area,
    );

    let buttons_area = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(popup.height.saturating_sub(2)),
        popup.width.saturating_sub(2),
        1,
    );
    let segments = [
        (" YES ", Some(UiClickAction::ConfirmHistoryCatalogScan)),
        ("   ", None),
        (" NO ", Some(UiClickAction::CancelHistoryCatalogScan)),
    ];
    state
        .ui_hit_targets
        .extend(centered_targets(buttons_area, &segments));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            pill("YES", false),
            Span::raw("   "),
            pill("NO", true),
        ]))
        .alignment(Alignment::Center),
        buttons_area,
    );
}

fn render_quit_confirmation(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    state.ui_hit_targets.clear();

    let popup = centered_rect(area.width.min(39), area.height.min(10), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .title(Span::styled(
                " Quit ",
                Style::default().add_modifier(Modifier::BOLD),
            )),
        popup,
    );

    let message_area = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(2),
        popup.width.saturating_sub(2),
        3.min(popup.height.saturating_sub(3)),
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from("Are you sure you want to quit?"),
            Line::from(Span::styled(
                "Left/Right selects; Enter chooses",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "Y yes; N/Esc no; Space toggles",
                Style::default().fg(Color::Gray),
            )),
        ]))
        .alignment(Alignment::Center),
        message_area,
    );

    let checkbox_area = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(6),
        popup.width.saturating_sub(2),
        1,
    );
    let checkbox_text = if state.quit_dont_ask_again {
        "[x] Don't show again"
    } else {
        "[ ] Don't show again"
    };
    state.ui_hit_targets.extend(centered_targets(
        checkbox_area,
        &[(checkbox_text, Some(UiClickAction::ToggleQuitDontAskAgain))],
    ));
    frame.render_widget(
        Paragraph::new(Line::from(checkbox_span(
            state.quit_dont_ask_again,
            Some("Don't show again"),
        )))
        .alignment(Alignment::Center),
        checkbox_area,
    );

    let buttons_area = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(popup.height.saturating_sub(2)),
        popup.width.saturating_sub(2),
        1,
    );
    let segments = [
        (" YES ", Some(UiClickAction::ConfirmQuit)),
        ("   ", None),
        (" NO ", Some(UiClickAction::CancelQuit)),
    ];
    state
        .ui_hit_targets
        .extend(centered_targets(buttons_area, &segments));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            pill("YES", state.quit_confirm_yes_selected),
            Span::raw("   "),
            pill("NO", !state.quit_confirm_yes_selected),
        ]))
        .alignment(Alignment::Center),
        buttons_area,
    );
}

fn render_quit_preference_confirmation(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    state.ui_hit_targets.clear();
    let disable_confirmation = state.quit_preference_prompt.unwrap_or(false);
    let (title, question, explanation) = if disable_confirmation {
        (
            " Exit confirmation ",
            "Disable exit confirmation?",
            "q and QUIT will exit immediately.",
        )
    } else {
        (
            " Exit confirmation ",
            "Enable exit confirmation?",
            "q and QUIT will ask before exiting.",
        )
    };

    let popup = centered_rect(area.width.min(43), area.height.min(8), area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .title(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            )),
        popup,
    );

    let message_area = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(2),
        popup.width.saturating_sub(2),
        2.min(popup.height.saturating_sub(3)),
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(question),
            Line::from(Span::styled(explanation, Style::default().fg(Color::Gray))),
        ]))
        .alignment(Alignment::Center),
        message_area,
    );

    let buttons_area = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(popup.height.saturating_sub(2)),
        popup.width.saturating_sub(2),
        1,
    );
    let segments = [
        (
            " YES ",
            Some(UiClickAction::ConfirmQuitConfirmationPreference),
        ),
        ("   ", None),
        (
            " NO ",
            Some(UiClickAction::CancelQuitConfirmationPreference),
        ),
    ];
    state
        .ui_hit_targets
        .extend(centered_targets(buttons_area, &segments));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            pill("YES", false),
            Span::raw("   "),
            pill("NO", true),
        ]))
        .alignment(Alignment::Center),
        buttons_area,
    );
}

fn checkbox_span(checked: bool, label: Option<&str>) -> Span<'static> {
    let mark = if checked { "x" } else { " " };
    let text = match label {
        Some(label) => format!("[{mark}] {label}"),
        None => format!(" [{mark}] "),
    };
    let style = if checked {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Span::styled(text, style)
}

fn pill(label: &str, active: bool) -> Span<'static> {
    let style = if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Span::styled(format!(" {label} "), style)
}

fn register_right_aligned_targets(
    state: &mut AppState,
    area: Rect,
    segments: &[(&str, Option<UiClickAction>)],
) {
    state
        .ui_hit_targets
        .extend(right_aligned_targets(area, segments));
}

fn right_aligned_targets(
    area: Rect,
    segments: &[(&str, Option<UiClickAction>)],
) -> Vec<UiHitTarget> {
    let total_width = segments.iter().fold(0_u16, |total, (text, _)| {
        total.saturating_add(UnicodeWidthStr::width(*text).min(u16::MAX as usize) as u16)
    });
    if total_width > area.width || area.height == 0 {
        return Vec::new();
    }

    let mut targets = Vec::new();
    let mut x = area.x.saturating_add(area.width - total_width);
    for (text, action) in segments {
        let width = UnicodeWidthStr::width(*text).min(u16::MAX as usize) as u16;
        if let Some(action) = action {
            if width > 0 {
                targets.push(UiHitTarget {
                    area: Rect {
                        x,
                        y: area.y,
                        width,
                        height: 1,
                    },
                    action: *action,
                });
            }
        }
        x = x.saturating_add(width);
    }
    targets
}

fn centered_targets(area: Rect, segments: &[(&str, Option<UiClickAction>)]) -> Vec<UiHitTarget> {
    let total_width = segments.iter().fold(0_u16, |total, (text, _)| {
        total.saturating_add(UnicodeWidthStr::width(*text).min(u16::MAX as usize) as u16)
    });
    if total_width > area.width || area.height == 0 {
        return Vec::new();
    }

    let mut targets = Vec::new();
    let mut x = area
        .x
        .saturating_add((area.width.saturating_sub(total_width)) / 2);
    for (text, action) in segments {
        let width = UnicodeWidthStr::width(*text).min(u16::MAX as usize) as u16;
        if let Some(action) = action {
            if width > 0 {
                targets.push(UiHitTarget {
                    area: Rect::new(x, area.y, width, 1),
                    action: *action,
                });
            }
        }
        x = x.saturating_add(width);
    }
    targets
}

fn card(
    title: &str,
    value: &str,
    caption1: Option<&str>,
    caption2: Option<&str>,
) -> Paragraph<'static> {
    card_with_captions(title, value, &[caption1, caption2])
}

fn render_today_card(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    title: &str,
    value: &str,
    caption1: Option<&str>,
    caption2: Option<&str>,
) {
    let today_hit_rate = state
        .usage
        .as_ref()
        .and_then(|snapshot| snapshot.days_for_zone(state.usage_zone).last())
        .map(|day| {
            cache_hit_rate_label(
                day.prompt_tokens(),
                day.cache_read_tokens,
                state.formatter(),
            )
        });
    frame.render_widget(
        card_with_captions(
            title,
            value,
            &[today_hit_rate.as_deref(), caption1, caption2],
        ),
        area,
    );
}

fn today_card_title(formatter: DisplayFormatter<'_>, now: NaiveDateTime) -> String {
    format!(
        "TODAY_{}",
        formatter.format_session_datetime(now).replace(' ', "_")
    )
}

fn card_with_captions(title: &str, value: &str, captions: &[Option<&str>]) -> Paragraph<'static> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Gray),
        ));

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        value.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for caption in captions.iter().flatten() {
        if !caption.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                caption.to_string(),
                Style::default().fg(Color::Gray),
            )));
        }
    }
    lines.push(Line::from(""));

    Paragraph::new(Text::from(lines))
        .block(block)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true })
}

fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let mut total = 0_u16;

    for logical_line in text.split('\n') {
        let mut line_width = 0_usize;
        let mut line_count = 0_u16;
        for word in logical_line.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            if line_width == 0 {
                if word_width > width {
                    line_count = line_count.saturating_add(word_width.div_ceil(width) as u16);
                } else {
                    line_width = word_width;
                }
                continue;
            }

            if word_width > width || line_width.saturating_add(1 + word_width) > width {
                line_count = line_count.saturating_add(1);
                if word_width > width {
                    line_count = line_count.saturating_add(word_width.div_ceil(width) as u16);
                    line_width = 0;
                } else {
                    line_width = word_width;
                }
            } else {
                line_width = line_width.saturating_add(1 + word_width);
            }
        }

        if line_width > 0 || line_count == 0 {
            line_count = line_count.saturating_add(1);
        }
        total = total.saturating_add(line_count);
    }

    total.max(1)
}

fn format_window_label(window_duration_mins: f64) -> Option<String> {
    if !window_duration_mins.is_finite() {
        return None;
    }
    let mins = window_duration_mins.max(0.0);
    if mins < 1.0 {
        return None;
    }
    if mins >= 60.0 {
        Some(format!("{}h", (mins / 60.0).round() as i64))
    } else {
        Some(format!("{}m", mins.round() as i64))
    }
}

fn percent_left_value(used_percent: Option<f64>) -> String {
    let Some(used) = used_percent else {
        return "--%".to_string();
    };
    if !used.is_finite() {
        return "--%".to_string();
    }
    let left = (100.0 - used).clamp(0.0, 100.0);
    format!("{}%", left.round() as i64)
}

fn resets_label(resets_at: Option<i64>, formatter: DisplayFormatter<'_>) -> Option<String> {
    let raw = resets_at?;
    let ms = crate::limits::normalize_epoch_millis(raw);
    let dt = Local.timestamp_millis_opt(ms).single()?;
    let today = Local::now().date_naive();
    let day = dt.date_naive();
    let time = formatter.format_time(dt.naive_local());
    if day == today {
        Some(format!("resets {time}"))
    } else {
        Some(format!(
            "resets {time}, {}",
            formatter.format_reset_date(day)
        ))
    }
}

fn reset_compact_label(resets_at: Option<i64>, formatter: DisplayFormatter<'_>) -> Option<String> {
    let raw = resets_at?;
    let ms = crate::limits::normalize_epoch_millis(raw);
    let dt = Local.timestamp_millis_opt(ms).single()?;
    let today = Local::now().date_naive();
    if dt.date_naive() == today {
        Some(formatter.format_time(dt.naive_local()))
    } else {
        Some(formatter.format_reset_date(dt.date_naive()))
    }
}

fn format_limit_compact_line(
    label_with_colon: &str,
    window: Option<&crate::limits::RateLimitWindow>,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> String {
    // Match requested alignment:
    // 5h limit: 100% (resets 20:43)
    // Weekly:   99% (resets 09:47, 10 Feb)
    const LABEL_W: usize = 10;
    let label = format!("{label_with_colon:<LABEL_W$}");
    let Some(w) = window else {
        return format!("{label}--");
    };
    let pct = percent_left_value(w.used_percent);
    if compact {
        let label = label_with_colon
            .trim_end_matches(':')
            .strip_suffix(" limit")
            .unwrap_or(label_with_colon.trim_end_matches(':'));
        let reset = reset_compact_label(w.resets_at, formatter)
            .map(|value| format!(" | {value}"))
            .unwrap_or_default();
        return format!("{label}: {pct}{reset}");
    }
    let resets = resets_label(w.resets_at, formatter)
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    format!("{label}{pct}{resets}")
}

fn format_rolling_limit_lines(
    short_window: Option<&crate::limits::RateLimitWindow>,
    weekly_window: Option<&crate::limits::RateLimitWindow>,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> (String, String) {
    let short_label = short_window
        .and_then(|w| w.window_duration_mins)
        .and_then(format_window_label)
        .map(|w| format!("{w} limit:"))
        .unwrap_or_else(|| "5h limit:".to_string());
    let weekly_label = if compact { "7d:" } else { "Weekly:" };

    // The newer response can expose the seven-day window as `primary` with no
    // short window. Keep the populated weekly limit in the card's value slot.
    if short_window.is_none() && weekly_window.is_some() {
        let weekly = format_limit_compact_line(weekly_label, weekly_window, compact, formatter);
        let short = format_limit_compact_line(&short_label, short_window, compact, formatter);
        return (weekly, short);
    }

    let short = format_limit_compact_line(&short_label, short_window, compact, formatter);
    let weekly = format_limit_compact_line(weekly_label, weekly_window, compact, formatter);
    (short, weekly)
}

fn format_limits_compact_card_lines(
    l: &crate::limits::AccountRateLimits,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let (short_window, weekly_window) = rolling_windows_for_limits(l);
    let (rolling_first, rolling_second) =
        format_rolling_limit_lines(short_window, weekly_window, compact, formatter);
    let extra = format_extra_bucket_compact_line(l, compact, formatter)
        .or_else(|| format_extra_usage_compact_line(l, formatter));
    (rolling_first, Some(rolling_second), extra, None)
}

const WEEKLY_WINDOW_MINUTES: f64 = 6.0 * 24.0 * 60.0;

fn is_weekly_rolling_window(window: &crate::limits::RateLimitWindow) -> bool {
    window
        .window_duration_mins
        .is_some_and(|minutes| minutes.is_finite() && minutes >= WEEKLY_WINDOW_MINUTES)
}

fn classify_rolling_windows<'a>(
    primary: Option<&'a crate::limits::RateLimitWindow>,
    secondary: Option<&'a crate::limits::RateLimitWindow>,
) -> (
    Option<&'a crate::limits::RateLimitWindow>,
    Option<&'a crate::limits::RateLimitWindow>,
) {
    let primary_is_weekly = primary.is_some_and(is_weekly_rolling_window);
    let secondary_is_weekly = secondary.is_some_and(is_weekly_rolling_window);

    if primary_is_weekly {
        return (secondary.filter(|_| !secondary_is_weekly), primary);
    }
    if secondary_is_weekly {
        return (primary.filter(|_| !primary_is_weekly), secondary);
    }

    // Older responses use primary/secondary position to mean short/weekly and
    // do not always include a usable duration. Preserve that shape unchanged.
    (primary, secondary)
}

fn rolling_windows_for_limits(
    l: &crate::limits::AccountRateLimits,
) -> (
    Option<&crate::limits::RateLimitWindow>,
    Option<&crate::limits::RateLimitWindow>,
) {
    if l.primary.is_some() || l.secondary.is_some() {
        return classify_rolling_windows(l.primary.as_ref(), l.secondary.as_ref());
    }
    l.buckets
        .iter()
        .find(|bucket| bucket.primary.is_some())
        .map(|bucket| classify_rolling_windows(bucket.primary.as_ref(), None))
        .unwrap_or((None, None))
}

/// How full the elapsed-time share of the weekly window is.
///
/// Pace uses API `used_percent` (not the displayed percent-left). A seven-day
/// window unlocks exactly 1/7 of its limit for every 24 hours elapsed from the
/// reset boundary, rather than jumping at local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WeeklyPaceBand {
    /// fill < 50% of today's daily allowance
    Normal,
    /// fill >= 50% of today's daily allowance
    Yellow,
    /// fill >= 70% of today's daily allowance
    Orange,
    /// fill >= 90% of today's daily allowance
    Red,
}

const PACE_THRESHOLD_EPSILON: f64 = 1e-9;

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| {
            let secs = duration.as_secs();
            if secs > i64::MAX as u64 {
                i64::MAX
            } else {
                secs as i64
            }
        })
        .unwrap_or(0)
}

fn weekly_window_bounds_secs(window: &crate::limits::RateLimitWindow) -> Option<(i64, i64, f64)> {
    let window_mins = window
        .window_duration_mins
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let resets_at = window.resets_at?;
    let reset_ms = crate::limits::normalize_epoch_millis(resets_at);
    let reset_secs = reset_ms.div_euclid(1000);
    // Prefer integer minutes when the API sends whole minutes (typical 10080).
    let window_secs = if window_mins.fract().abs() < f64::EPSILON {
        (window_mins as i64).saturating_mul(60).max(1)
    } else {
        (window_mins * 60.0).round().max(1.0) as i64
    };
    let start_secs = reset_secs.saturating_sub(window_secs);
    Some((start_secs, reset_secs, window_mins))
}

/// Elapsed fair-share percent of the weekly window at `now_unix_secs`.
/// Returns `None` when the window cannot be placed on a timeline.
fn weekly_elapsed_allowed_percent(
    window: &crate::limits::RateLimitWindow,
    now_unix_secs: i64,
) -> Option<f64> {
    let (start_secs, reset_secs, _) = weekly_window_bounds_secs(window)?;
    let window_secs = reset_secs.saturating_sub(start_secs);
    if window_secs <= 0 {
        return None;
    }
    let now_clamped = now_unix_secs.clamp(start_secs, reset_secs);
    let elapsed_secs = now_clamped.saturating_sub(start_secs);
    Some((elapsed_secs as f64 / window_secs as f64 * 100.0).clamp(0.0, 100.0))
}

#[derive(Debug, Clone, Copy)]
struct WeeklyDailyPace {
    used_percent: f64,
    day_index: u64,
    total_days: u64,
    daily_limit_percent: f64,
    used_today_percent: f64,
    safe_today_percent: f64,
}

/// Estimate the current day's usage against one daily share of the weekly limit.
///
/// Unused shares from completed 24-hour periods remain available today. For
/// example, on day 3, using one daily share leaves 200% of today's share safe.
fn weekly_daily_pace(
    window: &crate::limits::RateLimitWindow,
    now_unix_secs: i64,
) -> Option<WeeklyDailyPace> {
    let used_percent = window
        .used_percent
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 100.0))?;
    let (start_secs, reset_secs, window_mins) = weekly_window_bounds_secs(window)?;
    let total_days = (window_mins / (24.0 * 60.0)).ceil().max(1.0) as u64;
    let daily_limit_percent = 100.0 / total_days as f64;
    let now_clamped = now_unix_secs.clamp(start_secs, reset_secs);
    let elapsed_days = now_clamped
        .saturating_sub(start_secs)
        .div_euclid(24 * 60 * 60) as u64;
    let completed_days = elapsed_days.min(total_days.saturating_sub(1));
    let day_index = completed_days.saturating_add(1);
    let used_today_percent = used_percent - completed_days as f64 * daily_limit_percent;
    let safe_today_percent =
        ((daily_limit_percent - used_today_percent) / daily_limit_percent) * 100.0;

    Some(WeeklyDailyPace {
        used_percent,
        day_index,
        total_days,
        daily_limit_percent,
        used_today_percent,
        safe_today_percent,
    })
}

/// Map weekly window + clock to a display band.
///
/// `fill%` is the current day's consumed share of its 100% daily allowance.
fn weekly_pace_band(window: &crate::limits::RateLimitWindow, now_unix_secs: i64) -> WeeklyPaceBand {
    let Some(pace) = weekly_daily_pace(window, now_unix_secs) else {
        return WeeklyPaceBand::Normal;
    };
    let daily_used_percent = (100.0 - pace.safe_today_percent).clamp(0.0, 100.0);
    let fill = daily_used_percent + PACE_THRESHOLD_EPSILON;
    if fill >= 90.0 {
        WeeklyPaceBand::Red
    } else if fill >= 70.0 {
        WeeklyPaceBand::Orange
    } else if fill >= 50.0 {
        WeeklyPaceBand::Yellow
    } else {
        WeeklyPaceBand::Normal
    }
}

fn style_weekly_limit_line(plain: &str, band: WeeklyPaceBand, emphasize: bool) -> Line<'static> {
    let style = match band {
        WeeklyPaceBand::Normal => {
            let mut style = Style::default().fg(Color::White);
            if emphasize {
                style = style.add_modifier(Modifier::BOLD);
            }
            style
        }
        WeeklyPaceBand::Yellow => Style::default().fg(Color::Yellow),
        WeeklyPaceBand::Orange => Style::default().fg(Color::White).bg(WEEKLY_PACE_ORANGE_BG),
        WeeklyPaceBand::Red => Style::default().fg(Color::White).bg(Color::Red),
    };
    Line::from(Span::styled(plain.to_string(), style))
}

fn limits_line_for_slot(
    text: &str,
    weekly_plain: Option<&str>,
    band: WeeklyPaceBand,
    is_value: bool,
) -> Line<'static> {
    if weekly_plain == Some(text) {
        return style_weekly_limit_line(text, band, is_value);
    }
    if is_value {
        Line::from(Span::styled(
            text.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Color::Gray),
        ))
    }
}

#[derive(Debug, Clone)]
struct WeeklyPaceHover {
    mouse: (u16, u16),
    text: String,
}

/// Card chrome for `card_with_captions` (border 1 + pad top 1 / left 2).
const CARD_CONTENT_TOP: u16 = 2;
const CARD_CONTENT_LEFT: u16 = 3;
const CARD_CONTENT_RIGHT_PAD: u16 = 2;

fn card_content_line_rect(card_area: Rect, line_index: usize) -> Option<Rect> {
    if card_area.width < 6 || card_area.height < 4 {
        return None;
    }
    let y = card_area
        .y
        .saturating_add(CARD_CONTENT_TOP)
        .saturating_add(line_index as u16);
    let bottom = card_area
        .y
        .saturating_add(card_area.height)
        .saturating_sub(1);
    if y >= bottom {
        return None;
    }
    let x = card_area.x.saturating_add(CARD_CONTENT_LEFT);
    let width = card_area
        .width
        .saturating_sub(CARD_CONTENT_LEFT)
        .saturating_sub(CARD_CONTENT_RIGHT_PAD)
        .max(1);
    Some(Rect::new(x, y, width, 1))
}

const LIMIT_GAUGE_TRAILING_PAD: u16 = 1;

fn limit_gauge_line_rect(card_area: Rect, line_index: usize) -> Option<Rect> {
    let content = card_content_line_rect(card_area, line_index)?;
    let width = content.width.saturating_sub(LIMIT_GAUGE_TRAILING_PAD);
    (width > 0).then(|| Rect::new(content.x, content.y, width, content.height))
}

fn weekly_line_hit_rect(card_area: Rect, line_index: usize) -> Option<Rect> {
    card_content_line_rect(card_area, line_index)
}

fn gauge_divider_positions<const SEGMENTS: usize>(width: u16) -> Option<[u16; SEGMENTS]> {
    if SEGMENTS == 0 || usize::from(width) < SEGMENTS {
        return None;
    }

    let mut positions = [0_u16; SEGMENTS];
    for (segment, position) in positions.iter_mut().enumerate() {
        // Rounded cumulative fractions distribute remainder cells symmetrically.
        let numerator = ((segment + 1) as u32) * u32::from(width);
        *position = ((numerator + (SEGMENTS as u32 / 2)) / SEGMENTS as u32) as u16 - 1;
    }
    Some(positions)
}

fn gauge_filled_width(used_percent: Option<f64>, width: u16) -> u16 {
    let Some(used_percent) = used_percent.filter(|value| value.is_finite()) else {
        return 0;
    };
    ((used_percent.clamp(0.0, 100.0) * f64::from(width)) / 100.0).round() as u16
}

fn render_segmented_usage_gauge<const SEGMENTS: usize>(
    buffer: &mut Buffer,
    area: Rect,
    used_percent: Option<f64>,
    fair_percent: Option<f64>,
) {
    if area.height == 0 {
        return;
    }
    let Some(dividers) = gauge_divider_positions::<SEGMENTS>(area.width) else {
        return;
    };

    let filled_width = gauge_filled_width(used_percent, area.width);
    let fair_width = fair_percent
        .filter(|value| value.is_finite())
        .map(|value| gauge_filled_width(Some(value), area.width));
    let fair_marker =
        fair_width.map(|width| width.saturating_sub(1).min(area.width.saturating_sub(1)));
    let empty_divider_style = Style::default().fg(Color::DarkGray);
    let mut next_divider = 0_usize;

    for offset in 0..area.width {
        let is_divider = next_divider < dividers.len() && offset == dividers[next_divider];
        if is_divider {
            next_divider += 1;
        }
        let fill_color = if let Some(fair) = fair_width {
            if offset < fair {
                Color::Yellow
            } else {
                Color::Red
            }
        } else {
            Color::White
        };

        let cell = &mut buffer[(area.x.saturating_add(offset), area.y)];
        cell.reset();
        if is_divider || fair_marker == Some(offset) {
            if offset < filled_width {
                // Use a left-seven-eighths block for filled boundaries. It renders as
                // the colored portion with a narrow dark edge more consistently in
                // macOS Terminal than an inverted right-one-eighth block.
                cell.set_symbol(LIMIT_GAUGE_FILLED_DIVIDER)
                    .set_style(Style::default().fg(fill_color).bg(Color::Black));
            } else if fair_marker == Some(offset) {
                cell.set_symbol(LIMIT_GAUGE_DIVIDER);
                cell.set_style(Style::default().fg(Color::Yellow));
            } else {
                cell.set_symbol(LIMIT_GAUGE_DIVIDER);
                cell.set_style(empty_divider_style);
            }
        } else if offset < filled_width {
            cell.set_symbol(LIMIT_GAUGE_FILL)
                .set_style(Style::default().fg(fill_color));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LimitUsageGauge {
    Weekly {
        used_percent: Option<f64>,
        fair_percent: Option<f64>,
    },
    Monthly {
        used_percent: Option<f64>,
    },
}

fn limit_usage_gauge(
    limits: Option<&crate::limits::AccountRateLimits>,
    now_unix_secs: i64,
) -> LimitUsageGauge {
    let Some(limits) = limits else {
        return LimitUsageGauge::Weekly {
            used_percent: None,
            fair_percent: None,
        };
    };
    let (_, top_level_weekly_window) =
        classify_rolling_windows(limits.primary.as_ref(), limits.secondary.as_ref());
    if let Some(window) = top_level_weekly_window
        .filter(|window| window.used_percent.is_some_and(|value| value.is_finite()))
    {
        return LimitUsageGauge::Weekly {
            used_percent: window.used_percent,
            fair_percent: weekly_elapsed_allowed_percent(window, now_unix_secs),
        };
    }

    let (short_window, weekly_window) = rolling_windows_for_limits(limits);
    let usable_weekly_window =
        weekly_window.filter(|window| window.used_percent.is_some_and(|value| value.is_finite()));
    if let Some(window) = usable_weekly_window {
        return LimitUsageGauge::Weekly {
            used_percent: window.used_percent,
            fair_percent: weekly_elapsed_allowed_percent(window, now_unix_secs),
        };
    }
    let has_usable_short_window = short_window
        .is_some_and(|window| window.used_percent.is_some_and(|value| value.is_finite()));
    if !has_usable_short_window {
        if let Some(spend) = limits.spend_limit.as_ref() {
            return LimitUsageGauge::Monthly {
                used_percent: spend.used_percent.filter(|value| value.is_finite()),
            };
        }
    }
    LimitUsageGauge::Weekly {
        used_percent: None,
        fair_percent: None,
    }
}

fn render_limit_usage_gauge(buffer: &mut Buffer, area: Rect, gauge: LimitUsageGauge) {
    match gauge {
        LimitUsageGauge::Weekly {
            used_percent,
            fair_percent,
        } => {
            render_segmented_usage_gauge::<WEEKLY_GAUGE_DAYS>(
                buffer,
                area,
                used_percent,
                fair_percent,
            );
        }
        LimitUsageGauge::Monthly { used_percent } => {
            render_segmented_usage_gauge::<1>(buffer, area, used_percent, None);
        }
    }
}

fn rect_contains(area: Rect, point: (u16, u16)) -> bool {
    point.0 >= area.x
        && point.0 < area.x.saturating_add(area.width)
        && point.1 >= area.y
        && point.1 < area.y.saturating_add(area.height)
}

/// Human-friendly hover text for weekly pace, available for every pace band.
///
/// Uses API `used_percent` and elapsed fair share; not the displayed percent-left.
fn format_weekly_pace_tooltip(
    window: &crate::limits::RateLimitWindow,
    band: WeeklyPaceBand,
    now_unix_secs: i64,
) -> Option<String> {
    let pace = weekly_daily_pace(window, now_unix_secs)?;
    let (start_secs, reset_secs, _) = weekly_window_bounds_secs(window)?;
    let now_clamped = now_unix_secs.clamp(start_secs, reset_secs);
    let daily_used_percent = (pace.used_today_percent / pace.daily_limit_percent * 100.0).max(0.0);
    let used_i = pace.used_percent.round() as i64;
    let remaining_i = (100.0 - pace.used_percent).round() as i64;
    let (headline, advice) = match band {
        WeeklyPaceBand::Normal => (
            "Daily usage is within today's allowance.",
            "Unused capacity carries into today.",
        ),
        WeeklyPaceBand::Yellow => (
            "Today's allowance is more than half used.",
            "Keep usage steady until the weekly limit resets.",
        ),
        WeeklyPaceBand::Orange => (
            "Today's allowance is running low.",
            "Reduce usage until the weekly limit resets.",
        ),
        WeeklyPaceBand::Red => (
            "Today's allowance is nearly exhausted.",
            "The weekly limit may end before it resets.",
        ),
    };
    Some(format!(
        "{headline}\nDay {}/{} | Weekly used: {used_i}% | Remaining: {remaining_i}%\nDaily budget: {:.1}% | Used today: {:.1}% | Safe left: {:.1}%\nResets in: {}\n{advice}",
        pace.day_index,
        pace.total_days,
        pace.daily_limit_percent,
        daily_used_percent,
        pace.safe_today_percent,
        format_age(reset_secs.saturating_sub(now_clamped))
    ))
}

fn limits_card_paragraph(
    state: &AppState,
    compact: bool,
    content_width: u16,
) -> (Paragraph<'static>, Option<usize>, usize, Option<String>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .padding(Padding {
            left: 2,
            right: 1,
            top: 1,
            bottom: 0,
        })
        .title(Span::styled(
            " LIMITS ".to_string(),
            Style::default().fg(Color::Gray),
        ));

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut weekly_line_index: Option<usize> = None;
    let mut content_line_index = 0_usize;
    let mut tooltip: Option<String> = None;

    if !state.limits_enabled {
        let msg = state
            .limits_error
            .as_deref()
            .or(state.limits_notice.as_deref())
            .unwrap_or("Limits unavailable.");
        lines.push(Line::from(Span::styled(
            "Unavailable".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(Color::Gray),
        )));
    } else if let Some(limits) = state.limits.as_ref() {
        let formatter = state.formatter();
        let now = now_unix_secs();
        let (value, caption1, caption2, caption3) =
            format_limits_compact_card_lines(limits, compact, formatter);
        let (_, weekly_window) = rolling_windows_for_limits(limits);
        let weekly_label = if compact { "7d:" } else { "Weekly:" };
        let weekly_plain = weekly_window.map(|window| {
            format_limit_compact_line(weekly_label, Some(window), compact, formatter)
        });
        let band = weekly_window
            .map(|window| weekly_pace_band(window, now))
            .unwrap_or(WeeklyPaceBand::Normal);
        if let Some(window) = weekly_window {
            tooltip = format_weekly_pace_tooltip(window, band, now);
        }

        let mut push_line = |text: &str, is_value: bool| {
            if weekly_plain.as_deref() == Some(text) {
                weekly_line_index = Some(content_line_index);
            }
            lines.push(limits_line_for_slot(
                text,
                weekly_plain.as_deref(),
                band,
                is_value,
            ));
            content_line_index = content_line_index
                .saturating_add(usize::from(wrapped_line_count(text, content_width)));
        };

        push_line(&value, true);
        for caption in [caption1, caption2, caption3].into_iter().flatten() {
            if caption.trim().is_empty() {
                continue;
            }
            push_line(&caption, false);
        }
    } else {
        lines.push(Line::from(Span::styled(
            "Loading...".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
    }
    let gauge_line_index = content_line_index;
    lines.push(Line::from(""));

    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    (paragraph, weekly_line_index, gauge_line_index, tooltip)
}

fn render_limits_card(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    compact: bool,
) -> Option<WeeklyPaceHover> {
    let content_width = area.width.saturating_sub(5).max(1);
    let (paragraph, weekly_line_index, gauge_line_index, tooltip) =
        limits_card_paragraph(state, compact, content_width);
    frame.render_widget(paragraph, area);

    let limits = state
        .limits_enabled
        .then_some(state.limits.as_ref())
        .flatten();
    let gauge = limit_usage_gauge(limits, now_unix_secs());
    if let Some(gauge_area) = limit_gauge_line_rect(area, gauge_line_index) {
        render_limit_usage_gauge(frame.buffer_mut(), gauge_area, gauge);
    }

    let mouse = state.mouse_position?;
    let line_index = weekly_line_index?;
    let text = tooltip?;
    let hit = weekly_line_hit_rect(area, line_index)?;
    if !rect_contains(hit, mouse) {
        return None;
    }
    Some(WeeklyPaceHover { mouse, text })
}

/// The most used scoped limit (for example one model's weekly limit).
fn format_extra_bucket_compact_line(
    l: &crate::limits::AccountRateLimits,
    compact: bool,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    let (bucket, window) = l
        .buckets
        .iter()
        .filter_map(|bucket| Some((bucket, bucket.primary.as_ref()?)))
        .max_by(|(_, left), (_, right)| {
            left.used_percent
                .unwrap_or(0.0)
                .total_cmp(&right.used_percent.unwrap_or(0.0))
        })?;
    let label = compact_bucket_label(bucket);
    Some(format_limit_compact_line(
        &label,
        Some(window),
        compact,
        formatter,
    ))
}

fn compact_bucket_label(bucket: &crate::limits::RateLimitSnapshot) -> String {
    let raw = bucket.limit_name.as_deref().unwrap_or("Other").trim();
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "Other:".to_string();
    }
    format!("{}:", truncate_middle(cleaned, 9))
}

/// Extra (paid) usage beyond the subscription limits, when enabled.
fn format_extra_usage_compact_line(
    l: &crate::limits::AccountRateLimits,
    formatter: DisplayFormatter<'_>,
) -> Option<String> {
    const LABEL_W: usize = 10;
    let extra = l.extra_usage.as_ref().filter(|extra| extra.enabled)?;
    let used = extra.used_amount?;
    let currency = extra.currency.as_deref().unwrap_or("");
    Some(
        format!(
            "{:<LABEL_W$}{} {currency}",
            "Extra:",
            formatter.format_two_decimals(used)
        )
        .trim_end()
        .to_string(),
    )
}

fn truncate_middle(value: &str, max: usize) -> String {
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    if max <= 3 {
        return "...".to_string();
    }
    let keep = (max - 3) / 2;
    let mut start = String::with_capacity(keep);
    let mut end = String::with_capacity(keep);

    for ch in cleaned.chars().take(keep) {
        start.push(ch);
    }
    for ch in cleaned
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        end.push(ch);
    }
    format!("{start}...{end}")
}

fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let x = r.x + (r.width.saturating_sub(width)) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn apply_margin(area: Rect, margin: Margin) -> Rect {
    let x = area.x.saturating_add(margin.horizontal);
    let y = area.y.saturating_add(margin.vertical);
    let width = area
        .width
        .saturating_sub(margin.horizontal.saturating_mul(2));
    let height = area
        .height
        .saturating_sub(margin.vertical.saturating_mul(2));
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn preferred_vertical_bar_width(area_width: u16) -> u16 {
    match area_width {
        0..=47 => 1,
        48..=119 => 2,
        _ => 3,
    }
}

fn vertical_bar_capacity(area_width: u16) -> usize {
    let bar_width = preferred_vertical_bar_width(area_width);
    let slot_width = bar_width.saturating_add(1);
    usize::from(
        area_width
            .saturating_add(1)
            .checked_div(slot_width)
            .unwrap_or(1)
            .max(1),
    )
}

fn compute_bar_layout(area: Rect, bars: u16) -> (u16, u16) {
    if bars == 0 {
        return (1, 1);
    }

    // Prefer a gap of 1, but fall back to 0 if bars would be too cramped.
    let mut gap = 1u16;
    let mut width = area
        .width
        .saturating_sub(gap.saturating_mul(bars.saturating_sub(1)))
        / bars;
    if width == 0 {
        gap = 0;
        width = area.width / bars;
    }
    if width == 0 {
        width = 1;
    }
    (width, gap)
}

fn inset_with_border_and_padding(area: Rect, padding: Padding) -> Rect {
    // Account for a 1-cell border on all sides.
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    Rect {
        x: inner.x.saturating_add(padding.left),
        y: inner.y.saturating_add(padding.top),
        width: inner
            .width
            .saturating_sub(padding.left.saturating_add(padding.right)),
        height: inner
            .height
            .saturating_sub(padding.top.saturating_add(padding.bottom)),
    }
}

// counts-line helper removed (values are rendered inside bars now)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_bounds_move_from_newest_to_older_periods() {
        assert_eq!(viewport_bounds(30, 10, 0), (20, 30));
        assert_eq!(viewport_bounds(30, 10, 1), (19, 29));
        assert_eq!(viewport_bounds(30, 10, 99), (0, 10));
        assert_eq!(viewport_bounds(5, 10, 3), (0, 5));
        assert_eq!(viewport_bounds(0, 10, 0), (0, 0));
    }

    #[test]
    fn right_aligned_control_targets_follow_rendered_segments() {
        let targets = right_aligned_targets(
            Rect::new(10, 3, 30, 1),
            &[
                ("VIEW ", None),
                (
                    " TOKENS ",
                    Some(UiClickAction::SetMetric(UsageMetric::Tokens)),
                ),
                (" TIME ", Some(UiClickAction::SetMetric(UsageMetric::Time))),
            ],
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].area, Rect::new(26, 3, 8, 1));
        assert_eq!(targets[1].area, Rect::new(34, 3, 6, 1));
    }

    #[test]
    fn right_aligned_control_targets_are_disabled_when_clipped() {
        let targets = right_aligned_targets(
            Rect::new(0, 0, 4, 1),
            &[(
                " TOKENS ",
                Some(UiClickAction::SetMetric(UsageMetric::Tokens)),
            )],
        );
        assert!(targets.is_empty());
    }

    #[test]
    fn navigation_tabs_are_clickable_on_the_outer_border() {
        let (title, targets) = navigation_title(Rect::new(4, 2, 80, 20), ActiveScreen::Activity);

        assert_eq!(title.width(), 42);
        assert_eq!(targets.len(), 5);
        assert_eq!(targets[0].area, Rect::new(42, 2, 7, 1));
        assert_eq!(
            targets[0].action,
            UiClickAction::SetScreen(ActiveScreen::Usage)
        );
        assert_eq!(targets[1].area, Rect::new(49, 2, 9, 1));
        assert_eq!(
            targets[1].action,
            UiClickAction::SetScreen(ActiveScreen::ApiStat)
        );
        assert_eq!(targets[2].area, Rect::new(58, 2, 6, 1));
        assert_eq!(
            targets[2].action,
            UiClickAction::SetScreen(ActiveScreen::Cost)
        );
        assert_eq!(targets[3].area, Rect::new(64, 2, 10, 1));
        assert_eq!(
            targets[3].action,
            UiClickAction::SetScreen(ActiveScreen::Activity)
        );
        assert_eq!(targets[4].area, Rect::new(74, 2, 9, 1));
        assert_eq!(
            targets[4].action,
            UiClickAction::SetScreen(ActiveScreen::Read)
        );
    }

    #[test]
    fn api_stats_outer_title_matches_usage() {
        assert_eq!(
            app_title(),
            format!(" claudemon :: {} ", env!("CARGO_PKG_VERSION"))
        );
        let (navigation, targets) =
            navigation_title(Rect::new(0, 0, 80, 24), ActiveScreen::ApiStat);
        assert_eq!(navigation.width(), 42);
        assert_eq!(targets.len(), 5);
    }

    #[test]
    fn today_card_title_includes_the_current_date_and_time() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(DisplayStyle::Classic, &system_locale);
        let now = NaiveDate::from_ymd_opt(2026, 7, 22)
            .unwrap()
            .and_hms_opt(14, 35, 0)
            .unwrap();

        assert_eq!(today_card_title(formatter, now), "TODAY_2026-07-22_14:35");

        for style in [DisplayStyle::SystemCompact, DisplayStyle::SystemFull] {
            let formatter = DisplayFormatter::new(style, &system_locale);
            assert_eq!(today_card_title(formatter, now), "TODAY_07/22/2026_14:35");
        }
    }

    #[test]
    fn usage_week_grouping_uses_iso_week_number_and_range() {
        let days = vec![
            UsageDay {
                day: "2026-04-05".to_string(),
                input_tokens: 10,
                cache_read_tokens: 2,
                total_tokens: 12,
                agent_time_ms: 100,
                agent_runs: 1,
                ..UsageDay::default()
            },
            UsageDay {
                day: "2026-04-06".to_string(),
                input_tokens: 20,
                cache_read_tokens: 3,
                total_tokens: 24,
                agent_time_ms: 200,
                agent_runs: 2,
                ..UsageDay::default()
            },
        ];
        let grouped = aggregate_usage_days(&days, ChartRange::Week);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].day, "2026-03-30");
        assert_eq!(grouped[1].day, "2026-04-06");

        let locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(DisplayStyle::Classic, &locale);
        let label = format_usage_period_label(
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap(),
            ChartRange::Week,
            formatter,
            false,
        );
        assert_eq!(label, "W14 (Mar 30-Apr 5)");
    }

    #[test]
    fn chart_scrollbar_reserves_only_the_requested_edge() {
        let area = Rect::new(10, 20, 40, 12);
        assert_eq!(
            reserve_chart_scrollbar(area, ChartScrollbarAxis::Vertical, true),
            (Rect::new(10, 20, 38, 12), Some(Rect::new(49, 20, 1, 12)))
        );
        assert_eq!(
            reserve_chart_scrollbar(area, ChartScrollbarAxis::Horizontal, true),
            (Rect::new(10, 20, 40, 10), Some(Rect::new(10, 31, 40, 1)))
        );
        assert_eq!(
            reserve_chart_scrollbar(area, ChartScrollbarAxis::Vertical, false),
            (area, None)
        );
    }

    #[test]
    fn scrollbar_thumbs_use_inverted_spaces() {
        let (horizontal_char, horizontal_style) = chart_scrollbar_cell(true);
        assert_eq!(horizontal_char, ' ');
        assert_eq!(horizontal_style.bg, Some(Color::White));

        let (vertical_char, vertical_style) = chart_scrollbar_cell(true);
        assert_eq!(vertical_char, ' ');
        assert_eq!(vertical_style.bg, Some(Color::White));

        let (track_char, track_style) = chart_scrollbar_cell(false);
        assert_eq!(track_char, '.');
        assert_eq!(track_style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn vertical_bar_capacity_prefers_wider_columns() {
        assert_eq!(preferred_vertical_bar_width(40), 1);
        assert_eq!(preferred_vertical_bar_width(80), 2);
        assert_eq!(preferred_vertical_bar_width(160), 3);
        assert_eq!(vertical_bar_capacity(40), 20);
        assert_eq!(vertical_bar_capacity(80), 27);
        assert_eq!(vertical_bar_capacity(160), 40);
        assert_eq!(compute_bar_layout(Rect::new(0, 0, 160, 10), 40), (3, 1));
    }

    #[test]
    fn navigation_title_hides_tabs_before_they_touch_the_version() {
        let (title, targets) = navigation_title(Rect::new(0, 0, 52, 10), ActiveScreen::Usage);
        assert!(title.spans.is_empty());
        assert!(targets.is_empty());
    }

    #[test]
    fn quit_action_is_on_the_bottom_right_border() {
        let (title, targets) = quit_title(Rect::new(4, 2, 71, 20), true, true);

        assert_eq!(title.spans[0].content.as_ref(), " [x] ");
        assert_eq!(title.spans[1].style.fg, Some(Color::Black));
        assert_eq!(title.spans[1].style.bg, Some(Color::White));
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].area, Rect::new(62, 21, 5, 1));
        assert_eq!(
            targets[0].action,
            UiClickAction::PromptQuitConfirmationPreference
        );
        assert_eq!(targets[1].area, Rect::new(67, 21, 6, 1));
        assert_eq!(targets[1].action, UiClickAction::PromptQuit);
    }

    #[test]
    fn quit_confirmation_buttons_are_centered_and_separate() {
        let targets = centered_targets(
            Rect::new(10, 5, 33, 1),
            &[
                (" YES ", Some(UiClickAction::ConfirmQuit)),
                ("   ", None),
                (" NO ", Some(UiClickAction::CancelQuit)),
            ],
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].area, Rect::new(20, 5, 5, 1));
        assert_eq!(targets[1].area, Rect::new(28, 5, 4, 1));
    }

    #[test]
    fn usage_header_has_one_blank_row_above_and_below() {
        let chunks = usage_screen_layout(Rect::new(2, 1, 100, 20), 1);
        let header_line = header_line_area(chunks[0]);

        assert_eq!(chunks[0], Rect::new(2, 1, 100, 3));
        assert_eq!(header_line, Rect::new(2, 2, 100, 1));
        assert_eq!(header_line.y + header_line.height + 1, chunks[1].y);
    }

    #[test]
    fn control_group_labels_use_the_selected_accent_color() {
        let label = control_group_label("VIEW", Color::Magenta);
        assert_eq!(label.content.as_ref(), " VIEW ");
        assert_eq!(label.style.bg, Some(Color::Magenta));
        assert_eq!(label.style.fg, Some(Color::Black));
    }

    #[test]
    fn alternating_bars_use_the_selected_fill_mode() {
        let textured =
            alternating_bar_fill(0, Color::Red, Color::LightRed, BarFillMode::Semigraphic);
        assert_eq!(textured.glyph, TEXTURED_BAR_GLYPH);
        assert_eq!(textured.cell_style.fg, Some(Color::LightRed));
        assert_eq!(textured.cell_style.bg, None);
        assert_eq!(textured.value_style.fg, Some(Color::White));

        let solid = alternating_bar_fill(1, Color::Red, Color::LightRed, BarFillMode::Semigraphic);
        assert_eq!(solid.glyph, SOLID_BAR_GLYPH);
        assert_eq!(solid.cell_style.fg, Some(Color::LightRed));
        assert_eq!(solid.cell_style.bg, None);
        assert_eq!(solid.value_style.fg, Some(Color::White));
        assert_eq!(solid.value_style.bg, None);

        let background = alternating_bar_fill(
            0,
            Color::Red,
            Color::LightRed,
            BarFillMode::DualColorBackground,
        );
        assert_eq!(background.glyph, ' ');
        assert_eq!(background.cell_style.fg, None);
        assert_eq!(background.cell_style.bg, Some(Color::Red));
        assert_eq!(background.value_style.fg, Some(Color::Black));
        assert_eq!(background.value_style.bg, Some(Color::Red));
    }

    #[test]
    fn activity_project_controls_are_increment_count_decrement() {
        let targets = right_aligned_targets(
            Rect::new(0, 0, 17, 1),
            &[
                (" PROJECTS ", None),
                (" + ", Some(UiClickAction::IncreaseProjects)),
                ("10", None),
                (" -", Some(UiClickAction::DecreaseProjects)),
            ],
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].area, Rect::new(10, 0, 3, 1));
        assert_eq!(targets[0].action, UiClickAction::IncreaseProjects);
        assert_eq!(targets[1].area, Rect::new(15, 0, 2, 1));
        assert_eq!(targets[1].action, UiClickAction::DecreaseProjects);

        for value in ["7", "42", "1234"] {
            let spans = activity_project_control_spans(value.to_string());
            let rendered = spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(rendered, format!(" + {value} -"));
        }
    }

    #[test]
    fn token_chart_header_explains_the_slash_pair() {
        assert_eq!(
            usage_chart_metric_label(UsageMetric::Tokens),
            "TOKENS (INPUT / CACHE-WRITE / CACHE-READ / OUTPUT)"
        );
    }

    #[test]
    fn truncate_middle_is_unicode_safe_and_strips_control_chars() {
        let input = "ab\x1b[31mcd\x1b[0m-zu\u{0308}rich";
        let out = truncate_middle(input, 10);
        assert!(!out.chars().any(|c| c.is_control()));
        assert!(out.chars().count() <= 10);
    }

    #[test]
    fn truncate_middle_handles_small_limits() {
        assert_eq!(truncate_middle("hello", 0), "...");
        assert_eq!(truncate_middle("hello", 1), "...");
        assert_eq!(truncate_middle("hello", 2), "...");
        assert_eq!(truncate_middle("hello", 3), "...");
    }

    #[test]
    fn card_row_uses_the_tallest_measured_card() {
        let short = CardSpec::new("42".to_string(), vec!["One line".to_string()]);
        let tall = CardSpec::new(
            "Weekly limit".to_string(),
            vec!["Resets: 3 available | earliest expires 21 Jul".to_string()],
        );
        let width = 20;
        let expected = tall.required_height(width);

        assert_eq!(card_row_height(&[short, tall], width), expected);
    }

    #[test]
    fn limits_card_height_reserves_the_gauge_row() {
        let regular = CardSpec::new("value".to_string(), vec!["caption".to_string()]);
        let limits = CardSpec::limits("value".to_string(), vec!["caption".to_string()]);
        let regular_height = regular.required_height(40);
        let limits_height = limits.required_height(40);

        assert_eq!(
            limits_height,
            regular_height.saturating_add(LIMITS_CARD_RESERVED_ROWS)
        );
        assert_eq!(card_row_height(&[limits, regular], 40), limits_height);
    }

    #[test]
    fn short_usage_layout_preserves_measured_card_height() {
        let area = Rect::new(0, 0, 140, 18);
        let chunks = usage_layout(area, 2, 12);

        assert_eq!(chunks[0].height, 2);
        assert_eq!(chunks[1].height, 12);
        assert_eq!(chunks[2].height, 1);
        assert_eq!(chunks[3].height, 3);
    }

    #[test]
    fn card_format_density_uses_the_actual_card_width() {
        let six_columns = usage_card_layout(180);
        assert_eq!(six_columns.columns, 6);
        assert_eq!(six_columns.min_card_width, 28);
        assert!(uses_compact_limit_lines(six_columns.min_card_width));

        let three_columns = usage_card_layout(179);
        assert_eq!(three_columns.columns, 3);
        assert_eq!(three_columns.min_card_width, 59);
        assert!(!uses_compact_limit_lines(three_columns.min_card_width));
    }

    #[test]
    fn wrapped_line_count_matches_word_wrapping_and_long_words() {
        assert_eq!(wrapped_line_count("one two three", 7), 2);
        assert_eq!(wrapped_line_count("abcdefghijk", 10), 2);
        assert!(wrapped_line_count(footer_hint(ActiveScreen::Usage), 48) > 1);
    }

    #[test]
    fn limits_card_treats_weekly_primary_as_weekly() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let limits = crate::limits::AccountRateLimits {
            primary: Some(crate::limits::RateLimitWindow {
                used_percent: Some(4.0),
                window_duration_mins: Some(10080.0),
                resets_at: None,
            }),
            secondary: None,
            ..crate::limits::AccountRateLimits::empty(crate::limits::LimitsSourceKind::OAuth, 0)
        };

        let (short_window, weekly_window) = rolling_windows_for_limits(&limits);
        assert!(short_window.is_none());
        assert_eq!(
            weekly_window.and_then(|window| window.window_duration_mins),
            Some(10080.0)
        );

        let (value, caption1, _, _) = format_limits_compact_card_lines(&limits, false, formatter);
        assert_eq!(value, "Weekly:   96%");
        assert_eq!(caption1.as_deref(), Some("5h limit: --"));

        let (compact_value, compact_caption1, _, _) =
            format_limits_compact_card_lines(&limits, true, formatter);
        assert_eq!(compact_value, "7d: 96%");
        assert_eq!(compact_caption1.as_deref(), Some("5h limit: --"));
    }

    fn local_unix_secs(date: NaiveDate, hour: u32, minute: u32) -> i64 {
        let naive = date
            .and_hms_opt(hour, minute, 0)
            .expect("valid local wall time");
        Local
            .from_local_datetime(&naive)
            .single()
            .or_else(|| Local.from_local_datetime(&naive).earliest())
            .expect("local datetime")
            .timestamp()
    }

    /// Build a 7-day weekly window starting at local midnight of `window_start_date`.
    fn weekly_window(
        used_percent: f64,
        window_start_date: NaiveDate,
        now_date: NaiveDate,
        now_hour: u32,
        now_minute: u32,
    ) -> (crate::limits::RateLimitWindow, i64) {
        weekly_window_from_start(
            used_percent,
            window_start_date,
            0,
            0,
            now_date,
            now_hour,
            now_minute,
        )
    }

    fn weekly_window_from_start(
        used_percent: f64,
        window_start_date: NaiveDate,
        window_start_hour: u32,
        window_start_minute: u32,
        now_date: NaiveDate,
        now_hour: u32,
        now_minute: u32,
    ) -> (crate::limits::RateLimitWindow, i64) {
        const WINDOW_DAYS: i64 = 7;
        let start_secs = local_unix_secs(window_start_date, window_start_hour, window_start_minute);
        let reset_secs = start_secs.saturating_add(WINDOW_DAYS.saturating_mul(24 * 3600));
        let now_secs = local_unix_secs(now_date, now_hour, now_minute);
        let window = crate::limits::RateLimitWindow {
            used_percent: Some(used_percent),
            window_duration_mins: Some((WINDOW_DAYS * 24 * 60) as f64),
            resets_at: Some(reset_secs),
        };
        (window, now_secs)
    }

    #[test]
    fn weekly_pace_band_thresholds_use_elapsed_share() {
        // During the first day, the daily budget is 100/7 ~ 14.286%.
        // used 6 -> 42% of today's budget -> Normal
        // used 8 -> 56% -> Yellow
        // used 10 -> 70% -> Orange
        // used 13 -> 91% -> Red
        let start = NaiveDate::from_ymd_opt(2026, 7, 20).expect("date");
        let (w6, now) = weekly_window(6.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&w6, now), WeeklyPaceBand::Normal);
        let (w8, now) = weekly_window(8.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&w8, now), WeeklyPaceBand::Yellow);
        let (w10, now) = weekly_window(10.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&w10, now), WeeklyPaceBand::Orange);
        let (w13, now) = weekly_window(13.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&w13, now), WeeklyPaceBand::Red);
    }

    #[test]
    fn weekly_daily_pace_carries_unused_completed_days_into_today() {
        let start = NaiveDate::from_ymd_opt(2026, 7, 20).expect("date");
        let day3 = NaiveDate::from_ymd_opt(2026, 7, 22).expect("date");
        let daily = 100.0 / 7.0;

        let (on_pace, now) = weekly_window(daily * 2.0, start, day3, 0, 0);
        let on_pace_details = weekly_daily_pace(&on_pace, now).expect("daily pace");
        assert!((on_pace_details.safe_today_percent - 100.0).abs() < 0.01);
        assert_eq!(weekly_pace_band(&on_pace, now), WeeklyPaceBand::Normal);

        let (half_used, now) = weekly_window(daily * 2.5, start, day3, 0, 0);
        let half_used_details = weekly_daily_pace(&half_used, now).expect("daily pace");
        assert!((half_used_details.safe_today_percent - 50.0).abs() < 0.01);
        assert_eq!(weekly_pace_band(&half_used, now), WeeklyPaceBand::Yellow);

        let (carryover, now) = weekly_window(daily, start, day3, 0, 0);
        let carryover_details = weekly_daily_pace(&carryover, now).expect("daily pace");
        assert!((carryover_details.safe_today_percent - 200.0).abs() < 0.01);
        assert_eq!(weekly_pace_band(&carryover, now), WeeklyPaceBand::Normal);
    }

    #[test]
    fn weekly_pace_band_uses_24_hour_days_from_reset_boundary() {
        let start = NaiveDate::from_ymd_opt(2026, 7, 20).expect("date");
        let day2 = NaiveDate::from_ymd_opt(2026, 7, 21).expect("date");
        let used = 8.0;

        // A local midnight does not start a new daily allowance when reset was 19:54.
        let (w, now) = weekly_window_from_start(used, start, 19, 54, day2, 0, 23);
        assert_eq!(weekly_pace_band(&w, now), WeeklyPaceBand::Yellow);

        // The allowance changes only after the reset-anchored 24 hours complete.
        let (w, now) = weekly_window_from_start(used, start, 19, 54, day2, 19, 54);
        assert_eq!(weekly_pace_band(&w, now), WeeklyPaceBand::Normal);
    }

    #[test]
    fn weekly_pace_band_uses_exact_reset_countdown() {
        let reset = local_unix_secs(NaiveDate::from_ymd_opt(2026, 9, 23).expect("date"), 19, 54);
        let now = local_unix_secs(NaiveDate::from_ymd_opt(2026, 9, 21).expect("date"), 0, 32);
        let window = crate::limits::RateLimitWindow {
            used_percent: Some(66.0),
            window_duration_mins: Some(10_080.0),
            resets_at: Some(reset),
        };

        assert_eq!(weekly_pace_band(&window, now), WeeklyPaceBand::Yellow);
        let daily = weekly_daily_pace(&window, now).expect("daily pace");
        assert_eq!(daily.day_index, 5);
        assert!((daily.safe_today_percent - 38.0).abs() < 0.1);
        let fair = weekly_elapsed_allowed_percent(&window, now).expect("fair share");
        assert!((fair - 59.9).abs() < 0.1);
    }

    #[test]
    fn weekly_pace_band_is_normal_without_resets_at_or_bad_used() {
        let now = local_unix_secs(NaiveDate::from_ymd_opt(2026, 7, 20).expect("date"), 12, 0);
        let missing_reset = crate::limits::RateLimitWindow {
            used_percent: Some(50.0),
            window_duration_mins: Some(10080.0),
            resets_at: None,
        };
        assert_eq!(
            weekly_pace_band(&missing_reset, now),
            WeeklyPaceBand::Normal
        );

        let bad_used = crate::limits::RateLimitWindow {
            used_percent: Some(f64::NAN),
            window_duration_mins: Some(10080.0),
            resets_at: Some(now + 3_600),
        };
        assert_eq!(weekly_pace_band(&bad_used, now), WeeklyPaceBand::Normal);
    }

    #[test]
    fn weekly_pace_tooltip_is_available_for_every_band() {
        let start = NaiveDate::from_ymd_opt(2026, 7, 20).expect("date");

        let (normal, now) = weekly_window(6.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&normal, now), WeeklyPaceBand::Normal);
        let normal_text =
            format_weekly_pace_tooltip(&normal, WeeklyPaceBand::Normal, now).expect("normal");
        assert_eq!(normal_text.lines().count(), 5);
        assert!(normal_text.contains("within today's allowance"));
        assert!(normal_text.contains("Daily budget"));
        assert!(normal_text.contains("Resets in"));

        let (yellow, now) = weekly_window(8.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&yellow, now), WeeklyPaceBand::Yellow);
        let yellow_text =
            format_weekly_pace_tooltip(&yellow, WeeklyPaceBand::Yellow, now).expect("yellow");
        assert_eq!(yellow_text.lines().count(), 5);
        assert!(yellow_text.contains("more than half used"));

        let (orange, now) = weekly_window(10.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&orange, now), WeeklyPaceBand::Orange);
        let orange_text =
            format_weekly_pace_tooltip(&orange, WeeklyPaceBand::Orange, now).expect("orange");
        assert_eq!(orange_text.lines().count(), 5);
        assert!(orange_text.contains("allowance is running low"));
        assert!(orange_text.contains("Reduce usage"));

        let (red, now) = weekly_window(17.0, start, start, 1, 0);
        assert_eq!(weekly_pace_band(&red, now), WeeklyPaceBand::Red);
        let red_text = format_weekly_pace_tooltip(&red, WeeklyPaceBand::Red, now).expect("red");
        assert_eq!(red_text.lines().count(), 5);
        assert!(red_text.contains("allowance is nearly exhausted"));
        assert!(red_text.contains("Safe left"));
    }

    #[test]
    fn weekly_line_hit_rect_matches_card_chrome() {
        let card = Rect::new(10, 20, 40, 10);
        let hit = weekly_line_hit_rect(card, 1).expect("hit");
        assert_eq!(hit.x, 13); // + left border + pad
        assert_eq!(hit.y, 23); // + top border + pad + line 1
        assert_eq!(hit.height, 1);
        assert!(hit.width > 0);
        assert!(rect_contains(hit, (13, 23)));
        assert!(!rect_contains(hit, (13, 22)));
    }

    #[test]
    fn limit_gauge_reserves_two_cells_before_card_edge() {
        let card = Rect::new(10, 20, 40, 10);
        let gauge = limit_gauge_line_rect(card, 0).expect("gauge");
        assert_eq!(gauge.x, 13);
        assert_eq!(gauge.y, 22);
        assert_eq!(gauge.width, 34);
        let right_border_x = card.x + card.width - 1;
        assert_eq!(right_border_x - (gauge.x + gauge.width), 2);
    }

    #[test]
    fn today_cache_hit_rate_label_uses_daily_input_and_cached_tokens() {
        let locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(DisplayStyle::Classic, &locale);
        assert_eq!(
            cache_hit_rate_label(1_000, 375, formatter),
            "Hit Rate 37.5%"
        );
        assert_eq!(cache_hit_rate_label(0, 0, formatter), "Hit Rate 0.0%");
    }

    #[test]
    fn weekly_gauge_dividers_evenly_fill_the_content_width() {
        assert_eq!(
            gauge_divider_positions::<WEEKLY_GAUGE_DAYS>(28),
            Some([3, 7, 11, 15, 19, 23, 27])
        );
        assert_eq!(
            gauge_divider_positions::<WEEKLY_GAUGE_DAYS>(23),
            Some([2, 6, 9, 12, 15, 19, 22])
        );

        for width in 7..=80 {
            let positions =
                gauge_divider_positions::<WEEKLY_GAUGE_DAYS>(width).expect("visible gauge");
            let mut segment_widths = [0_u16; WEEKLY_GAUGE_DAYS];
            let mut start = 0_u16;
            for (index, end) in positions.into_iter().enumerate() {
                segment_widths[index] = end.saturating_add(1).saturating_sub(start);
                start = end.saturating_add(1);
            }
            assert_eq!(positions[WEEKLY_GAUGE_DAYS - 1], width - 1);
            assert_eq!(segment_widths, {
                let mut reversed = segment_widths;
                reversed.reverse();
                reversed
            });
            let shortest = segment_widths.iter().copied().min().expect("segment");
            let longest = segment_widths.iter().copied().max().expect("segment");
            assert!(longest - shortest <= 1);
        }
    }

    #[test]
    fn weekly_gauge_colors_fair_progress_yellow_and_overage_red() {
        let area = Rect::new(0, 0, 28, 1);
        let mut buffer = Buffer::empty(area);
        render_segmented_usage_gauge::<WEEKLY_GAUGE_DAYS>(
            &mut buffer,
            area,
            Some(35.7),
            Some(14.3),
        );

        assert_eq!(buffer[(0, 0)].symbol(), LIMIT_GAUGE_FILL);
        assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Yellow));
        assert_eq!(buffer[(3, 0)].symbol(), LIMIT_GAUGE_FILLED_DIVIDER);
        assert_eq!(buffer[(3, 0)].style().fg, Some(Color::Yellow));
        assert_eq!(buffer[(3, 0)].style().bg, Some(Color::Black));
        assert!(!buffer[(3, 0)]
            .style()
            .add_modifier
            .contains(Modifier::REVERSED));
        assert_eq!(buffer[(4, 0)].symbol(), LIMIT_GAUGE_FILL);
        assert_eq!(buffer[(4, 0)].style().fg, Some(Color::Red));
        assert_eq!(buffer[(7, 0)].symbol(), LIMIT_GAUGE_FILLED_DIVIDER);
        assert_eq!(buffer[(7, 0)].style().fg, Some(Color::Red));
        assert_eq!(buffer[(7, 0)].style().bg, Some(Color::Black));
        assert!(!buffer[(7, 0)]
            .style()
            .add_modifier
            .contains(Modifier::REVERSED));
        assert_eq!(buffer[(10, 0)].symbol(), " ");
        assert_eq!(buffer[(11, 0)].symbol(), LIMIT_GAUGE_DIVIDER);
        assert_eq!(buffer[(11, 0)].style().fg, Some(Color::DarkGray));
    }

    #[test]
    fn weekly_gauge_shows_the_fair_limit_before_usage_reaches_it() {
        let area = Rect::new(0, 0, 28, 1);
        let mut buffer = Buffer::empty(area);

        render_segmented_usage_gauge::<WEEKLY_GAUGE_DAYS>(&mut buffer, area, Some(7.0), Some(14.3));

        assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Yellow));
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(3, 0)].symbol(), LIMIT_GAUGE_DIVIDER);
        assert_eq!(buffer[(3, 0)].style().fg, Some(Color::Yellow));
        assert_eq!(buffer[(3, 0)].style().bg, Some(Color::Reset));
    }

    #[test]
    fn weekly_gauge_clamps_usage_and_handles_missing_values() {
        assert_eq!(gauge_filled_width(None, 28), 0);
        assert_eq!(gauge_filled_width(Some(f64::NAN), 28), 0);
        assert_eq!(gauge_filled_width(Some(-1.0), 28), 0);
        assert_eq!(gauge_filled_width(Some(50.0), 28), 14);
        assert_eq!(gauge_filled_width(Some(101.0), 28), 28);
    }

    #[test]
    fn weekly_limit_gauge_uses_the_current_fair_share() {
        let start = NaiveDate::from_ymd_opt(2026, 7, 20).expect("date");
        let next_day = NaiveDate::from_ymd_opt(2026, 7, 21).expect("date");
        let (weekly, now) = weekly_window(20.0, start, next_day, 0, 0);
        let limits = crate::limits::AccountRateLimits {
            primary: None,
            secondary: Some(weekly),
            ..crate::limits::AccountRateLimits::empty(crate::limits::LimitsSourceKind::OAuth, 0)
        };

        let LimitUsageGauge::Weekly {
            used_percent,
            fair_percent,
        } = limit_usage_gauge(Some(&limits), now)
        else {
            panic!("weekly gauge");
        };
        assert_eq!(used_percent, Some(20.0));
        assert!((fair_percent.expect("fair share") - 100.0 / 7.0).abs() < 0.01);
    }

    #[test]
    fn spend_limit_only_selects_a_single_segment_gauge() {
        let limits = crate::limits::AccountRateLimits {
            spend_limit: Some(crate::limits::RateLimitWindow {
                used_percent: Some(25.0),
                window_duration_mins: None,
                resets_at: None,
            }),
            ..crate::limits::AccountRateLimits::empty(
                crate::limits::LimitsSourceKind::StatusLine,
                0,
            )
        };

        assert_eq!(
            limit_usage_gauge(Some(&limits), 0),
            LimitUsageGauge::Monthly {
                used_percent: Some(25.0)
            }
        );
    }

    #[test]
    fn limits_card_shows_the_most_used_model_limit() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let window = |used: f64| crate::limits::RateLimitWindow {
            used_percent: Some(used),
            window_duration_mins: Some(crate::limits::SEVEN_DAY_WINDOW_MINS),
            resets_at: None,
        };
        let limits = crate::limits::AccountRateLimits {
            primary: Some(crate::limits::RateLimitWindow {
                used_percent: Some(10.0),
                window_duration_mins: Some(crate::limits::FIVE_HOUR_WINDOW_MINS),
                resets_at: None,
            }),
            secondary: Some(window(13.0)),
            buckets: vec![
                crate::limits::RateLimitSnapshot {
                    limit_name: Some("Sonnet".to_string()),
                    primary: Some(window(5.0)),
                },
                crate::limits::RateLimitSnapshot {
                    limit_name: Some("Opus".to_string()),
                    primary: Some(window(40.0)),
                },
            ],
            ..crate::limits::AccountRateLimits::empty(crate::limits::LimitsSourceKind::OAuth, 0)
        };

        let (value, caption1, caption2, caption3) =
            format_limits_compact_card_lines(&limits, false, formatter);
        assert_eq!(value, "5h limit: 90%");
        assert_eq!(caption1.as_deref(), Some("Weekly:   87%"));
        assert_eq!(caption2.as_deref(), Some("Opus:     60%"));
        assert_eq!(caption3, None);
    }

    #[test]
    fn monthly_gauge_has_only_the_right_edge_divider() {
        let area = Rect::new(0, 0, 12, 1);
        let mut buffer = Buffer::empty(area);
        render_limit_usage_gauge(
            &mut buffer,
            area,
            LimitUsageGauge::Monthly {
                used_percent: Some(50.0),
            },
        );

        for x in 0..6 {
            assert_eq!(buffer[(x, 0)].symbol(), LIMIT_GAUGE_FILL);
        }
        for x in 6..11 {
            assert_eq!(buffer[(x, 0)].symbol(), " ");
        }
        assert_eq!(buffer[(11, 0)].symbol(), LIMIT_GAUGE_DIVIDER);
        assert_eq!(buffer[(11, 0)].style().fg, Some(Color::DarkGray));
    }

    #[test]
    fn style_weekly_limit_line_matches_band_colors() {
        let plain = "Weekly:   83% (resets 12:14, 4 Aug)";
        let normal = style_weekly_limit_line(plain, WeeklyPaceBand::Normal, true);
        assert_eq!(normal.spans.len(), 1);
        assert_eq!(normal.spans[0].content.as_ref(), plain);
        assert_eq!(normal.spans[0].style.fg, Some(Color::White));
        assert!(normal.spans[0].style.add_modifier.contains(Modifier::BOLD));

        let yellow = style_weekly_limit_line(plain, WeeklyPaceBand::Yellow, false);
        assert_eq!(yellow.spans.len(), 1);
        assert_eq!(yellow.spans[0].content.as_ref(), plain);
        assert_eq!(yellow.spans[0].style.fg, Some(Color::Yellow));

        let orange = style_weekly_limit_line(plain, WeeklyPaceBand::Orange, false);
        assert_eq!(orange.spans.len(), 1);
        assert_eq!(orange.spans[0].content.as_ref(), plain);
        assert_eq!(orange.spans[0].style.bg, Some(WEEKLY_PACE_ORANGE_BG));
        assert_eq!(orange.spans[0].style.fg, Some(Color::White));

        let red = style_weekly_limit_line(plain, WeeklyPaceBand::Red, false);
        assert_eq!(red.spans.len(), 1);
        assert_eq!(red.spans[0].content.as_ref(), plain);
        assert_eq!(red.spans[0].style.bg, Some(Color::Red));
        assert_eq!(red.spans[0].style.fg, Some(Color::White));

        let compact_plain = "7d: 50% | 12:14";
        let compact = style_weekly_limit_line(compact_plain, WeeklyPaceBand::Orange, false);
        assert_eq!(compact.spans.len(), 1);
        assert_eq!(compact.spans[0].content.as_ref(), compact_plain);
        assert_eq!(compact.spans[0].style.bg, Some(WEEKLY_PACE_ORANGE_BG));
        assert_eq!(compact.spans[0].style.fg, Some(Color::White));
    }

    #[test]
    fn classic_horizontal_tokens_keep_full_total_and_non_cached() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let out = format_horizontal_value(
            45_456_785,
            Some(1_756_241),
            UsageMetric::Tokens,
            u16::MAX,
            formatter,
        );
        assert_eq!(out, "45,456,785 / 1,756,241");
    }

    #[test]
    fn system_horizontal_tokens_use_compact_total_and_non_cached() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter =
            DisplayFormatter::new(crate::locale::DisplayStyle::SystemCompact, &system_locale);
        let out = format_horizontal_value(
            45_456_785,
            Some(1_756_241),
            UsageMetric::Tokens,
            u16::MAX,
            formatter,
        );
        assert_eq!(out, "45.46M / 1.76M");
    }

    #[test]
    fn vertical_token_values_compact_only_in_system_mode() {
        let system_locale = crate::locale::SystemLocale::default();
        let classic = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let system =
            DisplayFormatter::new(crate::locale::DisplayStyle::SystemCompact, &system_locale);

        assert_eq!(
            format_vertical_token_value(45_456_785, 20, classic),
            "45456785"
        );
        assert_eq!(
            format_vertical_token_value(45_456_785, 20, system),
            "45.46M"
        );
    }

    #[test]
    fn system_full_compacts_only_tight_vertical_token_values() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter =
            DisplayFormatter::new(crate::locale::DisplayStyle::SystemFull, &system_locale);

        assert_eq!(
            format_horizontal_value(
                45_456_785,
                Some(1_756_241),
                UsageMetric::Tokens,
                u16::MAX,
                formatter,
            ),
            "45 456 785 / 1 756 241"
        );
        assert_eq!(
            format_vertical_token_value(45_456_785, 20, formatter),
            "45 456 785"
        );
        assert_eq!(
            format_horizontal_value(
                45_456_785,
                Some(1_756_241),
                UsageMetric::Tokens,
                8,
                formatter,
            ),
            "45 456 785 / 1 756 241"
        );
        assert_eq!(format_vertical_token_value(45_456_785, 4, formatter), "45M");
    }

    #[test]
    fn vertical_bar_hover_requires_the_filled_bar_and_ignores_gaps() {
        let area = Rect::new(10, 5, 11, 10);
        let values = [50, 100];

        assert_eq!(
            hovered_vertical_bar_index(Some((10, 10)), area, 3, 1, &values, 100),
            Some(0)
        );
        assert_eq!(
            hovered_vertical_bar_index(Some((10, 9)), area, 3, 1, &values, 100),
            None
        );
        assert_eq!(
            hovered_vertical_bar_index(Some((13, 12)), area, 3, 1, &values, 100),
            None
        );
        assert_eq!(
            hovered_vertical_bar_index(Some((14, 5)), area, 3, 1, &values, 100),
            Some(1)
        );
    }

    #[test]
    fn vertical_bar_tooltip_shows_exact_value_and_stays_inside_chart() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(DisplayStyle::Classic, &system_locale);

        assert_eq!(
            format_vertical_bar_tooltip("2026-07-22", 45_456_785, UsageMetric::Tokens, formatter,),
            "2026-07-22 | 45,456,785 tokens"
        );
        assert_eq!(
            chart_tooltip_rect(Rect::new(2, 2, 20, 10), (20, 3), 10),
            Some(Rect::new(8, 4, 12, 3))
        );
    }

    #[test]
    fn weekly_pace_tooltip_reserves_single_cell_horizontal_margins() {
        assert_eq!(
            hover_tooltip_rect_with_horizontal_padding(Rect::new(2, 2, 20, 10), (20, 3), 10, 3, 1),
            Some(Rect::new(6, 4, 14, 5))
        );
    }

    pub(crate) fn test_usage_snapshot() -> crate::usage::LocalUsageSnapshot {
        let totals = crate::usage::UsageTotalsTokens {
            last7_days_tokens: 0,
            last30_days_tokens: 0,
            average_daily_tokens: 0,
            cache_hit_rate_percent: 0.0,
            peak_day: None,
            peak_day_tokens: 0,
        };
        crate::usage::LocalUsageSnapshot {
            days: Vec::new(),
            totals: totals.clone(),
            top_models: Vec::new(),
            utc_days: Vec::new(),
            utc_totals: totals,
            utc_top_models: Vec::new(),
            activity_first_weekday: Weekday::Mon,
            model_daily: Vec::new(),
            utc_model_daily: Vec::new(),
            project_model_daily: Vec::new(),
            utc_project_model_daily: Vec::new(),
            reported_costs: crate::usage::ReportedCosts::default(),
            project_activity: Vec::new(),
            project_usage: Vec::new(),
            matched_session_files: 0,
            scan_total_files: 0,
            scan_indexed_files: 0,
            scan_pending_files: 0,
            scan_processed_bytes: 0,
        }
    }

    #[test]
    fn models_table_text_lists_models_with_four_columns() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(DisplayStyle::Classic, &system_locale);
        let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
        let tokens = |input, cache_write, cache_read, output| crate::usage::TokenBreakdown {
            input,
            cache_write,
            cache_write_1h: 0,
            cache_read,
            output,
        };
        let series =
            |model: &str, value: crate::usage::TokenBreakdown| crate::usage::ModelDailyUsage {
                model: model.to_string(),
                days: [(today.clone(), value)].into_iter().collect(),
            };
        let snapshot = crate::usage::LocalUsageSnapshot {
            days: vec![UsageDay {
                day: today.clone(),
                ..UsageDay::default()
            }],
            model_daily: vec![
                series("claude-opus-5-5", tokens(1_000, 2_000, 3_000, 4_000)),
                series("claude-sonnet-5", tokens(0, 0, 0, 10_000)),
            ],
            ..test_usage_snapshot()
        };

        let text = apistat::models_table_text(
            &snapshot,
            UsageZone::Local,
            crate::app::ModelsRange::Last7Days,
            formatter,
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("claudemon models: Last 7 days (local time"));
        assert!(lines[1].starts_with("Model"));
        assert!(lines[2].starts_with("Opus 5.5"));
        assert!(lines[2].contains("50.0%"));
        assert!(lines[2].ends_with("10,000"));
        assert!(lines[3].starts_with("Sonnet 5"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn age_labels_are_compact() {
        assert_eq!(format_age(42), "42s");
        assert_eq!(format_age(12 * 60 + 5), "12m");
        assert_eq!(format_age(3 * 3600 + 5 * 60), "3h 5m");
        assert_eq!(format_age(2 * 86_400 + 4 * 3600), "2d 4h");
    }

    #[test]
    fn horizontal_token_columns_show_input_cache_write_cache_read_and_output() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let day = UsageDay {
            day: "2026-09-24".to_string(),
            input_tokens: 2_000,
            cache_write_tokens: 1_000,
            cache_read_tokens: 7_000,
            output_tokens: 500,
            total_tokens: 10_500,
            ..UsageDay::default()
        };
        let row = usage_day_token_columns(&day);
        assert_eq!(row, [2_000, 1_000, 7_000, 500]);
        let columns = horizontal_token_columns_widths(&[row], u16::MAX, formatter);
        assert_eq!(
            format_horizontal_token_columns(row, 40, columns, formatter),
            "2,000 / 1,000 / 7,000 / 500"
        );
        let empty = [0; 4];
        let columns = horizontal_token_columns_widths(&[empty], u16::MAX, formatter);
        assert_eq!(
            format_horizontal_token_columns(empty, 13, columns, formatter),
            "0 / 0 / 0 / 0"
        );
    }

    #[test]
    fn horizontal_token_columns_align_rows_and_fit_compact_narrow_and_full_modes() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        assert_eq!(right_align_token_cell("\u{754c}", 3), " \u{754c}");
        let rows: [TokenColumns; 3] = [
            [0, 0, 0, 0],
            [121_030_387, 5_000_000, 115_000_000, 8_681_443],
            [403_643_361, 15_714_273, 387_929_088, 287_431_139],
        ];
        let columns = horizontal_token_columns_widths(&rows, u16::MAX, formatter);
        assert_eq!(columns, Some([11, 10, 11, 11]));
        let formatted = rows
            .iter()
            .map(|row| format_horizontal_token_columns(*row, u16::MAX, columns, formatter))
            .collect::<Vec<_>>();
        let separators = |row: &String| {
            row.match_indices(" / ")
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        assert!(formatted
            .iter()
            .all(|row| separators(row) == separators(&formatted[0])));

        let tight_row = [1_000_000, 2_000_000, 3_000_000, 4_000_000];
        let tight = horizontal_token_columns_widths(&[tight_row], 21, formatter);
        let compact = format_horizontal_token_columns(tight_row, 21, tight, formatter);
        assert_eq!(compact, "1M / 2M / 3M / 4M");
        for style in [
            DisplayStyle::Classic,
            DisplayStyle::SystemCompact,
            DisplayStyle::SystemFull,
        ] {
            let mixed_formatter = DisplayFormatter::new(style, &system_locale);
            let mixed_row = [1_000_000, 0, 0, 500];
            let mixed_columns = horizontal_token_columns_widths(&[mixed_row], 21, mixed_formatter);
            assert_eq!(
                format_horizontal_token_columns(mixed_row, 21, mixed_columns, mixed_formatter),
                "1M / 0 / 0 / 500",
                "{style:?}"
            );
        }
        assert_eq!(horizontal_token_columns_widths(&rows, 12, formatter), None);
        assert_eq!(
            format_horizontal_token_columns([1, 1, 0, 0], 12, None, formatter),
            "\u{2026}"
        );
        let nines = [999; 4];
        let one_cell_columns = horizontal_token_columns_widths(&[nines], 13, formatter);
        assert_eq!(
            format_horizontal_token_columns(nines, 13, one_cell_columns, formatter),
            "\u{2026} / \u{2026} / \u{2026} / \u{2026}"
        );

        let full_formatter = DisplayFormatter::new(DisplayStyle::SystemFull, &system_locale);
        let full_columns = horizontal_token_columns_widths(&rows, u16::MAX, full_formatter);
        let full = format_horizontal_token_columns(rows[2], u16::MAX, full_columns, full_formatter);
        assert!(
            full.contains("403 643 361 / 15 714 273 / 387 929 088 / 287 431 139"),
            "{full:?}"
        );
    }

    #[test]
    fn project_activity_tokens_show_total_and_out_of_cache() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        let project = ProjectActivity {
            display_path: "/tmp/SFM".to_string(),
            days: Vec::new(),
            last_activity_day: Some("2026-06-05".to_string()),
            total_tokens: 250,
            cache_read_tokens: 150,
            agent_time_ms: 0,
            agent_runs: 0,
        };

        assert_eq!(
            activity_metric_total_label(&project, UsageMetric::Tokens, formatter),
            "250 / 100 tokens"
        );
    }

    #[test]
    fn activity_color_levels_scale_to_project_max() {
        assert_eq!(activity_color_level(0, 100), 0);
        assert_eq!(activity_color_level(1, 100), 1);
        assert_eq!(activity_color_level(50, 100), 2);
        assert_eq!(activity_color_level(100, 100), 4);
    }

    #[test]
    fn activity_project_name_uses_path_leaf() {
        assert_eq!(activity_project_name("/tmp/Photonia"), "Photonia");
        assert_eq!(activity_project_name(r"C:\src\SFM"), "SFM");
    }

    #[test]
    fn activity_day_cell_width_expands_when_all_weeks_fit() {
        assert_eq!(activity_day_cell_width(108, 54), 2);
        assert_eq!(activity_day_cell_width(107, 54), 1);
    }

    #[test]
    fn activity_weekday_labels_follow_first_weekday() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(crate::locale::DisplayStyle::Classic, &system_locale);
        assert_eq!(activity_weekday_label(Weekday::Mon, 0, formatter), "Mon");
        assert_eq!(activity_weekday_label(Weekday::Mon, 6, formatter), "Sun");
        assert_eq!(activity_weekday_label(Weekday::Sun, 0, formatter), "");
        assert_eq!(activity_weekday_label(Weekday::Sun, 1, formatter), "Mon");
        assert_eq!(activity_weekday_label(Weekday::Sun, 6, formatter), "");
        assert_eq!(activity_weekday_label(Weekday::Sat, 2, formatter), "Mon");
    }
}
