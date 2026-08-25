//! Update checking: is the version on the website newer than this one.
//!
//! The site publishes one manifest for every product it hosts. EmFit looks
//! itself up in it by name ([`crate::app::MANIFEST_KEY`]), compares the version
//! it finds against its own, and reports. That is the whole of the check.
//!
//! **It never runs on its own.** Every entry point here is reached from a
//! click: the Help menu, or the Settings opt-in that the user turned on
//! themselves. A fresh install talks to nothing.
//!
//! Once the user asks for it, applying is automatic as far as it can be. EmFit
//! ships as one portable executable, so an update is one file replacing one
//! file: [`apply::apply`] swaps the running binary and the app restarts into
//! it. Where that cannot work, because the install directory will not take a
//! write, the download stays staged and the blocker carries a sentence saying
//! why ([`apply::Blocker`]). Nothing fails silently.
//!
//! ```ignore
//! let status = update::check(&config)?;
//! if status.state == UpdateState::Available {
//!     let file = update::download(&status, &mut |done, total| {}, &cancel)?;
//!     match update::apply::apply(&file)? {
//!         Applied::Replaced { exe } => update::apply::restart(&exe)?,
//!         Applied::Blocked { blocker } => show(blocker.reason()),
//!     }
//! }
//! ```

pub mod apply;
pub mod fetch;
pub mod manifest;
pub mod verify;
pub mod version;

use std::path::PathBuf;

use crate::app;
use crate::error::Result;
use crate::service::config::Config;
use crate::service::task::CancellationToken;

/// What the check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateState {
    /// The site lists the version we already run.
    UpToDate,
    /// A newer version is published.
    Available,
    /// We are newer than the site - a developer build, normally. Reported
    /// rather than shown as up to date, so it is obvious why nothing is on
    /// offer.
    Ahead,
    /// The manifest loaded, but EmFit is not in it. Not an error: the site
    /// hosts other products, and a release may simply not be published yet.
    NotListed,
}

impl UpdateState {
    /// The wire form the shell serializes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UpToDate => "up_to_date",
            Self::Available => "available",
            Self::Ahead => "ahead",
            Self::NotListed => "not_listed",
        }
    }
}

/// The result of one check.
#[derive(Debug, Clone)]
pub struct UpdateStatus {
    pub state: UpdateState,
    /// This build's version, without the manifest's `v` prefix.
    pub current: String,
    /// The published version, as the manifest writes it. Empty when EmFit is
    /// not listed.
    pub latest: String,
    /// The publication date the manifest carries, verbatim. Empty when absent.
    pub released_at: String,
    /// The release notes, fetched only when there is an update to describe.
    /// Plain text: it is rendered as text, never as markup (STANDARDS sec 3.6 -
    /// this is remote content).
    pub notes: String,
    /// What the download will be saved as.
    pub file_name: String,
    /// Absolute URL of the release file.
    pub download_url: String,
    /// The manifest's checksum for that file, when it publishes one. See
    /// [`verify`].
    pub sha256: Option<String>,
}

impl UpdateStatus {
    fn nothing(state: UpdateState, current: &str) -> Self {
        Self {
            state,
            current: current.to_string(),
            latest: String::new(),
            released_at: String::new(),
            notes: String::new(),
            file_name: String::new(),
            download_url: String::new(),
            sha256: None,
        }
    }
}

/// This build's version. The single source of truth is the workspace
/// `Cargo.toml`, which `scripts/version.js` keeps in step with
/// `tauri.conf.json` and `package.json` (STANDARDS sec 5.2).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Where the manifest lives. The config may override it, which is how a
/// staging site gets tested without a rebuild; otherwise it is the constant in
/// [`crate::app`].
pub fn manifest_url(config: &Config) -> String {
    config
        .update
        .manifest_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(app::MANIFEST_URL)
        .to_string()
}

/// Fetch the manifest and work out where we stand.
///
/// Network failures surface as [`crate::error::Error::Network`]; a machine
/// with no route to the site is a normal thing to be told about, not something
/// to swallow.
#[tracing::instrument(skip(config))]
pub fn check(config: &Config) -> Result<UpdateStatus> {
    let url = manifest_url(config);
    let current = current_version();

    let text = fetch::manifest(&url)?;
    let manifest = manifest::parse(&text)?;

    let Some(release) = manifest.get(app::MANIFEST_KEY) else {
        tracing::info!(
            key = app::MANIFEST_KEY,
            "update: the manifest does not list this product"
        );
        return Ok(UpdateStatus::nothing(UpdateState::NotListed, current));
    };

    let state = if version::is_newer(&release.version, current) {
        UpdateState::Available
    } else if version::is_ahead(current, &release.version) {
        UpdateState::Ahead
    } else {
        UpdateState::UpToDate
    };

    tracing::info!(
        current,
        latest = %release.version,
        state = state.as_str(),
        "update: checked"
    );

    // One request, not two, unless there is actually something to describe.
    let notes = if state == UpdateState::Available {
        release
            .readme_file
            .as_deref()
            .map(|path| manifest::resolve(&url, path))
            .and_then(|notes_url| fetch::notes(&notes_url))
            .unwrap_or_default()
    } else {
        String::new()
    };

    Ok(UpdateStatus {
        state,
        current: current.to_string(),
        latest: release.version.clone(),
        released_at: release.released_at.clone(),
        notes,
        file_name: release.file_name(),
        download_url: manifest::resolve(&url, &release.release_file),
        sha256: release.sha256.clone(),
    })
}

/// Download the release `status` describes, verify it, and return where it
/// landed.
///
/// Only meaningful for [`UpdateState::Available`]; anything else has no file
/// to fetch and is refused rather than silently doing nothing.
pub fn download(
    status: &UpdateStatus,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    if status.state != UpdateState::Available || status.download_url.is_empty() {
        return Err(crate::error::Error::Config {
            message: "there is no update to download".to_string(),
        });
    }

    let dest = apply::staged_path(&status.file_name)?;
    fetch::download(&status.download_url, &dest, progress, cancel)?;
    verify::check(&dest, status.sha256.as_deref())?;

    // Older downloads are only useful until the next one arrives.
    apply::sweep();
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_version_parses() {
        assert!(
            version::parse(current_version()).is_some(),
            "the crate version must be comparable against the manifest"
        );
    }

    #[test]
    fn the_manifest_url_falls_back_to_the_built_in_one() {
        let config = Config::default();
        assert_eq!(manifest_url(&config), app::MANIFEST_URL);
    }

    #[test]
    fn a_configured_url_wins() {
        let mut config = Config::default();
        config.update.manifest_url = Some("https://staging.example/manifest.json".to_string());
        assert_eq!(
            manifest_url(&config),
            "https://staging.example/manifest.json"
        );
    }

    #[test]
    fn a_blank_override_is_ignored_rather_than_used() {
        let mut config = Config::default();
        config.update.manifest_url = Some("   ".to_string());
        assert_eq!(manifest_url(&config), app::MANIFEST_URL);
    }

    #[test]
    fn downloading_without_an_update_is_refused() {
        let status = UpdateStatus::nothing(UpdateState::UpToDate, "0.1.0");
        let mut seen = |_: u64, _: Option<u64>| {};
        assert!(download(&status, &mut seen, &CancellationToken::new()).is_err());
    }

    #[test]
    fn every_state_has_a_wire_name() {
        for state in [
            UpdateState::UpToDate,
            UpdateState::Available,
            UpdateState::Ahead,
            UpdateState::NotListed,
        ] {
            assert!(!state.as_str().is_empty());
        }
    }
}
