// SPDX-License-Identifier: MPL-2.0
//! Play-in-editor with several clients, and the net stats / bandwidth panel (`docs/specs/77`
//! § 10.2 and § 10.4, `book/agents/pie-net.md`, ADR-1909).
//!
//! PIE gains a mode (`offline`, `listen`, `dedicated`) and a client count. When play starts with a
//! mode other than `offline`, [`PieNet`] hosts a [`NetSession`] without a socket and joins
//! `clients` in-memory clients to it: one process, N clients over the loopback transport, behind
//! the seeded session conditioner. A client may store its own preset (`pie_client_conditioner`,
//! ADR-3996); the default is that session preset. The session runs on a manual clock that the editor
//! advances by [`FRAME_MS`] per frame (or the agent by `editor.pie_net_run { action: "step" }`),
//! so a run with the same configuration reports the same numbers every time.
//!
//! The Net panel and `editor.pie_net_state` read the same rows: [`NetSession::connection_stats`]
//! for the overlay and [`NetSession::bandwidth_view`] for the per-message-kind byte table.
//! The clients here are session-level (handshake, join, streams, game messages). With a game crate
//! open, [`crate::pie_worlds`] gives each one its own ECS world and view (ADR-2623).

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_net::session::NetSession;
use regia_net::session::conditioner::{SESSION_CONDITIONER_PRESETS, SessionConditioner};
use regia_net::session::lifecycle::HostOptions;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// Most clients one PIE session joins.
pub const MAX_PIE_CLIENTS: u32 = 16;
/// Session clock advance per editor frame, milliseconds (60 Hz).
pub const FRAME_MS: u64 = 16;
/// Most frames one `editor.pie_net_run` step advances.
pub const MAX_STEP_FRAMES: u64 = 100_000;

/// How PIE hosts the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PieNetMode {
    /// No network session: play as before.
    #[default]
    Offline,
    /// The editor process is the server and a player; `clients` more join in memory.
    Listen,
    /// The editor process is a dedicated server; `clients` join in memory.
    Dedicated,
}

impl PieNetMode {
    /// Stable name for the protocol and the panel.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Listen => "listen",
            Self::Dedicated => "dedicated",
        }
    }

    /// Parses a name from [`PieNetMode::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "offline" => Some(Self::Offline),
            "listen" => Some(Self::Listen),
            "dedicated" => Some(Self::Dedicated),
            _ => None,
        }
    }
}

/// The PIE network configuration and the session it started.
#[derive(Resource, Default)]
pub struct PieNet {
    /// Configured mode (applied when play starts).
    pub mode: PieNetMode,
    /// Configured client count, `0..=MAX_PIE_CLIENTS`.
    pub clients: u32,
    /// Configured conditioner preset name (empty = `off`).
    pub preset: String,
    /// Conditioner seed.
    pub seed: u64,
    /// Per-client conditioner preset. `None` follows [`Self::preset`] (ADR-3996).
    /// In memory only: not written into the project.
    pub(crate) client_presets: Vec<Option<String>>,
    /// The panel shows the overlay lines under the table.
    pub overlay: bool,
    /// Frames the running session has stepped.
    pub frames: u64,
    /// Run the server and clients as separate processes (`crate::pie_process`, ADR-2682).
    pub separate_processes: bool,
    /// Separate-process clients get a window ("PIE client k") instead of running offscreen.
    pub windowed_clients: bool,
    session: Option<NetSession>,
    /// The session runs on the wall clock (a UDP listen host for separate processes).
    wall_clock: bool,
}

/// A refused PIE net request: `code` is the lowercase protocol code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieNetError {
    /// The code (`pie_net_clients_limit`, `preset_unknown`, `pie_net_offline`, `pie_net_not_running`).
    pub code: &'static str,
    /// Human text.
    pub message: String,
}

fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, PieNetError> {
    Err(PieNetError {
        code,
        message: message.into(),
    })
}

impl PieNet {
    /// True while a session runs.
    #[must_use]
    pub fn running(&self) -> bool {
        self.session.is_some()
    }

    /// The running session, for the stats and traffic readers.
    #[must_use]
    pub fn session(&self) -> Option<&NetSession> {
        self.session.as_ref()
    }

