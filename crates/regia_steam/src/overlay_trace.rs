// SPDX-License-Identifier: MPL-2.0
//! The Steam overlay trace (ADR-7349): `REGIA_STEAM_OVERLAY_TRACE=<file>` makes a game that runs
//! [`crate::SteamPlugin`] write one timestamped line per thing that can open, close or fight the
//! overlay, so a run by hand (the overlay only draws in a game Steam started) leaves data:
//!
//! - every `GameOverlayActivated` callback from Steam, repeats included (`edge` or `repeat`),
//! - `OverlayOpened` / `OverlayClosed` as the game saw them,
//! - window focus changes (`WindowFocused`),
//! - Shift and Tab key events (down, up, auto-repeat),
//! - the cursor grab and visibility the game asks for (the primary [`regia_platform::Window`]),
//!   the overlay's cursor hold ([`regia_platform::CursorRelease`]) and every `set_cursor_grab` /
//!   `set_cursor_visible` call the window layer made ([`regia_platform::CursorCalls`]),
//! - a heartbeat once a second: frames since the last one, overlay state, grab calls.
//!
//! Lines look like `t=+12.345s frame=740 steam callback GameOverlayActivated active=true (edge)`.
//! The file is created (truncated) when the plugin builds; a file that cannot be created is a
//! warning, never an error. A game adds its own lines with [`SteamOverlayTrace::note`] (the
//! `net_game` sample adds the renderer's hidden-surface throttle).

use std::fmt::Display;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regia_app::prelude::*;
use regia_platform::{
    ButtonState, CursorCalls, CursorGrab, CursorRelease, KeyboardInput, PrimaryWindow, Window, WindowFocused,
};

use crate::overlay::{OverlayActive, OverlayClosed, OverlayOpened};

/// The environment variable that turns the trace on: the path of the file to write.
pub const STEAM_OVERLAY_TRACE_ENV: &str = "REGIA_STEAM_OVERLAY_TRACE";

/// How often the heartbeat line is written.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(1);

/// The open trace (a resource; present only while tracing). Each line is written and flushed at
/// once, so a crash or a kill keeps everything before it.
#[derive(Resource)]
pub struct SteamOverlayTrace {
    out: Box<dyn Write + Send + Sync>,
    start: Instant,
    frame: u64,
    lines: u64,
    write_failed: bool,
    heartbeat_at: Instant,
    heartbeat_frames: u64,
}

impl std::fmt::Debug for SteamOverlayTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamOverlayTrace")
            .field("frame", &self.frame)
            .field("lines", &self.lines)
            .field("write_failed", &self.write_failed)
            .finish_non_exhaustive()
    }
}

impl SteamOverlayTrace {
    /// Creates (truncates) `path` and writes the header line.
    ///
    /// # Errors
    /// The file cannot be created or written.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::create(path)?;
        Self::to_writer(Box::new(file))
    }

    /// A trace into any writer (tests use a shared buffer).
    ///
    /// # Errors
    /// The header line cannot be written.
    pub fn to_writer(mut out: Box<dyn Write + Send + Sync>) -> std::io::Result<Self> {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        writeln!(
            out,
            "regia steam overlay trace 1; pid {}; unix_ms {unix_ms}; os {}; {}",
            std::process::id(),
            std::env::consts::OS,
            environment_facts(|key| std::env::var_os(key))
        )?;
        out.flush()?;
        let now = Instant::now();
        Ok(Self {
            out,
            start: now,
            frame: 0,
            lines: 1,
            write_failed: false,
            heartbeat_at: now,
            heartbeat_frames: 0,
        })
    }

    /// Writes one line, prefixed with the time since the trace started and the frame number. A
    /// failed write is logged once and the trace goes quiet.
    pub fn note(&mut self, line: impl Display) {
        if self.write_failed {
            return;
        }
        let t = self.start.elapsed().as_secs_f64();
        let result =
            writeln!(self.out, "t=+{t:.3}s frame={} {line}", self.frame).and_then(|()| self.out.flush());
        match result {
            Ok(()) => self.lines += 1,
            Err(err) => {
                self.write_failed = true;
                tracing::warn!(target: "regia_steam", "overlay trace: write failed, tracing stops: {err}");
            }
        }
    }

    /// Lines written so far, the header included.
    #[must_use]
    pub fn lines_written(&self) -> u64 {
        self.lines
    }

    /// The frame number the next line carries (counted in `Last`).
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame
    }
}

