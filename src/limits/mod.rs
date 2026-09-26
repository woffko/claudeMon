//! Account limit and server-usage data shared by the APISTAT screen.
//!
//! The types mirror the layout the UI renders: a primary (short) and a
//! secondary (weekly) rate-limit window, optional per-model buckets, and
//! optional server-side daily usage. Claude limit sources (OAuth usage
//! endpoint, status-line bridge) populate them; until a source is enabled
//! the APISTAT screen shows an "unavailable" state.

// Fields are populated by limit sources added in a later phase.
#![allow(dead_code)]

#[derive(Debug, Clone)]
pub struct RateLimitWindow {
    pub used_percent: Option<f64>,
    pub window_duration_mins: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

/// Detail rows are retained for the planned reset-details screen. The compact summary currently
/// consumes only `expires_at`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RateLimitResetCredit {
    pub id: Option<String>,
    pub reset_type: Option<String>,
    pub status: Option<String>,
    pub granted_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpendControlLimitSnapshot {
    pub limit: Option<String>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub used: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RateLimitSnapshot {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub individual_limit: Option<SpendControlLimitSnapshot>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
}

#[derive(Debug, Clone)]
pub struct AccountRateLimits {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub individual_limit: Option<SpendControlLimitSnapshot>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
    pub buckets: Vec<RateLimitSnapshot>,
    pub reset_credits_available: Option<i64>,
    /// `None` means the backend did not return the optional credit detail rows.
    pub reset_credits: Option<Vec<RateLimitResetCredit>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCreditOutcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsageSummary {
    pub lifetime_tokens: Option<u64>,
    pub peak_daily_tokens: Option<u64>,
    pub longest_running_turn_sec: Option<u64>,
    pub current_streak_days: Option<u64>,
    pub longest_streak_days: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyUsageBucket {
    pub start_date: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsage {
    pub summary: AccountUsageSummary,
    /// `None` means the backend did not return the optional daily activity series.
    pub daily_usage_buckets: Option<Vec<DailyUsageBucket>>,
}

pub fn normalize_epoch_millis(value: i64) -> i64 {
    if value > 0 && value < 1_000_000_000_000 {
        value * 1000
    } else {
        value
    }
}
