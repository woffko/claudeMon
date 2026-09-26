# ClaudeMon work plan

ClaudeMon is a single-binary, cross-platform TUI for Claude Code. Its code
base is imported from [CoMon](https://github.com/ssh4net/CoMon) (Codex
monitor, v0.5.5) as a clean
single-commit import without history: CoMon targets Codex, so ClaudeMon
evolves independently and does not track CoMon upstream.

Kept from CoMon: TUI framework and widgets, SQLite scan cache with
incremental offsets, locale formatting, themes, private-file hardening,
packaging scripts. Replaced: every Codex data source.

Goals:

1. **USAGE** - local token/time/run stats (7/30 days, chart, top models) from
   Claude Code session logs.
2. **APISTAT** - everything the Anthropic servers report: subscription
   limits, and (for API organizations) server-side daily usage and cost.
   Adds a **Models** view (per-model tokens-per-day chart and breakdown).
3. **COST** - API-equivalent cost of local usage, separate tab.
4. **Session history** - browser grouped by project path, with titles and
   prompt previews.

Naming:

| comon                        | claudemon                                   |
|------------------------------|---------------------------------------------|
| binary `comon`               | binary `claudemon`                          |
| display name `CoMon`         | display name `ClaudeMon`                    |
| `COMON_HOME`, `~/.comon`     | `CLAUDEMON_HOME`, `~/.claudemon`            |
| `comon.db`                   | `claudemon.db`                              |
| `--codex-home`, `CODEX_HOME` | `--claude-dir`, `CLAUDE_CONFIG_DIR`, default `~/.claude` |
| `--comon-home`               | `--claudemon-home`                          |
| `--codex-bin`, `--app-server-bin` | removed                                |

## Decisions

| # | Question | Decision |
|---|----------|----------|
| D1 | Repo strategy | Clean import of the CoMon tree, one commit, no upstream remote |
| D2 | Shared code with CoMon | None; independent copy |
| D3 | Token columns | Four: INPUT / CACHE-WRITE / CACHE-READ / OUTPUT |
| D4 | Server statistics | APISTAT kept and fed by every available server source (section 3) |
| D5 | Cost | Separate COST tab |
| D6 | Target account | Claude Max/Pro subscription first: S1 + S5 are the primary APISTAT sources; Admin API (S2-S4) is optional, later |

---

## 1. Source data: what Claude Code writes locally

Verified against Claude Code 2.1.283 on 2026-09-26 (format is undocumented and
can change between releases; the parser must be tolerant).

### Session transcripts

- Path: `<claude-dir>/projects/<project-slug>/<session-id>.jsonl`.
- `<project-slug>` is the cwd with `/` replaced by `-` (for example
  `-home-user-project`). The mapping is **lossy** (`-` in real names), so the
  project path must come from the `cwd` field of records, not from the slug.
- Sibling dir `<session-id>/` holds `tool-results/` and, in some versions,
  `subagents/*.jsonl`; subagent records carry `isSidechain: true`.
- One JSON object per line; `type` values seen: `assistant`, `user`,
  `attachment`, `system`, `last-prompt`, `ai-title`, `mode`,
  `permission-mode`, `queue-operation`, `file-history-snapshot`,
  `file-history-delta`, `cost-state`, `bridge-session`, `atis-latch`.

### Token usage (`type: "assistant"`)

`message.usage` fields: `input_tokens`, `cache_creation_input_tokens`,
`cache_read_input_tokens`, `output_tokens`,
`output_tokens_details.thinking_tokens`,
`cache_creation.ephemeral_{5m,1h}_input_tokens`,
`server_tool_use.web_{search,fetch}_requests`, `service_tier`, `speed`.
Also `message.model`, `timestamp`, `cwd`, `gitBranch`, `sessionId`,
`requestId`, `version`, `isSidechain`.

**Critical:** one API response is written as several lines (one per content
block), each repeating the same usage. In sampled logs fewer than half of the
assistant lines had a unique `(message.id, requestId)` pair. Usage must be deduplicated by that
key, or totals are inflated roughly 2-3x.

Skip `message.model == "<synthetic>"` (local placeholder messages).

### Session cost summary (`type: "cost-state"`)

Written by Claude Code itself: `totalCostUSD`, `totalAPIDuration`,
`totalDuration`, `totalLinesAdded/Removed`, and per-model `modelUsage`
(input/output/thinking/cache tokens, `costUSD`). Cross-check for our token
sums and a cost source for COST. Not present in every session.

### Titles and previews

- `ai-title.aiTitle` - generated session title (take the latest).
- `last-prompt.lastPrompt` - last user prompt.
- First meaningful `user` message text (skip tool results, `isMeta`,
  command wrappers) as a fallback preview.

