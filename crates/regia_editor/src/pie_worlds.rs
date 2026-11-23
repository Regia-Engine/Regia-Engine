// SPDX-License-Identifier: MPL-2.0
//! One ECS world and one view per play-in-editor client (`docs/specs/22` T2.3 "PIE with N clients",
//! `docs/specs/77` § 10.2, `book/agents/pie-net.md`, ADR-2623).
//!
//! [`crate::pie_net`] joins N session-level clients to an in-memory [`regia_net::session::NetSession`]
//! (handshake, streams, stats). This module gives each of those clients a **world**: when play
//! starts with the Net tab set to `listen` or `dedicated`, `clients >= 1` and a game crate open,
//! [`pump`] makes the play world the replication server and builds one client [`App`] per client.
//!
//! - **Server.** The play world keeps its game; [`pump`] adds `NetPlugin` and `NetLinkPlugin` when
//!   the game did not, sets [`NetMode`] to `ListenServer` or `DedicatedServer`, and hosts its
//!   [`NetLink`] on an in-memory [`LoopbackNet`] at [`PIE_SERVER_ADDR`].
//! - **Clients.** Each client app gets the editor's minimal plugin set, the scene plugin and the
//!   game's own `register` export run with [`NetMode::Client`] already inserted, so a game picks its
//!   client side (replication client, prediction, interpolation) the same way it does in a client
//!   build. The level the editor has open is spawned into it without its `Replicated` entities
//!   (those arrive from the server), and its link connects to the server over the same loopback
//!   network. Client state is real replicated state.
//! - **Clock.** Every world takes one fixed step per editor frame: the play world and the clients
//!   run `enable_fixed_step` at the game's tick rate and the links run a manual clock of one step
//!   per frame, as the session clock of [`crate::pie_net`] does. A client is stepped as many frames
//!   as the play world was: none while paused, and for a step of `n` ([`step_play`]) `n` turns of the
//!   play world and then the clients, so the links talk through a long step as they do through `n`
//!   frames. [`pump`] runs before the play world's frame, so the play world is on the manual step
//!   from its first editor frame and a run with the same configuration and the same inputs repeats
//!   exactly.
//! - **Configuration.** The Net tab's mode and client count apply when play starts. A session keeps
//!   its client worlds, or its lack of them, until it ends.
//! - **Input.** Keys go to the focused client only ([`PieWorlds::set_focus`]), as
//!   `regia_platform::KeyboardInput` messages, which `regia_input`'s keyboard reads. A key held
//!   when the focus moves is released, and so is a key pressed over the view when the pointer
//!   leaves it, the window loses focus or the view stops being drawn.
//! - **Views.** Each client has a [`ViewportInstance`] with its own renderer (the viewport-instance
//!   path), drawn in the Net tab, captured by `editor.capture_viewport { client }`.
//!
//! The worlds live in the edit world as a non-send resource ([`App`] is not `Send`); the editor
//! loop's thread inserts and reads it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Instant;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkOptions, NetLink, NetLinkPlugin};
use regia_net::runtime::NetNode;
use regia_net::{CryptoMode, LoopbackNet, NetGame, NetId, NetMode, NetOwnedLocally, NetPlugin, Replicated};
use regia_scene::prelude::{SpawnOptions, snapshot_scene, spawn_scene};
use regia_scene::spawn::{SnapshotFilter, UnknownTypePolicyOption};
use serde_json::{Value, json};

use crate::engine::app::{App, MinimalPlugins};
use crate::engine::ecs::prelude::*;
use crate::pie::{CurrentGame, PieSession};
use crate::pie_net::{PieNet, PieNetMode};
use crate::viewport::ViewportPanelState;
use crate::viewport_agent::Handler;
use crate::viewport_instance::ViewportInstance;
use crate::views::ViewKind;

/// The in-memory address the play world's server link binds.
pub const PIE_SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7777);
/// Viewport-instance ids of client views are this plus the client index (far above the ids the
/// viewport-instance registry hands out, so their renderers never collide).
pub const CLIENT_VIEW_ID_BASE: u64 = 0x5049_4543_0000_0000;
/// Default size of `editor.capture_viewport { client }`, pixels.
pub const CLIENT_CAPTURE_SIZE: [u32; 2] = [640, 360];

/// The in-memory address client `index` binds: its own loopback host, port picked by the network.
///
/// Every client of a session plays on one machine, but the server limits peers per address
/// (`ServerConfig::max_peers_per_ip`, 8 by default, not settable through `LinkOptions`) to stop a
/// remote machine from taking every slot. A shared host would refuse the ninth client.
fn client_addr(index: usize) -> SocketAddr {
    let offset = u32::try_from(index).map_or(u32::MAX, |i| i.saturating_add(1));
    let host = u32::from(Ipv4Addr::LOCALHOST).saturating_add(offset);
    SocketAddr::new(IpAddr::V4(Ipv4Addr::from(host)), 0)
}

/// The link identity of PIE: the server and every client link use it, so the handshake matches
/// whatever the game calls itself. Replication rides on the link as `NetFrame`s and checks the
/// game's own protocol hash.
struct PieWire;

impl NetGame for PieWire {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.editor.pie";
    const PROTOCOL_VERSION: u32 = 1;
}

/// One client world.
pub struct PieClientWorld {
    /// 0-based index (`client-1` is index 0).
    pub index: usize,
    app: App,
    /// The client's view: camera, layout, quality; its id names its renderer.
    pub view: ViewportInstance,
    panel: ViewportPanelState,
    /// Frames this world has stepped.
    pub frames: u64,
    /// Wall time of the last step, microseconds (reported, never asserted).
    pub last_update_us: u64,
    /// Wall time of every step so far, microseconds (reported, never asserted).
    pub total_update_us: u64,
    /// A panic in game code that stopped this client.
    pub panic: Option<String>,
    /// The backtrace of [`Self::panic`], when the PIE boundary captured one (ADR-7205, ADR-7340).
    pub panic_backtrace: Option<String>,
}

impl PieClientWorld {
    /// The client's world.
    #[must_use]
    pub fn world(&self) -> &World {
        self.app.world()
    }

