//! Launch the user's Chrome (or Edge, Chromium, Brave) against a profile of
//! our own with the DevTools port open, and shut it down cleanly afterwards.
//!
//! The dedicated profile is not a courtesy: since Chrome 136 the browser
//! refuses `--remote-debugging-port` on the default user profile. The cost
//! is one sign-in inside this profile; it persists, so later runs need none.
//! It also makes the profile a credential store, which is why `logout`
//! deletes it, and why deletion is guarded by a sentinel we wrote ourselves.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::paths;

const SENTINEL: &str = ".slack-browser-profile";
/// Bumped when a launch-flag change makes existing profiles unreadable.
const PROFILE_FORMAT: &str = "v1";
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// The initial tab registers a few hundred ms after the port is up.
const PAGE_TARGET_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

const CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/bin/microsoft-edge",
    "/usr/bin/brave-browser",
];
const PATH_NAMES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "microsoft-edge",
    "brave-browser",
];

/// `$SLACK_BROWSER` wins outright, so an unusual install is never blocked
/// by the list above.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("SLACK_BROWSER").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let on_path = || {
        let dirs: Vec<PathBuf> = env::var_os("PATH")
            .map(|path| env::split_paths(&path).collect())
            .unwrap_or_default();
        PATH_NAMES
            .iter()
            .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
            .find(|path| path.is_file())
    };
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .or_else(on_path)
}

/// `$SLACK_BROWSER_PROFILE`, else `browser-profile` next to the store.
pub fn profile_dir() -> Result<PathBuf> {
    match env::var_os("SLACK_BROWSER_PROFILE").filter(|value| !value.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(paths::config_dir()?.join("browser-profile")),
    }
}

/// A running browser. Dropping it shuts it down.
pub struct Browser {
    child: Child,
    port: u16,
    profile_dir: PathBuf,
    stopped: bool,
}

