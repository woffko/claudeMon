# Claude data sources

Field-level notes for the live-limit sources in `PLAN.md` section 3.
Observed 2026-09-26 with Claude Code 2.1.283 on a subscription account.
Only field names and value types are recorded here; no tokens or account
values.

## S1. OAuth usage endpoint (undocumented)

Request:

```
GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1
Authorization: Bearer <claudeAiOauth.accessToken>
anthropic-beta: oauth-2025-04-20
```

`cedar_ember=1` adds the limit-reset status (`cedar_ember` below); Claude
Code sends it before offering `/limit-reset`. Without it the field is null
and the rest of the response is the same (checked 2026-10-18, Claude Code
2.1.302).

No rate-limit headers were returned.

Credentials file `<claude-dir>/.credentials.json` (Linux/Windows; macOS uses
the Keychain): `claudeAiOauth.{accessToken, refreshToken, expiresAt (ms),
refreshTokenExpiresAt, scopes[], subscriptionType ("pro", "max", ...),
rateLimitTier}`. ClaudeMon must never refresh or rewrite this file; when the
access token is expired it reports "stale token, run Claude Code".

Response (top level):

| Field | Type | Notes |
|-------|------|-------|
| `five_hour`, `seven_day` | window or null | `utilization` (0-100 float), `resets_at` (RFC 3339 with offset), `limit_dollars`, `used_dollars`, `remaining_dollars`, `locked_reason` |
| `seven_day_opus`, `seven_day_sonnet`, `seven_day_oauth_apps`, `seven_day_cowork`, ... | window or null | Legacy per-scope windows |
| many code-named keys (`nimbus_quill`, `amber_gauge`, ...) | window or null | Unstable internal names; ignore |
| `limits[]` | array | **Preferred.** Self-describing list, see below |
| `extra_usage` | object | `is_enabled`, `monthly_limit`, `used_credits`, `utilization`, `currency`, `decimal_places`, `disabled_reason`, `user_disabled`, `spend_limit_reached`, `credits_ever_enabled`, `daily`, `weekly` |
| `spend` | object | `used{amount_minor, currency, exponent}`, `limit`, `percent`, `severity`, `enabled`, `disabled_reason`, `cap`, `balance`, `auto_reload`, `disclaimer`, `can_purchase_credits`, `can_toggle` |
| `seven_day_breakdown` | object | `as_of`, `window_started_at`, `rows[]{key, display_name, percent}`; keys seen: `claude_code`, `chat`, `cowork`, `other` |
| `member_dashboard_available` | bool | |
| `weekly_scoped_shares[]` | array | `limit_index` (index into `limits[]`), `used_percent_of_weekly`, `allowance_percent_of_weekly` |
| `cedar_ember` | object or null | Limit-reset status, see below |
| `seven_day_overage_included` | window or null | Weekly per-model bucket that Claude Code calls the "Fable limit" (also in the `anthropic-ratelimit-unified-*` response headers) |

`limits[]` entries:

| Field | Type | Values seen |
|-------|------|-------------|
| `kind` | string | `session`, `weekly_all`, `weekly_scoped` |
| `group` | string | `session`, `weekly` |
| `percent` | integer | 0-100 |
| `severity` | string | `normal` (others expected: warning/critical) |
| `resets_at` | RFC 3339 string or null | |
| `scope` | object or null | `model{id, display_name}`, `surface` |
| `is_active` | bool | Which limit currently binds |

Mapping to `limits::AccountRateLimits`: `session` -> `primary`
(300-minute window), `weekly_all` -> `secondary` (10080-minute window),
each `weekly_scoped` -> one entry in `buckets` named after
`scope.model.display_name`. `extra_usage`/`spend` replace the Codex
credits summary; `seven_day_breakdown` is a new "by surface" row.
`weekly_scoped_shares` sets `share_of_weekly` on the bucket of
`limits[limit_index]`.

`cedar_ember` (limit resets):

| Field | Type | Notes |
|-------|------|-------|
| `eligible` | bool | |
| `ineligible_reason` | string or null | `config_off`, `tier`, `seat`, `mobile`, `surface`, `cli_version`, `no_grant`, `tenure`, `other_experiment`, `unavailable`, `unknown`; `surface` was returned to a non-Claude-Code client |
| `at_limit` | bool | |
| `exhausted[]` | strings | Limit types at their cap |
| `grants[]` | array | `id`, `label`, `resets_total`, `resets_left`, `starts_at`, `ends_at`, `clears[]` (limit types: `five_hour`, `seven_day`, `seven_day_overage_included`, ...), `paused`, `usable_now`, `use_requires_limit`, `percent_used{}`, `blocking[]` |
| `next_grant_id`, `weekly_resets_at`, `cooldown_until` | string or null | |

