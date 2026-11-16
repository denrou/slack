//! The files of `slack send --file`, uploaded the way the Slack app does
//! it: `files.getUploadURLExternal` names a URL and a file id, the bytes
//! go to that URL, and `files.completeUploadExternal` keeps the file,
//! shared in a conversation with a message or left unshared for a draft
//! to carry. A file never completed is discarded by Slack, so a failure
//! midway leaves nothing behind.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::api::Client;

/// Slack's own limit for one file.
const MAX_BYTES: u64 = 1 << 30;

/// A file on disk that passed `check`.
#[derive(Debug, PartialEq)]
pub struct Local {
    path: PathBuf,
    name: String,
}

/// A file Slack holds, not yet kept by `complete`.
#[derive(Debug, PartialEq)]
pub struct Uploaded {
    id: String,
    title: String,
}

#[cfg(test)]
pub fn uploaded(id: &str, title: &str) -> Uploaded {
    Uploaded {
        id: id.to_string(),
        title: title.to_string(),
    }
}

/// Where `complete` shares the files, with the message that introduces
/// them.
pub struct Share<'a> {
    pub channel: &'a str,
    pub thread_ts: Option<&'a str>,
    pub text: &'a str,
}

/// Every path of `--file` as a file that can be uploaded, before anything
/// is asked of Slack: one wrong path fails the run with nothing sent.
pub fn check(paths: &[PathBuf]) -> Result<Vec<Local>> {
    paths.iter().map(|path| check_one(path)).collect()
}

fn check_one(path: &Path) -> Result<Local> {
    let shown = path.display();
    let meta = std::fs::metadata(path).with_context(|| format!("could not read `{shown}`"))?;
    if meta.is_dir() {
        bail!("`{shown}` is a folder; --file takes a file");
    }
    if !meta.is_file() {
        bail!("`{shown}` is not a regular file");
    }
    check_length(meta.len()).with_context(|| format!("`{shown}` cannot be attached"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("`{shown}` has no name Slack could show"))?
        .to_string();
    Ok(Local {
        path: path.to_path_buf(),
        name,
    })
}

fn check_length(length: u64) -> Result<()> {
    if length == 0 {
        bail!("the file is empty");
    }
    if length > MAX_BYTES {
        bail!("the file is over Slack's limit of 1 GB");
    }
    Ok(())
}

/// Uploads each file and returns them in the order given. Nothing is
/// kept by Slack until `complete`.
pub fn send(client: &Client, files: &[Local]) -> Result<Vec<Uploaded>> {
    files.iter().map(|file| send_one(client, file)).collect()
}

fn send_one(client: &Client, local: &Local) -> Result<Uploaded> {
    let shown = local.path.display();
    let file = File::open(&local.path).with_context(|| format!("could not open `{shown}`"))?;
    // Deliberate: the length of the file as opened, not as checked, since
    // it is what goes out and Slack refuses a length that does not match.
    let length = file
        .metadata()
        .with_context(|| format!("could not read `{shown}`"))?
        .len();
    check_length(length).with_context(|| format!("`{shown}` cannot be attached"))?;
    let length_param = length.to_string();
    let body = client
        .call(
            "files.getUploadURLExternal",
            &[("filename", &local.name), ("length", &length_param)],
        )
        .with_context(|| format!("could not start the upload of `{shown}`"))?;
    let ticket = Ticket::parse(&body)?;
    client
        .upload(ticket.url, &file)
        .with_context(|| format!("could not upload `{shown}`"))?;
    Ok(Uploaded {
        id: ticket.id.to_string(),
        title: local.name.clone(),
    })
}

/// What `files.getUploadURLExternal` answers: where the bytes go and the
/// id the file will have.
#[derive(Debug, PartialEq)]
struct Ticket<'a> {
    url: &'a str,
    id: &'a str,
}

impl<'a> Ticket<'a> {
    fn parse(body: &'a Value) -> Result<Ticket<'a>> {
        let field = |name: &str| {
            body.get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("files.getUploadURLExternal answered without `{name}`"))
        };
        Ok(Ticket {
            url: field("upload_url")?,
            id: field("file_id")?,
        })
    }
}

/// Keeps the uploaded files: shared as one message when `share` says
/// where, else unshared, for a draft to carry. Returns Slack's answer.
pub fn complete(client: &Client, uploaded: &[Uploaded], share: Option<&Share>) -> Result<Value> {
    let params = complete_params(uploaded, share);
    let params: Vec<(&str, &str)> = params
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    Ok(client.call("files.completeUploadExternal", &params)?)
}

fn complete_params(uploaded: &[Uploaded], share: Option<&Share>) -> Vec<(&'static str, String)> {
    let files: Vec<Value> = uploaded
        .iter()
        .map(|file| json!({ "id": file.id, "title": file.title }))
        .collect();
    let mut params = vec![("files", Value::from(files).to_string())];
    if let Some(share) = share {
        params.push(("channel_id", share.channel.to_string()));
        params.push(("initial_comment", share.text.to_string()));
        if let Some(ts) = share.thread_ts {
            params.push(("thread_ts", ts.to_string()));
        }
    }
    params
}

