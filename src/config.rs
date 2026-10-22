//! `~/.config/slack/config.toml`, the settings a person writes by hand.
//! A file rather than environment variables: the screen is often opened
//! from a terminal's key binding, where the shell's exports never reach.
//! Without the file every setting is off.

use std::fs;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// A shell command that corrects a draft: the instructions and the
    /// draft on its stdin, the corrected text on its stdout (`fix.rs`).
    pub fix_command: Option<String>,
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = paths::config_dir()?.join("config.toml");
        match fs::read_to_string(&path) {
            Ok(text) => parse(&text).with_context(|| format!("cannot read {}", path.display())),
            Err(err) if paths::is_not_found(&err) => Ok(Config::default()),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }
}

fn parse(text: &str) -> Result<Config> {
    let mut config: Config = toml::from_str(text)?;
    // An empty command is a setting left blank, not a command to run.
    config.fix_command = config
        .fix_command
        .filter(|command| !command.trim().is_empty());
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_fix_command() {
        let config = parse("fix_command = \"claude -p --model haiku\"\n").unwrap();
        assert_eq!(
            config.fix_command.as_deref(),
            Some("claude -p --model haiku")
        );
    }

    #[test]
    fn an_empty_file_or_command_sets_nothing() {
        assert_eq!(parse("").unwrap(), Config::default());
        assert_eq!(parse("fix_command = \"  \"").unwrap(), Config::default());
    }

    #[test]
    fn a_misspelt_key_is_an_error_rather_than_ignored() {
        assert!(parse("fix_comand = \"claude -p\"").is_err());
    }
}