A reset is used with `POST /api/organizations/{org}/reset_rate_limits`.
ClaudeMon never sends it: the user resets on claude.ai.

## S5. Status-line input (documented by Claude Code)

Claude Code pipes a JSON object to the `statusLine.command` on stdin. The
relevant part:

```
"rate_limits": {           // optional: subscribers, after first API response
  "five_hour":  { "used_percentage": number, "resets_at": number },
  "seven_day":  { "used_percentage": number, "resets_at": number },
  "spend_limit":{ "used_percentage": number, "resets_at": number }  // gateway only
}
```

`resets_at` is Unix epoch seconds; each window is present only while its
reset time has not passed. The same payload also carries model, session
cost, context-window usage, and `prompt_cache` health.

S5 needs no credentials, so it is the default source; S1 is opt-in and adds
per-model weekly limits, extra usage, the by-surface breakdown, and the
limit-reset status.

## S6. Claude Code's cached usage

Claude Code stores its last S1 response in its global config file
(`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`):

```
"cachedUsageUtilization": {
  "fetchedAtMs": number,       // Unix milliseconds
  "accountUuid": string,
  "utilization": { ... }       // the S1 response body
}
```

It is refreshed when Claude Code calls S1 itself (for example on `/usage`),
so it can be hours or days old. With the status-line source, claudemon
reads only this key and `oauthAccount.accountUuid` (a cache left by another
account is ignored) and takes the per-model limits, extra usage, and
by-surface breakdown from it; the newer of S5 and S6 supplies the 5-hour and
weekly windows.

## S7. Agent sessions (RECENT screen)

All read-only and local; checked 2026-11-07 with Claude Code 2.1.319,
Codex 0.167.0, opencode 1.18.35, and agy 1.5.2.

**Claude Code.** Transcripts `<claude-dir>/projects/<project>/<id>.jsonl`:
the first lines carry `sessionId`, `cwd`, `timestamp`, and `entrypoint`
(`cli` interactive, `sdk-cli` for `claude -p`, which is skipped); title
records (`custom-title`, `ai-title`, `summary`) can appear anywhere, and
transcripts can be hundreds of megabytes, so only the first 256 KiB and
the last 1 MiB are read. Running processes register in
`<claude-dir>/sessions/<pid>.json`: `pid`, `sessionId`, `cwd`, `startedAt`
and `updatedAt` (Unix ms), `procStart` (field 22 of `/proc/<pid>/stat`),
`name`, `nameSource` (`user` or `derived`), `status` (`busy`, `idle`,
`waiting`, `shell`), `waitingFor`. The files stay after the process exits;
a session is running only while the PID is alive with that `procStart`.
The `<pid>.<hash>.key` files next to them hold peer tokens and are never
read. Resume: `claude --resume <id>`, plus `--name <name>` for a user name.

**Codex.** `$CODEX_HOME/state_<n>.sqlite` (default `~/.codex`), table
`threads`: `id`, `cwd`, `name`, `title`, `created_at_ms`, `updated_at_ms`,
`source` (`cli`, `vscode`, `exec`, or JSON for subagent threads),
`thread_source` (`user`, `subagent`, `guardian_review`), `archived`.
Listed: `source` `cli`/`vscode`, `thread_source` `user` or null, not
archived. No registry of running sessions: a `codex` process (excluding
`app-server`, `exec`, `mcp`, ...) is matched by `resume <id>` in its
command line, else by its working directory. Resume: `codex resume <id>`.

**opencode.** `$XDG_DATA_HOME/opencode/opencode.db` (default
`~/.local/share`), table `session`: `id` (`ses_...`), `parent_id` (set for
subagents), `directory`, `title`, `time_created`, `time_updated`,
`time_archived` (Unix ms). Running: an `opencode` process (excluding `run`,
`serve`, `acp`, `web`, ...) matched by `-s <id>`, else by its working
directory. Resume: `opencode -s <id>`.

**Antigravity (`agy`).** `~/.gemini/antigravity-cli/conversation_summaries.db`,
table `conversation_summaries`: `conversation_id`, `title` (often empty),
`preview`, `last_modified_time` (`YYYY-MM-DD HH:MM:SS.f+00:00`),
`workspace_uris` (JSON array of `file://` URIs), `status`
(`CASCADE_RUN_STATUS_*`), `nesting_depth` (0 for top-level). Conversations
whose workspace is in the temporary directory (scripted runs) are skipped.
A running agy holds an exclusive `flock` on
`presence/<conversation-id>.lock`; the holder is read from `/proc/locks`
by inode, so claudemon takes no lock. Resume: `agy --conversation <id>`.

Matching by working directory (Codex, opencode) picks the session in that
directory that was active most recently since the process started; a
process that has not sent a message yet matches nothing.
