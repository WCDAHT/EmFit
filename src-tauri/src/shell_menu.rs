//! The native Windows shell context menu (roadmap M4, rescoped 2026-07-30).
//!
//! Right-click hands the node's path to the real `IContextMenu` — the same
//! menu Explorer shows, extensions and all — exactly as WizTree does. EmFit
//! appends two items of its own, **Copy path** and (for folders) **Zoom
//! in**, and ships no mutating entries; delete/rename/move are the shell's
//! verbs and the shell's business. EmFit actions that touch app state are
//! returned as a [`MenuAction`] for the caller to apply — this module knows
//! paths and menus, not nodes and views.
//!
//! # Threading & ownership
//!
//! The menu is tracked on the **main thread with the real app window as
//! owner**. The first cut hosted it on a helper thread owning a hidden
//! window, and the menu would sometimes open and close instantly: a menu
//! dismisses with `WM_CANCELMODE` the moment its owning thread loses
//! activation, `SetForegroundWindow` from a background thread is subject to
//! the foreground-lock rules (so it silently failed at times), and WebView2
//! — a separate *process* — re-asserts activation right after the click.
//! Owning the menu on the window the user just clicked sidesteps all three:
//! it already IS the foreground window.
//!
//! `TrackPopupMenuEx` runs its own modal message loop, so the app stays
//! responsive while the menu is up — this is how every native app shows
//! menus. Shell extensions with dynamic submenus ("Open with", "Send to")
//! populate only if `WM_INITMENUPOPUP` / `WM_DRAWITEM` / `WM_MEASUREITEM`
//! reach `IContextMenu2/3`; since the owner's window procedure belongs to
//! tao, the window is subclassed (`SetWindowSubclass`) for exactly the
//! lifetime of the menu to forward them.

/// What the user picked from EmFit's own menu items — shell verbs and Copy
/// path are handled internally and report [`MenuAction::None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    None,
    /// Zoom the treemap into this node.
    ZoomIn,
}

