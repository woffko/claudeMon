//! Claude Code sessions: transcripts under `<claude-dir>/projects` and the
//! registry Claude Code keeps for its running processes
//! (`<claude-dir>/sessions/<pid>.json`: session id, cwd, name, status).
//! Registry files stay behind after a process exits, so a session counts as
//! running only while its PID is alive with the recorded start time
//! (`procStart`). The `.key` files next to them hold peer tokens and are
//! never opened.

use super::{
    directory_name, proc, Agent, LiveProcess, RecentSession, SessionState, MAX_AGE_SECS,
    SUMMARY_REFRESH_SECS,
};
use crate::launch::ResumeCommand;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAX_REGISTRY_FILE_BYTES: u64 = 64 * 1024;
/// Transcripts can be hundreds of megabytes, so only the start (session id,
/// cwd, entrypoint) and the end (the latest title records) are read.
const HEAD_BYTES: u64 = 256 * 1024;
const TAIL_BYTES: u64 = 1024 * 1024;
const MAX_TITLE_CHARS: usize = 96;

#[derive(Debug, Clone, Deserialize)]
struct RegistryEntry {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: Option<String>,
    /// Unix milliseconds.
    #[serde(rename = "startedAt")]
    started_at: Option<i64>,
    #[serde(rename = "procStart")]
    proc_start: Option<String>,
    name: Option<String>,
    /// `user` (set with `--name` or `/rename`) or `derived`.
    #[serde(rename = "nameSource")]
    name_source: Option<String>,
    status: Option<String>,
    #[serde(rename = "waitingFor")]
    waiting_for: Option<String>,
    /// Unix milliseconds.
    #[serde(rename = "updatedAt")]
    updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct Registered {
    entry: RegistryEntry,
    state: SessionState,
}

#[derive(Debug, Clone)]
struct TranscriptInfo {
    cwd: Option<String>,
    title: Option<String>,
    started_at_ms: i64,
    /// `false` for scripted runs (`claude -p`, entrypoint `sdk-cli`).
    interactive: bool,
}

#[derive(Debug, Clone)]
struct CachedTranscript {
    len: u64,
    modified: i64,
    scanned_at: i64,
    info: Option<TranscriptInfo>,
}

pub(crate) struct ClaudeReader {
    claude_dir: PathBuf,
    transcripts: HashMap<PathBuf, CachedTranscript>,
}

impl ClaudeReader {
    pub(crate) fn new(claude_dir: PathBuf) -> Self {
        Self {
            claude_dir,
            transcripts: HashMap::new(),
        }
    }

    pub(crate) fn scan(&mut self, now: i64) -> Vec<RecentSession> {
        let registry = read_registry(&self.claude_dir.join("sessions"));
        let files = list_transcripts(&self.claude_dir.join("projects"), now - MAX_AGE_SECS);
        let mut sessions = Vec::new();
        let mut listed = HashSet::new();
        for (path, len, modified) in &files {
            let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Some(info) = self.transcript_info(path, *len, *modified, now) else {
                continue;
            };
            if !info.interactive {
                continue;
            }
            listed.insert(session_id.to_string());
            sessions.push(session(
                session_id,
                Some(&info),
                *modified,
                registry.get(session_id),
            ));
        }
        // Running sessions without a transcript yet (no message sent).
        for (session_id, registered) in &registry {
            if !listed.contains(session_id) && registered.state != SessionState::Stopped {
                sessions.push(session(session_id, None, 0, Some(registered)));
            }
        }
        let files: HashSet<&PathBuf> = files.iter().map(|(path, _, _)| path).collect();
        self.transcripts.retain(|path, _| files.contains(path));
        sessions
    }

