//! COST screen: API-equivalent cost of local usage at list prices, by day,
//! model, and project, with Claude Code's own cost records as a cross-check.

use super::{control_group_label, pill, register_right_aligned_targets, truncate_middle};
use crate::app::{AppState, ModelsRange, UiClickAction};
use crate::locale::{DisplayFormatter, DisplayStyle};
use crate::pricing::{CostBreakdown, PriceTable};
use crate::usage::{model_display_name, LocalUsageSnapshot, ModelDailyUsage, UsageZone};
use chrono::NaiveDate;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::BTreeMap;

const MAX_PROJECT_ROWS: usize = 12;
const MIDDLE_DOT: &str = "\u{00b7}";

/// Costs of the selected range.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct CostView {
    pub(crate) days: Vec<(String, f64)>,
    pub(crate) models: Vec<(String, CostBreakdown)>,
    pub(crate) projects: Vec<(String, f64)>,
    pub(crate) total: f64,
    /// Tokens of models without a configured price.
    pub(crate) unpriced: BTreeMap<String, i64>,
}

fn add_series(
    series: &ModelDailyUsage,
    days: &[String],
    pricing: &PriceTable,
    unpriced: &mut BTreeMap<String, i64>,
) -> (CostBreakdown, Vec<f64>) {
    let mut total = CostBreakdown::default();
    let mut per_day = vec![0.0; days.len()];
    for (index, day) in days.iter().enumerate() {
        let Some(tokens) = series.days.get(day) else {
            continue;
        };
        match pricing.cost(&series.model, *tokens) {
            Some(cost) => {
                per_day[index] += cost.total();
                total.add(cost);
            }
            None => *unpriced.entry(series.model.clone()).or_default() += tokens.total(),
        }
    }
    (total, per_day)
}

pub(crate) fn cost_view(
    snapshot: &LocalUsageSnapshot,
    zone: UsageZone,
    range: ModelsRange,
    pricing: &PriceTable,
) -> CostView {
    let days = super::apistat::range_days(snapshot, zone, range);
    let mut view = CostView {
        days: days.iter().map(|day| (day.clone(), 0.0)).collect(),
        ..CostView::default()
    };
    for series in snapshot.model_daily_for_zone(zone) {
        let (total, per_day) = add_series(series, &days, pricing, &mut view.unpriced);
        for (slot, value) in view.days.iter_mut().zip(per_day) {
            slot.1 += value;
        }
        if total.total() > 0.0 {
            view.total += total.total();
            view.models.push((series.model.clone(), total));
        }
    }
    view.models
        .sort_by(|left, right| right.1.total().total_cmp(&left.1.total()));
    let mut ignored = BTreeMap::new();
    for project in snapshot.project_model_daily_for_zone(zone) {
        let cost: f64 = project
            .models
            .iter()
            .map(|series| add_series(series, &days, pricing, &mut ignored).0.total())
            .sum();
        if cost > 0.0 {
            view.projects.push((project.project.clone(), cost));
        }
    }
    view.projects
        .sort_by(|left, right| right.1.total_cmp(&left.1));
    view
}

pub(crate) fn format_usd(value: f64, formatter: DisplayFormatter<'_>) -> String {
    if value.abs() >= 1000.0 {
        format!("${}", formatter.format_count(value.round() as i64))
    } else {
        format!("${}", formatter.format_two_decimals(value))
    }
}

pub(crate) fn render_header(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let line_area = super::header_line_area(area);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(40)])
        .split(line_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "COST :: API-EQUIVALENT :: LOCAL",
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        row[0],
    );
    let updated = state
        .usage_updated_label()
        .unwrap_or_else(|| "Updated --".to_string());
    frame.render_widget(Paragraph::new(updated).alignment(Alignment::Right), row[1]);
}

