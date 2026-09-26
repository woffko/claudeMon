//! APISTAT screen: live subscription limits (Overview) and per-model token
//! usage over time (Models).

use super::{
    control_group_label, format_compact_kmb, pill, register_right_aligned_targets, resets_label,
    truncate_middle, weekly_elapsed_allowed_percent,
};
use crate::app::{ApiStatView, AppState, LiveLimitsMode, ModelsRange, UiClickAction, UiHitTarget};
use crate::limits::{AccountRateLimits, LimitsSourceKind, RateLimitWindow};
use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::usage::{
    format_tokens_compact, model_display_name, LocalUsageSnapshot, ModelDailyUsage, TokenBreakdown,
    UsageZone,
};
use chrono::{Duration as ChronoDuration, NaiveDate};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Chart, Dataset, GraphType, Padding, Paragraph, Wrap,
};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

/// Status-line snapshots older than this are marked stale.
const STALE_SNAPSHOT_SECS: i64 = 15 * 60;
const GAUGE_WIDTH: usize = 24;
const MAX_CHART_MODELS: usize = 6;
const DOT: &str = "\u{25cf}";
const MIDDLE_DOT: &str = "\u{00b7}";

pub(crate) fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let line_area = super::header_line_area(area);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(40)])
        .split(line_area);

    let source = match state.api_stat_view {
        ApiStatView::Models => "LOCAL".to_string(),
        ApiStatView::Overview => match (&state.limits, state.live_limits_mode) {
            (Some(limits), _) => limits.source.label().to_string(),
            (None, LiveLimitsMode::StatusLine) => "STATUSLINE".to_string(),
            (None, LiveLimitsMode::OAuth) => "OAUTH".to_string(),
            (None, LiveLimitsMode::Off) => "OFF".to_string(),
        },
    };
    let view = match state.api_stat_view {
        ApiStatView::Overview => "LIMITS",
        ApiStatView::Models => "MODELS",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("APISTAT :: {view} :: {source}"),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        row[0],
    );

    let updated = match state.api_stat_view {
        ApiStatView::Models => state.usage_updated_label(),
        ApiStatView::Overview => state.limits_updated_label(),
    }
    .unwrap_or_else(|| "Updated --".to_string());
    frame.render_widget(Paragraph::new(updated).alignment(Alignment::Right), row[1]);
}

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    render_controls(frame, chunks[0], state);
    match state.api_stat_view {
        ApiStatView::Overview => render_overview(frame, chunks[1], state),
        ApiStatView::Models => render_models(frame, chunks[1], state),
    }
}

fn render_controls(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let accent = state.accent_text_color();
    let mut segments: Vec<(&str, Option<UiClickAction>)> = vec![
        (" VIEW ", None),
        (
            " LIMITS ",
            Some(UiClickAction::SetApiStatView(ApiStatView::Overview)),
        ),
        (
            " MODELS ",
            Some(UiClickAction::SetApiStatView(ApiStatView::Models)),
        ),
    ];
    let mut spans = vec![
        control_group_label("VIEW", accent),
        pill("LIMITS", state.api_stat_view == ApiStatView::Overview),
        pill("MODELS", state.api_stat_view == ApiStatView::Models),
    ];
    if state.api_stat_view == ApiStatView::Models {
        segments.push((" RANGE ", None));
        spans.push(control_group_label("RANGE", accent));
        for (label, range) in [
            (" ALL ", ModelsRange::AllTime),
            (" 7D ", ModelsRange::Last7Days),
            (" 30D ", ModelsRange::Last30Days),
        ] {
            segments.push((label, Some(UiClickAction::SetModelsRange(range))));
            spans.push(pill(label.trim(), state.models_range == range));
        }
        segments.push((" COPY ", Some(UiClickAction::CopyModelsTable)));
        spans.push(pill("COPY", false));
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
    if let Some(notice) = state.api_stat_notice.as_deref() {
        let controls_width = segments
            .iter()
            .map(|(text, _)| UnicodeWidthStr::width(*text))
            .sum::<usize>();
        let free = (area.width as usize).saturating_sub(controls_width + 2);
        if free > 8 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_middle(notice, free),
                    Style::default().fg(accent),
                )),
                Rect::new(area.x, area.y, free as u16, 1),
            );
        }
    }
}

fn boxed(title: &str) -> Block<'static> {
    Block::default()
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
        ))
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