    /// The client's world, mutably.
    pub fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }

    /// `client-N`, 1-based like the Net tab's rows.
    #[must_use]
    pub fn label(&self) -> String {
        format!("client-{}", self.index + 1)
    }

    fn step(&mut self) {
        if self.panic.is_some() {
            return;
        }
        // The Profiler's play source is everything under `pie_client_frame` (ADR-9740).
        let _span = tracing::info_span!(target: "regia_editor", "pie_client_frame").entered();
        let _context = regia_core::profiling::flame_context("pie_client_frame");
        let started = Instant::now();
        let result = crate::pie::catch_in_pie(|| self.app.update());
        let us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.last_update_us = us;
        self.total_update_us = self.total_update_us.saturating_add(us);
        self.frames += 1;
        if let Err(caught) = result {
            tracing::error!(target: "regia_editor", "PIE {} panicked: {}", self.label(), caught.log_text());
            self.panic = Some(caught.message);
            self.panic_backtrace = caught.backtrace;
        }
    }

    /// What `editor.pie_net_clients` reports for this world.
    #[must_use]
    pub fn row(&mut self) -> PieClientRow {
        let label = self.label();
        let world = self.app.world_mut();
        let entities = world.iter_entities().count();
        let replicated = world.query_filtered::<(), With<NetId>>().iter(world).count();
        let owned = world
            .query_filtered::<(), With<NetOwnedLocally>>()
            .iter(world)
            .count();
        let tick = world.get_resource::<NetNode>().map_or(0, |n| u64::from(n.tick.0));
        let (connected, peer) = world.get_resource::<NetLink>().map_or((false, None), |l| {
            (l.is_connected(), l.status().and_then(|s| s.peer_id))
        });
        PieClientRow {
            index: self.index,
            label,
            connected,
            peer,
            entities,
            replicated,
            owned,
            last_applied_tick: tick,
            frames: self.frames,
            last_update_us: self.last_update_us,
            mean_update_us: self.total_update_us.checked_div(self.frames).unwrap_or(0),
            panic: self.panic.clone(),
        }
    }
}

/// One row of `editor.pie_net_clients`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PieClientRow {
    /// 0-based index.
    pub index: usize,
    /// `client-N`.
    pub label: String,
    /// The link is connected to the play world's server.
    pub connected: bool,
    /// The peer id the server gave this client.
    pub peer: Option<u32>,
    /// Entities in the world (resource entities included).
    pub entities: usize,
    /// Entities with a `NetId` (replicated from the server).
    pub replicated: usize,
    /// Entities this client owns (predicted locally).
    pub owned: usize,
    /// The newest server tick this client applied.
    pub last_applied_tick: u64,
    /// Frames stepped.
    pub frames: u64,
    /// Wall time of the last step, microseconds.
    pub last_update_us: u64,
    /// Mean wall time of a step, microseconds.
    pub mean_update_us: u64,
    /// The panic that stopped this client, if one did.
    pub panic: Option<String>,
}

/// The client worlds of the running play session. A non-send resource of the edit world.
pub struct PieWorlds {
    session: Instant,
    clients: Vec<PieClientWorld>,
    /// The client keys go to (`None`: no client receives input).
    focus: Option<usize>,
    /// Keys down in the focused client, released when the focus moves (a key is never left held in
    /// a world nothing can release it from).
    held: Vec<HeldKey>,
    /// The client the Net tab shows.
    pub picked: usize,
    /// The Net tab drew a client view since the last [`pump`]. Only a drawn view can see the release
    /// of a key pressed over it.
    view_drawn: bool,
    server_frames: u64,
    hz: f64,
    game: CurrentGame,
    _net: LoopbackNet,
}

impl PieWorlds {
    /// The client worlds, in index order.
    #[must_use]
    pub fn clients(&self) -> &[PieClientWorld] {
        &self.clients
    }

    /// One client world.
    pub fn client_mut(&mut self, index: usize) -> Option<&mut PieClientWorld> {
        self.clients.get_mut(index)
    }

    /// The fixed-step rate every world runs at.
    #[must_use]
    pub fn tick_hz(&self) -> f64 {
        self.hz
    }

    /// The client keys go to.
    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// Moves the input focus. Every key held in the client losing it is released first.
    pub fn set_focus(&mut self, focus: Option<usize>) {
        if focus != self.focus {
            self.release_held(|_| true);
        }
        self.focus = focus;
    }

    /// Sends one key to the focused client as a `regia_platform::KeyboardInput` message.
    /// `key` is a physical key name (`KeyW`, `ArrowUp`, `Space`).
    ///
    /// # Errors
    ///
    /// `pie_net_no_focus` when no client has the focus.
    pub fn send_key(&mut self, key: &str, down: bool) -> Result<usize, PieWorldsError> {
        self.key_message(key, down, false)
    }

    /// Releases the held keys `which` selects.
    fn release_held(&mut self, which: impl Fn(&HeldKey) -> bool) {
        let released: Vec<String> = self
            .held
            .iter()
            .filter(|h| which(h))
            .map(|h| h.key.clone())
            .collect();
        for key in released {
            // A held key implies a focused client, so this cannot be refused.
            let _ = self.key_message(&key, false, false);
        }
    }

    fn key_message(&mut self, key: &str, down: bool, from_view: bool) -> Result<usize, PieWorldsError> {
        let Some(index) = self.focus.filter(|i| *i < self.clients.len()) else {
            return Err(PieWorldsError::new(
                "pie_net_no_focus",
                "no client has the input focus; set it with editor.pie_net_focus",
            ));
        };
        self.held.retain(|h| h.key != key);
        if down {
            self.held.push(HeldKey {
                key: key.to_owned(),
                from_view,
            });
        }
        let world = self.clients[index].world_mut();
        world.write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: key.to_owned(),
            logical_key: key.to_owned(),
            state: if down {
                regia_platform::ButtonState::Pressed
            } else {
                regia_platform::ButtonState::Released
            },
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        Ok(index)
    }

    /// Steps every client as many frames as the play world ran since the last call.
    fn advance(&mut self, session: &PieSession) {
        let behind = session.frames.saturating_sub(self.server_frames);
        self.server_frames = session.frames;
        for _ in 0..behind {
            for client in &mut self.clients {
                client.step();
            }
        }
    }

