//! Warnings on the way to a result: a conversation that could not be
//! read, a cache that could not be saved. They go to stderr, except while
//! the screen is up, where stderr would draw over it: the screen holds
//! them and shows them in its status line.

use std::sync::Mutex;

static HELD: Mutex<Option<Vec<String>>> = Mutex::new(None);

pub fn warn(message: impl Into<String>) {
    let message = message.into();
    if let Ok(mut held) = HELD.lock()
        && let Some(held) = held.as_mut()
    {
        held.push(message);
        return;
    }
    eprintln!("warning: {message}");
}

/// From here on warnings are held for `drain`, until `release`.
pub fn hold() {
    if let Ok(mut held) = HELD.lock() {
        held.get_or_insert_with(Vec::new);
    }
}

/// The warnings held since the last call.
pub fn drain() -> Vec<String> {
    HELD.lock()
        .ok()
        .and_then(|mut held| held.as_mut().map(std::mem::take))
        .unwrap_or_default()
}

/// Back to stderr; returns what was still held.
pub fn release() -> Vec<String> {
    HELD.lock()
        .ok()
        .and_then(|mut held| held.take())
        .unwrap_or_default()
}
