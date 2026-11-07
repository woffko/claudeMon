//! Claude Code's cached copy of its last `/usage` response. Claude Code
//! keeps it in its global config file (`~/.claude.json`, or
//! `$CLAUDE_CONFIG_DIR/.claude.json`) under `cachedUsageUtilization` and
//! refreshes it whenever it calls the usage endpoint itself (for example on
//! `/usage`). It adds the per-model weekly limits to the status-line source
//! without credentials.
//!
//! Only that key and the signed-in account id (to skip a cache left by
//! another account) are deserialized; nothing from the file is stored.

use super::{normalize_epoch_millis, oauth::parse_usage, AccountRateLimits, LimitsSourceKind};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

const CONFIG_FILE_NAME: &str = ".claude.json";
const MAX_CONFIG_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
struct GlobalConfig {
    #[serde(rename = "cachedUsageUtilization")]
    cached_usage: Option<Value>,
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<Value>,
}

/// Path of Claude Code's global config file: next to the default
/// `~/.claude` directory, inside a directory set with `CLAUDE_CONFIG_DIR`.
pub fn config_path(claude_dir: &Path) -> PathBuf {
    let home = ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from);
    config_path_for(claude_dir, home.as_deref())
}

fn config_path_for(claude_dir: &Path, home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) if claude_dir == home.join(".claude") => home.join(CONFIG_FILE_NAME),
        _ => claude_dir.join(CONFIG_FILE_NAME),
    }
}

/// Loads the cached usage, or `None` when Claude Code has not cached any
/// for the signed-in account.
pub fn load(claude_dir: &Path, now: i64) -> Result<Option<AccountRateLimits>> {
    let path = config_path(claude_dir);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", path.display()))
        }
    };
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        anyhow::bail!("Refusing to read {}: not a regular file", path.display());
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("Unable to read {}", path.display()))?;
    parse_config(&bytes, now).with_context(|| format!("Unable to parse {}", path.display()))
}

