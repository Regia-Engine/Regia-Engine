// SPDX-License-Identifier: MPL-2.0
//! Game tabs: a separate-process play session shown inside the editor (ADR-7831,
//! `book/agents/pie-net.md` § "Game tabs").
//!
//! Each process of a PIE process session (the dedicated server and every client,
//! [`crate::pie_process`]) can be shown in its own **Game** dock tab. The child keeps running
//! offscreen; a worker thread per open tab talks to the child's agent server (the endpoint the
//! child advertises in its own agent directory, as the Profiler's process source does) and:
//!
//! - polls `play.frame` about fifteen times a second (one call in flight), decodes the PNG and
//!   keeps the newest picture with the frame number it was taken on and the round-trip latency;
//! - sends the toolbar's commands in order: `play.pause`, `play.resume`, `play.step`,
//!   `play.set_time_scale`, `play.mute`, and the input the focused tab forwards
//!   (`play.inject_input`: keys, pointer moves, buttons and the wheel).
//!
//! The editor frame never waits on the network: the tab reads the worker's latest snapshot.
//! Closing a tab stops its worker only; the process keeps running unless the tab's "Stop process"
//! (or `editor.pie_process_view { open: false, stop_process: true }`) asks for it. "Pop out"
//! starts that client again in its own window ([`PieProcesses::pop_out`]) and closes the tab.
//!
//! Keyboard: the picture is a focusable widget (Tab reaches it); while it has focus every key goes
//! to the game, Tab and the arrows included, and **Shift+F1** gives the focus back to the editor
//! (the same release key as in-process play, ADR-7205).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use regia_agent::client::{Client, ClientError};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::pie_process::{PieProcesses, ProcessRole};
use crate::profiler_source::{LinkState, parse_role, role_name};
use crate::viewport_agent::Handler;

/// Time between two `play.frame` polls (about 15 pictures a second).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(66);
/// Time between two connection attempts after a failure.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// Longest wait for one answer before the connection is redone.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest picture a tab asks for (`play.frame`'s `max_width`, `max_height`).
pub const STREAM_SIZE: (u32, u32) = (640, 360);
/// The speeds the toolbar steps through.
pub const SPEEDS: [f32; 7] = [0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0];
/// Most input events one forwarded batch carries (`play.inject_input` takes 1024).
pub const MAX_FORWARD_EVENTS: usize = 256;

/// One command a tab sends to its process, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewCommand {
    /// `play.inject_input` with these raw events.
    Input(Vec<Value>),
    /// `play.pause`.
    Pause,
    /// `play.resume`.
    Resume,
    /// `play.step` by this many frames.
    Step(u64),
    /// `play.set_time_scale`.
    Speed(f32),
    /// `play.mute`.
    Mute(bool),
}

impl ViewCommand {
    /// The protocol call.
    #[must_use]
    pub fn call(&self) -> (&'static str, Value) {
        match self {
            Self::Input(events) => ("play.inject_input", json!({ "events": events })),
            Self::Pause => ("play.pause", json!({})),
            Self::Resume => ("play.resume", json!({})),
            Self::Step(frames) => ("play.step", json!({ "frames": frames })),
            Self::Speed(scale) => ("play.set_time_scale", json!({ "scale": scale })),
            Self::Mute(muted) => ("play.mute", json!({ "muted": muted })),
        }
    }
}

/// The newest picture of a process.
#[derive(Clone, Debug)]
pub struct ViewFrame {
    /// The child's frame number when it was taken.
    pub frame: u64,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// The child's window (or feed) size it was scaled from: forwarded pointer positions use it.
    pub source_size: (u32, u32),
    /// RGBA8, top row first.
    pub rgba: Arc<Vec<u8>>,
    /// Request to decoded picture, milliseconds.
    pub latency_ms: f64,
    /// Pictures received before this one plus one: a new number means a new picture.
    pub seq: u64,
}

/// What a tab's worker last saw.
#[derive(Clone, Debug)]
pub struct ViewSnapshot {
    /// The connection state.
    pub link: LinkState,
    /// The newest picture.
    pub frame: Option<ViewFrame>,
    /// Pictures received.
    pub frames_received: u64,
    /// Commands answered (or failed).
    pub commands_done: u64,
    /// The last command's answer.
    pub last_reply: Option<Value>,
    /// The last command's refusal or failure.
    pub last_error: Option<String>,
    /// Why the last `play.frame` was refused (a child with no picture source).
    pub frame_error: Option<String>,
}

impl Default for ViewSnapshot {
    fn default() -> Self {
        Self {
            link: LinkState::Connecting,
            frame: None,
            frames_received: 0,
            commands_done: 0,
            last_reply: None,
            last_error: None,
            frame_error: None,
        }
    }
}

#[derive(Default)]
struct Shared {
    snapshot: Mutex<ViewSnapshot>,
    changed: Condvar,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut ViewSnapshot)) {
        if let Ok(mut s) = self.snapshot.lock() {
            f(&mut s);
        }
        self.changed.notify_all();
    }
}

/// The thread that streams one process's pictures and sends its commands. Dropping it stops the
/// thread at its next wake-up without waiting for it; the process is not touched.
pub struct ViewWorker {
    dir: PathBuf,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    commands: Sender<ViewCommand>,
}

impl std::fmt::Debug for ViewWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewWorker").field("dir", &self.dir).finish()
    }
}

