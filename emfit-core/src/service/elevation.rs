//! Whether we can read raw volumes, and how to ask for permission.
//!
//! Opening `\\.\C:` or `\\.\PhysicalDrive0` requires Administrator. Without
//! it, the scan fails with a bare access-denied that tells the user nothing.
//! Checking up front means the app can offer a one-click relaunch instead
//! (`features.md` sec 1.1).
//!
//! Note that *discovery* needs no elevation - `service::volume` opens volumes
//! with zero desired access, so the drive list, sizes, and partition offsets
//! are all available before the user decides. Only reading bytes needs rights.

use crate::error::Result;

/// Whether this process can open raw devices.
///
/// Windows: whether the process token is elevated. Elsewhere: `false`, since
/// no raw scanner exists on those platforms - the directory walker is the only
/// path and it needs no special rights.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        windows_impl::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Relaunch this executable with an elevation prompt, passing the current
/// arguments through.
///
/// Returns once the new process has been *launched*, not once it is ready; the
/// caller should exit promptly afterwards so the two instances do not both
/// hold the UI. If the user dismisses the UAC prompt this returns an error and
/// the caller carries on unelevated.
pub fn relaunch_elevated() -> Result<()> {
    #[cfg(windows)]
    {
        windows_impl::relaunch_elevated()
    }
    #[cfg(not(windows))]
    {
        Err(crate::error::Error::UnsupportedPlatform {
            operation: "elevated relaunch".to_string(),
        })
    }
}

/// Run a program elevated and wait for it, returning its exit code.
///
/// One UAC prompt, at the moment the user asked for the thing that needs it -
/// which is why this exists rather than relaunching the whole app elevated to
/// register a scheduled task. Dismissing the prompt is an error, not a zero
/// exit code, so a caller cannot mistake a refusal for success.
pub fn run_elevated(exe: &str, args: &str) -> Result<u32> {
    #[cfg(windows)]
    {
        windows_impl::run_elevated(exe, args)
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, args);
        Err(crate::error::Error::UnsupportedPlatform {
            operation: "elevated execution".to_string(),
        })
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "token inspection and ShellExecuteEx have no safe std equivalent; \
              each block wraps one call and documents its preconditions"
)]
mod windows_impl {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
    };
    use windows::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};
    use windows::core::PCWSTR;

    use crate::error::{Error, Result};

    pub(super) fn is_elevated() -> bool {
        let mut token = HANDLE::default();

        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
        // closing; `token` is a live local receiving the real handle.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened.is_err() {
            return false;
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;

        // SAFETY: `token` is the open handle from above. The buffer is a live
        // TOKEN_ELEVATION and its true size is passed, which is what
        // TokenElevation expects.
        let queried = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                Some(std::ptr::from_mut(&mut elevation).cast()),
                u32::try_from(size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
                &mut returned,
            )
        };

        // SAFETY: `token` came from OpenProcessToken and is closed exactly once.
        let _ = unsafe { CloseHandle(token) };

        queried.is_ok() && elevation.TokenIsElevated != 0
    }

    pub(super) fn relaunch_elevated() -> Result<()> {
        let exe = std::env::current_exe().map_err(|source| Error::Io {
            path: std::path::PathBuf::from("<current exe>"),
            source,
        })?;

        // Pass our arguments through so the relaunch lands where the user was.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let params = args.join(" ");

        let verb = to_wide("runas");
        let file = to_wide(&exe.to_string_lossy());
        let parameters = to_wide(&params);

        let mut info = SHELLEXECUTEINFOW {
            cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
            // Without NOASYNC the call can return before the shell has acted
            // on it, and a caller that exits immediately would cancel itself.
            fMask: SEE_MASK_NOASYNC,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };

        // SAFETY: `info` is a live, fully initialized struct whose cbSize
        // matches; every PCWSTR points at a NUL-terminated buffer that outlives
        // the call.
        unsafe { ShellExecuteExW(&mut info) }.map_err(|e| Error::WindowsApi {
            api: "ShellExecuteExW(runas)".to_string(),
            source: std::io::Error::from_raw_os_error(e.code().0),
        })
    }

    pub(super) fn run_elevated(exe: &str, args: &str) -> Result<u32> {
        let verb = to_wide("runas");
        let file = to_wide(exe);
        let parameters = to_wide(args);

        let mut info = SHELLEXECUTEINFOW {
            cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
            // NOCLOSEPROCESS is what makes `hProcess` valid afterwards, which
            // is the only way to learn whether the thing actually worked.
            fMask: SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            // A console flashing up for a fraction of a second is worse than
            // no window at all; the caller reports the outcome itself.
            nShow: SW_HIDE.0,
            ..Default::default()
        };

        // SAFETY: `info` is fully initialized with a matching cbSize, and each
        // PCWSTR points at a NUL-terminated buffer that outlives the call.
        unsafe { ShellExecuteExW(&mut info) }.map_err(|e| Error::WindowsApi {
            api: "ShellExecuteExW(runas)".to_string(),
            source: std::io::Error::from_raw_os_error(e.code().0),
        })?;

        if info.hProcess.is_invalid() {
            return Err(Error::WindowsApi {
                api: "ShellExecuteExW(runas)".to_string(),
                source: std::io::Error::other("no process handle was returned"),
            });
        }

        // SAFETY: `hProcess` is the live handle ShellExecuteExW just returned.
        unsafe { WaitForSingleObject(info.hProcess, INFINITE) };

        let mut code = 0u32;
        // SAFETY: same handle, and `code` is a live local.
        let read = unsafe { GetExitCodeProcess(info.hProcess, &mut code) };
        // SAFETY: closing the handle NOCLOSEPROCESS handed over, exactly once.
        let _ = unsafe { CloseHandle(info.hProcess) };

        read.map_err(|e| Error::WindowsApi {
            api: "GetExitCodeProcess".to_string(),
            source: std::io::Error::from_raw_os_error(e.code().0),
        })?;
        Ok(code)
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_check_does_not_panic() {
        // The value depends on how the test runner was started, so only the
        // absence of a crash is assertable here.
        let _ = is_elevated();
    }

    #[cfg(not(windows))]
    #[test]
    fn relaunch_is_unsupported_off_windows() {
        assert!(matches!(
            relaunch_elevated(),
            Err(crate::error::Error::UnsupportedPlatform { .. })
        ));
    }
}
