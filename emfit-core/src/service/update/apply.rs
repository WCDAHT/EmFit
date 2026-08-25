//! Installing a downloaded release over the running one.
//!
//! EmFit ships as a portable executable, so an update is one file replacing
//! one file. [`apply`] does that in place and asks the app to restart. When it
//! cannot, it says why in a sentence a user can act on and leaves the download
//! staged so they can finish by hand - it never fails silently and never
//! leaves the app without a working executable.
//!
//! **The Windows swap.** A running `.exe` cannot be deleted, but it *can* be
//! renamed: the open image handle follows the file. So the sequence is rename
//! the running binary aside, copy the new one into its place, restart, and
//! delete the renamed one on the next start ([`clean_previous`]). Every step
//! before the copy is reversible, and the copy is the only one that is not -
//! which is why [`replaceability`] pre-flights the directory with a real write
//! before anything moves.
//!
//! **What blocks it**, all reported rather than guessed at:
//! - a directory this process cannot write - `Program Files` without
//!   Administrator, read-only media, a locked-down share;
//! - an executable path the OS will not report;
//! - a non-Windows build, where none of this is wired.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use directories::ProjectDirs;

use crate::app;
use crate::error::{Error, Result};
use crate::service::elevation;

/// Directory name under the product's local data folder.
const STAGING_DIR_NAME: &str = "updates";

/// Suffix the outgoing executable is renamed to. Deleted on the next start.
const BACKUP_SUFFIX: &str = ".old";

