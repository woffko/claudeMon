use crate::locale::{DisplayFormatter, DisplayStyle};
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc, Weekday};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant, SystemTime};

const MAX_ACTIVITY_GAP_MS: i64 = 2 * 60 * 1000;
const DEFAULT_MAX_SESSION_FILE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_SESSION_FILES_SCANNED: usize = 10_000;
const DEFAULT_MAX_JSONL_LINE_BYTES: usize = 512 * 1024;
const DEFAULT_SCAN_TIME_BUDGET_MS: u64 = 1500;
const MAX_DISTINCT_MODELS: usize = 5_000;
// v14: each session has one immutable owner cwd. Derived cache rows from
// earlier schemas can contain mutable-context or git-collapsed attribution.
const SCAN_CACHE_DB_SCHEMA_VERSION: i64 = 1;
pub(crate) const PROJECT_IDENTITY_LINE_LIMIT: usize = 128;
const MAX_OWNER_IDENTITY_LINE_BYTES: usize = 512 * 1024;
pub const DEFAULT_SCAN_CACHE_MAX_ENTRIES: usize = 50_000;
pub const ACTIVITY_TIMELINE_WEEKS: usize = 54;
pub const ACTIVITY_TIMELINE_DAYS: usize = ACTIVITY_TIMELINE_WEEKS * 7;