### Other files (read-only, optional)

- `<claude-dir>/stats-cache.json` - Claude Code's own aggregates, the data
  behind its `/stats` screen: `dailyActivity` (messages, sessions, tool
  calls per day), `dailyModelTokens` (tokens per model per day),
  `modelUsage` (per-model input/output/cache read/cache write totals),
  `hourCounts`, `longestSession`, `totalSessions`. **This is local data, not
  server data.** Used as a cross-check and as a fast fallback for the
  Models view.
- `<claude-dir>/history.jsonl` - prompt history (`display`, `timestamp`,
  `project`, `sessionId`).
- `<claude-dir>/sessions/<pid>.json` - running sessions (`cwd`, `status`,
  `startedAt`); could power a "live sessions" indicator later.

---

## 2. Token columns (D3)

| Column      | Source field(s)                    | Note |
|-------------|------------------------------------|------|
| INPUT       | `input_tokens`                     | uncached input |
| CACHE-WRITE | `cache_creation_input_tokens`      | 5m/1h split kept in cache DB for COST |
| CACHE-READ  | `cache_read_input_tokens`          | |
| OUTPUT      | `output_tokens`                    | thinking included; thinking share in detail views |

Bar lengths and summary totals = sum of all four (matches Claude Code's
own "tokens" figure in `/stats`). Same four categories are returned by the
Admin API usage report, so local and server numbers line up column by
column.

---

## 3. Server-side statistics (APISTAT)

What the Anthropic servers expose depends on the account type. ClaudeMon
auto-detects which sources are configured and shows each with its own
source badge.

| Source | Account | Official | Data | Auth |
|--------|---------|----------|------|------|
| S1. OAuth usage endpoint (used by Claude Code `/usage`) | Pro / Max / Team subscription | No (undocumented) | Current utilization % and reset time for the 5-hour window, 7-day window, per-model 7-day windows, extra-usage credits | Claude Code OAuth token |
| S2. Usage report `GET /v1/organizations/usage_report/messages` | API (Console) organization | Yes (Admin API) | Daily (or hourly) buckets of uncached input, cache read, cache write 5m/1h, output; group by model, workspace, API key, service tier | Admin API key `sk-ant-admin...` |
| S3. Cost report `GET /v1/organizations/cost_report` | API (Console) organization | Yes (Admin API) | Daily USD cost grouped by description (model, token type, service tier) | Admin API key |
| S4. Claude Code analytics endpoint | API / Enterprise organization | To verify in Phase 0 | Per-user daily Claude Code metrics | Admin API key |
| S5. Status-line bridge (`claudemon statusline`) | Any | Yes (documented statusLine hook) | Whatever Claude Code passes to the status line (model, session cost, rate-limit fields if present in the running version) | None |

Conclusions:

- For a **subscription** there is no official, documented server API. The
  only server source of limits is S1, so S1 is implemented, opt-in
  (`--live-limits oauth` / config), with S5 as the secret-free complement.
  There is no server-side daily usage history for subscriptions at all.
- For an **API organization**, S2 and S3 are the official, complete source
  of server statistics. Per D6 they are implemented after S1/S5, as an
  optional source.
- Admin API keys are not available for individual accounts.

Secret handling (S1-S4):

- S1: token read from `<claude-dir>/.credentials.json` (Linux/Windows) or
  the macOS Keychain at request time; never logged, cached, or written.
- S2-S4: admin key only from the environment variable
  `CLAUDEMON_ADMIN_KEY` (never from `config.json`, never persisted).
- All responses parsed strictly; on schema change the source shows
  "unavailable" instead of wrong numbers.
- Refresh intervals respect server rate limits (S2/S3 at most every few
  minutes; data is daily anyway).

### APISTAT layout

Sub-views, switched with a small tab row (like Claude Code `/stats`):

1. **Overview** - CoMon's existing APISTAT layout: live limit gauges (5h,
   weekly, per-model weekly, with CoMon's fair-share and weekly-pace
   coloring), server total cards, extra-usage/credits summary.
2. **Models** - modelled on Claude Code's `/stats` -> Models screen:
   - "Tokens per Day" chart: one line per model, y-axis with compact
     units (192.3M, 1.5B), x-axis date labels, colored legend
     (`* Opus 5.5 * Opus 5 * Sonnet 5`).
   - Range selector: All time / Last 7 days / Last 30 days (`d` cycles,
     since `r` refreshes everywhere; mouse clickable).
   - Per-model cards in two columns: name, share %,
     `In: ... Out: ...`, `Cache: ... read ... write`.
   - Data source: S2 when an admin key is configured (badge `SERVER`),
     otherwise local logs (badge `LOCAL`), so the view is useful for
     subscription users too.
   - Optional: copy the visible table as text (`ctrl+s`, as in Claude Code).

