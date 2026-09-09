//! The mark on the taskbar button.
//!
//! A tab that wants you is invisible from another application — which is
//! exactly where you are when one starts wanting you. Windows has somewhere to
//! put that: a small overlay in the corner of the taskbar button, the same
//! place a chat app puts its unread dot. It stays until it is taken away,
//! unlike the highlight from asking for attention, which ends at the next
//! click. Nowhere else has an equivalent worth a dependency; elsewhere the
//! window title and the attention request carry it.

/// Show or clear the "a tab wants you" mark. `hwnd` is a Win32 window handle,
/// ignored everywhere else: macOS marks the dock tile, which belongs to the
/// application rather than to a window, and Linux has the title and the
/// urgency hint the caller already sets.
pub fn set(hwnd: isize, waiting: usize) {
    #[cfg(windows)]
    imp::set(hwnd, waiting);
    #[cfg(target_os = "macos")]
    mac::set(waiting);
    let _ = (hwnd, waiting);
}

/// The dock tile's badge: the same red bubble Mail puts an unread count in.
/// It outlasts a bouncing icon, which is over in a second.
#[cfg(target_os = "macos")]
mod mac {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    pub fn set(waiting: usize) {
        // Only from the thread AppKit belongs to, which is the one painting.
        let Some(marker) = MainThreadMarker::new() else {
            return;
        };
        let tile = NSApplication::sharedApplication(marker).dockTile();
        let label = (waiting > 0).then(|| NSString::from_str(&waiting.to_string()));
        unsafe { tile.setBadgeLabel(label.as_deref()) };
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    use windows::Win32::Foundation::{HMODULE, HWND};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
    use windows::Win32::UI::WindowsAndMessaging::{HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW};
    use windows::core::{PCWSTR, w};

    /// COM objects are not `Send`, and this one never leaves the UI thread —
    /// the promise a `OnceLock` needs and this module keeps.
    struct Taskbar(ITaskbarList3);
    unsafe impl Send for Taskbar {}
    unsafe impl Sync for Taskbar {}

    /// The badge icon, compiled into this binary as resource 2 (`build.rs`).
    struct Badge(HICON);
    unsafe impl Send for Badge {}
    unsafe impl Sync for Badge {}

    fn taskbar() -> Option<&'static ITaskbarList3> {
        static TASKBAR: OnceLock<Option<Taskbar>> = OnceLock::new();
        TASKBAR
            .get_or_init(|| {
                // SAFETY: called once, on the thread that owns the window.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    let list: ITaskbarList3 =
                        CoCreateInstance(&TaskbarList, None, CLSCTX_ALL).ok()?;
                    list.HrInit().ok()?;
                    Some(Taskbar(list))
                }
            })
            .as_ref()
            .map(|t| &t.0)
    }

    fn badge() -> Option<HICON> {
        static BADGE: OnceLock<Option<Badge>> = OnceLock::new();
        BADGE
            .get_or_init(|| {
                // SAFETY: loading an icon resource out of our own module.
                unsafe {
                    let module: HMODULE = GetModuleHandleW(PCWSTR::null()).ok()?;
                    let handle = LoadImageW(
                        Some(module.into()),
                        // MAKEINTRESOURCE(2): the id `build.rs` gave it.
                        PCWSTR(2 as *const u16),
                        IMAGE_ICON,
                        0,
                        0,
                        LR_DEFAULTCOLOR,
                    )
                    .ok()?;
                    Some(Badge(HICON(handle.0)))
                }
            })
            .as_ref()
            .map(|b| b.0)
    }

    pub fn set(hwnd: isize, waiting: usize) {
        let Some(taskbar) = taskbar() else { return };
        let icon = if waiting > 0 { badge() } else { None };
        // SAFETY: a live window handle from the running viewport, and an icon
        // owned by this process for its lifetime.
        unsafe {
            let _ = taskbar.SetOverlayIcon(HWND(hwnd as *mut _), icon, w!("waiting for you"));
        }
    }
}