pub fn launch(profile_dir: &Path, headless: bool) -> Result<Browser> {
    let executable = find_browser().context(
        "no Chrome, Edge, Chromium or Brave found; install one or point $SLACK_BROWSER at the executable",
    )?;
    claim_profile(profile_dir)?;
    // A port file left by a previous run would be read as this run's port.
    let port_file = profile_dir.join("DevToolsActivePort");
    let _ = fs::remove_file(&port_file);

    let mut command = Command::new(&executable);
    command
        // Port 0: the OS picks a free one, reported through DevToolsActivePort.
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        // Slack serves a degraded client to browsers that announce automation.
        .arg("--disable-blink-features=AutomationControlled")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if headless {
        command.arg("--headless=new");
    }
    // Not detached, deliberately: a new session cuts Chrome off from the
    // login keychain on macOS, and it then cannot decrypt the cookies the
    // next run depends on. Ctrl-C reaches it through our process group.
    let child = command
        .spawn()
        .with_context(|| format!("could not start {}", executable.display()))?;
    let mut browser = Browser {
        child,
        port: 0,
        profile_dir: profile_dir.to_path_buf(),
        stopped: false,
    };

    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    loop {
        if let Some(port) = read_port(&port_file) {
            browser.port = port;
            return Ok(browser);
        }
        if browser.child.try_wait()?.is_some() {
            bail!(
                "the browser exited before exposing its DevTools port; most often the profile is \
                 already open in another `slack login` window"
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "the browser did not expose its DevTools port within {}s",
                LAUNCH_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

impl Browser {
    /// The WebSocket URL of the page target, once the initial tab exists. A
    /// fresh profile also lists extension targets, so the type filter is
    /// load-bearing.
    pub fn page_ws_url(&self) -> Result<String> {
        let endpoint = format!("http://127.0.0.1:{}/json/list", self.port);
        let deadline = Instant::now() + PAGE_TARGET_TIMEOUT;
        loop {
            if let Some(url) = page_target(&endpoint) {
                return Ok(url);
            }
            if Instant::now() >= deadline {
                bail!("the browser started but exposed no page to attach to");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if !self.exited() {
            // SIGTERM first, so Chrome flushes the profile: the session
            // cookie and localConfig_v2 the next run depends on live there.
            signal(self.child.id(), "-TERM");
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            while Instant::now() < deadline && !self.exited() {
                thread::sleep(Duration::from_millis(100));
            }
            if !self.exited() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
        // Chrome leaves a utility helper behind even after a clean exit, and
        // every helper carries our --user-data-dir, which nothing else does.
        // Only after shutdown: sweeping first leaves a profile Chrome refuses
        // to reopen.
        thread::sleep(Duration::from_millis(300));
        sweep_helpers(&self.profile_dir);
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop();
    }
}

pub enum ClearOutcome {
    Cleared,
    Absent,
    NotOurs,
}

/// Delete the profile, but only one we created: `$SLACK_BROWSER_PROFILE`
/// is user input and this is a recursive delete.
pub fn clear_profile(dir: &Path) -> Result<ClearOutcome> {
    if !is_owned(dir) {
        return Ok(if fs::symlink_metadata(dir).is_ok() {
            ClearOutcome::NotOurs
        } else {
            ClearOutcome::Absent
        });
    }
    fs::remove_dir_all(dir).with_context(|| format!("could not delete {}", dir.display()))?;
    Ok(ClearOutcome::Cleared)
}

fn read_port(port_file: &Path) -> Option<u16> {
    let contents = fs::read_to_string(port_file).ok()?;
    contents
        .lines()
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
}

fn page_target(endpoint: &str) -> Option<String> {
    let mut response = ureq::get(endpoint).call().ok()?;
    let targets: Vec<Value> = response.body_mut().read_json().ok()?;
    targets
        .iter()
        .find(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|target| target.get("webSocketDebuggerUrl")?.as_str())
        .map(str::to_string)
}

/// Take ownership of the profile directory, or refuse it. An existing
/// directory without our sentinel is somebody else's, most likely a real
/// browser profile the override was pointed at.
fn claim_profile(dir: &Path) -> Result<()> {
    if is_owned(dir) {
        if !has_current_format(dir) {
            fs::remove_dir_all(dir)
                .with_context(|| format!("could not reset {}", dir.display()))?;
        } else {
            // mkdir's mode applies on creation only; a restored or older
            // profile may hold a live cookie store with looser bits.
            paths::restrict(dir, 0o700)?;
            return stamp(dir);
        }
    } else {
        match fs::read_dir(dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    bail!(
                        "{} already contains data not created by slack; point \
                         $SLACK_BROWSER_PROFILE at a new or empty directory, or unset it",
                        dir.display()
                    );
                }
            }
            Err(err) if paths::is_not_found(&err) => {}
            Err(err) => {
                return Err(err).with_context(|| format!("could not inspect {}", dir.display()));
            }
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    paths::restrict(dir, 0o700)?;
    stamp(dir)
}

fn stamp(dir: &Path) -> Result<()> {
    let contents = format!("slack browser profile\n{PROFILE_FORMAT}\n");
    paths::write_private(&dir.join(SENTINEL), contents.as_bytes())?;
    Ok(())
}

/// `symlink_metadata`, not `metadata`: a symlink to a real profile would
/// pass a plain check, and the delete would then unlink the link while the
/// credential store it pointed at survived.
fn is_owned(dir: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return false;
    };
    meta.is_dir()
        && fs::symlink_metadata(dir.join(SENTINEL)).is_ok_and(|sentinel| sentinel.is_file())
}

fn has_current_format(dir: &Path) -> bool {
    fs::read_to_string(dir.join(SENTINEL))
        .map(|contents| contents.lines().any(|line| line.trim() == PROFILE_FORMAT))
        .unwrap_or(false)
}

fn signal(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn sweep_helpers(profile_dir: &Path) {
    // The pattern drops the flag's leading `--`: pkill reads a pattern that
    // starts with a dash as an option and aborts.
    // Anchored: a profile at `~/p` must not take `~/p-work` with it.
    let pattern = format!(
        "user-data-dir={}( |$)",
        escape_ere(&profile_dir.to_string_lossy())
    );
    let _ = Command::new("pkill")
        .args(["-9", "-f", &pattern])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Escape for `pkill -f`, which reads its pattern as an extended regex.
fn escape_ere(value: &str) -> String {
    const SPECIALS: &[char] = &[
        '.', '*', '+', '?', '^', '$', '{', '}', '(', ')', '|', '[', ']', '\\',
    ];
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if SPECIALS.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ere_specials_are_escaped() {
        assert_eq!(escape_ere("/a.b/c+d(e)"), r"/a\.b/c\+d\(e\)");
        assert_eq!(escape_ere("plain-name_1"), "plain-name_1");
    }

    #[test]
    fn the_first_line_of_the_port_file_is_the_port() {
        let dir = std::env::temp_dir().join(format!("slack-port-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("DevToolsActivePort");
        fs::write(&file, "54321\n/devtools/browser/abc\n").unwrap();
        assert_eq!(read_port(&file), Some(54321));
        fs::write(&file, "\n").unwrap();
        assert_eq!(read_port(&file), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ownership_needs_the_sentinel_in_a_real_directory() {
        let dir = std::env::temp_dir().join(format!("slack-profile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(!is_owned(&dir));
        fs::create_dir_all(&dir).unwrap();
        assert!(!is_owned(&dir));
        stamp(&dir).unwrap();
        assert!(is_owned(&dir));
        assert!(has_current_format(&dir));
        fs::write(dir.join(SENTINEL), "slack browser profile\nv0\n").unwrap();
        assert!(!has_current_format(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }
}
