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

    /// A raw device or disk image could not be opened. Most often a missing
    /// drive, or raw volume access attempted without Administrator rights.
    #[error("cannot open {path}: {source}")]
    Device {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A read from a device or image failed.
    #[error("read of {len} bytes at offset {offset} on {device} failed: {source}")]
    BlockRead {
        device: String,
        offset: u64,
        len: usize,
        #[source]
        source: std::io::Error,
    },

    /// A read returned fewer bytes than required, having run past the end of
    /// the device, image, or partition.
    #[error("read at offset {offset} on {device} returned {got} of {wanted} bytes")]
    ShortRead {
        device: String,
        offset: u64,
        wanted: usize,
        got: usize,
    },

    /// Unbuffered device reads require the offset, the length, *and* the
    /// buffer's address to be sector-aligned. A caller bug, reported plainly
    /// rather than as the OS's opaque "invalid parameter".
    #[error(
        "misaligned unbuffered read on {device}: offset {offset}, length {len}, \
         buffer address off by {buffer_align} — all must be multiples of the \
         {sector_size}-byte sector"
    )]
    Misaligned {
        device: String,
        offset: u64,
        len: usize,
        buffer_align: usize,
        sector_size: u32,
    },

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

    /// A Windows API call failed. Carries the API's name so a raw OS error
    /// code can be traced back to the call that produced it.
    #[error("{api} failed: {source}")]
    WindowsApi {
        api: String,
        #[source]
        source: std::io::Error,
    },

    /// The operation exists only on some platforms — raw volume access and
    /// elevation are Windows-only. Returned rather than `cfg`-gating the
    /// function away, so callers need no conditional compilation.
    #[error("{operation} is not supported on this platform")]
    UnsupportedPlatform { operation: String },

    /// A long-running operation was cancelled by the user. See
    /// `service::task::CancellationToken`.
    #[error("operation cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;
