// SPDX-License-Identifier: MPL-2.0
//! Android device screen mirroring with tap forwarding (P-299, ADR-7836,
//! `book/agents/editor-launch.md` § Device screen).
//!
//! While mirroring runs, a worker thread asks the device for a screenshot at a low rate
//! (`adb -s <serial> exec-out screencap -p`, every [`DEFAULT_INTERVAL_MS`] by default; no new
//! dependency and nothing installed on the device), decodes the PNG and keeps the newest frame.
//! The Output Log's launch bar shows it under the bar while the Android target is chosen; a click
//! on the picture, or the keyboard path (the X and Y fields and **Send tap**), sends
//! `adb -s <serial> shell input tap <x> <y>` in device pixels. Coordinates are whole numbers
//! inside the last frame, so the device shell receives two plain integers.
//!
//! The agent drives the same thing with `editor.device_mirror` (start, stop, status, frame, tap)
//! and `editor.launch { target: "android", mirror: true }`.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::launch::{LOG_TARGET, LaunchTools, Launcher};
use crate::viewport_agent::Handler;

/// The default time between two screenshots (two frames a second).
pub const DEFAULT_INTERVAL_MS: u64 = 500;
/// The shortest interval accepted (`interval_ms` is clamped to it).
pub const MIN_INTERVAL_MS: u64 = 100;
/// The longest interval accepted.
pub const MAX_INTERVAL_MS: u64 = 5000;
/// How long one `screencap` or `input tap` call may take before it is killed.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest screenshot accepted (bytes of PNG).
const MAX_PNG_BYTES: usize = 64 << 20;
/// The largest frame edge accepted (pixels).
const MAX_EDGE: u32 = 16_384;

/// The error codes (lowercase ascii, listed on `book/agents/editor-launch.md`).
pub mod codes {
    /// `action` is not start, stop, status, frame or tap.
    pub const BAD_ACTION: &str = "mirror_bad_action";
    /// adb was not found.
    pub const NO_ADB: &str = "mirror_no_adb";
    /// No device was named and no Android launch has one.
    pub const NO_DEVICE: &str = "mirror_no_device";
    /// Mirroring is not running (tap, frame).
    pub const NOT_RUNNING: &str = "mirror_not_running";
    /// No frame has arrived yet (tap, frame).
    pub const NO_FRAME: &str = "mirror_no_frame";
    /// `x` or `y` is missing, not a whole number, or outside the last frame.
    pub const BAD_TAP: &str = "mirror_bad_tap";
    /// `adb shell input tap` could not run or failed.
    pub const TAP_FAILED: &str = "mirror_tap_failed";
    /// `editor.launch { mirror: true }` on a target other than android.
    pub const ANDROID_ONLY: &str = "mirror_android_only";

    /// Every code, for the book page and its test.
    pub const ALL: &[&str] = &[
        BAD_ACTION,
        NO_ADB,
        NO_DEVICE,
        NOT_RUNNING,
        NO_FRAME,
        BAD_TAP,
        TAP_FAILED,
        ANDROID_ONLY,
    ];
}

fn mirror_error(code: i64, regia_code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(code, regia_code, message, hint)
}

/// One decoded screenshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorFrame {
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
    /// RGBA8 pixels, row-major.
    pub rgba: Arc<Vec<u8>>,
    /// 1 for the first frame of a mirror run, then counting up.
    pub seq: u64,
}

/// Decodes a `screencap -p` PNG (8-bit RGBA, RGB, gray or gray-alpha) into RGBA8.
///
/// # Errors
///
/// Not a PNG, another bit depth, an empty or oversized image.
pub fn decode_screencap_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if bytes.len() > MAX_PNG_BYTES {
        return Err(format!("screenshot of {} bytes is too large", bytes.len()));
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("not a PNG screenshot: {e}"))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
        return Err(format!("screenshot size {width}x{height} is out of range"));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "screenshot too large to decode".to_owned())?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("screenshot does not decode: {e}"))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!("screenshot bit depth {:?} is not 8", info.bit_depth));
    }
    let pixels = (width as usize) * (height as usize);
    let data = buf
        .get(..info.buffer_size())
        .ok_or_else(|| "screenshot frame is short".to_owned())?;
    let rgba = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|[r, g, b]| [*r, *g, *b, 255])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|v| [*v, *v, *v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|[v, a]| [*v, *v, *v, *a])
            .collect(),
        png::ColorType::Indexed => return Err("indexed screenshot was not expanded".to_owned()),
    };
    if rgba.len() != pixels * 4 {
        return Err("screenshot pixel data is short".to_owned());
    }
    Ok((width, height, rgba))
}