    fn transcript_info(
        &mut self,
        path: &Path,
        len: u64,
        modified: i64,
        now: i64,
    ) -> Option<TranscriptInfo> {
        if let Some(cached) = self.transcripts.get(path) {
            let unchanged = cached.len == len && cached.modified == modified;
            if unchanged || now - cached.scanned_at < SUMMARY_REFRESH_SECS {
                return cached.info.clone();
            }
        }
        let info = read_transcript_info(path, len);
        self.transcripts.insert(
            path.to_path_buf(),
            CachedTranscript {
                len,
                modified,
                scanned_at: now,
                info: info.clone(),
            },
        );
        info
    }
}

fn session(
    session_id: &str,
    info: Option<&TranscriptInfo>,
    modified: i64,
    registered: Option<&Registered>,
) -> RecentSession {
    let entry = registered.map(|registered| &registered.entry);
    let state = match registered {
        Some(registered) => registered.state.clone(),
        None if proc::CAN_CHECK => SessionState::Stopped,
        None => SessionState::Unknown,
    };
    let cwd = entry
        .and_then(|entry| entry.cwd.clone())
        .or_else(|| info.and_then(|info| info.cwd.clone()))
        .map(PathBuf::from);
    let title = info.and_then(|info| info.title.clone());
    let registry_name = entry
        .and_then(|entry| entry.name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let name = registry_name
        .map(str::to_string)
        .or_else(|| title.clone())
        .or_else(|| directory_name(cwd.as_deref()))
        .unwrap_or_else(|| session_id.chars().take(8).collect());
    let user_name = registry_name
        .filter(|_| entry.and_then(|entry| entry.name_source.as_deref()) == Some("user"));
    let running = matches!(state, SessionState::Running(_));
    let process_start = entry.and_then(|entry| entry.started_at).map(|ms| ms / 1000);
    let transcript_start = info
        .map(|info| info.started_at_ms / 1000)
        .filter(|seconds| *seconds > 0);
    let started_at = if running {
        process_start.or(transcript_start)
    } else {
        transcript_start.or(process_start)
    };
    let registry_active = entry
        .and_then(|entry| entry.updated_at)
        .map(|ms| ms / 1000)
        .unwrap_or(0);
    RecentSession {
        agent: Agent::Claude,
        session_id: session_id.to_string(),
        cwd,
        name,
        title,
        started_at,
        last_active: modified.max(registry_active),
        state,
        resume: ResumeCommand::claude(session_id, user_name),
    }
}

/// Session metadata from the start and the end of a transcript.
fn read_transcript_info(path: &Path, len: u64) -> Option<TranscriptInfo> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.by_ref().take(HEAD_BYTES).read_to_end(&mut head).ok()?;
    let mut info = TranscriptInfo {
        cwd: None,
        title: None,
        started_at_ms: 0,
        interactive: true,
    };
    let mut titles = Titles::default();
    let head_complete = len <= HEAD_BYTES;
    for line in complete_lines(&head, false, !head_complete) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let text = |key: &str| value.get(key).and_then(Value::as_str);
        if info.cwd.is_none() {
            info.cwd = text("cwd")
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string);
        }
        if info.started_at_ms == 0 {
            if let Some(timestamp) = text("timestamp") {
                info.started_at_ms = chrono::DateTime::parse_from_rfc3339(timestamp)
                    .map(|parsed| parsed.timestamp_millis())
                    .unwrap_or(0);
            }
        }
        if text("entrypoint") == Some("sdk-cli") {
            info.interactive = false;
        }
        titles.observe(&value);
    }
    if !head_complete {
        let start = len.saturating_sub(TAIL_BYTES).max(HEAD_BYTES);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut tail = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
        for line in complete_lines(&tail, start > HEAD_BYTES, false) {
            if is_title_line(line) {
                if let Ok(value) = serde_json::from_slice::<Value>(line) {
                    titles.observe(&value);
                }
            }
        }
    }
    info.title = titles.best();
    Some(info)
}

/// Latest title of each kind; a title set by the user wins.
#[derive(Default)]
struct Titles {
    custom: Option<String>,
    ai: Option<String>,
    summary: Option<String>,
}