---

## 4. Phases

### Phase 0 - Spike (1 day)

- [ ] Fixture generator: turn real transcripts into **structure-only**
      fixtures (usage numbers kept, all text replaced), covering multi-line
      responses, subagents, `<synthetic>`, `cost-state`, truncated last line,
      very long lines.
- [ ] Capture S1 response shape from the current Claude Code (field names
      only, no token in any artifact).
- [ ] Check which status-line fields the current Claude Code provides (S5).
- [ ] Confirm S2/S3/S4 request and response schemas from the official docs.

### Phase 1 - Import and rename (1 day)

- [x] Import CoMon tree at `1f94a82` (without `.codex/`,
      `history_refactoring.md`, `review.md`, screenshots).
- [x] Rename crate (v0.1.0), binary, env vars, paths, display strings.
- [x] Remove `src/codex_rpc`; data types moved to `src/limits`; the limits
      worker reports "unavailable" until Phase 4 adds sources.
- [x] Fresh `CHANGELOG.md`; keep `LICENSE` (Apache-2.0); add `NOTICE`
      crediting the original CoMon authors (ssh4net/CoMon).
- [x] `cargo check`, `cargo test` (167 passed), `scripts/check-ascii.sh` pass.
- [x] First commit.

### Phase 2 - USAGE from Claude Code logs (done)

- [x] Discovery: every `*.jsonl` under `<claude-dir>/projects`, including
      `<session>/subagents/*.jsonl`.
- [x] Parser for `assistant` records with dedupe by
      `(message.id, requestId)`. Verified on real logs: duplicate lines
      carry identical usage and are always contiguous, so the last key is
      persisted in the parser state and survives incremental resume.
- [x] Unterminated last line (record still being written) is left for the
      next refresh instead of being consumed.
- [x] Attribution: project = cwd of the first record (launch directory);
      later `cd` in tool calls never re-homes a session.
- [x] Four token columns (section 2) in chart, cards, and cache.
      Cache hit rate = cache reads / (input + cache writes + cache reads).
- [x] Runs = distinct `promptId` (one per turn, including background task
      notifications). Time = gap estimate per turn, corrected to the exact
      `system/turn_duration.durationMs` when the turn records it.
- [x] Model names: `claude-opus-5-5` -> "Opus 5.5".
- [x] Fresh cache schema v1; any schema mismatch rebuilds rows.
- [x] Claude Code deletes transcripts after `cleanupPeriodDays` (30 by
      default): cache rows of deleted transcripts are kept and still
      counted, so claudemon history outlives the raw logs.
- [x] Cross-check against an independent Python reference on real logs:
      runs identical, tokens within the live-session growth between runs.
- [ ] Config `exclude_project_globs` (for example `/tmp/<agent-wrapper>-*`).
- [ ] Optional import of `stats-cache.json` history older than the oldest
      transcript (per-model daily tokens only).

### Phase 3 - Session history (done)

- [x] Adapt `src/read/scan.rs`: session meta from the first records
      (`sessionId`, `gitBranch`, `version`, timestamp, first model); title
      from `custom-title`, else the latest `ai-title`, else `summary`, else
      the first meaningful user prompt (Claude Code wrappers such as
      `<command-...>`, `<local-command...>`, reminders, and task
      notifications are skipped). Title records after the header window are
      found with a cheap substring prefilter.
- [x] Group by project path from the launch cwd; Strict/Deep/Full catalog
      logic kept, evidence extraction remapped to Claude `tool_use` blocks
      (`Bash`, `Read`, `Edit`, `MultiEdit`, `Write`, `NotebookEdit`, `Glob`,
      `Grep`, `LS`) with relative paths resolved against the record cwd.
- [x] Subagent transcripts nested under their parent session (count in the
      list, file names in the detail); orphans are listed on their own.
- [x] Session detail: client version, deduplicated four-column tokens,
      tool calls/results, thinking blocks, user turns.
- [x] Optional: show `claude --resume <session-id>` for the selected session.

### Phase 4 - APISTAT (done)

- [x] Limit sources in `src/limits`, selected by `--live-limits` /
      config `live_limits` (`statusline` default, `oauth`, `off`) and
      polled by the limits worker:
      S5 status-line bridge `claudemon statusline [--wrap <cmd>]`
      (snapshot in `~/.claudemon/limits.json`, 0600, atomic write, about
      30 ms, always exit 0); S1 OAuth usage endpoint (opt-in, token read per
      request and never stored, `limits[]` preferred over fixed keys,
      at most one request per minute).
