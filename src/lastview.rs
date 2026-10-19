//! What the last view showed, per workspace: each conversation with the
//! newest message it had at the time, so `slack read` moves the cursors to
//! what was actually seen, not to whatever arrived since.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::view::View;

#[derive(Clone, Serialize, Deserialize)]
pub struct Shown {
    /// The newest message the conversation had when it was shown.
    pub latest: String,
    /// It was in the attention sections (a DM or a mention), not a channel
    /// listed by `-a`. `slack read` without arguments stops at these.
    pub attention: bool,
}

type Store = BTreeMap<String, BTreeMap<String, Shown>>;

fn path() -> Result<PathBuf> {
    Ok(paths::cache_dir()?.join("last-view.json"))
}

fn load() -> Result<Store> {
    match fs::read(path()?) {
        Ok(raw) => serde_json::from_slice(&raw).context("last-view.json is not valid"),
        Err(err) if paths::is_not_found(&err) => Ok(Store::new()),
        Err(err) => Err(err.into()),
    }
}

fn save(store: &Store) -> Result<()> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    paths::write_private(&path, &serde_json::to_vec(store)?)?;
    Ok(())
}

/// What the last view showed for this workspace, by conversation id.
pub fn recall(team_id: &str) -> Result<BTreeMap<String, Shown>> {
    Ok(load()?.remove(team_id).unwrap_or_default())
}

/// Drop conversations from the record once they were marked as read.
pub fn forget(team_id: &str, ids: &[String]) -> Result<()> {
    let mut store = load()?;
    if let Some(shown) = store.get_mut(team_id) {
        for id in ids {
            shown.remove(id);
        }
    }
    save(&store)
}

/// Replace the workspace's record with what this view showed.
pub fn remember(team_id: &str, view: &View) -> Result<()> {
    // A corrupt cache is replaced, not fatal: it only feeds `slack read`.
    let mut store = load().unwrap_or_default();
    let shown = view
        .shown()
        .map(|(id, latest, attention)| {
            let shown = Shown {
                latest: latest.to_string(),
                attention,
            };
            (id.to_string(), shown)
        })
        .collect();
    store.insert(team_id.to_string(), shown);
    save(&store)
}