    fn teardown(mut self) {
        unload_clients(&self.game, &mut self.clients);
        for client in &self.clients {
            crate::render::release_instance_renderers(client.view.id);
        }
        tracing::info!(target: "regia_editor", clients = self.clients.len(), "PIE client worlds ended");
    }
}

/// Runs the game's `on_unload` in one client's app; a panic there is logged (with its
/// backtrace: the boundary is the same `pie::catch_in_pie` the play world uses).
fn unload_client(game: &CurrentGame, app: &mut App, label: &str) {
    let on_unload = game.exports.on_unload;
    if let Err(caught) = crate::pie::catch_in_pie(|| on_unload(app.world_mut())) {
        tracing::error!(
            target: "regia_editor",
            "game on_unload panicked in PIE {label}: {}",
            caught.log_text()
        );
    }
}

fn unload_clients(game: &CurrentGame, clients: &mut [PieClientWorld]) {
    for client in clients {
        let label = client.label();
        unload_client(game, &mut client.app, &label);
    }
}

/// A key down in the focused client.
#[derive(Debug, Clone)]
struct HeldKey {
    key: String,
    /// Pressed through the client view, so it is released when the pointer leaves the view. Keys
    /// from `editor.pie_net_key` stay down until the agent releases them or the focus moves.
    from_view: bool,
}

/// Why there are no client worlds, or why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieWorldsError {
    /// Lowercase protocol code.
    pub code: &'static str,
    /// Human text.
    pub message: String,
}

impl PieWorldsError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Why the session has no client worlds, readable by panels and agents (`Send`).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct PieWorldsStatus {
    /// `None` while client worlds run; else the reason code (`not_playing`, `offline`,
    /// `no_clients`, `separate_processes`, `pie_net_no_game`, `pie_net_link_busy`,
    /// `pie_net_host_failed`).
    pub reason: Option<String>,
    /// Human text for the reason.
    pub message: String,
    /// Client worlds running.
    pub clients: usize,
}

/// Editor-loop hook, once per frame after the play requests and **before** the play world's frame:
/// builds the client worlds when play starts with a network mode (so the play world runs on the
/// manual step from its first editor frame), steps them as many frames as the play world stepped
/// since the last call, and ends them with play. Both editor loops (windowed and headless) call it.
#[tracing::instrument(target = "regia_editor", skip_all, name = "pie_worlds_pump")]
pub fn pump(world: &mut World, mut pie: Option<&mut PieSession>) {
    // Both editor loops call this hook, including headless and minimized-window play.
    crate::debug_colors::sync_play(world, pie.as_deref_mut().map(|session| session.app.world_mut()));
    let Some(session) = pie else {
        end(world, "not_playing", "play is stopped");
        return;
    };
    let current = world
        .get_non_send::<PieWorlds>()
        .map(|w| w.session == session.started);
    if current == Some(false) {
        end(world, "not_playing", "play restarted");
    }
    if current != Some(true) {
        start_session(world, session);
    }
    let Some(mut worlds) = world.remove_non_send::<PieWorlds>() else {
        return;
    };
    if !std::mem::take(&mut worlds.view_drawn) {
        worlds.release_held(|h| h.from_view);
    }
    worlds.advance(session);
    world.insert_non_send(worlds);
}

/// A paused step of `frames` (`editor.step`, the toolbar): one turn of the play world and then the
/// clients per frame, so inputs and link traffic pass as they do through single frames and a step
/// longer than the links' silence timeout drops nobody. Without client worlds this is
/// [`PieSession::step`]. Returns the panic message when the play world panicked.
pub fn step_play(world: &mut World, session: &mut PieSession, frames: u32) -> Option<String> {
    let Some(mut worlds) = world.remove_non_send::<PieWorlds>() else {
        return session.step(frames);
    };
    let mut panic = None;
    if worlds.session == session.started {
        for _ in 0..frames.max(1) {
            panic = session.step(1);
            if panic.is_some() {
                break;
            }
            worlds.advance(session);
        }
    } else {
        panic = session.step(frames);
    }
    world.insert_non_send(worlds);
    panic
}

/// Ends the client worlds before play restarts, so nothing of the old session unloads after the new
/// session's `on_load`.
pub fn end_before_restart(world: &mut World) {
    end(world, "not_playing", "play restarted");
}

/// The session's one attempt to build client worlds, success or not: the Net tab's configuration
/// applies when play starts.
fn start_session(world: &mut World, session: &mut PieSession) {
    let attempted = world
        .get_resource::<PieWorldsAttempt>()
        .is_some_and(|a| a.0 == Some(session.started));
    if attempted {
        return;
    }
    world.insert_resource(PieWorldsAttempt(Some(session.started)));
    let (mode, clients) = world
        .get_resource::<PieNet>()
        .map_or((PieNetMode::Offline, 0), |n| (n.mode, n.clients));
    if world
        .get_resource::<PieNet>()
        .is_some_and(|n| n.separate_processes)
    {
        // The clients are processes with their own windows (`crate::pie_process`, ADR-2682).
        end(
            world,
            "separate_processes",
            "the Net tab runs the clients as separate processes",
        );
        return;
    }
    if mode == PieNetMode::Offline {
        end(
            world,
            "offline",
            "the Net tab's mode was offline when play started",
        );
        return;
    }
    if clients == 0 {
        end(
            world,
            "no_clients",
            "the Net tab's client count was 0 when play started",
        );
        return;
    }
    match install(world, session, mode, clients as usize) {
        Ok(worlds) => {
            world.insert_resource(PieWorldsStatus {
                reason: None,
                message: String::new(),
                clients: worlds.clients.len(),
            });
            world.insert_non_send(worlds);
        }
        Err(e) => {
            tracing::warn!(target: "regia_editor", "PIE client worlds did not start: {}", e.message);
            world.insert_resource(PieWorldsStatus {
                reason: Some(e.code.to_owned()),
                message: e.message,
                clients: 0,
            });
        }
    }
}

/// The play session a build was last attempted for (one attempt per session).
#[derive(Resource, Debug, Clone, Copy, Default)]
struct PieWorldsAttempt(Option<Instant>);

