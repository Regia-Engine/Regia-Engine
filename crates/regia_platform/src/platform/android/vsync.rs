// SPDX-License-Identifier: MPL-2.0
//! Vsync ticks from `AChoreographer` (N12, ADR-1942; Android NDK `choreographer.h`).
//!
//! A dedicated thread owns a looper, because a choreographer delivers its callbacks on the looper
//! of the thread that asked for it and the `android_main` thread spends its time in frames. The
//! callback stamps the tick and posts itself for the next vsync. While the app is in the
//! background nothing is posted, so the thread sleeps in its looper and costs no wakeups.
//!
//! The entry points are looked up with `dlsym`: `AChoreographer_postFrameCallback64` is API 29
//! and `ANativeWindow_setFrameRate` API 30, while `min_sdk` may be 24, and a hard link to a newer
//! symbol stops the library from loading on an older phone.

use std::cell::Cell;
use std::ffi::{CStr, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ndk::looper::{ForeignLooper, ThreadLooper};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::frame_pacing::VsyncTicks;

/// `AChoreographer_frameCallback64`. On the 64-bit ABIs this crate targets, `long` is 64 bits, so
/// the older `AChoreographer_frameCallback` has the same signature.
type FrameCallback = unsafe extern "C" fn(frame_time_nanos: i64, data: *mut c_void);
type GetInstance = unsafe extern "C" fn() -> *mut c_void;
type PostFrameCallback = unsafe extern "C" fn(*mut c_void, FrameCallback, *mut c_void);
type SetFrameRate = unsafe extern "C" fn(*mut c_void, f32, i8) -> c_int;

/// `ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_DEFAULT`: the rate is a preference; the compositor
/// should keep a smooth multiple of it.
const FRAME_RATE_COMPATIBILITY_DEFAULT: i8 = 0;

/// How long `start_vsync_thread` waits for the thread to report in.
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// Opens an NDK library. The handle is never closed: the library stays for the process.
fn open_library(name: &CStr) -> *mut c_void {
    // SAFETY: a valid NUL-terminated name of an NDK system library, whose initializers are the
    // platform's own. The handle is never closed, so addresses taken from it stay valid.
    unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW) }
}

/// The address of `name` in the library behind `handle`.
fn symbol(handle: *mut c_void, name: &CStr) -> Option<*mut c_void> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: a live handle from `dlopen` and a NUL-terminated name.
    let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
    (!address.is_null()).then_some(address)
}

#[derive(Clone, Copy)]
struct Choreographer {
    get_instance: GetInstance,
    post: PostFrameCallback,
}

fn choreographer_api() -> Option<Choreographer> {
    let library = open_library(c"libandroid.so");
    let get_instance = symbol(library, c"AChoreographer_getInstance")?;
    let post = symbol(library, c"AChoreographer_postFrameCallback64")
        .or_else(|| symbol(library, c"AChoreographer_postFrameCallback"))?;
    // SAFETY: both symbols have these signatures in the NDK's `choreographer.h`.
    Some(unsafe { choreographer_from(get_instance, post) })
}

/// The entry points at two addresses.
///
/// # Safety
///
/// Both addresses are non-null, of functions with these signatures, and stay mapped.
unsafe fn choreographer_from(get_instance: *mut c_void, post: *mut c_void) -> Choreographer {
    // SAFETY: the caller's contract.
    unsafe {
        Choreographer {
            get_instance: std::mem::transmute::<*mut c_void, GetInstance>(get_instance),
            post: std::mem::transmute::<*mut c_void, PostFrameCallback>(post),
        }
    }
}

#[derive(Default)]
struct Flags {
    active: AtomicBool,
    stop: AtomicBool,
}

/// What the frame callback reads. Lives on the thread's stack for as long as its looper runs, and
/// callbacks only ever run from that looper.
struct Poster {
    api: Choreographer,
    instance: *mut c_void,
    ticks: Arc<VsyncTicks>,
    flags: Arc<Flags>,
    posted: Cell<bool>,
}

impl Poster {
    /// Asks for the next vsync, once, while the app is in the foreground.
    fn post(&self) {
        if self.posted.get() || !self.flags.active.load(Ordering::Acquire) {
            return;
        }
        self.posted.set(true);
        // SAFETY: `instance` came from this thread's `AChoreographer_getInstance`, and `self` stays
        // alive and in place until the looper stops running callbacks (see `vsync_loop`).
        unsafe {
            (self.api.post)(
                self.instance,
                on_frame,
                std::ptr::from_ref(self).cast_mut().cast(),
            );
        }
    }
}

