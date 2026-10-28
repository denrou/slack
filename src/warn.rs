//! Warnings on the way to a result: a conversation that could not be
//! read, a cache that could not be saved. They go to stderr, so the
//! result on stdout stays clean.

pub fn warn(message: impl Into<String>) {
    eprintln!("warning: {}", message.into());
}
