//! What the last view showed, per workspace: each conversation with the
//! newest message it had at the time, so `slack read` moves the cursors to
//! what was actually seen, and the threads it numbered, so `slack thread 2`
//! and `slack send 2` know which one that is.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::view::View;

#[derive(Clone, Serialize, Deserialize)]
pub struct Shown {
    /// The newest message the conversation had when it was shown.
    pub latest: String,
    /// It was in the attention sections (a DM, a mention, a followed
    /// channel), not a channel listed by `-a`. `slack read` without
    /// arguments stops at these.
    pub attention: bool,
}

/// A thread the view numbered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    pub channel: String,
    pub ts: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    conversations: BTreeMap<String, Shown>,
    /// Keyed by the number printed in the view, from 1.
    #[serde(default)]
    threads: BTreeMap<usize, ThreadRef>,
}

type Store = BTreeMap<String, Record>;

fn path() -> Result<PathBuf> {
    Ok(paths::cache_dir()?.join("last-view.json"))
}

/// The store, or an empty one: it is a cache, and an unreadable or
/// outdated file only means the next view rewrites it.
fn load() -> Result<Store> {
    match fs::read(path()?) {
        Ok(raw) => Ok(serde_json::from_slice(&raw).unwrap_or_default()),
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
    Ok(load()?
        .remove(team_id)
        .map(|record| record.conversations)
        .unwrap_or_default())
}

/// The thread the last view numbered `number`.
pub fn thread_ref(team_id: &str, number: usize) -> Result<Option<ThreadRef>> {
    Ok(load()?
        .get_mut(team_id)
        .and_then(|record| record.threads.remove(&number)))
}

/// Drop conversations from the record once they were marked as read.
pub fn forget(team_id: &str, ids: &[String]) -> Result<()> {
    let mut store = load()?;
    if let Some(record) = store.get_mut(team_id) {
        for id in ids {
            record.conversations.remove(id);
        }
    }
    save(&store)
}

/// Record one conversation shown on its own (`slack show`): its newest
/// message, so `slack read <name>` can mark it, and the threads that
/// output numbered, which replace the view's numbering: a number always
/// refers to the last output that printed one.
pub fn remember_conversation(
    team_id: &str,
    channel: &str,
    latest: Option<&str>,
    threads: Vec<ThreadRef>,
) -> Result<()> {
    let mut store = load().unwrap_or_default();
    let record = store.entry(team_id.to_string()).or_default();
    if let Some(latest) = latest {
        // A DM the view listed keeps its place in a bare `slack read`.
        let attention = record
            .conversations
            .get(channel)
            .is_some_and(|shown| shown.attention);
        record.conversations.insert(
            channel.to_string(),
            Shown {
                latest: latest.to_string(),
                attention,
            },
        );
    }
    record.threads = number(threads);
    save(&store)
}

fn number(threads: Vec<ThreadRef>) -> BTreeMap<usize, ThreadRef> {
    threads
        .into_iter()
        .enumerate()
        .map(|(index, thread)| (index + 1, thread))
        .collect()
}

/// Replace the workspace's record with what this view showed.
pub fn remember(team_id: &str, view: &View) -> Result<()> {
    let mut store = load().unwrap_or_default();
    let conversations = view
        .shown()
        .map(|(id, latest, attention)| {
            let shown = Shown {
                latest: latest.to_string(),
                attention,
            };
            (id.to_string(), shown)
        })
        .collect();
    store.insert(
        team_id.to_string(),
        Record {
            conversations,
            threads: number(view.thread_refs()),
        },
    );
    save(&store)
}
