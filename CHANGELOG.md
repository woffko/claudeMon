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
- Usage statistics now come from Claude Code transcripts
  (`<claude-dir>/projects/**/*.jsonl`, including subagent transcripts),
  with usage deduplicated per API response.
- The Usage chart shows four token columns: INPUT / CACHE-WRITE /
  CACHE-READ / OUTPUT. Cache hit rate is cache reads over all prompt tokens.
- Runs count turns (distinct prompt ids); agent time uses Claude Code's
  recorded turn durations when available.
- Usage history is kept for transcripts that Claude Code has already
  deleted (`cleanupPeriodDays`).
- Model names are shown as "Opus 5.5", "Sonnet 5", and so on.
- Session history reads Claude Code transcripts: titles from custom or
  AI-generated titles, the Claude Code version, the first model, and
  subagent transcripts nested under their parent session.
- Session detail shows deduplicated four-column token usage, thinking
  blocks, and the `claude --resume` command for the session.
- Deep/Full project discovery uses Claude Code tool calls as evidence.