impl Drop for ViewWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl ViewWorker {
    /// Starts streaming the process whose agent directory is `dir`.
    ///
    /// # Errors
    /// The thread could not be spawned.
    pub fn start(dir: PathBuf) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (commands, inbox) = channel();
        let (d, s, x) = (dir.clone(), Arc::clone(&shared), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("pie-game-view".into())
            .spawn(move || run(&d, &s, &x, &inbox))?;
        Ok(Self {
            dir,
            shared,
            stop,
            commands,
        })
    }

    /// The agent directory it reads the endpoint from.
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Queues a command (answered in order, after the ones before it).
    pub fn send(&self, command: ViewCommand) {
        let _ = self.commands.send(command);
    }

    /// A copy of the latest snapshot; never waits on the network.
    #[must_use]
    pub fn snapshot(&self) -> ViewSnapshot {
        match self.shared.snapshot.lock() {
            Ok(s) => s.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Blocks until `done` holds for the snapshot or `timeout` passes; returns the snapshot then.
    /// For tests and tools (a bounded wait on the worker, no sleeps).
    #[must_use]
    pub fn wait_until(&self, timeout: Duration, done: impl Fn(&ViewSnapshot) -> bool) -> ViewSnapshot {
        let guard = match self.shared.snapshot.lock() {
            Ok(g) => g,
            Err(poisoned) => return poisoned.into_inner().clone(),
        };
        match self
            .shared
            .changed
            .wait_timeout_while(guard, timeout, |s| !done(s))
        {
            Ok((g, _)) => g.clone(),
            Err(poisoned) => poisoned.into_inner().0.clone(),
        }
    }
}

fn connect(dir: &std::path::Path) -> Result<Client, String> {
    let (endpoint, token) =
        regia_agent::endpoint::read(dir).map_err(|e| format!("no endpoint in {} yet: {e}", dir.display()))?;
    let mut client = Client::connect(&endpoint.url, &token).map_err(|e| e.to_string())?;
    client
        .set_read_timeout(Some(CALL_TIMEOUT))
        .map_err(|e| e.to_string())?;
    Ok(client)
}

/// Decodes a `play.frame` answer's PNG into RGBA8.
///
/// # Errors
/// No `png_base64`, bad base64 or a PNG that is not 8-bit RGBA.
pub fn decode_frame(answer: &Value) -> Result<(u32, u32, Vec<u8>), String> {
    let text = answer["png_base64"].as_str().ok_or("no png_base64")?;
    let bytes = base64_decode(text)?;
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let size = reader.output_buffer_size().ok_or("picture too large")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!("{:?} {:?} is not RGBA8", info.color_type, info.bit_depth));
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

/// Standard base64 (RFC 4648 § 4, padded).
fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let (mut acc, mut bits) = (0_u32, 0_u32);
    for &c in bytes {
        acc = (acc << 6) | value(c).ok_or("bad base64")?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// The text of a refused call: the error data's `code` and the message.
fn refusal(e: &ClientError) -> String {
    match e {
        ClientError::Rpc(rpc) => {
            let code = rpc
                .data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            format!("{code}: {}", rpc.message)
        }
        other => other.to_string(),
    }
}

fn run(dir: &std::path::Path, shared: &Shared, stop: &AtomicBool, inbox: &Receiver<ViewCommand>) {
    let mut client: Option<Client> = None;
    let mut pending: std::collections::VecDeque<ViewCommand> = std::collections::VecDeque::new();
    let mut next_frame = Instant::now();
    let mut received = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        pending.extend(inbox.try_iter());
        let Some(c) = client.as_mut() else {
            match connect(dir) {
                Ok(c) => {
                    client = Some(c);
                    shared.update(|s| s.link = LinkState::Connected);
                }
                Err(message) => {
                    shared.update(|s| s.link = LinkState::Disconnected(message));
                    match inbox.recv_timeout(RETRY_INTERVAL) {
                        Ok(command) => pending.push_back(command),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            continue;
        };
        // Commands first, in order: a step or a key is never delayed behind a picture.
        let mut dropped = false;
        while let Some(command) = pending.pop_front() {
            let (method, params) = command.call();
            let result = c.call(method, params);
            let transport = matches!(&result, Err(e) if !matches!(e, ClientError::Rpc(_)));
            shared.update(|s| {
                s.commands_done += 1;
                match &result {
                    Ok(answer) => {
                        s.last_reply = Some(json!({ "method": method, "answer": answer }));
                        s.last_error = None;
                    }
                    Err(e) => s.last_error = Some(format!("{method}: {}", refusal(e))),
                }
            });
            if transport {
                dropped = true;
                break;
            }
        }
        if !dropped && Instant::now() >= next_frame {
            let started = Instant::now();
            next_frame = started + FRAME_INTERVAL;
            let _span = tracing::trace_span!("pie_game_view_frame").entered();
            match c.call(
                "play.frame",
                json!({ "max_width": STREAM_SIZE.0, "max_height": STREAM_SIZE.1 }),
            ) {
                Ok(answer) => match decode_frame(&answer) {
                    Ok((width, height, rgba)) => {
                        received += 1;
                        let frame = ViewFrame {
                            frame: answer["frame"].as_u64().unwrap_or(0),
                            width,
                            height,
                            source_size: (
                                answer["source_width"]
                                    .as_u64()
                                    .and_then(|v| u32::try_from(v).ok())
                                    .unwrap_or(width),
                                answer["source_height"]
                                    .as_u64()
                                    .and_then(|v| u32::try_from(v).ok())
                                    .unwrap_or(height),
                            ),
                            rgba: Arc::new(rgba),
                            latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                            seq: received,
                        };
                        shared.update(|s| {
                            s.frame = Some(frame);
                            s.frames_received = received;
                            s.frame_error = None;
                            s.link = LinkState::Connected;
                        });
                    }
                    Err(e) => shared.update(|s| s.frame_error = Some(e)),
                },
                Err(ClientError::Rpc(rpc)) => {
                    let text = refusal(&ClientError::Rpc(rpc));
                    shared.update(|s| s.frame_error = Some(text));
                }
                Err(e) => {
                    shared.update(|s| s.link = LinkState::Disconnected(e.to_string()));
                    dropped = true;
                }
            }
        }
        if dropped {
            client = None;
            continue;
        }
        let wait = next_frame.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(wait) {
            Ok(command) => pending.push_back(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// One open Game tab.
pub struct GameView {
    /// The process it shows.
    pub role: ProcessRole,
    /// The tab forwards input (its picture has keyboard focus).
    pub input_focus: bool,
    /// The toolbar's pause state.
    pub paused: bool,
    /// The toolbar's speed.
    pub speed: f32,
    /// The toolbar's mute.
    pub muted: bool,
    /// Input events forwarded so far.
    pub forwarded: u64,
    worker: Option<ViewWorker>,
    texture: Option<egui::TextureHandle>,
    shown_seq: u64,
    forward_frame: Option<u64>,
}

impl std::fmt::Debug for GameView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameView")
            .field("role", &self.role)
            .field("input_focus", &self.input_focus)
            .field("paused", &self.paused)
            .field("speed", &self.speed)
            .field("muted", &self.muted)
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl GameView {
    fn new(role: ProcessRole) -> Self {
        Self {
            role,
            input_focus: false,
            paused: false,
            speed: 1.0,
            muted: false,
            forwarded: 0,
            worker: None,
            texture: None,
            shown_seq: 0,
            forward_frame: None,
        }
    }

    /// Its worker, while the session runs.
    #[must_use]
    pub fn worker(&self) -> Option<&ViewWorker> {
        self.worker.as_ref()
    }

    /// The worker's snapshot (a disconnected default with no worker).
    #[must_use]
    pub fn snapshot(&self) -> ViewSnapshot {
        self.worker.as_ref().map_or_else(
            || ViewSnapshot {
                link: LinkState::Disconnected("no PIE process session runs".to_owned()),
                ..ViewSnapshot::default()
            },
            ViewWorker::snapshot,
        )
    }

    /// Sends `command` and keeps the toolbar state in step with it.
    pub fn send(&mut self, command: ViewCommand) {
        match &command {
            ViewCommand::Pause => self.paused = true,
            ViewCommand::Resume => self.paused = false,
            ViewCommand::Step(_) => self.paused = true,
            ViewCommand::Speed(s) => self.speed = *s,
            ViewCommand::Mute(m) => self.muted = *m,
            ViewCommand::Input(events) => self.forwarded += events.len() as u64,
        }
        if let Some(worker) = &self.worker {
            worker.send(command);
        }
    }

    /// The `editor.pie_process_view` answer.
    #[must_use]
    pub fn to_json(&self, process_running: bool) -> Value {
        let s = self.snapshot();
        let message = match &s.link {
            LinkState::Disconnected(m) => Some(m.clone()),
            _ => None,
        };
        json!({
            "process": role_name(self.role),
            "open": true,
            "process_running": process_running,
            "link": s.link.as_str(),
            "message": message,
            "frame": s.frame.as_ref().map(|f| f.frame),
            "width": s.frame.as_ref().map(|f| f.width),
            "height": s.frame.as_ref().map(|f| f.height),
            "latency_ms": s.frame.as_ref().map(|f| f.latency_ms),
            "frames_received": s.frames_received,
            "frame_error": s.frame_error,
            "commands_done": s.commands_done,
            "last_reply": s.last_reply,
            "last_error": s.last_error,
            "paused": self.paused,
            "speed": self.speed,
            "muted": self.muted,
            "input_focus": self.input_focus,
            "forwarded": self.forwarded,
        })
    }
}

/// The open Game tabs.
#[derive(Resource, Debug, Default)]
pub struct GameViews {
    views: Vec<GameView>,
    /// A tab the shell brings forward (set by an open).
    pub focus: Option<ProcessRole>,
}

impl GameViews {
    /// The open tabs, in opening order.
    #[must_use]
    pub fn views(&self) -> &[GameView] {
        &self.views
    }

    /// The tab of `role`.
    #[must_use]
    pub fn get(&self, role: ProcessRole) -> Option<&GameView> {
        self.views.iter().find(|v| v.role == role)
    }

    /// The tab of `role`, mutable.
    pub fn get_mut(&mut self, role: ProcessRole) -> Option<&mut GameView> {
        self.views.iter_mut().find(|v| v.role == role)
    }

    /// The roles with an open tab.
    #[must_use]
    pub fn roles(&self) -> Vec<ProcessRole> {
        self.views.iter().map(|v| v.role).collect()
    }
}

/// A refusal: a lowercase code and a message.
pub type ViewError = (&'static str, String);

/// Opens the Game tab of `role` (focusing it when it is open already).
///
/// # Errors
/// `pie_view_no_session` with no PIE process session, `pie_view_no_process` when the session has
/// no running process of that role, `pie_view_worker_failed` when the thread cannot start.
pub fn open(world: &mut World, role: ProcessRole) -> Result<(), ViewError> {
    let Some(procs) = world.get_resource::<PieProcesses>() else {
        return Err(no_session());
    };
    let Some(dir) = procs.agent_dir(role) else {
        return Err(no_session());
    };
    if !procs.process_alive(role) {
        return Err(("pie_view_no_process", format!("{} does not run", role.prefix())));
    }
    let mut views = world.get_resource_or_insert_with(GameViews::default);
    views.focus = Some(role);
    if views.get(role).is_some_and(|v| v.worker.is_some()) {
        return Ok(());
    }
    let worker = ViewWorker::start(dir).map_err(|e| ("pie_view_worker_failed", e.to_string()))?;
    match views.get_mut(role) {
        Some(view) => view.worker = Some(worker),
        None => {
            let mut view = GameView::new(role);
            view.worker = Some(worker);
            views.views.push(view);
        }
    }
    Ok(())
}

fn no_session() -> ViewError {
    (
        "pie_view_no_session",
        "no PIE process session runs (Net tab: separate processes, then Play)".to_owned(),
    )
}

/// Closes the Game tab of `role`: its worker stops; the process keeps running unless
/// `stop_process`. Returns whether a tab was open.
pub fn close(world: &mut World, role: ProcessRole, stop_process: bool) -> bool {
    let was_open = world.get_resource_mut::<GameViews>().is_some_and(|mut views| {
        let before = views.views.len();
        views.views.retain(|v| v.role != role);
        before != views.views.len()
    });
    if stop_process && let Some(mut procs) = world.get_resource_mut::<PieProcesses>() {
        procs.stop_one(role);
    }
    was_open
}

/// Starts client `role` again in its own window and closes its tab.
///
/// # Errors
/// See [`PieProcesses::pop_out`].
pub fn pop_out(world: &mut World, role: ProcessRole) -> Result<(), ViewError> {
    let Some(mut procs) = world.get_resource_mut::<PieProcesses>() else {
        return Err(no_session());
    };
    procs.pop_out(role).map_err(|e| (e.code, e.message))?;
    close(world, role, false);
    Ok(())
}

/// Once per editor frame: a tab whose process moved (a new session) gets a new worker; with no
/// session its worker stops (the tab shows "no session" until play starts again).
pub fn sync(world: &mut World) {
    let roles = match world.get_resource::<GameViews>() {
        Some(views) if !views.views.is_empty() => views.roles(),
        _ => return,
    };
    let dirs: Vec<(ProcessRole, Option<PathBuf>)> = {
        let procs = world.get_resource::<PieProcesses>();
        roles
            .into_iter()
            .map(|r| (r, procs.and_then(|p| p.agent_dir(r))))
            .collect()
    };
    let Some(mut views) = world.get_resource_mut::<GameViews>() else {
        return;
    };
    for (role, dir) in dirs {
        let Some(view) = views.get_mut(role) else {
            continue;
        };
        match dir {
            None => view.worker = None,
            Some(dir) if view.worker.as_ref().map(ViewWorker::dir) != Some(dir.as_path()) => {
                view.worker = ViewWorker::start(dir).ok();
            }
            Some(_) => {}
        }
    }
}

/// The physical key name the runtime uses (`KeyW`, `Digit1`, `ArrowUp`, `Space`, `F1`).
#[must_use]
pub fn physical_key_name(key: egui::Key) -> String {
    let name = key.name();
    if name.len() == 1 && name.as_bytes()[0].is_ascii_uppercase() {
        return format!("Key{name}");
    }
    if name.len() == 1 && name.as_bytes()[0].is_ascii_digit() {
        return format!("Digit{name}");
    }
    match key {
        egui::Key::ArrowUp => "ArrowUp".to_owned(),
        egui::Key::ArrowDown => "ArrowDown".to_owned(),
        egui::Key::ArrowLeft => "ArrowLeft".to_owned(),
        egui::Key::ArrowRight => "ArrowRight".to_owned(),
        egui::Key::Space => "Space".to_owned(),
        egui::Key::Enter => "Enter".to_owned(),
        egui::Key::Escape => "Escape".to_owned(),
        egui::Key::Tab => "Tab".to_owned(),
        egui::Key::Backspace => "Backspace".to_owned(),
        other => format!("{other:?}"),
    }
}

/// The raw events the focused picture forwards this frame: keys (but not Shift+F1), and pointer
/// moves, buttons and the wheel over `rect` mapped to the child's `source` pixels.
#[must_use]
pub fn forward_events(input: &egui::InputState, rect: egui::Rect, source: (u32, u32)) -> Vec<Value> {
    let to_child = |pos: egui::Pos2| -> Option<(f64, f64)> {
        if !rect.contains(pos) || rect.width() <= 0.0 || rect.height() <= 0.0 {
            return None;
        }
        let u = f64::from((pos.x - rect.min.x) / rect.width());
        let v = f64::from((pos.y - rect.min.y) / rect.height());
        Some((u * f64::from(source.0), v * f64::from(source.1)))
    };
    let mut out = Vec::new();
    for event in &input.events {
        match event {
            egui::Event::Key {
                key,
                pressed,
                repeat: false,
                modifiers,
                ..
            } => {
                if *key == egui::Key::F1 && modifiers.shift {
                    continue;
                }
                out.push(json!({
                    "type": "key",
                    "key": physical_key_name(*key),
                    "state": if *pressed { "pressed" } else { "released" },
                }));
            }
            egui::Event::PointerMoved(pos) => {
                if let Some((x, y)) = to_child(*pos) {
                    out.push(json!({ "type": "cursor", "x": x, "y": y }));
                }
            }
            egui::Event::PointerButton {
                pos, button, pressed, ..
            } if to_child(*pos).is_some() => {
                let button = match button {
                    egui::PointerButton::Secondary => "right",
                    egui::PointerButton::Middle => "middle",
                    egui::PointerButton::Extra1 => "back",
                    egui::PointerButton::Extra2 => "forward",
                    egui::PointerButton::Primary => "left",
                };
                out.push(json!({
                    "type": "mouse_button",
                    "button": button,
                    "state": if *pressed { "pressed" } else { "released" },
                }));
            }
            egui::Event::MouseWheel { delta, .. }
                if input.pointer.hover_pos().and_then(to_child).is_some() =>
            {
                out.push(json!({ "type": "wheel", "dx": delta.x, "dy": delta.y }));
            }
            _ => {}
        }
        if out.len() >= MAX_FORWARD_EVENTS {
            break;
        }
    }
    out
}

/// The next speed of [`SPEEDS`] above (`up`) or below `speed`.
#[must_use]
pub fn next_speed(speed: f32, up: bool) -> f32 {
    if up {
        SPEEDS
            .iter()
            .copied()
            .find(|s| *s > speed + 1e-4)
            .unwrap_or(SPEEDS[SPEEDS.len() - 1])
    } else {
        SPEEDS
            .iter()
            .rev()
            .copied()
            .find(|s| *s < speed - 1e-4)
            .unwrap_or(SPEEDS[0])
    }
}

/// The dock tab title of `role`.
#[must_use]
pub fn tab_title(role: ProcessRole) -> String {
    match role {
        ProcessRole::Server => "Game: server".to_owned(),
        ProcessRole::Client(k) => format!("Game: client {k}"),
    }
}

/// What the toolbar asked for this frame.
enum ToolbarAction {
    Command(ViewCommand),
    PopOut,
    StopProcess,
}

/// Draws the Game tab of `role`: the toolbar, the status line and the picture.
pub fn game_tab(ui: &mut egui::Ui, world: &mut World, role: ProcessRole) {
    let process_running = world
        .get_resource::<PieProcesses>()
        .is_some_and(|p| p.process_alive(role));
    let Some(mut views) = world.get_resource_mut::<GameViews>() else {
        ui.label("This Game tab is closed. Open it again from the Net tab.");
        return;
    };
    let Some(view) = views.get_mut(role) else {
        ui.label("This Game tab is closed. Open it again from the Net tab.");
        return;
    };
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let snapshot = view.snapshot();
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        let pause_text = if view.paused { "Resume" } else { "Pause" };
        if ui.button(pause_text).clicked() {
            action = Some(ToolbarAction::Command(if view.paused {
                ViewCommand::Resume
            } else {
                ViewCommand::Pause
            }));
        }
        if ui
            .button("Step")
            .on_hover_text("Run exactly one frame of game time, then pause")
            .clicked()
        {
            action = Some(ToolbarAction::Command(ViewCommand::Step(1)));
        }
        ui.separator();
        if ui.button("Slower").clicked() {
            action = Some(ToolbarAction::Command(ViewCommand::Speed(next_speed(
                view.speed, false,
            ))));
        }
        ui.label(format!("Speed {}x", view.speed));
        if ui.button("Faster").clicked() {
            action = Some(ToolbarAction::Command(ViewCommand::Speed(next_speed(
                view.speed, true,
            ))));
        }
        ui.separator();
        let mut muted = view.muted;
        if ui.checkbox(&mut muted, "Mute").changed() {
            action = Some(ToolbarAction::Command(ViewCommand::Mute(muted)));
        }
        ui.separator();
        let can_pop = matches!(role, ProcessRole::Client(_)) && process_running;
        if ui
            .add_enabled(can_pop, egui::Button::new("Pop out"))
            .on_hover_text("Start this client again in its own window and close the tab")
            .clicked()
        {
            action = Some(ToolbarAction::PopOut);
        }
        if ui
            .add_enabled(process_running, egui::Button::new("Stop process"))
            .on_hover_text("Stop this process; closing the tab alone leaves it running")
            .clicked()
        {
            action = Some(ToolbarAction::StopProcess);
        }
    });
    let status = match &snapshot.frame {
        Some(f) => format!(
            "frame {} · latency {:.0} ms · {} · {}",
            f.frame,
            f.latency_ms,
            snapshot.link.as_str(),
            if process_running { "running" } else { "exited" }
        ),
        None => format!(
            "no picture yet · {} · {}",
            snapshot.link.as_str(),
            if process_running { "running" } else { "exited" }
        ),
    };
    ui.label(status);
    for text in [
        match &snapshot.link {
            LinkState::Disconnected(m) => Some(m.clone()),
            _ => None,
        },
        snapshot.frame_error.clone(),
        snapshot.last_error.clone(),
    ]
    .into_iter()
    .flatten()
    {
        ui.colored_label(tokens.warn, text);
    }
    // The picture: upload a new one when the worker has it.
    if let Some(frame) = &snapshot.frame
        && frame.seq != view.shown_seq
    {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match &mut view.texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                view.texture = Some(ui.ctx().load_texture(
                    format!("pie-game-{}", role_name(role)),
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        view.shown_seq = frame.seq;
    }
    let avail = ui.available_size().max(egui::vec2(32.0, 32.0));
    let aspect = snapshot
        .frame
        .as_ref()
        .map_or(16.0 / 9.0, |f| f.width as f32 / f.height.max(1) as f32);
    let size = if avail.x / avail.y > aspect {
        egui::vec2(avail.y * aspect, avail.y)
    } else {
        egui::vec2(avail.x, avail.x / aspect)
    };
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let spoken = format!(
        "{} picture; focus it to send keys and the mouse to the game, Shift+F1 to leave",
        tab_title(role)
    );
    let response = crate::a11y_audit::name(response, egui::WidgetType::Button, &spoken);
    match &view.texture {
        Some(texture) => {
            // The identity tint: the game's own pixels, not a UI color.
            ui.painter().image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => {
            ui.painter().rect_filled(rect, 0.0, tokens.well);
        }
    }
    if response.clicked() {
        response.request_focus();
    }
    let focused = response.has_focus();
    if focused {
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(2.0, tokens.signal),
            egui::StrokeKind::Inside,
        );
        // Every key goes to the game while the picture has focus.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
        if ui.input(|i| i.modifiers.shift && i.key_pressed(egui::Key::F1)) {
            response.surrender_focus();
        }
    }
    view.input_focus = focused;
    // egui may run this panel twice in a frame: forward each frame's input once.
    let frame_nr = ui.ctx().cumulative_frame_nr();
    if focused && view.forward_frame != Some(frame_nr) {
        view.forward_frame = Some(frame_nr);
        let source = snapshot.frame.as_ref().map_or((0, 0), |f| f.source_size);
        let events = ui.input(|i| forward_events(i, rect, source));
        if !events.is_empty() {
            view.send(ViewCommand::Input(events));
        }
    }
    let action = match action {
        Some(ToolbarAction::Command(command)) => {
            view.send(command);
            None
        }
        other => other,
    };
    match action {
        Some(ToolbarAction::Command(_)) | None => {}
        Some(ToolbarAction::PopOut) => {
            if let Err((code, message)) = pop_out(world, role) {
                tracing::warn!(target: "regia_editor", "Pop out {}: {code}: {message}", role_name(role));
            }
        }
        Some(ToolbarAction::StopProcess) => {
            if let Some(mut procs) = world.get_resource_mut::<PieProcesses>() {
                procs.stop_one(role);
            }
        }
    }
}

/// The Net tab's row of "Show in a Game tab" buttons, one per running process.
pub fn open_buttons(ui: &mut egui::Ui, world: &mut World) {
    let roles: Vec<ProcessRole> = world.get_resource::<PieProcesses>().map_or_else(Vec::new, |p| {
        p.processes()
            .iter()
            .filter(|e| e.exit.is_none() && p.agent_dir(e.role).is_some())
            .map(|e| e.role)
            .collect()
    });
    if roles.is_empty() {
        return;
    }
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.label("Show in a Game tab:");
        for role in roles {
            if ui.button(role_name(role)).clicked() {
                clicked = Some(role);
            }
        }
    });
    if let Some(role) = clicked
        && let Err((code, message)) = open(world, role)
    {
        tracing::warn!(target: "regia_editor", "Game tab {}: {code}: {message}", role_name(role));
    }
}

fn view_error(code: &'static str, message: String) -> RpcError {
    let hint = match code {
        "pie_view_no_session" => {
            "editor.pie_net_config { mode: \"dedicated\", separate_processes: true }, then editor.pie_net_run { action: \"start\" }"
        }
        "pie_view_no_process" => "editor.pie_processes lists the processes and their state",
        "pie_view_not_open" => "open the tab first: { process, open: true }",
        "pie_view_no_window" => "only a client has a window; the server runs headless",
        _ => "see book/agents/pie-net.md (Game tabs)",
    };
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

/// `editor.pie_process_view`.
fn pie_process_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("process")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`process` (\"server\" or \"client <k>\") is required"))?;
    let role = parse_role(text).ok_or_else(|| {
        view_error(
            "pie_view_bad_process",
            format!("`{text}` is not \"server\" or \"client <k>\""),
        )
    })?;
    let open_flag = match params.get("open") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`open` must be true or false"))?,
        ),
    };
    let stop_process = match params.get("stop_process") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`stop_process` must be true or false"))?,
    };
    let action = match params.get("action") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| RpcError::invalid_params("`action` must be a string"))?,
        ),
    };
    let command = match action {
        None => None,
        Some("pause") => Some(ViewCommand::Pause),
        Some("resume") => Some(ViewCommand::Resume),
        Some("step") => {
            let frames = match params.get("frames") {
                None | Some(Value::Null) => 1,
                Some(v) => v
                    .as_u64()
                    .filter(|n| (1..=10_000).contains(n))
                    .ok_or_else(|| RpcError::invalid_params("`frames` must be an integer in 1..=10000"))?,
            };
            Some(ViewCommand::Step(frames))
        }
        Some("speed") => {
            let speed = params
                .get("speed")
                .and_then(Value::as_f64)
                .filter(|s| s.is_finite() && (0.0..=1000.0).contains(s))
                .ok_or_else(|| RpcError::invalid_params("`speed` (0..=1000) is required for action speed"))?;
            Some(ViewCommand::Speed(speed as f32))
        }
        Some("mute") => Some(ViewCommand::Mute(true)),
        Some("unmute") => Some(ViewCommand::Mute(false)),
        Some("input") => {
            let events = params
                .get("events")
                .and_then(Value::as_array)
                .filter(|e| !e.is_empty() && e.len() <= MAX_FORWARD_EVENTS)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`events` (1..={MAX_FORWARD_EVENTS} raw input events, as play.inject_input takes) is required for action input"
                    ))
                })?;
            Some(ViewCommand::Input(events.clone()))
        }
        Some("pop_out") => None,
        Some(other) => {
            return Err(view_error(
                "pie_view_bad_action",
                format!(
                    "unknown action `{other}` (pause, resume, step, speed, mute, unmute, input, pop_out)"
                ),
            ));
        }
    };
    match open_flag {
        Some(true) => open(world, role).map_err(|(c, m)| view_error(c, m))?,
        Some(false) => {
            close(world, role, stop_process);
        }
        None => {}
    }
    if action == Some("pop_out") {
        pop_out(world, role).map_err(|(c, m)| view_error(c, m))?;
    } else if let Some(command) = command {
        let mut views = world.get_resource_or_insert_with(GameViews::default);
        let view = views.get_mut(role).ok_or_else(|| {
            view_error(
                "pie_view_not_open",
                format!("no Game tab shows {}", role_name(role)),
            )
        })?;
        view.send(command);
    }
    let running = world
        .get_resource::<PieProcesses>()
        .is_some_and(|p| p.process_alive(role));
    Ok(world
        .get_resource::<GameViews>()
        .and_then(|v| v.get(role))
        .map_or_else(
            || json!({ "process": role_name(role), "open": false, "process_running": running }),
            |view| view.to_json(running),
        ))
}

