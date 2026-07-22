//! Error type for `#[tauri::command]` functions.
//!
//! Command return errors cross the IPC boundary into JavaScript, so they
//! must be `Serialize`. We keep a thin shell-level enum that wraps the core
//! crate's `Error` (STANDARDS §4.1) and serializes to its display string —
//! the webview gets a readable message, the Rust side keeps the typed error
//! for logging. Do not return `app_name_core::Error` directly from commands;
//! it isn't `Serialize` and shouldn't have to be.

use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// A failure that originated in the core logic crate.
    #[error(transparent)]
    Core(#[from] app_name_core::Error),

    /// A background task (e.g. a `spawn_blocking` worker) failed to join — it
    /// panicked or the runtime dropped it. Distinct from a domain error
    /// returned *by* the task, which arrives as `Core`.
    #[error("background task failed: {0}")]
    Task(#[from] tauri::Error),

    /// A failure that originated in the shell itself (bad argument, IO at the
    /// Tauri boundary). Prefer a typed `Core` variant when the failure is
    /// really a domain error.
    #[error("{0}")]
    Shell(String),
}

impl Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Result alias for command functions.
pub type CommandResult<T> = std::result::Result<T, CommandError>;