/// What the process environment says about how Steam started it, for the header: whether Steam
/// preloaded its overlay renderer (`LD_PRELOAD` names `gameoverlayrenderer`), the Vulkan overlay
/// layer switch, Steam's game id variables, and which display the window can use (a Wayland
/// session that also has `DISPLAY` set runs natively; `WAYLAND_DISPLAY=` empty forces XWayland).
fn environment_facts(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> String {
    let set = |key: &str| var(key).is_some_and(|v| !v.is_empty());
    let preload = var("LD_PRELOAD").is_some_and(|v| v.to_string_lossy().contains("gameoverlayrenderer"));
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    format!(
        "overlay_renderer_preloaded {}; vk_overlay_layer {}; steam_game_id {}; wayland_display {}; x11_display {}",
        yes_no(preload),
        var("ENABLE_VK_LAYER_VALVE_steam_overlay_1")
            .map_or_else(|| "unset".to_owned(), |v| v.to_string_lossy().into_owned()),
        yes_no(set("SteamGameId") || set("SteamOverlayGameId")),
        yes_no(set("WAYLAND_DISPLAY")),
        yes_no(set("DISPLAY")),
    )
}

/// Opens the trace named by [`STEAM_OVERLAY_TRACE_ENV`] and adds the trace systems (they run only
/// while a [`SteamOverlayTrace`] resource exists, so a test may insert one later).
pub(crate) fn install_from_env(app: &mut App) {
    app.add_message::<KeyboardInput>()
        .add_message::<WindowFocused>()
        .add_systems(Last, trace_frame.run_if(resource_exists::<SteamOverlayTrace>));
    let Some(path) = std::env::var_os(STEAM_OVERLAY_TRACE_ENV).filter(|p| !p.is_empty()) else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    match SteamOverlayTrace::create(&path) {
        Ok(trace) => {
            tracing::info!(target: "regia_steam", "overlay trace: writing {}", path.display());
            app.insert_resource(trace);
        }
        Err(err) => {
            tracing::warn!(target: "regia_steam", "overlay trace: cannot create {}: {err}", path.display());
        }
    }
}

/// What the trace saw last frame, so only changes are written.
#[derive(Default)]
pub(crate) struct TraceSeen {
    wish: Option<(CursorGrab, bool)>,
    hold: Option<bool>,
    calls: CursorCalls,
}

/// Whether a key event belongs in the trace: Shift and Tab (Steam's overlay shortcut).
fn traced_key(event: &KeyboardInput) -> bool {
    event.physical_key.starts_with("Shift")
        || event.physical_key == "Tab"
        || event.logical_key == "Tab"
        || event.logical_key == "Shift"
}

/// The text of a traced key event.
fn key_line(event: &KeyboardInput) -> String {
    let state = match event.state {
        ButtonState::Pressed => "down",
        ButtonState::Released => "up",
    };
    let repeat = if event.repeat { " repeat" } else { "" };
    format!(
        "key {} ({}) {state}{repeat}",
        event.physical_key, event.logical_key
    )
}

/// `Last`: writes this frame's traced events and state changes, and the heartbeat.
// One parameter per message and resource the trace reads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_frame(
    mut trace: ResMut<SteamOverlayTrace>,
    mut keys: MessageReader<KeyboardInput>,
    mut focus: MessageReader<WindowFocused>,
    mut opened: MessageReader<OverlayOpened>,
    mut closed: MessageReader<OverlayClosed>,
    overlay: Res<OverlayActive>,
    hold: Option<Res<CursorRelease>>,
    calls: Option<Res<CursorCalls>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut seen: Local<TraceSeen>,
) {
    let trace = &mut *trace;
    for event in focus.read() {
        trace.note(format_args!(
            "window focus {} (window {:?})",
            if event.focused { "in" } else { "out" },
            event.window
        ));
    }
    for event in keys.read().filter(|e| traced_key(e)) {
        trace.note(key_line(event));
    }
    for _ in opened.read() {
        trace.note("game OverlayOpened");
    }
    for _ in closed.read() {
        trace.note("game OverlayClosed");
    }
    if let Some(window) = windows.iter().next() {
        let wish = (window.cursor_grab, window.cursor_visible);
        if seen.wish != Some(wish) {
            seen.wish = Some(wish);
            trace.note(format_args!(
                "game asks cursor_grab={:?} cursor_visible={}",
                wish.0, wish.1
            ));
        }
    }
    let held = hold.as_deref().is_some_and(|h| h.0);
    if seen.hold != Some(held) {
        seen.hold = Some(held);
        trace.note(format_args!(
            "cursor hold for the overlay: {}",
            if held { "on" } else { "off" }
        ));
    }
    if let Some(calls) = calls.as_deref()
        && *calls != seen.calls
    {
        if calls.grab_calls != seen.calls.grab_calls {
            trace.note(format_args!(
                "os set_cursor_grab call #{} {:?}",
                calls.grab_calls, calls.last_grab
            ));
        }
        if calls.visible_calls != seen.calls.visible_calls {
            trace.note(format_args!(
                "os set_cursor_visible call #{} {}",
                calls.visible_calls, calls.last_visible
            ));
        }
        seen.calls = *calls;
    }
    trace.frame += 1;
    trace.heartbeat_frames += 1;
    if trace.heartbeat_at.elapsed() >= HEARTBEAT_EVERY {
        let frames = std::mem::take(&mut trace.heartbeat_frames);
        trace.heartbeat_at = Instant::now();
        let grab_calls = seen.calls.grab_calls;
        trace.note(format_args!(
            "heartbeat frames={frames} overlay_open={} cursor_hold={held} grab_calls={grab_calls}",
            overlay.0
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(physical: &str, logical: &str, state: ButtonState, repeat: bool) -> KeyboardInput {
        KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: physical.to_owned(),
            logical_key: logical.to_owned(),
            state,
            repeat,
            timestamp: regia_platform::InputTimestamp::now(),
        }
    }

    #[test]
    fn the_header_says_whether_steam_injected_its_overlay() {
        let steam = |key: &str| {
            match key {
            "LD_PRELOAD" => Some(
                "/home/u/.steam/ubuntu12_32/gameoverlayrenderer.so:/home/u/.steam/ubuntu12_64/gameoverlayrenderer.so"
                    .into(),
            ),
            "ENABLE_VK_LAYER_VALVE_steam_overlay_1" => Some("1".into()),
            "SteamGameId" => Some("12345".into()),
            "WAYLAND_DISPLAY" => Some("".into()),
            "DISPLAY" => Some(":0".into()),
            _ => None,
        }
        };
        assert_eq!(
            environment_facts(steam),
            "overlay_renderer_preloaded yes; vk_overlay_layer 1; steam_game_id yes; wayland_display no; x11_display yes"
        );
        assert_eq!(
            environment_facts(|_| None),
            "overlay_renderer_preloaded no; vk_overlay_layer unset; steam_game_id no; wayland_display no; x11_display no"
        );
    }

    #[test]
    fn only_shift_and_tab_are_traced() {
        assert!(traced_key(&key(
            "ShiftLeft",
            "Shift",
            ButtonState::Pressed,
            false
        )));
        assert!(traced_key(&key(
            "ShiftRight",
            "Shift",
            ButtonState::Pressed,
            false
        )));
        assert!(traced_key(&key("Tab", "Tab", ButtonState::Released, false)));
        assert!(!traced_key(&key("KeyW", "w", ButtonState::Pressed, false)));
        assert_eq!(
            key_line(&key("Tab", "Tab", ButtonState::Pressed, true)),
            "key Tab (Tab) down repeat"
        );
        assert_eq!(
            key_line(&key("ShiftLeft", "Shift", ButtonState::Released, false)),
            "key ShiftLeft (Shift) up"
        );
    }
}
