# Changelog

All notable changes to this project are documented in this file.

## Unreleased

- Imported the CoMon 0.5.5 code base (commit `1f94a82`) as the starting
  point for a Claude Code monitor.
- Renamed the crate, binary, state directory (`~/.claudemon`,
  `CLAUDEMON_HOME`), and cache database (`claudemon.db`).
- Replaced `--codex-home` with `--claude-dir` (default: `$CLAUDE_CONFIG_DIR`
  or `~/.claude`); the session history screen reads `<claude-dir>/projects`.
- Removed the Codex App Server client and the `--codex-bin`,
  `--app-server-bin`, and `--cwd` options. Live limits report "unavailable"
  until a Claude limit source is implemented.
