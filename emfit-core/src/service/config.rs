//! Application configuration (STANDARDS Â§4.4).
//!
//! A single TOML file under the OS config directory holds the user's settings
//! and any state the app wants to remember between runs (the current theme,
//! window geometry, recent files, â€¦). It is loaded once at startup and
//! rewritten whenever a setting changes.
//!
//! Design notes (this module is copied verbatim into new apps â€” keep it
//! project-agnostic):
//! - **Identity comes from [`crate::app`].** The company/product constants
//!   there resolve the on-disk path; nothing here is app-specific.
//! - **Typed, not stringly.** [`Config`] is a plain serde struct. To add a
//!   setting, add a field with a sensible default â€” that's the only change
//!   needed (load/save are generic). `theme` is the worked example.
//! - **Schema-versioned.** Every config carries [`SCHEMA_VERSION`] so a future
//!   format change can migrate instead of guessing.
//! - **First-run defaults.** A missing file is not an error: [`Config::load`]
//!   returns defaults. The app never asks the user to create the file.
//! - **Corruption is recoverable.** A malformed file logs a warning and falls
//!   back to defaults (STANDARDS Â§4.5: a bad config can be regenerated). The
//!   next save overwrites it.
//!
//! ```ignore
//! use emfit_core::service::config::Config;
//!
//! let mut cfg = Config::load();          // never fails; defaults on miss
//! cfg.theme = Some("light".to_string());
//! cfg.save()?;                            // atomic write under %APPDATA%
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::app;
use crate::error::{Error, Result};

/// Bump when a breaking change to [`Config`]'s shape lands, and add migration
/// handling keyed off the loaded value (STANDARDS Â§4.4).
pub const SCHEMA_VERSION: u32 = 1;

/// File name within the config directory.
const FILE_NAME: &str = "config.toml";

/// Persistent application configuration.
///
/// `#[serde(default)]` makes every field optional on read: an older file
/// missing a newly-added field loads cleanly (the field takes its default),
/// which keeps forward/backward compatibility cheap. Add new settings as
/// fields below â€” no other code changes are required to persist them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// On-disk schema version. Compared against [`SCHEMA_VERSION`] at load to
    /// decide whether a migration is needed.
    pub schema_version: u32,

    /// Selected UI theme: `"dark"`, `"light"`, or `None` to follow the OS.
    /// The worked example of a persisted setting â€” replace/extend per app.
    pub theme: Option<String>,

    /// Treemap coloring, edited from the Tree view's settings dialog.
    pub treemap: TreemapConfig,
    // --- add further settings here (window geometry, recent files, â€¦) ---
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            theme: None,
            treemap: TreemapConfig::default(),
        }
    }
}

/// How the treemap colors its rectangles.
///
/// Two modes (UI direction of 2026-07-29):
/// - `"ranked"` — WizTree's scheme: extensions are ranked by total
///   allocated bytes and assigned the 13-color WizTree palette in rank
///   order; any extension past the list takes the last (gray) color. The
///   palette currently lives in the frontend (`Treemap.svelte`).
/// - `"extension"` — colors by extension with a configurable
///   extension→color list. **Not in the current milestone:** today it falls
///   back to the built-in category palette (`--category-N`); the editable
///   per-extension list (and the ranked palette itself) should eventually
///   persist here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TreemapConfig {
    /// `"ranked"` (WizTree size-ranked palette) or `"extension"`.
    pub color_mode: String,
    /// Draw the synthetic free-space block in the treemap. Off by default:
    /// the map shows only real files unless the user opts in.
    pub show_free_space: bool,
}

impl Default for TreemapConfig {
    fn default() -> Self {
        Self {
            color_mode: "ranked".to_string(),
            show_free_space: false,
        }
    }
}

impl Config {
    /// Load the config from its default location. Never fails: a missing file
    /// or an unresolvable path yields defaults, and a corrupt file logs a
    /// warning and yields defaults (the bad file is left for inspection and
    /// overwritten on the next [`save`](Self::save)).
    pub fn load() -> Self {
        let path = match config_path() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "config: cannot resolve path; using defaults");
                return Config::default();
            }
        };
        match Self::load_from(&path) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "config: load failed; using defaults");
                Config::default()
            }
        }
    }

    /// Load from an explicit path. A missing file returns defaults (`Ok`);
    /// a present-but-malformed file returns `Err`. Exposed for tests and for
    /// callers that manage their own config location.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::info!(path = %path.display(), "config: no file yet; using defaults");
            return Ok(Config::default());
        }
        let text = fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Config = toml::from_str(&text)?;
        tracing::debug!(path = %path.display(), "config: loaded");
        Ok(cfg)
    }

    /// Persist to the default location, creating the config directory if
    /// needed. Writes atomically (temp file + fsync + rename) so a crash
    /// mid-write never leaves a half-written config.
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        self.save_to(&path)
    }

    /// Persist to an explicit path. Exposed for tests and custom locations.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self)?;

        // Atomic write, mirroring `service::case::Sidecar`: a crash leaves the
        // previous valid file in place, never a truncated one.
        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(text.as_bytes()).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            f.sync_all().map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        tracing::info!(path = %path.display(), "config: saved");
        Ok(())
    }
}

/// The config directory for this app under the OS conventions, seeded by the
/// [`crate::app`] identity constants. On Windows:
/// `%APPDATA%\<COMPANY>\<PRODUCT>\config\`.
pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(app::QUALIFIER, app::COMPANY, app::PRODUCT).ok_or_else(|| {
        Error::Config {
            message: "could not determine the OS config directory (no valid home directory)"
                .to_string(),
        }
    })?;
    Ok(dirs.config_dir().to_path_buf())
}

/// Full path to the config file: [`config_dir`] + [`FILE_NAME`].
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("emfit-config-test-{}-{tag}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        d.join("config.toml")
    }

    #[test]
    fn default_carries_current_schema_version() {
        assert_eq!(Config::default().schema_version, SCHEMA_VERSION);
        assert_eq!(Config::default().theme, None);
    }

    #[test]
    fn load_from_missing_file_returns_defaults() {
        let path = tmp_path("missing");
        let _ = fs::remove_file(&path);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = tmp_path("roundtrip");
        let cfg = Config {
            theme: Some("light".to_string()),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, cfg);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_field_falls_back_to_default() {
        // An older/partial file with only schema_version still loads; theme
        // takes its default. This is the forward-compat guarantee.
        let path = tmp_path("partial");
        fs::write(&path, "schema_version = 1\n").unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.theme, None);
        assert_eq!(loaded.schema_version, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_an_error_for_load_from() {
        let path = tmp_path("corrupt");
        fs::write(&path, "this is = = not valid toml ][").unwrap();
        assert!(Config::load_from(&path).is_err());
        let _ = fs::remove_file(&path);
    }
}