fn render_controls(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let accent = state.accent_text_color();
    let mut segments: Vec<(&str, Option<UiClickAction>)> = vec![(" RANGE ", None)];
    let mut spans = vec![control_group_label("RANGE", accent)];
    for (label, range) in [
        (" ALL ", ModelsRange::AllTime),
        (" 7D ", ModelsRange::Last7Days),
        (" 30D ", ModelsRange::Last30Days),
    ] {
        segments.push((label, Some(UiClickAction::SetCostRange(range))));
        spans.push(pill(label.trim(), state.cost_range == range));
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

fn boxed(title: &str) -> Block<'static> {
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

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(area);
    render_controls(frame, chunks[0], state);

    let formatter = state.formatter();
    let accent = state.accent_text_color();
    let Some(snapshot) = state.usage.as_ref() else {
        frame.render_widget(
            Paragraph::new("Indexing local transcripts...").block(boxed("COST")),
            chunks[2],
        );
        return;
    };
    let view = cost_view(snapshot, state.usage_zone, state.cost_range, &state.pricing);

    // Summary and labeling.
    let today = view.days.last().map(|(_, cost)| *cost).unwrap_or(0.0);
    let active_days = view.days.iter().filter(|(_, cost)| *cost > 0.0).count();
    let average = if active_days > 0 {
        view.total / active_days as f64
    } else {
        0.0
    };
    let plan = state
        .limits
        .as_ref()
        .and_then(|limits| limits.plan.as_deref())
        .map(|plan| format!("plan {plan}: "))
        .unwrap_or_default();
    let summary = Text::from(vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", state.cost_range.label().to_uppercase()),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                format_usd(view.total, formatter),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   TODAY {}", format_usd(today, formatter)),
                Style::default(),
            ),
            Span::styled(
                format!("   AVG / ACTIVE DAY {}", format_usd(average, formatter)),
                Style::default(),
            ),
        ]),
        Line::from(Span::styled(
            format!(
                "API-equivalent at list prices (table {}; edit `pricing` in config.json). {plan}subscription usage is not billed per token.",
                state.pricing.updated
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(Paragraph::new(summary).wrap(Wrap { trim: true }), chunks[1]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[2]);
    render_daily(frame, columns[0], &view, accent, formatter);
    let tables = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(view.models.len() as u16 + 3),
            Constraint::Min(3),
        ])
        .split(columns[1]);
    render_models(frame, tables[0], &view, formatter);
    render_projects(frame, tables[1], &view, accent, formatter);
    render_cross_check(frame, chunks[3], snapshot, &view, &state.pricing, formatter);
}

