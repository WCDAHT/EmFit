//! The Start Menu shortcut, kept pointing at the running executable.
//!
//! EmFit ships as a single portable file, so the thing a Start Menu entry has
//! to survive is the user moving that file. An installer writes a shortcut
//! once and is never asked again; a portable app has no installer and no
//! uninstaller, so the only moment it can notice the path changed is its own
//! launch.
//!
//! Hence [`ensure`]: on every start, look at what the shortcut points at and
//! rewrite it only when that is not where this process is running from. The
//! common case reads one file and writes nothing.
//!
//! Per-user (`FOLDERID_Programs`, under `%APPDATA%`), never the machine-wide
//! Start Menu: writing there needs Administrator, and a portable app that
//! demands a UAC prompt to be launched is not portable.

use std::path::{Path, PathBuf};

use crate::error::Result;

/// The shortcut's file name, and so the name shown in the Start Menu.
pub const LINK_NAME: &str = "EmFit.lnk";

/// What [`ensure`] did, for the caller's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The shortcut was already pointing at this executable.
    Current,
    /// There was no shortcut, and now there is.
    Created(PathBuf),
    /// The shortcut pointed somewhere else - EmFit was moved, or a second copy
    /// was run - and now points here. Carries where it used to point.
    Retargeted { link: PathBuf, was: PathBuf },
}

/// Point the Start Menu shortcut at `exe`, creating it if it is missing.
///
/// Best-effort by design: every failure here is cosmetic (the user has no
/// Start Menu entry, exactly as before), so callers log and carry on rather
/// than letting it affect startup.
pub fn ensure(exe: &Path) -> Result<Outcome> {
    #[cfg(windows)]
    {
        windows_impl::ensure(exe)
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err(crate::error::Error::UnsupportedPlatform {
            operation: "creating a Start Menu shortcut".to_string(),
        })
    }
}