/// Ends the client worlds (idempotent) and records why there are none.
fn end(world: &mut World, reason: &str, message: &str) {
    if let Some(worlds) = world.remove_non_send::<PieWorlds>() {
        worlds.teardown();
    }
    if reason == "not_playing"
        && world
            .get_resource::<PieWorldsAttempt>()
            .is_some_and(|a| a.0.is_some())
    {
        world.insert_resource(PieWorldsAttempt(None));
    }
    let status = PieWorldsStatus {
        reason: Some(reason.to_owned()),
        message: message.to_owned(),
        clients: 0,
    };
    if world.get_resource::<PieWorldsStatus>() != Some(&status) {
        world.insert_resource(status);
    }
}

fn link_options(hz: f64, label: String, name: String) -> LinkOptions {
    let step_us = (1_000_000.0 / hz).round() as u64;
    LinkOptions {
        // In-process: nothing leaves the editor, and a plain link keeps the bytes readable.
        crypto: CryptoMode::Off,
        label,
        name,
        tick_rate_hz: hz.round() as u32,
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: step_us.max(1),
        },
        ..LinkOptions::default()
    }
}

fn install(
    edit: &mut World,
    session: &mut PieSession,
    mode: PieNetMode,
    clients: usize,
) -> Result<PieWorlds, PieWorldsError> {
    let game = session.game.clone().ok_or_else(|| {
        PieWorldsError::new(
            "pie_net_no_game",
            "client worlds run the game's client plugins; open a game crate (clients stay session-level)",
        )
    })?;
    let net_mode = if mode == PieNetMode::Dedicated {
        NetMode::DedicatedServer
    } else {
        NetMode::ListenServer
    };
    let link_running = session
        .app
        .world()
        .get_resource::<NetLink>()
        .is_some_and(|l| l.status().is_some());
    if link_running {
        return Err(PieWorldsError::new(
            "pie_net_link_busy",
            "the play world's link is already running; do not start a NetLink in register",
        ));
    }
    let hz = session
        .app
        .world()
        .get_resource::<regia_app::FixedTime>()
        .map_or(60.0, |f| f.hz);
    let net = LoopbackNet::new();
    let socket = net
        .bind(PIE_SERVER_ADDR)
        .map_err(|e| PieWorldsError::new("pie_net_host_failed", e.to_string()))?;
    let level = level_snapshot(edit);
    let mut built = Vec::with_capacity(clients);
    for index in 0..clients {
        match build_client(edit, &game, &net, level.as_ref(), index, hz) {
            Ok(client) => built.push(client),
            Err(e) => {
                unload_clients(&game, &mut built);
                return Err(e);
            }
        }
    }
    // The play world changes only now: a client that fails to build leaves it as the game made it.
    // A client's first datagrams wait on the bound socket until the server's first frame.
    let server = &mut session.app;
    regia_app::enable_fixed_step(server, hz);
    if !server.has_plugin(std::any::type_name::<NetPlugin>()) {
        server.add_plugins(NetPlugin { mode: net_mode });
    }
    if !server.has_plugin(std::any::type_name::<NetLinkPlugin>()) {
        server.add_plugins(NetLinkPlugin);
    }
    server.insert_resource(net_mode);
    let opts = link_options(hz, "pie-server".to_owned(), "server".to_owned());
    let hosted = server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<PieWire>(Box::new(socket), &opts);
    if let Err(e) = hosted {
        unload_clients(&game, &mut built);
        return Err(PieWorldsError::new(
            "pie_net_host_failed",
            format!("the play world's server link: {e}"),
        ));
    }
    tracing::info!(target: "regia_editor", clients, hz, mode = mode.as_str(), "PIE client worlds started");
    Ok(PieWorlds {
        session: session.started,
        clients: built,
        focus: None,
        held: Vec::new(),
        picked: 0,
        view_drawn: false,
        server_frames: session.frames,
        hz,
        game,
        _net: net,
    })
}

/// The edit world's level as the play world got it (entities and level settings), or `None`.
fn level_snapshot(edit: &mut World) -> Option<regia_scene::prelude::SceneFile> {
    let filter = SnapshotFilter {
        level: None,
        resources: vec![regia_scene::level::LEVEL_SETTINGS_PATH.to_string()],
    };
    match snapshot_scene(edit, &filter) {
        Ok(scene) => Some(scene),
        Err(e) => {
            tracing::warn!(target: "regia_editor", "PIE client worlds start without the level: {e}");
            None
        }
    }
}