#[derive(Debug, Clone, Copy)]
pub struct ScanLimits {
    pub max_session_file_bytes: u64,
    pub max_session_total_bytes: u64,
    pub max_session_files_scanned: usize,
    pub max_jsonl_line_bytes: usize,
    pub scan_time_budget_ms: u64,
    pub full_scan: bool,
    pub scan_cache_max_entries: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_session_file_bytes: DEFAULT_MAX_SESSION_FILE_BYTES,
            max_session_total_bytes: DEFAULT_MAX_SESSION_TOTAL_BYTES,
            max_session_files_scanned: DEFAULT_MAX_SESSION_FILES_SCANNED,
            max_jsonl_line_bytes: DEFAULT_MAX_JSONL_LINE_BYTES,
            scan_time_budget_ms: DEFAULT_SCAN_TIME_BUDGET_MS,
            full_scan: false,
            scan_cache_max_entries: DEFAULT_SCAN_CACHE_MAX_ENTRIES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMetric {
    Tokens,
    Time,
    Runs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartRange {
    Day,
    Week,
    Month,
}

impl ChartRange {
    pub fn toggled(self) -> Self {
        match self {
            Self::Day => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Day,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageZone {
    Local,
    Utc,
}

impl UsageZone {
    pub fn toggled(self) -> Self {
        match self {
            Self::Local => Self::Utc,
            Self::Utc => Self::Local,
        }
    }
}

pub fn format_compact_kmb(value: u64, max_width: u16, formatter: DisplayFormatter<'_>) -> String {
    // Examples (depending on width):
    //  1234 -> 1.23K / 1.2K / 1K
    //  12_345_678 -> 12.35M / 12.3M / 12M
    //  999 -> 999
    if max_width == 0 {
        return String::new();
    }
    if value < 1000 {
        let s = formatter.format_u64(value);
        return if s.len() <= max_width as usize {
            s
        } else {
            // Worst-case truncate from the left.
            s[s.len().saturating_sub(max_width as usize)..].to_string()
        };
    }

    let (div, suffix) = if value >= 1_000_000_000_000 {
        (1_000_000_000_000f64, "T")
    } else if value >= 1_000_000_000 {
        (1_000_000_000f64, "B")
    } else if value >= 1_000_000 {
        (1_000_000f64, "M")
    } else {
        (1_000f64, "K")
    };
    let scaled = (value as f64) / div;

    // In dense layouts, force integer suffixes (e.g. 27M instead of 27.4M).
    // Heuristic: if the label width is <= 5 cells, decimals tend to hurt readability.
    if max_width <= 5 {
        let s = format_compact_scaled(scaled, suffix, 0, formatter);
        return if s.len() <= max_width as usize {
            s
        } else {
            // truncate
            s[..max_width as usize].to_string()
        };
    }

    // Prefer 2 decimals, then reduce precision if it doesn't fit.
    for decimals in [2usize, 1usize, 0usize] {
        let s = format_compact_scaled(scaled, suffix, decimals, formatter);
        if s.len() <= max_width as usize {
            return s;
        }
    }

    // Final fallback: "1K/M/B/T"
    let s = formatter.localize_decimal(&format!("{:.0}{suffix}", scaled.max(0.0).round()));
    if s.len() <= max_width as usize {
        return s;
    }

    // Last resort: truncate.
    s[..max_width as usize].to_string()
}

fn format_compact_scaled(
    value: f64,
    suffix: &str,
    decimals: usize,
    formatter: DisplayFormatter<'_>,
) -> String {
    // Format with fixed decimals, then trim trailing zeros and a trailing dot.
    let mut s = format!("{:.*}", decimals, value.max(0.0));
    if decimals > 0 {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s.push_str(suffix);
    formatter.localize_decimal(&s)
}

#[derive(Debug, Clone, Default)]
pub struct UsageDay {
    pub day: String,
    /// Uncached input tokens.
    pub input_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    /// Sum of all four token categories.
    pub total_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
}

impl UsageDay {
    fn from_totals(day: &str, totals: DailyTotals) -> Self {
        Self {
            day: day.to_string(),
            input_tokens: totals.input,
            cache_write_tokens: totals.cache_write,
            cache_read_tokens: totals.cache_read,
            output_tokens: totals.output,
            total_tokens: totals.tokens(),
            agent_time_ms: totals.agent_ms,
            agent_runs: totals.agent_runs,
        }
    }

    pub fn short_label(&self, formatter: DisplayFormatter<'_>) -> String {
        format_day_short(&self.day, formatter)
    }

    /// All prompt-side tokens: uncached input plus cache writes and reads.
    pub fn prompt_tokens(&self) -> i64 {
        self.input_tokens
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.cache_read_tokens)
    }
}

fn format_day_short(day: &str, formatter: DisplayFormatter<'_>) -> String {
    // Expect YYYY-MM-DD
    if day.len() == 10 {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
            return formatter.format_short_date(date);
        }
    }
    day.to_string()
}

#[derive(Debug, Clone)]
pub struct UsageTotalsTokens {
    pub last7_days_tokens: i64,
    pub last30_days_tokens: i64,
    pub average_daily_tokens: i64,
    pub cache_hit_rate_percent: f64,
    pub peak_day: Option<String>,
    pub peak_day_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct LocalUsageModel {
    pub model: String,
    pub tokens: i64,
    pub share_percent: f64,
}

#[derive(Debug, Clone)]
pub struct ProjectActivity {
    pub display_path: String,
    pub days: Vec<UsageDay>,
    pub last_activity_day: Option<String>,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
}

#[derive(Debug, Clone)]
pub struct ProjectUsageSummary {
    pub display_path: String,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub agent_time_ms: i64,
    pub agent_runs: i64,
    pub indexed_files: usize,
}

#[derive(Debug, Clone)]
pub struct LocalUsageSnapshot {
    pub days: Vec<UsageDay>,
    pub totals: UsageTotalsTokens,
    pub top_models: Vec<LocalUsageModel>,
    pub utc_days: Vec<UsageDay>,
    pub utc_totals: UsageTotalsTokens,
    pub utc_top_models: Vec<LocalUsageModel>,
    pub activity_first_weekday: Weekday,
    pub project_activity: Vec<ProjectActivity>,
    pub project_usage: Vec<ProjectUsageSummary>,
    // Number of session files that were identified as belonging to the selected workspace filter.
    // When no workspace filter is used, this is 0.
    pub matched_session_files: u32,
    pub scan_total_files: usize,
    pub scan_indexed_files: usize,
    pub scan_pending_files: usize,
    pub scan_processed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct UsageTotalsView {
    pub last7_primary_label: String,
    pub last30_primary_label: String,
    pub avg_primary_label: String,
    pub cache_label: String,
    pub total_label: String,
    pub runs_label: String,
    pub peak_day_label: String,
    pub peak_sub_label: String,
}

impl LocalUsageSnapshot {
    pub fn project_usage_for_path(&self, path: &str) -> Option<&ProjectUsageSummary> {
        let key = normalize_project_key(path);
        self.project_usage
            .iter()
            .find(|project| normalize_project_key(&project.display_path) == key)
    }

    pub fn days_for_zone(&self, zone: UsageZone) -> &[UsageDay] {
        match zone {
            UsageZone::Local => &self.days,
            UsageZone::Utc => &self.utc_days,
        }
    }

    pub fn totals_for_zone(&self, zone: UsageZone) -> &UsageTotalsTokens {
        match zone {
            UsageZone::Local => &self.totals,
            UsageZone::Utc => &self.utc_totals,
        }
    }

    pub fn top_models_for_zone(&self, zone: UsageZone) -> &[LocalUsageModel] {
        match zone {
            UsageZone::Local => &self.top_models,
            UsageZone::Utc => &self.utc_top_models,
        }
    }

    pub fn last7_days_for_zone(&self, zone: UsageZone) -> Vec<UsageDay> {
        self.last_n_days_for_zone(zone, 7)
    }

    pub fn last_n_days_for_zone(&self, zone: UsageZone, n: usize) -> Vec<UsageDay> {
        self.days_for_zone(zone)
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    pub fn totals_view_for_zone(
        &self,
        metric: UsageMetric,
        formatter: DisplayFormatter<'_>,
        zone: UsageZone,
    ) -> UsageTotalsView {
        let totals = self.totals_for_zone(zone);
        let last7 = self.last7_days_for_zone(zone);
        let last30 = self.last_n_days_for_zone(zone, 30);
        let last7_agent_ms: i64 = last7.iter().map(|d| d.agent_time_ms).sum();
        let last30_agent_ms: i64 = last30.iter().map(|d| d.agent_time_ms).sum();
        let last7_runs: i64 = last7.iter().map(|d| d.agent_runs).sum();
        let last30_runs: i64 = last30.iter().map(|d| d.agent_runs).sum();

        let (peak_day, _peak_value, peak_sub) = match metric {
            UsageMetric::Tokens => {
                let peak = last30
                    .iter()
                    .max_by_key(|d| d.total_tokens)
                    .filter(|d| d.total_tokens > 0);
                let peak_day = peak
                    .map(|d| d.short_label(formatter))
                    .unwrap_or_else(|| "--".to_string());
                let peak_tokens = peak.map(|d| d.total_tokens).unwrap_or(0);
                let sub = format!("{} tokens", format_tokens_compact(peak_tokens, formatter));
                (peak_day, peak_tokens, sub)
            }
            UsageMetric::Time => {
                let peak = last30
                    .iter()
                    .max_by_key(|d| d.agent_time_ms)
                    .filter(|d| d.agent_time_ms > 0);
                let peak_day = peak
                    .map(|d| d.short_label(formatter))
                    .unwrap_or_else(|| "--".to_string());
                let peak_ms = peak.map(|d| d.agent_time_ms).unwrap_or(0);
                let sub = format!("{} agent time", format_duration_compact(peak_ms));
                (peak_day, peak_ms, sub)
            }
            UsageMetric::Runs => {
                let peak = last30
                    .iter()
                    .max_by_key(|d| d.agent_runs)
                    .filter(|d| d.agent_runs > 0);
                let peak_day = peak
                    .map(|d| d.short_label(formatter))
                    .unwrap_or_else(|| "--".to_string());
                let peak_runs = peak.map(|d| d.agent_runs).unwrap_or(0);
                let sub = format!("{} runs", format_count(peak_runs, formatter));
                (peak_day, peak_runs, sub)
            }
        };

        match metric {
            UsageMetric::Tokens => UsageTotalsView {
                last7_primary_label: format!(
                    "{} tokens",
                    format_tokens_compact(totals.last7_days_tokens, formatter)
                ),
                last30_primary_label: format!(
                    "{} tokens",
                    format_tokens_compact(totals.last30_days_tokens, formatter)
                ),
                avg_primary_label: format_tokens_compact(totals.average_daily_tokens, formatter),
                cache_label: format!(
                    "{}%",
                    formatter.format_one_decimal(totals.cache_hit_rate_percent)
                ),
                total_label: format_tokens_overview(totals.last30_days_tokens, formatter),
                runs_label: format!("{} runs", format_count(last7_runs, formatter)),
                peak_day_label: self
                    .totals_for_zone(zone)
                    .peak_day
                    .as_deref()
                    .map(|day| format_day_short(day, formatter))
                    .unwrap_or(peak_day),
                peak_sub_label: format!(
                    "{} tokens",
                    format_tokens_compact(totals.peak_day_tokens, formatter)
                ),
            },
            UsageMetric::Time => {
                let avg_ms = if last7.is_empty() {
                    0
                } else {
                    (last7_agent_ms as f64 / last7.len() as f64).round() as i64
                };
                UsageTotalsView {
                    last7_primary_label: format_duration_compact(last7_agent_ms),
                    last30_primary_label: format_duration_compact(last30_agent_ms),
                    avg_primary_label: format_duration_compact(avg_ms),
                    cache_label: "--".to_string(),
                    total_label: format_duration(last30_agent_ms),
                    runs_label: format!("{} runs", format_count(last7_runs, formatter)),
                    peak_day_label: peak_day,
                    peak_sub_label: peak_sub,
                }
            }
            UsageMetric::Runs => {
                let avg_7 = if last7.is_empty() {
                    0
                } else {
                    (last7_runs as f64 / last7.len() as f64).round() as i64
                };
                UsageTotalsView {
                    last7_primary_label: format!("{} runs", format_count(last7_runs, formatter)),
                    last30_primary_label: format!("{} runs", format_count(last30_runs, formatter)),
                    avg_primary_label: format_count(avg_7, formatter),
                    cache_label: "--".to_string(),
                    total_label: format_count(last30_runs, formatter),
                    runs_label: format!("{} runs", format_count(last7_runs, formatter)),
                    peak_day_label: peak_day,
                    peak_sub_label: peak_sub,
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct DailyTotals {
    #[serde(default)]
    input: i64,
    #[serde(default)]
    cache_write: i64,
    #[serde(default)]
    cache_read: i64,
    #[serde(default)]
    output: i64,
    #[serde(default)]
    agent_ms: i64,
    #[serde(default)]
    agent_runs: i64,
}

impl DailyTotals {
    fn tokens(self) -> i64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }

    fn add(&mut self, other: DailyTotals) {
        self.input = self.input.saturating_add(other.input);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.output = self.output.saturating_add(other.output);
        self.agent_ms = self.agent_ms.saturating_add(other.agent_ms);
        self.agent_runs = self.agent_runs.saturating_add(other.agent_runs);
    }
}

/// Token usage of one Claude API response (`message.usage`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TokenUsage {
    input: i64,
    cache_write: i64,
    cache_read: i64,
    output: i64,
}

impl TokenUsage {
    fn total(self) -> i64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }
}

/// Incremental parser state persisted with each cache row so appended session
/// files resume at `file_offset` without replaying earlier lines.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ParserState {
    #[serde(default)]
    last_activity_ms: Option<i64>,
    /// `message.id` + `requestId` of the last counted response. Claude Code
    /// writes one line per content block and repeats the same usage on each;
    /// those lines are contiguous, so remembering the last key deduplicates.
    #[serde(default)]
    last_usage_key: Option<String>,
    /// `promptId` of the current turn; a new value starts a new run.
    #[serde(default)]
    last_prompt_id: Option<String>,
    /// Gap-estimated agent time already counted for the current turn. A later
    /// `turn_duration` record replaces the estimate with the exact duration.
    #[serde(default)]
    turn_estimate_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionOwner {
    pub(crate) cwd: String,
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ScanCacheStore {
    entries: HashMap<String, CachedFileScanEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CachedFileScanEntry {
    size: u64,
    modified_epoch_secs: Option<u64>,
    #[serde(default)]
    file_offset: u64,
    #[serde(default = "default_true")]
    fully_parsed: bool,
    session_cwd: Option<String>,
    #[serde(default)]
    parser_state: ParserState,
    #[serde(default)]
    daily: HashMap<String, DailyTotals>,
    #[serde(default)]
    model_totals_by_day: HashMap<String, HashMap<String, i64>>,
    updated_at: i64,
}

#[derive(Debug, Clone, Default)]
struct FileScanSummary {
    session_cwd: Option<String>,
    parser_state: ParserState,
    file_offset: u64,
    fully_parsed: bool,
    daily: HashMap<String, DailyTotals>,
    model_totals_by_day: HashMap<String, HashMap<String, i64>>,
}

#[derive(Debug, Clone, Default)]
struct ProjectActivityBuilder {
    display_path: String,
    daily: HashMap<String, DailyTotals>,
}

#[derive(Debug, Default)]
struct ProjectUsageBuilder {
    display_path: String,
    total_tokens: i64,
    cache_read_tokens: i64,
    agent_time_ms: i64,
    agent_runs: i64,
    indexed_files: usize,
}

fn default_true() -> bool {
    true
}

#[derive(Debug)]
struct ScanCacheDb {
    path: PathBuf,
    conn: Connection,
}

/// Resolves the Claude Code config directory: explicit override, then
/// `CLAUDE_CONFIG_DIR`, then `~/.claude`.
pub fn resolve_claude_dir(override_dir: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = override_dir {
        return Some(path);
    }
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed).join(".claude"));
            }
        }
    }
    None
}

pub fn compute_snapshot(
    days: u32,
    claude_dir: &Path,
    workspace_path: Option<&Path>,
    limits: ScanLimits,
    scan_cache_db_path: Option<&Path>,
) -> Result<LocalUsageSnapshot> {
    let days = days.clamp(1, 90);

    let sessions_root = claude_dir.join("projects");
    // The configured window controls summary cards and model shares. Charts are
    // expanded to the complete indexed history after cached rows are applied.
    let summary_day_keys = make_day_keys_for_zone(days, UsageZone::Local);
    let utc_summary_day_keys = make_day_keys_for_zone(days, UsageZone::Utc);
    let summary_day_filter: HashSet<String> = summary_day_keys.iter().cloned().collect();
    let utc_summary_day_filter: HashSet<String> = utc_summary_day_keys.iter().cloned().collect();
    let activity_first_weekday = system_first_weekday();
    let activity_day_keys = make_activity_day_keys(activity_first_weekday);
    let mut scan_day_keys = activity_day_keys.clone();
    for day_key in &summary_day_keys {
        if !scan_day_keys.contains(day_key) {
            scan_day_keys.push(day_key.clone());
        }
    }
    let mut daily: HashMap<String, DailyTotals> = scan_day_keys
        .iter()
        .map(|key| (key.clone(), DailyTotals::default()))
        .collect();
    let mut utc_daily: HashMap<String, DailyTotals> = utc_summary_day_keys
        .iter()
        .map(|key| (key.clone(), DailyTotals::default()))
        .collect();
    let mut model_totals: HashMap<String, i64> = HashMap::new();
    let mut utc_model_totals: HashMap<String, i64> = HashMap::new();
    let mut project_activity: HashMap<String, ProjectActivityBuilder> = HashMap::new();

    if !sessions_root.exists() {
        return Ok(build_snapshot(
            summary_day_keys,
            daily,
            model_totals,
            utc_summary_day_keys,
            utc_daily,
            utc_model_totals,
            0,
            activity_first_weekday,
            activity_day_keys,
            project_activity,
            Vec::new(),
            0,
            0,
            0,
            0,
        ));
    }

    // Build a full candidate list (metadata-only).
    let mut candidates = collect_session_file_candidates(&sessions_root);
    // Newest activity first.
    candidates.sort_by(|a, b| {
        b.modified_epoch_secs
            .cmp(&a.modified_epoch_secs)
            .then_with(|| a.len.cmp(&b.len))
    });

    let mut matched_session_files: u32 = 0;
    let mut scan_cache_db = scan_cache_db_path
        .map(open_or_init_scan_cache_db)
        .transpose()?;
    let candidate_paths: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.path.to_string_lossy().to_string())
        .collect();
    let (mut scan_cache_store, mut removed_cache_paths) = if let Some(db) = scan_cache_db.as_ref() {
        load_scan_cache_store(db)?
    } else {
        (ScanCacheStore::default(), HashSet::new())
    };

    // Claude Code deletes transcripts after `cleanupPeriodDays` (30 by
    // default). Rows of deleted transcripts are kept and still counted so
    // history outlives the raw logs. Rows for files that still exist but are
    // no longer valid candidates are dropped; rows outside this projects root
    // (another --claude-dir) are kept but not counted.
    let mut archived_cache_paths: Vec<String> = Vec::new();
    if scan_cache_db.is_some() {
        let valid_paths: HashSet<&str> =
            candidate_paths.iter().map(|value| value.as_str()).collect();
        scan_cache_store.entries.retain(|file_path, _| {
            if valid_paths.contains(file_path.as_str()) {
                return true;
            }
            let path = Path::new(file_path);
            if !path.starts_with(&sessions_root) {
                return true;
            }
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    archived_cache_paths.push(file_path.clone());
                    true
                }
                _ => {
                    removed_cache_paths.insert(file_path.clone());
                    false
                }
            }
        });
        archived_cache_paths.sort();
    }

    let force_reparse_all = limits.full_scan && limits.scan_time_budget_ms == 0;
    // Index every local session eventually. The normal scan budgets keep each
    // refresh bounded; `full_scan` still processes the whole backlog at once.
    let eligible_indices: Vec<usize> = (0..candidates.len()).collect();
    let mut work_indices: Vec<usize> = eligible_indices
        .iter()
        .copied()
        .filter(|idx| {
            if force_reparse_all {
                return true;
            }
            let key = &candidate_paths[*idx];
            scan_cache_store
                .entries
                .get(key)
                .is_none_or(|entry| !cache_entry_matches_candidate(entry, &candidates[*idx]))
        })
        .collect();

    // Changed or partially parsed files are latency-sensitive. Uncached files form a
    // deterministic backlog: once one is cached it drops out of this list, so later
    // refreshes naturally advance instead of rescanning the same newest files forever.
    work_indices.sort_by_key(|idx| {
        let key = &candidate_paths[*idx];
        match scan_cache_store.entries.get(key) {
            // Appended live sessions first, then brand-new sessions. Large or
            // unresolved partial files rotate only after current usage has had
            // a chance to enter the cache.
            Some(entry) if entry.fully_parsed => (0_u8, entry.updated_at),
            None => (1_u8, i64::MIN),
            Some(entry) => (2_u8, entry.updated_at),
        }
    });

    let mut planned_indices: Vec<usize> = Vec::new();
    let mut planned_total_bytes: u64 = 0;
    for idx in work_indices {
        if limits.full_scan {
            planned_indices.push(idx);
            continue;
        }
        if planned_indices.len() >= limits.max_session_files_scanned
            || planned_total_bytes >= limits.max_session_total_bytes
        {
            break;
        }
        let candidate = &candidates[idx];
        let already_read = scan_cache_store
            .entries
            .get(&candidate_paths[idx])
            .map(|entry| entry.file_offset.min(candidate.len))
            .unwrap_or(0);
        let remaining = candidate.len.saturating_sub(already_read).max(1);
        let candidate_weight = remaining.min(limits.max_session_file_bytes.max(1));
        if !planned_indices.is_empty()
            && planned_total_bytes.saturating_add(candidate_weight) > limits.max_session_total_bytes
        {
            continue;
        }
        planned_indices.push(idx);
        planned_total_bytes = planned_total_bytes.saturating_add(candidate_weight);
    }
    let planned_set: HashSet<usize> = planned_indices.iter().copied().collect();
    let scan_deadline = if limits.scan_time_budget_ms == 0 {
        None
    } else {
        Instant::now().checked_add(StdDuration::from_millis(limits.scan_time_budget_ms))
    };
    let mut dirty_cache_paths: HashSet<String> = HashSet::new();
    let mut uncached_indexed_files = 0usize;

    if scan_cache_db.is_some() {
        for (idx, candidate) in candidates.iter().enumerate() {
            let candidate_key = &candidate_paths[idx];
            let cached_entry = scan_cache_store.entries.get(candidate_key).cloned();
            let cached_entry_matches = if force_reparse_all {
                None
            } else {
                cached_entry
                    .as_ref()
                    .filter(|entry| cache_entry_matches_candidate(entry, candidate))
                    .cloned()
            };

            if planned_set.contains(&idx) {
                let entry = if let Some(entry) = cached_entry_matches {
                    entry
                } else {
                    if let Some(deadline) = scan_deadline {
                        if Instant::now() >= deadline {
                            if let Some(stale) = cached_entry {
                                apply_cached_file_entry(
                                    &stale,
                                    workspace_path,
                                    &mut daily,
                                    &mut model_totals,
                                    &summary_day_filter,
                                    &mut utc_daily,
                                    &mut utc_model_totals,
                                    &utc_summary_day_filter,
                                    &mut project_activity,
                                    &mut matched_session_files,
                                );
                            }
                            continue;
                        }
                    }

                    let parsed = match parse_file_summary(
                        &candidate.path,
                        limits.max_jsonl_line_bytes,
                        cached_entry.as_ref(),
                        scan_deadline,
                    ) {
                        Ok(parsed) => parsed,
                        Err(_) => {
                            if let Some(stale) = cached_entry {
                                apply_cached_file_entry(
                                    &stale,
                                    workspace_path,
                                    &mut daily,
                                    &mut model_totals,
                                    &summary_day_filter,
                                    &mut utc_daily,
                                    &mut utc_model_totals,
                                    &utc_summary_day_filter,
                                    &mut project_activity,
                                    &mut matched_session_files,
                                );
                            }
                            continue;
                        }
                    };
                    let entry = CachedFileScanEntry {
                        size: candidate.len,
                        modified_epoch_secs: candidate.modified_epoch_secs,
                        file_offset: parsed.file_offset.min(candidate.len),
                        fully_parsed: parsed.fully_parsed,
                        session_cwd: parsed.session_cwd,
                        parser_state: parsed.parser_state,
                        daily: parsed.daily,
                        model_totals_by_day: parsed.model_totals_by_day,
                        updated_at: unix_time_seconds(),
                    };
                    scan_cache_store
                        .entries
                        .insert(candidate_key.clone(), entry.clone());
                    dirty_cache_paths.insert(candidate_key.clone());
                    entry
                };
                apply_cached_file_entry(
                    &entry,
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &summary_day_filter,
                    &mut utc_daily,
                    &mut utc_model_totals,
                    &utc_summary_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
                continue;
            }

            if let Some(entry) = cached_entry_matches {
                apply_cached_file_entry(
                    &entry,
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &summary_day_filter,
                    &mut utc_daily,
                    &mut utc_model_totals,
                    &utc_summary_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
                continue;
            }

            if let Some(stale) = cached_entry {
                apply_cached_file_entry(
                    &stale,
                    workspace_path,
                    &mut daily,
                    &mut model_totals,
                    &summary_day_filter,
                    &mut utc_daily,
                    &mut utc_model_totals,
                    &utc_summary_day_filter,
                    &mut project_activity,
                    &mut matched_session_files,
                );
            }
        }
    } else {
        for idx in planned_indices {
            let candidate = &candidates[idx];
            let parsed =
                parse_file_summary(&candidate.path, limits.max_jsonl_line_bytes, None, None)?;
            let entry = CachedFileScanEntry {
                size: candidate.len,
                modified_epoch_secs: candidate.modified_epoch_secs,
                file_offset: parsed.file_offset.min(candidate.len),
                fully_parsed: parsed.fully_parsed,
                session_cwd: parsed.session_cwd,
                parser_state: parsed.parser_state,
                daily: parsed.daily,
                model_totals_by_day: parsed.model_totals_by_day,
                updated_at: unix_time_seconds(),
            };
            if cache_entry_matches_candidate(&entry, candidate) {
                uncached_indexed_files = uncached_indexed_files.saturating_add(1);
            }
            apply_cached_file_entry(
                &entry,
                workspace_path,
                &mut daily,
                &mut model_totals,
                &summary_day_filter,
                &mut utc_daily,
                &mut utc_model_totals,
                &utc_summary_day_filter,
                &mut project_activity,
                &mut matched_session_files,
            );
            scan_cache_store
                .entries
                .insert(candidate_paths[idx].clone(), entry);
        }
    }

    for key in &archived_cache_paths {
        if let Some(entry) = scan_cache_store.entries.get(key) {
            apply_cached_file_entry(
                entry,
                workspace_path,
                &mut daily,
                &mut model_totals,
                &summary_day_filter,
                &mut utc_daily,
                &mut utc_model_totals,
                &utc_summary_day_filter,
                &mut project_activity,
                &mut matched_session_files,
            );
        }
    }

    if let Some(scan_cache_db) = scan_cache_db.as_mut() {
        if !dirty_cache_paths.is_empty() || !removed_cache_paths.is_empty() {
            let _ = persist_scan_cache_changes(
                scan_cache_db,
                &scan_cache_store,
                &removed_cache_paths,
                &dirty_cache_paths,
            );
        }
        let _ = trim_scan_cache_db_entries_to_limit(
            scan_cache_db,
            limits.scan_cache_max_entries.max(1),
        );
    }

    let scan_total_files = eligible_indices.len();
    let scan_indexed_files = if scan_cache_db.is_some() {
        eligible_indices
            .iter()
            .filter(|idx| {
                scan_cache_store
                    .entries
                    .get(&candidate_paths[**idx])
                    .is_some_and(|entry| cache_entry_matches_candidate(entry, &candidates[**idx]))
            })
            .count()
    } else {
        uncached_indexed_files
    };
    let scan_pending_files = scan_total_files.saturating_sub(scan_indexed_files);
    let scan_processed_bytes = eligible_indices
        .iter()
        .filter_map(|idx| {
            scan_cache_store
                .entries
                .get(&candidate_paths[*idx])
                .map(|entry| entry.file_offset.min(candidates[*idx].len))
        })
        .fold(0_u64, u64::saturating_add);

    let chart_day_keys = make_complete_chart_day_keys(&daily, UsageZone::Local, &summary_day_keys);
    let utc_chart_day_keys =
        make_complete_chart_day_keys(&utc_daily, UsageZone::Utc, &utc_summary_day_keys);
    let project_usage = build_project_usage_summaries(
        &candidates,
        &candidate_paths,
        &archived_cache_paths,
        &scan_cache_store,
    );

    Ok(build_snapshot(
        chart_day_keys,
        daily,
        model_totals,
        utc_chart_day_keys,
        utc_daily,
        utc_model_totals,
        matched_session_files,
        activity_first_weekday,
        activity_day_keys,
        project_activity,
        project_usage,
        scan_total_files,
        scan_indexed_files,
        scan_pending_files,
        scan_processed_bytes,
    ))
}

#[derive(Debug, Clone)]
struct SessionFileCandidate {
    path: PathBuf,
    len: u64,
    modified_epoch_secs: Option<u64>,
}

fn collect_session_file_candidates(sessions_root: &Path) -> Vec<SessionFileCandidate> {
    let mut out: Vec<SessionFileCandidate> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![sessions_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Never follow symlinks or read special files (FIFOs/devices) from an untrusted sessions tree.
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let len = meta.len();
            if len == 0 {
                continue;
            }

            let modified = meta.modified().ok();
            let modified_epoch_secs = modified
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            out.push(SessionFileCandidate {
                path,
                len,
                modified_epoch_secs,
            });
        }
    }

    out
}

fn cache_entry_matches_candidate(
    entry: &CachedFileScanEntry,
    candidate: &SessionFileCandidate,
) -> bool {
    entry.fully_parsed
        && entry.size == candidate.len
        && entry.modified_epoch_secs == candidate.modified_epoch_secs
        && entry.file_offset >= candidate.len
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    day_keys: Vec<String>,
    daily: HashMap<String, DailyTotals>,
    model_totals: HashMap<String, i64>,
    utc_day_keys: Vec<String>,
    utc_daily: HashMap<String, DailyTotals>,
    utc_model_totals: HashMap<String, i64>,
    matched_session_files: u32,
    activity_first_weekday: Weekday,
    activity_day_keys: Vec<String>,
    project_activity: HashMap<String, ProjectActivityBuilder>,
    project_usage: Vec<ProjectUsageSummary>,
    scan_total_files: usize,
    scan_indexed_files: usize,
    scan_pending_files: usize,
    scan_processed_bytes: u64,
) -> LocalUsageSnapshot {
    let (days, totals, top_models) = build_zone_snapshot(day_keys, daily, model_totals);
    let (utc_days, utc_totals, utc_top_models) =
        build_zone_snapshot(utc_day_keys, utc_daily, utc_model_totals);

    LocalUsageSnapshot {
        days,
        totals,
        top_models,
        utc_days,
        utc_totals,
        utc_top_models,
        activity_first_weekday,
        project_activity: build_project_activity(activity_day_keys, project_activity),
        project_usage,
        matched_session_files,
        scan_total_files,
        scan_indexed_files,
        scan_pending_files,
        scan_processed_bytes,
    }
}

fn build_zone_snapshot(
    day_keys: Vec<String>,
    daily: HashMap<String, DailyTotals>,
    model_totals: HashMap<String, i64>,
) -> (Vec<UsageDay>, UsageTotalsTokens, Vec<LocalUsageModel>) {
    let mut days: Vec<UsageDay> = Vec::with_capacity(day_keys.len());

    for day_key in &day_keys {
        let totals = daily.get(day_key).copied().unwrap_or_default();
        days.push(UsageDay::from_totals(day_key, totals));
    }

    let last30 = days.iter().rev().take(30).cloned().collect::<Vec<_>>();
    let last7 = days.iter().rev().take(7).cloned().collect::<Vec<_>>();
    let total_tokens: i64 = last30.iter().map(|day| day.total_tokens).sum();
    let last7_tokens: i64 = last7.iter().map(|day| day.total_tokens).sum();
    let last7_prompt: i64 = last7.iter().map(UsageDay::prompt_tokens).sum();
    let last7_cache_read: i64 = last7.iter().map(|day| day.cache_read_tokens).sum();

    let average_daily_tokens = if last7.is_empty() {
        0
    } else {
        ((last7_tokens as f64) / (last7.len() as f64)).round() as i64
    };

    let cache_hit_rate_percent = if last7_prompt > 0 {
        ((last7_cache_read as f64) / (last7_prompt as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let peak = last30
        .iter()
        .max_by_key(|day| day.total_tokens)
        .filter(|day| day.total_tokens > 0);
    let peak_day = peak.map(|day| day.day.clone());
    let peak_day_tokens = peak.map(|day| day.total_tokens).unwrap_or(0);

    let mut top_models: Vec<LocalUsageModel> = model_totals
        .into_iter()
        .filter(|(model, tokens)| model != "unknown" && *tokens > 0)
        .map(|(model, tokens)| LocalUsageModel {
            model,
            tokens,
            share_percent: if total_tokens > 0 {
                ((tokens as f64) / (total_tokens as f64) * 1000.0).round() / 10.0
            } else {
                0.0
            },
        })
        .collect();
    top_models.sort_by_key(|model| std::cmp::Reverse(model.tokens));
    top_models.truncate(4);

    (
        days,
        UsageTotalsTokens {
            last7_days_tokens: last7_tokens,
            last30_days_tokens: total_tokens,
            average_daily_tokens,
            cache_hit_rate_percent,
            peak_day,
            peak_day_tokens,
        },
        top_models,
    )
}

fn build_project_usage_summaries(
    candidates: &[SessionFileCandidate],
    candidate_paths: &[String],
    archived_paths: &[String],
    cache: &ScanCacheStore,
) -> Vec<ProjectUsageSummary> {
    let mut projects: HashMap<String, ProjectUsageBuilder> = HashMap::new();

    let indexed = candidates
        .iter()
        .zip(candidate_paths)
        .filter_map(|(candidate, path)| {
            cache
                .entries
                .get(path)
                .filter(|entry| cache_entry_matches_candidate(entry, candidate))
        });
    let archived = archived_paths
        .iter()
        .filter_map(|path| cache.entries.get(path));
    for entry in indexed.chain(archived) {
        for cwd in entry_project_paths(entry) {
            let key = normalize_project_key(&cwd);
            let project = projects.entry(key).or_default();
            prefer_project_display_path(&mut project.display_path, &cwd);
            project.indexed_files = project.indexed_files.saturating_add(1);
        }

        let Some(cwd) = entry
            .session_cwd
            .as_deref()
            .and_then(fallback_project_identity)
        else {
            continue;
        };
        let project = projects.entry(normalize_project_key(&cwd)).or_default();
        for (cache_key, totals) in &entry.daily {
            let Some((UsageZone::Local, _)) = split_cache_day_key(cache_key) else {
                continue;
            };
            project.total_tokens = project.total_tokens.saturating_add(totals.tokens());
            project.cache_read_tokens = project.cache_read_tokens.saturating_add(totals.cache_read);
            project.agent_time_ms = project.agent_time_ms.saturating_add(totals.agent_ms);
            project.agent_runs = project.agent_runs.saturating_add(totals.agent_runs);
        }
    }

    let mut out = projects
        .into_values()
        .map(|project| ProjectUsageSummary {
            display_path: project.display_path,
            total_tokens: project.total_tokens,
            cache_read_tokens: project.cache_read_tokens,
            agent_time_ms: project.agent_time_ms,
            agent_runs: project.agent_runs,
            indexed_files: project.indexed_files,
        })
        .collect::<Vec<_>>();
    out.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    out
}

fn entry_project_paths(entry: &CachedFileScanEntry) -> Vec<String> {
    entry
        .session_cwd
        .as_deref()
        .and_then(fallback_project_identity)
        .into_iter()
        .collect()
}

fn prefer_project_display_path(current: &mut String, candidate: &str) {
    if current.is_empty()
        || candidate.len() < current.len()
        || (candidate.len() == current.len() && candidate < current.as_str())
    {
        *current = candidate.to_string();
    }
}

fn build_project_activity(
    day_keys: Vec<String>,
    projects: HashMap<String, ProjectActivityBuilder>,
) -> Vec<ProjectActivity> {
    let mut out: Vec<ProjectActivity> = Vec::with_capacity(projects.len());

    for (_, project) in projects {
        let mut days: Vec<UsageDay> = Vec::with_capacity(day_keys.len());
        let mut total_tokens = 0i64;
        let mut cache_read_tokens = 0i64;
        let mut agent_time_ms = 0i64;
        let mut agent_runs = 0i64;
        let mut last_activity_day: Option<String> = None;

        for day_key in &day_keys {
            let totals = project.daily.get(day_key).copied().unwrap_or_default();
            total_tokens += totals.tokens();
            cache_read_tokens += totals.cache_read;
            agent_time_ms += totals.agent_ms;
            agent_runs += totals.agent_runs;
            if daily_has_activity(totals) {
                last_activity_day = Some(day_key.clone());
            }
            days.push(UsageDay::from_totals(day_key, totals));
        }

        if last_activity_day.is_none() {
            continue;
        }

        out.push(ProjectActivity {
            display_path: project.display_path,
            days,
            last_activity_day,
            total_tokens,
            cache_read_tokens,
            agent_time_ms,
            agent_runs,
        });
    }

    out.sort_by(|left, right| {
        right
            .last_activity_day
            .cmp(&left.last_activity_day)
            .then_with(|| left.display_path.cmp(&right.display_path))
    });
    out
}

fn daily_has_activity(totals: DailyTotals) -> bool {
    totals.tokens() > 0 || totals.agent_ms > 0 || totals.agent_runs > 0
}

fn fallback_project_identity(session_cwd: &str) -> Option<String> {
    session_cwd_identity(session_cwd)
}

fn add_model_tokens_limited(
    model_totals: &mut HashMap<String, i64>,
    model: String,
    delta_tokens: i64,
) {
    if delta_tokens <= 0 {
        return;
    }
    if model_totals.len() <= MAX_DISTINCT_MODELS || model_totals.contains_key(&model) {
        *model_totals.entry(model).or_insert(0) += delta_tokens;
    } else {
        *model_totals.entry("other".to_string()).or_insert(0) += delta_tokens;
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_cached_file_entry(
    entry: &CachedFileScanEntry,
    workspace_path: Option<&Path>,
    daily: &mut HashMap<String, DailyTotals>,
    model_totals: &mut HashMap<String, i64>,
    chart_day_filter: &HashSet<String>,
    utc_daily: &mut HashMap<String, DailyTotals>,
    utc_model_totals: &mut HashMap<String, i64>,
    utc_chart_day_filter: &HashSet<String>,
    project_activity: &mut HashMap<String, ProjectActivityBuilder>,
    matched_session_files: &mut u32,
) {
    let matches_workspace = match workspace_path {
        None => true,
        Some(filter) => entry_project_paths(entry)
            .iter()
            .any(|project| path_matches_workspace(project, filter)),
    };
    if !matches_workspace {
        return;
    }

    if workspace_path.is_some() {
        *matched_session_files = matched_session_files.saturating_add(1);
    }

    for (cache_key, totals) in &entry.daily {
        let Some((zone, day_key)) = split_cache_day_key(cache_key) else {
            continue;
        };
        let target = match zone {
            UsageZone::Local => &mut *daily,
            UsageZone::Utc => &mut *utc_daily,
        };
        target.entry(day_key.to_string()).or_default().add(*totals);
    }

    for (cache_key, per_day_models) in &entry.model_totals_by_day {
        let Some((zone, day_key)) = split_cache_day_key(cache_key) else {
            continue;
        };
        let (filter, target) = match zone {
            UsageZone::Local => (chart_day_filter, &mut *model_totals),
            UsageZone::Utc => (utc_chart_day_filter, &mut *utc_model_totals),
        };
        if !filter.contains(day_key) {
            continue;
        }
        for (model, tokens) in per_day_models {
            add_model_tokens_limited(target, model.clone(), *tokens);
        }
    }

    for project in entry_project_paths(entry) {
        apply_project_activity(&project, &entry.daily, daily, project_activity);
    }
}

fn apply_project_activity(
    cwd: &str,
    entry_daily: &HashMap<String, DailyTotals>,
    day_filter: &HashMap<String, DailyTotals>,
    project_activity: &mut HashMap<String, ProjectActivityBuilder>,
) {
    if cwd.trim().is_empty() {
        return;
    }
    let key = normalize_project_key(cwd);
    if key.is_empty() {
        return;
    }

    let builder = project_activity.entry(key).or_default();
    if builder.display_path.is_empty()
        || cwd.len() < builder.display_path.len()
        || (cwd.len() == builder.display_path.len() && cwd < builder.display_path.as_str())
    {
        builder.display_path = cwd.to_string();
    }

    for (cache_key, totals) in entry_daily {
        let Some((UsageZone::Local, day_key)) = split_cache_day_key(cache_key) else {
            continue;
        };
        if !day_filter.contains_key(day_key) {
            continue;
        }
        builder
            .daily
            .entry(day_key.to_string())
            .or_default()
            .add(*totals);
    }
}

/// Parses (or resumes parsing) one Claude Code session transcript.
///
/// Token usage comes from `assistant` records (`message.usage`), deduplicated
/// by `message.id` + `requestId`. A run is a distinct `promptId` on `user`
/// records. Agent time is gap-estimated from record timestamps and corrected
/// to the exact `system/turn_duration` value when the turn records one.
fn parse_file_summary(
    path: &Path,
    max_jsonl_line_bytes: usize,
    existing: Option<&CachedFileScanEntry>,
    deadline: Option<Instant>,
) -> Result<FileScanSummary> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(FileScanSummary::default()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return Ok(FileScanSummary::default());
    }
    if meta.len() == 0 {
        return Ok(FileScanSummary::default());
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(FileScanSummary::default()),
    };
    let file_len = meta.len();
    let current_modified_epoch = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    // The owner (launch cwd) is resolved before parsing so every record in the
    // file is attributed to the same project.
    let session_cwd = resolve_session_owner(path)
        .ok()
        .flatten()
        .map(|owner| owner.cwd);
    let cached_owner = existing.and_then(|entry| entry.session_cwd.as_deref());
    let owner_matches_cache = cached_owner == session_cwd.as_deref();

    let can_resume = existing
        .filter(|_| owner_matches_cache)
        .filter(|entry| entry.file_offset > 0 && entry.file_offset <= file_len)
        .filter(|entry| {
            if entry.size < file_len {
                return true;
            }
            entry.size == file_len
                && !entry.fully_parsed
                && entry.modified_epoch_secs == current_modified_epoch
        })
        .is_some();
    let resumed = existing.filter(|_| can_resume);
    let mut file_offset: u64 = resumed.map(|entry| entry.file_offset).unwrap_or(0);
    if file_offset > 0 {
        file.seek(SeekFrom::Start(file_offset))
            .with_context(|| format!("Unable to seek {}", path.display()))?;
    }
    let mut daily: HashMap<String, DailyTotals> =
        resumed.map(|entry| entry.daily.clone()).unwrap_or_default();
    let mut model_totals_by_day: HashMap<String, HashMap<String, i64>> = resumed
        .map(|entry| entry.model_totals_by_day.clone())
        .unwrap_or_default();
    let mut state = resumed
        .map(|entry| entry.parser_state.clone())
        .unwrap_or_default();

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut fully_parsed = true;

    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                fully_parsed = false;
                break;
            }
        }

        line.clear();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(bytes_read) => bytes_read,
            Err(_) => break,
        };
        if bytes_read == 0 {
            break;
        }
        if !line.ends_with('\n') {
            // Claude Code is still writing this record. Leave it for the next
            // refresh instead of consuming a truncated line.
            fully_parsed = false;
            break;
        }
        file_offset = file_offset.saturating_add(bytes_read as u64);
        if line.len() > max_jsonl_line_bytes {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(timestamp_ms) = read_timestamp_ms(&value) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "assistant" => {
                track_turn_activity(&mut daily, &mut state, timestamp_ms);
                let Some((key, model, usage)) = extract_assistant_usage(&value) else {
                    continue;
                };
                if state.last_usage_key.as_deref() == Some(key.as_str()) {
                    continue;
                }
                state.last_usage_key = Some(key);
                add_token_usage(
                    &mut daily,
                    &mut model_totals_by_day,
                    timestamp_ms,
                    &model,
                    usage,
                );
            }
            "user" => {
                if let Some(prompt_id) = value.get("promptId").and_then(Value::as_str) {
                    if state.last_prompt_id.as_deref() != Some(prompt_id) {
                        state.last_prompt_id = Some(prompt_id.to_string());
                        state.turn_estimate_ms = 0;
                        // A new turn does not bridge the idle gap before it.
                        state.last_activity_ms = None;
                        add_agent_run(&mut daily, timestamp_ms);
                    }
                }
                track_turn_activity(&mut daily, &mut state, timestamp_ms);
            }
            "system" => {
                if value.get("subtype").and_then(Value::as_str) == Some("turn_duration") {
                    if let Some(duration_ms) = value.get("durationMs").and_then(Value::as_i64) {
                        let correction = duration_ms.max(0) - state.turn_estimate_ms;
                        add_agent_ms(&mut daily, timestamp_ms, correction);
                        state.turn_estimate_ms = duration_ms.max(0);
                    }
                    // Records after the turn end (hooks, summaries) are idle time.
                    state.last_activity_ms = None;
                } else {
                    track_turn_activity(&mut daily, &mut state, timestamp_ms);
                }
            }
            _ => {}
        }
    }

    Ok(FileScanSummary {
        session_cwd,
        parser_state: state,
        file_offset: file_offset.min(file_len),
        fully_parsed: fully_parsed && file_offset >= file_len,
        daily,
        model_totals_by_day,
    })
}

/// Returns the dedupe key, model, and usage of an `assistant` record.
fn extract_assistant_usage(value: &Value) -> Option<(String, String, TokenUsage)> {
    let message = value.get("message")?.as_object()?;
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if model == "<synthetic>" {
        return None;
    }
    let usage = message.get("usage")?.as_object()?;
    let read = |key: &str| -> i64 {
        usage
            .get(key)
            .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|v| v as i64)))
            .unwrap_or(0)
            .max(0)
    };
    let tokens = TokenUsage {
        input: read("input_tokens"),
        cache_write: read("cache_creation_input_tokens"),
        cache_read: read("cache_read_input_tokens"),
        output: read("output_tokens"),
    };
    if tokens.total() == 0 {
        return None;
    }
    let message_id = message.get("id").and_then(Value::as_str).unwrap_or("");
    let request_id = value.get("requestId").and_then(Value::as_str).unwrap_or("");
    let key = if message_id.is_empty() && request_id.is_empty() {
        // No identity: fall back to the record uuid so the line counts once.
        value
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        format!("{message_id}:{request_id}")
    };
    Some((key, model.to_string(), tokens))
}

fn add_token_usage(
    daily: &mut HashMap<String, DailyTotals>,
    model_totals_by_day: &mut HashMap<String, HashMap<String, i64>>,
    timestamp_ms: i64,
    model: &str,
    usage: TokenUsage,
) {
    for zone in [UsageZone::Local, UsageZone::Utc] {
        let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) else {
            continue;
        };
        daily.entry(day_key.clone()).or_default().add(DailyTotals {
            input: usage.input,
            cache_write: usage.cache_write,
            cache_read: usage.cache_read,
            output: usage.output,
            ..DailyTotals::default()
        });
        add_model_tokens_limited(
            model_totals_by_day.entry(day_key).or_default(),
            model.to_string(),
            usage.total(),
        );
    }
}

