// SPDX-License-Identifier: MPL-2.0
//! macOS services (ADR-009 deferred the port; the macOS bring-up lane added this module).

use std::process::{Command, Stdio};

use crate::services::parse::{first_apple_language, locale_from_env, normalise_language_tag};
use crate::services::{OsInfo, ServiceError};

/// Hands the URL to `/usr/bin/open` and returns without waiting for the browser.
pub(crate) fn open_url(url: &str) -> Result<(), ServiceError> {
    let mut child = Command::new("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ServiceError::Os {
            operation: "open_url (open)",
            message: e.to_string(),
        })?;
    // Reap the child in the background so it doesn't linger as a zombie.
    let spawned = std::thread::Builder::new()
        .name("regia-open-url".into())
        .spawn(move || match child.wait() {
            Ok(status) if !status.success() => {
                tracing::warn!(target: "regia_platform", "open exited with {status}");
            }
            Err(err) => tracing::warn!(target: "regia_platform", "open: {err}"),
            Ok(_) => {}
        });
    if let Err(err) = spawned {
        tracing::debug!(target: "regia_platform", "cannot spawn open reaper thread: {err}");
    }
    Ok(())
}

/// Runs a system tool and returns its trimmed, non-empty stdout.
fn tool_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The POSIX environment variables first, then `AppleLanguages` and `AppleLocale` (GUI launches
/// carry no `LANG`).
pub(crate) fn system_locale() -> Option<String> {
    locale_from_env(|var| regia_core::brand::env_var(var).ok())
        .or_else(|| {
            // The UI language list, not the region format in `AppleLocale`.
            let languages = tool_output("/usr/bin/defaults", &["read", "-g", "AppleLanguages"])?;
            first_apple_language(&languages)
        })
        .or_else(|| {
            let apple = tool_output("/usr/bin/defaults", &["read", "-g", "AppleLocale"])?;
            normalise_language_tag(&apple)
        })
}

/// `sw_vers` for the version and `sysctl` for the model and memory size.
pub(crate) fn os_info() -> OsInfo {
    let mut info = OsInfo::unknown();
    info.version = match tool_output("/usr/bin/sw_vers", &["-productVersion"]) {
        Some(v) => format!("macOS {v}"),
        None => "macOS".to_owned(),
    };
    info.device_model = tool_output("/usr/sbin/sysctl", &["-n", "hw.model"]).unwrap_or_default();
    info.cpu_model = tool_output("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"]).unwrap_or_default();
    info.total_ram_bytes = tool_output("/usr/sbin/sysctl", &["-n", "hw.memsize"])
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    info
}

/// Ends the process now with `code`, without running exit handlers (the clean exit's watchdog).
pub(crate) fn exit_immediately(code: i32) -> ! {
    // SAFETY: `_exit` takes no pointers and never returns; skipping the exit handlers (where a
    // stuck GPU driver blocks) is the point.
    unsafe { libc::_exit(code) }
}

/// The screen-reader bridge's NSAccessibility adapter (`accesskit` feature, ADR-1998): AccessKit's
/// subclassing adapter adds the accessibility methods to the window's content view. It wants the view
/// before it is first shown, so the runner creates the window hidden and shows it after attaching.
#[cfg(feature = "accesskit-adapters")]
pub(crate) mod accesskit_adapter {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use crate::accessibility::{AccessKitBridge, BridgeHandler, OsAccessKitAdapter};

    /// The adapter's name in the agent's report.
    pub(crate) const NAME: &str = "ns_accessibility";
    /// The view should not have been shown or focused before the adapter subclasses it.
    pub(crate) const CREATE_HIDDEN: bool = true;

    struct NsAccessibility(accesskit_macos::SubclassingAdapter);

    impl OsAccessKitAdapter for NsAccessibility {
        fn update(&mut self, factory: Box<dyn FnOnce() -> accesskit::TreeUpdate + '_>) {
            if let Some(events) = self.0.update_if_active(factory) {
                events.raise();
            }
        }

        fn set_window_focused(&mut self, focused: bool) {
            if let Some(events) = self.0.update_view_focus_state(focused) {
                events.raise();
            }
        }
    }

    /// The NSAccessibility adapter of `window`'s view.
    pub(crate) fn create(
        window: &winit::window::Window,
        bridge: &AccessKitBridge,
    ) -> Result<Box<dyn OsAccessKitAdapter>, String> {
        let handle = window
            .window_handle()
            .map_err(|e| format!("no window handle: {e}"))?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return Err("the window handle is not an AppKit handle".to_owned());
        };
        let handler = BridgeHandler::new(bridge.clone());
        // SAFETY: `ns_view` is the live content view of the window winit just created on this (the
        // main) thread; the adapter retains it for as long as it lives.
        let adapter = unsafe {
            accesskit_macos::SubclassingAdapter::new(appkit.ns_view.as_ptr(), handler.clone(), handler)
        };
        Ok(Box::new(NsAccessibility(adapter)))
    }
}

/// The per-user directory of the single-instance lock (ADR-8833): `$TMPDIR`, which macOS keeps
/// per user (`/var/folders/...`).
pub(crate) fn instance_lock_dir() -> Option<(std::path::PathBuf, String)> {
    Some((std::env::temp_dir(), String::new()))
}

/// Whether process `pid` is alive (`kill(pid, 0)`; EPERM: alive under another user).
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

/// The Dock brings a running app forward on a relaunch from Finder; a second process started from
/// a shell has no AppKit call to raise another process's window: nothing to do.
pub(crate) fn raise_instance_window(_pid: u32) -> bool {
    false
}

/// Takes `window` off screen now (the fast exit, ADR-8833): `orderOut` through winit.
#[cfg(has_window)]
pub(crate) fn hide_window_now(window: &winit::window::Window) -> &'static str {
    window.set_visible(false);
    "macos: ordered out"
}
