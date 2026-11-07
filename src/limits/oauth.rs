//! Opt-in OAuth usage source: the endpoint behind Claude Code's `/usage`
//! screen. It is undocumented, so parsing is strict about types and lenient
//! about missing fields; field notes are in `docs/sources.md`.
//!
//! The access token is read from Claude Code's credentials at request time
//! and kept only in memory for that request. It is never logged, cached,
//! refreshed, or written; an expired token is reported instead.

use super::{
    as_f64, as_unix_seconds, unix_now, AccountRateLimits, ExtraUsage, LimitResetStatus,
    LimitsSourceKind, RateLimitSnapshot, RateLimitWindow, ResetGrant, SurfaceShare,
    FIVE_HOUR_WINDOW_MINS, SEVEN_DAY_WINDOW_MINS,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// `cedar_ember=1` asks for the limit-reset status as well, as Claude Code
/// does before offering `/limit-reset`.
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_CREDENTIALS_BYTES: u64 = 64 * 1024;
/// Minimum interval between requests, regardless of the configured refresh.
pub const MIN_REFRESH_SECS: u64 = 60;

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    plan: Option<String>,
}

/// Fetches and parses the current limits. Blocking; run it off the UI thread.
pub fn fetch(claude_dir: &Path) -> Result<AccountRateLimits> {
    let credentials = read_credentials(claude_dir)?;
    let now = unix_now();
    if credentials
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
    {
        anyhow::bail!("Claude Code OAuth token expired; run Claude Code to refresh it.");
    }

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!("claudemon/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent
        .get(USAGE_URL)
        .header(
            "Authorization",
            &format!("Bearer {}", credentials.access_token),
        )
        .header("anthropic-beta", OAUTH_BETA)
        .call()
        .context("OAuth usage request failed")?;
    let status = response.status().as_u16();
    if status != 200 {
        anyhow::bail!("OAuth usage endpoint returned HTTP {status}.");
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .context("Unable to read OAuth usage response")?;
    let value: Value =
        serde_json::from_str(&body).context("OAuth usage response is not valid JSON")?;
    let mut limits = parse_usage(&value, now)?;
    limits.plan = credentials.plan;
    Ok(limits)
}

fn read_credentials(claude_dir: &Path) -> Result<Credentials> {
    let path = claude_dir.join(".credentials.json");
    let raw = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() && meta.len() <= MAX_CREDENTIALS_BYTES => {
            std::fs::read(&path).with_context(|| format!("Unable to read {}", path.display()))?
        }
        Ok(_) => anyhow::bail!("Refusing to read {}: not a regular file", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => keychain_credentials()
            .with_context(|| {
                format!(
                    "No Claude Code credentials at {}; log in with Claude Code first.",
                    path.display()
                )
            })?,
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", path.display()))
        }
    };
    parse_credentials(&raw)
}

/// Claude Code stores credentials in the macOS Keychain instead of a file.
#[cfg(target_os = "macos")]
fn keychain_credentials() -> Result<Vec<u8>> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .context("Unable to run the macOS security tool")?;
    if !output.status.success() {
        anyhow::bail!("Claude Code credentials are not in the Keychain.");
    }
    Ok(output.stdout)
}

#[cfg(not(target_os = "macos"))]
fn keychain_credentials() -> Result<Vec<u8>> {
    anyhow::bail!("Claude Code credentials file not found.")
}