    /// Sets the configuration. Refuses a bad preset or too many clients and changes nothing then.
    /// A running session keeps its clients; the preset applies to it at once.
    pub fn configure(
        &mut self,
        mode: Option<PieNetMode>,
        clients: Option<u32>,
        preset: Option<&str>,
        seed: Option<u64>,
    ) -> Result<(), PieNetError> {
        if let Some(n) = clients
            && n > MAX_PIE_CLIENTS
        {
            return refuse(
                "pie_net_clients_limit",
                format!("at most {MAX_PIE_CLIENTS} clients per play session, not {n}"),
            );
        }
        if let Some(name) = preset
            && !name.is_empty()
            && SessionConditioner::preset(name).is_none()
        {
            return refuse("preset_unknown", format!("unknown conditioner preset `{name}`"));
        }
        if let Some(m) = mode {
            self.mode = m;
        }
        if let Some(n) = clients {
            self.clients = n;
            crate::pie_client_conditioner::resize_slots(&mut self.client_presets, n);
        }
        if let Some(name) = preset {
            self.preset = name.to_owned();
        }
        if let Some(s) = seed {
            self.seed = s;
        }
        let conditioner = self.conditioner();
        if let Some(session) = self.session.as_mut() {
            session.set_conditioner(conditioner);
        }
        Ok(())
    }

    /// The session conditioner: the configured preset (empty means `off`) and the seed.
    pub(crate) fn conditioner(&self) -> SessionConditioner {
        let name = if self.preset.is_empty() {
            "off"
        } else {
            &self.preset
        };
        let mut c = SessionConditioner::preset(name).unwrap_or_default();
        c.seed = self.seed;
        c
    }

    /// Starts the configured session: hosts without a socket and joins the clients. Replaces a
    /// running session. Refuses `offline`.
    pub fn start(&mut self) -> Result<(), PieNetError> {
        let dedicated = match self.mode {
            PieNetMode::Offline => {
                return refuse(
                    "pie_net_offline",
                    "the mode is offline; set listen or dedicated first",
                );
            }
            PieNetMode::Listen => false,
            PieNetMode::Dedicated => true,
        };
        self.stop();
        crate::pie_client_conditioner::resize_slots(&mut self.client_presets, self.clients);
        let mut session = NetSession::default();
        session.set_manual_clock(0);
        session.set_conditioner(self.conditioner());
        if let Err(e) = session.host_local(HostOptions::default(), dedicated) {
            return refuse("pie_net_host_failed", e.to_string());
        }
        if let Err(e) = crate::pie_client_conditioner::add_clients(&mut session, self) {
            return refuse(e.code, e.message);
        }
        self.frames = 0;
        self.wall_clock = false;
        self.session = Some(session);
        Ok(())
    }

    /// Hosts a listen session on an ephemeral UDP port on loopback, on the wall clock, with no
    /// in-memory clients: separate client processes join it (ADR-2682). Replaces a running session.
    pub fn start_udp_listen(&mut self) -> Result<std::net::SocketAddr, PieNetError> {
        self.stop();
        let mut session = NetSession::default();
        session.set_conditioner(self.conditioner());
        let bind = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        if let Err(e) = session.host(bind, HostOptions::default(), false) {
            return refuse("pie_net_host_failed", e.to_string());
        }
        let Some(addr) = session.local_addr() else {
            return refuse("pie_net_host_failed", "the listen session bound no port");
        };
        self.frames = 0;
        self.wall_clock = true;
        self.session = Some(session);
        Ok(addr)
    }

    /// Ends the session. Idempotent.
    pub fn stop(&mut self) {
        self.session = None;
    }

    /// Advances the running session by `frames` frames of [`FRAME_MS`] each.
    pub fn step(&mut self, frames: u64) -> Result<(), PieNetError> {
        let _span = tracing::trace_span!("pie_net_step", frames).entered();
        let Some(session) = self.session.as_mut() else {
            return refuse("pie_net_not_running", "no PIE net session is running");
        };
        for _ in 0..frames.min(MAX_STEP_FRAMES) {
            if !self.wall_clock {
                session.advance_manual_clock(FRAME_MS);
            }
            session.update();
            self.frames += 1;
        }
        Ok(())
    }

