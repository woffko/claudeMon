//! Subscription limit data shown on the USAGE limits card and the APISTAT
//! screen, and the sources that produce it (see `docs/sources.md`):
//!
//! - status-line bridge (default): `claudemon statusline` records the rate
//!   limits Claude Code passes to its status line; the TUI reads that
//!   snapshot. No credentials are involved.
//! - OAuth usage endpoint (opt-in): the endpoint behind Claude Code's
//!   `/usage` screen, called with the user's Claude Code OAuth token. It adds
//!   per-model weekly limits, extra usage, the by-surface breakdown, and the
//!   limit-reset status.
//! - Claude Code's cache: the last `/usage` response Claude Code itself
//!   saved in its global config file. It fills in the per-model weekly
//!   limits for the status-line source without credentials.

pub mod claude_cache;
pub mod oauth;
pub mod statusline;

use chrono::DateTime;
use serde_json::Value;

pub const FIVE_HOUR_WINDOW_MINS: f64 = 5.0 * 60.0;
pub const SEVEN_DAY_WINDOW_MINS: f64 = 7.0 * 24.0 * 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsSourceKind {
    StatusLine,
    OAuth,
    ClaudeCache,
}

impl LimitsSourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::StatusLine => "STATUSLINE",
            Self::OAuth => "OAUTH",
            Self::ClaudeCache => "CLAUDE CACHE",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindow {
    /// Share of the limit used, 0-100.
    pub used_percent: Option<f64>,
    pub window_duration_mins: Option<f64>,
    /// Unix seconds when the window resets.
    pub resets_at: Option<i64>,
}

/// A limit scoped to one model or surface (for example the weekly limit of
/// one model family).
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitSnapshot {
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
    /// Size of this limit as a percentage of the all-models weekly limit.
    pub share_of_weekly: Option<f64>,
}

/// One grant of limit resets: each reset refills the listed limits at once
/// without moving the weekly reset day.
#[derive(Debug, Clone, PartialEq)]
pub struct ResetGrant {
    pub label: Option<String>,
    pub resets_left: u32,
    pub resets_total: u32,
    /// Unix seconds after which the grant can no longer be used.
    pub ends_at: Option<i64>,
    /// Display names of the limits a reset refills.
    pub clears: Vec<String>,
    pub paused: bool,
    pub usable_now: bool,
    /// A reset can be used only while one of the limits is reached.
    pub requires_limit: bool,
}

/// Limit-reset status ("Use your reset" on claude.ai, `/limit-reset` in
/// Claude Code).
#[derive(Debug, Clone, PartialEq)]
pub struct LimitResetStatus {
    pub eligible: bool,
    /// Why no reset is offered (for example `no_grant`), as reported.
    pub ineligible_reason: Option<String>,
    pub at_limit: bool,
    pub grants: Vec<ResetGrant>,
    /// Unix seconds until which no reset can be used.
    pub cooldown_until: Option<i64>,
}

impl LimitResetStatus {
    /// Resets that can still be used, across grants that are not paused.
    pub fn resets_left(&self) -> u32 {
        self.grants
            .iter()
            .filter(|grant| !grant.paused)
            .map(|grant| grant.resets_left)
            .sum()
    }

    /// The usable grant that expires first.
    pub fn next_grant(&self) -> Option<&ResetGrant> {
        self.grants
            .iter()
            .filter(|grant| grant.resets_left > 0 && !grant.paused)
            .min_by_key(|grant| grant.ends_at.unwrap_or(i64::MAX))
    }
}

/// Paid usage beyond the subscription limits.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraUsage {
    pub enabled: bool,
    pub used_amount: Option<f64>,
    pub limit_amount: Option<f64>,
    pub currency: Option<String>,
    pub used_percent: Option<f64>,
}

/// Share of the weekly usage by surface (Claude Code, chat, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceShare {
    pub name: String,
    pub percent: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountRateLimits {
    pub source: LimitsSourceKind,
    /// Unix seconds when the data was captured.
    pub captured_at: i64,
    /// Subscription type reported with the credentials ("pro", "max", ...).
    pub plan: Option<String>,
    /// Five-hour session window.
    pub primary: Option<RateLimitWindow>,
    /// Seven-day window across all models.
    pub secondary: Option<RateLimitWindow>,
    /// Scoped weekly limits (per model).
    pub buckets: Vec<RateLimitSnapshot>,
    /// Gateway spend limit (status line only).
    pub spend_limit: Option<RateLimitWindow>,
    pub extra_usage: Option<ExtraUsage>,
    pub surfaces: Vec<SurfaceShare>,
    /// Name of the limit that currently binds, when the server reports it.
    pub active_limit: Option<String>,
    pub limit_reset: Option<LimitResetStatus>,
    /// Set when the scoped limits, extra usage, surfaces, and reset status
    /// come from Claude Code's cache, captured at this (older) time.
    pub details_captured_at: Option<i64>,
}

impl AccountRateLimits {
    pub fn empty(source: LimitsSourceKind, captured_at: i64) -> Self {
        Self {
            source,
            captured_at,
            plan: None,
            primary: None,
            secondary: None,
            buckets: Vec::new(),
            spend_limit: None,
            extra_usage: None,
            surfaces: Vec::new(),
            active_limit: None,
            limit_reset: None,
            details_captured_at: None,
        }
    }

    /// Clears windows whose reset time has passed: the old value no longer
    /// applies and the new one is unknown. Drops expired reset grants.
    pub(crate) fn expire(&mut self, now: i64) {
        let windows = [
            self.primary.as_mut(),
            self.secondary.as_mut(),
            self.spend_limit.as_mut(),
        ];
        let buckets = self
            .buckets
            .iter_mut()
            .filter_map(|bucket| bucket.primary.as_mut());
        for window in windows.into_iter().flatten().chain(buckets) {
            if window.resets_at.is_some_and(|reset| reset <= now) {
                window.used_percent = None;
                window.resets_at = None;
            }
        }
        if let Some(status) = self.limit_reset.as_mut() {
            status
                .grants
                .retain(|grant| grant.ends_at.is_none_or(|ends_at| ends_at > now));
            status.cooldown_until = status.cooldown_until.filter(|until| *until > now);
        }
    }
}

pub fn normalize_epoch_millis(value: i64) -> i64 {
    if value > 0 && value < 1_000_000_000_000 {
        value * 1000
    } else {
        value
    }
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Reads a number that may be encoded as an integer, float, or numeric string.
pub(crate) fn as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|number: &f64| number.is_finite())
}

/// Reads a timestamp given as Unix seconds/milliseconds or an RFC 3339 string
/// and returns Unix seconds.
pub(crate) fn as_unix_seconds(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|parsed| parsed.timestamp());
    }
    let raw = value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))?;
    Some(normalize_epoch_millis(raw) / 1000)
}