fn parse_config(bytes: &[u8], now: i64) -> Result<Option<AccountRateLimits>> {
    let config: GlobalConfig = serde_json::from_slice(bytes)?;
    let Some(cached) = config.cached_usage.filter(Value::is_object) else {
        return Ok(None);
    };
    let account = |value: Option<&Value>| {
        value
            .and_then(|value| value.get("accountUuid"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let cached_account = account(Some(&cached));
    let current_account = account(config.oauth_account.as_ref());
    if cached_account.is_some() && current_account.is_some() && cached_account != current_account {
        return Ok(None);
    }
    let (Some(fetched_at_ms), Some(utilization)) = (
        cached.get("fetchedAtMs").and_then(Value::as_i64),
        cached.get("utilization").filter(|value| value.is_object()),
    ) else {
        return Ok(None);
    };
    let captured_at = normalize_epoch_millis(fetched_at_ms) / 1000;
    let mut limits = parse_usage(utilization, captured_at)?;
    limits.source = LimitsSourceKind::ClaudeCache;
    limits.expire(now);
    Ok(Some(limits))
}

/// Combines a status-line snapshot with Claude Code's cached usage. The
/// newer of the two supplies the session and weekly windows; the cache
/// always supplies what the status line does not carry (per-model limits,
/// extra usage, the by-surface breakdown).
pub fn merge(live: AccountRateLimits, cached: Option<AccountRateLimits>) -> AccountRateLimits {
    let Some(mut cached) = cached else {
        return live;
    };
    if cached.captured_at >= live.captured_at {
        cached.spend_limit = cached.spend_limit.or(live.spend_limit);
        return cached;
    }
    let mut merged = live;
    let has_details = !cached.buckets.is_empty()
        || !cached.surfaces.is_empty()
        || cached.extra_usage.is_some()
        || cached.limit_reset.is_some();
    merged.buckets = cached.buckets;
    merged.surfaces = cached.surfaces;
    merged.extra_usage = cached.extra_usage;
    merged.limit_reset = cached.limit_reset;
    merged.details_captured_at = has_details.then_some(cached.captured_at);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::RateLimitWindow;

    /// Synthetic config: unrelated keys are ignored, the cached response has
    /// the same layout as the usage endpoint.
    fn config(cached_account: &str, current_account: &str) -> Vec<u8> {
        serde_json::json!({
            "numStartups": 3,
            "projects": {"/synthetic/project": {"allowedTools": []}},
            "oauthAccount": {"accountUuid": current_account, "emailAddress": "user@example.com"},
            "cachedUsageUtilization": {
                "fetchedAtMs": 1_000_000_000_000_i64,
                "accountUuid": cached_account,
                "utilization": {
                    "five_hour": {"utilization": 10, "resets_at": "2001-09-09T03:00:00+00:00"},
                    "limits": [
                        {"kind": "session", "group": "session", "percent": 10,
                         "resets_at": "2001-09-09T03:00:00+00:00", "scope": null},
                        {"kind": "weekly_all", "group": "weekly", "percent": 66,
                         "resets_at": "2001-09-12T00:00:00+00:00", "scope": null},
                        {"kind": "weekly_scoped", "group": "weekly", "percent": 4,
                         "resets_at": "2001-09-12T00:00:00+00:00",
                         "scope": {"model": {"id": null, "display_name": "Fable"}}}
                    ],
                    "weekly_scoped_shares": [
                        {"limit_index": 2, "allowance_percent_of_weekly": 50}
                    ],
                    "cedar_ember": null
                }
            }
        })
        .to_string()
        .into_bytes()
    }

    fn window(used: f64, resets_at: i64) -> RateLimitWindow {
        RateLimitWindow {
            used_percent: Some(used),
            window_duration_mins: None,
            resets_at: Some(resets_at),
        }
    }

    #[test]
    fn config_path_follows_claude_code() {
        let home = Path::new("/home/user");
        assert_eq!(
            config_path_for(&home.join(".claude"), Some(home)),
            home.join(".claude.json")
        );
        assert_eq!(
            config_path_for(Path::new("/srv/claude-config"), Some(home)),
            Path::new("/srv/claude-config/.claude.json")
        );
    }

    #[test]
    fn cached_usage_parses_and_expires_passed_windows() {
        // 2001-09-09T01:46:40Z, before the session reset.
        let now = 1_000_000_000;
        let limits = parse_config(&config("a", "a"), now)
            .expect("parse")
            .expect("cached usage");
        assert_eq!(limits.source, LimitsSourceKind::ClaudeCache);
        assert_eq!(limits.captured_at, 1_000_000_000);
        assert_eq!(
            limits.primary.as_ref().and_then(|w| w.used_percent),
            Some(10.0)
        );
        assert_eq!(limits.buckets[0].limit_name.as_deref(), Some("Fable"));
        assert_eq!(limits.buckets[0].share_of_weekly, Some(50.0));
        assert_eq!(limits.limit_reset, None);

        // After the session reset the old session percentage no longer applies.
        let later = parse_config(&config("a", "a"), 1_000_020_000)
            .expect("parse")
            .expect("cached usage");
        let session = later.primary.expect("session window");
        assert_eq!((session.used_percent, session.resets_at), (None, None));
        assert_eq!(
            later.buckets[0]
                .primary
                .as_ref()
                .and_then(|w| w.used_percent),
            Some(4.0)
        );
    }

    #[test]
    fn cache_of_another_account_or_without_data_is_ignored() {
        assert_eq!(parse_config(&config("a", "b"), 1).expect("parse"), None);
        assert_eq!(
            parse_config(b"{\"numStartups\": 1}", 1).expect("parse"),
            None
        );
        assert_eq!(
            parse_config(b"{\"cachedUsageUtilization\": null}", 1).expect("parse"),
            None
        );
        assert!(parse_config(b"not json", 1).is_err());
    }

    #[test]
    fn merge_keeps_the_newer_windows_and_adds_cached_details() {
        let cached = parse_config(&config("a", "a"), 1_000_000_000)
            .expect("parse")
            .expect("cached usage");

        let mut live = AccountRateLimits::empty(LimitsSourceKind::StatusLine, 1_000_000_500);
        live.secondary = Some(window(70.0, 1_000_200_000));
        let merged = merge(live.clone(), Some(cached.clone()));
        assert_eq!(merged.source, LimitsSourceKind::StatusLine);
        assert_eq!(merged.secondary, live.secondary);
        assert_eq!(merged.buckets, cached.buckets);
        assert_eq!(merged.details_captured_at, Some(1_000_000_000));

        let mut older = AccountRateLimits::empty(LimitsSourceKind::StatusLine, 999_000_000);
        older.spend_limit = Some(window(5.0, 1_000_300_000));
        let merged = merge(older.clone(), Some(cached.clone()));
        assert_eq!(merged.source, LimitsSourceKind::ClaudeCache);
        assert_eq!(merged.secondary, cached.secondary);
        assert_eq!(merged.spend_limit, older.spend_limit);
        assert_eq!(merged.details_captured_at, None);

        assert_eq!(merge(live.clone(), None), live);
    }
}
