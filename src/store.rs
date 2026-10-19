//! `workspaces.json`: every workspace the login captured, with its
//! credentials, and which one the commands act on.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::{Client, Credentials};
use crate::paths;

#[derive(Default, Serialize, Deserialize)]
pub struct Workspaces {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub workspaces: BTreeMap<String, Workspace>,
}

/// One signed-in workspace. Deliberately no `Debug`: the tokens must never
/// reach a log or an error message.
#[derive(Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub xoxc: String,
    pub xoxd: String,
}

impl Workspace {
    pub fn client(&self) -> Client {
        Client::new(
            &self.url,
            &Credentials {
                xoxc: &self.xoxc,
                xoxd: &self.xoxd,
            },
        )
    }
}

impl Workspaces {
    pub fn path() -> Result<PathBuf> {
        Ok(paths::config_dir()?.join("workspaces.json"))
    }

    /// The saved store, or an empty one when none was written yet. A file
    /// that exists but cannot be read is an error, not "logged out".
    pub fn load() -> Result<Workspaces> {
        let path = Self::path()?;
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(err) if paths::is_not_found(&err) => return Ok(Workspaces::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("could not read {}", path.display()));
            }
        };
        serde_json::from_slice(&raw)
            .with_context(|| format!("{} is not a valid workspace store", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let dir = path
            .parent()
            .context("the workspace store has no parent directory")?;
        fs::create_dir_all(dir)?;
        paths::restrict(dir, 0o700)?;
        // Write then rename, so a crash never leaves a half-written store.
        let tmp = path.with_extension("json.tmp");
        paths::write_private(&tmp, &serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, &path).with_context(|| format!("could not write {}", path.display()))?;
        Ok(())
    }

    /// Delete the store. True when there was one.
    pub fn remove() -> Result<bool> {
        match fs::remove_file(Self::path()?) {
            Ok(()) => Ok(true),
            Err(err) if paths::is_not_found(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// The workspace commands act on: the stored default, else the only one.
    pub fn default_workspace(&self) -> Result<(&str, &Workspace)> {
        if let Some(id) = &self.default
            && let Some(workspace) = self.workspaces.get(id)
        {
            return Ok((id, workspace));
        }
        let mut all = self.workspaces.iter();
        match (all.next(), all.next()) {
            (Some((id, workspace)), None) => Ok((id, workspace)),
            (None, _) => bail!("not logged in; run `slack login`"),
            _ => bail!(
                "several workspaces and no default; set \"default\" in {}",
                Self::path()?.display()
            ),
        }
    }

    /// Add or refresh a workspace; the first one saved becomes the default.
    pub fn upsert(&mut self, team_id: &str, workspace: Workspace) {
        self.workspaces.insert(team_id.to_string(), workspace);
        if self.default.is_none() {
            self.default = Some(team_id.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(json: &str) -> Workspaces {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_default_wins() {
        let store = store(
            r#"{"default": "T2", "workspaces": {
                "T1": {"name": "One", "url": "https://one.slack.com", "xoxc": "c1", "xoxd": "d1"},
                "T2": {"name": "Two", "url": "https://two.slack.com", "xoxc": "c2", "xoxd": "d2"}}}"#,
        );
        assert_eq!(store.default_workspace().unwrap().0, "T2");
    }

    #[test]
    fn a_single_workspace_needs_no_default() {
        let store = store(
            r#"{"workspaces": {
                "T1": {"name": "One", "url": "https://one.slack.com", "xoxc": "c1", "xoxd": "d1"}}}"#,
        );
        assert_eq!(store.default_workspace().unwrap().1.name, "One");
    }

    #[test]
    fn an_empty_store_means_not_logged_in() {
        let err = Workspaces::default()
            .default_workspace()
            .err()
            .expect("an empty store has no default workspace");
        assert!(err.to_string().contains("slack login"));
    }

    #[test]
    fn the_first_upsert_becomes_the_default() {
        let mut store = Workspaces::default();
        let workspace = Workspace {
            name: "One".into(),
            url: "https://one.slack.com".into(),
            user_id: None,
            xoxc: "c".into(),
            xoxd: "d".into(),
        };
        store.upsert("T1", workspace.clone());
        store.upsert("T2", workspace);
        assert_eq!(store.default.as_deref(), Some("T1"));
    }
}
