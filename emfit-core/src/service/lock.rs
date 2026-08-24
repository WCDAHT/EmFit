//! Cross-process locks, so two EmFit processes do not do one job twice.
//!
//! The app in front of the user and the background scan behind it can both
//! decide to scan `C:` at the same moment. Nothing breaks if they do - each
//! snapshot writer has its own temporary and the rename is atomic
//! (`service::cache::format`) - but one of them is wasting a few seconds of
//! disk bandwidth the other is already spending.
//!
//! **This is an optimization, and the API is shaped to keep it one.** A lock
//! is refused only when another process demonstrably holds it. A lock that
//! cannot be created at all - an OS refusal, a platform with no named
//! objects - reports success, because declining to work when the coordination
//! is unavailable would be the worse failure by far.
//!
//! A named mutex rather than a lock file, deliberately: a lock file left
//! behind by a process that was killed disables the thing it guards forever,
//! silently, with nothing to diagnose. A named mutex belongs to the kernel,
//! which releases it when its owner dies - a stale one cannot exist.

/// A held cross-process lock. Released when dropped, and by the OS if this
/// process dies still holding it.
#[derive(Debug)]
pub struct ProcessLock {
    /// Held for its `Drop`, which releases and closes the mutex. Nothing
    /// reads it, and nothing should: the lock *is* the handle being alive.
    #[cfg(windows)]
    #[allow(dead_code, reason = "the value's lifetime is the whole point")]
    handle: windows_impl::Handle,
    /// What it guards, for logging.
    name: String,
}

impl ProcessLock {
    /// Take the named lock if it is free *right now*.
    ///
    /// Never waits: `None` means another process holds it, and the caller is
    /// expected to skip rather than queue. See the module docs for why a
    /// failure to create the lock is reported as success.
    pub fn try_acquire(name: &str) -> Option<Self> {
        #[cfg(windows)]
        {
            windows_impl::try_acquire(name).map(|handle| Self {
                handle,
                name: name.to_string(),
            })
        }
        #[cfg(not(windows))]
        {
            // No raw scanner exists off Windows, so nothing here ever races.
            Some(Self {
                name: name.to_string(),
            })
        }
    }

    /// What this lock guards.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The lock one volume's scan takes, in whichever process is scanning it.
pub fn volume_scan(display_name: &str) -> String {
    format!("EmFit.scan.{display_name}")
}

/// The lock a background run takes, so two of them cannot overlap.
pub const BACKGROUND_RUN: &str = "EmFit.background";

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "named mutexes have no safe std equivalent; each block wraps one \
              call and documents its preconditions"
)]
mod windows_impl {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    use windows::core::PCWSTR;

    /// Owns the mutex handle so `Drop` can release and close it exactly once.
    #[derive(Debug)]
    pub(super) struct Handle(HANDLE);

    // SAFETY: a mutex HANDLE is a kernel object usable from any thread; this
    // wrapper hands out no interior references and closes it exactly once.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from CreateMutexW and was waited on
            // successfully, so this process owns it; both calls run once.
            unsafe {
                let _ = ReleaseMutex(self.0);
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub(super) fn try_acquire(name: &str) -> Option<Handle> {
        // `Global\` so the lock is shared across sessions: a scheduled task
        // and the user's window need not be in the same one. Creating a global
        // object needs a privilege ordinary users lack, and both sides of this
        // are elevated already (a raw volume read requires it) - but a refusal
        // falls back rather than failing, and the session-local name is still
        // better than nothing.
        for full in [format!("Global\\{name}"), format!("Local\\{name}")] {
            let wide: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();

            // SAFETY: `wide` is a NUL-terminated buffer that outlives the call.
            let handle = match unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) } {
                Ok(handle) => handle,
                Err(e) => {
                    tracing::debug!(name = %full, error = %e, "lock: could not create");
                    continue;
                }
            };

            // SAFETY: `handle` is the mutex just created. A zero timeout makes
            // this a test, not a wait.
            let waited = unsafe { WaitForSingleObject(handle, 0) };
            return match waited {
                // WAIT_ABANDONED: the previous owner died holding it. The
                // kernel hands ownership over anyway, which is exactly the
                // stale-lock case a lock file could not recover from.
                WAIT_OBJECT_0 | WAIT_ABANDONED => Some(Handle(handle)),
                _ => {
                    // SAFETY: closing the handle this call opened, once.
                    unsafe { CloseHandle(handle) }.ok();
                    None
                }
            };
        }

        // Nothing could be created, so nothing is known. Say yes: see the
        // module docs.
        tracing::warn!(name, "lock: unavailable; proceeding without it");
        Some(Handle(HANDLE::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_is_free_again_once_it_is_dropped() {
        let name = format!("EmFit.test.{}", std::process::id());
        let first = ProcessLock::try_acquire(&name).expect("free to start with");
        assert_eq!(first.name(), name);
        drop(first);
        assert!(
            ProcessLock::try_acquire(&name).is_some(),
            "dropping it must release it"
        );
    }

    #[test]
    fn volume_locks_are_named_per_volume() {
        assert_ne!(volume_scan("C:"), volume_scan("D:"));
        assert_ne!(volume_scan("C:"), BACKGROUND_RUN);
    }
}