fn render_overview(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let formatter = state.formatter();
    let accent = state.accent_text_color();
    let now = crate::limits::unix_now();

    let Some(limits) = state.limits.as_ref().filter(|_| state.limits_enabled) else {
        let message = state
            .limits_error
            .as_deref()
            .or(state.limits_notice.as_deref())
            .unwrap_or("Loading limits...");
        frame.render_widget(
            Paragraph::new(setup_text(message, state.live_limits_mode, accent))
                .block(boxed("LIMITS"))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(source_line(limits, now, formatter));
    if let Some(error) = state.limits_error.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("Last refresh failed: {error}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));

    let mut rows: Vec<(String, &RateLimitWindow)> = Vec::new();
    if let Some(window) = limits.primary.as_ref() {
        rows.push(("5h session".to_string(), window));
    }
    if let Some(window) = limits.secondary.as_ref() {
        rows.push(("Weekly".to_string(), window));
    }
    for bucket in &limits.buckets {
        if let Some(window) = bucket.primary.as_ref() {
            let name = bucket.limit_name.as_deref().unwrap_or("Scoped");
            rows.push((format!("Weekly {}", truncate_middle(name, 12)), window));
        }
    }
    if let Some(window) = limits.spend_limit.as_ref() {
        rows.push(("Spend limit".to_string(), window));
    }
    if rows.is_empty() {
        lines.push(Line::from("The source reported no limit windows."));
    }
    for (label, window) in rows {
        let active = limits
            .active_limit
            .as_deref()
            .is_some_and(|active| label.ends_with(active) || active == label);
        lines.push(limit_row(&label, window, active, now, accent, formatter));
    }

    if let Some(extra) = limits.extra_usage.as_ref() {
        lines.push(Line::from(""));
        let currency = extra.currency.as_deref().unwrap_or("");
        let amount = |value: Option<f64>| {
            value
                .map(|value| format!("{} {currency}", formatter.format_two_decimals(value)))
                .unwrap_or_else(|| "--".to_string())
        };
        let mut text = format!(
            "{}  used {}",
            if extra.enabled { "enabled" } else { "disabled" },
            amount(extra.used_amount).trim_end()
        );
        if extra.limit_amount.is_some() {
            text.push_str(&format!(" of {}", amount(extra.limit_amount).trim_end()));
        }
        if let Some(percent) = extra.used_percent.filter(|_| extra.limit_amount.is_some()) {
            text.push_str(&format!(" ({}%)", percent.round() as i64));
        }
        lines.push(Line::from(vec![
            Span::styled("EXTRA USAGE  ", Style::default().fg(Color::Gray)),
            Span::raw(text),
        ]));
    }

    if !limits.surfaces.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "WEEKLY USAGE BY SURFACE",
            Style::default().fg(Color::Gray),
        )));
        for surface in &limits.surfaces {
            lines.push(Line::from(vec![
                Span::raw(format!("  {:<14}", truncate_middle(&surface.name, 14))),
                gauge_span(Some(surface.percent), None, accent),
                Span::raw(format!(" {:>3}%", surface.percent.round() as i64)),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(boxed("LIMITS"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn setup_text(message: &str, mode: LiveLimitsMode, accent: Color) -> Text<'static> {
    let mut lines = vec![Line::from(message.to_string()), Line::from("")];
    if mode == LiveLimitsMode::StatusLine {
        let code = Style::default().fg(accent);
        lines.extend([
            Line::from(Span::styled(
                "Enable live limits (no credentials needed):",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  Add to ~/.claude/settings.json:"),
            Line::from(Span::styled(
                "    \"statusLine\": {\"type\": \"command\", \"command\": \"claudemon statusline\"}",
                code,
            )),
            Line::from("  Keep an existing status line by wrapping it:"),
            Line::from(Span::styled(
                "    claudemon statusline --wrap '<your status-line command>'",
                code,
            )),
            Line::from(
                "  Limits appear after Claude Code's next reply and update while it runs.",
            ),
            Line::from(""),
            Line::from(Span::styled(
                "Opt-in: --live-limits oauth reads Claude Code's OAuth token and adds",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "per-model weekly limits, extra usage, and the usage-by-surface breakdown.",
                Style::default().fg(Color::Gray),
            )),
        ]);
    }
    Text::from(lines)
}

fn source_line(
    limits: &AccountRateLimits,
    now: i64,
    formatter: DisplayFormatter<'_>,
) -> Line<'static> {
    let age = now.saturating_sub(limits.captured_at).max(0);
    let captured = super::format_age(age);
    let (source, stale) = match limits.source {
        LimitsSourceKind::StatusLine => ("status-line snapshot", age > STALE_SNAPSHOT_SECS),
        LimitsSourceKind::OAuth => ("OAuth usage endpoint", false),
    };
    let mut spans = vec![
        Span::styled("SOURCE  ", Style::default().fg(Color::Gray)),
        Span::raw(format!("{source}, captured {captured} ago")),
    ];
    if let Some(plan) = limits.plan.as_deref() {
        spans.push(Span::raw(format!("  {MIDDLE_DOT}  plan {plan}")));
    }
    if stale {
        spans.push(Span::styled(
            "  (stale: updates while Claude Code runs)",
            Style::default().fg(Color::Yellow),
        ));
    }
    let _ = formatter;
    Line::from(spans)
}

fn limit_row(
    label: &str,
    window: &RateLimitWindow,
    active: bool,
    now: i64,
    accent: Color,
    formatter: DisplayFormatter<'_>,
) -> Line<'static> {
    let fair = window
        .window_duration_mins
        .filter(|minutes| *minutes >= crate::limits::SEVEN_DAY_WINDOW_MINS - 1.0)
        .and_then(|_| weekly_elapsed_allowed_percent(window, now));
    let used = window
        .used_percent
        .map(|used| format!("{:>3}% used", used.round() as i64))
        .unwrap_or_else(|| "  --% used".to_string());
    let resets = resets_label(window.resets_at, formatter)
        .map(|label| {
            let remaining = window
                .resets_at
                .map(|reset| super::format_age(reset.saturating_sub(now).max(0)))
                .unwrap_or_default();
            format!("  {label} (in {remaining})")
        })
        .unwrap_or_default();
    let label_style = if active {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::styled(format!("{:<20}", truncate_middle(label, 20)), label_style),
        gauge_span(window.used_percent, fair, accent),
        Span::raw(format!(" {used}")),
        Span::styled(resets, Style::default().fg(Color::Gray)),
    ];
    if active {
        spans.push(Span::styled("  binding", Style::default().fg(accent)));
    }
    Line::from(spans)
}

/// Text gauge: filled cells for usage; a marker for the fair share of a
/// weekly window at the current time.
fn gauge_span(used: Option<f64>, fair: Option<f64>, accent: Color) -> Span<'static> {
    let cells =
        |percent: f64| ((percent.clamp(0.0, 100.0) / 100.0) * GAUGE_WIDTH as f64).round() as usize;
    let filled = used.map(cells).unwrap_or(0);
    let marker = fair.map(|fair| cells(fair).min(GAUGE_WIDTH.saturating_sub(1)));
    let mut text = String::with_capacity(GAUGE_WIDTH * 3);
    for index in 0..GAUGE_WIDTH {
        let glyph = if Some(index) == marker {
            '\u{2502}'
        } else if index < filled {
            '\u{2588}'
        } else {
            '\u{2591}'
        };
        text.push(glyph);
    }
    let over_pace = matches!((used, fair), (Some(used), Some(fair)) if used > fair);
    let color = if used.is_some_and(|used| used >= 90.0) {
        Color::Red
    } else if over_pace {
        Color::Yellow
    } else {
        accent
    };
    Span::styled(text, Style::default().fg(color))
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// Model usage within the selected range, largest first.
struct ModelRangeUsage<'a> {
    series: &'a ModelDailyUsage,
    total: TokenBreakdown,
}

/// Day keys (`YYYY-MM-DD`) of the selected range, oldest first.
fn range_days(snapshot: &LocalUsageSnapshot, zone: UsageZone, range: ModelsRange) -> Vec<String> {
    let days = snapshot.days_for_zone(zone);
    let Some(last) = days
        .last()
        .and_then(|day| NaiveDate::parse_from_str(&day.day, "%Y-%m-%d").ok())
    else {
        return Vec::new();
    };
    let first = match range.days() {
        Some(count) => last - ChronoDuration::days(count.saturating_sub(1) as i64),
        None => snapshot
            .model_daily_for_zone(zone)
            .iter()
            .filter_map(|series| series.days.keys().next())
            .filter_map(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
            .min()
            .unwrap_or(last)
            .min(last),
    };
    let mut out = Vec::new();
    let mut day = first;
    while day <= last {
        out.push(day.format("%Y-%m-%d").to_string());
        day += ChronoDuration::days(1);
    }
    out
}

fn models_in_range<'a>(
    snapshot: &'a LocalUsageSnapshot,
    zone: UsageZone,
    first_day: Option<&str>,
) -> Vec<ModelRangeUsage<'a>> {
    let mut models: Vec<ModelRangeUsage<'a>> = snapshot
        .model_daily_for_zone(zone)
        .iter()
        .map(|series| ModelRangeUsage {
            series,
            total: series.total_since(first_day),
        })
        .filter(|usage| usage.total.total() > 0)
        .collect();
    models.sort_by(|left, right| {
        right
            .total
            .total()
            .cmp(&left.total.total())
            .then_with(|| left.series.model.cmp(&right.series.model))
    });
    models
}

fn series_color(index: usize, accent: Color) -> Color {
    [
        accent,
        Color::LightGreen,
        Color::Yellow,
        Color::LightMagenta,
        Color::LightBlue,
        Color::LightRed,
    ][index % MAX_CHART_MODELS]
}

fn render_models(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let accent = state.accent_text_color();
    let formatter = state.formatter();
    let Some(snapshot) = state.usage.as_ref() else {
        frame.render_widget(
            Paragraph::new("Indexing local transcripts...").block(boxed("MODELS")),
            area,
        );
        return;
    };
    let zone = state.usage_zone;
    let days = range_days(snapshot, zone, state.models_range);
    let models = models_in_range(snapshot, zone, days.first().map(String::as_str));
    if days.is_empty() || models.is_empty() {
        frame.render_widget(
            Paragraph::new("No model usage in this range.").block(boxed("MODELS")),
            area,
        );
        return;
    }

    let card_rows = models.len().div_ceil(2) as u16 * 4;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(card_rows.min(area.height / 2).max(4)),
        ])
        .split(area);

    render_models_chart(
        frame, chunks[0], snapshot, zone, &days, &models, accent, formatter,
    );
    render_models_legend(frame, chunks[1], &models, accent);
    let mut targets = Vec::new();
    render_range_selector(frame, chunks[2], state.models_range, accent, &mut targets);
    render_model_cards(frame, chunks[3], &models, accent, formatter);
    state.ui_hit_targets.extend(targets);
}

#[allow(clippy::too_many_arguments)]
fn render_models_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &LocalUsageSnapshot,
    zone: UsageZone,
    days: &[String],
    models: &[ModelRangeUsage<'_>],
    accent: Color,
    formatter: DisplayFormatter<'_>,
) {
    let _ = (snapshot, zone);
    let points: Vec<Vec<(f64, f64)>> = models
        .iter()
        .take(MAX_CHART_MODELS)
        .map(|usage| {
            days.iter()
                .enumerate()
                .map(|(index, day)| {
                    let tokens = usage
                        .series
                        .days
                        .get(day)
                        .map(|tokens| tokens.total())
                        .unwrap_or(0);
                    (index as f64, tokens as f64)
                })
                .collect()
        })
        .collect();
    let max_value = points
        .iter()
        .flatten()
        .map(|(_, value)| *value)
        .fold(0.0_f64, f64::max)
        .max(1.0);

    let datasets: Vec<Dataset<'_>> = points
        .iter()
        .enumerate()
        .map(|(index, data)| {
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(series_color(index, accent)))
                .data(data)
        })
        .collect();

    let y_label = |value: f64| format_compact_kmb(value.round() as u64, 8, formatter);
    let x_label = |index: usize| {
        days.get(index)
            .and_then(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
            .map(|date| formatter.format_short_date(date))
            .unwrap_or_default()
    };
    let last_index = days.len().saturating_sub(1);
    let x_max = (last_index as f64).max(1.0);
    let chart = Chart::new(datasets)
        .block(boxed("TOKENS PER DAY"))
        .legend_position(None)
        .x_axis(
            Axis::default()
                .bounds([0.0, x_max])
                .style(Style::default().fg(Color::Gray))
                .labels(vec![
                    Line::from(x_label(0)),
                    Line::from(x_label(last_index / 2)),
                    Line::from(x_label(last_index)),
                ]),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, max_value * 1.05])
                .style(Style::default().fg(Color::Gray))
                .labels(vec![
                    Line::from("0"),
                    Line::from(y_label(max_value / 2.0)),
                    Line::from(y_label(max_value)),
                ]),
        );
    frame.render_widget(chart, area);
}

