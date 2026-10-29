//! Where the tool keeps its files, and how it writes the private ones.
//!
//! `$XDG_CONFIG_HOME/slack`, falling back to `~/.config/slack`, like gh-prs
//! and slackcli. The macOS convention would be `~/Library/Application
//! Support`; neither of them uses it, and one place is easier to find.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::{env, io::ErrorKind};

use anyhow::{Context, Result};

pub fn config_dir() -> Result<PathBuf> {
    base_dir("XDG_CONFIG_HOME", ".config")
}

/// Caches, safe to delete: names and the last view.
pub fn cache_dir() -> Result<PathBuf> {
    base_dir("XDG_CACHE_HOME", ".cache")
}

/// What a run leaves to be read later, not safe to delete: the watch log.
pub fn state_dir() -> Result<PathBuf> {
    base_dir("XDG_STATE_HOME", ".local/state")
}

fn base_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    let base = match env::var_os(var).filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => env::home_dir()
            .context("neither $HOME nor the XDG base directory variables are set")?
            .join(fallback),
    };
    Ok(base.join("slack"))
}

/// Make `path` readable by its owner only (no-op outside Unix).
pub fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Write `contents` to `path`, owner-readable from the first byte: a file
/// created with the default mode and tightened afterwards is world-readable
/// in between.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    create_private(path)?.write_all(contents)
}

/// Create or truncate `path` for writing, owner-readable from the first
/// byte, as `write_private` does for contents written in one go.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    // The mode above applies on creation only; an existing file keeps its own.
    restrict(path, 0o600)?;
    Ok(file)
}

/// Open `path` to add to its end, owner-readable from the first byte.
pub fn append_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    restrict(path, 0o600)?;
    Ok(file)
}

/// Whether the error says the file was simply not there.
pub fn is_not_found(err: &io::Error) -> bool {
    err.kind() == ErrorKind::NotFound
}