/// The vsync callback, on the vsync thread.
///
/// # Safety
///
/// `data` is the [`Poster`] that posted this callback ([`Poster::post`]), still alive: it lives on
/// the stack of the thread whose looper runs this callback, until that looper stops polling.
unsafe extern "C" fn on_frame(frame_time_nanos: i64, data: *mut c_void) {
    // SAFETY: the function's contract. A shared reference is enough: `posted` is a `Cell`, and this
    // thread is the only one that reaches the `Poster`.
    let poster = unsafe { &*data.cast::<Poster>() };
    // The delivery time goes with the tick, so the trace shows how late the compositor's
    // callback came (ADR-5461).
    poster
        .ticks
        .tick_delivered(u64::try_from(frame_time_nanos).unwrap_or(0), monotonic_ns());
    poster.posted.set(false);
    poster.post();
}

fn vsync_loop(
    api: Choreographer,
    ticks: Arc<VsyncTicks>,
    flags: Arc<Flags>,
    ready: mpsc::Sender<Option<(ForeignLooper, i32)>>,
) {
    let looper = ThreadLooper::prepare();
    // SAFETY: called on a thread with a prepared looper, as the NDK asks.
    let instance = unsafe { (api.get_instance)() };
    if instance.is_null() {
        let _ = ready.send(None);
        return;
    }
    let thread_id = super::perf_hint::current_thread_id();
    if ready
        .send(Some((looper.as_foreign().clone(), thread_id)))
        .is_err()
    {
        return;
    }
    let poster = Poster {
        api,
        instance,
        ticks,
        flags,
        posted: Cell::new(false),
    };
    while !poster.flags.stop.load(Ordering::Acquire) {
        poster.post();
        if let Err(err) = looper.poll_once() {
            tracing::warn!(target: "regia_platform", "vsync thread: looper error ({err:?})");
            return;
        }
    }
}

/// The running vsync thread (`platform/mod.rs` seam).
pub(crate) struct VsyncThread {
    looper: ForeignLooper,
    flags: Arc<Flags>,
    thread_id: i32,
}

impl VsyncThread {
    /// The kernel id of the vsync thread, for the frame loop's performance hint session.
    pub(crate) fn thread_id(&self) -> Option<i32> {
        Some(self.thread_id)
    }

    /// Starts or stops the ticks. A stopped thread sleeps in its looper.
    pub(crate) fn set_active(&self, active: bool) {
        self.flags.active.store(active, Ordering::Release);
        self.looper.wake();
    }
}

impl Drop for VsyncThread {
    fn drop(&mut self) {
        self.flags.stop.store(true, Ordering::Release);
        self.looper.wake();
    }
}

/// Starts the `regia-vsync` thread feeding `ticks`, or `None` when the NDK has no choreographer
/// or the thread cannot start; frames then fall back to the software cap.
pub(crate) fn start_vsync_thread(ticks: Arc<VsyncTicks>) -> Option<VsyncThread> {
    let Some(api) = choreographer_api() else {
        tracing::info!(target: "regia_platform", "frame pacing: no AChoreographer; software pacing");
        return None;
    };
    let flags = Arc::new(Flags::default());
    let thread_flags = flags.clone();
    let (ready, started) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("regia-vsync".to_owned())
        .spawn(move || vsync_loop(api, ticks, thread_flags, ready));
    if let Err(err) = spawned {
        tracing::warn!(target: "regia_platform", "frame pacing: cannot start the vsync thread: {err}");
        return None;
    }
    match started.recv_timeout(START_TIMEOUT) {
        Ok(Some((looper, thread_id))) => Some(VsyncThread {
            looper,
            flags,
            thread_id,
        }),
        Ok(None) => {
            tracing::warn!(target: "regia_platform", "frame pacing: no choreographer on the vsync thread");
            None
        }
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            tracing::warn!(target: "regia_platform", "frame pacing: the vsync thread did not start");
            None
        }
    }
}

/// `CLOCK_MONOTONIC`, the clock of Choreographer's `frameTimeNanos`.
pub(crate) fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid out pointer and `CLOCK_MONOTONIC` always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    u64::try_from(now.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(now.tv_nsec).unwrap_or(0)
}