fn render_daily(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    accent: Color,
    formatter: DisplayFormatter<'_>,
) {
    let rows = area.height.saturating_sub(2) as usize;
    let shown: Vec<&(String, f64)> = view.days.iter().rev().take(rows).rev().collect();
    let max = shown
        .iter()
        .map(|(_, cost)| *cost)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    let labels: Vec<String> = shown
        .iter()
        .map(|(day, _)| {
            NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .map(|date| formatter.format_short_date(date))
                .unwrap_or_else(|_| day.clone())
        })
        .collect();
    let values: Vec<String> = shown
        .iter()
        .map(|(_, cost)| format_usd(*cost, formatter))
        .collect();
    let label_width = labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    let value_width = values
        .iter()
        .map(|value| value.chars().count())
        .max()
        .unwrap_or(0);
    let bar_width = (area.width as usize)
        .saturating_sub(label_width + value_width + 6)
        .max(1);
    let lines: Vec<Line<'static>> = shown
        .iter()
        .zip(labels.iter().zip(&values))
        .map(|((_, cost), (label, value))| {
            let filled = ((cost / max) * bar_width as f64).round() as usize;
            Line::from(vec![
                Span::styled(
                    format!("{label:<label_width$} "),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    "\u{2588}".repeat(filled.min(bar_width)),
                    Style::default().fg(accent),
                ),
                Span::raw(" ".repeat(bar_width - filled.min(bar_width))),
                Span::raw(format!(" {value:>value_width$}")),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(boxed("BY DAY")),
        area,
    );
}

fn render_models(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    formatter: DisplayFormatter<'_>,
) {
    let header = Line::from(Span::styled(
        format!(
            "{:<12} {:>10} {:>10} {:>10} {:>10} {:>11} {:>6}",
            "MODEL", "INPUT", "CACHE-W", "CACHE-R", "OUTPUT", "TOTAL", "SHARE"
        ),
        Style::default().fg(Color::Gray),
    ));
    let mut lines = vec![header];
    for (model, cost) in &view.models {
        let share = if view.total > 0.0 {
            cost.total() / view.total * 100.0
        } else {
            0.0
        };
        lines.push(Line::from(format!(
            "{:<12} {:>10} {:>10} {:>10} {:>10} {:>11} {:>5}%",
            truncate_middle(&model_display_name(model), 12),
            format_usd(cost.input, formatter),
            format_usd(cost.cache_write, formatter),
            format_usd(cost.cache_read, formatter),
            format_usd(cost.output, formatter),
            format_usd(cost.total(), formatter),
            formatter.format_one_decimal(share),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(boxed("BY MODEL")),
        area,
    );
}

fn render_projects(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &CostView,
    accent: Color,
    formatter: DisplayFormatter<'_>,
) {
    let width = area.width.saturating_sub(18) as usize;
    let mut lines: Vec<Line<'static>> = view
        .projects
        .iter()
        .take(MAX_PROJECT_ROWS.min(area.height.saturating_sub(2) as usize))
        .map(|(project, cost)| {
            Line::from(vec![
                Span::raw(format!("{:<width$}", truncate_middle(project, width))),
                Span::styled(
                    format!(" {:>12}", format_usd(*cost, formatter)),
                    Style::default().fg(accent),
                ),
            ])
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from("--"));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(boxed("BY PROJECT")),
        area,
    );
}

fn render_cross_check(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &LocalUsageSnapshot,
    view: &CostView,
    pricing: &PriceTable,
    formatter: DisplayFormatter<'_>,
) {
    let mut lines = Vec::new();
    let reported = &snapshot.reported_costs;
    if reported.sessions > 0 {
        let priced: f64 = reported
            .tokens_by_model
            .iter()
            .filter_map(|(model, tokens)| pricing.cost(model, *tokens))
            .map(|cost| cost.total())
            .sum();
        lines.push(Line::from(vec![
            Span::styled("CHECK  ", Style::default().fg(Color::Gray)),
            Span::raw(format!(
                "Claude Code recorded {} for {} session(s) {MIDDLE_DOT} the same transcripts price at {} (background model calls such as titles are not in transcripts)",
                format_usd(reported.reported_usd, formatter),
                formatter.format_usize(reported.sessions),
                format_usd(priced, formatter),
            )),
        ]));
    }
    if !view.unpriced.is_empty() {
        let names: Vec<String> = view
            .unpriced
            .iter()
            .map(|(model, tokens)| format!("{model} ({} tokens)", formatter.format_count(*tokens)))
            .collect();
        lines.push(Line::from(Span::styled(
            format!(
                "Unpriced models excluded: {}. Add them to `pricing` in config.json.",
                names.join(", ")
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{ProjectModelDaily, TokenBreakdown};

    #[test]
    fn cost_view_prices_days_models_projects_and_flags_unpriced_models() {
        let system_locale = crate::locale::SystemLocale::default();
        let formatter = DisplayFormatter::new(DisplayStyle::Classic, &system_locale);
        let today = chrono::Local::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let key = |date: NaiveDate| date.format("%Y-%m-%d").to_string();
        let million_output = TokenBreakdown {
            output: 1_000_000,
            ..TokenBreakdown::default()
        };
        let series = |model: &str| ModelDailyUsage {
            model: model.to_string(),
            days: [
                (key(yesterday), million_output),
                (key(today), million_output),
            ]
            .into_iter()
            .collect(),
        };
        let mut snapshot = crate::ui::tests::test_usage_snapshot();
        snapshot.days = vec![crate::usage::UsageDay {
            day: key(today),
            ..crate::usage::UsageDay::default()
        }];
        snapshot.model_daily = vec![series("claude-sonnet-5"), series("mystery-model")];
        snapshot.project_model_daily = vec![ProjectModelDaily {
            project: "/work/app".to_string(),
            models: vec![series("claude-sonnet-5")],
        }];

        let view = cost_view(
            &snapshot,
            UsageZone::Local,
            ModelsRange::Last7Days,
            &PriceTable::default(),
        );
        assert_eq!(view.days.len(), 7);
        assert_eq!(view.days[6].1, 10.0);
        assert_eq!(view.days[5].1, 10.0);
        assert_eq!(view.total, 20.0);
        assert_eq!(view.models.len(), 1);
        assert_eq!(view.projects, vec![("/work/app".to_string(), 20.0)]);
        assert_eq!(view.unpriced["mystery-model"], 2_000_000);
        assert_eq!(format_usd(view.total, formatter), "$20.00");
        assert_eq!(format_usd(1234.4, formatter), "$1,234");
    }
}
