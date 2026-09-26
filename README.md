# claudemon

Single-binary, cross-platform TUI monitor for Claude Code: local usage stats,
server-side account statistics and subscription limits, API-equivalent cost,
and a session-history browser.

Derived from [CoMon](https://github.com/ssh4net/CoMon) (Codex monitor).

Status: early development. The code base has been renamed and the Codex App
Server client removed; Claude Code log parsing and limit sources are in
progress. See [PLAN.md](PLAN.md) and [CHANGELOG.md](CHANGELOG.md).

## Build

```bash
cargo build --release
```

Requires Rust 1.88+ and a C compiler (bundled SQLite).

## Run

```bash
claudemon                      # Usage screen, all workspaces
claudemon --read               # Session history screen
claudemon --claude-dir <path>  # Override ~/.claude (or set CLAUDE_CONFIG_DIR)
claudemon --live-limits off    # Disable live limits
```

## Live limits

Claude subscriptions (Pro/Max) have no documented limits API, so claudemon
offers two sources.

**Status line (default, no credentials).** Claude Code passes the current
5-hour and weekly limits to its status-line command. Point it at claudemon
in `~/.claude/settings.json`:

```json
{
  "statusLine": { "type": "command", "command": "claudemon statusline" }
}
```

To keep an existing status line, wrap it; its output is shown unchanged:

```bash
claudemon statusline --wrap 'your-status-line-command'
```

The snapshot is stored in `~/.claudemon/limits.json` and updates while
Claude Code runs.

**OAuth usage endpoint (opt-in).** `claudemon --live-limits oauth` (or
`"live_limits": "oauth"` in `~/.claudemon/config.json`) calls the endpoint
behind Claude Code's `/usage` screen with Claude Code's OAuth token. It adds
per-model weekly limits, extra usage, and weekly usage by surface. The
endpoint is undocumented and may change; the token is read per request and
never stored or refreshed by claudemon.

## Screens

- **USAGE** - tokens (input / cache write / cache read / output), agent
  time, and runs per day from `~/.claude/projects` transcripts.
- **APISTAT** - `v` switches LIMITS (live limits) and MODELS (tokens per day
  per model; `d` cycles all time / 7 days / 30 days; `ctrl+s` copies the
  table).
- **ACTIVITY** - per-project activity heatmaps.
- **HISTORY** - sessions grouped by project, with titles, token usage, and
  the `claude --resume` command.

Usage history is kept in claudemon's cache even after Claude Code deletes old
transcripts (`cleanupPeriodDays`, 30 days by default).

State and cache live in `~/.claudemon` (override with `CLAUDEMON_HOME` or
`--claudemon-home`).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