fn build_client(
    edit: &World,
    game: &CurrentGame,
    net: &LoopbackNet,
    level: Option<&regia_scene::prelude::SceneFile>,
    index: usize,
    hz: f64,
) -> Result<PieClientWorld, PieWorldsError> {
    let label = format!("client-{}", index + 1);
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_message::<regia_platform::KeyboardInput>();
    // The game reads this in `register` to pick its client side.
    app.insert_resource(NetMode::Client);
    app.insert_resource(crate::render::PlayWorld);
    for copy in [
        copy_resource::<crate::mesh_source::MeshLibrary>,
        copy_resource::<regia_scene::prelude::PrefabLibrary>,
        copy_resource::<crate::agent_ext::EditorProject>,
        copy_resource::<crate::views::ViewportLayout>,
        copy_resource::<crate::viewport::ViewportSettings>,
    ] {
        copy(edit, app.world_mut());
    }
    crate::mesh_source::register_level_types(app.world_mut());
    let owner = crate::engine::ecs::prelude::TypeOwner::Game(game.generation);
    let register = game.exports.register;
    crate::pie::catch_in_pie(|| {
        crate::hot_reload::with_game_owner(&mut app, owner, register);
    })
    .map_err(|caught| {
        tracing::error!(
            target: "regia_editor",
            "game register panicked in PIE {label}: {}",
            caught.log_text()
        );
        PieWorldsError::new(
            "pie_net_host_failed",
            format!("game register panicked in PIE {label}: {}", caught.message),
        )
    })?;
    if !app.has_plugin(std::any::type_name::<NetPlugin>()) {
        app.add_plugins(NetPlugin {
            mode: NetMode::Client,
        });
    }
    if !app.has_plugin(std::any::type_name::<NetLinkPlugin>()) {
        app.add_plugins(NetLinkPlugin);
    }
    app.insert_resource(NetMode::Client);
    regia_app::enable_fixed_step(&mut app, hz);
    if let Some(scene) = level {
        let opts = SpawnOptions {
            level: None,
            persistent: false,
            unknown_types: UnknownTypePolicyOption::Preserve,
        };
        if let Err(e) = spawn_scene(app.world_mut(), scene, &opts) {
            tracing::warn!(target: "regia_editor", "PIE {label} starts without the level: {e}");
        }
        // Replicated entities come from the server, never from the client's copy of the level.
        let world = app.world_mut();
        let local: Vec<Entity> = world
            .query_filtered::<Entity, With<Replicated>>()
            .iter(world)
            .collect();
        for entity in local {
            world.despawn(entity);
        }
    }
    let on_load = game.exports.on_load;
    if let Err(caught) = crate::pie::catch_in_pie(|| on_load(app.world_mut())) {
        tracing::error!(
            target: "regia_editor",
            "game on_load panicked in PIE {label}: {}",
            caught.log_text()
        );
    }
    app.finish_plugins();
    let socket = net
        .bind(client_addr(index))
        .map_err(|e| PieWorldsError::new("pie_net_host_failed", e.to_string()))?;
    let opts = link_options(hz, label.clone(), label.clone());
    let connected = app.world_mut().resource_mut::<NetLink>().connect::<PieWire>(
        Box::new(socket),
        PIE_SERVER_ADDR,
        &opts,
    );
    if let Err(e) = connected {
        // `on_load` already ran in this client.
        unload_client(game, &mut app, &label);
        let code = if e.code() == "link_running" {
            "pie_net_link_busy"
        } else {
            "pie_net_host_failed"
        };
        return Err(PieWorldsError::new(code, format!("{label}'s link: {e}")));
    }
    let id = CLIENT_VIEW_ID_BASE + index as u64;
    let mut view = ViewportInstance::from_world(edit, id);
    view.layout.possessed = true;
    app.insert_resource(view.camera.clone());
    Ok(PieClientWorld {
        index,
        app,
        view,
        panel: ViewportPanelState::default(),
        frames: 0,
        last_update_us: 0,
        total_update_us: 0,
        panic: None,
        panic_backtrace: None,
    })
}

fn copy_resource<R: Resource + Clone>(from: &World, to: &mut World) {
    if let Some(r) = from.get_resource::<R>() {
        to.insert_resource(r.clone());
    }
}

/// Runs `f` on client `index`'s world.
///
/// # Errors
///
/// `pie_net_no_client_worlds` when none run, `pie_net_client_unknown` for an index out of range.
pub fn with_client<R>(
    world: &mut World,
    index: usize,
    f: impl FnOnce(&mut World, &mut PieClientWorld) -> R,
) -> Result<R, PieWorldsError> {
    let Some(mut worlds) = world.remove_non_send::<PieWorlds>() else {
        let status = world
            .get_resource::<PieWorldsStatus>()
            .cloned()
            .unwrap_or_default();
        return Err(PieWorldsError::new(
            "pie_net_no_client_worlds",
            format!(
                "no PIE client worlds run ({})",
                status.reason.as_deref().unwrap_or("not_playing")
            ),
        ));
    };
    let count = worlds.clients.len();
    let result = match worlds.clients.get_mut(index) {
        Some(client) => Ok(f(world, client)),
        None => Err(PieWorldsError::new(
            "pie_net_client_unknown",
            format!("client {index} is not in 0..{count}"),
        )),
    };
    world.insert_non_send(worlds);
    result
}

// ---------------------------------------------------------------------------------------------
// The Net tab's client views.

/// Maps an egui key to the physical key name `regia_input` uses (`KeyW`, `Digit1`, `ArrowUp`).
#[must_use]
pub fn physical_key_name(key: egui::Key) -> String {
    let debug = format!("{key:?}");
    if debug.len() == 1 && debug.chars().all(|c| c.is_ascii_uppercase()) {
        return format!("Key{debug}");
    }
    if let Some(digit) = debug.strip_prefix("Num")
        && digit.len() == 1
    {
        return format!("Digit{digit}");
    }
    debug
}

/// Draws the client worlds part of the Net tab: a client picker, the input-focus toggle, the
/// client's numbers and its view (the viewport-instance pane over the client's world).
pub fn clients_panel(ui: &mut egui::Ui, world: &mut World, device: Option<&regia_render::RenderDevice>) {
    ui.separator();
    ui.strong("Client worlds");
    let Some(mut worlds) = world.remove_non_send::<PieWorlds>() else {
        let status = world
            .get_resource::<PieWorldsStatus>()
            .cloned()
            .unwrap_or_default();
        ui.weak(match status.reason.as_deref() {
            None | Some("not_playing") => {
                "Press Play with a mode and clients set: each client gets its own world and view.".to_owned()
            }
            Some(_) => status.message,
        });
        return;
    };
    let count = worlds.clients.len();
    worlds.picked = worlds.picked.min(count.saturating_sub(1));
    ui.horizontal(|ui| {
        let picked = worlds.picked;
        egui::ComboBox::from_label("Client")
            .selected_text(format!("client-{}", picked + 1))
            .show_ui(ui, |ui| {
                for i in 0..count {
                    ui.selectable_value(&mut worlds.picked, i, format!("client-{}", i + 1));
                }
            });
        let mut focused = worlds.focus == Some(worlds.picked);
        if ui.checkbox(&mut focused, "Focus input on this client").changed() {
            let focus = focused.then_some(worlds.picked);
            worlds.set_focus(focus);
            // The Outliner, Inspector and play border follow the focused client (ADR-7829).
            if let Some(index) = focus {
                world.insert_resource(PieViewRole::Client(index));
            }
        }
    });
    let picked = worlds.picked;
    let focus = worlds.focus;
    let textures = world.get_resource::<crate::pane_texture::PaneTextures>().cloned();
    let theme = world.get_resource::<crate::theme::ThemeTokens>().cloned();
    if let Some(client) = worlds.clients.get_mut(picked) {
        let row = client.row();
        ui.label(format!(
            "{}: {} · {} entities, {} replicated, {} owned · tick {} · step {} us",
            row.label,
            if row.connected { "connected" } else { "joining" },
            row.entities,
            row.replicated,
            row.owned,
            row.last_applied_tick,
            row.last_update_us
        ));
        if let Some(panic) = &row.panic {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("stopped by a panic: {panic}"));
        }
        let cw = client.app.world_mut();
        if let Some(t) = textures {
            cw.insert_resource(t);
        }
        if let Some(t) = theme {
            cw.insert_resource(t);
        }
        let size = egui::vec2(
            ui.available_width(),
            (ui.available_width() * 9.0 / 16.0).max(120.0),
        );
        let response = ui
            .allocate_ui(size, |ui| {
                crate::viewport::viewport_panel_with(ui, cw, device, &mut client.panel, &mut client.view);
            })
            .response;
        cw.insert_resource(client.view.camera.clone());
        worlds.view_drawn = true;
        if focus != Some(picked) || !response.contains_pointer() || !ui.input(|i| i.focused) {
            // The release of a key pressed over the view may never come back to this view.
            worlds.release_held(|h| h.from_view);
        } else {
            let keys: Vec<(String, bool)> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Key {
                            key,
                            pressed,
                            repeat: false,
                            ..
                        } => Some((physical_key_name(*key), *pressed)),
                        _ => None,
                    })
                    .collect()
            });
            for (key, down) in keys {
                // The focus is this client, so the key cannot be refused.
                let _ = worlds.key_message(&key, down, true);
            }
        }
    }
    world.insert_non_send(worlds);
}