/// The `editor.pie_process_view` method.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.pie_process_view",
            tool: "pie_process_view",
            description: "Show a PIE process (separate-process play) in a Game dock tab and drive it: open or close the tab (closing leaves the process running unless stop_process), pause, resume, step, speed, mute or unmute it, forward raw input events to it, or pop a client out into its own window. Answers the tab's state: link, newest frame number and latency, pictures received, toolbar state.",
            params: &[
                ParamSchema {
                    name: "process",
                    kind: "string",
                    required: true,
                    description: "\"server\" or \"client <k>\".",
                },
                ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "true opens (or focuses) the tab, false closes it; absent reads it.",
                },
                ParamSchema {
                    name: "stop_process",
                    kind: "boolean",
                    required: false,
                    description: "With open: false, also stop the process (default false).",
                },
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: false,
                    description: "pause, resume, step, speed, mute, unmute, input, pop_out.",
                },
                ParamSchema {
                    name: "frames",
                    kind: "integer",
                    required: false,
                    description: "Frames for action step (1..=10000, default 1).",
                },
                ParamSchema {
                    name: "speed",
                    kind: "number",
                    required: false,
                    description: "Time scale for action speed (0..=1000).",
                },
                ParamSchema {
                    name: "events",
                    kind: "array",
                    required: false,
                    description: "Raw input events for action input, as play.inject_input takes them (at most 256).",
                },
            ],
            result: "{ process, open, process_running, link?, message?, frame?, width?, height?, latency_ms?, frames_received?, frame_error?, commands_done?, last_reply?, last_error?, paused?, speed?, muted?, input_focus?, forwarded? }",
            mutating: true,
            permission: Permission::Play,
        },
        pie_process_view as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_get_the_runtime_physical_names() {
        assert_eq!(physical_key_name(egui::Key::W), "KeyW");
        assert_eq!(physical_key_name(egui::Key::Num1), "Digit1");
        assert_eq!(physical_key_name(egui::Key::ArrowUp), "ArrowUp");
        assert_eq!(physical_key_name(egui::Key::Space), "Space");
        assert_eq!(physical_key_name(egui::Key::F5), "F5");
    }

    #[test]
    fn speeds_step_through_the_list_and_stop_at_the_ends() {
        assert_eq!(next_speed(1.0, true), 2.0);
        assert_eq!(next_speed(1.0, false), 0.5);
        assert_eq!(next_speed(8.0, true), 8.0);
        assert_eq!(next_speed(0.1, false), 0.1);
        assert_eq!(next_speed(0.3, true), 0.5);
    }

    #[test]
    fn base64_decodes_what_the_agent_encodes() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let text = regia_agent::base64_encode(data);
            assert_eq!(base64_decode(&text).expect("decodes"), data, "{text}");
        }
        assert!(base64_decode("a$b").is_err());
    }

    #[test]
    fn a_frame_answer_decodes_to_rgba() {
        let rgba: Vec<u8> = (0..8 * 4 * 4).map(|i| (i * 7 % 256) as u8).collect();
        let png = regia_agent::play_view::fast_png(&rgba, 8, 4).expect("png");
        let answer = json!({ "png_base64": regia_agent::base64_encode(&png) });
        let (w, h, out) = decode_frame(&answer).expect("decodes");
        assert_eq!((w, h), (8, 4));
        assert_eq!(out, rgba);
        assert!(decode_frame(&json!({})).is_err());
    }

    fn key(key: egui::Key, pressed: bool, shift: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers {
                shift,
                ..egui::Modifiers::NONE
            },
        }
    }

    /// The forwarded batch: keys (Shift+F1 kept for the editor), pointer moves inside the picture
    /// mapped to the child's pixels, buttons inside it; nothing outside.
    #[test]
    fn forwarded_events_map_keys_and_the_pointer_into_the_child() {
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 100.0));
        let raw = egui::RawInput {
            events: vec![
                key(egui::Key::W, true, false),
                key(egui::Key::W, false, false),
                key(egui::Key::F1, true, true),
                egui::Event::PointerMoved(egui::pos2(200.0, 100.0)),
                egui::Event::PointerMoved(egui::pos2(10.0, 10.0)),
                egui::Event::PointerButton {
                    pos: egui::pos2(150.0, 75.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let mut events = Vec::new();
        let mut out = ctx.run_ui(raw, |ui| {
            events = ui.input(|i| forward_events(i, rect, (1280, 720)));
        });
        out.textures_delta.clear();
        assert_eq!(
            events,
            vec![
                json!({ "type": "key", "key": "KeyW", "state": "pressed" }),
                json!({ "type": "key", "key": "KeyW", "state": "released" }),
                json!({ "type": "cursor", "x": 640.0, "y": 360.0 }),
                json!({ "type": "mouse_button", "button": "left", "state": "pressed" }),
            ]
        );
    }

    #[test]
    fn the_method_refuses_bad_processes_and_closed_tabs() {
        let mut world = World::new();
        let code = |e: RpcError| {
            e.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let e = pie_process_view(&mut world, &json!({ "process": "camera" })).expect_err("bad");
        assert_eq!(code(e).as_deref(), Some("pie_view_bad_process"));
        let e =
            pie_process_view(&mut world, &json!({ "process": "client 0", "open": true })).expect_err("none");
        assert_eq!(code(e).as_deref(), Some("pie_view_no_session"));
        let e = pie_process_view(&mut world, &json!({ "process": "client 0", "action": "pause" }))
            .expect_err("not open");
        assert_eq!(code(e).as_deref(), Some("pie_view_not_open"));
        let e = pie_process_view(&mut world, &json!({ "process": "client 0", "action": "warp" }))
            .expect_err("bad action");
        assert_eq!(code(e).as_deref(), Some("pie_view_bad_action"));
        let out = pie_process_view(&mut world, &json!({ "process": "server" })).expect("reads");
        assert_eq!(
            out,
            json!({ "process": "server", "open": false, "process_running": false })
        );
    }

    /// Accessibility (ADR-7831): the tab draws headless, every interactive widget has a name, and
    /// the keyboard reaches the picture (Tab), which then forwards keys and keeps Tab for the
    /// game until Shift+F1 gives the focus back.
    #[test]
    fn the_game_tab_is_named_and_reachable_from_the_keyboard() {
        let mut world = World::new();
        let mut views = GameViews::default();
        views.views.push(GameView::new(ProcessRole::Client(0)));
        world.insert_resource(views);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let run = |world: &mut World, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| game_tab(ui, world, ProcessRole::Client(0)));
            });
            out.textures_delta.clear();
            out
        };
        let out = run(&mut world, Vec::new());
        let update = out.platform_output.accesskit_update.expect("accesskit is on");
        assert!(
            crate::a11y_audit::unnamed_widgets(&update).is_empty(),
            "{:?}",
            crate::a11y_audit::unnamed_widgets(&update)
        );
        // Tab through the toolbar to the picture (egui moves the focus at the end of the frame
        // that saw the Tab, so each press is followed by an empty frame that reads it).
        let focused = |world: &World| {
            world
                .resource::<GameViews>()
                .get(ProcessRole::Client(0))
                .is_some_and(|v| v.input_focus)
        };
        let forwarded = |world: &World| {
            world
                .resource::<GameViews>()
                .get(ProcessRole::Client(0))
                .map_or(0, |v| v.forwarded)
        };
        let mut reached = false;
        for _ in 0..16 {
            run(
                &mut world,
                vec![
                    key(egui::Key::Tab, true, false),
                    key(egui::Key::Tab, false, false),
                ],
            );
            run(&mut world, Vec::new());
            if focused(&world) {
                reached = true;
                break;
            }
        }
        assert!(reached, "Tab never reached the picture");
        let before = forwarded(&world);
        run(
            &mut world,
            vec![key(egui::Key::A, true, false), key(egui::Key::Tab, true, false)],
        );
        run(&mut world, Vec::new());
        assert!(
            focused(&world),
            "Tab went to the game, the picture kept the focus"
        );
        assert_eq!(forwarded(&world) - before, 2, "A and Tab were forwarded");
        let shift = egui::Modifiers {
            shift: true,
            ..egui::Modifiers::NONE
        };
        run(
            &mut world,
            vec![
                egui::Event::ModifiersChanged(shift),
                key(egui::Key::F1, true, true),
            ],
        );
        run(
            &mut world,
            vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)],
        );
        run(&mut world, Vec::new());
        assert!(!focused(&world), "Shift+F1 gives the focus back");
        // The toolbar's state follows its buttons (no process: the commands go nowhere).
        let mut views = world.resource_mut::<GameViews>();
        let view = views.get_mut(ProcessRole::Client(0)).expect("view");
        view.send(ViewCommand::Speed(next_speed(1.0, true)));
        view.send(ViewCommand::Mute(true));
        view.send(ViewCommand::Step(1));
        assert_eq!((view.speed, view.muted, view.paused), (2.0, true, true));
    }
}
