//! opencode sessions from its database (`opencode.db`, table `session`).
//! opencode keeps no registry of running sessions, so a running `opencode`
//! process is matched by `-s <id>` on its command line or by its working
//! directory.

use super::{
    directory_name, flag_value, mark_running, open_read_only, proc, short_title, unmatched_state,
    Agent, RecentSession, MAX_SESSIONS_PER_AGENT,
};
use crate::launch::ResumeCommand;
use std::path::{Path, PathBuf};

/// Subcommands that are not an interactive session.
const NON_INTERACTIVE: [&str; 6] = ["run", "serve", "acp", "web", "mcp", "auth"];

pub(crate) fn scan(db: &Path) -> Vec<RecentSession> {
    let Some(connection) = open_read_only(db) else {
        return Vec::new();
    };
    // Top-level sessions only: subagent sessions have a parent.
    let Ok(mut statement) = connection.prepare(
        "SELECT id, directory, title, time_created, time_updated FROM session \
         WHERE parent_id IS NULL AND time_archived IS NULL \
         ORDER BY time_updated DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let rows = statement.query_map([MAX_SESSIONS_PER_AGENT as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut sessions: Vec<RecentSession> = rows
        .flatten()
        .map(|(id, directory, title, created_ms, updated_ms)| {
            let cwd = directory.filter(|dir| !dir.is_empty()).map(PathBuf::from);
            let title = title.as_deref().and_then(short_title);
            let name = title
                .clone()
                .or_else(|| directory_name(cwd.as_deref()))
                .unwrap_or_else(|| id.chars().take(12).collect());
            RecentSession {
                agent: Agent::OpenCode,
                resume: ResumeCommand::new(
                    "opencode",
                    &id,
                    vec!["opencode".to_string(), "-s".to_string(), id.clone()],
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

    let processes = proc::processes(&["opencode"])
        .into_iter()
        .filter(|process| {
            process
                .argv
                .get(1)
                .is_none_or(|first| !NON_INTERACTIVE.contains(&first.as_str()))
        })
        .map(|process| {
            let explicit = flag_value(&process.argv, &["-s", "--session"]);
            (process, explicit)
        })
        .collect();
    mark_running(&mut sessions, processes);
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_table_lists_top_level_sessions() {
        let dir = std::env::temp_dir().join(format!("claudemon-opencode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        let connection = rusqlite::Connection::open(&db).expect("db");
        connection
            .execute_batch(
                "CREATE TABLE session (id TEXT, parent_id TEXT, directory TEXT, title TEXT, \
                 time_created INTEGER, time_updated INTEGER, time_archived INTEGER);
                 INSERT INTO session VALUES ('ses_top', NULL, '/work/app', 'Plan the release', \
                   1000000, 2000000000000, NULL);
                 INSERT INTO session VALUES ('ses_child', 'ses_top', '/work/app', 'subtask', \
                   1000000, 2000000000000, NULL);
                 INSERT INTO session VALUES ('ses_archived', NULL, '/work/app', 'old', \
                   1000000, 2000000000000, 5);",
            )
            .expect("schema");
        drop(connection);

        let sessions = scan(&db);
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        assert_eq!(sessions[0].name, "Plan the release");
        assert_eq!(sessions[0].cwd.as_deref(), Some(Path::new("/work/app")));
        assert_eq!(
            sessions[0].resume.as_ref().map(|resume| resume.text()),
            Some("opencode -s ses_top".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