// ---------------------------------------------------------------------------------------------
// Agent methods.

fn rpc(e: PieWorldsError) -> RpcError {
    let (kind, hint) = match e.code {
        "pie_net_client_unknown" => (NOT_FOUND, "take indexes from editor.pie_net_clients"),
        "pie_net_no_client_worlds" => (
            INVALID_PARAMS,
            "editor.pie_net_config { mode: \"listen\", clients: 2 }, open a game crate, then editor.play",
        ),
        "pie_net_no_focus" => (INVALID_PARAMS, "editor.pie_net_focus { client: 0 } first"),
        _ => (INVALID_PARAMS, "see book/agents/pie-net.md"),
    };
    RpcError::new(kind, e.code, e.message, hint)
}

/// The `client` parameter: `None` when absent.
///
/// # Errors
///
/// `client` is not a whole number.
pub fn client_param(params: &Value) -> Result<Option<usize>, RpcError> {
    match params.get("client") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("`client` must be a 0-based client index")),
    }
}

/// Runs `handler` on the client world `params.client` names; `None` when the call has no
/// `client` (the caller answers on the edit world as before). The handler sees the params
/// without `client`, and the answer gains `client`.
pub fn route_to_client(
    world: &mut World,
    params: &Value,
    handler: Handler,
) -> Option<Result<Value, RpcError>> {
    let index = match client_param(params) {
        Ok(Some(index)) => index,
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    let mut inner = params.clone();
    if let Value::Object(map) = &mut inner {
        map.remove("client");
    }
    Some(
        with_client(world, index, |_, client| {
            let mut out = handler(client.app.world_mut(), &inner)?;
            if let Value::Object(map) = &mut out {
                map.insert("client".into(), json!(index));
            }
            Ok(out)
        })
        .map_err(rpc)
        .and_then(|r| r),
    )
}

/// `editor.capture_viewport { client }`: the client's view (its own camera and renderer).
///
/// # Errors
///
/// No adapter, no client worlds, an unknown client, or a failed render.
pub fn capture_client(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let index = client_param(params)?.unwrap_or(0);
    if let Some(err) = crate::headless::no_adapter_error(world) {
        return Err(err);
    }
    let device = world
        .get_resource::<regia_render::RenderDevice>()
        .cloned()
        .ok_or_else(|| {
            RpcError::new(
                regia_agent::protocol::UNSUPPORTED,
                "F0011",
                "no render device for the capture",
                "a Vulkan adapter (lavapipe or hardware) is required",
            )
        })?;
    let dim = |name: &str, default: u32| {
        params
            .get(name)
            .and_then(Value::as_u64)
            .map_or(default, |v| v.clamp(16, 4096) as u32)
    };
    let size = [
        dim("width", CLIENT_CAPTURE_SIZE[0]),
        dim("height", CLIENT_CAPTURE_SIZE[1]),
    ];
    let frame = with_client(world, index, |_, client| {
        let _span = tracing::info_span!(target: "regia_editor", "pie_client_capture").entered();
        let started = Instant::now();
        let frame = crate::render::capture_instance(
            &device,
            client.app.world_mut(),
            &client.view,
            ViewKind::Perspective,
            size,
        );
        (frame, started.elapsed().as_micros())
    })
    .map_err(rpc)?;
    let (Some(frame), render_us) = frame else {
        return Err(RpcError::new(
            regia_agent::protocol::UNSUPPORTED,
            "F0011",
            "the client view capture produced no image",
            "a Vulkan adapter (lavapipe or hardware) is required",
        ));
    };
    let png = crate::render::viewport_png_base64(&frame.image).map_err(RpcError::internal)?;
    Ok(json!({
        "client": index,
        "width": frame.image.width(),
        "height": frame.image.height(),
        "mime": "image/png",
        "png_base64": png,
        "view": "perspective",
        "possessed": frame.info.possessed,
        "render_us": u64::try_from(render_us).unwrap_or(u64::MAX),
    }))
}

/// `editor.pie_net_clients`.
fn pie_net_clients(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let status = world
        .get_resource::<PieWorldsStatus>()
        .cloned()
        .unwrap_or_default();
    let Some(mut worlds) = world.remove_non_send::<PieWorlds>() else {
        return Ok(json!({
            "running": false,
            "reason": status.reason.unwrap_or_else(|| "not_playing".to_owned()),
            "message": status.message,
            "focus": Value::Null,
            "clients": [],
        }));
    };
    let rows: Vec<PieClientRow> = worlds.clients.iter_mut().map(PieClientWorld::row).collect();
    let out = json!({
        "running": true,
        "tick_hz": worlds.hz,
        "server_frames": worlds.server_frames,
        "focus": worlds.focus,
        "picked": worlds.picked,
        "clients": rows,
    });
    world.insert_non_send(worlds);
    Ok(out)
}

/// Most rows `editor.pie_net_entities` answers by default.
const ENTITY_ROWS_DEFAULT: u64 = 200;

/// `editor.pie_net_entities`: what one client world holds, by net id (names are not replicated, so
/// `editor.hierarchy { client }` alone cannot tell a client's avatar from another's).
fn pie_net_entities(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(index) = client_param(params)? else {
        return Err(RpcError::invalid_params(
            "`client` is required: a 0-based index from editor.pie_net_clients",
        ));
    };
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(ENTITY_ROWS_DEFAULT);
    let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
    with_client(world, index, |_, client| {
        let row = client.row();
        let world = client.world_mut();
        let mut rows: Vec<(u32, Value)> = world
            .query::<(
                Entity,
                Option<&NetId>,
                Option<&NetOwnedLocally>,
                Option<&regia_scene::prelude::Name>,
                Option<&regia_scene::prelude::Transform>,
            )>()
            .iter(world)
            .filter(|(_, id, ..)| all || id.is_some())
            .map(|(entity, id, owned, name, transform)| {
                (
                    id.map_or(u32::MAX, |n| n.0),
                    json!({
                        "entity": entity.to_bits(),
                        "net_id": id.map(|n| n.0),
                        "owned": owned.is_some(),
                        "name": name.map(|n| n.as_str().to_owned()),
                        "translation": transform.map(|t| t.translation.to_array()),
                    }),
                )
            })
            .collect();
        let total = rows.len();
        // Net ids first, in order; entity bits break the tie among local entities.
        rows.sort_by_key(|(id, row)| (*id, row["entity"].as_u64()));
        let entities: Vec<Value> = rows
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .map(|(_, row)| row)
            .collect();
        json!({
            "client": index,
            "last_applied_tick": row.last_applied_tick,
            "total": total,
            "count": entities.len(),
            "entities": entities,
        })
    })
    .map_err(rpc)
}

/// `editor.pie_net_focus`.
fn pie_net_focus(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let index = client_param(params)?;
    let Some(mut worlds) = world.remove_non_send::<PieWorlds>() else {
        return Err(rpc(PieWorldsError::new(
            "pie_net_no_client_worlds",
            "no PIE client worlds run",
        )));
    };
    let count = worlds.clients.len();
    let result = match index {
        Some(i) if i >= count => Err(rpc(PieWorldsError::new(
            "pie_net_client_unknown",
            format!("client {i} is not in 0..{count}"),
        ))),
        _ => {
            worlds.set_focus(index);
            if let Some(i) = index {
                worlds.picked = i;
                // The panels follow the focused client (ADR-7829).
                world.insert_resource(PieViewRole::Client(i));
            }
            Ok(json!({ "focus": worlds.focus, "picked": worlds.picked }))
        }
    };
    world.insert_non_send(worlds);
    result
}

/// `editor.pie_net_key`.
fn pie_net_key(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty() && k.len() <= 64)
        .ok_or_else(|| RpcError::invalid_params("`key` is required: a physical key name such as KeyW"))?;
    let down = params.get("down").and_then(Value::as_bool).unwrap_or(true);
    let Some(mut worlds) = world.remove_non_send::<PieWorlds>() else {
        return Err(rpc(PieWorldsError::new(
            "pie_net_no_client_worlds",
            "no PIE client worlds run",
        )));
    };
    let result = worlds.send_key(key, down).map_err(rpc);
    world.insert_non_send(worlds);
    let client = result?;
    Ok(json!({ "client": client, "key": key, "down": down }))
}

