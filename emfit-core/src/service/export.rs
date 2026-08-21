//! Export trait for writing the app's data out to a foreign format.
//!
//! Three of four reference projects ship some kind of export: COMRADE
//! exports PNG, FENDER exports Excel/JSON/KML, Message-Maestro exports PDF.
//! Each format gets its own `Exporter` impl in a sibling module
//! (`pdf_export.rs`, `kml_export.rs`, `xlsx_export.rs`).
//!
//! Usage:
//!
//! ```ignore
//! use crate::service::export::Exporter;
//!
//! let exporter = pdf_export::PdfExporter::new(theme, examiner);
//! exporter.write(&document, &path, &cancel)?;
//! ```
//!
//! The trait is intentionally narrow: take a domain document and a
//! destination path, return success or a structured error. Exporters that
//! need more context (PDF needs branding, KML needs projection, XLSX
//! needs column maps) take that context at construction.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::service::task::CancellationToken;

/// Write a domain object to an output file in some target format.
///
/// `Document` is whatever the app considers its primary unit (a parsed
/// conversation, a case, an analysis result). Each app defines it; the
/// trait is generic over it.
pub trait Exporter<Document> {
    /// Human-readable name of the format ("PDF", "KML", "Excel").
    fn name(&self) -> &str;

    /// File extension *without* the leading dot ("pdf", "kml", "xlsx").
    /// Used by the UI's save dialog and by `default_filename`.
    fn extension(&self) -> &str;

    /// Suggest a default filename for a given document. The default
    /// implementation appends `.{extension()}` to the document's
    /// `display_name`; override when the app wants something fancier
    /// (timestamp suffix, case-number prefix, etc.).
    fn default_filename(&self, display_name: &str) -> String {
        format!("{display_name}.{}", self.extension())
    }

    /// Write `doc` to `dest`. Implementations should:
    ///
    /// - Write to a temp file and atomically rename on success - use
    ///   [`write_atomic`] (STANDARDS sec 4.5).
    /// - Surface errors with enough context that the UI can show what failed.
    /// - Be re-entrant: no global state.
    /// - Poll `cancel` during long in-memory builds (large documents take
    ///   seconds to serialise) and return [`Error::Cancelled`] *before* writing
    ///   the file, so a cancelled export leaves no partial output.
    fn write(&self, doc: &Document, dest: &Path, cancel: &CancellationToken) -> Result<()>;
}

/// Write `bytes` to `dest` atomically: stream to a sibling `*.tmp`, fsync, then
/// rename over the target (STANDARDS sec 4.5). A crash mid-write leaves any prior
/// file intact rather than a half-written document. Shared by every exporter so
/// the durability discipline lives in one place.
pub fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    // Keep the temp file in the destination directory so the rename is a
    // same-filesystem move (atomic) rather than a cross-device copy.
    let tmp = match dest.file_name() {
        Some(name) => dest.with_file_name(format!("{}.tmp", name.to_string_lossy())),
        None => dest.with_extension("tmp"),
    };

    {
        let mut f = fs::File::create(&tmp).map_err(|source| Error::Io {
            path: tmp.clone(),
            source,
        })?;
        f.write_all(bytes).map_err(|source| Error::Io {
            path: tmp.clone(),
            source,
        })?;
        f.sync_all().map_err(|source| Error::Io {
            path: tmp.clone(),
            source,
        })?;
    }

    tracing::debug!(
        bytes = bytes.len(),
        tmp = %tmp.display(),
        dest = %dest.display(),
        "export: wrote temp file, renaming atomically over destination"
    );

    fs::rename(&tmp, dest).map_err(|source| Error::Io {
        path: dest.to_path_buf(),
        source,
    })
}