fn render_models_legend(
    frame: &mut Frame<'_>,
    area: Rect,
    models: &[ModelRangeUsage<'_>],
    accent: Color,
) {
    let mut spans = vec![Span::raw(" ")];
    for (index, usage) in models.iter().take(MAX_CHART_MODELS).enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                format!(" {MIDDLE_DOT} "),
                Style::default().fg(Color::Gray),
            ));
        }
        spans.push(Span::styled(
            DOT,
            Style::default().fg(series_color(index, accent)),
        ));
        spans.push(Span::raw(format!(
            " {}",
            model_display_name(&usage.series.model)
        )));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_range_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    selected: ModelsRange,
    accent: Color,
    targets: &mut Vec<UiHitTarget>,
) {
    let mut spans = vec![Span::raw(" ")];
    let mut x = area.x.saturating_add(1);
    for (index, range) in ModelsRange::ALL.into_iter().enumerate() {
        if index > 0 {
            let separator = format!(" {MIDDLE_DOT} ");
            x = x.saturating_add(UnicodeWidthStr::width(separator.as_str()) as u16);
            spans.push(Span::styled(separator, Style::default().fg(Color::Gray)));
        }
        let label = range.label();
        let width = UnicodeWidthStr::width(label) as u16;
        let style = if range == selected {
            Style::default().fg(accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
        targets.push(UiHitTarget {
            area: Rect::new(x, area.y, width, 1),
            action: UiClickAction::SetModelsRange(range),
        });
        x = x.saturating_add(width);
    }
    spans.push(Span::styled(
        format!("   d cycle dates {MIDDLE_DOT} v limits {MIDDLE_DOT} ctrl+s copy"),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_model_cards(
    frame: &mut Frame<'_>,
    area: Rect,
    models: &[ModelRangeUsage<'_>],
    accent: Color,
    formatter: DisplayFormatter<'_>,
) {
    let grand_total: i64 = models.iter().map(|usage| usage.total.total()).sum();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let mut column_lines: [Vec<Line<'static>>; 2] = [Vec::new(), Vec::new()];
    for (index, usage) in models.iter().enumerate() {
        let share = if grand_total > 0 {
            usage.total.total() as f64 / grand_total as f64 * 100.0
        } else {
            0.0
        };
        let tokens = |value: i64| format_tokens_compact(value, formatter);
        let lines = &mut column_lines[index % 2];
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {DOT} "),
                Style::default().fg(series_color(index, accent)),
            ),
            Span::styled(
                model_display_name(&usage.series.model),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" ({}%)", formatter.format_one_decimal(share)),
                Style::default().fg(Color::Gray),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "   In: {} {MIDDLE_DOT} Out: {}",
                tokens(usage.total.input),
                tokens(usage.total.output)
            ),
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "   Cache: {} read {MIDDLE_DOT} {} write",
                tokens(usage.total.cache_read),
                tokens(usage.total.cache_write)
            ),
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(""));
    }
    for (column, lines) in columns.iter().zip(column_lines) {
        frame.render_widget(Paragraph::new(Text::from(lines)), *column);
    }
}

/// Plain-text table of the Models view for the clipboard.
pub(crate) fn models_table_text(
    snapshot: &LocalUsageSnapshot,
    zone: UsageZone,
    range: ModelsRange,
    formatter: DisplayFormatter<'_>,
) -> String {
    let days = range_days(snapshot, zone, range);
    let models = models_in_range(snapshot, zone, days.first().map(String::as_str));
    if models.is_empty() {
        return String::new();
    }
    let grand_total: i64 = models.iter().map(|usage| usage.total.total()).sum();
    let mut rows = vec![[
        "Model".to_string(),
        "Share".to_string(),
        "Input".to_string(),
        "Cache write".to_string(),
        "Cache read".to_string(),
        "Output".to_string(),
        "Total".to_string(),
    ]];
    for usage in &models {
        let share = usage.total.total() as f64 / grand_total.max(1) as f64 * 100.0;
        rows.push([
            model_display_name(&usage.series.model),
            format!("{}%", formatter.format_one_decimal(share)),
            formatter.format_count(usage.total.input),
            formatter.format_count(usage.total.cache_write),
            formatter.format_count(usage.total.cache_read),
            formatter.format_count(usage.total.output),
            formatter.format_count(usage.total.total()),
        ]);
    }
    let widths: Vec<usize> = (0..7)
        .map(|column| {
            rows.iter()
                .map(|row| UnicodeWidthStr::width(row[column].as_str()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = format!(
        "claudemon models: {} ({}, {} to {})\n",
        range.label(),
        match zone {
            UsageZone::Local => "local time",
            UsageZone::Utc => "UTC",
        },
        days.first().map(String::as_str).unwrap_or("--"),
        days.last().map(String::as_str).unwrap_or("--"),
    );
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let pad = widths[column].saturating_sub(UnicodeWidthStr::width(cell.as_str()));
                if column == 0 {
                    format!("{cell}{}", " ".repeat(pad))
                } else {
                    format!("{}{cell}", " ".repeat(pad))
                }
            })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}
