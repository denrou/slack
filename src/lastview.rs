//! What the last view showed, per workspace: each conversation with the
//! newest message it had at the time, so `slack read` moves the cursors to
//! what was actually seen, not to whatever arrived since.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::paths;
use crate::view::View;

type Store = BTreeMap<String, BTreeMap<String, String>>;

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

/// Replace the workspace's record with what this view showed.
pub fn remember(team_id: &str, view: &View) -> Result<()> {
    // A corrupt cache is replaced, not fatal: it only feeds `slack read`.
    let mut store = load().unwrap_or_default();
    let shown = view
        .shown()
        .map(|(id, latest)| (id.to_string(), latest.to_string()))
        .collect();
    store.insert(team_id.to_string(), shown);
    save(&store)
}
