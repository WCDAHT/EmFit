//! The release manifest the website publishes.
//!
//! One JSON object, one entry per product, keyed by product name:
//!
//! ```json
//! {
//!   "EmFit": {
//!     "version": "v0.4.0",
//!     "released_at": "2026-08-17",
//!     "original_filename": "EmFit-windows-x64.exe",
//!     "release_file": "content/EmFit/EmFit-windows-x64.exe",
//!     "readme_file": "content/EmFit/README.md"
//!   }
//! }
//! ```
//!
//! Paths are relative to the manifest's own URL, so moving the site to another
//! host needs no manifest edit.
//!
//! Every field but `version` and `release_file` is optional here even though
//! the site writes them all. A manifest is fetched from the network and shared
//! with five other products; a missing `readme_file` should cost the update
//! dialog its notes, not the whole check.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::{Error, Result};

/// One product's current release.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    /// `v0.4.0`. Compared with [`super::version`].
    pub version: String,
    #[serde(default)]
    pub released_at: String,
    /// What the file should be called once downloaded. Falls back to the last
    /// segment of `release_file` when absent.
    #[serde(default)]
    pub original_filename: String,
    /// Path to the binary, relative to the manifest URL.
    pub release_file: String,
    /// Path to the release notes, relative to the manifest URL.
    #[serde(default)]
    pub readme_file: Option<String>,
    /// Hex SHA-256 of `release_file`.
    ///
    /// **The site does not publish this yet.** It is parsed so that adding the
    /// field turns integrity checking on with no code change here (see
    /// [`super::verify`]); until then downloads are trusted on the strength of
    /// HTTPS alone, which authenticates the host but not the bytes it serves.
    #[serde(default)]
    pub sha256: Option<String>,
}

impl Release {
    /// The name to save the download under. Never a path: a manifest is
    /// remote input, and a `release_file` of `../../etc/passwd` must not
    /// choose where we write.
    pub fn file_name(&self) -> String {
        let candidate = if self.original_filename.is_empty() {
            self.release_file.rsplit('/').next().unwrap_or_default()
        } else {
            &self.original_filename
        };
        let stripped = candidate
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .trim_matches('.');
        if stripped.is_empty() {
            "download".to_string()
        } else {
            stripped.to_string()
        }
    }
}

/// The whole file: product name to release. `BTreeMap` rather than a struct
/// per product - EmFit has no business knowing that Toast exists.
pub type Manifest = BTreeMap<String, Release>;

/// Parse a fetched manifest.
pub fn parse(text: &str) -> Result<Manifest> {
    serde_json::from_str(text).map_err(Error::from)
}

/// Resolve a manifest-relative path against the manifest's own URL.
///
/// `https://host/releases/manifest.json` + `content/EmFit/x.exe`
/// -> `https://host/releases/content/EmFit/x.exe`.
///
/// An absolute URL in the manifest is returned untouched, so a release can be
/// moved to a CDN without changing this code.
pub fn resolve(manifest_url: &str, relative: &str) -> String {
    if relative.starts_with("http://") || relative.starts_with("https://") {
        return relative.to_string();
    }
    let base = match manifest_url.rfind('/') {
        Some(cut) => &manifest_url[..=cut],
        None => return relative.to_string(),
    };
    format!("{base}{}", relative.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "COMRADE": {
        "version": "v0.8.0",
        "released_at": "2026-06-30",
        "original_filename": "comrade-windows-x64.exe",
        "release_file": "content/COMRADE/comrade-windows-x64.exe",
        "readme_file": "content/COMRADE/README.md"
      },
      "EmFit": {
        "version": "v0.4.0",
        "released_at": "2026-08-17",
        "original_filename": "EmFit-windows-x64.exe",
        "release_file": "content/EmFit/EmFit-windows-x64.exe",
        "readme_file": "content/EmFit/README.md"
      }
    }"#;

    #[test]
    fn the_products_are_read_by_name() {
        let manifest = parse(SAMPLE).unwrap();
        assert_eq!(manifest.len(), 2);
        let ours = &manifest["EmFit"];
        assert_eq!(ours.version, "v0.4.0");
        assert_eq!(ours.file_name(), "EmFit-windows-x64.exe");
        assert_eq!(ours.readme_file.as_deref(), Some("content/EmFit/README.md"));
    }

    #[test]
    fn another_products_entry_is_ignored() {
        let manifest = parse(SAMPLE).unwrap();
        assert!(!manifest.contains_key("Toast"));
    }

    #[test]
    fn a_product_missing_the_optional_fields_still_loads() {
        let manifest =
            parse(r#"{"EmFit":{"version":"v1.0.0","release_file":"content/EmFit/a.exe"}}"#)
                .unwrap();
        let ours = &manifest["EmFit"];
        assert_eq!(ours.released_at, "");
        assert_eq!(ours.readme_file, None);
        assert_eq!(ours.sha256, None);
        assert_eq!(ours.file_name(), "a.exe");
    }

    #[test]
    fn a_published_hash_is_picked_up_without_a_code_change() {
        let manifest =
            parse(r#"{"EmFit":{"version":"v1.0.0","release_file":"a.exe","sha256":"abc123"}}"#)
                .unwrap();
        assert_eq!(manifest["EmFit"].sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn relative_paths_resolve_against_the_manifest() {
        assert_eq!(
            resolve("https://host/releases/manifest.json", "content/EmFit/a.exe"),
            "https://host/releases/content/EmFit/a.exe"
        );
        assert_eq!(
            resolve("https://host/manifest.json", "/content/a.exe"),
            "https://host/content/a.exe"
        );
    }

    #[test]
    fn an_absolute_release_url_is_left_alone() {
        assert_eq!(
            resolve("https://host/manifest.json", "https://cdn/a.exe"),
            "https://cdn/a.exe"
        );
    }

    #[test]
    fn a_manifest_cannot_choose_where_we_write() {
        let release = Release {
            version: "v1.0.0".into(),
            released_at: String::new(),
            original_filename: r"..\..\Windows\System32\evil.exe".into(),
            release_file: "content/EmFit/a.exe".into(),
            readme_file: None,
            sha256: None,
        };
        assert_eq!(release.file_name(), "evil.exe");
    }

    #[test]
    fn a_malformed_manifest_is_an_error_not_a_panic() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"EmFit":{"released_at":"2026-01-01"}}"#).is_err());
    }
}
