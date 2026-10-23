//! The screen's `u` on a file: a copy in the cache, either the preview the
//! screen draws or the file itself for the system viewer. The copies are
//! private (owner-only, like the credentials) and dropped a week after
//! they were fetched, so what was posted in a private channel does not
//! pile up on disk.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::api::Client;
use crate::history::File;
use crate::paths;

/// How long a copy is kept: long enough for a file looked at again the
/// same week, short enough that the cache is not an archive of Slack.
const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Which copy of a file (not `Copy`, which would hide the trait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// The preview of an image, drawn in the messages pane.
    Thumb,
    /// The file as posted, for the system viewer.
    Original,
}

/// The copy on disk, fetched unless it is there already.
pub fn fetch(client: &Client, file: &File, version: Version) -> Result<PathBuf> {
    let url = match version {
        Version::Thumb => file.thumb.as_deref(),
        Version::Original => file.url.as_deref(),
    }
    .context("Slack gives no link to this file")?;
    let dir = dir()?;
    let path = dir.join(relative(file, version)?);
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > 0) {
        return Ok(path);
    }
    let parent = path.parent().unwrap_or(&dir);
    for dir in [&dir, parent] {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
        paths::restrict(dir, 0o700)?;
    }
    // Written aside and renamed, so a broken download is never taken for
    // the file the next time.
    let partial = partial(&path);
    let written = paths::create_private(&partial)
        .with_context(|| format!("could not write {}", partial.display()))
        .and_then(|mut sink| {
            let html =
                version == Version::Original && file.mimetype.as_deref() == Some("text/html");
            Ok(client.download(url, html, &mut sink)?)
        })
        .and_then(|_| match version {
            Version::Original => quarantine(&partial),
            Version::Thumb => Ok(()),
        });
    if let Err(err) = written {
        let _ = fs::remove_file(&partial);
        return Err(err);
    }
    fs::rename(&partial, &path).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// Mark the file as downloaded from the internet, as a browser does, so
/// macOS asks before it runs anything: `open` would otherwise run a
/// posted `.terminal` or `.command` without a word. Deliberate: a copy
/// that cannot be marked is not handed over.
#[cfg(target_os = "macos")]
fn quarantine(path: &Path) -> Result<()> {
    use std::process::{Command, Stdio};

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let status = Command::new("/usr/bin/xattr")
        .args([
            "-w",
            "com.apple.quarantine",
            &format!("0081;{now:x};slack;"),
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("could not run xattr to mark the download")?;
    if !status.success() {
        bail!("could not mark {} as downloaded", path.display());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn quarantine(_path: &Path) -> Result<()> {
    Ok(())
}

/// Drop the copies older than a week. Each file has its own directory,
/// whose time is that of its last copy. One that cannot go does not keep
/// the others: the first failure is reported once all were tried.
pub fn prune() -> Result<()> {
    let dir = dir()?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if paths::is_not_found(&err) => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("could not read {}", dir.display())),
    };
    let now = SystemTime::now();
    let mut failed = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| is_stale(modified, now));
        if !stale {
            continue;
        }
        let removed = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        // Another screen starting at the same time may have taken it.
        if let Err(err) = removed
            && !paths::is_not_found(&err)
        {
            failed.get_or_insert_with(|| format!("could not remove {}: {err}", path.display()));
        }
    }
    failed.map_or(Ok(()), |failed| bail!(failed))
}

/// Appended, not swapped for the extension: a file named `x.part` would
/// be its own partial copy.
fn partial(path: &Path) -> PathBuf {
    let mut name = OsString::from(path);
    name.push(".part");
    PathBuf::from(name)
}

/// Drop every copy, for `slack logout`; false when there was none.
pub fn clear() -> Result<bool> {
    let dir = dir()?;
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(true),
        Err(err) if paths::is_not_found(&err) => Ok(false),
        Err(err) => Err(err).with_context(|| format!("could not remove {}", dir.display())),
    }
}

fn dir() -> Result<PathBuf> {
    Ok(paths::cache_dir()?.join("files"))
}

fn is_stale(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified).is_ok_and(|age| age > KEEP_FOR)
}

/// `<id>/<name>` for the file, `<id>/.thumb` for its preview: the name
/// keeps its extension, which the system viewer goes by, and shows in the
/// viewer's title.
fn relative(file: &File, version: Version) -> Result<PathBuf> {
    // The id names a directory: anything but Slack's own shape could
    // climb out of the cache.
    if file.id.is_empty() || !file.id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("Slack gave this file no usable id");
    }
    let name = match version {
        Version::Thumb => ".thumb".to_string(),
        Version::Original => safe_name(&file.name),
    };
    Ok(Path::new(&file.id).join(name))
}

/// The file name as one path component, never a hidden file, so it
/// cannot meet `.thumb` or a `.part` of its own.
fn safe_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim_start_matches('.');
    if name.is_empty() {
        "file".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, name: &str) -> File {
        File {
            id: id.to_string(),
            title: name.to_string(),
            name: name.to_string(),
            mimetype: None,
            url: None,
            thumb: None,
            permalink: None,
        }
    }

    #[test]
    fn a_copy_stays_in_its_files_directory() {
        let path = |id: &str, name: &str, version| relative(&file(id, name), version);
        assert_eq!(
            path("F1", "shot.png", Version::Original).unwrap(),
            Path::new("F1/shot.png")
        );
        assert_eq!(
            path("F1", "shot.png", Version::Thumb).unwrap(),
            Path::new("F1/.thumb")
        );
        assert_eq!(
            path("F1", "../../.ssh/config", Version::Original).unwrap(),
            Path::new("F1/_.._.ssh_config")
        );
        assert_eq!(
            path("F1", ".thumb", Version::Original).unwrap(),
            Path::new("F1/thumb")
        );
        assert_eq!(
            path("F1", "", Version::Original).unwrap(),
            Path::new("F1/file")
        );
        assert!(path("../F1", "shot.png", Version::Original).is_err());
        assert!(path("", "shot.png", Version::Original).is_err());
    }

    #[test]
    fn a_partial_copy_never_has_the_name_of_a_file() {
        assert_eq!(
            partial(Path::new("F1/shot.png")),
            Path::new("F1/shot.png.part")
        );
        assert_eq!(partial(Path::new("F1/x.part")), Path::new("F1/x.part.part"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_download_is_marked_as_one() {
        use std::env;
        use std::process::Command;

        let dir = env::temp_dir().join(format!("slack-quarantine-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("setup.terminal");
        fs::write(&path, "x").unwrap();
        quarantine(&path).unwrap();
        let read = Command::new("/usr/bin/xattr")
            .args(["-p", "com.apple.quarantine"])
            .arg(&path)
            .output()
            .unwrap();
        fs::remove_dir_all(&dir).unwrap();
        let value = String::from_utf8_lossy(&read.stdout);
        assert!(
            value.starts_with("0081;") && value.trim_end().ends_with(";slack;"),
            "{value}"
        );
    }

    #[test]
    fn a_copy_goes_after_a_week() {
        let now = SystemTime::now();
        let ago = |days: u64| now - Duration::from_secs(days * 24 * 60 * 60);
        assert!(!is_stale(ago(6), now));
        assert!(is_stale(ago(8), now));
        assert!(
            !is_stale(now + Duration::from_secs(60), now),
            "a clock that moved back"
        );
    }
}
