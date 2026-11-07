//! Opening a resumed Claude Code session in a new terminal window or tab,
//! so the claudemon TUI stays open, and opening web pages in the browser.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a launcher before treating it as detached.
const LAUNCH_WAIT: Duration = Duration::from_secs(5);

/// Where a resumed session opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Window,
    Tab,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launcher {
    /// `resume_terminal_command` / `resume_terminal_tab_command` from
    /// config.json, run through the shell.
    Custom {
        window: Option<String>,
        tab: Option<String>,
    },
    /// A new tmux window in the current tmux session (tmux's tab).
    Tmux,
    /// A new Windows Terminal window running the WSL distribution.
    WindowsTerminalWsl { wt: PathBuf, distro: String },
    /// A new Windows Terminal window on native Windows.
    WindowsTerminal { wt: PathBuf },
    /// A new Terminal.app window on macOS.
    MacTerminal,
}

impl Launcher {
    /// Picks a launcher: the configured templates, then tmux, then Windows
    /// Terminal (from WSL or native Windows), then Terminal.app on macOS.
    pub fn detect(custom_window: Option<&str>, custom_tab: Option<&str>) -> Option<Self> {
        let clean = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let (window, tab) = (clean(custom_window), clean(custom_tab));
        if window.is_some() || tab.is_some() {
            return Some(Self::Custom { window, tab });
        }
        if std::env::var_os("TMUX").is_some() && find_in_path("tmux").is_some() {
            return Some(Self::Tmux);
        }
        if cfg!(windows) {
            return find_in_path("wt.exe").map(|wt| Self::WindowsTerminal { wt });
        }
        if let Ok(distro) = std::env::var("WSL_DISTRO_NAME") {
            if !distro.trim().is_empty() {
                if let Some(wt) = find_in_path("wt.exe") {
                    return Some(Self::WindowsTerminalWsl { wt, distro });
                }
            }
        }
        if cfg!(target_os = "macos") {
            return Some(Self::MacTerminal);
        }
        None
    }

    /// Whether this launcher can open the session in `placement`.
    pub fn supports(&self, placement: Placement) -> bool {
        match (self, placement) {
            (Self::Custom { window, .. }, Placement::Window) => window.is_some(),
            (Self::Custom { tab, .. }, Placement::Tab) => tab.is_some(),
            (Self::Tmux, placement) => placement == Placement::Tab,
            (Self::WindowsTerminalWsl { .. } | Self::WindowsTerminal { .. }, _) => true,
            (Self::MacTerminal, placement) => placement == Placement::Window,
        }
    }

    /// Short name for the UI ("a new Windows Terminal tab").
    pub fn label(&self, placement: Placement) -> &'static str {
        match (self, placement) {
            (Self::Custom { .. }, Placement::Window) => "the configured terminal window",
            (Self::Custom { .. }, Placement::Tab) => "the configured terminal tab",
            (Self::Tmux, _) => "a new tmux window",
            (Self::WindowsTerminalWsl { .. } | Self::WindowsTerminal { .. }, Placement::Window) => {
                "a new Windows Terminal window"
            }
            (Self::WindowsTerminalWsl { .. } | Self::WindowsTerminal { .. }, Placement::Tab) => {
                "a new Windows Terminal tab"
            }
            (Self::MacTerminal, _) => "a new Terminal window",
        }
    }

    /// Program and arguments that open the session.
    fn command(
        &self,
        cwd: &Path,
        session_id: &str,
        placement: Placement,
    ) -> (PathBuf, Vec<String>) {
        let cwd_text = cwd.display().to_string();
        let title = window_title(cwd);
        // Windows Terminal: `-w new` opens a window, `-w 0` targets the most
        // recently used window (the one running claudemon) for a new tab.
        let wt_window = match placement {
            Placement::Window => "new",
            Placement::Tab => "0",
        };
        match self {
            Self::Custom { window, tab } => {
                let template = match placement {
                    Placement::Window => window.as_deref(),
                    Placement::Tab => tab.as_deref(),
                }
                .or(window.as_deref())
                .or(tab.as_deref())
                .unwrap_or_default();
                let expanded = expand_template(template, cwd, session_id);
                if cfg!(windows) {
                    (PathBuf::from("cmd"), vec!["/C".to_string(), expanded])
                } else {
                    (PathBuf::from("sh"), vec!["-c".to_string(), expanded])
                }
            }
            Self::Tmux => {
                let shell = user_shell();
                let script = shell_script(session_id, &shell);
                (
                    PathBuf::from("tmux"),
                    vec![
                        "new-window".to_string(),
                        "-c".to_string(),
                        cwd_text,
                        "-n".to_string(),
                        title,
                        format!("{shell} -ic {}", sh_quote(&script)),
                    ],
                )
            }
            Self::WindowsTerminalWsl { wt, distro } => {
                let shell = user_shell();
                let script = shell_script(session_id, &shell);
                (
                    wt.clone(),
                    vec![
                        "-w".to_string(),
                        wt_window.to_string(),
                        "new-tab".to_string(),
                        "--title".to_string(),
                        title,
                        "wsl.exe".to_string(),
                        "-d".to_string(),
                        distro.clone(),
                        "--cd".to_string(),
                        cwd_text,
                        "--".to_string(),
                        shell,
                        "-ic".to_string(),
                        script,
                    ],
                )
            }
            Self::WindowsTerminal { wt } => (
                wt.clone(),
                vec![
                    "-w".to_string(),
                    wt_window.to_string(),
                    "new-tab".to_string(),
                    "--title".to_string(),
                    title,
                    "-d".to_string(),
                    cwd_text,
                    "cmd".to_string(),
                    "/k".to_string(),
                    format!("claude --resume {session_id}"),
                ],
            ),
            Self::MacTerminal => {
                let script = format!("cd {} && claude --resume {session_id}", sh_quote(&cwd_text));
                (
                    PathBuf::from("osascript"),
                    vec![
                        "-e".to_string(),
                        format!(
                            "tell application \"Terminal\" to do script \"{}\"",
                            applescript_escape(&script)
                        ),
                        "-e".to_string(),
                        "tell application \"Terminal\" to activate".to_string(),
                    ],
                )
            }
        }
    }

    /// Opens the session. Blocking for at most a few seconds: launchers
    /// that keep running (for example a custom terminal command) are left
    /// to run on their own.
    pub fn open(&self, cwd: &Path, session_id: &str, placement: Placement) -> Result<()> {
        let (program, args) = self.command(cwd, session_id, placement);
        run_launcher(&program, &args, Some(cwd))
    }
}