fn track_turn_activity(
    daily: &mut HashMap<String, DailyTotals>,
    state: &mut ParserState,
    timestamp_ms: i64,
) {
    if let Some(previous) = state.last_activity_ms {
        let delta = timestamp_ms - previous;
        if delta > 0 && delta <= MAX_ACTIVITY_GAP_MS {
            add_agent_ms(daily, timestamp_ms, delta);
            state.turn_estimate_ms = state.turn_estimate_ms.saturating_add(delta);
        }
    }
    state.last_activity_ms = Some(timestamp_ms);
}

fn add_agent_ms(daily: &mut HashMap<String, DailyTotals>, timestamp_ms: i64, delta_ms: i64) {
    if delta_ms == 0 {
        return;
    }
    for zone in [UsageZone::Local, UsageZone::Utc] {
        if let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) {
            let totals = daily.entry(day_key).or_default();
            totals.agent_ms = totals.agent_ms.saturating_add(delta_ms).max(0);
        }
    }
}

fn open_or_init_scan_cache_db(path: &Path) -> Result<ScanCacheDb> {
    if let Some(parent) = path.parent() {
        crate::storage::ensure_private_dir(parent)?;
        ensure_directory_not_symlink(parent, "cache database parent directory")?;
    }
    ensure_regular_file_or_missing(path, "cache database file")?;
    let mut wal_path: OsString = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let mut shm_path: OsString = path.as_os_str().to_os_string();
    shm_path.push("-shm");
    ensure_regular_file_or_missing(Path::new(&wal_path), "cache database WAL file")?;
    ensure_regular_file_or_missing(Path::new(&shm_path), "cache database SHM file")?;

    let conn = Connection::open(path)
        .with_context(|| format!("Unable to open cache database {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .with_context(|| format!("Unable to set WAL journal mode for {}", path.display()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .with_context(|| format!("Unable to set synchronous mode for {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .with_context(|| format!("Unable to enable foreign keys for {}", path.display()))?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS cache_meta (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS file_cache (
            file_path TEXT PRIMARY KEY,
            file_size INTEGER NOT NULL,
            file_mtime INTEGER,
            file_offset INTEGER NOT NULL DEFAULT 0,
            fully_parsed INTEGER NOT NULL DEFAULT 1,
            session_cwd TEXT,
            parser_state_json TEXT NOT NULL DEFAULT '{}',
            daily_json TEXT NOT NULL,
            model_daily_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_file_cache_updated_at
            ON file_cache(updated_at);
        ",
    )
    .with_context(|| format!("Unable to initialize cache database {}", path.display()))?;
    conn.execute(
        "
        INSERT INTO cache_meta(key, value)
        SELECT 'schema_version', ?1
        WHERE NOT EXISTS (
            SELECT 1 FROM cache_meta WHERE key = 'schema_version'
        );
        ",
        params![SCAN_CACHE_DB_SCHEMA_VERSION],
    )
    .with_context(|| {
        format!(
            "Unable to write schema version metadata for {}",
            path.display()
        )
    })?;

    let schema_version: Option<i64> = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'schema_version';",
            [],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| {
            format!(
                "Unable to read schema version metadata for {}",
                path.display()
            )
        })?;
    let Some(schema_version) = schema_version else {
        anyhow::bail!("Missing scan cache schema version metadata");
    };
    if schema_version != SCAN_CACHE_DB_SCHEMA_VERSION {
        // Cache rows are derived data: any schema change rebuilds them from
        // the raw session logs instead of migrating.
        conn.execute("DELETE FROM file_cache;", [])
            .with_context(|| format!("Unable to rebuild scan cache in {}", path.display()))?;
        conn.execute(
            "UPDATE cache_meta SET value = ?1 WHERE key = 'schema_version';",
            params![SCAN_CACHE_DB_SCHEMA_VERSION],
        )
        .with_context(|| {
            format!(
                "Unable to update schema version metadata for {}",
                path.display()
            )
        })?;
    }
    let effective_schema_version: Option<i64> = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'schema_version';",
            [],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| {
            format!(
                "Unable to re-read schema version metadata for {}",
                path.display()
            )
        })?;
    let Some(effective_schema_version) = effective_schema_version else {
        anyhow::bail!("Missing scan cache schema version metadata after migration");
    };
    if effective_schema_version != SCAN_CACHE_DB_SCHEMA_VERSION {
        anyhow::bail!(
            "Unsupported scan cache schema version: {} (expected {})",
            effective_schema_version,
            SCAN_CACHE_DB_SCHEMA_VERSION
        );
    }

    let db = ScanCacheDb {
        path: path.to_path_buf(),
        conn,
    };
    enforce_scan_cache_db_permissions(&db.path)?;
    Ok(db)
}

fn load_scan_cache_store(db: &ScanCacheDb) -> Result<(ScanCacheStore, HashSet<String>)> {
    let mut store = ScanCacheStore::default();
    let mut invalid_paths: HashSet<String> = HashSet::new();
    let mut stmt = db
        .conn
        .prepare(
            "
            SELECT
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                session_cwd,
                parser_state_json,
                daily_json,
                model_daily_json,
                updated_at
            FROM file_cache;
            ",
        )
        .with_context(|| format!("Unable to query cache entries from {}", db.path.display()))?;
    let mut rows = stmt
        .query([])
        .with_context(|| format!("Unable to iterate cache entries from {}", db.path.display()))?;
    while let Some(row) = rows
        .next()
        .with_context(|| format!("Unable to read cache row from {}", db.path.display()))?
    {
        let file_path: String = row.get(0).with_context(|| {
            format!(
                "Unable to read file_path from cache row in {}",
                db.path.display()
            )
        })?;
        let file_size_raw: i64 = row.get(1).with_context(|| {
            format!(
                "Unable to read file_size from cache row in {}",
                db.path.display()
            )
        })?;
        let file_mtime_raw: Option<i64> = row.get(2).with_context(|| {
            format!(
                "Unable to read file_mtime from cache row in {}",
                db.path.display()
            )
        })?;
        let file_offset_raw: i64 = row.get(3).with_context(|| {
            format!(
                "Unable to read file_offset from cache row in {}",
                db.path.display()
            )
        })?;
        let fully_parsed_raw: i64 = row.get(4).with_context(|| {
            format!(
                "Unable to read fully_parsed from cache row in {}",
                db.path.display()
            )
        })?;
        let session_cwd: Option<String> = row.get(5).with_context(|| {
            format!(
                "Unable to read session_cwd from cache row in {}",
                db.path.display()
            )
        })?;
        let parser_state_json: String = row.get(6).with_context(|| {
            format!(
                "Unable to read parser_state_json from cache row in {}",
                db.path.display()
            )
        })?;
        let daily_json: String = row.get(7).with_context(|| {
            format!(
                "Unable to read daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let model_daily_json: String = row.get(8).with_context(|| {
            format!(
                "Unable to read model_daily_json from cache row in {}",
                db.path.display()
            )
        })?;
        let updated_at: i64 = row.get(9).with_context(|| {
            format!(
                "Unable to read updated_at from cache row in {}",
                db.path.display()
            )
        })?;

        let Ok(file_size) = u64::try_from(file_size_raw.max(0)) else {
            invalid_paths.insert(file_path);
            continue;
        };
        let file_mtime = file_mtime_raw.and_then(|value| u64::try_from(value).ok());
        let file_offset = match u64::try_from(file_offset_raw.max(0)) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let parser_state = match serde_json::from_str::<ParserState>(&parser_state_json) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let daily = match serde_json::from_str::<HashMap<String, DailyTotals>>(&daily_json) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };
        let model_totals_by_day = match serde_json::from_str::<HashMap<String, HashMap<String, i64>>>(
            &model_daily_json,
        ) {
            Ok(value) => value,
            Err(_) => {
                invalid_paths.insert(file_path);
                continue;
            }
        };

        store.entries.insert(
            file_path,
            CachedFileScanEntry {
                size: file_size,
                modified_epoch_secs: file_mtime,
                file_offset,
                fully_parsed: fully_parsed_raw != 0,
                session_cwd,
                parser_state,
                daily,
                model_totals_by_day,
                updated_at,
            },
        );
    }

    Ok((store, invalid_paths))
}

#[cfg(test)]
fn prune_scan_cache_store(
    store: &mut ScanCacheStore,
    sessions_root: &Path,
    max_session_file_bytes: u64,
    max_entries: usize,
    removed_paths: &mut HashSet<String>,
) -> bool {
    let mut pruned = false;

    store.entries.retain(|entry_path, _| {
        let path = Path::new(entry_path);
        let keep = is_valid_cached_session_file(path, sessions_root, max_session_file_bytes);
        if !keep {
            removed_paths.insert(entry_path.clone());
            pruned = true;
        }
        keep
    });

    trim_scan_cache_to_limit(store, max_entries.max(1), removed_paths) || pruned
}

#[cfg(test)]
fn trim_scan_cache_to_limit(
    store: &mut ScanCacheStore,
    max_entries: usize,
    removed_paths: &mut HashSet<String>,
) -> bool {
    if store.entries.len() <= max_entries {
        return false;
    }
    let mut entries_by_age: Vec<(String, i64)> = store
        .entries
        .iter()
        .map(|(key, entry)| (key.clone(), entry.updated_at))
        .collect();
    entries_by_age
        .sort_unstable_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

    let remove_count = entries_by_age.len().saturating_sub(max_entries);
    for (key, _) in entries_by_age.into_iter().take(remove_count) {
        store.entries.remove(&key);
        removed_paths.insert(key);
    }
    true
}

fn persist_scan_cache_changes(
    db: &mut ScanCacheDb,
    store: &ScanCacheStore,
    removed_paths: &HashSet<String>,
    dirty_paths: &HashSet<String>,
) -> Result<()> {
    if removed_paths.is_empty() && dirty_paths.is_empty() {
        return Ok(());
    }

    let tx = db
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .with_context(|| {
            format!(
                "Unable to start cache transaction for {}",
                db.path.display()
            )
        })?;

    let mut delete_stmt = tx
        .prepare("DELETE FROM file_cache WHERE file_path = ?1;")
        .with_context(|| {
            format!(
                "Unable to prepare delete statement for {}",
                db.path.display()
            )
        })?;
    let mut upsert_stmt = tx
        .prepare(
            "
            INSERT INTO file_cache(
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                session_cwd,
                parser_state_json,
                daily_json,
                model_daily_json,
                updated_at
            )
            VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(file_path) DO UPDATE SET
                file_size=excluded.file_size,
                file_mtime=excluded.file_mtime,
                file_offset=excluded.file_offset,
                fully_parsed=excluded.fully_parsed,
                session_cwd=excluded.session_cwd,
                parser_state_json=excluded.parser_state_json,
                daily_json=excluded.daily_json,
                model_daily_json=excluded.model_daily_json,
                updated_at=excluded.updated_at;
            ",
        )
        .with_context(|| {
            format!(
                "Unable to prepare upsert statement for {}",
                db.path.display()
            )
        })?;

    let mut removed_sorted: Vec<&String> = removed_paths.iter().collect();
    removed_sorted.sort();
    for file_path in removed_sorted {
        delete_stmt
            .execute(params![file_path])
            .with_context(|| format!("Unable to delete cache entry in {}", db.path.display()))?;
    }

    let mut dirty_sorted: Vec<&String> = dirty_paths.iter().collect();
    dirty_sorted.sort();
    for file_path in dirty_sorted {
        let Some(entry) = store.entries.get(file_path) else {
            continue;
        };
        let daily_json = serde_json::to_string(&entry.daily)
            .with_context(|| format!("Unable to serialize daily cache JSON for {}", file_path))?;
        let model_daily_json =
            serde_json::to_string(&entry.model_totals_by_day).with_context(|| {
                format!(
                    "Unable to serialize model-daily cache JSON for {}",
                    file_path
                )
            })?;
        let parser_state_json = serde_json::to_string(&entry.parser_state).with_context(|| {
            format!(
                "Unable to serialize parser-state cache JSON for {}",
                file_path
            )
        })?;
        let file_size = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let file_mtime = entry
            .modified_epoch_secs
            .and_then(|value| i64::try_from(value).ok());
        let file_offset = i64::try_from(entry.file_offset).unwrap_or(i64::MAX);
        let fully_parsed = if entry.fully_parsed { 1_i64 } else { 0_i64 };

        upsert_stmt
            .execute(params![
                file_path,
                file_size,
                file_mtime,
                file_offset,
                fully_parsed,
                entry.session_cwd.as_deref(),
                parser_state_json,
                daily_json,
                model_daily_json,
                entry.updated_at
            ])
            .with_context(|| format!("Unable to upsert cache entry in {}", db.path.display()))?;
    }

    drop(delete_stmt);
    drop(upsert_stmt);
    tx.commit().with_context(|| {
        format!(
            "Unable to commit cache transaction for {}",
            db.path.display()
        )
    })?;
    enforce_scan_cache_db_permissions(&db.path)?;
    Ok(())
}

fn trim_scan_cache_db_entries_to_limit(db: &ScanCacheDb, max_entries: usize) -> Result<bool> {
    let max_entries = max_entries.max(1);
    let max_entries_i64 = i64::try_from(max_entries).unwrap_or(i64::MAX);
    let total_rows: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM file_cache;", [], |row| row.get(0))
        .with_context(|| format!("Unable to count cache rows in {}", db.path.display()))?;
    if total_rows <= max_entries_i64 {
        return Ok(false);
    }

    let remove_count = total_rows - max_entries_i64;
    db.conn
        .execute(
            "
            DELETE FROM file_cache
            WHERE file_path IN (
                SELECT file_path
                FROM file_cache
                ORDER BY updated_at ASC, file_path ASC
                LIMIT ?1
            );
            ",
            params![remove_count],
        )
        .with_context(|| format!("Unable to trim cache rows in {}", db.path.display()))?;
    enforce_scan_cache_db_permissions(&db.path)?;
    Ok(true)
}

#[cfg(test)]
fn is_valid_cached_session_file(
    path: &Path,
    sessions_root: &Path,
    max_session_file_bytes: u64,
) -> bool {
    if !path.starts_with(sessions_root) {
        return false;
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() {
        return false;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return false;
    }
    let len = meta.len();
    len > 0 && len <= max_session_file_bytes
}

fn enforce_scan_cache_db_permissions(path: &Path) -> Result<()> {
    ensure_regular_file_or_missing(path, "cache database file")?;
    crate::storage::enforce_private_file_if_exists(path)?;
    let mut wal_path: OsString = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let mut shm_path: OsString = path.as_os_str().to_os_string();
    shm_path.push("-shm");
    ensure_regular_file_or_missing(Path::new(&wal_path), "cache database WAL file")?;
    ensure_regular_file_or_missing(Path::new(&shm_path), "cache database SHM file")?;
    let _ = crate::storage::enforce_private_file_if_exists(Path::new(&wal_path));
    let _ = crate::storage::enforce_private_file_if_exists(Path::new(&shm_path));
    Ok(())
}

fn ensure_directory_not_symlink(path: &Path, label: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("Unable to inspect {} {}", label, path.display()))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        anyhow::bail!(
            "Refusing to use {} {}: symlink is not allowed",
            label,
            path.display()
        );
    }
    if !ft.is_dir() {
        anyhow::bail!(
            "Refusing to use {} {}: expected directory",
            label,
            path.display()
        );
    }
    Ok(())
}

fn ensure_regular_file_or_missing(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                anyhow::bail!(
                    "Refusing to use {} {}: symlink is not allowed",
                    label,
                    path.display()
                );
            }
            if !ft.is_file() {
                anyhow::bail!(
                    "Refusing to use {} {}: expected regular file",
                    label,
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Unable to inspect {} {}", label, path.display()))
        }
    }
}