    /// Clients that reached the game.
    #[must_use]
    pub fn in_game(&self) -> usize {
        self.session
            .as_ref()
            .and_then(NetSession::simulated)
            .map_or(0, |s| s.clients().iter().filter(|c| c.in_game()).count())
    }

    /// The overlay and table text: header, one line per connection, then the bandwidth lines.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let Some(session) = self.session.as_ref() else {
            return vec!["no PIE net session".to_owned()];
        };
        let mut out = vec![format!(
            "{} server, {} client(s) ({} in game), frame {}",
            self.mode.as_str(),
            self.clients,
            self.in_game(),
            self.frames
        )];
        out.extend(session.connection_stats().iter().map(|r| r.line()));
        out.extend(session.bandwidth_view().lines());
        out
    }
}

/// Editor-loop hook, once per frame: steps the session by one frame unless play is paused.
pub fn step_frame(world: &mut World, paused: bool) {
    if paused {
        return;
    }
    if let Some(mut net) = world.get_resource_mut::<PieNet>()
        && net.running()
    {
        // Only fails when no session runs, which was just checked.
        let _ = net.step(1);
    }
}

/// Editor-loop hook after the PIE requests: starts the configured session when play begins and
/// ends it when play ends.
pub fn sync_with_play(world: &mut World, playing: bool) {
    // Separate processes (ADR-2682) start, stop and pump on their own; a listen session they host
    // is this resource's session and ends below with play.
    if world
        .get_resource::<PieNet>()
        .is_some_and(|n| n.separate_processes)
        || world.contains_resource::<crate::pie_process::PieProcesses>()
    {
        crate::pie_process::sync_with_play(world, playing);
    }
    // `separate_processes` may have been turned off while a separate session runs: never start an
    // in-process session beside it (review rc1-rev-editor-k finding 11).
    let processes_active = world
        .get_resource::<crate::pie_process::PieProcesses>()
        .is_some_and(crate::pie_process::PieProcesses::active);
    let Some(mut net) = world.get_resource_mut::<PieNet>() else {
        return;
    };
    if playing
        && !net.running()
        && net.mode != PieNetMode::Offline
        && !net.separate_processes
        && !processes_active
    {
        if let Err(e) = net.start() {
            tracing::warn!(target: "regia_editor", "PIE net session did not start: {}", e.message);
        }
    } else if !playing && net.running() {
        net.stop();
    }
}

/// Draws the Net panel: configuration, start/stop, the connection table and the bandwidth rows.
pub fn net_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut net = world.remove_resource::<PieNet>().unwrap_or_default();
    let action = draw(ui, &mut net);
    world.insert_resource(net);
    match action {
        PanelAction::None => {}
        PanelAction::StartProcesses => {
            // Kept for the processes table below.
            let _ = crate::pie_process::start(world);
        }
        PanelAction::StopProcesses => {
            crate::pie_process::stop(world);
            world.resource_mut::<PieNet>().stop();
        }
    }
    crate::pie_process::processes_panel(ui, world);
}

/// What the Net tab's buttons asked for that needs the whole world.
enum PanelAction {
    None,
    StartProcesses,
    StopProcesses,
}

/// [`net_panel`] plus the client worlds: a client picker, the input-focus toggle and the picked
/// client's view, rendered on `device` ([`crate::pie_worlds::clients_panel`]).
pub fn net_panel_with(ui: &mut egui::Ui, world: &mut World, device: Option<&regia_render::RenderDevice>) {
    egui::ScrollArea::vertical()
        .id_salt("pie-net-scroll")
        .show(ui, |ui| {
            net_panel(ui, world);
            crate::pie_worlds::clients_panel(ui, world, device);
        });
}