- [ ] Optional, after S1/S5: `ServerUsageSource` for S2 + S3 (+ S4 if
      confirmed) with pagination, backoff, per-day cache in `claudemon.db`.
- [x] Overview sub-view (LIMITS): 5h, weekly, and per-model weekly gauges
      with the weekly fair-share marker and reset countdowns, binding limit,
      extra usage, weekly usage by surface, source and capture age. The
      Codex reset-credit screen, button, and dialog were removed; the USAGE
      limits card shows the 5h/weekly windows and the most used model limit.
- [x] Models sub-view (section 3): multi-series Braille line chart per
      model, legend, All time / 7 / 30 days selector (`d`, mouse), two-column
      model cards (share, in/out, cache read/write), `LOCAL` source badge,
      `ctrl+s` copies the table to the terminal clipboard (OSC 52). Backed by
      per-model daily four-column usage in the scan cache (schema v2).
- [x] Graceful states: setup instructions until a status-line snapshot
      exists, errors kept next to the last good data, stale status-line
      snapshots (> 15 min) flagged, expired windows shown without a value.

### Phase 5 - COST (done)

- [x] Pricing table in `config.json` (`pricing`: per model input, output,
      cache write 5m, cache write 1h, cache read in USD per million tokens)
      seeded with the published rates, `pricing_updated` date; model ids
      match exactly, without a date suffix, or by longest family prefix;
      unknown models are listed as unpriced. Validated against Claude
      Code's own `cost-state` records to the cent (Opus 5.5 with 1-hour
      cache writes, Haiku 4.5 with 5-minute writes), which required
      tracking the 1-hour share of cache writes (cache schema v4).
- [x] Per day / per model (input, cache write, cache read, output) / per
      project cost for all time, 7, or 30 days, with Claude Code's recorded
      session costs compared against the same transcripts priced up to the
      record (usage after the last record is excluded).
- [ ] Optional (depends on the optional S3 source): show server cost next
      to the local estimate.
- [x] Clear labeling: "API-equivalent at list prices", the price table
      date, and that subscription usage is not billed per token.

### Phase 6 - UI polish (done)

- [x] Tab bar: USAGE / APISTAT / COST / ACTIVITY / HISTORY (CoMon's
      ACTIVITY heatmaps kept); help overlays, footer key hints, and header
      (`claudemon :: <version>`) updated; no Codex wording left; `ctrl+s` no
      longer triggers the global screen switch.
- [x] Default accent theme is orange (close to Claude's brand color)
      instead of CoMon's cyan.
- [x] Empty states: USAGE explains where transcripts are expected and how
      to point claudemon elsewhere; the LIMITS card says "Not set up" and
      points to APISTAT, which shows the status-line setup instructions.
- [x] Classic / System Compact / System Full verified on USAGE, COST, and
      MODELS with real data (`1,234,567,890` / `1.23B` / `1 234 567 890`).
- [x] Clippy is clean with no warnings (the warnings inherited from CoMon
      were fixed as well).

### Phase 7 - Packaging and release (1 day)

- [ ] Rename install/package scripts.
- [ ] CI: ascii check + `cargo test` + clippy on Linux/macOS/Windows.
- [ ] README with screenshots from synthetic data only.
- [ ] Tag `v0.1.0`, musl + macOS + Windows zips.

**Total estimate: ~14-18 working days.**

---

## 5. Testing

- Unit tests on synthetic fixtures only (no real prompts, paths, ids, or
  tokens in the repo).
- Golden tests: dedupe, incremental scan resumed mid-response, truncated
  final line, subagent attribution, `<synthetic>` skip, unknown record types.
- Recorded-shape tests for S1-S4 responses (synthetic values).
- Manual: compare 30-day totals with Claude Code `/stats` and, for API
  orgs, with the Console usage page.

## 6. Privacy and security

- Persist metadata and aggregates only; never prompt or completion text.
- `0700` on `CLAUDEMON_HOME`, `0600` on its files, symlink refusal.
- Credentials never stored by ClaudeMon; OAuth mode is opt-in; admin key
  only via environment.
- Status-line bridge never breaks Claude Code's status line.

## 7. Risks

| Risk | Mitigation |
|------|------------|
| Undocumented JSONL format changes | Tolerant parser, ignore unknown types, log `version`, fixture per observed version |
| Duplicate usage lines | Per-request rows keyed by `(message.id, requestId)` |
| S1 endpoint is undocumented and may change or be restricted | Opt-in, strict parsing, "unavailable" state, S5 as fallback |
| Subscription has no server usage history | Models view falls back to local data with a `LOCAL` badge |
| Pricing drift for COST | Pricing in config with update date; unknown models flagged |
| Lossy project slug | Always use record `cwd` |