/// The device pixel under `pos` when the frame of `size` is drawn into `rect`; `None` outside.
#[must_use]
pub fn map_to_device(pos: egui::Pos2, rect: egui::Rect, size: [u32; 2]) -> Option<(u32, u32)> {
    if !rect.contains(pos) || rect.width() <= 0.0 || rect.height() <= 0.0 || size[0] == 0 || size[1] == 0 {
        return None;
    }
    let u = (pos.x - rect.min.x) / rect.width();
    let v = (pos.y - rect.min.y) / rect.height();
    let x = ((u * size[0] as f32) as u32).min(size[0] - 1);
    let y = ((v * size[1] as f32) as u32).min(size[1] - 1);
    Some((x, y))
}

/// The largest size with the frame's aspect that fits `avail`.
#[must_use]
pub fn fit_size(avail: egui::Vec2, size: [u32; 2]) -> egui::Vec2 {
    if size[0] == 0 || size[1] == 0 || avail.x <= 0.0 || avail.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (avail.x / size[0] as f32).min(avail.y / size[1] as f32);
    egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale)
}

/// What the worker and the editor share.
#[derive(Default)]
struct Shared {
    frame: Mutex<Option<MirrorFrame>>,
    frames: AtomicU64,
    errors: AtomicU64,
    taps: AtomicU64,
    last_error: Mutex<Option<String>>,
    /// Set by stop; the worker waits on `wake` between frames.
    stop: Mutex<bool>,
    wake: Condvar,
}

impl Shared {
    fn note_error(&self, error: String) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(target: LOG_TARGET, "device mirror: {error}");
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        self.wake.notify_all();
    }

    fn stopped(&self) -> bool {
        *self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// An adb invocation for one device.
#[derive(Clone, Debug)]
struct AdbTarget {
    adb: PathBuf,
    serial: String,
    path_env: Option<OsString>,
}

impl AdbTarget {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.adb);
        if let Some(path) = &self.path_env {
            cmd.env("PATH", path);
        }
        cmd.args(["-s", &self.serial]);
        cmd
    }
}

/// Runs `cmd` to completion with a time limit; returns its stdout or why it failed.
fn run_bounded(mut cmd: Command, limit: usize) -> Result<Vec<u8>, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    regia_platform::child_bind::bind_to_parent(&mut cmd);
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {program}: {e}"))?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(limit as u64 + 1).read_to_end(&mut out);
        }
        out
    });
    let stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut err = String::new();
        if let Some(stderr) = stderr {
            let _ = stderr.take(4096).read_to_string(&mut err);
        }
        err
    });
    let deadline = Instant::now() + CALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("cannot wait for {program}: {e}")),
        }
    };
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    match status {
        None => Err(format!("{program} took longer than {} s", CALL_TIMEOUT.as_secs())),
        Some(status) if !status.success() => Err(format!("{program} failed ({status}): {}", err.trim())),
        Some(_) if out.len() > limit => Err(format!("{program} wrote more than {limit} bytes")),
        Some(_) => Ok(out),
    }
}

/// One running mirror.
struct MirrorRun {
    target: AdbTarget,
    interval_ms: u64,
    shared: Arc<Shared>,
}

/// The editor's device mirror: at most one runs. A world resource shared by the launch bar and
/// `editor.device_mirror`.
#[derive(Resource, Default)]
pub struct DeviceMirror {
    run: Option<MirrorRun>,
    /// The texture of the frame shown, and its `seq`.
    texture: Option<(egui::TextureHandle, u64)>,
    /// The keyboard tap position (device pixels).
    tap_x: u32,
    tap_y: u32,
    /// The egui frame whose click was handled (a frame may run the UI twice).
    handled_frame: Option<u64>,
    /// The last tap or start outcome, shown next to the controls.
    note: String,
}

