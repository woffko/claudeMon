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

Per-model weekly limits (for example the separate Fable limit) are not in
the status-line input. claudemon adds them from the copy of the last
`/usage` response that Claude Code keeps in its own config file
(`cachedUsageUtilization` in `~/.claude.json`), shown with its age. Claude
Code refreshes that copy when it calls the usage endpoint, for example on
`/usage`.

**OAuth usage endpoint (opt-in).** `claudemon --live-limits oauth` (or
`"live_limits": "oauth"` in `~/.claudemon/config.json`) calls the endpoint
behind Claude Code's `/usage` screen with Claude Code's OAuth token. It adds
fresh per-model weekly limits, extra usage, weekly usage by surface, and the
limit-reset status (resets left, the date to use them by, and which limits
a reset refills). The
endpoint is undocumented and may change; the token is read per request and
never stored or refreshed by claudemon.

## Screens

- **USAGE** - tokens (input / cache write / cache read / output), agent
  time, and runs per day from `~/.claude/projects` transcripts.
- **APISTAT** - `v` switches LIMITS (live limits) and MODELS (tokens per day
  per model; `d` cycles all time / 7 days / 30 days; `ctrl+s` copies the
  table). On LIMITS, `o` or the CLAUDE.AI button opens
  claude.ai/settings/usage, where limit resets are used; claudemon itself
  never uses a reset.
- **COST** - API-equivalent cost at list prices by day, model, and project
  (`d` cycles dates). Prices are in `~/.claudemon/config.json` under
  `pricing` (USD per million tokens) and can be edited.
- **ACTIVITY** - per-project activity heatmaps.
- **HISTORY** - sessions grouped by project, with titles and token usage.
  The Resume box shows the command for the selected session (for example
  `cd ~/project && claude --resume <id>`). Enter or double-click opens the
  session in a new terminal window, `t` in a new tab of the current window;
  claudemon stays open. `i` runs it in the current terminal and returns when
  Claude Code exits; `y` copies the command. Windows Terminal (WSL or
  Windows) supports windows and tabs, tmux opens a tmux window (tab), and
  Terminal.app (macOS) opens a window. Other terminals can be set in
  `~/.claudemon/config.json` with `resume_terminal_command` (window) and
  `resume_terminal_tab_command` (tab), for example
  `"gnome-terminal --working-directory={cwd} -- bash -ic {command}"`
  (`{cwd}`, `{session_id}`, and `{command}` are shell-quoted).
- **RECENT** - coding-agent sessions used in the last day, week, or month
  (`d`), with whether each one is running now: Claude Code, Codex,
  opencode, and Antigravity (`agy`). Running sessions show their status
  (busy / idle / waiting / shell for Claude Code), PID, and terminal; a
  stopped one has a RUN button. RUN or `t` resumes it in a new terminal tab
  (a window when the terminal has no tabs), Enter or double-click in a new
  window, with the agent's own command
  (`claude --resume`, `codex resume`, `opencode -s`,
  `agy --conversation`) in the session's directory; `y` copies it. The list
  is re-read every 15 seconds while the screen is shown. Only local files
  and `/proc` are read; no requests are made and no tokens are used.

Usage history is kept in claudemon's cache even after Claude Code deletes old
transcripts (`cleanupPeriodDays`, 30 days by default).

State and cache live in `~/.claudemon` (override with `CLAUDEMON_HOME` or
`--claudemon-home`).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
