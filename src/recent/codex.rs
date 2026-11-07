//! Codex CLI sessions from its thread index (`$CODEX_HOME/state_<n>.sqlite`,
//! table `threads`). Codex keeps no registry of running sessions, so a
//! running `codex` process is matched by the id on its command line
//! (`codex resume <id>`) or by its working directory.

use super::{
    directory_name, mark_running, open_read_only, proc, short_title, unmatched_state, Agent,
    RecentSession, MAX_AGE_SECS,
};
use crate::launch::ResumeCommand;
use std::path::{Path, PathBuf};

/// Subcommands that are not an interactive session.
const NON_INTERACTIVE: [&str; 9] = [
    "app-server",
    "exec",
    "e",
    "mcp",
    "mcp-server",
    "login",
    "logout",
    "apply",
    "sandbox",
];

pub(crate) fn scan(codex_home: &Path, now: i64) -> Vec<RecentSession> {
    let Some(connection) = latest_state_db(codex_home).and_then(|db| open_read_only(&db)) else {
        return Vec::new();
    };
    // Interactive threads only: subagent and review threads have a JSON
    // `source`, `exec` runs are scripted.
    let Ok(mut statement) = connection.prepare(
        "SELECT id, cwd, name, title, created_at_ms, updated_at_ms FROM threads \
         WHERE archived = 0 AND source IN ('cli', 'vscode') \
         AND (thread_source IS NULL OR thread_source = 'user') AND updated_at_ms >= ?1 \
         ORDER BY updated_at_ms DESC LIMIT 500",
    ) else {
        return Vec::new();
    };
    let rows = statement.query_map([(now - MAX_AGE_SECS) * 1000], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut sessions: Vec<RecentSession> = rows
        .flatten()
        .map(|(id, cwd, name, title, created_ms, updated_ms)| {
            let cwd = cwd.filter(|cwd| !cwd.is_empty()).map(PathBuf::from);
            let title = title.as_deref().and_then(short_title);
            let name = name
                .as_deref()
                .and_then(short_title)
                .or_else(|| title.clone())
                .or_else(|| directory_name(cwd.as_deref()))
                .unwrap_or_else(|| id.chars().take(8).collect());
            RecentSession {
                agent: Agent::Codex,
                resume: ResumeCommand::new(
                    "Codex",
                    &id,
                    vec!["codex".to_string(), "resume".to_string(), id.clone()],
                ),
                session_id: id,
                cwd,
                name,
                title,
                started_at: created_ms.map(|ms| ms / 1000),
                last_active: updated_ms.map(|ms| ms / 1000).unwrap_or(0),
                state: unmatched_state(),
            }
        })
        .collect();

    let processes = proc::processes(&["codex"])
        .into_iter()
        .filter(|process| {
            !process
                .argv
                .iter()
                .skip(1)
                .any(|arg| NON_INTERACTIVE.contains(&arg.as_str()))
        })
        .map(|process| {
            let explicit = resumed_id(&process.argv);
            (process, explicit)
        })
        .collect();
    mark_running(&mut sessions, processes);
    sessions
}

/// The newest `state_<n>.sqlite` in the Codex home.
fn latest_state_db(codex_home: &Path) -> Option<PathBuf> {
    std::fs::read_dir(codex_home)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let version: u32 = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

/// Session id of `codex resume <id>`.
fn resumed_id(argv: &[String]) -> Option<String> {
    let position = argv.iter().position(|arg| arg == "resume")?;
    argv.get(position + 1)
        .filter(|value| !value.starts_with('-'))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn resumed_id_is_read_from_the_command_line() {
        assert_eq!(
            resumed_id(&args(&["codex", "resume", "0190-abc"])),
            Some("0190-abc".to_string())
        );
        assert_eq!(resumed_id(&args(&["codex", "resume", "--last"])), None);
        assert_eq!(resumed_id(&args(&["codex"])), None);
    }

    #[test]
    fn threads_index_lists_interactive_sessions() {
        let dir = std::env::temp_dir().join(format!("claudemon-codex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let db = dir.join("state_5.sqlite");
        let _ = std::fs::remove_file(&db);
        let connection = rusqlite::Connection::open(&db).expect("db");
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT, cwd TEXT, name TEXT, title TEXT, \
                 created_at_ms INTEGER, updated_at_ms INTEGER, source TEXT, \
                 thread_source TEXT, archived INTEGER);
                 INSERT INTO threads VALUES ('t-cli', '/work/app', 'refactor', 'Refactor the parser', \
                   1000000, 2000000000000, 'cli', 'user', 0);
                 INSERT INTO threads VALUES ('t-sub', '/work/app', NULL, 'helper', \
                   1000000, 2000000000000, '{\"subagent\":{}}', 'subagent', 0);
                 INSERT INTO threads VALUES ('t-exec', '/work/app', NULL, 'scripted', \
                   1000000, 2000000000000, 'exec', 'user', 0);
                 INSERT INTO threads VALUES ('t-old', '/work/app', NULL, 'old', \
                   1000000, 1000000, 'cli', 'user', 0);",
            )
            .expect("schema");
        drop(connection);

        let sessions = scan(&dir, 2_000_000_000);
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        let session = &sessions[0];
        assert_eq!(session.agent, Agent::Codex);
        assert_eq!(session.name, "refactor");
        assert_eq!(session.title.as_deref(), Some("Refactor the parser"));
        assert_eq!(session.last_active, 2_000_000_000);
        assert_eq!(
            session.resume.as_ref().map(|resume| resume.text()),
            Some("codex resume t-cli".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