fn read_timestamp_ms(value: &Value) -> Option<i64> {
    parse_timestamp_value_ms(value.get("timestamp")?)
}

fn parse_timestamp_value_ms(raw: &Value) -> Option<i64> {
    if let Some(text) = raw.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .map(|value| value.timestamp_millis())
            .ok();
    }
    let numeric = raw
        .as_i64()
        .or_else(|| raw.as_f64().map(|value| value as i64))?;
    if numeric > 0 && numeric < 1_000_000_000_000 {
        return Some(numeric * 1000);
    }
    Some(numeric)
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn add_agent_run(daily: &mut HashMap<String, DailyTotals>, timestamp_ms: i64) {
    for zone in [UsageZone::Local, UsageZone::Utc] {
        if let Some(day_key) = cache_day_key_for_timestamp_ms(timestamp_ms, zone) {
            daily.entry(day_key).or_default().agent_runs += 1;
        }
    }
}

fn display_day_key_for_timestamp_ms(timestamp_ms: i64, zone: UsageZone) -> Option<String> {
    let utc = Utc.timestamp_millis_opt(timestamp_ms).single()?;
    Some(match zone {
        UsageZone::Local => utc.with_timezone(&Local).format("%Y-%m-%d").to_string(),
        UsageZone::Utc => utc.format("%Y-%m-%d").to_string(),
    })
}

