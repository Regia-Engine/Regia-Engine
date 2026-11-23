// SPDX-License-Identifier: MPL-2.0
//! A performance hint session for the frame loop (ADR-5461; Android NDK `performance_hint.h`,
//! API 33, the Android Dynamic Performance Framework).
//!
//! The frame loop tells the system how long a frame may take (the slot) and how long each one
//! took, so the scheduler keeps the frame threads on cores and clocks that meet the slot. Without
//! it a frame loop that sleeps most of the slot looks light to the governor, which lowers the
//! clocks and lets the cores idle deeply: the frame after a long wait then starts late and runs
//! slow.
//!
//! The entry points are looked up with `dlsym`, as `vsync.rs` does: `min_sdk` may be below 33.

use std::ffi::{CStr, c_int, c_void};
use std::ptr::NonNull;
use std::sync::OnceLock;

type GetManager = unsafe extern "C" fn() -> *mut c_void;
type CreateSession = unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void;
type UpdateTarget = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type ReportActual = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type CloseSession = unsafe extern "C" fn(*mut c_void);

#[derive(Clone, Copy)]
struct Api {
    get_manager: GetManager,
    create: CreateSession,
    update_target: UpdateTarget,
    report: ReportActual,
    close: CloseSession,
}

fn symbol(handle: *mut c_void, name: &CStr) -> Option<*mut c_void> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: a live handle from `dlopen` and a NUL-terminated name.
    let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
    (!address.is_null()).then_some(address)
}

fn api() -> Option<Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    *API.get_or_init(|| {
        // SAFETY: a valid NUL-terminated name of an NDK system library. The handle is never
        // closed, so the addresses taken from it stay valid for the process.
        let library = unsafe { libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW) };
        let get_manager = symbol(library, c"APerformanceHint_getManager")?;
        let create = symbol(library, c"APerformanceHint_createSession")?;
        let update_target = symbol(library, c"APerformanceHint_updateTargetWorkDuration")?;
        let report = symbol(library, c"APerformanceHint_reportActualWorkDuration")?;
        let close = symbol(library, c"APerformanceHint_closeSession")?;
        // SAFETY: the five symbols have these signatures in the NDK's `performance_hint.h`.
        unsafe {
            Some(Api {
                get_manager: std::mem::transmute::<*mut c_void, GetManager>(get_manager),
                create: std::mem::transmute::<*mut c_void, CreateSession>(create),
                update_target: std::mem::transmute::<*mut c_void, UpdateTarget>(update_target),
                report: std::mem::transmute::<*mut c_void, ReportActual>(report),
                close: std::mem::transmute::<*mut c_void, CloseSession>(close),
            })
        }
    })
}

/// The kernel id of the calling thread, for [`PerfHintSession::open`].
pub(crate) fn current_thread_id() -> i32 {
    // SAFETY: `gettid` takes no arguments and cannot fail.
    unsafe { libc::gettid() }
}

/// An open `APerformanceHintSession`, closed on drop.
pub(crate) struct PerfHintSession {
    api: Api,
    session: NonNull<c_void>,
}

// SAFETY: the session is a handle owned by this value alone. The NDK asks that one session is not
// used from two threads at once; `&mut self`/`&self` on a value with one owner keeps it so.
unsafe impl Send for PerfHintSession {}

impl PerfHintSession {
    /// A session for `threads` (kernel thread ids) with a first target of `target_ns`, or `None`
    /// when the platform has no performance hint API or refuses the session (`None` from
    /// `APerformanceHint_createSession`: unsupported device, or the app is not in the
    /// foreground).
    pub(crate) fn open(threads: &[i32], target_ns: i64) -> Option<Self> {
        let api = api()?;
        if threads.is_empty() || target_ns <= 0 {
            return None;
        }
        // SAFETY: the manager takes no arguments; a null manager is checked before use.
        let manager = unsafe { (api.get_manager)() };
        if manager.is_null() {
            return None;
        }
        // SAFETY: a live manager, a pointer to `threads.len()` thread ids that outlives the call
        // (the NDK copies them), and a positive target.
        let session = unsafe { (api.create)(manager, threads.as_ptr(), threads.len(), target_ns) };
        Some(Self {
            api,
            session: NonNull::new(session)?,
        })
    }

    /// Sets a new target. `false`: the platform refused it.
    pub(crate) fn set_target(&mut self, target_ns: i64) -> bool {
        // SAFETY: a live session owned by `self`.
        target_ns > 0 && unsafe { (self.api.update_target)(self.session.as_ptr(), target_ns) } == 0
    }

    /// Reports one frame's work. `false`: the platform refused it.
    pub(crate) fn report(&mut self, actual_ns: i64) -> bool {
        // SAFETY: a live session owned by `self`.
        actual_ns > 0 && unsafe { (self.api.report)(self.session.as_ptr(), actual_ns) } == 0
    }
}

impl Drop for PerfHintSession {
    fn drop(&mut self) {
        // SAFETY: a live session owned by `self`, closed once.
        unsafe { (self.api.close)(self.session.as_ptr()) };
    }
}