fn parse_credentials(raw: &[u8]) -> Result<Credentials> {
    let value: Value =
        serde_json::from_slice(raw).context("Claude Code credentials are not valid JSON")?;
    let oauth = value
        .get("claudeAiOauth")
        .context("Claude Code credentials have no claudeAiOauth entry")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .context("Claude Code credentials have no access token")?
        .to_string();
    Ok(Credentials {
        access_token,
        expires_at: oauth.get("expiresAt").and_then(as_unix_seconds),
        plan: oauth
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Parses the usage response. The self-describing `limits[]` list is
/// preferred; the fixed `five_hour` / `seven_day*` objects are a fallback.
pub(crate) fn parse_usage(value: &Value, now: i64) -> Result<AccountRateLimits> {
    if !value.is_object() {
        anyhow::bail!("OAuth usage response has an unexpected shape.");
    }
    let mut limits = AccountRateLimits::empty(LimitsSourceKind::OAuth, now);

    if let Some(entries) = value.get("limits").and_then(Value::as_array) {
        // Bucket index of each `limits[]` entry, for `weekly_scoped_shares`.
        let mut bucket_of_entry: Vec<Option<usize>> = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut bucket_index = None;
            let window = RateLimitWindow {
                used_percent: entry.get("percent").and_then(as_f64),
                window_duration_mins: None,
                resets_at: entry.get("resets_at").and_then(as_unix_seconds),
            };
            let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
            let group = entry.get("group").and_then(Value::as_str).unwrap_or("");
            let name = match (kind, group) {
                ("session", _) | (_, "session") => {
                    limits.primary = Some(RateLimitWindow {
                        window_duration_mins: Some(FIVE_HOUR_WINDOW_MINS),
                        ..window
                    });
                    "5h session".to_string()
                }
                ("weekly_all", _) => {
                    limits.secondary = Some(RateLimitWindow {
                        window_duration_mins: Some(SEVEN_DAY_WINDOW_MINS),
                        ..window
                    });
                    "Weekly".to_string()
                }
                _ => {
                    let name =
                        scope_name(entry.get("scope")).unwrap_or_else(|| kind.replace('_', " "));
                    bucket_index = Some(limits.buckets.len());
                    limits.buckets.push(RateLimitSnapshot {
                        limit_name: Some(name.clone()),
                        primary: Some(RateLimitWindow {
                            window_duration_mins: (group == "weekly")
                                .then_some(SEVEN_DAY_WINDOW_MINS),
                            ..window
                        }),
                        share_of_weekly: None,
                    });
                    name
                }
            };
            bucket_of_entry.push(bucket_index);
            if entry.get("is_active").and_then(Value::as_bool) == Some(true) {
                limits.active_limit = Some(name);
            }
        }
        for share in value
            .get("weekly_scoped_shares")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let bucket = share
                .get("limit_index")
                .and_then(Value::as_u64)
                .and_then(|index| bucket_of_entry.get(usize::try_from(index).ok()?).copied())
                .flatten();
            if let Some(bucket) = bucket {
                limits.buckets[bucket].share_of_weekly =
                    share.get("allowance_percent_of_weekly").and_then(as_f64);
            }
        }
    } else {
        let window = |key: &str, minutes: f64| {
            value
                .get(key)
                .filter(|raw| raw.is_object())
                .map(|raw| RateLimitWindow {
                    used_percent: raw.get("utilization").and_then(as_f64),
                    window_duration_mins: Some(minutes),
                    resets_at: raw.get("resets_at").and_then(as_unix_seconds),
                })
        };
        limits.primary = window("five_hour", FIVE_HOUR_WINDOW_MINS);
        limits.secondary = window("seven_day", SEVEN_DAY_WINDOW_MINS);
        for (key, name) in [
            ("seven_day_opus", "Opus"),
            ("seven_day_sonnet", "Sonnet"),
            ("seven_day_overage_included", "Fable"),
        ] {
            if let Some(scoped) = window(key, SEVEN_DAY_WINDOW_MINS) {
                limits.buckets.push(RateLimitSnapshot {
                    limit_name: Some(name.to_string()),
                    primary: Some(scoped),
                    share_of_weekly: None,
                });
            }
        }
    }

    limits.extra_usage = parse_extra_usage(value);
    limits.limit_reset = value.get("cedar_ember").and_then(parse_limit_reset);
    if let Some(rows) = value
        .get("seven_day_breakdown")
        .and_then(|breakdown| breakdown.get("rows"))
        .and_then(Value::as_array)
    {
        limits.surfaces = rows
            .iter()
            .filter_map(|row| {
                let name = row
                    .get("display_name")
                    .or_else(|| row.get("key"))
                    .and_then(Value::as_str)?;
                Some(SurfaceShare {
                    name: name.to_string(),
                    percent: row.get("percent").and_then(as_f64)?,
                })
            })
            .collect();
    }
    Ok(limits)
}

fn scope_name(scope: Option<&Value>) -> Option<String> {
    let scope = scope?;
    scope
        .get("model")
        .and_then(|model| model.get("display_name").or_else(|| model.get("id")))
        .or_else(|| scope.get("surface"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Limit-reset status from the `cedar_ember` object (present when asked for
/// with `cedar_ember=1`). Malformed grants are skipped.
fn parse_limit_reset(value: &Value) -> Option<LimitResetStatus> {
    let eligible = value.get("eligible").and_then(Value::as_bool)?;
    let count = |grant: &Value, key: &str| {
        grant
            .get(key)
            .and_then(Value::as_u64)
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
    };
    let flag = |grant: &Value, key: &str, default: bool| {
        grant.get(key).and_then(Value::as_bool).unwrap_or(default)
    };
    let grants = value
        .get("grants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|grant| {
            let resets_left = count(grant, "resets_left")?;
            Some(ResetGrant {
                label: grant
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string),
                resets_left,
                resets_total: count(grant, "resets_total").unwrap_or(0).max(resets_left),
                ends_at: grant.get("ends_at").and_then(as_unix_seconds),
                clears: grant
                    .get("clears")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(limit_type_name)
                    .collect(),
                paused: flag(grant, "paused", false),
                usable_now: flag(grant, "usable_now", false),
                requires_limit: flag(grant, "use_requires_limit", true),
            })
        })
        .collect();
    Some(LimitResetStatus {
        eligible,
        ineligible_reason: value
            .get("ineligible_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        at_limit: value
            .get("at_limit")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        grants,
        cooldown_until: value.get("cooldown_until").and_then(as_unix_seconds),
    })
}

/// Display name of a rate-limit type as Claude Code names it.
fn limit_type_name(key: &str) -> String {
    match key {
        "five_hour" => "5h session".to_string(),
        "seven_day" => "weekly".to_string(),
        "seven_day_opus" => "Opus weekly".to_string(),
        "seven_day_sonnet" => "Sonnet weekly".to_string(),
        "seven_day_overage_included" => "Fable weekly".to_string(),
        other => other.replace('_', " "),
    }
}

/// Extra usage from `spend` (amounts in minor units with an exponent), with
/// the enabled flag from `extra_usage`.
fn parse_extra_usage(value: &Value) -> Option<ExtraUsage> {
    let spend = value.get("spend").filter(|spend| spend.is_object());
    let extra = value.get("extra_usage").filter(|extra| extra.is_object());
    if spend.is_none() && extra.is_none() {
        return None;
    }
    let amount = |raw: Option<&Value>| -> Option<(f64, Option<String>)> {
        let raw = raw?;
        let minor = raw.get("amount_minor").and_then(as_f64)?;
        let exponent = raw.get("exponent").and_then(Value::as_i64).unwrap_or(2);
        let currency = raw
            .get("currency")
            .and_then(Value::as_str)
            .map(str::to_string);
        Some((minor / 10f64.powi(exponent.clamp(0, 9) as i32), currency))
    };
    let used = amount(spend.and_then(|spend| spend.get("used")));
    let limit = amount(spend.and_then(|spend| spend.get("limit")));
    let enabled = spend
        .and_then(|spend| spend.get("enabled"))
        .and_then(Value::as_bool)
        .or_else(|| {
            extra
                .and_then(|extra| extra.get("is_enabled"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    Some(ExtraUsage {
        enabled,
        used_amount: used.as_ref().map(|(amount, _)| *amount),
        limit_amount: limit.as_ref().map(|(amount, _)| *amount),
        currency: used
            .and_then(|(_, currency)| currency)
            .or_else(|| limit.and_then(|(_, currency)| currency)),
        used_percent: spend
            .and_then(|spend| spend.get("percent"))
            .and_then(as_f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic response with the field layout recorded in docs/sources.md.
    fn sample_response() -> Value {
        serde_json::json!({
            "five_hour": {"utilization": 10.0, "resets_at": "2026-09-26T08:40:00.277471+00:00"},
            "seven_day": {"utilization": 13.0, "resets_at": "2026-10-01T17:00:00+00:00"},
            "seven_day_opus": null,
            "nimbus_quill": {"utilization": 0.0, "resets_at": null},
            "extra_usage": {"is_enabled": false, "monthly_limit": null},
            "limits": [
                {"kind": "session", "group": "session", "percent": 10, "severity": "normal",
                 "resets_at": "2026-09-26T08:40:00.277471+00:00", "scope": null, "is_active": false},
                {"kind": "weekly_all", "group": "weekly", "percent": 13, "severity": "normal",
                 "resets_at": "2026-10-01T17:00:00+00:00", "scope": null, "is_active": true},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal",
                 "resets_at": "2026-10-01T17:00:00+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
                 "is_active": false}
            ],
            "spend": {"used": {"amount_minor": 1250, "currency": "USD", "exponent": 2},
                      "limit": null, "percent": 0, "enabled": false},
            "seven_day_breakdown": {"rows": [
                {"key": "claude_code", "display_name": "Claude Code", "percent": 99},
                {"key": "chat", "display_name": "Chats", "percent": 1}
            ]},
            "weekly_scoped_shares": [
                {"limit_index": 2, "used_percent_of_weekly": 0, "allowance_percent_of_weekly": 50},
                {"limit_index": 0, "allowance_percent_of_weekly": 10},
                {"limit_index": 9, "allowance_percent_of_weekly": 10}
            ],
            "cedar_ember": {
                "eligible": true, "at_limit": false, "next_grant_id": "g1",
                "grants": [
                    {"id": "g1", "label": "Launch reset", "resets_total": 2, "resets_left": 1,
                     "starts_at": "2026-09-20T00:00:00+00:00", "ends_at": "2026-10-04T00:00:00+00:00",
                     "clears": ["five_hour", "seven_day", "seven_day_overage_included", "new_kind"],
                     "paused": false, "usable_now": false, "use_requires_limit": true,
                     "percent_used": {}, "blocking": []},
                    {"id": "broken", "resets_left": -1}
                ]
            }
        })
    }

    #[test]
    fn parse_usage_prefers_limits_list() {
        let limits = parse_usage(&sample_response(), 1_000).expect("parse");
        let primary = limits.primary.expect("session window");
        assert_eq!(primary.used_percent, Some(10.0));
        assert_eq!(primary.window_duration_mins, Some(FIVE_HOUR_WINDOW_MINS));
        assert_eq!(primary.resets_at, Some(1_790_412_000));
        let weekly = limits.secondary.expect("weekly window");
        assert_eq!(weekly.used_percent, Some(13.0));
        assert_eq!(limits.buckets.len(), 1);
        assert_eq!(limits.buckets[0].limit_name.as_deref(), Some("Fable"));
        assert_eq!(
            limits.buckets[0].share_of_weekly,
            Some(50.0),
            "shares map limits[] indexes to buckets; non-bucket indexes are ignored"
        );
        assert_eq!(limits.active_limit.as_deref(), Some("Weekly"));
        assert_eq!(
            limits.surfaces,
            vec![
                SurfaceShare {
                    name: "Claude Code".to_string(),
                    percent: 99.0,
                },
                SurfaceShare {
                    name: "Chats".to_string(),
                    percent: 1.0,
                },
            ]
        );
        let extra = limits.extra_usage.expect("extra usage");
        assert!(!extra.enabled);
        assert_eq!(extra.used_amount, Some(12.5));
        assert_eq!(extra.currency.as_deref(), Some("USD"));

        let reset = limits.limit_reset.expect("reset status");
        assert!(reset.eligible && !reset.at_limit);
        assert_eq!(reset.grants.len(), 1, "the malformed grant is skipped");
        let grant = &reset.grants[0];
        assert_eq!(grant.label.as_deref(), Some("Launch reset"));
        assert_eq!((grant.resets_left, grant.resets_total), (1, 2));
        assert_eq!(grant.ends_at, Some(1_791_072_000));
        assert_eq!(
            grant.clears,
            ["5h session", "weekly", "Fable weekly", "new kind"]
        );
        assert!(grant.requires_limit && !grant.usable_now && !grant.paused);
        assert_eq!(reset.resets_left(), 1);
        assert_eq!(reset.next_grant(), Some(grant));
    }

    #[test]
    fn parse_usage_reads_an_ineligible_reset_status() {
        let value = serde_json::json!({
            "cedar_ember": {"eligible": false, "ineligible_reason": "no_grant"}
        });
        let reset = parse_usage(&value, 1)
            .expect("parse")
            .limit_reset
            .expect("reset status");
        assert!(!reset.eligible);
        assert_eq!(reset.ineligible_reason.as_deref(), Some("no_grant"));
        assert_eq!(reset.resets_left(), 0);
        assert_eq!(reset.next_grant(), None);
        let missing = parse_usage(&serde_json::json!({"cedar_ember": null}), 1).expect("parse");
        assert_eq!(missing.limit_reset, None);
    }

    #[test]
    fn parse_usage_falls_back_to_fixed_windows() {
        let value = serde_json::json!({
            "five_hour": {"utilization": 55.5, "resets_at": "2026-09-26T08:40:00+00:00"},
            "seven_day": null,
            "seven_day_opus": {"utilization": 20, "resets_at": null},
            "seven_day_overage_included": {"utilization": 7, "resets_at": null}
        });
        let limits = parse_usage(&value, 1).expect("parse");
        assert_eq!(limits.primary.and_then(|w| w.used_percent), Some(55.5));
        assert_eq!(limits.secondary, None);
        assert_eq!(limits.buckets[0].limit_name.as_deref(), Some("Opus"));
        assert_eq!(limits.buckets[1].limit_name.as_deref(), Some("Fable"));
        assert_eq!(limits.extra_usage, None);
    }

    #[test]
    fn parse_usage_rejects_non_object() {
        assert!(parse_usage(&serde_json::json!([1, 2]), 1).is_err());
    }

    #[test]
    fn credentials_parse_token_expiry_and_plan() {
        let raw = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "synthetic-token",
                "expiresAt": 1_900_000_000_000_i64,
                "subscriptionType": "pro"
            }
        })
        .to_string();
        let credentials = parse_credentials(raw.as_bytes()).expect("credentials");
        assert_eq!(credentials.access_token, "synthetic-token");
        assert_eq!(credentials.expires_at, Some(1_900_000_000));
        assert_eq!(credentials.plan.as_deref(), Some("pro"));
        assert!(parse_credentials(b"{\"claudeAiOauth\":{}}").is_err());
    }
}