/// `ANativeWindow_setFrameRate` (API 30): tells the compositor the rate this window runs at, so a
/// 120 Hz panel can settle on a multiple of it. `0.0` clears the preference. A hint only: an older
/// phone, or a window without a native surface, keeps its panel rate.
pub(crate) fn set_frame_rate_hint(window: &winit::window::Window, fps: f32) {
    static SET: OnceLock<Option<SetFrameRate>> = OnceLock::new();
    let lookup = || {
        let library = open_library(c"libnativewindow.so");
        let address = symbol(library, c"ANativeWindow_setFrameRate")?;
        // SAFETY: `ANativeWindow_setFrameRate` has this signature in the NDK's `native_window.h`, and
        // the library is never closed.
        Some(unsafe { std::mem::transmute::<*mut c_void, SetFrameRate>(address) })
    };
    let Some(set) = *SET.get_or_init(lookup) else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AndroidNdk(native) = handle.as_raw() else {
        return;
    };
    // SAFETY: the handle is the window's live surface, and the caller runs on the event loop thread,
    // which is the one that drops the surface on a suspend.
    let status = unsafe {
        set(
            native.a_native_window.as_ptr(),
            fps,
            FRAME_RATE_COMPATIBILITY_DEFAULT,
        )
    };
    if status != 0 {
        tracing::debug!(target: "regia_platform", "ANativeWindow_setFrameRate({fps}) answered {status}");
    }
}

/// Runs with the Android target under Miri, on any host (`docs/06-testing-and-ci.md` § 14): nothing
/// here touches the NDK, because the entry points are stand-ins.
#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    static POSTS: AtomicUsize = AtomicUsize::new(0);

    /// Stands in for `AChoreographer_getInstance`.
    ///
    /// # Safety
    ///
    /// None: `unsafe` only to match the entry point's type.
    unsafe extern "C" fn no_instance() -> *mut c_void {
        std::ptr::null_mut()
    }

    /// Stands in for `AChoreographer_postFrameCallback64`: counts, and hands the data pointer back
    /// to the callback only when the test does.
    ///
    /// # Safety
    ///
    /// None: `unsafe` only to match the entry point's type.
    unsafe extern "C" fn count_post(_: *mut c_void, _: FrameCallback, _: *mut c_void) {
        POSTS.fetch_add(1, Ordering::SeqCst);
    }

    fn stand_in_api() -> Choreographer {
        let get_instance = no_instance as GetInstance as *mut c_void;
        let post = count_post as PostFrameCallback as *mut c_void;
        // SAFETY: the two addresses are of functions with exactly these signatures.
        unsafe { choreographer_from(get_instance, post) }
    }

    /// The vsync callback rebuilds the `Poster` from the pointer `post` gave out, stamps the tick and
    /// asks for the next one while the app is in the foreground, and stops asking in the background.
    #[test]
    fn a_frame_callback_ticks_and_posts_the_next_one_only_in_the_foreground() {
        let flags = Arc::new(Flags::default());
        flags.active.store(true, Ordering::Release);
        let ticks = VsyncTicks::new();
        let poster = Poster {
            api: stand_in_api(),
            instance: std::ptr::null_mut(),
            ticks: Arc::clone(&ticks),
            flags: Arc::clone(&flags),
            posted: Cell::new(false),
        };
        let data = std::ptr::from_ref(&poster).cast_mut().cast::<c_void>();
        let before = POSTS.load(Ordering::SeqCst);
        poster.post();
        poster.post();
        assert_eq!(POSTS.load(Ordering::SeqCst) - before, 1, "one post at a time");
        // SAFETY: `data` is the `Poster` above, alive for the call, on the thread that owns it.
        unsafe { on_frame(16_000_000, data) };
        assert_eq!(ticks.total(), 1);
        assert_eq!(
            POSTS.load(Ordering::SeqCst) - before,
            2,
            "the next vsync is asked for"
        );
        flags.active.store(false, Ordering::Release);
        // SAFETY: as above.
        unsafe { on_frame(32_000_000, data) };
        assert_eq!(ticks.total(), 2);
        assert_eq!(
            POSTS.load(Ordering::SeqCst) - before,
            2,
            "nothing is posted in the background"
        );
    }

    /// The address-to-function casts keep a callable function pointer.
    #[test]
    fn a_function_pointer_survives_the_cast_from_its_address() {
        let api = stand_in_api();
        // SAFETY: `no_instance` takes nothing and has no contract.
        assert!(unsafe { (api.get_instance)() }.is_null());
    }
}