impl Drop for DeviceMirror {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A point-in-time view of the mirror.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MirrorStatus {
    /// Whether a mirror runs.
    pub running: bool,
    /// Its device serial.
    pub device: Option<String>,
    /// Frames decoded so far.
    pub frames: u64,
    /// Failed screenshots or taps.
    pub errors: u64,
    /// Taps sent.
    pub taps: u64,
    /// The newest failure.
    pub last_error: Option<String>,
    /// The newest frame's size.
    pub size: Option<[u32; 2]>,
    /// The interval between screenshots.
    pub interval_ms: u64,
}

impl DeviceMirror {
    /// Starts mirroring `serial` (replacing a running mirror) with `adb` from `tools`.
    ///
    /// # Errors
    ///
    /// [`codes::NO_ADB`] when adb is not found.
    pub fn start(&mut self, tools: &LaunchTools, serial: &str, interval_ms: u64) -> Result<(), RpcError> {
        let Some(adb) = tools.adb_program() else {
            return Err(mirror_error(
                UNSUPPORTED,
                codes::NO_ADB,
                "adb not found (no SDK platform-tools, not on PATH)",
                "install the Android SDK platform-tools",
            ));
        };
        self.stop();
        let target = AdbTarget {
            adb,
            serial: serial.to_owned(),
            path_env: tools.path_env.clone(),
        };
        let interval_ms = interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        let worker_target = target.clone();
        let spawned = std::thread::Builder::new()
            .name("device-mirror".to_owned())
            .spawn(move || capture_loop(&worker_target, &worker, interval_ms));
        if let Err(e) = spawned {
            return Err(RpcError::internal(format!("cannot start the mirror thread: {e}")));
        }
        tracing::info!(target: LOG_TARGET, "device mirror of {serial} started ({interval_ms} ms)");
        self.run = Some(MirrorRun {
            target,
            interval_ms,
            shared,
        });
        self.texture = None;
        Ok(())
    }

    /// Stops mirroring; `false` when none ran. The worker ends after its current screenshot.
    pub fn stop(&mut self) -> bool {
        let Some(run) = self.run.take() else {
            return false;
        };
        *run.shared
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        run.shared.wake.notify_all();
        self.texture = None;
        true
    }

