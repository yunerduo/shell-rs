//! 系统注销与关机. Windows asks each window whether the session may end
//! before it does; the answer here is no while connections are in use, and
//! the workspace then asks the user the same question the close button does.
//!
//! The flags are the only part the window procedure and the workspace share:
//! the workspace keeps them current, the system reads them.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::*;

/// Terminals and uploads in use, as the workspace last saw them.
static IN_USE: AtomicBool = AtomicBool::new(false);
/// The system asked to end the session, and was told to wait.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Keep the answer to the system current. Called by the workspace while it
/// watches: a shutdown request reads this, not the workspace itself.
pub fn set_connections_in_use(in_use: bool) {
    IN_USE.store(in_use, Ordering::Relaxed);
}

/// Whether the system is waiting for an answer about ending the session.
pub fn system_shutdown_requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// Take the system's request, once the workspace can ask the user.
pub fn take_system_shutdown_request() -> bool {
    REQUESTED.swap(false, Ordering::AcqRel)
}

/// Answer the system's request with the workspace's question from now on.
/// Only the main window, and only where the system asks windows at all.
#[cfg(windows)]
pub fn install_system_shutdown_guard(window: &Window) {
    native::install(window);
}

#[cfg(not(windows))]
pub fn install_system_shutdown_guard(_window: &Window) {}

/// The system's reason line is done with, once the workspace has the answer
/// on screen: the session end it blocked was canceled, not postponed.
#[cfg(windows)]
pub fn release_shutdown_block(window: &Window) {
    native::destroy_reason(window);
}

#[cfg(not(windows))]
pub fn release_shutdown_block(_window: &Window) {}

#[cfg(windows)]
mod native {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    use gpui_kit::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Shutdown::{
        ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, SetWindowLongPtrW, WM_QUERYENDSESSION,
    };

    use crate::i18n::t;

    type Proc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

    static PREVIOUS: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static REASON: OnceLock<Vec<u16>> = OnceLock::new();

    pub(super) fn install(window: &Window) {
        if INSTALLED.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(hwnd) = hwnd(window) else {
            INSTALLED.store(false, Ordering::Release);
            return;
        };
        // SAFETY: called on the thread that made the window, before the
        // message loop runs, and only once.
        let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hook as *const () as isize) };
        PREVIOUS.store(previous as *mut c_void, Ordering::Release);
    }

    /// WM_QUERYENDSESSION is the system's question: answer no while
    /// connections are in use, and let the workspace ask the user. Every
    /// other message goes where it went before.
    unsafe extern "system" fn hook(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_QUERYENDSESSION && super::IN_USE.load(Ordering::Relaxed) {
            super::REQUESTED.store(true, Ordering::Release);
            // SAFETY: the handle is this window's, and the text outlives it.
            unsafe { ShutdownBlockReasonCreate(hwnd, reason().as_ptr()) };
            return 0;
        }
        let previous = PREVIOUS.load(Ordering::Acquire);
        if previous.is_null() {
            // SAFETY: the window's own messages, handled as before.
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        // SAFETY: `previous` is the procedure this window had before the
        // hook was installed, and both are function pointers.
        let previous: Proc = unsafe { std::mem::transmute(previous) };
        unsafe { CallWindowProcW(Some(previous), hwnd, message, wparam, lparam) }
    }

    pub(super) fn destroy_reason(window: &Window) {
        if let Some(hwnd) = hwnd(window) {
            // SAFETY: the handle is this window's.
            unsafe { ShutdownBlockReasonDestroy(hwnd) };
        }
    }

    /// What the shutdown screen says this app is waiting for.
    fn reason() -> &'static Vec<u16> {
        REASON.get_or_init(|| {
            let text = t!("app.shutdown.blocked", app = crate::app::PRODUCT_NAME).to_string();
            text.encode_utf16().chain(std::iter::once(0)).collect()
        })
    }

    fn hwnd(window: &Window) -> Option<HWND> {
        // GPUI's own `window_handle` is the app's handle, not the system's.
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as *mut c_void),
            _ => None,
        }
    }
}
