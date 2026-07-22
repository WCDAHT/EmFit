//! Crate-level error type. See STANDARDS §4.1.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("parse error in {format} parser: {message}")]
    Parse { format: String, message: String },

    /// JSON (de)serialisation failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A TOML config file could not be parsed (corrupt or hand-edited wrong).
    /// The caller falls back to defaults; the original file is left in place.
    #[error("config parse error: {0}")]
    TomlDe(#[from] toml::de::Error),

    /// The config could not be serialised to TOML for writing.
    #[error("config serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    /// A configuration problem that isn't a (de)serialisation failure — most
    /// often the OS config/data directory could not be resolved.
    #[error("config error: {message}")]
    Config { message: String },

    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),

    /// A long-running operation was cancelled by the user. See
    /// `service::task::CancellationToken`.
    #[error("operation cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;