/// The `file_ids` of `drafts.create`.
pub fn ids_param(uploaded: &[Uploaded]) -> String {
    let ids: Vec<&str> = uploaded.iter().map(|file| file.id.as_str()).collect();
    json!(ids).to_string()
}

/// The ts of the message that shared the files in `channel`, when the
/// answer of `files.completeUploadExternal` already carries it: Slack
/// shares after answering more often than not.
pub fn shared_ts(body: &Value, channel: &str) -> Option<String> {
    body.get("files")?.as_array()?.iter().find_map(|file| {
        file.get("shares")?
            .as_object()?
            .values()
            .find_map(|kind| kind.get(channel)?.get(0)?.get("ts")?.as_str())
            .map(str::to_string)
    })
}

/// " with 2 files", for the line `send` prints; empty without files.
pub fn with_files(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => " with 1 file".to_string(),
        n => format!(" with {n} files"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// A folder of its own under the system's temporary one.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slack-upload-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        File::create(&path).unwrap().write_all(bytes).unwrap();
        path
    }

    #[test]
    fn a_file_is_taken_with_its_name() {
        let dir = scratch("ok");
        let path = write(&dir, "bundle.json", b"{}");
        assert_eq!(
            check(std::slice::from_ref(&path)).unwrap(),
            [Local {
                path,
                name: "bundle.json".to_string()
            }]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_an_empty_one_and_a_folder_are_refused() {
        let dir = scratch("refused");
        let good = write(&dir, "good.txt", b"x");
        let empty = write(&dir, "empty.txt", b"");

        let err = check(&[dir.join("absent.txt")]).unwrap_err();
        assert!(format!("{err:#}").contains("could not read"), "{err:#}");
        let err = check(std::slice::from_ref(&empty)).unwrap_err();
        assert!(format!("{err:#}").contains("empty"), "{err:#}");
        let err = check(std::slice::from_ref(&dir)).unwrap_err();
        assert!(format!("{err:#}").contains("folder"), "{err:#}");
        // One wrong path refuses the lot.
        assert!(check(&[good, empty]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_size_is_held_to_slacks_limit() {
        assert!(check_length(1).is_ok());
        assert!(check_length(MAX_BYTES).is_ok());
        assert!(check_length(MAX_BYTES + 1).is_err());
        assert!(check_length(0).is_err());
    }

    #[test]
    fn a_ticket_needs_its_url_and_its_id() {
        let body = json!({
            "ok": true,
            "upload_url": "https://files.slack.com/upload/v1/ABC123",
            "file_id": "F123ABC456",
        });
        assert_eq!(
            Ticket::parse(&body).unwrap(),
            Ticket {
                url: "https://files.slack.com/upload/v1/ABC123",
                id: "F123ABC456"
            }
        );
        assert!(Ticket::parse(&json!({"ok": true, "file_id": "F1"})).is_err());
        assert!(
            Ticket::parse(&json!({"ok": true, "upload_url": "https://x", "file_id": ""})).is_err()
        );
    }

    #[test]
    fn files_for_a_draft_are_kept_unshared() {
        let files = [uploaded("F1", "a.json"), uploaded("F2", "b.png")];
        assert_eq!(
            complete_params(&files, None),
            [(
                "files",
                r#"[{"id":"F1","title":"a.json"},{"id":"F2","title":"b.png"}]"#.to_string()
            )]
        );
        assert_eq!(ids_param(&files), r#"["F1","F2"]"#);
        assert_eq!(ids_param(&[]), "[]");
    }

    #[test]
    fn files_sent_now_go_with_the_message_to_its_place() {
        let files = [uploaded("F1", "a.json")];
        let share = Share {
            channel: "C0A754E2V",
            thread_ts: None,
            text: "the bundle, <@U1>",
        };
        assert_eq!(
            complete_params(&files, Some(&share)),
            [
                ("files", r#"[{"id":"F1","title":"a.json"}]"#.to_string()),
                ("channel_id", "C0A754E2V".to_string()),
                ("initial_comment", "the bundle, <@U1>".to_string()),
            ]
        );
        let share = Share {
            thread_ts: Some("1760880000.000100"),
            ..share
        };
        assert_eq!(
            complete_params(&files, Some(&share)).last().unwrap(),
            &("thread_ts", "1760880000.000100".to_string())
        );
    }

    #[test]
    fn the_shared_message_is_read_when_the_answer_has_it() {
        let body = json!({"ok": true, "files": [{
            "id": "F1",
            "shares": {"private": {"D0A1": [{"ts": "1760880000.000200"}]}},
        }]});
        assert_eq!(
            shared_ts(&body, "D0A1").as_deref(),
            Some("1760880000.000200")
        );
        assert_eq!(shared_ts(&body, "C0FF"), None);
        let bare = json!({"ok": true, "files": [{"id": "F1", "title": "a.json"}]});
        assert_eq!(shared_ts(&bare, "D0A1"), None);
    }

    #[test]
    fn the_printed_line_counts_the_files() {
        assert_eq!(with_files(0), "");
        assert_eq!(with_files(1), " with 1 file");
        assert_eq!(with_files(3), " with 3 files");
    }
}