impl Titles {
    fn observe(&mut self, value: &Value) {
        let field = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| crate::read::scan::truncate_single_line(text, MAX_TITLE_CHARS))
        };
        match value.get("type").and_then(Value::as_str) {
            Some("custom-title") => self.custom = field("customTitle").or(self.custom.take()),
            Some("ai-title") => self.ai = field("aiTitle").or(self.ai.take()),
            Some("summary") => self.summary = field("summary").or(self.summary.take()),
            _ => {}
        }
    }

    fn best(self) -> Option<String> {
        self.custom.or(self.ai).or(self.summary)
    }
}

fn is_title_line(line: &[u8]) -> bool {
    const MARKERS: [&[u8]; 3] = [
        b"\"type\":\"custom-title\"",
        b"\"type\":\"ai-title\"",
        b"\"type\":\"summary\"",
    ];
    MARKERS
        .iter()
        .any(|marker| line.windows(marker.len()).any(|window| window == *marker))
}

/// Lines of `bytes`, leaving out a partial first line (when the buffer
/// starts mid-file) and a partial last line (when the file goes on).
fn complete_lines(bytes: &[u8], skip_first: bool, skip_last: bool) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    if skip_last || !bytes.ends_with(b"\n") {
        lines.pop();
    }
    if skip_first && !lines.is_empty() {
        lines.remove(0);
    }
    lines.retain(|line| !line.is_empty());
    lines
}

/// Registry entries by session id. A session resumed several times has
/// several files; the running one wins, then the most recently updated.
fn read_registry(dir: &Path) -> HashMap<String, Registered> {
    let mut sessions: HashMap<String, Registered> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return sessions;
    };
    for dir_entry in entries.flatten() {
        let path = dir_entry.path();
        // Only `<pid>.json`; the `.key` files are never read.
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() || meta.len() > MAX_REGISTRY_FILE_BYTES {
            continue;
        }
        let Some(entry) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RegistryEntry>(&bytes).ok())
        else {
            continue;
        };
        let state = match proc::liveness(entry.pid, entry.proc_start.as_deref()) {
            proc::Liveness::Running => SessionState::Running(LiveProcess {
                pid: entry.pid,
                tty: proc::tty(entry.pid),
                status: entry.status.clone(),
                waiting_for: entry.waiting_for.clone(),
            }),
            proc::Liveness::Gone => SessionState::Stopped,
            proc::Liveness::Unknown => SessionState::Unknown,
        };
        let candidate = Registered { entry, state };
        let rank = |registered: &Registered| {
            let state_rank = match registered.state {
                SessionState::Running(_) => 2,
                SessionState::Unknown => 1,
                SessionState::Stopped => 0,
            };
            (state_rank, registered.entry.updated_at.unwrap_or(0))
        };
        match sessions.get(&candidate.entry.session_id) {
            Some(existing) if rank(existing) >= rank(&candidate) => {}
            _ => {
                sessions.insert(candidate.entry.session_id.clone(), candidate);
            }
        }
    }
    sessions
}

