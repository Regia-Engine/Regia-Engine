// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/winplat.rs,
// commit d6ed37677, regia-absorb-1), ADR-8012; adapted under ADR-8156.
//! Native bare-Alt subclass; also exercised by the standalone Windows seam harness.

// Adapted from DO27 winplat.rs:340-367, d6ed37677 (regia-absorb-1), P3/adapt,
// owner exception 2026-11-16. Added owner-thread check and destruction cleanup.
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, WM_NCDESTROY};
type Subclass = Option<unsafe extern "system" fn(HWND, u32, usize, isize, usize, usize) -> isize>;
// SAFETY: these declarations match comctl32's documented subclass ABI; callbacks
// live for the process lifetime and only receive live handles from Windows.
#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(hwnd: HWND, callback: Subclass, id: usize, data: usize) -> i32;
    fn DefSubclassProc(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> isize;
    fn RemoveWindowSubclass(hwnd: HWND, callback: Subclass, id: usize) -> i32;
}
const ID: usize = 0x5247_414c;

// SAFETY: Windows invokes this static callback with its subclass ABI and live
// HWND on the owning thread; it forwards the exact incoming arguments.
/// # Safety
/// Only Windows may invoke this callback as an installed window subclass.
unsafe extern "system" fn callback(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
    id: usize,
    _data: usize,
) -> isize {
    if crate::alt_menu::swallows_message(message, wparam, lparam) {
        return 0;
    }
    if message == WM_NCDESTROY {
        // SAFETY: removes exactly this callback/id on its owning thread.
        unsafe {
            RemoveWindowSubclass(hwnd, Some(callback), id);
        }
    }
    // SAFETY: preserve the subclass chain with the original message arguments.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// # Safety
/// `hwnd` must be a live window, retained for the duration of this call.
pub(super) unsafe fn install(hwnd: HWND) -> std::io::Result<()> {
    // SAFETY: reads ownership of the live HWND; optional process output is null.
    let owner = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
    // SAFETY: no arguments; identifies the caller without acquiring a handle.
    if owner == 0 || owner != unsafe { GetCurrentThreadId() } {
        return Err(std::io::Error::other(
            "Alt-menu subclass requires the window's owning thread",
        ));
    }
    // SAFETY: thread ownership checked above; static callback, no borrowed data.
    if unsafe { SetWindowSubclass(hwnd, Some(callback), ID, 0) } == 0 {
        return Err(std::io::Error::other("Windows refused the Alt-menu subclass"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, SendMessageW};

    // SAFETY: public User32 ABI, used only with windows owned by this test thread.
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetFocus(hwnd: HWND) -> HWND;
        fn GetFocus() -> HWND;
        fn InvalidateRect(hwnd: HWND, rect: *const RECT, erase: i32) -> i32;
        fn UpdateWindow(hwnd: HWND) -> i32;
    }

    // SAFETY: Windows calls this test observer with a live window and subclass ABI.
    /// # Safety
    /// Called only as a registered subclass on the current test's live window.
    unsafe extern "system" fn observer(
        hwnd: HWND,
        msg: u32,
        wp: usize,
        lp: isize,
        _id: usize,
        data: usize,
    ) -> isize {
        if msg == 0x000f && data != 0 {
            // SAFETY: the paint test retains this counter until after the owned
            // window is destroyed; other tests supply zero, which is not read.
            unsafe { &*(data as *const AtomicUsize) }.fetch_add(1, Ordering::Relaxed);
        }
        if [0x0112, 0x0104, 0x0105, 0x8001].contains(&msg) {
            return 7;
        }
        // SAFETY: preserves the chain for lifecycle messages, including destruction.
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }

    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: the test owns this live window on its creating thread.
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn native_alt_subclass_swallows_bare_alt_and_preserves_the_message_chain() {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: built-in class, valid terminated name, hidden owned window,
        // optional handles/creation data null. This test keeps it until drop.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        let window = Window(hwnd);
        // SAFETY: live test window on this thread, static observer with no data.
        assert_ne!(unsafe { SetWindowSubclass(hwnd, Some(observer), ID + 1, 0) }, 0);
        // SAFETY: live test window on this thread; repeat install is idempotent.
        unsafe {
            install(hwnd).expect("test fixture");
            install(hwnd).expect("test fixture");
        }
        for (msg, wp, lp, result) in [
            (0x0112, 0xf100, 0, 0),
            (0x0112, 0xf10f, 0, 0),
            (0x0112, 0xf100, 0x20, 7),
            (0x0112, 0xf060, 0, 7),
            (0x0112, 0xf020, 0, 7),
            (0x0112, 0xf030, 0, 7),
            (0x0104, 0x12, 0, 7),
            (0x0105, 0x12, 0, 7),
            (0x8001, 0, 0, 7),
        ] {
            // SAFETY: synchronous message to our own window; the observer consumes
            // all non-bare-Alt test messages so no native modal menu can open.
            assert_eq!(unsafe { SendMessageW(hwnd, msg, wp, lp) }, result);
        }
        let address = hwnd as usize;
        let wrong_thread = std::thread::spawn(move || {
            // SAFETY: the owning test retains the HWND until this thread joins;
            // install checks and refuses the wrong thread before any mutation.
            unsafe { install(address as HWND).is_err() }
        })
        .join()
        .expect("test fixture");
        assert!(wrong_thread);
        drop(window);
    }

    #[test]
    fn native_bare_alt_keeps_focus_and_redraws() {
        let paints = AtomicUsize::new(0);
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: built-in class, valid strings, a tiny visible window on this
        // thread. Its lifetime ends before the paint counter leaves scope.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0x1000_0000, // WS_VISIBLE: permit a real update-region paint.
                0,
                0,
                16,
                16,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        let window = Window(hwnd);
        // SAFETY: both subclasses run on this thread; the borrowed counter stays
        // live until the window and its observer are destroyed below.
        unsafe {
            assert_ne!(
                SetWindowSubclass(hwnd, Some(observer), ID + 1, std::ptr::from_ref(&paints) as usize),
                0
            );
            install(hwnd).expect("test fixture");
            SetFocus(hwnd);
            assert_eq!(GetFocus(), hwnd);
        }
        for _ in 0..2 {
            let before = paints.load(Ordering::Relaxed);
            // SAFETY: synchronous calls to our live window. UpdateWindow sends
            // WM_PAINT directly, so this uses no sleep or wall-clock assertion.
            unsafe {
                assert_eq!(SendMessageW(hwnd, 0x0112, 0xf100, 0), 0);
                assert_eq!(GetFocus(), hwnd);
                assert_ne!(InvalidateRect(hwnd, std::ptr::null(), 0), 0);
                assert_ne!(UpdateWindow(hwnd), 0);
            }
            assert!(paints.load(Ordering::Relaxed) > before);
        }
        drop(window);
    }
}
