pub(crate) mod catalog;
pub(crate) mod scan;
pub(crate) mod tui;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) sessions_dir: PathBuf,
}

pub(crate) fn print_sessions_dir(
    claude_dir: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<()> {
    let config = build_config(claude_dir, sessions_dir)?;
    println!("{}", config.sessions_dir.display());
    Ok(())
}

pub(crate) fn build_config(
    claude_dir: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<Config> {
    let sessions_dir = resolve_sessions_dir(claude_dir, sessions_dir)?;
    Ok(Config { sessions_dir })
}

pub(crate) fn build_browser(config: &Config) -> Result<tui::BrowserState> {
    let catalog = scan::build_catalog(&config.sessions_dir)?;
    Ok(tui::BrowserState::new(catalog))
}

fn resolve_sessions_dir(
    claude_dir: Option<PathBuf>,
    sessions_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = sessions_dir {
        return validate_dir(&path, "--sessions-dir");
    }

    let claude_dir = crate::usage::resolve_claude_dir(claude_dir)
        .context("Unable to resolve the Claude Code config directory (default: ~/.claude)")?;
    Ok(claude_dir.join("projects"))
}

fn validate_dir(path: &Path, label: &str) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).with_context(|| format!("{label} does not exist"))?;
    if !meta.is_dir() {
        anyhow::bail!("{label} must be a directory");
    }
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}