    /// The newest frame.
    #[must_use]
    pub fn frame(&self) -> Option<MirrorFrame> {
        self.run.as_ref().and_then(|run| {
            run.shared
                .frame
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
    }

    /// The mirror's state.
    #[must_use]
    pub fn status(&self) -> MirrorStatus {
        let Some(run) = &self.run else {
            return MirrorStatus::default();
        };
        MirrorStatus {
            running: true,
            device: Some(run.target.serial.clone()),
            frames: run.shared.frames.load(Ordering::Relaxed),
            errors: run.shared.errors.load(Ordering::Relaxed),
            taps: run.shared.taps.load(Ordering::Relaxed),
            last_error: run
                .shared
                .last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            size: self.frame().map(|f| [f.width, f.height]),
            interval_ms: run.interval_ms,
        }
    }

    /// Blocks until `frames` frames arrived or an error was recorded, or `timeout` passes; returns
    /// whether `frames` frames arrived.
    pub fn wait_frames(&self, frames: u64, timeout: Duration) -> bool {
        let Some(run) = &self.run else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        let mut guard = run
            .shared
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if run.shared.frames.load(Ordering::Relaxed) >= frames {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            guard = match run
                .shared
                .wake
                .wait_timeout(guard, left.min(Duration::from_millis(50)))
            {
                Ok((g, _)) => g,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Sends a tap at device pixel (`x`, `y`) (`adb shell input tap`); the point must lie in the
    /// newest frame. Blocks until adb answered (at most 10 s).
    ///
    /// # Errors
    ///
    /// [`codes::NOT_RUNNING`], [`codes::NO_FRAME`], [`codes::BAD_TAP`], [`codes::TAP_FAILED`].
    pub fn tap(&self, x: u32, y: u32) -> Result<(), RpcError> {
        let run = self.tap_target(x, y)?;
        send_tap(&run.target, &run.shared, x, y)
    }

    /// [`Self::tap`] for the editor UI: checks the point, then sends the tap on a thread of its own,
    /// so a slow `adb shell input tap` (up to its 10 s bound) never stalls the editor's frame
    /// (review sprint-w12-rev-g14 #1). The outcome lands in the status (`taps`, `last_error`).
    ///
    /// # Errors
    ///
    /// The checks of [`Self::tap`], or a thread that could not start.
    pub fn tap_in_background(&self, x: u32, y: u32) -> Result<(), RpcError> {
        let run = self.tap_target(x, y)?;
        let (target, shared) = (run.target.clone(), Arc::clone(&run.shared));
        std::thread::Builder::new()
            .name("regia-device-tap".into())
            .spawn(move || {
                let _ = send_tap(&target, &shared, x, y);
            })
            .map(|_| ())
            .map_err(|e| {
                mirror_error(
                    UNSUPPORTED,
                    codes::TAP_FAILED,
                    format!("cannot start the tap thread: {e}"),
                    "try again",
                )
            })
    }

    /// The running mirror, when `(x, y)` is a point on its last frame.
    fn tap_target(&self, x: u32, y: u32) -> Result<&MirrorRun, RpcError> {
        let Some(run) = &self.run else {
            return Err(not_running());
        };
        let Some(frame) = self.frame() else {
            return Err(mirror_error(
                NOT_FOUND,
                codes::NO_FRAME,
                "no screenshot has arrived yet",
                "wait for editor.device_mirror status frames > 0",
            ));
        };
        if x >= frame.width || y >= frame.height {
            return Err(mirror_error(
                INVALID_PARAMS,
                codes::BAD_TAP,
                format!(
                    "({x}, {y}) is outside the {}x{} screen",
                    frame.width, frame.height
                ),
                "tap inside the device screen (device pixels)",
            ));
        }
        Ok(run)
    }
}

/// Runs `adb shell input tap x y` on `target` (bounded wait) and counts it in `shared`.
fn send_tap(target: &AdbTarget, shared: &Shared, x: u32, y: u32) -> Result<(), RpcError> {
    let mut cmd = target.command();
    cmd.args(["shell", "input", "tap", &x.to_string(), &y.to_string()]);
    match run_bounded(cmd, 64 * 1024) {
        Ok(_) => {
            shared.taps.fetch_add(1, Ordering::Relaxed);
            tracing::info!(target: LOG_TARGET, "device mirror: tap ({x}, {y}) on {}", target.serial);
            Ok(())
        }
        Err(e) => {
            shared.note_error(e.clone());
            Err(mirror_error(
                UNSUPPORTED,
                codes::TAP_FAILED,
                e,
                "check the device is attached and unlocked",
            ))
        }
    }
}

fn not_running() -> RpcError {
    mirror_error(
        NOT_FOUND,
        codes::NOT_RUNNING,
        "the device mirror is not running",
        "start it with editor.device_mirror { action: \"start\" }",
    )
}

/// The worker: a screenshot, decode, keep; then wait `interval_ms` or until stop.
fn capture_loop(target: &AdbTarget, shared: &Shared, interval_ms: u64) {
    let mut seq = 0u64;
    while !shared.stopped() {
        let started = Instant::now();
        {
            let _span = tracing::debug_span!(target: LOG_TARGET, "device_mirror_capture").entered();
            let mut cmd = target.command();
            cmd.args(["exec-out", "screencap", "-p"]);
            match run_bounded(cmd, MAX_PNG_BYTES).and_then(|png| decode_screencap_png(&png)) {
                Ok((width, height, rgba)) => {
                    seq += 1;
                    *shared
                        .frame
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(MirrorFrame {
                        width,
                        height,
                        rgba: Arc::new(rgba),
                        seq,
                    });
                    shared.frames.fetch_add(1, Ordering::Relaxed);
                    shared.wake.notify_all();
                }
                Err(e) => shared.note_error(e),
            }
        }
        let wait = Duration::from_millis(interval_ms).saturating_sub(started.elapsed());
        let guard = shared
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *guard {
            break;
        }
        drop(shared.wake.wait_timeout(guard, wait));
    }
}

/// The device a mirror starts on: `device` when given, else the current Android launch's.
fn launch_device(world: &World) -> Option<String> {
    world
        .get_resource::<Launcher>()
        .and_then(Launcher::status)
        .filter(|s| s.target == crate::launch::LaunchTarget::Android)
        .and_then(|s| s.device)
}

/// Starts the mirror on `device` (or the Android launch's device) with the launcher's tools.
///
/// # Errors
///
/// [`codes::NO_DEVICE`], [`codes::NO_ADB`].
pub fn start(world: &mut World, device: Option<String>, interval_ms: u64) -> Result<(), RpcError> {
    let Some(serial) = device.or_else(|| launch_device(world)) else {
        return Err(mirror_error(
            INVALID_PARAMS,
            codes::NO_DEVICE,
            "no device named and no Android launch has one",
            "pass device (editor.launch_devices lists them) or launch on Android first",
        ));
    };
    let tools = world
        .get_resource::<Launcher>()
        .map(|l| l.tools.clone())
        .unwrap_or_default();
    world
        .get_resource_or_insert_with(DeviceMirror::default)
        .start(&tools, &serial, interval_ms)
}

// ---------------------------------------------------------------------------
// The panel (under the launch bar while the Android target is chosen)
// ---------------------------------------------------------------------------

/// Draws the mirror controls and, while it runs, the device screen. A click on the screen taps
/// there; the X and Y fields and **Send tap** are the keyboard path.
pub fn mirror_ui(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!(target: LOG_TARGET, "device_mirror_ui").entered();
    let frame_nr = ui.ctx().cumulative_frame_nr();
    let mut mirror = world.remove_resource::<DeviceMirror>().unwrap_or_default();
    let status = mirror.status();
    let mut clicked_start = false;
    let mut clicked_stop = false;
    let mut tap_at: Option<(u32, u32)> = None;
    ui.horizontal(|ui| {
        ui.label("Device screen:");
        clicked_start = crate::agent_parity::action_enabled(
            ui,
            !status.running,
            &crate::agent_parity::MIRROR_START,
            None,
        )
        .on_hover_text("Show the device's screen here (a screenshot at most twice a second)")
        .clicked();
        clicked_stop =
            crate::agent_parity::action_enabled(ui, status.running, &crate::agent_parity::MIRROR_STOP, None)
                .clicked();
        if status.running {
            let size = status.size.unwrap_or([0, 0]);
            ui.label("X");
            ui.add(egui::DragValue::new(&mut mirror.tap_x).range(0..=size[0].saturating_sub(1)))
                .on_hover_text("Tap position, device pixels from the left");
            ui.label("Y");
            ui.add(egui::DragValue::new(&mut mirror.tap_y).range(0..=size[1].saturating_sub(1)))
                .on_hover_text("Tap position, device pixels from the top");
            if crate::agent_parity::action_enabled(
                ui,
                status.size.is_some(),
                &crate::agent_parity::MIRROR_TAP,
                None,
            )
            .clicked()
            {
                tap_at = Some((mirror.tap_x, mirror.tap_y));
            }
            ui.weak(format!(
                "{} frames, {} taps{}",
                status.frames,
                status.taps,
                status
                    .device
                    .as_deref()
                    .map(|d| format!(" on {d}"))
                    .unwrap_or_default()
            ));
            if let Some(error) = &status.last_error {
                ui.weak(format!("{} errors", status.errors)).on_hover_text(error);
            }
        }
        if !mirror.note.is_empty() {
            ui.weak(&mirror.note);
        }
    });
    if let Some(frame) = mirror.frame() {
        let stale = mirror.texture.as_ref().is_none_or(|(_, seq)| *seq != frame.seq);
        if stale {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            let handle = match mirror.texture.take() {
                Some((mut handle, _)) => {
                    handle.set(image, egui::TextureOptions::LINEAR);
                    handle
                }
                None => ui
                    .ctx()
                    .load_texture("device-mirror", image, egui::TextureOptions::LINEAR),
            };
            mirror.texture = Some((handle, frame.seq));
        }
        if let Some((handle, _)) = &mirror.texture {
            let avail = egui::vec2(ui.available_width(), ui.available_height().clamp(120.0, 520.0));
            let size = fit_size(avail, [frame.width, frame.height]);
            let response = ui
                .add(egui::Image::new((handle.id(), size)).sense(egui::Sense::click()))
                .on_hover_text("Click to tap the device here");
            if response.clicked()
                && let Some(pos) = response.interact_pointer_pos()
                && let Some(point) = map_to_device(pos, response.rect, [frame.width, frame.height])
            {
                tap_at = Some(point);
            }
        }
    }
    let fresh = mirror.handled_frame != Some(frame_nr);
    if fresh && (clicked_start || clicked_stop || tap_at.is_some()) {
        mirror.handled_frame = Some(frame_nr);
    }
    if fresh && clicked_stop {
        mirror.stop();
        mirror.note.clear();
    }
    if fresh && let Some((x, y)) = tap_at {
        mirror.tap_x = x;
        mirror.tap_y = y;
        // Off the UI thread: the status line's tap count shows when adb finished.
        mirror.note = match mirror.tap_in_background(x, y) {
            Ok(()) => format!("tap ({x}, {y}) sent"),
            Err(e) => e.message,
        };
    }
    world.insert_resource(mirror);
    if fresh && clicked_start {
        let note = match start(world, None, DEFAULT_INTERVAL_MS) {
            Ok(()) => String::new(),
            Err(e) => e.message,
        };
        world.resource_mut::<DeviceMirror>().note = note;
    }
    if status.running {
        // New frames arrive on the worker thread; repaint at the mirror's rate.
        ui.ctx()
            .request_repaint_after(Duration::from_millis(status.interval_ms.max(MIN_INTERVAL_MS)));
    }
}

// ---------------------------------------------------------------------------
// Agent method
// ---------------------------------------------------------------------------

fn status_json(mirror: Option<&DeviceMirror>) -> Value {
    let status = mirror.map(DeviceMirror::status).unwrap_or_default();
    json!({
        "running": status.running,
        "device": status.device,
        "frames": status.frames,
        "taps": status.taps,
        "errors": status.errors,
        "last_error": status.last_error,
        "width": status.size.map(|s| s[0]),
        "height": status.size.map(|s| s[1]),
        "interval_ms": status.interval_ms,
    })
}

fn coord(params: &Value, name: &str) -> Result<u32, RpcError> {
    params
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            mirror_error(
                INVALID_PARAMS,
                codes::BAD_TAP,
                format!("`{name}` must be a whole number of device pixels"),
                "pass x and y inside the device screen",
            )
        })
}

/// `editor.device_mirror { action, device?, interval_ms?, x?, y? }`.
fn editor_device_mirror(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or("status");
    match action {
        "status" => {}
        "start" => {
            let device = match params.get("device") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(_) => return Err(RpcError::wrong_type("device", "an adb serial string")),
            };
            let interval = params
                .get("interval_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_INTERVAL_MS);
            start(world, device, interval)?;
        }
        "stop" => {
            let stopped = world
                .get_resource_mut::<DeviceMirror>()
                .is_some_and(|mut m| m.stop());
            let mut out = status_json(world.get_resource::<DeviceMirror>());
            if let Value::Object(map) = &mut out {
                map.insert("stopped".into(), json!(stopped));
            }
            return Ok(out);
        }
        "tap" => {
            let (x, y) = (coord(params, "x")?, coord(params, "y")?);
            let Some(mirror) = world.get_resource::<DeviceMirror>() else {
                return Err(not_running());
            };
            mirror.tap(x, y)?;
        }
        "frame" => {
            let Some(mirror) = world.get_resource::<DeviceMirror>() else {
                return Err(not_running());
            };
            if !mirror.status().running {
                return Err(not_running());
            }
            let Some(frame) = mirror.frame() else {
                return Err(mirror_error(
                    NOT_FOUND,
                    codes::NO_FRAME,
                    "no screenshot has arrived yet",
                    "wait for frames > 0 in the status",
                ));
            };
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            let png = crate::render::viewport_png_base64(&image).map_err(RpcError::internal)?;
            let mut out = status_json(Some(mirror));
            if let Value::Object(map) = &mut out {
                map.insert("seq".into(), json!(frame.seq));
                map.insert("png_base64".into(), json!(png));
            }
            return Ok(out);
        }
        other => {
            return Err(mirror_error(
                INVALID_PARAMS,
                codes::BAD_ACTION,
                format!("unknown action `{other}`"),
                "action is start, stop, status, frame or tap",
            ));
        }
    }
    Ok(status_json(world.get_resource::<DeviceMirror>()))
}

/// The device mirror method.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.device_mirror",
            tool: "device_mirror",
            description: "Mirror an Android device's screen in the editor and forward taps: `start` (device, or the \
                Android launch's device; screenshots with adb exec-out screencap -p every interval_ms, default 500), \
                `stop`, `status` (running, device, frames, taps, errors, width, height), `frame` (the newest \
                screenshot as png_base64) and `tap` (x, y in device pixels inside the newest frame; adb shell input \
                tap). Errors: mirror_bad_action, mirror_no_adb, mirror_no_device, mirror_not_running, \
                mirror_no_frame, mirror_bad_tap, mirror_tap_failed.",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: false,
                    description: "start, stop, status (default), frame or tap.",
                },
                ParamSchema {
                    name: "device",
                    kind: "string",
                    required: false,
                    description: "start: the adb serial (default: the Android launch's device).",
                },
                ParamSchema {
                    name: "interval_ms",
                    kind: "integer",
                    required: false,
                    description: "start: milliseconds between screenshots (100..=5000, default 500).",
                },
                ParamSchema {
                    name: "x",
                    kind: "integer",
                    required: false,
                    description: "tap: device pixels from the left.",
                },
                ParamSchema {
                    name: "y",
                    kind: "integer",
                    required: false,
                    description: "tap: device pixels from the top.",
                },
            ],
            result: "{ running, device, frames, taps, errors, last_error, width, height, interval_ms, stopped?, seq?, \
                png_base64? }",
            mutating: true,
            permission: Permission::Build,
        },
        editor_device_mirror as Handler,
    )]
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use std::path::Path;

    use super::*;

    fn png_bytes(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(rgba).expect("data");
        drop(writer);
        out
    }

    /// A fake adb that serves `screen.png` for screencap and records every call.
    fn fake_adb(dir: &Path) -> PathBuf {
        let adb = dir.join("adb");
        let tmp = dir.join("adb.tmp");
        std::fs::write(
            &tmp,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"{log}\"\nif [ \"$3 $4\" = \"exec-out screencap\" ]; then cat \"{png}\"; fi\n",
                log = dir.join("argv.log").display(),
                png = dir.join("screen.png").display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::rename(&tmp, &adb).expect("publish");
        adb
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn device_mirror_decodes_screencap_pngs() {
        let rgba: Vec<u8> = (0..2 * 3 * 4).map(|v| v as u8).collect();
        let (w, h, out) = decode_screencap_png(&png_bytes(2, 3, &rgba)).expect("decodes");
        assert_eq!((w, h), (2, 3));
        assert_eq!(out, rgba);
        assert!(decode_screencap_png(b"not a png").is_err());
        assert!(decode_screencap_png(&[]).is_err());
    }

    #[test]
    fn device_mirror_maps_clicks_to_device_pixels_and_fits_the_frame() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 200.0));
        let size = [1080, 2400];
        assert_eq!(map_to_device(egui::pos2(10.0, 20.0), rect, size), Some((0, 0)));
        assert_eq!(
            map_to_device(egui::pos2(60.0, 120.0), rect, size),
            Some((540, 1200))
        );
        assert_eq!(
            map_to_device(egui::pos2(110.0, 220.0), rect, size),
            Some((1079, 2399)),
            "the far edge clamps inside the screen"
        );
        assert_eq!(map_to_device(egui::pos2(9.0, 20.0), rect, size), None);
        assert_eq!(map_to_device(egui::pos2(50.0, 50.0), rect, [0, 0]), None);
        let fitted = fit_size(egui::vec2(400.0, 400.0), [1080, 2400]);
        assert!(
            (fitted.y - 400.0).abs() < 1e-3 && (fitted.x - 180.0).abs() < 1e-3,
            "{fitted:?}"
        );
        assert_eq!(fit_size(egui::vec2(0.0, 10.0), size), egui::Vec2::ZERO);
    }

    #[test]
    fn device_mirror_captures_frames_and_forwards_taps_through_adb() {
        let dir = regia_test_utils::unique_temp_dir("device-mirror");
        let rgba = vec![200u8; 4 * 6 * 4];
        std::fs::write(dir.join("screen.png"), png_bytes(4, 6, &rgba)).expect("png");
        let adb = fake_adb(&dir);
        let tools = LaunchTools {
            adb: Some(adb),
            sdk: crate::launch::SdkSource::None,
            ..LaunchTools::default()
        };
        let mut world = World::new();
        let mut launcher = Launcher::default();
        launcher.tools = tools;
        world.insert_resource(launcher);

        // No device named and no Android launch: refused.
        let e = editor_device_mirror(&mut world, &json!({ "action": "start" })).expect_err("no device");
        assert_eq!(code(&e), codes::NO_DEVICE);
        let e =
            editor_device_mirror(&mut world, &json!({ "action": "tap", "x": 1, "y": 1 })).expect_err("idle");
        assert_eq!(code(&e), codes::NOT_RUNNING);

        let started = editor_device_mirror(
            &mut world,
            &json!({ "action": "start", "device": "TESTSERIAL", "interval_ms": 1 }),
        )
        .expect("start");
        assert_eq!(started["running"], true);
        assert_eq!(started["interval_ms"], MIN_INTERVAL_MS, "the interval is clamped");
        assert!(
            world
                .resource::<DeviceMirror>()
                .wait_frames(1, Duration::from_secs(60)),
            "a frame arrived: {:?}",
            world.resource::<DeviceMirror>().status()
        );
        let status = editor_device_mirror(&mut world, &json!({})).expect("status");
        assert_eq!(
            (status["width"].as_u64(), status["height"].as_u64()),
            (Some(4), Some(6))
        );
        let frame = editor_device_mirror(&mut world, &json!({ "action": "frame" })).expect("frame");
        assert!(frame["png_base64"].as_str().is_some_and(|s| !s.is_empty()));

        editor_device_mirror(&mut world, &json!({ "action": "tap", "x": 3, "y": 5 })).expect("tap");
        let e = editor_device_mirror(&mut world, &json!({ "action": "tap", "x": 4, "y": 0 }))
            .expect_err("outside");
        assert_eq!(code(&e), codes::BAD_TAP);
        let e = editor_device_mirror(&mut world, &json!({ "action": "tap", "x": -1, "y": 0 }))
            .expect_err("negative");
        assert_eq!(code(&e), codes::BAD_TAP);
        let e = editor_device_mirror(&mut world, &json!({ "action": "wiggle" })).expect_err("bad action");
        assert_eq!(code(&e), codes::BAD_ACTION);

        let stopped = editor_device_mirror(&mut world, &json!({ "action": "stop" })).expect("stop");
        assert_eq!(stopped["stopped"], true);
        assert_eq!(stopped["running"], false);
        let calls = std::fs::read_to_string(dir.join("argv.log")).expect("argv");
        assert!(
            calls.lines().any(|l| l == "-s TESTSERIAL exec-out screencap -p"),
            "{calls}"
        );
        assert!(
            calls.lines().any(|l| l == "-s TESTSERIAL shell input tap 3 5"),
            "{calls}"
        );
        assert_eq!(
            calls.lines().filter(|l| l.contains("input tap")).count(),
            1,
            "refused taps never reach the device: {calls}"
        );
        for c in codes::ALL {
            assert!(c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "{c}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_mirror_panel_keyboard_path_reaches_start_and_send_tap() {
        // The panel's controls are focusable buttons and drag values (no mouse-only action): Tab
        // reaches Start mirror, and Space presses it. Without adb the press reports why.
        let ctx = egui::Context::default();
        let mut world = World::new();
        let mut launcher = Launcher::default();
        launcher.tools = LaunchTools {
            sdk: crate::launch::SdkSource::None,
            path_env: Some(OsString::new()),
            ..LaunchTools::default()
        };
        world.insert_resource(launcher);
        let mut run = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| mirror_ui(ui, &mut world));
            output.textures_delta.clear();
        };
        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        run(Vec::new());
        run(vec![key(egui::Key::Tab)]);
        run(vec![key(egui::Key::Space)]);
        run(Vec::new());
        let note = world.resource::<DeviceMirror>().note.clone();
        assert!(
            note.contains("no device") || note.contains("adb"),
            "Start mirror was pressed from the keyboard: {note:?}"
        );
    }

    /// Review sprint-w12-rev-g14 #1: the panel's tap returns while adb is still running (the fake adb
    /// below blocks until the test releases it), so the editor frame never waits on the device.
    #[test]
    fn a_panel_tap_never_waits_for_adb() {
        let dir = regia_test_utils::unique_temp_dir("device-mirror-tap");
        std::fs::create_dir_all(&dir).expect("test io");
        let release = dir.join("release");
        let adb = dir.join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nwhile [ ! -e '{}' ]; do sleep 0.02; done\n",
                release.display()
            ),
        )
        .expect("test io");
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).expect("test io");
        let shared = Arc::new(Shared::default());
        *shared.frame.lock().expect("test io") = Some(MirrorFrame {
            width: 100,
            height: 200,
            rgba: Arc::new(vec![0; 100 * 200 * 4]),
            seq: 1,
        });
        let mut mirror = DeviceMirror::default();
        mirror.run = Some(MirrorRun {
            target: AdbTarget {
                adb,
                serial: "fake".into(),
                path_env: None,
            },
            interval_ms: DEFAULT_INTERVAL_MS,
            shared: Arc::clone(&shared),
        });
        mirror.tap_in_background(10, 20).expect("test io");
        assert_eq!(
            shared.taps.load(Ordering::Relaxed),
            0,
            "returned while adb is still held"
        );
        assert_eq!(
            mirror
                .tap_in_background(500, 20)
                .expect_err("a point off the screen")
                .data
                .expect("test io")["code"],
            codes::BAD_TAP,
            "the point is still checked first"
        );
        std::fs::write(&release, "").expect("test io");
        // The mirror's Drop stops the run; the tap thread ends once adb returns.
        drop(mirror);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
