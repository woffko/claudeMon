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

State and cache live in `~/.claudemon` (override with `CLAUDEMON_HOME` or
`--claudemon-home`).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