fn cache_day_key_for_timestamp_ms(timestamp_ms: i64, zone: UsageZone) -> Option<String> {
    let day = display_day_key_for_timestamp_ms(timestamp_ms, zone)?;
    Some(match zone {
        UsageZone::Local => format!("L:{day}"),
        UsageZone::Utc => format!("U:{day}"),
    })
}

fn split_cache_day_key(value: &str) -> Option<(UsageZone, &str)> {
    if let Some(day) = value.strip_prefix("L:") {
        Some((UsageZone::Local, day))
    } else {
        value.strip_prefix("U:").map(|day| (UsageZone::Utc, day))
    }
}

/// Resolves the owner of a Claude Code session transcript: the first record
/// carrying a valid absolute `cwd` (the directory Claude Code was launched
/// in). Later records may report a different cwd after `cd` in a tool call;
/// those never change the owner.
pub(crate) fn resolve_session_owner(path: &Path) -> Result<Option<SessionOwner>> {
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let file_session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| is_uuid_like(stem))
        .map(str::to_string);

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            return Ok(None);
        }
        if line.len() > MAX_OWNER_IDENTITY_LINE_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(cwd) = value
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(session_cwd_identity)
        else {
            continue;
        };
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(file_session_id);
        return Ok(Some(SessionOwner { cwd, session_id }));
    }
}

