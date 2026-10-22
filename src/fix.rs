//! Correcting a draft before it is sent: the accents a QWERTY keyboard
//! leaves out, the typos, the grammar, never the wording. The model is
//! whatever command `fix_command` names in the config, so no key is kept
//! here and any runner (`claude -p`, `ollama run`, `llm`) fits; the
//! instructions are ours, so every runner gets the same ones.

use std::env;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Longer than a model's usual few seconds, short enough that a command
/// stuck on a prompt of its own does not hold the screen.
const TIMEOUT: Duration = Duration::from_secs(30);

/// What the command reads on its stdin: the instructions, then the draft
/// between tags so a draft that reads like an instruction stays a draft.
#[must_use]
pub fn prompt(draft: &str) -> String {
    format!(
        "Correct the spelling, the missing accents and the grammar of the Slack message \
         between the <message> tags, in the language it is written in. Do not rephrase: keep \
         the words, the tone, the meaning and the line breaks. Leave Slack markup, emoji codes, \
         mentions, code and URLs as they are. Reply with the corrected message alone, without \
         the tags and without a comment; if nothing needs correcting, reply with the message \
         unchanged.\n\n<message>\n{draft}\n</message>\n"
    )
}

/// The corrected text out of what the command printed: the tags a model
/// echoes despite the instructions are dropped. None when nothing is left.
#[must_use]
pub fn cleaned(output: &str) -> Option<String> {
    let text = output.trim();
    let text = text.strip_prefix("<message>").unwrap_or(text);
    let text = text.strip_suffix("</message>").unwrap_or(text);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Run `command` through `sh -c` on the draft and return its correction.
pub fn run(command: &str, draft: &str) -> Result<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        // Deliberate: an agent CLI such as `claude -p` reads the project
        // files of the directory it starts in, wherever the screen was opened.
        .current_dir(env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start the fix command")?;
    // Each pipe has its own thread: a command that writes before it has
    // read all of its stdin would otherwise block on a full pipe, and us
    // with it.
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        bail!("the fix command's pipes were not set up");
    };
    let prompt = prompt(draft);
    let writer = thread::spawn(move || stdin.write_all(prompt.as_bytes()));
    let output = thread::spawn(move || read_all(stdout));
    let errors = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the fix command gave no answer in {} seconds",
                TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    // A command that answers without reading its stdin breaks the pipe;
    // its answer is what counts.
    let _ = writer.join();
    let output = output.join().unwrap_or_default();
    if !status.success() {
        let errors = errors.join().unwrap_or_default();
        let last = errors.lines().rev().find(|line| !line.trim().is_empty());
        match last {
            Some(line) => bail!("the fix command failed: {}", line.trim()),
            None => bail!("the fix command failed ({status})"),
        }
    }
    cleaned(&output).context("the fix command printed nothing")
}

/// The whole of a pipe, lossy: a stray byte must not cost the answer.
fn read_all(mut pipe: impl Read) -> String {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_draft_sits_between_the_tags() {
        assert!(prompt("ca va ?").ends_with("<message>\nca va ?\n</message>\n"));
    }

    #[test]
    fn cleaned_drops_echoed_tags_and_blank_answers() {
        assert_eq!(cleaned("ça va ?\n").as_deref(), Some("ça va ?"));
        assert_eq!(
            cleaned("<message>\nça va ?\n</message>").as_deref(),
            Some("ça va ?")
        );
        assert_eq!(cleaned("one\ntwo").as_deref(), Some("one\ntwo"));
        assert_eq!(cleaned(" \n"), None);
    }

    #[test]
    fn runs_the_command_on_the_prompt() {
        // `tail` keeps the draft and its closing tag: the prompt reached stdin.
        let fixed = run("tail -n 2 | head -n 1", "ca va ?").unwrap();
        assert_eq!(fixed, "ca va ?");
    }

    #[test]
    fn a_failing_command_says_why() {
        let err = run("echo 'not logged in' >&2; exit 3", "x").unwrap_err();
        assert_eq!(err.to_string(), "the fix command failed: not logged in");
    }

    #[test]
    fn an_empty_answer_is_an_error() {
        let err = run("cat >/dev/null", "x").unwrap_err();
        assert_eq!(err.to_string(), "the fix command printed nothing");
    }
}