fn draw(ui: &mut egui::Ui, net: &mut PieNet) -> PanelAction {
    let mut action = PanelAction::None;
    let mut mode = net.mode;
    let mut clients = net.clients;
    let mut preset = if net.preset.is_empty() {
        "off".to_owned()
    } else {
        net.preset.clone()
    };
    ui.horizontal(|ui| {
        egui::ComboBox::from_label("Mode")
            .selected_text(mode.as_str())
            .show_ui(ui, |ui| {
                for m in [PieNetMode::Offline, PieNetMode::Listen, PieNetMode::Dedicated] {
                    ui.selectable_value(&mut mode, m, m.as_str());
                }
            });
        ui.add(
            egui::DragValue::new(&mut clients)
                .range(0..=MAX_PIE_CLIENTS)
                .prefix("clients "),
        );
        egui::ComboBox::from_label("Conditions")
            .selected_text(preset.clone())
            .show_ui(ui, |ui| {
                for p in SESSION_CONDITIONER_PRESETS {
                    ui.selectable_value(&mut preset, (*p).to_owned(), *p);
                }
            });
    });
    // The widgets only offer valid values, so the configuration cannot be refused here.
    let _ = net.configure(Some(mode), Some(clients), Some(&preset), None);
    ui.horizontal(|ui| {
        ui.checkbox(&mut net.separate_processes, "Separate processes")
            .on_hover_text("Run the server and each client as its own process (regia server --clients n)");
        ui.add_enabled(
            net.separate_processes,
            egui::Checkbox::new(&mut net.windowed_clients, "Windowed clients"),
        );
    });
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!net.running(), egui::Button::new("Start"))
            .clicked()
        {
            if net.separate_processes {
                action = PanelAction::StartProcesses;
            } else if let Err(e) = net.start() {
                ui.label(e.message);
            }
        }
        if ui.button("Stop").clicked() {
            if net.separate_processes {
                action = PanelAction::StopProcesses;
            } else {
                net.stop();
            }
        }
        ui.checkbox(&mut net.overlay, "Overlay lines");
    });
    ui.separator();
    let Some(session) = net.session() else {
        if !net.separate_processes {
            ui.label("No PIE net session. Pick a mode and press Start, or Play with a mode set.");
        }
        return action;
    };
    egui::Grid::new("pie-net-stats").striped(true).show(ui, |ui| {
        for h in [
            "Client",
            "State",
            "RTT ms",
            "Jitter",
            "Loss out %",
            "Loss in %",
            "Out B/s",
            "In B/s",
            "Pkt/s",
        ] {
            ui.strong(h);
        }
        ui.end_row();
        for r in session.connection_stats() {
            ui.label(&r.label);
            ui.label(&r.state);
            ui.label(format!("{:.0}", r.rtt_ms));
            ui.label(format!("{:.0}", r.jitter_ms));
            ui.label(format!("{:.1}", r.loss_out * 100.0));
            ui.label(format!("{:.1}", r.loss_in * 100.0));
            ui.label(format!("{:.0}", r.bytes_out_per_s));
            ui.label(format!("{:.0}", r.bytes_in_per_s));
            ui.label(format!("{:.0}", r.packets_per_s));
            ui.end_row();
        }
    });
    ui.separator();
    ui.strong("Bandwidth by message kind");
    egui::Grid::new("pie-net-traffic").striped(true).show(ui, |ui| {
        for h in ["Kind", "Out B", "In B", "Share %"] {
            ui.strong(h);
        }
        ui.end_row();
        for k in session.bandwidth_view().kinds {
            ui.label(&k.kind);
            ui.label(k.bytes_out.to_string());
            ui.label(k.bytes_in.to_string());
            ui.label(format!("{:.1}", k.share * 100.0));
            ui.end_row();
        }
    });
    if net.overlay {
        ui.separator();
        for line in net.lines() {
            ui.monospace(line);
        }
    }
    action
}

fn rpc(e: PieNetError) -> RpcError {
    let hint = match e.code {
        "pie_net_clients_limit" => "use at most 16 clients; net.sim_clients scales further for load tests",
        "preset_unknown" => "use off, lan, good, average, bad or terrible",
        "pie_net_offline" => "editor.pie_net_config { mode: \"listen\" } first",
        "pie_net_not_running" => "editor.pie_net_run { action: \"start\" } first",
        _ => "see book/agents/pie-net.md",
    };
    RpcError::new(INVALID_PARAMS, e.code, e.message, hint)
}

fn net_mut(world: &mut World) -> Mut<'_, PieNet> {
    world.get_resource_or_insert_with::<PieNet>(Default::default)
}

