//! Process checks for the RECENT screen. Linux reads `/proc`; other Unix
//! systems can only tell whether a PID exists; elsewhere the state is
//! unknown.

/// Whether this platform can tell running sessions from stopped ones.
pub(crate) const CAN_CHECK: bool = cfg!(unix);
/// Whether `processes` can list processes (needed for agents that keep no
/// session registry).
pub(crate) const CAN_LIST_PROCESSES: bool = cfg!(target_os = "linux");

/// Whether a recorded agent process is still the one running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    Running,
    Gone,
    #[cfg_attr(unix, allow(dead_code))]
    Unknown,
}

/// Checks `pid`. With `start_ticks` (the process start time in clock ticks
/// since boot, field 22 of `/proc/<pid>/stat`), a reused PID counts as gone.
#[cfg(target_os = "linux")]
pub(crate) fn liveness(pid: u32, start_ticks: Option<&str>) -> Liveness {
    match (start_time_ticks(pid), start_ticks) {
        (None, _) => Liveness::Gone,
        (Some(actual), Some(expected)) if actual != expected.trim() => Liveness::Gone,
        (Some(_), _) => Liveness::Running,
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn liveness(pid: u32, _start_ticks: Option<&str>) -> Liveness {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return Liveness::Gone;
    };
    // SAFETY: signal 0 only checks that the process exists.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
        Liveness::Running
    } else {
        Liveness::Gone
    }
}

#[cfg(not(unix))]
pub(crate) fn liveness(_pid: u32, _start_ticks: Option<&str>) -> Liveness {
    Liveness::Unknown
}

/// Field 22 (`starttime`) of `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn start_time_ticks(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) is in parentheses and may contain spaces.
    let after_name = stat.rsplit_once(')')?.1;
    after_name.split_whitespace().nth(19).map(str::to_string)
}

/// Terminal of a process ("pts/3"), from its standard input.
#[cfg(target_os = "linux")]
pub(crate) fn tty(pid: u32) -> Option<String> {
    let target = std::fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    let text = target.to_str()?;
    text.strip_prefix("/dev/")
        .filter(|name| name.starts_with("pts/") || name.starts_with("tty"))
        .map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn tty(_pid: u32) -> Option<String> {
    None
}

/// A running process, for agents that keep no registry of their sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: u32,
    /// Executable name (`/proc/<pid>/comm`).
    pub(crate) comm: String,
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: Option<std::path::PathBuf>,
    /// Unix seconds.
    pub(crate) started_at: Option<i64>,
}

/// Processes of the current user named one of `names`. Linux only; other
/// systems report none.
#[cfg(target_os = "linux")]
pub(crate) fn processes(names: &[&str]) -> Vec<ProcessInfo> {
    let boot = boot_time();
    // SAFETY: sysconf has no preconditions.
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue;
        };
        let comm = comm.trim_end().to_string();
        if !names.contains(&comm.as_str()) {
            continue;
        }
        let argv = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let started_at = match (boot, start_time_ticks(pid)) {
            (Some(boot), Some(ticks)) if ticks_per_second > 0 => ticks
                .parse::<i64>()
                .ok()
                .map(|ticks| boot + ticks / ticks_per_second as i64),
            _ => None,
        };
        found.push(ProcessInfo {
            pid,
            comm,
            argv,
            cwd: std::fs::read_link(format!("/proc/{pid}/cwd")).ok(),
            started_at,
        });
    }
    found
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn processes(_names: &[&str]) -> Vec<ProcessInfo> {
    Vec::new()
}

/// Boot time in Unix seconds (`btime` in `/proc/stat`).
#[cfg(target_os = "linux")]
fn boot_time() -> Option<i64> {
    std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse().ok())
}

/// PIDs holding a `flock` on each inode, from `/proc/locks`. Reading it
/// takes no lock itself, so it cannot disturb the agent holding one.
#[cfg(target_os = "linux")]
pub(crate) fn flock_holders() -> std::collections::HashMap<u64, u32> {
    let mut holders = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string("/proc/locks") else {
        return holders;
    };
    for line in text.lines() {
        // "3: FLOCK  ADVISORY  WRITE 55439 08:30:1129944 0 EOF"
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(1) != Some(&"FLOCK") || fields.len() < 6 {
            continue;
        }
        let (Ok(pid), Some(inode)) = (
            fields[4].parse::<u32>(),
            fields[5]
                .rsplit(':')
                .next()
                .and_then(|inode| inode.parse::<u64>().ok()),
        ) else {
            continue;
        };
        holders.insert(inode, pid);
    }
    holders
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn flock_holders() -> std::collections::HashMap<u64, u32> {
    std::collections::HashMap::new()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_running_and_a_wrong_start_time_is_gone() {
        let pid = std::process::id();
        let ticks = start_time_ticks(pid).expect("own start time");
        assert_eq!(liveness(pid, Some(&ticks)), Liveness::Running);
        assert_eq!(liveness(pid, None), Liveness::Running);
        assert_eq!(liveness(pid, Some("1")), Liveness::Gone);
        assert_eq!(liveness(u32::MAX - 1, None), Liveness::Gone);
    }
}
