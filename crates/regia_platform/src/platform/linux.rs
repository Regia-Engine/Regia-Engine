// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/fast_quit.rs, commit c9513623f), ADR-8012.
//! Linux services (and other desktop Unix until macOS gets its own module, ADR-009).

use std::process::{Command, Stdio};

use crate::services::parse::{locale_from_env, meminfo_total_bytes, os_release_value};
use crate::services::{OsInfo, ServiceError};

/// Hands the URL to `xdg-open` and returns without waiting for the browser.
pub(crate) fn open_url(url: &str) -> Result<(), ServiceError> {
    let mut child = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ServiceError::Os {
            operation: "open_url (xdg-open)",
            message: e.to_string(),
        })?;
    // Reap the child in the background so it doesn't linger as a zombie.
    let spawned = std::thread::Builder::new()
        .name("regia-xdg-open".into())
        .spawn(move || match child.wait() {
            Ok(status) if !status.success() => {
                tracing::warn!(target: "regia_platform", "xdg-open exited with {status}");
            }
            Err(err) => tracing::warn!(target: "regia_platform", "xdg-open: {err}"),
            Ok(_) => {}
        });
    if let Err(err) = spawned {
        tracing::debug!(target: "regia_platform", "cannot spawn xdg-open reaper thread: {err}");
    }
    Ok(())
}

/// `LC_ALL`, then `LC_MESSAGES`, then `LANG`.
pub(crate) fn system_locale() -> Option<String> {
    locale_from_env(|var| regia_core::brand::env_var(var).ok())
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// `/etc/os-release`, `/proc/meminfo` and the DMI product name.
pub(crate) fn os_info() -> OsInfo {
    let mut info = OsInfo::unknown();
    let os_release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    let distro = os_release_value(&os_release, "PRETTY_NAME")
        .or_else(|| os_release_value(&os_release, "NAME"))
        .unwrap_or_else(|| "Linux".to_owned());
    info.version = match read_trimmed("/proc/sys/kernel/osrelease") {
        Some(kernel) => format!("{distro} (kernel {kernel})"),
        None => distro,
    };
    let vendor = read_trimmed("/sys/devices/virtual/dmi/id/sys_vendor");
    let product = read_trimmed("/sys/devices/virtual/dmi/id/product_name");
    info.device_model = match (vendor, product) {
        (Some(v), Some(p)) if !p.starts_with(&v) => format!("{v} {p}"),
        (_, Some(p)) => p,
        (Some(v), None) => v,
        (None, None) => String::new(),
    };
    info.total_ram_bytes = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| meminfo_total_bytes(&m))
        .unwrap_or(0);
    info
}

/// Sets the Wayland `app_id` and the X11 `WM_CLASS` (class and instance) to `app_id`.
#[cfg(has_window)]
pub(crate) fn with_app_id(
    attrs: winit::window::WindowAttributes,
    app_id: &str,
) -> winit::window::WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland;
    use winit::platform::x11::WindowAttributesExtX11;
    let attrs = WindowAttributesExtWayland::with_name(attrs, app_id, app_id);
    WindowAttributesExtX11::with_name(attrs, app_id, app_id)
}

/// Ends the process now with `code`, without running exit handlers (the clean exit's watchdog).
#[cfg(target_os = "linux")]
pub(crate) fn exit_immediately(code: i32) -> ! {
    // SAFETY: `_exit` takes no pointers and never returns; skipping the exit handlers (where a
    // stuck GPU driver blocks) is the point.
    unsafe { libc::_exit(code) }
}

/// Ends the process now, without running exit handlers (other desktop Unix: no `libc` here).
#[cfg(not(target_os = "linux"))]
pub(crate) fn exit_immediately(_code: i32) -> ! {
    std::process::abort()
}

/// The screen-reader bridge's AT-SPI adapter (`accesskit` feature, ADR-1998): AccessKit's Unix adapter
/// talks to the accessibility bus over D-Bus from its own thread, so it needs no window handle.
#[cfg(all(feature = "accesskit-adapters", target_os = "linux"))]
pub(crate) mod accesskit_adapter {
    use crate::accessibility::{AccessKitBridge, BridgeHandler, OsAccessKitAdapter};

    /// The adapter's name in the agent's report.
    pub(crate) const NAME: &str = "atspi";
    /// AT-SPI does not need the window hidden while the adapter attaches.
    pub(crate) const CREATE_HIDDEN: bool = false;

    struct AtSpi(accesskit_unix::Adapter);

    impl OsAccessKitAdapter for AtSpi {
        fn update(&mut self, factory: Box<dyn FnOnce() -> accesskit::TreeUpdate + '_>) {
            self.0.update_if_active(factory);
        }

        fn set_window_focused(&mut self, focused: bool) {
            self.0.update_window_focus_state(focused);
        }
    }

    /// The AT-SPI adapter; it activates when the session's screen reader is enabled.
    pub(crate) fn create(
        _window: &winit::window::Window,
        bridge: &AccessKitBridge,
    ) -> Result<Box<dyn OsAccessKitAdapter>, String> {
        let handler = BridgeHandler::new(bridge.clone());
        Ok(Box::new(AtSpi(accesskit_unix::Adapter::new(
            handler.clone(),
            handler.clone(),
            handler,
        ))))
    }
}

/// The per-user directory of the single-instance lock (ADR-8833) and a file-name prefix:
/// `$XDG_RUNTIME_DIR` (per user, mode 0700), else the temp dir with the uid in the name.
pub(crate) fn instance_lock_dir() -> Option<(std::path::PathBuf, String)> {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => Some((std::path::PathBuf::from(dir), String::new())),
        None => Some((std::env::temp_dir(), format!("regia-{}-", user_id()))),
    }
}