/// The `client` parameter of the methods that can answer for one PIE client world.
pub const P_CLIENT: ParamSchema = ParamSchema {
    name: "client",
    kind: "integer",
    required: false,
    description: "0-based play-in-editor client index (editor.pie_net_clients): answer for that client's world.",
};

/// The `editor.pie_net_clients`, `editor.pie_net_entities`, `editor.pie_net_focus` and
/// `editor.pie_net_key` methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.pie_net_clients",
                tool: "pie_net_clients",
                description: "List the play-in-editor client worlds: one ECS world per client running the game's client side, joined to the play world over the in-memory transport. Per client: connected, peer, entity counts (all, replicated, owned), the last applied server tick, frames and step time. Without client worlds, the reason (not_playing, offline, no_clients, pie_net_no_game, pie_net_link_busy, pie_net_host_failed).",
                params: &[],
                result: "{ running, reason?, message?, tick_hz?, server_frames?, focus, picked?, clients: [{ index, label, connected, peer, entities, replicated, owned, last_applied_tick, frames, last_update_us, mean_update_us, panic }] }",
                mutating: false,
                permission: Permission::Read,
            },
            pie_net_clients as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pie_net_entities",
                tool: "pie_net_entities",
                description: "What one PIE client world holds: its replicated entities by net id, with owned (predicted by this client) and translation, as this client sees them.",
                params: &[
                    ParamSchema {
                        name: "client",
                        kind: "integer",
                        required: true,
                        description: "0-based client index (editor.pie_net_clients).",
                    },
                    ParamSchema {
                        name: "all",
                        kind: "boolean",
                        required: false,
                        description: "Also list the world's local entities (no net id). Default false.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Most rows to answer (default 200); total still counts them all.",
                    },
                ],
                result: "{ client, last_applied_tick, total, count, entities: [{ entity, net_id, owned, name, translation }] }",
                mutating: false,
                permission: Permission::Read,
            },
            pie_net_entities as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pie_net_focus",
                tool: "pie_net_focus",
                description: "Give the input focus to one PIE client world (keys from the Net tab's client view and editor.pie_net_key go to it only), or take it away with client null.",
                params: &[P_CLIENT],
                result: "{ focus, picked }",
                mutating: true,
                permission: Permission::Play,
            },
            pie_net_focus as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pie_net_key",
                tool: "pie_net_key",
                description: "Press or release one key in the focused PIE client world (a regia_platform KeyboardInput message, read by regia_input's keyboard). The key is a physical name: KeyW, ArrowUp, Space, Digit1.",
                params: &[
                    ParamSchema {
                        name: "key",
                        kind: "string",
                        required: true,
                        description: "Physical key name (KeyW, ArrowUp, Space, Digit1).",
                    },
                    ParamSchema {
                        name: "down",
                        kind: "boolean",
                        required: false,
                        description: "true presses (default), false releases.",
                    },
                ],
                result: "{ client, key, down }",
                mutating: true,
                permission: Permission::Play,
            },
            pie_net_key as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pie_view",
                tool: "pie_view",
                description: "Choose which world of a networked play session the editor shows (ADR-7829): \
                    the Outliner and Inspector show that world, and the play border and toolbar tint use \
                    that role's theme color (accents.play_server, play_client_1..4). Without `role`, answers \
                    the current view. Focusing a client's input (editor.pie_net_focus) also views it. \
                    Without client worlds: `pie_net_no_client_worlds`; an unknown role: `pie_view_role_unknown`; \
                    a client out of range: `pie_net_client_unknown`.",
                params: &[
                    ParamSchema {
                        name: "role",
                        kind: "string",
                        required: false,
                        description: "\"server\" (the play world) or \"client\" (with `client`).",
                    },
                    ParamSchema {
                        name: "client",
                        kind: "integer",
                        required: false,
                        description: "0-based client index with `role: \"client\"` (shown as Client 1, 2, …).",
                    },
                ],
                result: "{ role, client?, label, color: \"#rrggbb\"|null, clients }",
                mutating: true,
                permission: Permission::Play,
            },
            pie_view as Handler,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// The view role (ADR-7829): which world of a networked session the panels show.

