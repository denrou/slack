//! Terminal styling that knows when to stay quiet: `--no-color`, the
//! `NO_COLOR` convention, or stdout not being a terminal all turn it off.

use std::env;
use std::io::IsTerminal;

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub enabled: bool,
}

impl Style {
    pub fn detect(no_color: bool) -> Style {
        let enabled = !no_color
            && env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
            && std::io::stdout().is_terminal();
        Style { enabled }
    }

    pub fn plain() -> Style {
        Style { enabled: false }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn italic(&self, text: &str) -> String {
        self.paint("3", text)
    }

    pub fn strike(&self, text: &str) -> String {
        self.paint("9", text)
    }

    pub fn heading(&self, text: &str) -> String {
        self.paint("1;4", text)
    }

    pub fn name(&self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn mention(&self, text: &str) -> String {
        self.paint("1;33", text)
    }

    pub fn code(&self, text: &str) -> String {
        self.paint("35", text)
    }

    pub fn link(&self, text: &str) -> String {
        self.paint("4;34", text)
    }
}