/// Show the shell context menu for an absolute path at the cursor, owned by
/// `hwnd` (the app window, as a raw handle so tauri's and our `windows`
/// crate versions never need to agree on a type). The zoom item reads
/// "Zoom in" for a folder (`is_dir`) and "Zoom in to parent" for a file —
/// the caller resolves which node actually gets drilled. Must be called on
/// the main thread; blocks it modally while the menu is open, invokes
/// whatever shell verb was chosen, and returns the EmFit action for the
/// caller to apply.
pub fn show_at(hwnd: isize, path: &str, is_dir: bool) -> MenuAction {
    #[cfg(windows)]
    {
        match windows_impl::show(hwnd, path, is_dir) {
            Ok(action) => action,
            Err(e) => {
                tracing::warn!(path, error = %e, "shell context menu failed");
                MenuAction::None
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, is_dir);
        tracing::debug!(path, "shell context menu is Windows-only; ignoring");
        MenuAction::None
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "hosting IContextMenu is raw COM + Win32 menu plumbing with no \
              safe equivalent; every block wraps one call or one documented \
              handoff, all on the main thread"
)]
mod windows_impl {
    use std::cell::RefCell;

    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        CMF_NORMAL, CMINVOKECOMMANDINFO, DefSubclassProc, IContextMenu, IContextMenu2,
        IContextMenu3, IShellFolder, RemoveWindowSubclass, SHBindToParent, SHParseDisplayName,
        SetWindowSubclass,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, MF_SEPARATOR, MF_STRING,
        SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
        WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
    };
    use windows::core::{Interface, PCWSTR, w};

    /// Shell verbs occupy `[ID_SHELL_FIRST, ID_SHELL_LAST]`;
    /// `TrackPopupMenuEx` returns 0 for "dismissed", which is why the range
    /// starts at 1.
    const ID_SHELL_FIRST: u32 = 1;
    const ID_SHELL_LAST: u32 = 0x6FFF;
    const ID_COPY_PATH: u32 = 0x7001;
    const ID_ZOOM_IN: u32 = 0x7002;

    /// Arbitrary-but-fixed id for our transient window subclass.
    const SUBCLASS_ID: usize = 0x454D46; // "EMF"

    thread_local! {
        /// The menu currently being tracked, for the subclass proc to
        /// forward `WM_INITMENUPOPUP`-family messages into. Main-thread only.
        static ACTIVE: RefCell<(Option<IContextMenu2>, Option<IContextMenu3>)> =
            const { RefCell::new((None, None)) };
    }

    /// Forwards menu-population messages to the active `IContextMenu2/3` so
    /// shell-extension submenus ("Open with", "Send to") fill in; everything
    /// else falls through to the window's real procedure.
    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if matches!(
            msg,
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
        ) {
            let handled = ACTIVE.with_borrow(|(icm2, icm3)| {
                if let Some(icm3) = icm3 {
                    let mut result = LRESULT(0);
                    // SAFETY: forwarding this window's own message to the
                    // COM object tracked on this same thread.
                    if unsafe { icm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)) }
                        .is_ok()
                    {
                        return Some(result);
                    }
                } else if let Some(icm2) = icm2 {
                    // SAFETY: as above, for the older interface.
                    if unsafe { icm2.HandleMenuMsg(msg, wparam, lparam) }.is_ok() {
                        return Some(LRESULT(0));
                    }
                }
                None
            });
            if let Some(result) = handled {
                return result;
            }
        }
        // SAFETY: standard subclass fallthrough.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    pub fn show(
        hwnd: isize,
        path: &str,
        is_dir: bool,
    ) -> windows::core::Result<super::MenuAction> {
        let hwnd = HWND(hwnd as *mut core::ffi::c_void);

        // The main thread is already an STA (WebView2 requires it); this is
        // a no-op returning S_FALSE there, and makes the module safe to call
        // early. RPC_E_CHANGED_MODE would mean an MTA main thread, which
        // tao/WebView2 never set up — log and bail rather than crash.
        // SAFETY: thread-local COM init with no teardown obligations at this
        // call depth (the matching uninit belongs to whoever made the STA).
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_err() {
            return Err(windows::core::Error::from_hresult(hr));
        }

        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        // Path → absolute PIDL → parent IShellFolder + child PIDL.
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated wide string; on success `pidl` is a
        // CoTaskMem allocation freed by the guard below.
        unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) }?;
        let _pidl_guard = PidlGuard(pidl);

        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: pidl is the valid absolute PIDL parsed above; the child
        // out-param is written before use and points into `pidl`.
        let folder: IShellFolder = unsafe { SHBindToParent(pidl, Some(&mut child)) }?;
        let child: *const ITEMIDLIST = child;

        // The context menu for exactly this one child.
        // SAFETY: `child` stays valid for the life of `pidl`; one-element
        // PIDL array as the API expects.
        let menu: IContextMenu =
            unsafe { folder.GetUIObjectOf(hwnd, std::slice::from_ref(&child), None) }?;
        let icm2: Option<IContextMenu2> = menu.cast().ok();
        let icm3: Option<IContextMenu3> = menu.cast().ok();

        // SAFETY: plain menu creation; destroyed at the end of this function.
        let hmenu = unsafe { CreatePopupMenu() }?;

        // Subclass the owner for exactly the menu's lifetime, so extension
        // submenus can populate through the forwarding above.
        // SAFETY: subclassing a window owned by this thread; removed below
        // before anything it references is dropped.
        let subclassed =
            unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) }.as_bool();
        if !subclassed {
            tracing::debug!("SetWindowSubclass failed; extension submenus may stay empty");
        }
        ACTIVE.set((icm2, icm3));

        let result = (|| -> windows::core::Result<super::MenuAction> {
            // SAFETY: fresh empty menu, id range documented at the consts.
            unsafe { menu.QueryContextMenu(hmenu, 0, ID_SHELL_FIRST, ID_SHELL_LAST, CMF_NORMAL) }
                .ok()?;

            // The EmFit items. Appended after the shell's, separated. A
            // file zooms to its parent directory, and says so.
            // SAFETY: valid menu handle and static strings.
            unsafe {
                AppendMenuW(hmenu, MF_SEPARATOR, 0, None)?;
                AppendMenuW(hmenu, MF_STRING, ID_COPY_PATH as usize, w!("Copy path"))?;
                AppendMenuW(
                    hmenu,
                    MF_STRING,
                    ID_ZOOM_IN as usize,
                    if is_dir {
                        w!("Zoom in")
                    } else {
                        w!("Zoom in to parent")
                    },
                )?;
            }

            let mut pt = POINT::default();
            // SAFETY: out-param write.
            unsafe { GetCursorPos(&mut pt) }?;
            // SAFETY: modal tracking on the owner's own thread — the window
            // is foreground from the very click that got us here, which is
            // what keeps the menu from being cancelled instantly.
            // TPM_RETURNCMD makes the return value the chosen id (0 =
            // dismissed).
            let chosen = unsafe {
                TrackPopupMenuEx(
                    hmenu,
                    (TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD).0,
                    pt.x,
                    pt.y,
                    hwnd,
                    None,
                )
            };

            match chosen.0 as u32 {
                0 => {} // dismissed
                ID_COPY_PATH => copy_to_clipboard(hwnd, &wide)?,
                // App-state actions are the caller's to apply — this module
                // knows menus, not views.
                ID_ZOOM_IN => return Ok(super::MenuAction::ZoomIn),
                id @ ID_SHELL_FIRST..=ID_SHELL_LAST => {
                    let info = CMINVOKECOMMANDINFO {
                        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                        hwnd,
                        // Verb by offset, the id-range protocol of
                        // QueryContextMenu.
                        lpVerb: windows::core::PCSTR((id - ID_SHELL_FIRST) as usize as *const u8),
                        nShow: SW_SHOWNORMAL.0,
                        ..Default::default()
                    };
                    // SAFETY: struct fully initialized; verb id came from
                    // this menu's own QueryContextMenu range.
                    unsafe { menu.InvokeCommand(&info) }?;
                }
                other => tracing::debug!(other, "menu returned an id outside every range"),
            }
            Ok(super::MenuAction::None)
        })();

        ACTIVE.set((None, None));
        if subclassed {
            // SAFETY: removing the subclass installed above, same thread.
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID) };
        }
        // SAFETY: destroying the menu created above.
        let _ = unsafe { DestroyMenu(hmenu) };
        result
    }

    /// CF_UNICODETEXT clipboard write of an already-NUL-terminated string.
    fn copy_to_clipboard(hwnd: HWND, wide: &[u16]) -> windows::core::Result<()> {
        let bytes = std::mem::size_of_val(wide);
        // SAFETY: allocating a movable global block big enough for the
        // string; ownership passes to the clipboard on success.
        let hglobal: HGLOBAL = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }?;
        // SAFETY: locking the block just allocated; copy stays in bounds.
        unsafe {
            let dst = GlobalLock(hglobal) as *mut u16;
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(hglobal);
        }
        // SAFETY: standard clipboard protocol; on SetClipboardData success
        // the system owns the allocation.
        unsafe {
            OpenClipboard(Some(hwnd))?;
            let result = EmptyClipboard()
                .and_then(|()| SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0))));
            let _ = CloseClipboard();
            result.map(|_| ())
        }
    }

    /// Frees the CoTaskMem PIDL on drop.
    struct PidlGuard(*mut ITEMIDLIST);
    impl Drop for PidlGuard {
        fn drop(&mut self) {
            // SAFETY: freeing the allocation SHParseDisplayName handed us.
            unsafe { CoTaskMemFree(Some(self.0.cast())) };
        }
    }
}
