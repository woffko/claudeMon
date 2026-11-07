//! Antigravity CLI (`agy`) conversations from its index
//! (`~/.gemini/antigravity-cli/conversation_summaries.db`). A running agy
//! holds an exclusive `flock` on `presence/<conversation-id>.lock`; the
//! holder is read from `/proc/locks`, so no lock is taken here.

use super::{
    directory_name, open_read_only, proc, short_title, unmatched_state, Agent, LiveProcess,
    RecentSession, SessionState, MAX_SESSIONS_PER_AGENT,
};
use crate::launch::ResumeCommand;
use std::path::{Path, PathBuf};

pub(crate) fn scan(agy_dir: &Path) -> Vec<RecentSession> {
    let Some(connection) = open_read_only(&agy_dir.join("conversation_summaries.db")) else {
        return Vec::new();
    };
    // Top-level conversations only: subagents have a nesting depth.
    let Ok(mut statement) = connection.prepare(
        "SELECT conversation_id, title, preview, last_modified_time, workspace_uris, status \
         FROM conversation_summaries WHERE nesting_depth IS NULL OR nesting_depth = 0 \
         ORDER BY last_modified_time DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let rows = statement.query_map([MAX_SESSIONS_PER_AGENT as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let holders = proc::flock_holders();
    let temp_dir = std::env::temp_dir();
    rows.flatten()
        .filter_map(|(id, title, preview, modified, workspaces, status)| {
            let last_active = modified.as_deref().and_then(parse_time).unwrap_or(0);
            let cwd = workspaces.as_deref().and_then(first_workspace);
            // Conversations in temporary workspaces are scripted runs.
            if cwd.as_deref().is_some_and(|cwd| cwd.starts_with(&temp_dir)) {
                return None;
            }
            let state = match lock_holder(agy_dir, &id, &holders) {
                Some(pid) => SessionState::Running(LiveProcess {
                    pid,
                    tty: proc::tty(pid),
                    status: status.as_deref().and_then(run_status),
                    waiting_for: None,
                }),
                None => unmatched_state(),
            };
            let title = title
                .as_deref()
                .and_then(short_title)
                .or_else(|| preview.as_deref().and_then(short_title));
            let name = title
                .clone()
                .or_else(|| directory_name(cwd.as_deref()))
                .unwrap_or_else(|| id.chars().take(8).collect());
            let started_at =
                std::fs::metadata(agy_dir.join("conversations").join(format!("{id}.db")))
                    .and_then(|meta| meta.created())
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs() as i64);
            Some(RecentSession {
                agent: Agent::Antigravity,
                resume: ResumeCommand::new(
                    "agy",
                    &id,
                    vec!["agy".to_string(), "--conversation".to_string(), id.clone()],
                ),
                session_id: id,
                cwd,
                name,
                title,
                started_at,
                last_active,
                state,
            })
        })
        .collect()
}

/// PID holding the presence lock of a conversation.
#[cfg(unix)]
fn lock_holder(
    agy_dir: &Path,
    id: &str,
    holders: &std::collections::HashMap<u64, u32>,
) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(agy_dir.join("presence").join(format!("{id}.lock"))).ok()?;
    holders.get(&meta.ino()).copied()
}

#[cfg(not(unix))]
fn lock_holder(
    _agy_dir: &Path,
    _id: &str,
    _holders: &std::collections::HashMap<u64, u32>,
) -> Option<u32> {
    None
}

/// `2026-11-06 18:29:23.475681611+00:00` (the fraction may be missing) as
/// Unix seconds.
fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%:z"]
        .iter()
        .find_map(|format| chrono::DateTime::parse_from_str(text, format).ok())
        .or_else(|| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|parsed| parsed.timestamp())
}

/// First `file://` URI of a JSON array, as a path.
fn first_workspace(json: &str) -> Option<PathBuf> {
    let uris: Vec<String> = serde_json::from_str(json).ok()?;
    let path = uris.first()?.strip_prefix("file://")?;
    let decoded = percent_decode(path)?;
    decoded.starts_with('/').then(|| PathBuf::from(decoded))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `CASCADE_RUN_STATUS_RUNNING` -> `busy`, `..._IDLE` -> `idle`.
fn run_status(status: &str) -> Option<String> {
    match status.strip_prefix("CASCADE_RUN_STATUS_")? {
        "RUNNING" | "BUSY" | "CANCELING" => Some("busy".to_string()),
        "IDLE" => Some("idle".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_parsed() {
        assert_eq!(
            parse_time("2001-09-09 01:46:40.475681611+00:00"),
            Some(1_000_000_000)
        );
        assert_eq!(parse_time("2001-09-09 01:46:40+00:00"), Some(1_000_000_000));
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(
            first_workspace(r#"["file:///home/user/my%20app", "file:///other"]"#),
            Some(PathBuf::from("/home/user/my app"))
        );
        assert_eq!(first_workspace("[]"), None);
        assert_eq!(
            run_status("CASCADE_RUN_STATUS_IDLE").as_deref(),
            Some("idle")
        );
        assert_eq!(run_status("CASCADE_RUN_STATUS_UNSPECIFIED"), None);
    }

    #[test]
    fn index_lists_top_level_conversations_outside_temp_dirs() {
        let dir = std::env::temp_dir().join(format!("claudemon-agy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let db = dir.join("conversation_summaries.db");
        let _ = std::fs::remove_file(&db);
        let temp = std::env::temp_dir().join("scripted");
        let connection = rusqlite::Connection::open(&db).expect("db");
        connection
            .execute_batch(&format!(
                "CREATE TABLE conversation_summaries (conversation_id TEXT, title TEXT, \
                 preview TEXT, last_modified_time TEXT, workspace_uris TEXT, status TEXT, \
                 nesting_depth INTEGER);
                 INSERT INTO conversation_summaries VALUES ('c-top', '', 'Review the build', \
                   '2001-09-09 01:46:40.0+00:00', '[\"file:///work/app\"]', \
                   'CASCADE_RUN_STATUS_IDLE', 0);
                 INSERT INTO conversation_summaries VALUES ('c-sub', 'sub', '', \
                   '2001-09-09 01:46:40.0+00:00', '[\"file:///work/app\"]', NULL, 1);
                 INSERT INTO conversation_summaries VALUES ('c-tmp', 'tmp', '', \
                   '2001-09-09 01:46:40.0+00:00', '[\"file://{}\"]', NULL, 0);",
                temp.display()
            ))
            .expect("schema");
        drop(connection);

        let sessions = scan(&dir);
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        let session = &sessions[0];
        assert_eq!(session.agent, Agent::Antigravity);
        assert_eq!(
            session.name, "Review the build",
            "preview stands in for an empty title"
        );
        assert_eq!(session.last_active, 1_000_000_000);
        assert_eq!(session.state, unmatched_state());
        assert_eq!(
            session.resume.as_ref().map(|resume| resume.text()),
            Some("agy --conversation c-top".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
