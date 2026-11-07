//! RECENT screen data: agent sessions used recently and whether each one is
//! running now. Each agent has its own reader; readers keep per-file
//! summaries between scans so unchanged session files are not read again.
//! Only local files and `/proc` are read; nothing is sent anywhere.

pub(crate) mod agy;
pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod opencode;
pub(crate) mod proc;

use crate::launch::ResumeCommand;
use proc::ProcessInfo;
use std::path::{Path, PathBuf};

/// At most this many sessions are read from each agent's index.
pub(crate) const MAX_SESSIONS_PER_AGENT: usize = 2000;

/// A summary read longer ago than this is read again when its file changes.
/// Titles change rarely, and a running session's file changes constantly.
pub(crate) const SUMMARY_REFRESH_SECS: i64 = 5 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Agent {
    Claude,
    Codex,
    OpenCode,
    Antigravity,
}

impl Agent {
    /// Command name, shown in the AGENT column.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Antigravity => "agy",
        }
    }
}

/// The process of a running session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveProcess {
    pub(crate) pid: u32,
    /// Terminal, for example `pts/3`.
    pub(crate) tty: Option<String>,
    /// Agent-reported state (`busy`, `idle`, `waiting`, `shell`), if any.
    pub(crate) status: Option<String>,
    /// What a waiting session waits for, if reported.
    pub(crate) waiting_for: Option<String>,
}

/// Whether a session is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionState {
    Running(LiveProcess),
    Stopped,
    /// This platform cannot tell.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecentSession {
    pub(crate) agent: Agent,
    pub(crate) session_id: String,
    pub(crate) cwd: Option<PathBuf>,
    /// Short name: the session name, else its title, else the directory.
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    /// Unix seconds.
    pub(crate) started_at: Option<i64>,
    /// Unix seconds of the last recorded activity.
    pub(crate) last_active: i64,
    pub(crate) state: SessionState,
    pub(crate) resume: Option<ResumeCommand>,
}

impl RecentSession {
    pub(crate) fn is_running(&self) -> bool {
        matches!(self.state, SessionState::Running(_))
    }
}

/// Reads every supported agent's sessions.
pub(crate) struct Scanner {
    claude: claude::ClaudeReader,
    codex_home: Option<PathBuf>,
    opencode_db: Option<PathBuf>,
    agy_dir: Option<PathBuf>,
}

impl Scanner {
    pub(crate) fn new(claude_dir: PathBuf) -> Self {
        let env_dir = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = env_dir("HOME").or_else(|| env_dir("USERPROFILE"));
        Self {
            claude: claude::ClaudeReader::new(claude_dir),
            codex_home: env_dir("CODEX_HOME")
                .or_else(|| home.as_ref().map(|home| home.join(".codex"))),
            opencode_db: env_dir("XDG_DATA_HOME")
                .or_else(|| home.as_ref().map(|home| home.join(".local").join("share")))
                .map(|data| data.join("opencode").join("opencode.db")),
            agy_dir: home.map(|home| home.join(".gemini").join("antigravity-cli")),
        }
    }

    /// Every session found, most recently active first; the screen filters
    /// them by period. Blocking.
    pub(crate) fn scan(&mut self, now: i64) -> Vec<RecentSession> {
        let mut sessions = self.claude.scan(now);
        if let Some(home) = self.codex_home.as_deref() {
            sessions.extend(codex::scan(home));
        }
        if let Some(db) = self.opencode_db.as_deref() {
            sessions.extend(opencode::scan(db));
        }
        if let Some(dir) = self.agy_dir.as_deref() {
            sessions.extend(agy::scan(dir));
        }
        sessions.sort_by(|left, right| {
            right
                .last_active
                .cmp(&left.last_active)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        sessions
    }
}

/// State of a session that no process was matched to.
pub(crate) fn unmatched_state() -> SessionState {
    if proc::CAN_LIST_PROCESSES {
        SessionState::Stopped
    } else {
        SessionState::Unknown
    }
}

/// Marks sessions of one agent as running. A process that names its session
/// on the command line (`explicit`) gets that session; any other is matched
/// by working directory to the session there that was active most recently
/// since the process started.
pub(crate) fn mark_running(
    sessions: &mut [RecentSession],
    mut processes: Vec<(ProcessInfo, Option<String>)>,
) {
    processes.sort_by_key(|(_, explicit)| explicit.is_none());
    for (process, explicit) in processes {
        let target = match explicit {
            Some(id) => sessions
                .iter()
                .position(|session| session.session_id == id && !session.is_running()),
            None => {
                let started = process.started_at.unwrap_or(i64::MIN);
                sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, session)| {
                        !session.is_running()
                            && session.cwd.is_some()
                            && session.cwd == process.cwd
                            && session.last_active >= started
                    })
                    .max_by_key(|(_, session)| session.last_active)
                    .map(|(index, _)| index)
            }
        };
        if let Some(index) = target {
            sessions[index].state = SessionState::Running(LiveProcess {
                pid: process.pid,
                tty: proc::tty(process.pid),
                status: None,
                waiting_for: None,
            });
        }
    }
}

/// Opens an agent's SQLite database read-only, or `None` when it is missing
/// or cannot be opened.
pub(crate) fn open_read_only(path: &Path) -> Option<rusqlite::Connection> {
    if !path.is_file() {
        return None;
    }
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    connection
        .busy_timeout(std::time::Duration::from_millis(250))
        .ok()?;
    Some(connection)
}

/// One-line title of at most 96 characters.
pub(crate) fn short_title(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| crate::read::scan::truncate_single_line(text, 96))
}

/// Value after `flag` (`-s ID`, `--session ID`, or `--session=ID`).
pub(crate) fn flag_value(argv: &[String], flags: &[&str]) -> Option<String> {
    for (index, arg) in argv.iter().enumerate().skip(1) {
        for flag in flags {
            if arg == flag {
                return argv
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .cloned();
            }
            if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Last path component of a directory, for a session without a name.
pub(crate) fn directory_name(cwd: Option<&std::path::Path>) -> Option<String> {
    cwd?.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}