#[cfg(target_os = "linux")]
fn user_id() -> u32 {
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(target_os = "linux"))]
fn user_id() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".to_owned())
}

/// Whether process `pid` is alive (`kill(pid, 0)`; EPERM: alive under another user).
#[cfg(target_os = "linux")]
pub(crate) fn process_alive(pid: u32) -> Option<bool> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: signal 0 only checks that the pid exists; no signal is sent.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Some(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Some(false),
        Some(libc::EPERM) => Some(true),
        _ => None,
    }
}

/// Other desktop Unix: no `libc` here; liveness unknown.
#[cfg(not(target_os = "linux"))]
pub(crate) fn process_alive(_pid: u32) -> Option<bool> {
    None
}

/// Raising another process's window needs the compositor's say on Wayland and X11 (focus
/// stealing prevention): nothing to do.
pub(crate) fn raise_instance_window(_pid: u32) -> bool {
    false
}

/// Takes `window` off screen now (the fast exit, ADR-8833): X11 `XUnmapWindow` through winit;
/// Wayland has no hide in winit, so a null buffer is attached and committed (xdg-shell unmaps the
/// toplevel) and the display flushed.
#[cfg(has_window)]
pub(crate) fn hide_window_now(window: &winit::window::Window) -> &'static str {
    use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    let display = window.display_handle().map(|h| h.as_raw());
    let surface = window.window_handle().map(|h| h.as_raw());
    match (display, surface) {
        (Ok(RawDisplayHandle::Wayland(d)), Ok(RawWindowHandle::Wayland(s))) => {
            match wayland_unmap(d.display.as_ptr(), s.surface.as_ptr()) {
                Ok(()) => "wayland: null buffer committed",
                Err(err) => {
                    tracing::warn!(target: "regia_platform", "regia fast exit: wayland unmap failed: {err}");
                    "wayland: unmap failed"
                }
            }
        }
        _ => {
            window.set_visible(false);
            "x11: unmapped"
        }
    }
}

/// Attaches a null buffer to `surface` and commits, then flushes `display`, through the
/// libwayland-client winit already loaded. Adapted from the absorbed fast-quit source
/// (docs/ported-sources.toml); requests per the Wayland core protocol (`wl_surface.attach`
/// opcode 1, `wl_surface.commit` opcode 6).
#[cfg(all(has_window, target_os = "linux"))]
fn wayland_unmap(display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) -> Result<(), String> {
    use std::ffi::c_void;
    /// `union wl_argument` (8 bytes).
    #[repr(C)]
    #[derive(Clone, Copy)]
    union WlArgument {
        i: i32,
        o: *mut c_void,
        _wide: u64,
    }
    type MarshalArrayFlags =
        unsafe extern "C" fn(*mut c_void, u32, *const c_void, u32, u32, *mut WlArgument) -> *mut c_void;
    type GetVersion = unsafe extern "C" fn(*mut c_void) -> u32;
    type DisplayFlush = unsafe extern "C" fn(*mut c_void) -> i32;
    const WL_SURFACE_ATTACH: u32 = 1;
    const WL_SURFACE_COMMIT: u32 = 6;
    if display.is_null() || surface.is_null() {
        return Err("no display or surface".into());
    }
    // SAFETY: the pointers are winit's live wl_display and wl_surface proxies (the caller holds
    // the window); RTLD_NOLOAD only finds the copy winit already loaded; the symbols are
    // libwayland-client's public API with these C signatures, and libwayland serialises
    // requests on the display internally.
    unsafe {
        let lib = libc::dlopen(
            c"libwayland-client.so.0".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_NOLOAD,
        );
        if lib.is_null() {
            return Err("libwayland-client.so.0 not loaded".into());
        }
        let marshal = libc::dlsym(lib, c"wl_proxy_marshal_array_flags".as_ptr());
        let version = libc::dlsym(lib, c"wl_proxy_get_version".as_ptr());
        let flush = libc::dlsym(lib, c"wl_display_flush".as_ptr());
        if marshal.is_null() || version.is_null() || flush.is_null() {
            return Err("libwayland-client has no wl_proxy_marshal_array_flags".into());
        }
        let marshal: MarshalArrayFlags = std::mem::transmute::<*mut c_void, MarshalArrayFlags>(marshal);
        let version: GetVersion = std::mem::transmute::<*mut c_void, GetVersion>(version);
        let flush: DisplayFlush = std::mem::transmute::<*mut c_void, DisplayFlush>(flush);
        let v = version(surface);
        let mut attach = [
            WlArgument {
                o: std::ptr::null_mut(),
            },
            WlArgument { i: 0 },
            WlArgument { i: 0 },
        ];
        marshal(
            surface,
            WL_SURFACE_ATTACH,
            std::ptr::null(),
            v,
            0,
            attach.as_mut_ptr(),
        );
        marshal(
            surface,
            WL_SURFACE_COMMIT,
            std::ptr::null(),
            v,
            0,
            std::ptr::null_mut(),
        );
        if flush(display) < 0 {
            return Err(format!("wl_display_flush: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Other desktop Unix: no `libc` here.
#[cfg(all(has_window, not(target_os = "linux")))]
fn wayland_unmap(_display: *mut std::ffi::c_void, _surface: *mut std::ffi::c_void) -> Result<(), String> {
    Err("no Wayland unmap on this platform".into())
}
