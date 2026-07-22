//! Serialize-able data-transfer types for the IPC boundary.
//!
//! Core types stay serde-for-IPC-free (STANDARDS Â§1): the shell owns the
//! `Serialize`/`Deserialize` shapes the webview sees. This module holds those
//! shapes plus `From` conversions from the core types they mirror. Add request
//! structs (deserialized from the webview) and outcome structs (serialized
//! back) here as commands grow.
//!
//! `ProgressDto` is the worked example â€” the serializable mirror of
//! `emfit_core::service::task::Progress`, streamed over a
//! `tauri::ipc::Channel` (Â§3.5). The matching TypeScript type lives in
//! `frontend/src/lib/types.ts`.

use emfit_core::service::task::Progress;
use serde::Serialize;

/// Serializable mirror of [`Progress`] for the progress `Channel`.
///
/// `#[serde(tag = "kind")]` makes each variant an object like
/// `{ "kind": "Tick", "done": 42 }`, which the frontend switches on.
///
/// `allow(dead_code)`: template scaffolding. The type is unused until the
/// first command streams progress over a `Channel<ProgressDto>` (Â§3.5);
/// remove the allow once a command references it.
#[allow(dead_code)]
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum ProgressDto {
    /// A phase begins. `message` describes what is happening right now; `total`
    /// is the expected unit count when known, `None` for indeterminate phases.
    Started { message: String, total: Option<u64> },
    /// One unit of progress. `done` is cumulative, not delta.
    Tick { done: u64 },
    /// Work finished successfully.
    Finished,
    /// Work failed partway through. `message` is for user display; the
    /// underlying error should still be returned from the service method
    /// via `Result`.
    Failed { message: String },
}

impl From<Progress> for ProgressDto {
    fn from(progress: Progress) -> Self {
        match progress {
            Progress::Started { message, total } => ProgressDto::Started { message, total },
            Progress::Tick { done } => ProgressDto::Tick { done },
            Progress::Finished => ProgressDto::Finished,
            Progress::Failed { message } => ProgressDto::Failed { message },
        }
    }
}