fn config_json(net: &PieNet) -> Value {
    json!({
        "mode": net.mode.as_str(),
        "clients": net.clients,
        "preset": if net.preset.is_empty() { "off" } else { net.preset.as_str() },
        "seed": net.seed,
        "overlay": net.overlay,
        "running": net.running(),
        "separate_processes": net.separate_processes,
        "windowed": net.windowed_clients,
    })
}

/// `editor.pie_net_config`.
fn pie_net_config(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = match params.get("mode") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(PieNetMode::parse)
                .ok_or_else(|| RpcError::invalid_params("`mode` must be offline, listen or dedicated"))?,
        ),
    };
    let clients = match params.get("clients") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
                .ok_or_else(|| RpcError::invalid_params("`clients` must be a whole number"))?,
        ),
    };
    let preset = params.get("preset").and_then(Value::as_str);
    let seed = params.get("seed").and_then(Value::as_u64);
    let mut net = net_mut(world);
    net.configure(mode, clients, preset, seed).map_err(rpc)?;
    if let Some(on) = params.get("overlay").and_then(Value::as_bool) {
        net.overlay = on;
    }
    if let Some(on) = params.get("separate_processes").and_then(Value::as_bool) {
        net.separate_processes = on;
    }
    if let Some(on) = params.get("windowed").and_then(Value::as_bool) {
        net.windowed_clients = on;
    }
    Ok(config_json(&net))
}

/// `editor.pie_net_run`.
fn pie_net_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`action` is required: start, stop or step"))?;
    if let Some(on) = params.get("separate_processes").and_then(Value::as_bool) {
        net_mut(world).separate_processes = on;
    }
    let separate = net_mut(world).separate_processes;
    if separate && action == "start" {
        crate::pie_process::start(world).map_err(|e| crate::pie_process::rpc(&e))?;
    }
    if action == "stop" {
        crate::pie_process::stop(world);
    }
    let mut net = net_mut(world);
    match action {
        "start" if separate => {}
        "start" => net.start().map_err(rpc)?,
        "stop" => net.stop(),
        "step" => {
            let frames = params.get("frames").and_then(Value::as_u64).unwrap_or(1);
            net.step(frames).map_err(rpc)?;
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "`action` must be start, stop or step, not `{other}`"
            )));
        }
    }
    let out = json!({
        "running": net.running(),
        "frames": net.frames,
        "in_game": net.in_game(),
        "separate_processes": separate,
    });
    Ok(out)
}

/// `editor.pie_net_state`.
fn pie_net_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let _ = net_mut(world);
    let Some(net) = world.get_resource::<PieNet>() else {
        return Ok(Value::Null);
    };
    let mut out = json!({
        "config": config_json(net),
        "frames": net.frames,
        "in_game": net.in_game(),
        "lines": net.lines(),
    });
    if let (Some(session), Value::Object(map)) = (net.session(), &mut out) {
        map.insert("stats".into(), regia_agent::net_stats_value(session));
        map.insert("traffic".into(), regia_agent::net_traffic_value(session));
        map.insert("dedicated".into(), json!(session.is_dedicated()));
    }
    // Separate processes: the peers in game and the session state (`editor.pie_processes` has
    // the per-process detail).
    if let (Some(procs), Value::Object(map)) =
        (world.get_resource::<crate::pie_process::PieProcesses>(), &mut out)
    {
        map.insert("processes_state".into(), json!(procs.state().as_str()));
        map.insert("peers".into(), json!(procs.peers(net.session())));
    }
    Ok(out)
}