/// Top-level transcripts (`<projects>/<project>/<session-id>.jsonl`)
/// modified at or after `since`, with their size and modification time.
fn list_transcripts(projects: &Path, since: i64) -> Vec<(PathBuf, u64, i64)> {
    let mut files = Vec::new();
    let Ok(projects) = std::fs::read_dir(projects) else {
        return files;
    };
    for project in projects.flatten() {
        if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0);
            if modified >= since {
                files.push((path, meta.len(), modified));
            }
        }
    }
    files
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    const RUNNING: &str = "00000000-0000-4000-8000-00000000000a";
    const STOPPED: &str = "00000000-0000-4000-8000-00000000000b";
    const SCRIPTED: &str = "00000000-0000-4000-8000-00000000000c";
    const NO_TRANSCRIPT: &str = "00000000-0000-4000-8000-00000000000d";

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claudemon-recent-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn transcript(dir: &Path, session_id: &str, entrypoint: &str, title: Option<&str>) {
        let mut lines = vec![serde_json::json!({
            "type": "user", "sessionId": session_id, "cwd": "/work/app",
            "entrypoint": entrypoint, "timestamp": "2026-11-01T10:00:00Z",
            "message": {"role": "user", "content": "synthetic prompt"}
        })];
        if let Some(title) = title {
            lines.push(serde_json::json!({"type": "custom-title", "customTitle": title}));
        }
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(dir.join(format!("{session_id}.jsonl")), text).expect("transcript");
    }

    #[test]
    fn transcripts_and_registry_give_names_states_and_resume_commands() {
        let claude_dir = temp_dir("claude");
        let project = claude_dir.join("projects").join("-work-app");
        let registry = claude_dir.join("sessions");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::create_dir_all(&registry).expect("registry dir");
        transcript(&project, RUNNING, "cli", Some("Fix the parser"));
        transcript(&project, STOPPED, "cli", None);
        transcript(&project, SCRIPTED, "sdk-cli", None);

        let own_pid = std::process::id();
        let entry = |pid: u32, session_id: &str, name: &str, source: &str, status: &str| {
            serde_json::json!({
                "pid": pid, "sessionId": session_id, "cwd": "/work/app",
                "startedAt": 1_793_000_000_000_i64, "name": name, "nameSource": source,
                "status": status, "updatedAt": 1_793_000_100_000_i64, "kind": "interactive"
            })
            .to_string()
        };
        std::fs::write(
            registry.join(format!("{own_pid}.json")),
            entry(own_pid, RUNNING, "parser", "user", "busy"),
        )
        .expect("registry");
        std::fs::write(
            registry.join("4294967294.json"),
            entry(u32::MAX - 1, STOPPED, "old-name", "derived", "idle"),
        )
        .expect("registry");
        std::fs::write(
            registry.join("4294967293.json"),
            entry(u32::MAX - 2, NO_TRANSCRIPT, "empty", "derived", "idle"),
        )
        .expect("registry");
        std::fs::write(
            registry.join("1.secret.key"),
            "{\"peerToken\": \"synthetic\"}",
        )
        .expect("key file");

        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        let mut reader = ClaudeReader::new(claude_dir.clone());
        let mut sessions = reader.scan(now);
        sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(
            ids,
            [RUNNING, STOPPED],
            "scripted and empty stopped sessions are left out"
        );

        let running = &sessions[0];
        assert_eq!(running.name, "parser");
        assert_eq!(running.title.as_deref(), Some("Fix the parser"));
        assert_eq!(running.cwd.as_deref(), Some(Path::new("/work/app")));
        assert_eq!(running.started_at, Some(1_793_000_000));
        match &running.state {
            SessionState::Running(process) => {
                assert_eq!(process.pid, own_pid);
                assert_eq!(process.status.as_deref(), Some("busy"));
            }
            other => panic!("expected running, got {other:?}"),
        }
        assert_eq!(
            running.resume.as_ref().map(|resume| resume.text()),
            Some(format!("claude --resume {RUNNING} --name parser"))
        );

        let stopped = &sessions[1];
        assert_eq!(stopped.state, SessionState::Stopped);
        assert_eq!(stopped.name, "old-name");
        assert_eq!(
            stopped.resume.as_ref().map(|resume| resume.text()),
            Some(format!("claude --resume {STOPPED}")),
            "derived names are not passed to --name"
        );
        assert_eq!(stopped.started_at, Some(1_793_527_200), "transcript start");

        // A second scan reuses the cached summaries.
        assert_eq!(reader.scan(now).len(), 2);
        assert_eq!(reader.transcripts.len(), 3);
        let _ = std::fs::remove_dir_all(claude_dir);
    }
}