fn is_uuid_like(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn path_matches_workspace(cwd: &str, workspace_path: &Path) -> bool {
    let Some(cwd) = session_cwd_identity(cwd) else {
        return false;
    };
    let Some(workspace) = workspace_path.to_str().and_then(session_cwd_identity) else {
        return false;
    };
    let cwd_path = Path::new(&cwd);
    let workspace_path = Path::new(&workspace);
    cwd_path == workspace_path || cwd_path.starts_with(workspace_path)
}

pub(crate) fn normalize_project_key(path: &str) -> String {
    let mut normalized = session_cwd_identity(path).unwrap_or_else(|| {
        normalize_wsl_unc_path(path).unwrap_or_else(|| path.trim().replace('\\', "/"))
    });
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    #[cfg(windows)]
    {
        normalized.make_ascii_lowercase();
    }
    normalized
}

/// Project identity: the session working directory only.
///
/// Does not walk the filesystem for `.git`. Optional git branch/remote from
/// session logs remain display metadata elsewhere and never redefine this key.
pub(crate) fn session_cwd_identity(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 4096 || raw.chars().any(char::is_control) {
        return None;
    }
    let normalized = normalize_cross_platform_path(raw)?;
    if !normalized.is_absolute() {
        return None;
    }
    let mut display = normalized.display().to_string();
    while display.len() > 1 && (display.ends_with('/') || display.ends_with('\\')) {
        display.pop();
    }
    if display.trim().is_empty() {
        return None;
    }
    Some(display)
}

fn normalize_cross_platform_path(path: &str) -> Option<PathBuf> {
    let normalized = normalize_wsl_unc_path(path).unwrap_or_else(|| path.trim().replace('\\', "/"));
    if normalized.is_empty() {
        return None;
    }

    #[cfg(unix)]
    {
        let bytes = normalized.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'/'
        {
            let drive = char::from(bytes[0]).to_ascii_lowercase();
            return Some(PathBuf::from(format!("/mnt/{drive}/{}", &normalized[3..])));
        }
    }

    Some(PathBuf::from(normalized))
}

fn normalize_wsl_unc_path(path: &str) -> Option<String> {
    let normalized = path.trim().replace('\\', "/");
    let lower = normalized.to_lowercase();
    let prefix_len = if lower.starts_with("//wsl.localhost/") {
        "//wsl.localhost/".len()
    } else if lower.starts_with("//wsl$/") {
        "//wsl$/".len()
    } else {
        return None;
    };
    let rest = normalized.get(prefix_len..)?;
    let (_, linux_path) = rest.split_once('/')?;
    Some(format!("/{linux_path}"))
}

fn make_day_keys_for_zone(days: u32, zone: UsageZone) -> Vec<String> {
    let today = match zone {
        UsageZone::Local => Local::now().date_naive(),
        UsageZone::Utc => Utc::now().date_naive(),
    };
    (0..days)
        .rev()
        .map(|offset| {
            let day = today - Duration::days(offset as i64);
            day.format("%Y-%m-%d").to_string()
        })
        .collect()
}

fn make_complete_chart_day_keys(
    daily: &HashMap<String, DailyTotals>,
    zone: UsageZone,
    fallback: &[String],
) -> Vec<String> {
    let mut active_dates = daily.iter().filter_map(|(day, totals)| {
        daily_has_activity(*totals)
            .then(|| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
            .flatten()
    });
    let Some(mut first) = active_dates.next() else {
        return fallback.to_vec();
    };
    let mut last = first;
    for date in active_dates {
        first = first.min(date);
        last = last.max(date);
    }

    let today = match zone {
        UsageZone::Local => Local::now().date_naive(),
        UsageZone::Utc => Utc::now().date_naive(),
    };
    last = last.max(today);

    let mut keys = Vec::with_capacity(
        last.signed_duration_since(first)
            .num_days()
            .max(0)
            .saturating_add(1) as usize,
    );
    let mut date = first;
    while date <= last {
        keys.push(date.format("%Y-%m-%d").to_string());
        let Some(next) = date.checked_add_signed(Duration::days(1)) else {
            break;
        };
        date = next;
    }
    keys
}

pub fn system_first_weekday() -> Weekday {
    locale_region_from_env()
        .as_deref()
        .map(first_weekday_for_region)
        .unwrap_or(Weekday::Mon)
}

fn locale_region_from_env() -> Option<String> {
    for key in ["LC_TIME", "LC_ALL", "LANG"] {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        return locale_region(&value);
    }
    None
}

fn locale_region(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("c")
        || trimmed.eq_ignore_ascii_case("posix")
    {
        return None;
    }
    let base = trimmed
        .split(['.', '@'])
        .next()
        .unwrap_or(trimmed)
        .replace('-', "_");
    let mut parts = base.split('_');
    let _language = parts.next()?;
    let region = parts.next()?.trim();
    if region.len() != 2 || !region.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return None;
    }
    Some(region.to_ascii_uppercase())
}

fn first_weekday_for_region(region: &str) -> Weekday {
    match region.to_ascii_uppercase().as_str() {
        "AE" | "AF" | "BH" | "DJ" | "DZ" | "EG" | "IQ" | "IR" | "JO" | "KW" | "LY" | "OM"
        | "QA" | "SD" | "SY" | "YE" => Weekday::Sat,
        "AG" | "AR" | "AS" | "AU" | "BD" | "BR" | "BS" | "BT" | "BW" | "BZ" | "CA" | "CN"
        | "CO" | "DM" | "DO" | "ET" | "GT" | "GU" | "HK" | "HN" | "ID" | "IL" | "IN" | "JM"
        | "JP" | "KE" | "KH" | "KR" | "LA" | "MH" | "MM" | "MO" | "MT" | "MX" | "MZ" | "NI"
        | "NP" | "PA" | "PE" | "PH" | "PK" | "PR" | "PT" | "PY" | "SA" | "SG" | "SV" | "TH"
        | "TT" | "TW" | "UM" | "US" | "VE" | "VI" | "WS" | "ZA" | "ZW" => Weekday::Sun,
        _ => Weekday::Mon,
    }
}

fn make_activity_day_keys(first_weekday: Weekday) -> Vec<String> {
    let today = Local::now().date_naive();
    let days_from_week_start = days_since_week_start(today.weekday(), first_weekday);
    let current_week_start = today - Duration::days(days_from_week_start);
    let first_day = current_week_start - Duration::weeks((ACTIVITY_TIMELINE_WEEKS - 1) as i64);
    (0..ACTIVITY_TIMELINE_DAYS)
        .map(|offset| {
            let day = first_day + Duration::days(offset as i64);
            day.format("%Y-%m-%d").to_string()
        })
        .collect()
}

fn days_since_week_start(day: Weekday, first_weekday: Weekday) -> i64 {
    let day = day.num_days_from_monday() as i64;
    let first = first_weekday.num_days_from_monday() as i64;
    (7 + day - first) % 7
}

/// Human-readable model name: `claude-opus-5-5` -> `Opus 5.5`,
/// `claude-haiku-4-5-20251001` -> `Haiku 4.5`. Unknown ids pass through.
pub fn model_display_name(model: &str) -> String {
    let Some(rest) = model.strip_prefix("claude-") else {
        return model.to_string();
    };
    let mut parts = rest.split('-');
    let Some(family) = parts.next().filter(|family| !family.is_empty()) else {
        return model.to_string();
    };
    // Version parts are short numbers; an 8-digit date suffix is dropped.
    let version: Vec<&str> = parts
        .take_while(|part| part.len() <= 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    let mut name = family[..1].to_ascii_uppercase() + &family[1..];
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

pub fn format_count(value: i64, formatter: DisplayFormatter<'_>) -> String {
    formatter.format_count(value)
}

pub fn format_tokens_overview(value: i64, formatter: DisplayFormatter<'_>) -> String {
    match formatter.style() {
        DisplayStyle::Classic | DisplayStyle::SystemFull => format_count(value, formatter),
        DisplayStyle::SystemCompact => format_tokens_compact(value, formatter),
    }
}

pub fn format_tokens_compact(value: i64, formatter: DisplayFormatter<'_>) -> String {
    let v = value.max(0) as u64;
    if v < 1000 {
        return formatter.format_u64(v);
    }

    let (div, suffix) = if v >= 1_000_000_000_000 {
        (1_000_000_000_000f64, "T")
    } else if v >= 1_000_000_000 {
        (1_000_000_000f64, "B")
    } else if v >= 1_000_000 {
        (1_000_000f64, "M")
    } else {
        (1_000f64, "K")
    };
    let scaled = (v as f64) / div;
    format_compact_scaled(scaled, suffix, 2, formatter)
}

pub fn format_duration_compact(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

pub fn format_duration(ms: i64) -> String {
    let mut secs = ms.max(0) / 1000;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    secs %= 60;
    if hours > 0 {
        format!("{hours}h {mins}m {secs}s")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
    const TEST_PROJECT_CWD: &str = "/tmp/claudemon-test-project";

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("claudemon-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Creates `<root>/claude/projects/<slug>` and returns (claude_dir, project dir).
    fn make_claude_dir(root: &Path) -> (PathBuf, PathBuf) {
        let claude_dir = root.join("claude");
        let sessions_root = claude_dir
            .join("projects")
            .join("-tmp-claudemon-test-project");
        std::fs::create_dir_all(&sessions_root).expect("create projects dir");
        (claude_dir, sessions_root)
    }

    fn rfc3339(timestamp_ms: i64) -> String {
        Utc.timestamp_millis_opt(timestamp_ms)
            .single()
            .expect("valid timestamp")
            .to_rfc3339()
    }

    fn append_json_line(path: &Path, line: serde_json::Value) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("open jsonl file for append");
        use std::io::Write as _;
        writeln!(file, "{line}").expect("append json line");
    }

    fn assistant_line(
        timestamp_ms: i64,
        cwd: &str,
        message_id: &str,
        usage: [i64; 4],
    ) -> serde_json::Value {
        let [input, cache_write, cache_read, output] = usage;
        serde_json::json!({
            "type": "assistant",
            "timestamp": rfc3339(timestamp_ms),
            "cwd": cwd,
            "sessionId": "00000000-0000-4000-8000-000000000001",
            "requestId": format!("req-{message_id}"),
            "message": {
                "id": message_id,
                "model": "claude-test-1",
                "role": "assistant",
                "content": [],
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": cache_write,
                    "cache_read_input_tokens": cache_read,
                    "output_tokens": output
                }
            }
        })
    }

    /// Appends one API response with the given usage columns
    /// (input, cache write, cache read, output).
    fn append_usage_line(path: &Path, timestamp_ms: i64, cwd: &str, usage: [i64; 4]) {
        let message_id = format!("msg-{}", TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed));
        append_json_line(path, assistant_line(timestamp_ms, cwd, &message_id, usage));
    }

    fn write_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        let _ = std::fs::remove_file(path);
        append_token_file(path, timestamp_ms, input_tokens, output_tokens);
    }

    fn append_token_file(path: &Path, timestamp_ms: i64, input_tokens: i64, output_tokens: i64) {
        append_usage_line(
            path,
            timestamp_ms,
            TEST_PROJECT_CWD,
            [input_tokens, 0, 0, output_tokens],
        );
    }

    fn append_user_prompt(path: &Path, timestamp_ms: i64, prompt_id: &str) {
        append_json_line(
            path,
            serde_json::json!({
                "type": "user",
                "timestamp": rfc3339(timestamp_ms),
                "cwd": TEST_PROJECT_CWD,
                "promptId": prompt_id,
                "message": {"role": "user", "content": "synthetic prompt"}
            }),
        );
    }

    fn append_tool_result(path: &Path, timestamp_ms: i64, prompt_id: &str) {
        append_json_line(
            path,
            serde_json::json!({
                "type": "user",
                "timestamp": rfc3339(timestamp_ms),
                "cwd": TEST_PROJECT_CWD,
                "promptId": prompt_id,
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]
                }
            }),
        );
    }

    fn append_turn_duration(path: &Path, timestamp_ms: i64, duration_ms: i64) {
        append_json_line(
            path,
            serde_json::json!({
                "type": "system",
                "subtype": "turn_duration",
                "timestamp": rfc3339(timestamp_ms),
                "cwd": TEST_PROJECT_CWD,
                "durationMs": duration_ms
            }),
        );
    }

    fn default_test_limits(full_scan: bool) -> ScanLimits {
        ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan,
            scan_cache_max_entries: 1000,
        }
    }

    #[test]
    fn locale_region_parses_common_locale_values() {
        assert_eq!(locale_region("ja_JP.UTF-8").as_deref(), Some("JP"));
        assert_eq!(locale_region("en-US").as_deref(), Some("US"));
        assert_eq!(locale_region("C"), None);
    }

    #[test]
    fn first_weekday_uses_region_defaults() {
        assert_eq!(first_weekday_for_region("JP"), Weekday::Sun);
        assert_eq!(first_weekday_for_region("US"), Weekday::Sun);
        assert_eq!(first_weekday_for_region("GB"), Weekday::Mon);
    }

    #[test]
    fn activity_day_keys_start_on_requested_weekday() {
        let sunday_keys = make_activity_day_keys(Weekday::Sun);
        let monday_keys = make_activity_day_keys(Weekday::Mon);
        let sunday = chrono::NaiveDate::parse_from_str(&sunday_keys[0], "%Y-%m-%d")
            .expect("parse sunday-first activity key");
        let monday = chrono::NaiveDate::parse_from_str(&monday_keys[0], "%Y-%m-%d")
            .expect("parse monday-first activity key");
        assert_eq!(sunday.weekday(), Weekday::Sun);
        assert_eq!(monday.weekday(), Weekday::Mon);
    }

    #[test]
    fn cached_scanner_advances_through_backlog_across_refreshes() {
        let root = make_temp_dir("scan-backlog");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let now_ms = Utc::now().timestamp_millis();
        for index in 0..3 {
            write_token_file(
                &sessions_root.join(format!("session-{index}.jsonl")),
                now_ms + index,
                100 + index,
                20,
            );
        }
        let mut limits = default_test_limits(false);
        limits.max_session_files_scanned = 1;
        let cache_db_path = root.join("claudemon.db");

        let first = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("first snapshot");
        assert_eq!(first.scan_indexed_files, 1);
        assert_eq!(first.scan_pending_files, 2);
        assert!(first.scan_processed_bytes > 0);
        let second = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("second snapshot");
        assert_eq!(second.scan_indexed_files, 2);
        assert!(second.scan_processed_bytes > first.scan_processed_bytes);
        let third = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("third snapshot");
        assert_eq!(third.scan_indexed_files, 3);
        assert_eq!(third.scan_pending_files, 0);
        assert!(third.scan_processed_bytes > second.scan_processed_bytes);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_populates_local_and_utc_projections() {
        let root = make_temp_dir("usage-zones");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let now_ms = Utc::now().timestamp_millis();
        write_token_file(&sessions_root.join("session.jsonl"), now_ms, 100, 20);

        let snapshot = compute_snapshot(30, &claude_dir, None, default_test_limits(false), None)
            .expect("snapshot");
        assert_eq!(snapshot.totals.last30_days_tokens, 120);
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 120);
        assert_eq!(snapshot.days.len(), snapshot.utc_days.len());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_charts_all_indexed_days_but_keeps_thirty_day_summary() {
        let root = make_temp_dir("usage-full-chart-history");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let now_ms = Utc::now().timestamp_millis();
        let old_ms = now_ms - Duration::days(45).num_milliseconds();
        write_token_file(&sessions_root.join("old.jsonl"), old_ms, 1_000, 20);
        write_token_file(&sessions_root.join("recent.jsonl"), now_ms, 100, 20);

        let snapshot = compute_snapshot(30, &claude_dir, None, default_test_limits(false), None)
            .expect("snapshot");
        let old_day = Utc
            .timestamp_millis_opt(old_ms)
            .single()
            .expect("old timestamp")
            .format("%Y-%m-%d")
            .to_string();
        assert!(snapshot.utc_days.len() >= 46);
        assert_eq!(
            snapshot
                .utc_days
                .iter()
                .find(|day| day.day == old_day)
                .map(|day| day.total_tokens),
            Some(1_020)
        );
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 120);
        assert_eq!(snapshot.utc_totals.peak_day_tokens, 120);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_builds_project_activity_sorted_by_last_activity() {
        let root = make_temp_dir("project-activity");
        let (claude_dir, sessions_root) = make_claude_dir(&root);

        let now_ms = Utc::now().timestamp_millis();
        let older_ms = now_ms - Duration::days(5).num_milliseconds();
        let newer_ms = now_ms - Duration::days(1).num_milliseconds();

        let photonia = sessions_root.join("photonia.jsonl");
        append_usage_line(
            &photonia,
            older_ms + 100,
            "/outside/Photonia",
            [100, 0, 0, 20],
        );

        let sfm = sessions_root.join("sfm.jsonl");
        append_usage_line(&sfm, newer_ms + 100, "/outside/SFM", [40, 10, 150, 50]);

        let snapshot = compute_snapshot(30, &claude_dir, None, default_test_limits(false), None)
            .expect("snapshot");

        assert_eq!(snapshot.project_activity.len(), 2);
        assert_eq!(
            snapshot.project_activity[0].days.len(),
            ACTIVITY_TIMELINE_DAYS
        );
        assert_eq!(snapshot.project_activity[0].display_path, "/outside/SFM");
        assert_eq!(snapshot.project_activity[0].total_tokens, 250);
        assert_eq!(snapshot.project_activity[0].cache_read_tokens, 150);
        assert_eq!(
            snapshot.project_activity[1].display_path,
            "/outside/Photonia"
        );
        assert_eq!(snapshot.project_activity[1].total_tokens, 120);
        assert_eq!(snapshot.project_activity[1].cache_read_tokens, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_keeps_cached_totals_for_unplanned_unchanged_files() {
        let root = make_temp_dir("cache-unplanned");
        let (claude_dir, sessions_root) = make_claude_dir(&root);

        let now_ms = Utc::now().timestamp_millis();
        let older_ms = now_ms - Duration::days(20).num_milliseconds();
        let newer_ms = now_ms - Duration::days(5).num_milliseconds();

        let older_path = sessions_root.join("older.jsonl");
        let newer_path = sessions_root.join("newer.jsonl");
        write_token_file(&older_path, older_ms, 100, 25);
        write_token_file(&newer_path, newer_ms, 80, 20);

        let cache_db_path = root.join("claudemon.db");
        let warm_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: true,
            scan_cache_max_entries: 1000,
        };
        let warmed = compute_snapshot(
            30,
            &claude_dir,
            None,
            warm_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("warm snapshot");
        assert_eq!(warmed.totals.last30_days_tokens, 225);

        let restrictive_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 16 * 1024 * 1024,
            max_session_files_scanned: 1,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let restricted = compute_snapshot(
            30,
            &claude_dir,
            None,
            restrictive_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("restricted snapshot");
        assert_eq!(
            restricted.totals.last30_days_tokens, warmed.totals.last30_days_tokens,
            "unchanged files outside current scan plan should still contribute via cache"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_resumes_from_cached_offset_after_append() {
        let root = make_temp_dir("cache-append-resume");
        let (claude_dir, sessions_root) = make_claude_dir(&root);

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("session.jsonl");
        write_token_file(
            &session_path,
            now_ms - Duration::hours(2).num_milliseconds(),
            100,
            20,
        );

        let cache_db_path = root.join("claudemon.db");
        let limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 4 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };

        let first = compute_snapshot(30, &claude_dir, None, limits, Some(cache_db_path.as_path()))
            .expect("first snapshot");
        assert_eq!(first.totals.last30_days_tokens, 120);

        append_token_file(
            &session_path,
            now_ms - Duration::hours(1).num_milliseconds(),
            40,
            10,
        );

        let second = compute_snapshot(30, &claude_dir, None, limits, Some(cache_db_path.as_path()))
            .expect("second snapshot");
        assert_eq!(second.totals.last30_days_tokens, 170);

        let third = compute_snapshot(30, &claude_dir, None, limits, Some(cache_db_path.as_path()))
            .expect("third snapshot");
        assert_eq!(
            third.totals.last30_days_tokens, 170,
            "unchanged file should not double-count appended usage after resume"
        );

        let db = open_or_init_scan_cache_db(&cache_db_path).expect("open cache db");
        let (store, _) = load_scan_cache_store(&db).expect("load cache store");
        let key = session_path.to_string_lossy().to_string();
        let entry = store
            .entries
            .get(&key)
            .expect("cache row for appended session");
        assert!(entry.file_offset > 0);
        assert!(entry.fully_parsed);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_full_scan_ignores_file_and_byte_scan_caps() {
        let root = make_temp_dir("full-scan-caps");
        let (claude_dir, sessions_root) = make_claude_dir(&root);

        let now_ms = Utc::now().timestamp_millis();
        let files = [
            (
                "a.jsonl",
                now_ms - Duration::days(3).num_milliseconds(),
                90,
                10,
            ),
            (
                "b.jsonl",
                now_ms - Duration::days(2).num_milliseconds(),
                70,
                5,
            ),
            (
                "c.jsonl",
                now_ms - Duration::days(1).num_milliseconds(),
                40,
                15,
            ),
        ];
        let mut expected_total = 0_i64;
        for (name, ts, input, output) in files {
            write_token_file(&sessions_root.join(name), ts, input, output);
            expected_total += input + output;
        }

        let capped_limits = ScanLimits {
            max_session_file_bytes: 1,
            max_session_total_bytes: 1,
            max_session_files_scanned: 1,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let capped =
            compute_snapshot(30, &claude_dir, None, capped_limits, None).expect("capped snapshot");
        assert!(
            capped.totals.last30_days_tokens < expected_total,
            "planner caps should leave some files unscanned in non-full mode"
        );

        let uncapped_limits = ScanLimits {
            full_scan: true,
            ..capped_limits
        };
        let full =
            compute_snapshot(30, &claude_dir, None, uncapped_limits, None).expect("full snapshot");
        assert_eq!(
            full.totals.last30_days_tokens, expected_total,
            "full scan should include all files even when scan caps are tiny"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compute_snapshot_full_scan_reparses_when_cache_row_is_stale() {
        let root = make_temp_dir("full-scan-reparse");
        let (claude_dir, sessions_root) = make_claude_dir(&root);

        let now_ms = Utc::now().timestamp_millis();
        let session_path = sessions_root.join("session.jsonl");
        write_token_file(
            &session_path,
            now_ms - Duration::hours(6).num_milliseconds(),
            120,
            30,
        );
        let expected_total = 150_i64;

        let cache_db_path = root.join("claudemon.db");
        let baseline_limits = ScanLimits {
            max_session_file_bytes: 4 * 1024 * 1024,
            max_session_total_bytes: 4 * 1024 * 1024,
            max_session_files_scanned: 10,
            max_jsonl_line_bytes: 512 * 1024,
            scan_time_budget_ms: 0,
            full_scan: false,
            scan_cache_max_entries: 1000,
        };
        let baseline = compute_snapshot(
            30,
            &claude_dir,
            None,
            baseline_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("baseline snapshot");
        assert_eq!(baseline.totals.last30_days_tokens, expected_total);

        let db = open_or_init_scan_cache_db(&cache_db_path).expect("open cache db");
        let file_key = session_path.to_string_lossy().to_string();
        db.conn
            .execute(
                "UPDATE file_cache SET daily_json='{}', model_daily_json='{}' WHERE file_path = ?1;",
                rusqlite::params![file_key],
            )
            .expect("corrupt cache row");
        drop(db);

        let stale = compute_snapshot(
            30,
            &claude_dir,
            None,
            baseline_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("stale snapshot");
        assert_eq!(
            stale.totals.last30_days_tokens, 0,
            "non-full scan should still trust unchanged cache rows"
        );

        let full_limits = ScanLimits {
            full_scan: true,
            ..baseline_limits
        };
        let repaired = compute_snapshot(
            30,
            &claude_dir,
            None,
            full_limits,
            Some(cache_db_path.as_path()),
        )
        .expect("repaired snapshot");
        assert_eq!(
            repaired.totals.last30_days_tokens, expected_total,
            "full scan with unlimited time should reparse files and repair stale cache rows"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prune_scan_cache_db_removes_stale_entries() {
        let root = make_temp_dir("cache-prune");
        let sessions_root = root.join("sessions");
        std::fs::create_dir_all(&sessions_root).expect("create sessions root");
        let db_path = root.join("claudemon.db");
        let mut db = open_or_init_scan_cache_db(&db_path).expect("open cache db");

        let keep_path = sessions_root.join("keep.jsonl");
        std::fs::write(&keep_path, b"{}\n").expect("write keep jsonl");

        let wrong_ext_path = sessions_root.join("not_jsonl.txt");
        std::fs::write(&wrong_ext_path, b"ignored").expect("write txt");

        let outside_root = make_temp_dir("outside");
        let outside_path = outside_root.join("outside.jsonl");
        std::fs::write(&outside_path, b"{}\n").expect("write outside jsonl");

        let missing_path = sessions_root.join("missing.jsonl");

        let keep_key = keep_path.to_string_lossy().to_string();
        let wrong_ext_key = wrong_ext_path.to_string_lossy().to_string();
        let outside_key = outside_path.to_string_lossy().to_string();
        let missing_key = missing_path.to_string_lossy().to_string();

        let base = CachedFileScanEntry {
            size: 4,
            modified_epoch_secs: Some(1),
            file_offset: 4,
            fully_parsed: true,
            session_cwd: None,
            parser_state: ParserState::default(),
            daily: HashMap::new(),
            model_totals_by_day: HashMap::new(),
            updated_at: 1,
        };
        let mut initial_store = ScanCacheStore::default();
        initial_store.entries.insert(
            keep_key.clone(),
            CachedFileScanEntry {
                updated_at: 5,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            wrong_ext_key.clone(),
            CachedFileScanEntry {
                updated_at: 4,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            outside_key.clone(),
            CachedFileScanEntry {
                updated_at: 3,
                ..base.clone()
            },
        );
        initial_store.entries.insert(
            missing_key.clone(),
            CachedFileScanEntry {
                updated_at: 2,
                ..base
            },
        );
        let dirty_paths: HashSet<String> = initial_store.entries.keys().cloned().collect();
        persist_scan_cache_changes(&mut db, &initial_store, &HashSet::new(), &dirty_paths)
            .expect("persist initial rows");

        let (mut loaded_store, mut removed_paths) = load_scan_cache_store(&db).expect("load store");
        let pruned = prune_scan_cache_store(
            &mut loaded_store,
            &sessions_root,
            1024,
            100,
            &mut removed_paths,
        );
        assert!(pruned, "expected stale rows to be removed");
        persist_scan_cache_changes(&mut db, &loaded_store, &removed_paths, &HashSet::new())
            .expect("persist pruned rows");

        let (reloaded_store, _) = load_scan_cache_store(&db).expect("reload store");
        assert_eq!(reloaded_store.entries.len(), 1);
        assert!(reloaded_store.entries.contains_key(&keep_key));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside_root);
    }

    #[test]
    fn trim_scan_cache_db_to_limit_drops_oldest_entries() {
        let root = make_temp_dir("cache-trim");
        let db_path = root.join("claudemon.db");
        let mut db = open_or_init_scan_cache_db(&db_path).expect("open cache db");

        let mut store = ScanCacheStore::default();
        for (path, updated_at) in [("a", 30_i64), ("b", 10_i64), ("c", 20_i64)] {
            store.entries.insert(
                path.to_string(),
                CachedFileScanEntry {
                    size: 1,
                    modified_epoch_secs: Some(1),
                    file_offset: 1,
                    fully_parsed: true,
                    session_cwd: None,
                    parser_state: ParserState::default(),
                    daily: HashMap::new(),
                    model_totals_by_day: HashMap::new(),
                    updated_at,
                },
            );
        }
        let dirty_paths: HashSet<String> = store.entries.keys().cloned().collect();
        persist_scan_cache_changes(&mut db, &store, &HashSet::new(), &dirty_paths)
            .expect("persist initial rows");

        let (mut loaded_store, mut removed_paths) = load_scan_cache_store(&db).expect("load store");
        let pruned = trim_scan_cache_to_limit(&mut loaded_store, 2, &mut removed_paths);
        assert!(pruned, "expected trim to remove one row");
        persist_scan_cache_changes(&mut db, &loaded_store, &removed_paths, &HashSet::new())
            .expect("persist trimmed rows");

        let (reloaded_store, _) = load_scan_cache_store(&db).expect("reload store");
        let mut paths: Vec<String> = reloaded_store.entries.keys().cloned().collect();
        paths.sort();
        assert_eq!(paths, vec!["a".to_string(), "c".to_string()]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_keys_unify_wsl_unc_and_linux_paths() {
        assert_eq!(
            normalize_project_key(r"\\wsl.localhost\Ubuntu\home\user\sample-project\"),
            normalize_project_key("/home/user/sample-project")
        );
        assert_eq!(
            normalize_project_key(r"\\wsl$\Ubuntu\home\user\sample-project"),
            "/home/user/sample-project"
        );
    }

    #[test]
    fn session_cwd_identity_does_not_walk_filesystem_for_git() {
        let root = make_temp_dir("no-git-walk");
        let project = root.join("repo");
        let nested = project.join("src").join("crate");
        std::fs::create_dir_all(project.join(".git")).expect("create git dir");
        std::fs::create_dir_all(&nested).expect("create nested");

        let identity =
            session_cwd_identity(&nested.display().to_string()).expect("identity from nested cwd");
        assert_eq!(
            normalize_project_key(&identity),
            normalize_project_key(&nested.display().to_string())
        );
        assert_ne!(
            normalize_project_key(&identity),
            normalize_project_key(&project.display().to_string())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_cwd_identity_rejects_relative_paths() {
        assert!(session_cwd_identity("relative/project").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn windows_drive_path_maps_to_wsl_mount() {
        assert_eq!(
            normalize_cross_platform_path(r"C:\Users\user\project"),
            Some(PathBuf::from("/mnt/c/Users/user/project"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_filter_matches_normalized_windows_session_cwd() {
        assert!(path_matches_workspace(
            r"C:\Users\user\project\src",
            Path::new("/mnt/c/Users/user/project")
        ));
        assert!(!path_matches_workspace(
            r"C:\Users\user\project-other",
            Path::new("/mnt/c/Users/user/project")
        ));
    }

    #[test]
    fn duplicate_content_block_lines_count_usage_once() {
        let root = make_temp_dir("dedupe");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let now_ms = Utc::now().timestamp_millis();
        // One response written as three content-block lines, then a second response.
        for offset in 0..3 {
            append_json_line(
                &path,
                assistant_line(now_ms + offset, TEST_PROJECT_CWD, "msg-a", [10, 20, 30, 40]),
            );
        }
        append_json_line(
            &path,
            assistant_line(now_ms + 10, TEST_PROJECT_CWD, "msg-b", [1, 2, 3, 4]),
        );

        let snapshot = compute_snapshot(30, &claude_dir, None, default_test_limits(false), None)
            .expect("snapshot");
        let today = snapshot.utc_days.last().expect("today");
        assert_eq!(today.input_tokens, 11);
        assert_eq!(today.cache_write_tokens, 22);
        assert_eq!(today.cache_read_tokens, 33);
        assert_eq!(today.output_tokens, 44);
        assert_eq!(today.total_tokens, 110);
        assert_eq!(snapshot.utc_totals.cache_hit_rate_percent, 50.0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dedupe_survives_incremental_resume_between_duplicate_lines() {
        let root = make_temp_dir("dedupe-resume");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let cache_db_path = root.join("claudemon.db");
        let now_ms = Utc::now().timestamp_millis();
        append_json_line(
            &path,
            assistant_line(now_ms, TEST_PROJECT_CWD, "msg-a", [5, 0, 0, 5]),
        );
        let limits = default_test_limits(false);
        let first = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("first snapshot");
        assert_eq!(first.utc_totals.last30_days_tokens, 10);

        // The next content block of the same response arrives after the refresh.
        append_json_line(
            &path,
            assistant_line(now_ms + 1, TEST_PROJECT_CWD, "msg-a", [5, 0, 0, 5]),
        );
        let second = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("second snapshot");
        assert_eq!(second.utc_totals.last30_days_tokens, 10);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn synthetic_and_unterminated_lines_are_not_counted() {
        let root = make_temp_dir("synthetic");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let cache_db_path = root.join("claudemon.db");
        let now_ms = Utc::now().timestamp_millis();
        let mut synthetic = assistant_line(now_ms, TEST_PROJECT_CWD, "msg-s", [100, 0, 0, 100]);
        synthetic["message"]["model"] = serde_json::json!("<synthetic>");
        append_json_line(&path, synthetic);
        append_token_file(&path, now_ms + 1, 7, 3);
        // A record Claude Code is still writing (no trailing newline).
        let partial = assistant_line(now_ms + 2, TEST_PROJECT_CWD, "msg-p", [50, 0, 0, 50]);
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open for partial write");
            write!(file, "{partial}").expect("write partial line");
        }

        let limits = default_test_limits(false);
        let first = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("first snapshot");
        assert_eq!(first.utc_totals.last30_days_tokens, 10);
        assert_eq!(
            first.scan_pending_files, 1,
            "partial tail keeps the file pending"
        );

        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open to finish line");
            writeln!(file).expect("finish partial line");
        }
        let second = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("second snapshot");
        assert_eq!(second.utc_totals.last30_days_tokens, 110);
        assert_eq!(second.scan_pending_files, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn runs_count_distinct_prompt_ids_and_time_uses_turn_duration() {
        let root = make_temp_dir("runs-time");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("session.jsonl");
        let start_ms = Utc::now().timestamp_millis() - Duration::hours(1).num_milliseconds();

        // Turn 1: records span 60 s, the exact duration is 90 s.
        append_user_prompt(&path, start_ms, "p1");
        append_token_file(&path, start_ms + 30_000, 1, 1);
        append_tool_result(&path, start_ms + 45_000, "p1");
        append_token_file(&path, start_ms + 60_000, 1, 1);
        append_turn_duration(&path, start_ms + 61_000, 90_000);
        // Turn 2 after a long idle gap, without a turn_duration record: 20 s estimate.
        append_user_prompt(&path, start_ms + 3_600_000 / 2, "p2");
        append_token_file(&path, start_ms + 3_600_000 / 2 + 20_000, 1, 1);

        let snapshot = compute_snapshot(30, &claude_dir, None, default_test_limits(false), None)
            .expect("snapshot");
        let runs: i64 = snapshot.utc_days.iter().map(|day| day.agent_runs).sum();
        let time_ms: i64 = snapshot.utc_days.iter().map(|day| day.agent_time_ms).sum();
        assert_eq!(runs, 2);
        assert_eq!(time_ms, 110_000);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn subagent_transcripts_are_scanned_with_their_own_owner() {
        let root = make_temp_dir("subagents");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let session_dir = sessions_root
            .join("00000000-0000-4000-8000-000000000001")
            .join("subagents");
        std::fs::create_dir_all(&session_dir).expect("create subagents dir");
        let now_ms = Utc::now().timestamp_millis();
        write_token_file(&sessions_root.join("main.jsonl"), now_ms, 100, 10);
        write_token_file(&session_dir.join("agent-1.jsonl"), now_ms + 1, 30, 5);

        let snapshot = compute_snapshot(30, &claude_dir, None, default_test_limits(false), None)
            .expect("snapshot");
        assert_eq!(snapshot.utc_totals.last30_days_tokens, 145);
        assert_eq!(snapshot.scan_total_files, 2);
        assert_eq!(snapshot.project_activity.len(), 1);
        assert_eq!(snapshot.project_activity[0].display_path, TEST_PROJECT_CWD);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn owner_is_first_record_cwd_and_ignores_later_cwd_changes() {
        let root = make_temp_dir("owner");
        let (_claude_dir, sessions_root) = make_claude_dir(&root);
        let path = sessions_root.join("00000000-0000-4000-8000-00000000abcd.jsonl");
        append_json_line(
            &path,
            serde_json::json!({"type": "mode", "mode": "default"}),
        );
        append_usage_line(&path, 1_000, "/work/first", [1, 0, 0, 1]);
        append_usage_line(&path, 2_000, "/work/first/nested", [1, 0, 0, 1]);

        let owner = resolve_session_owner(&path)
            .expect("resolve owner")
            .expect("owner");
        assert_eq!(owner.cwd, "/work/first");
        assert_eq!(
            owner.session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn model_display_names_are_short() {
        assert_eq!(model_display_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_display_name("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(model_display_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_display_name("claude-fable-5-1"), "Fable 5.1");
        assert_eq!(model_display_name("other-model"), "other-model");
    }

    #[test]
    fn deleted_transcripts_keep_counting_from_cache() {
        let root = make_temp_dir("archived");
        let (claude_dir, sessions_root) = make_claude_dir(&root);
        let cache_db_path = root.join("claudemon.db");
        let now_ms = Utc::now().timestamp_millis();
        let old_path = sessions_root.join("old.jsonl");
        write_token_file(&old_path, now_ms - 1_000, 100, 10);
        write_token_file(&sessions_root.join("new.jsonl"), now_ms, 20, 2);
        let limits = default_test_limits(false);
        let first = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("first snapshot");
        assert_eq!(first.utc_totals.last30_days_tokens, 132);

        // Claude Code's cleanup removes the old transcript.
        std::fs::remove_file(&old_path).expect("delete transcript");
        let second = compute_snapshot(30, &claude_dir, None, limits, Some(&cache_db_path))
            .expect("second snapshot");
        assert_eq!(second.utc_totals.last30_days_tokens, 132);
        assert_eq!(second.scan_total_files, 1);
        assert_eq!(second.scan_pending_files, 0);
        assert_eq!(second.project_usage[0].total_tokens, 132);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn open_or_init_scan_cache_db_rebuilds_rows_on_schema_mismatch() {
        let root = make_temp_dir("cache-schema");
        let db_path = root.join("claudemon.db");
        {
            let conn = Connection::open(&db_path).expect("open raw db");
            conn.execute_batch(
                "
                CREATE TABLE cache_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
                INSERT INTO cache_meta(key, value) VALUES ('schema_version', 999);
                CREATE TABLE file_cache (
                    file_path TEXT PRIMARY KEY,
                    file_size INTEGER NOT NULL,
                    file_mtime INTEGER,
                    file_offset INTEGER NOT NULL DEFAULT 0,
                    fully_parsed INTEGER NOT NULL DEFAULT 1,
                    session_cwd TEXT,
                    parser_state_json TEXT NOT NULL DEFAULT '{}',
                    daily_json TEXT NOT NULL,
                    model_daily_json TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                INSERT INTO file_cache(file_path, file_size, daily_json, model_daily_json, updated_at)
                VALUES ('/stale.jsonl', 1, '{}', '{}', 1);
                ",
            )
            .expect("seed stale db");
        }

        let db = open_or_init_scan_cache_db(&db_path).expect("open cache db");
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM file_cache;", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(rows, 0);
        let version: i64 = db
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'schema_version';",
                [],
                |row| row.get(0),
            )
            .expect("read version");
        assert_eq!(version, SCAN_CACHE_DB_SCHEMA_VERSION);

        let _ = std::fs::remove_dir_all(root);
    }
}
