//! Per-document sidecar persistence (STANDARDS §4.5).
//!
//! Forensic apps routinely attach metadata to an external file the user
//! already manages: tags on a chat export, annotations on a video, notes on
//! a GPS dump. The sidecar pattern keeps that metadata in a `{stem}_{kind}.json`
//! file next to the original. Portable, no central database, easy to inspect.
//!
//! Atomic writes: write to `foo.json.tmp`, fsync, rename. A crash mid-write
//! leaves the previous valid file in place, never a half-written one.
//!
//! Usage:
//!
//! ```ignore
//! use crate::service::case::Sidecar;
//!
//! #[derive(serde::Serialize, serde::Deserialize)]
//! struct TagData { schema_version: u32, tags: Vec<Tag> }
//!
//! let sc = Sidecar::for_file("/cases/evidence-001.txt", "tags");
//! if let Some(data) = sc.load::<TagData>()? { /* ...use it... */ }
//! sc.save(&TagData { schema_version: 1, tags: vec![] })?;
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

/// A sidecar file paired with a source document.
///
/// Constructed from the source path and a kind suffix; the resulting sidecar
/// lives at `{source-dir}/{source-stem}_{kind}.json`.
pub struct Sidecar {
    path: PathBuf,
}

impl Sidecar {
    /// Construct a sidecar paired with `source` and labelled by `kind`
    /// (e.g. `"tags"`, `"annotations"`). The kind becomes part of the
    /// filename so multiple sidecars can attach to one source.
    pub fn for_file(source: impl AsRef<Path>, kind: &str) -> Self {
        let source = source.as_ref();
        let stem = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir = source.parent().unwrap_or_else(|| Path::new("."));
        let path = dir.join(format!("{stem}_{kind}.json"));
        Self { path }
    }

    /// The on-disk location of this sidecar. Exposed for logging and UI display.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load and deserialize the sidecar. Returns `Ok(None)` if the file
    /// does not exist (first-run case). Errors only on read or parse failure.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.path).map_err(|source| Error::Io {
            path: self.path.clone(),
            source,
        })?;
        let value: T = serde_json::from_slice(&bytes).map_err(|e| Error::Parse {
            format: "json".into(),
            message: e.to_string(),
        })?;
        Ok(Some(value))
    }

    /// Serialize and write the sidecar atomically. On success the file at
    /// [`Self::path`] reflects exactly the given value. On failure the
    /// previous file (if any) is untouched.
    pub fn save<T: Serialize>(&self, value: &T) -> Result<()> {
        let json = serde_json::to_vec_pretty(value).map_err(|e| Error::Parse {
            format: "json".into(),
            message: e.to_string(),
        })?;

        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(&json).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            f.sync_all().map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, &self.path).map_err(|source| Error::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Sample {
        schema_version: u32,
        name: String,
    }

    fn tmpdir() -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("emfit-case-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn load_returns_none_for_missing_sidecar() {
        let dir = tmpdir();
        let source = dir.join("nonexistent.txt");
        let sc = Sidecar::for_file(&source, "tags");
        let loaded: Option<Sample> = sc.load().unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tmpdir();
        let source = dir.join("evidence.txt");
        let sc = Sidecar::for_file(&source, "tags");
        let value = Sample {
            schema_version: 1,
            name: "alpha".into(),
        };
        sc.save(&value).unwrap();
        let loaded: Sample = sc.load().unwrap().unwrap();
        assert_eq!(loaded, value);
        let _ = fs::remove_file(sc.path());
    }

    #[test]
    fn sidecar_path_is_next_to_source() {
        let sc = Sidecar::for_file("/cases/sub/evidence.txt", "tags");
        assert_eq!(sc.path(), Path::new("/cases/sub/evidence_tags.json"));
    }
}