/// The `editor.pie_net_*` methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.pie_net_config",
                tool: "pie_net_config",
                description: "Set the play-in-editor network configuration: mode (offline, listen, dedicated), the number of in-memory clients (0..=16), the conditioner preset and seed. Applied when play starts or at editor.pie_net_run start.",
                params: &[
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "offline, listen or dedicated.",
                    },
                    ParamSchema {
                        name: "clients",
                        kind: "integer",
                        required: false,
                        description: "Clients to join, 0..=16.",
                    },
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: false,
                        description: "off, lan, good, average, bad or terrible.",
                    },
                    ParamSchema {
                        name: "seed",
                        kind: "integer",
                        required: false,
                        description: "Conditioner seed.",
                    },
                    ParamSchema {
                        name: "overlay",
                        kind: "boolean",
                        required: false,
                        description: "Show the overlay lines in the Net panel.",
                    },
                    ParamSchema {
                        name: "separate_processes",
                        kind: "boolean",
                        required: false,
                        description: "Run the server and clients as separate processes (the project's regia server binaries) instead of in memory.",
                    },
                    ParamSchema {
                        name: "windowed",
                        kind: "boolean",
                        required: false,
                        description: "Separate-process clients open a window titled \"PIE client k\" instead of running offscreen.",
                    },
                ],
                result: "{ mode, clients, preset, seed, overlay, running, separate_processes, windowed }",
                mutating: true,
                permission: Permission::Play,
            },
            pie_net_config as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pie_net_run",
                tool: "pie_net_run",
                description: "Start, stop or step the play-in-editor network session: one process, the configured clients joined to a listen or dedicated server over the in-memory transport. Step advances the session clock by 16 ms per frame, deterministically.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "start, stop or step.",
                    },
                    ParamSchema {
                        name: "frames",
                        kind: "integer",
                        required: false,
                        description: "Frames to step (default 1, at most 100000).",
                    },
                    ParamSchema {
                        name: "separate_processes",
                        kind: "boolean",
                        required: false,
                        description: "Set the separate-processes option before the action; start then launches the server and client processes (read them with editor.pie_processes).",
                    },
                ],
                result: "{ running, frames, in_game, separate_processes }",
                mutating: true,
                permission: Permission::Play,
            },
            pie_net_run as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pie_net_state",
                tool: "pie_net_state",
                description: "Read the play-in-editor network session: the configuration, clients in game, the stats overlay rows (rtt, jitter, loss, bytes per second per client) and the bandwidth view (bytes per message kind).",
                params: &[],
                result: "{ config, frames, in_game, lines, stats?, traffic?, dedicated?, processes_state?, peers? }",
                mutating: false,
                permission: Permission::Read,
            },
            pie_net_state as Handler,
        ),
    ]
    .into_iter()
    // One conditioner preset per client (`editor.pie_net_client_conditioner`, ADR-3996).
    .chain(crate::pie_client_conditioner::methods())
    // One world and one view per client (`editor.pie_net_clients`, `_focus`, `_key`; ADR-2623).
    .chain(crate::pie_worlds::methods())
    // Separate processes (`editor.pie_processes`; ADR-2682).
    .chain(crate::pie_process::methods())
    // Play time scale and mute (`editor.play_controls`; ADR-7829).
    .chain(crate::pie_controls::methods())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(
        world: &mut World,
        f: fn(&mut World, &Value) -> Result<Value, RpcError>,
        p: Value,
    ) -> Result<Value, RpcError> {
        f(world, &p)
    }

    fn code(r: Result<Value, RpcError>) -> String {
        r.expect_err("refused")
            .data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    #[test]
    fn modes_round_trip() {
        for m in [PieNetMode::Offline, PieNetMode::Listen, PieNetMode::Dedicated] {
            assert_eq!(PieNetMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(PieNetMode::parse("host"), None);
    }

    #[test]
    fn configuration_is_validated_before_anything_changes() {
        let mut net = PieNet::default();
        let e = net
            .configure(Some(PieNetMode::Listen), Some(MAX_PIE_CLIENTS + 1), None, None)
            .expect_err("limit");
        assert_eq!(e.code, "pie_net_clients_limit");
        let e = net
            .configure(Some(PieNetMode::Listen), Some(2), Some("nope"), None)
            .expect_err("preset");
        assert_eq!(e.code, "preset_unknown");
        assert_eq!(net.mode, PieNetMode::Offline, "a refused call changes nothing");
        assert_eq!(net.clients, 0);
        assert_eq!(net.start().expect_err("offline").code, "pie_net_offline");
        assert_eq!(net.step(1).expect_err("idle").code, "pie_net_not_running");
    }

    #[test]
    fn clients_join_in_memory_and_report_stats() {
        let mut net = PieNet::default();
        net.configure(Some(PieNetMode::Listen), Some(3), Some("good"), Some(5))
            .expect("config");
        net.start().expect("start");
        net.step(300).expect("step");
        assert_eq!(net.in_game(), 3, "every client joins");
        let session = net.session().expect("session");
        let rows = session.connection_stats();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].label, "client-1");
        assert!(rows.iter().all(|r| r.state == "in_game" && !r.simulated));
        // The `good` preset adds latency in both directions, so the round trip is real.
        assert!(rows.iter().all(|r| r.rtt_ms >= 20.0), "rtt {:?}", rows[0].rtt_ms);
        let view = session.bandwidth_view();
        assert!(view.total_in > 0 && view.total_out > 0);
        for kind in ["welcome", "in_game", "level_ready"] {
            assert!(
                view.kinds.iter().any(|k| k.kind == kind),
                "{kind} in {:?}",
                view.kinds
            );
        }
        let share: f64 = view.kinds.iter().map(|k| k.share).sum();
        assert!((share - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_same_configuration_gives_the_same_numbers() {
        let run = || {
            let mut net = PieNet::default();
            net.configure(Some(PieNetMode::Dedicated), Some(2), Some("bad"), Some(11))
                .expect("config");
            net.start().expect("start");
            net.step(400).expect("step");
            let s = net.session().expect("session");
            (s.connection_stats(), s.bandwidth_view())
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn stop_ends_the_session_and_play_sync_follows_play() {
        let mut world = World::new();
        world.insert_resource(PieNet::default());
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(1), None, None)
            .expect("config");
        sync_with_play(&mut world, true);
        assert!(world.resource::<PieNet>().running());
        step_frame(&mut world, true);
        assert_eq!(
            world.resource::<PieNet>().frames,
            0,
            "paused play does not step the net"
        );
        step_frame(&mut world, false);
        assert_eq!(world.resource::<PieNet>().frames, 1);
        sync_with_play(&mut world, false);
        assert!(!world.resource::<PieNet>().running());
    }

    #[test]
    fn agent_methods_drive_the_session() {
        let mut world = World::new();
        assert_eq!(
            code(call(&mut world, pie_net_run, json!({ "action": "step" }))),
            "pie_net_not_running"
        );
        assert_eq!(
            code(call(&mut world, pie_net_config, json!({ "clients": 99 }))),
            "pie_net_clients_limit"
        );
        assert_eq!(
            code(call(&mut world, pie_net_config, json!({ "preset": "x" }))),
            "preset_unknown"
        );
        assert_eq!(
            code(call(&mut world, pie_net_run, json!({ "action": "start" }))),
            "pie_net_offline"
        );
        call(
            &mut world,
            pie_net_config,
            json!({ "mode": "listen", "clients": 2, "preset": "lan", "seed": 3 }),
        )
        .expect("config");
        call(&mut world, pie_net_run, json!({ "action": "start" })).expect("start");
        let r = call(
            &mut world,
            pie_net_run,
            json!({ "action": "step", "frames": 200 }),
        )
        .expect("step");
        assert_eq!(r["in_game"], 2);
        let s = call(&mut world, pie_net_state, json!({})).expect("state");
        assert_eq!(s["stats"]["connections"].as_array().map(Vec::len), Some(2));
        assert!(s["traffic"]["kinds"].as_array().is_some_and(|k| !k.is_empty()));
        assert_eq!(s["dedicated"], false);
        call(&mut world, pie_net_run, json!({ "action": "stop" })).expect("stop");
        assert_eq!(
            call(&mut world, pie_net_state, json!({})).expect("state")["config"]["running"],
            false
        );
    }

    #[test]
    fn the_net_panel_draws_headless() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        {
            let mut net = PieNet::default();
            net.configure(Some(PieNetMode::Listen), Some(2), Some("average"), Some(1))
                .expect("config");
            net.overlay = true;
            net.start().expect("start");
            net.step(120).expect("step");
            world.insert_resource(net);
        }
        let mut shapes = 0;
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| net_panel(ui, &mut world));
            });
            out.textures_delta.clear();
            shapes += out.shapes.len();
        }
        assert!(shapes > 0, "the panel painted something");
        assert!(
            world.resource::<PieNet>().running(),
            "drawing does not stop the session"
        );
    }
}
