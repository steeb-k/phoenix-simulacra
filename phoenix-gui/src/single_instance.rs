//! Single-instance guard.
//!
//! Launching a second copy of the GUI focuses the already-running window and
//! exits instead of opening another: the user backs up (and mounts) multiple
//! drives from one window, so a second window is never wanted.
//!
//! Implemented with a named mutex in the session namespace. The first instance
//! creates it and holds the returned [`Guard`] for the whole process lifetime;
//! a later launch sees `ERROR_ALREADY_EXISTS`, brings the existing window to the
//! foreground, and bows out.

#[cfg(target_os = "windows")]
pub use imp::{acquire, Guard};

#[cfg(target_os = "windows")]
mod imp {
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE,
    };
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    // Session-namespace name: both instances run elevated (via the launcher) in
    // the same interactive session, so they share this object.
    const MUTEX_NAME: &str = "PhoenixSimulacra.SingleInstance";
    // Matches the app name passed to `eframe::run_native`, which becomes the
    // window title even though the window is chromeless.
    const WINDOW_TITLE: &str = "Phoenix Simulacra";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Owns the single-instance mutex for the process lifetime. Dropping it
    /// releases the handle so the next launch can become the owner.
    pub struct Guard(HANDLE);

    // The raw handle is created once and closed once on drop; the app holds a
    // single Guard for the whole run, so there is no concurrent handle access.
    unsafe impl Send for Guard {}
    unsafe impl Sync for Guard {}

    impl Drop for Guard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` came from CreateMutexW and is closed exactly once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// Try to become the single instance. `Some(Guard)` — we are the first and
    /// should keep the guard alive for the process lifetime. `None` — another
    /// instance already owns it; we have asked the OS to focus that window and
    /// the caller should exit.
    pub fn acquire() -> Option<Guard> {
        let name = wide(MUTEX_NAME);
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer valid for the call.
        let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        if handle.is_null() {
            // Couldn't create the mutex at all — fail open and let the app run.
            return Some(Guard(handle));
        }
        // GetLastError is meaningful even after a successful CreateMutexW: it
        // reports ERROR_ALREADY_EXISTS when the named object predates this call.
        // SAFETY: no intervening Win32 call has clobbered the last-error value.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // Ours is a duplicate handle to the existing mutex; drop it (the
            // first instance keeps ownership) and surface its window instead.
            // SAFETY: closing our own duplicate handle, exactly once.
            unsafe { CloseHandle(handle) };
            focus_existing();
            None
        } else {
            Some(Guard(handle))
        }
    }

    /// Bring the already-running window to the foreground, restoring it first if
    /// minimized. Best-effort: if the window can't be found (e.g. the first
    /// instance is still starting up), do nothing.
    fn focus_existing() {
        let title = wide(WINDOW_TITLE);
        // SAFETY: null class name + NUL-terminated title; returns null if absent.
        let hwnd = unsafe { FindWindowW(null(), title.as_ptr()) };
        if hwnd.is_null() {
            return;
        }
        // SAFETY: `hwnd` is a live top-level window handle from FindWindowW.
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::{acquire, Guard};

#[cfg(not(target_os = "windows"))]
mod stub {
    /// No-op guard on non-Windows (the app only ships on Windows).
    pub struct Guard;

    pub fn acquire() -> Option<Guard> {
        Some(Guard)
    }
}