/// Which world of a networked play session the Outliner, Inspector and play border show. An
/// edit-world resource (view state); it means something only while client worlds run.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PieViewRole {
    /// The play world (the server).
    #[default]
    Server,
    /// Client world `index` (0-based; shown as Client `index + 1`).
    Client(usize),
}

impl PieViewRole {
    /// The toolbar and log label: `Server`, `Client 1`, …
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Server => "Server".to_owned(),
            Self::Client(index) => format!("Client {}", index + 1),
        }
    }
}

/// The viewed role while client worlds run (`None` otherwise). A client index the session does not
/// have reads as the server.
#[must_use]
pub fn viewed_role(world: &World) -> Option<PieViewRole> {
    let count = world.get_non_send::<PieWorlds>()?.clients.len();
    if count == 0 {
        return None;
    }
    Some(
        match world.get_resource::<PieViewRole>().copied().unwrap_or_default() {
            PieViewRole::Client(index) if index < count => PieViewRole::Client(index),
            _ => PieViewRole::Server,
        },
    )
}

/// Runs `panel` on the world the Outliner and Inspector show: the viewed client's world while one
/// is viewed, else `play` (the play world while playing, the edit world otherwise).
pub fn with_viewed_world(edit: &mut World, play: Option<&mut World>, panel: impl FnOnce(&mut World)) {
    if let Some(PieViewRole::Client(index)) = viewed_role(edit)
        && let Some(mut worlds) = edit.remove_non_send::<PieWorlds>()
    {
        if let Some(client) = worlds.clients.get_mut(index) {
            panel(client.app.world_mut());
            edit.insert_non_send(worlds);
            return;
        }
        edit.insert_non_send(worlds);
    }
    panel(play.unwrap_or(edit));
}

/// The play toolbar's "Viewing:" switch while client worlds run: Server, Client 1, …; one
/// focusable button each (keyboard: Tab, Space).
pub fn view_switch(ui: &mut egui::Ui, world: &mut World) {
    use crate::agent_parity::{PIE_VIEW_CLIENT, PIE_VIEW_SERVER, action_button_text};
    let Some(current) = viewed_role(world) else {
        return;
    };
    let count = world.get_non_send::<PieWorlds>().map_or(0, |w| w.clients.len());
    ui.label("Viewing:");
    let mark = |on: bool| if on { "● " } else { "" };
    let mut picked = None;
    if action_button_text(
        ui,
        &PIE_VIEW_SERVER,
        format!("{}Server", mark(current == PieViewRole::Server)),
    )
    .on_hover_text("Show the server's world in the Outliner and Inspector")
    .clicked()
    {
        picked = Some(PieViewRole::Server);
    }
    for index in 0..count {
        let role = PieViewRole::Client(index);
        if action_button_text(
            ui,
            &PIE_VIEW_CLIENT,
            format!("{}{}", mark(current == role), role.label()),
        )
        .on_hover_text("Show this client's world in the Outliner and Inspector")
        .clicked()
        {
            picked = Some(role);
        }
    }
    if let Some(role) = picked {
        world.insert_resource(role);
    }
}

/// `editor.pie_view`.
fn pie_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let count = world.get_non_send::<PieWorlds>().map_or(0, |w| w.clients.len());
    if count == 0 {
        return Err(rpc(PieWorldsError::new(
            "pie_net_no_client_worlds",
            "no PIE client worlds run: there is one world to view",
        )));
    }
    match params.get("role").and_then(Value::as_str) {
        None => {}
        Some("server") => {
            world.insert_resource(PieViewRole::Server);
        }
        Some("client") => {
            let index = client_param(params)?.ok_or_else(|| {
                rpc(PieWorldsError::new(
                    "pie_net_client_unknown",
                    "`role: \"client\"` needs `client` (a 0-based index)",
                ))
            })?;
            if index >= count {
                return Err(rpc(PieWorldsError::new(
                    "pie_net_client_unknown",
                    format!("client {index} is not in 0..{count}"),
                )));
            }
            world.insert_resource(PieViewRole::Client(index));
        }
        Some(other) => {
            return Err(rpc(PieWorldsError::new(
                "pie_view_role_unknown",
                format!("`role` must be \"server\" or \"client\", not {other:?}"),
            )));
        }
    }
    let role = viewed_role(world).unwrap_or_default();
    let color = crate::pie_controls::play_tint(world).map(|c| {
        let [r, g, b, _] = c.to_srgba_unmultiplied();
        format!("#{r:02x}{g:02x}{b:02x}")
    });
    Ok(json!({
        "role": match role { PieViewRole::Server => "server", PieViewRole::Client(_) => "client" },
        "client": match role { PieViewRole::Server => None, PieViewRole::Client(i) => Some(i) },
        "label": role.label(),
        "color": color,
        "clients": count,
    }))
}

#[cfg(test)]
mod tests;