/// How long a staged download is kept before [`sweep`] removes it. Long enough
/// that a user who downloaded yesterday and forgot still finds the file;
/// short enough that the folder does not become an archive of every release.
const KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Where downloads are staged: `%LOCALAPPDATA%\<COMPANY>\<PRODUCT>\updates\`,
/// alongside the scan cache and the logs.
pub fn staging_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(app::QUALIFIER, app::COMPANY, app::PRODUCT).ok_or_else(|| {
        Error::Config {
            message: "could not determine the OS data directory (no valid home directory)"
                .to_string(),
        }
    })?;
    Ok(dirs.data_local_dir().join(STAGING_DIR_NAME))
}

/// Full path a release with this file name will be staged at.
pub fn staged_path(file_name: &str) -> Result<PathBuf> {
    Ok(staging_dir()?.join(file_name))
}

/// Why the running executable cannot be replaced where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// The swap is wired for Windows only.
    UnsupportedPlatform,
    /// The OS would not say where this executable lives.
    UnknownInstallDir { detail: String },
    /// The directory holding the executable refuses a write.
    NotWritable {
        dir: PathBuf,
        /// Whether this process is already elevated. When it is not, needing
        /// Administrator is the likely answer and worth saying; when it is,
        /// the folder is genuinely read-only and elevation would not help.
        elevated: bool,
        detail: String,
    },
    /// The swap started and something went wrong partway. Carries what the OS
    /// said, because at this point the user needs the specifics.
    SwapFailed { detail: String },
}

impl Blocker {
    /// One sentence for the dialog. Says what happened and what to do about
    /// it; the technical detail goes to the log, not the window.
    pub fn reason(&self) -> String {
        match self {
            Self::UnsupportedPlatform => {
                "Replacing the running program is only wired up on Windows.".to_string()
            }
            Self::UnknownInstallDir { .. } => {
                "Windows would not say where EmFit is running from, so it could not be replaced."
                    .to_string()
            }
            Self::NotWritable {
                dir,
                elevated: false,
                ..
            } => format!(
                "EmFit runs from {}, which needs Administrator to write to. \
                 Run EmFit as Administrator and update again, or replace the file yourself.",
                dir.display()
            ),
            Self::NotWritable {
                dir,
                elevated: true,
                ..
            } => format!(
                "The folder EmFit runs from ({}) is read-only, so it could not be replaced.",
                dir.display()
            ),
            // The OS half of this may or may not punctuate itself; the dialog
            // shows one sentence either way.
            Self::SwapFailed { detail } => format!(
                "EmFit could not replace itself: {}.",
                detail.trim_end_matches(['.', ' '])
            ),
        }
    }

    /// The detail worth logging, which is more than the sentence above says.
    fn detail(&self) -> String {
        match self {
            Self::UnsupportedPlatform => "not windows".to_string(),
            Self::UnknownInstallDir { detail }
            | Self::NotWritable { detail, .. }
            | Self::SwapFailed { detail } => detail.clone(),
        }
    }
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The executable was replaced. It takes effect on the next start, which
    /// is what [`restart`] is for.
    Replaced { exe: PathBuf },
    /// It was not, for a reason the user can be told. The download is still
    /// staged at the path [`apply`] was given.
    Blocked { blocker: Blocker },
}

/// Whether the running executable can be replaced in place, and where it is.
///
/// The writability test is a real file written into the install directory, not
/// a guess from the path: "is this under Program Files" answers a different
/// question than "can I write here", and gets both portable installs and
/// locked-down shares wrong.
pub fn replaceability() -> std::result::Result<PathBuf, Blocker> {
    if !cfg!(windows) {
        return Err(Blocker::UnsupportedPlatform);
    }

    let exe = std::env::current_exe().map_err(|e| Blocker::UnknownInstallDir {
        detail: format!("current_exe failed: {e}"),
    })?;
    let dir = exe
        .parent()
        .ok_or_else(|| Blocker::UnknownInstallDir {
            detail: format!("{} has no parent directory", exe.display()),
        })?
        .to_path_buf();

    let probe = dir.join(format!(".emfit-write-probe-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(exe)
        }
        Err(e) => {
            let elevated = elevation::is_elevated();
            Err(Blocker::NotWritable {
                dir,
                elevated,
                detail: format!("write probe failed: {e}"),
            })
        }
    }
}

/// Replace the running executable with the staged download.
///
/// Never returns `Err` for a reason the user should hear about: anything that
/// stops the swap comes back as [`Applied::Blocked`] carrying a sentence, and
/// is logged with the detail behind it. `Err` is reserved for the download
/// itself having gone missing, which means the caller lost track of its own
/// file.
pub fn apply(staged: &Path) -> Result<Applied> {
    if !staged.is_file() {
        return Err(Error::Io {
            path: staged.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the downloaded file is no longer there",
            ),
        });
    }

    let exe = match replaceability() {
        Ok(exe) => exe,
        Err(blocker) => {
            tracing::warn!(
                reason = %blocker.reason(),
                detail = %blocker.detail(),
                staged = %staged.display(),
                "update: cannot replace in place; leaving the download staged"
            );
            return Ok(Applied::Blocked { blocker });
        }
    };

    Ok(match swap(&exe, staged) {
        Ok(()) => {
            tracing::info!(
                exe = %exe.display(),
                staged = %staged.display(),
                "update: replaced the running executable; restart to finish"
            );
            Applied::Replaced { exe }
        }
        Err(blocker) => {
            tracing::error!(
                reason = %blocker.reason(),
                detail = %blocker.detail(),
                "update: the swap failed"
            );
            Applied::Blocked { blocker }
        }
    })
}

/// Rename the running binary aside, copy the new one into its place, and put
/// the old one back if that copy fails.
fn swap(exe: &Path, staged: &Path) -> std::result::Result<(), Blocker> {
    let backup = backup_path(exe);

    // A leftover from a previous update, still mapped by a process that has
    // not exited. Removing it is best-effort; the rename below is what
    // actually has to succeed.
    let _ = fs::remove_file(&backup);

    fs::rename(exe, &backup).map_err(|e| Blocker::SwapFailed {
        detail: format!(
            "could not move the current {} aside: {e}",
            exe.file_name().unwrap_or_default().to_string_lossy()
        ),
    })?;

    // Copy rather than rename: the staging directory is under %LOCALAPPDATA%
    // and the install may be on another volume, where rename fails outright.
    if let Err(e) = fs::copy(staged, exe) {
        // The only irreversible step, and it did not take. Put the working
        // executable back before reporting, so a failed update leaves the app
        // exactly as it was.
        let restored = fs::rename(&backup, exe);
        return Err(Blocker::SwapFailed {
            detail: match restored {
                Ok(()) => format!("could not write the new file: {e}"),
                Err(restore) => format!(
                    "could not write the new file ({e}), and putting the old one back \
                     also failed ({restore}); it is at {}",
                    backup.display()
                ),
            },
        });
    }

    Ok(())
}

/// `EmFit.exe` -> `EmFit.exe.old`. Appended rather than substituted, so the
/// leftover is obviously a leftover and `clean_previous` has one pattern to
/// look for.
fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_os_string();
    name.push(BACKUP_SUFFIX);
    PathBuf::from(name)
}

/// Delete the executable a previous update renamed aside.
///
/// Called once at startup. Best-effort by nature: if the process that was
/// replaced has not fully exited, Windows still has the image mapped and the
/// delete fails - the next start picks it up.
pub fn clean_previous() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let backup = backup_path(&exe);
    if !backup.exists() {
        return;
    }
    match fs::remove_file(&backup) {
        Ok(()) => tracing::info!(path = %backup.display(), "update: removed the previous version"),
        Err(e) => tracing::debug!(
            path = %backup.display(),
            error = %e,
            "update: the previous version is still in use; it will go on a later start"
        ),
    }
}

/// Start the replaced executable and hand over.
///
/// The caller exits immediately afterwards. Launched from this process, so it
/// inherits the token: an EmFit updated while running elevated comes back
/// elevated, which is the state a raw volume scan needs.
pub fn restart(exe: &Path) -> Result<()> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    std::process::Command::new(exe)
        .current_dir(dir)
        .spawn()
        .map_err(|source| Error::Io {
            path: exe.to_path_buf(),
            source,
        })?;
    tracing::info!(path = %exe.display(), "update: restarting into the new version");
    Ok(())
}

/// Delete staged downloads older than [`KEEP`], plus any abandoned `.part`
/// file regardless of age. Best-effort: called after a successful download,
/// and a failure to tidy is never worth failing the update over.
pub fn sweep() {
    let Ok(dir) = staging_dir() else { return };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_part = path.extension().is_some_and(|e| e == "part");
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age > KEEP);

        if (is_part || stale) && fs::remove_file(&path).is_ok() {
            tracing::debug!(path = %path.display(), "update: swept a stale download");
        }
    }
}

/// Run a staged release without installing it.
///
/// The fallback when [`apply`] is blocked: the user can still try the new
/// version, and EmFit stays open because two portable copies running at once
/// is a state they can see and close.
pub fn launch(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Err(Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the downloaded file is no longer there",
            ),
        });
    }

    let dir = path.parent().unwrap_or(Path::new("."));
    std::process::Command::new(path)
        .current_dir(dir)
        .spawn()
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    tracing::info!(path = %path.display(), "update: launched the downloaded release");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_stage_under_the_product_data_dir() {
        let dir = staging_dir().unwrap();
        assert!(dir.ends_with(STAGING_DIR_NAME));
        assert!(dir.to_string_lossy().contains(app::PRODUCT));
    }

    #[test]
    fn a_staged_path_is_the_file_inside_that_dir() {
        let path = staged_path("EmFit-windows-x64.exe").unwrap();
        assert_eq!(path.parent().unwrap(), staging_dir().unwrap());
        assert_eq!(path.file_name().unwrap(), "EmFit-windows-x64.exe");
    }

    #[test]
    fn the_backup_keeps_the_whole_name() {
        // `with_extension` would turn EmFit.exe into EmFit.old and collide
        // with anything else named that.
        assert_eq!(
            backup_path(Path::new(r"C:\Tools\EmFit.exe")),
            PathBuf::from(r"C:\Tools\EmFit.exe.old")
        );
    }

    #[test]
    fn applying_a_download_that_vanished_is_an_error() {
        let missing = staging_dir().unwrap().join("no-such-release.exe");
        assert!(apply(&missing).is_err());
        assert!(launch(&missing).is_err());
    }

    #[test]
    fn every_blocker_says_something_a_user_can_act_on() {
        let blockers = [
            Blocker::UnsupportedPlatform,
            Blocker::UnknownInstallDir { detail: "x".into() },
            Blocker::NotWritable {
                dir: PathBuf::from(r"C:\Program Files\EmFit"),
                elevated: false,
                detail: "x".into(),
            },
            Blocker::NotWritable {
                dir: PathBuf::from(r"E:\EmFit"),
                elevated: true,
                detail: "x".into(),
            },
            Blocker::SwapFailed {
                detail: "access denied".into(),
            },
        ];
        for blocker in blockers {
            let reason = blocker.reason();
            assert!(!reason.is_empty());
            assert!(
                reason.ends_with('.'),
                "the dialog shows this as a sentence: {reason}"
            );
        }
    }

    #[test]
    fn an_unelevated_program_files_install_is_told_to_elevate() {
        let blocker = Blocker::NotWritable {
            dir: PathBuf::from(r"C:\Program Files\EmFit"),
            elevated: false,
            detail: String::new(),
        };
        assert!(blocker.reason().contains("Administrator"));
    }

    #[test]
    fn a_read_only_folder_is_not_blamed_on_elevation() {
        let blocker = Blocker::NotWritable {
            dir: PathBuf::from(r"E:\EmFit"),
            elevated: true,
            detail: String::new(),
        };
        assert!(
            !blocker.reason().contains("Administrator"),
            "elevation would not help here, so do not send the user after it"
        );
    }

    #[test]
    fn the_write_probe_leaves_nothing_behind() {
        let _ = replaceability();
        let dir = std::env::current_exe().unwrap();
        let dir = dir.parent().unwrap();
        let leftovers: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".emfit-write-probe")
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn a_failed_copy_puts_the_original_back() {
        // The swap's one irreversible step, forced to fail: the staged file is
        // a directory, so the copy cannot succeed and the rename must undo.
        let dir = std::env::temp_dir().join(format!("emfit-swap-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let exe = dir.join("EmFit.exe");
        fs::write(&exe, b"original").unwrap();
        let staged = dir.join("staged-as-a-directory");
        let _ = fs::create_dir_all(&staged);

        let outcome = swap(&exe, &staged);

        assert!(matches!(outcome, Err(Blocker::SwapFailed { .. })));
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        assert!(!backup_path(&exe).exists(), "the backup must not linger");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_swap_leaves_the_old_binary_beside_the_new() {
        let dir = std::env::temp_dir().join(format!("emfit-swap-ok-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let exe = dir.join("EmFit.exe");
        fs::write(&exe, b"original").unwrap();
        let staged = dir.join("EmFit-new.exe");
        fs::write(&staged, b"replacement").unwrap();

        swap(&exe, &staged).unwrap();

        assert_eq!(fs::read(&exe).unwrap(), b"replacement");
        assert_eq!(fs::read(backup_path(&exe)).unwrap(), b"original");
        let _ = fs::remove_dir_all(&dir);
    }
}