/// Whether two executable paths mean the same file.
///
/// Compared case-insensitively after canonicalizing, because Windows paths are
/// case-insensitive and the shell readily hands back a path that differs from
/// ours only in case or in `..` segments - neither of which is a move, and
/// rewriting the shortcut for one would mean writing on every single launch.
/// Canonicalizing can fail (the old target no longer exists, which is exactly
/// the case worth retargeting), so each side falls back to what it had.
fn same_file(a: &Path, b: &Path) -> bool {
    let settle = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let (a, b) = (settle(a), settle(b));
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "IShellLink has no safe wrapper; every block wraps one COM call \
              with its lifetime contained in this module"
)]
mod windows_impl {
    use std::path::{Path, PathBuf};

    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree, IPersistFile, STGM_READ,
    };
    use windows::Win32::UI::Shell::{
        FOLDERID_Programs, IShellLinkW, KF_FLAG_CREATE, SHGetKnownFolderPath, ShellLink,
    };
    use windows::core::{HSTRING, Interface, PCWSTR};

    use super::{LINK_NAME, Outcome, same_file};
    use crate::error::{Error, Result};

    /// Longest target path `IShellLinkW::GetPath` will be asked to return.
    /// `MAX_PATH` is what the shell itself stores in a link.
    const MAX_PATH: usize = 260;

    pub fn ensure(exe: &Path) -> Result<Outcome> {
        ensure_in(&programs_dir()?, exe)
    }

    /// [`ensure`], against a named folder rather than the Start Menu.
    ///
    /// The seam the tests use: the COM round trip is the part worth proving,
    /// and proving it against the real Start Menu would mean a test that
    /// leaves an entry behind on whatever machine ran it.
    pub fn ensure_in(dir: &Path, exe: &Path) -> Result<Outcome> {
        // Every thread that touches COM has to have initialised it. This runs
        // off the startup path on a thread of its own, so it cannot assume the
        // main thread's apartment - and an already-initialised thread returns
        // a failure code that is not an error, which is why the result is
        // deliberately dropped rather than checked.
        //
        // SAFETY: apartment-threaded init on this thread, with no teardown
        // obligation - the process is expected to outlive this call, and
        // uninitialising would invalidate the interfaces below.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

        let link = dir.join(LINK_NAME);
        match read_target(&link) {
            Some(current) if same_file(&current, exe) => Ok(Outcome::Current),
            Some(was) => {
                write_link(&link, exe)?;
                Ok(Outcome::Retargeted { link, was })
            }
            None => {
                write_link(&link, exe)?;
                Ok(Outcome::Created(link))
            }
        }
    }

    /// `%APPDATA%\Microsoft\Windows\Start Menu\Programs`, from the shell rather
    /// than assembled from `%APPDATA%`: the folder is redirectable, and on a
    /// machine with folder redirection the assembled path is simply wrong.
    fn programs_dir() -> Result<PathBuf> {
        // SAFETY: the shell allocates the string and we free it with the
        // matching allocator below; the pointer is valid until then.
        let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_CREATE, None) }
            .map_err(|e| Error::WindowsApi {
                api: "SHGetKnownFolderPath(FOLDERID_Programs)".to_string(),
                source: std::io::Error::from_raw_os_error(e.code().0),
            })?;
        // SAFETY: `raw` is a shell-allocated null-terminated wide string.
        let path = PathBuf::from(unsafe { raw.to_string() }.map_err(|e| Error::WindowsApi {
            api: "SHGetKnownFolderPath returned an unreadable path".to_string(),
            source: std::io::Error::other(e.to_string()),
        })?);
        // SAFETY: freeing exactly what SHGetKnownFolderPath allocated, once.
        unsafe { CoTaskMemFree(Some(raw.0 as *const _)) };
        Ok(path)
    }

    /// What the shortcut at `link` points at, or `None` if there is no
    /// readable shortcut there.
    ///
    /// A file that exists but will not parse is treated as absent, so a
    /// corrupt or truncated `.lnk` is replaced rather than left in place
    /// forever as a Start Menu entry that does nothing.
    fn read_target(link: &Path) -> Option<PathBuf> {
        if !link.exists() {
            return None;
        }
        // SAFETY: each call is a COM interface method on an object created and
        // dropped inside this function.
        unsafe {
            let shell: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
            let file: IPersistFile = shell.cast().ok()?;
            file.Load(&HSTRING::from(link.as_os_str()), STGM_READ)
                .ok()?;

            let mut buf = [0u16; MAX_PATH];
            shell.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            (end > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..end])))
        }
    }

    /// Write a shortcut at `link` pointing at `exe`, replacing whatever is
    /// there. The working directory is the executable's own folder, so a
    /// launch from the Start Menu behaves like a launch from Explorer.
    fn write_link(link: &Path, exe: &Path) -> Result<()> {
        let api = |name: &str, e: windows::core::Error| Error::WindowsApi {
            api: name.to_string(),
            source: std::io::Error::from_raw_os_error(e.code().0),
        };

        // SAFETY: as above - one COM object, created, configured and saved
        // without escaping this function.
        unsafe {
            let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| api("CoCreateInstance(ShellLink)", e))?;

            shell
                .SetPath(PCWSTR(HSTRING::from(exe.as_os_str()).as_ptr()))
                .map_err(|e| api("IShellLink::SetPath", e))?;
            if let Some(dir) = exe.parent() {
                shell
                    .SetWorkingDirectory(PCWSTR(HSTRING::from(dir.as_os_str()).as_ptr()))
                    .map_err(|e| api("IShellLink::SetWorkingDirectory", e))?;
            }
            shell
                .SetDescription(PCWSTR(
                    HSTRING::from("Find files by name, instantly").as_ptr(),
                ))
                .map_err(|e| api("IShellLink::SetDescription", e))?;

            let file: IPersistFile = shell
                .cast()
                .map_err(|e| api("IShellLink::QueryInterface(IPersistFile)", e))?;
            file.Save(&HSTRING::from(link.as_os_str()), true)
                .map_err(|e| api("IPersistFile::Save", e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_the_same_file_whatever_its_case() {
        // Windows hands back whatever case the link was written with, and the
        // whole point of the check is to write nothing when nothing moved.
        assert!(same_file(
            Path::new("C:\\Tools\\EmFit.exe"),
            Path::new("c:\\tools\\emfit.EXE")
        ));
    }

    #[test]
    fn a_different_folder_is_a_different_file() {
        assert!(!same_file(
            Path::new("C:\\Tools\\EmFit.exe"),
            Path::new("C:\\Program Files\\EmFit.exe")
        ));
    }

    #[test]
    fn a_target_that_no_longer_exists_still_compares() {
        // Canonicalizing a path to a file that was deleted fails, and that is
        // exactly the case worth retargeting - so it has to compare, not
        // panic, and it has to come out unequal against a real executable.
        let gone = Path::new("C:\\gone\\EmFit.exe");
        assert!(same_file(gone, gone), "unchanged is unchanged");
        assert!(!same_file(gone, Path::new("C:\\here\\EmFit.exe")));
    }

    /// A folder of this test's own, removed when the guard drops, so a failing
    /// test cannot leave a stray shortcut anywhere - least of all in a real
    /// Start Menu, which is the whole reason [`ensure_in`] takes a folder.
    #[cfg(windows)]
    struct TempDir(PathBuf);

    #[cfg(windows)]
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("emfit-shortcut-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    #[cfg(windows)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[cfg(windows)]
    fn a_shortcut_is_written_read_back_and_left_alone() {
        use super::windows_impl::ensure_in;

        // A real file to point at: the check canonicalizes, so a target that
        // does not exist would not exercise the case that actually runs.
        let dir = TempDir::new("write");
        let exe = dir.0.join("EmFit.exe");
        std::fs::write(&exe, b"not really an executable").expect("write exe");

        // First launch: nothing there, so one gets written.
        match ensure_in(&dir.0, &exe).expect("create") {
            Outcome::Created(link) => assert!(link.exists(), "the .lnk is on disk"),
            other => panic!("expected Created, got {other:?}"),
        }

        // Second launch: the shortcut already points here, so nothing is
        // rewritten. This is the case that runs on all but the first start,
        // and the one that has to be cheap and silent.
        assert_eq!(
            ensure_in(&dir.0, &exe).expect("second run"),
            Outcome::Current
        );
    }

    #[test]
    #[cfg(windows)]
    fn a_shortcut_left_pointing_at_the_old_copy_is_retargeted() {
        use super::windows_impl::ensure_in;

        // EmFit is portable: the user moves the file and launches it from its
        // new home, and the Start Menu entry is then pointing at a path that
        // may not even exist any more.
        let dir = TempDir::new("move");
        let old = dir.0.join("old-EmFit.exe");
        let new = dir.0.join("new-EmFit.exe");
        std::fs::write(&old, b"x").expect("write old");
        std::fs::write(&new, b"x").expect("write new");

        ensure_in(&dir.0, &old).expect("create");
        match ensure_in(&dir.0, &new).expect("retarget") {
            Outcome::Retargeted { was, .. } => {
                assert!(
                    same_file(&was, &old),
                    "it reported the path it used to point at: {}",
                    was.display()
                );
            }
            other => panic!("expected Retargeted, got {other:?}"),
        }

        // And having moved it, it stays put.
        assert_eq!(ensure_in(&dir.0, &new).expect("settled"), Outcome::Current);
    }

    #[test]
    #[cfg(windows)]
    fn a_corrupt_shortcut_is_replaced_rather_than_trusted() {
        use super::windows_impl::ensure_in;

        // A truncated .lnk would otherwise sit in the Start Menu forever as an
        // entry that does nothing, because every launch would read it, fail,
        // and leave it alone.
        let dir = TempDir::new("corrupt");
        let exe = dir.0.join("EmFit.exe");
        std::fs::write(&exe, b"x").expect("write exe");
        std::fs::write(dir.0.join(LINK_NAME), b"this is not a shortcut").expect("write junk");

        assert!(matches!(
            ensure_in(&dir.0, &exe).expect("replace"),
            Outcome::Created(_)
        ));
        assert_eq!(ensure_in(&dir.0, &exe).expect("settled"), Outcome::Current);
    }
}