/// Opens `url` in the default browser. Blocking for at most a few seconds.
pub fn open_url(url: &str) -> Result<()> {
    let (program, args) = url_opener(url).context("No program to open a browser was found")?;
    run_launcher(&program, &args, None)
}

/// Program and arguments that open `url`: the OS handler, `wslview` or the
/// Windows URL handler under WSL, else `xdg-open`.
fn url_opener(url: &str) -> Option<(PathBuf, Vec<String>)> {
    let windows_handler = |program: PathBuf| {
        (
            program,
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
        )
    };
    if cfg!(windows) {
        return Some(windows_handler(PathBuf::from("rundll32")));
    }
    if cfg!(target_os = "macos") {
        return Some((PathBuf::from("open"), vec![url.to_string()]));
    }
    if std::env::var_os("WSL_DISTRO_NAME").is_some() {
        if let Some(wslview) = find_in_path("wslview") {
            return Some((wslview, vec![url.to_string()]));
        }
        if let Some(rundll32) = find_in_path("rundll32.exe") {
            return Some(windows_handler(rundll32));
        }
    }
    find_in_path("xdg-open").map(|program| (program, vec![url.to_string()]))
}

/// Runs a launcher and reports a quick failure with its first stderr line.
/// A launcher still running after a few seconds is left to run on its own.
fn run_launcher(program: &Path, args: &[String], cwd: Option<&Path>) -> Result<()> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Unable to start {}", program.display()))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let detail = stderr.lines().next().unwrap_or("").trim();
            anyhow::bail!(
                "{} exited with {status}{}",
                program.display(),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        if started.elapsed() >= LAUNCH_WAIT {
            // Still running: let it continue and reap it in the background.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs Claude Code, then leaves an interactive shell in the project
/// directory. Avoids `;`, which Windows Terminal treats as a separator.
fn shell_script(session_id: &str, shell: &str) -> String {
    format!("claude --resume {session_id} || true && exec {shell} -i")
}

fn user_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| shell.starts_with('/') && !shell.contains(char::is_whitespace))
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn window_title(cwd: &Path) -> String {
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    let clean: String = name
        .chars()
        .filter(|ch| !ch.is_control() && *ch != ';' && *ch != '"')
        .collect();
    format!("Claude: {clean}")
}

/// Expands `{cwd}`, `{session_id}`, and `{command}` (the full resume
/// command) with shell-quoted values.
fn expand_template(template: &str, cwd: &Path, session_id: &str) -> String {
    let cwd_text = cwd.display().to_string();
    let (quoted_cwd, command) = if cfg!(windows) {
        let quoted = format!("\"{cwd_text}\"");
        let command = format!("cd /d {quoted} && claude --resume {session_id}");
        (quoted, command)
    } else {
        let quoted = sh_quote(&cwd_text);
        let command = format!("cd {quoted} && claude --resume {session_id}");
        (quoted, command)
    };
    let quoted_command = if cfg!(windows) {
        format!("\"{command}\"")
    } else {
        sh_quote(&command)
    };
    template
        .replace("{cwd}", &quoted_cwd)
        .replace("{session_id}", session_id)
        .replace("{command}", &quoted_command)
}

fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    const ID: &str = "00000000-0000-4000-8000-000000000001";

    fn custom(window: Option<&str>, tab: Option<&str>) -> Launcher {
        Launcher::Custom {
            window: window.map(str::to_string),
            tab: tab.map(str::to_string),
        }
    }

    #[test]
    fn windows_terminal_from_wsl_opens_a_window_or_a_tab_without_semicolons() {
        let launcher = Launcher::WindowsTerminalWsl {
            wt: PathBuf::from("/mnt/c/wt.exe"),
            distro: "Ubuntu".to_string(),
        };
        let cwd = Path::new("/home/user/my;app");
        let (program, window) = launcher.command(cwd, ID, Placement::Window);
        assert_eq!(program, PathBuf::from("/mnt/c/wt.exe"));
        assert_eq!(&window[..3], ["-w", "new", "new-tab"]);
        assert_eq!(window[4], "Claude: myapp");
        let wsl = window
            .iter()
            .position(|arg| arg == "wsl.exe")
            .expect("wsl.exe");
        assert_eq!(
            &window[wsl + 1..wsl + 5],
            ["-d", "Ubuntu", "--cd", "/home/user/my;app"]
        );
        let script = window.last().expect("script");
        assert!(script.starts_with(&format!("claude --resume {ID} || true && exec ")));
        assert!(!script.contains(';'));

        let (_, tab) = launcher.command(cwd, ID, Placement::Tab);
        assert_eq!(&tab[..3], ["-w", "0", "new-tab"]);
        assert_eq!(&tab[3..], &window[3..]);
        assert!(launcher.supports(Placement::Window) && launcher.supports(Placement::Tab));
        assert_eq!(launcher.label(Placement::Tab), "a new Windows Terminal tab");
    }

    #[test]
    fn tmux_opens_tabs_only() {
        let (program, args) = Launcher::Tmux.command(Path::new("/srv/app"), ID, Placement::Tab);
        assert_eq!(program, PathBuf::from("tmux"));
        assert_eq!(&args[..3], ["new-window", "-c", "/srv/app"]);
        assert!(args[5].contains(&format!("claude --resume {ID}")));
        assert!(Launcher::Tmux.supports(Placement::Tab));
        assert!(!Launcher::Tmux.supports(Placement::Window));
        assert!(!Launcher::MacTerminal.supports(Placement::Tab));
    }

    #[test]
    fn custom_templates_get_shell_quoted_values_per_placement() {
        let launcher = custom(
            Some("printf 'W|%s|%s|%s' {cwd} {session_id} {command}"),
            Some("printf 'T|%s' {cwd}"),
        );
        let cwd = Path::new("/home/user/it's my app");
        let run = |placement| {
            let (program, args) = launcher.command(cwd, ID, placement);
            let output = Command::new(program).args(args).output().expect("run sh");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        assert_eq!(
            run(Placement::Window),
            format!(
                "W|/home/user/it's my app|{ID}|cd '/home/user/it'\\''s my app' && claude --resume {ID}"
            )
        );
        assert_eq!(run(Placement::Tab), "T|/home/user/it's my app");

        let window_only = custom(Some("true"), None);
        assert!(window_only.supports(Placement::Window));
        assert!(!window_only.supports(Placement::Tab));
        assert_eq!(
            Launcher::detect(Some("  "), Some("")).is_some(),
            Launcher::detect(None, None).is_some()
        );
    }

    #[test]
    fn url_opener_passes_the_url_as_one_argument() {
        let url = "https://claude.ai/settings/usage";
        if let Some((_, args)) = url_opener(url) {
            assert_eq!(args.last().map(String::as_str), Some(url));
        }
    }

    #[test]
    fn open_reports_a_failing_launcher() {
        let error = custom(Some("echo nope >&2 && exit 3"), None)
            .open(Path::new("/"), ID, Placement::Window)
            .expect_err("launcher fails");
        assert!(error.to_string().contains("nope"), "{error}");
        custom(Some("true"), None)
            .open(Path::new("/"), ID, Placement::Window)
            .expect("launcher succeeds");
    }
}
