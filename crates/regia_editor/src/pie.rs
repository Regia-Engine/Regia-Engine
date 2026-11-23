// SPDX-License-Identifier: MPL-2.0
//! Play in editor (PIE): the play world and its lifecycle (`docs/specs/19-editor.md` § 4).
//!
//! Play runs a **copy** of the edit world: [`start_play`] snapshots the edit world through
//! `regia_scene`, registers the hot-reloaded game plugin (when a game crate is open) into a fresh
//! play app, spawns the snapshot into it and calls the game's `on_load`, in the shipped game's
//! order (plugins, then the level; ADR-7251). The edit world is never
//! touched while playing, so Stop "restores" it by construction — the acceptance test snapshots
//! the edit world before play and after stop and requires the bytes to be identical.
//!
//! [`App`] is not `Send` (its plugin and runner boxes are single-threaded), so the play world
//! cannot live in a resource. The session is owned by the editor loop ([`crate::main`-style
//! hosts]); the panels read [`PieStatus`] and request transitions through [`PieRequests`], the
//! same resources the `editor.*` agent methods use (`specs/23` § 4: nothing blocks the protocol
//! thread, results are polled).
//!
//! Panics inside game code during play are caught per `specs/19` § 7: PIE stops, the panic is
//! logged with its backtrace ([`crate::panic_trace`]), the editor stays alive.
//!
//! A session starts in one of two [`PlayKind`]s (`specs/19` § 4, ADR-7205): **Play** runs the game
//! as a player would; **Simulate** runs the same world (physics, AI, scripts) with no local player
//! spawned or possessed, so the editor camera stays free. Physics follows the game (ADR-7251): with
//! a game crate open, play has physics when the game's `register` adds `PhysicsPlugin`, as in the
//! shipped game; without one, play adds it so the level's bodies simulate. Either way the physics
//! spawn-order tracking starts before the level is spawned (ADR-9180). [`PlayOptions::players`] above 1 joins that many local players through the
//! game's `PlayerManager` and reports one split-screen viewport each ([`PieStatus::player_views`]);
//! a play world without window cameras gets a PIE stand-in camera per player
//! ([`PiePlayerCamera`]), and the play window and a possessed viewport composite the views
//! (ADR-7395).

use std::time::Instant;

use crate::engine::app::App;
use crate::engine::app::MinimalPlugins;
use crate::engine::app::{PostUpdate, PostUpdateSet};
use crate::engine::ecs::prelude::*;
use regia_scene::layers::LEVEL_LAYERS_PATH;
use regia_scene::level::{LEVEL_SETTINGS_PATH, Level, LevelId, LevelLibrary, LevelManager};
use regia_scene::prelude::{Name, SpawnOptions, Transform, snapshot_scene, spawn_scene};
use regia_scene::spawn::{SnapshotFilter, UnknownTypePolicyOption};

use crate::hot_reload::GameExports;
use crate::selection::Selection;
use crate::viewport::{EditorCamera, GizmoState, ViewportSettings};
use regia_scene::prelude::PrefabLibrary;

/// Whether the play world is stepping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayMode {
    /// Simulating every frame.
    Playing,
    /// Frozen between frames (step with [`PieSession::step`]).
    Paused,
}

impl PlayMode {
    /// Stable name for the protocol and the status bar.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
        }
    }
}

/// How a session plays (`specs/19` § 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayKind {
    /// The game as a player plays it: its own players join and possess.
    #[default]
    Play,
    /// The world runs (physics, AI, scripts) with no local player spawned or possessed; the editor
    /// camera stays free.
    Simulate,
}

impl PlayKind {
    /// Stable name for the protocol (`play`, `simulate`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Simulate => "simulate",
        }
    }

    /// Parses [`PlayKind::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "play" => Some(Self::Play),
            "simulate" => Some(Self::Simulate),
            _ => None,
        }
    }
}

/// The most local players a split-screen play session joins (`editor.play { players }`).
pub const MAX_PIE_PLAYERS: u8 = 8;

/// What a play request asks for (`editor.play { mode, window, players, preset, size }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayOptions {
    /// Play or Simulate.
    pub kind: PlayKind,
    /// Show the play world in a separate window (the windowed editor only).
    pub window: bool,
    /// Local players to join for split-screen testing. `1` leaves joining to the game; `2..=8`
    /// joins that many through `PlayerManager`. Ignored by Simulate (it joins none).
    pub players: u8,
    /// The play window's resolution for this session (`editor.play`'s `preset` / `size`);
    /// `None` uses the `play.resolution` preference (ADR-7283). Only with `window`.
    pub resolution: Option<crate::play_window::PlayResolution>,
    /// Where the player starts (`editor.play { start }`, ADR-7829); `None` uses the Play
    /// dropdown's [`crate::pie_controls::PlayStartChoice`].
    pub start: Option<crate::pie_controls::PlayStart>,
    /// The session's starting time scale (`editor.play { time_scale }`; real time by default).
    pub time_scale: crate::pie_controls::TimeScale,
    /// Start with the play world's audio muted (`editor.play { mute }`).
    pub muted: bool,
}

impl Default for PlayOptions {
    fn default() -> Self {
        Self {
            kind: PlayKind::Play,
            window: false,
            players: 1,
            resolution: None,
            start: None,
            time_scale: crate::pie_controls::TimeScale::ONE,
            muted: false,
        }
    }
}

/// One local player's split-screen viewport in the play world (`PlayerManager`'s layout).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerView {
    /// The player index.
    pub player: u8,
    /// Normalized `(x, y, width, height)` of the player's viewport, `0..=1`.
    pub rect: [f32; 4],
}

/// PIE's stand-in camera for one joined local player (ADR-7395): welded to the player's
/// controller, laid out in that player's split-screen rect and drawn in player order, so the
/// play window (and a possessed viewport pane) can composite every player when the game has no
/// cameras of its own. A play world with window cameras — a game's or a level's — gets none:
/// whoever authored a camera owns presentation. PIE decides every frame, after the game's
/// `Startup` and `Update` ran (ADR-7466): a camera the game spawns at any point replaces the
/// stand-ins.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiePlayerCamera {
    /// The player whose view this camera shows.
    pub player: u8,
}

/// How far apart PIE stands the players it joined, meters: far enough that a player's stand-in view
/// cannot see the neighbours' markers in a quarter pane (45° vertical fov).
const STAND_IN_SPREAD: f32 = 4.0;

/// The stand-in camera's offset from its player's controller: over the shoulder, looking the
/// controller's forward (its −Z), like a game's chase camera at rest.
const STAND_IN_OFFSET: regia_core::math::Vec3 = regia_core::math::Vec3::new(0.0, 2.5, 5.0);

/// The game library a PIE session registers (the hot-reload host's current generation).
///
/// `GameExports` is three function pointers, so this is `Send + Sync` and can live in the edit
/// world as a resource; the editor binary refreshes it after every hot reload.
#[derive(Resource, Clone)]
pub struct CurrentGame {
    /// The export table of the loaded game dylib.
    pub exports: GameExports,
    /// Its hot-reload generation (the metadata salt).
    pub generation: u32,
    /// The game crate's package name, for the status bar.
    pub package: String,
}

/// One play session: the play world app and its bookkeeping.
pub struct PieSession {
    /// The play world: a copy of the edit world at [`start_play`] time.
    pub app: App,
    /// Whether the session steps.
    pub mode: PlayMode,
    /// Frames simulated so far.
    pub frames: u64,
    /// When play started (the `< 1 s` acceptance measure).
    pub started: Instant,
    /// Wall time from the play request through the first settled frame.
    pub start_elapsed_ms: u128,
    /// Entities the snapshot spawned, plus what PIE added at start (the game's `on_load`, the
    /// players it joined) and the stand-in cameras there are now: kept current as stand-ins come
    /// and go after start (rev-claude-58 play finding 3).
    pub entities: usize,
    /// The registered game plugin, when a game crate is open.
    pub game: Option<CurrentGame>,
    /// A panic message caught during a frame, when one ended the session.
    pub panic: Option<String>,
    /// The backtrace of [`PieSession::panic`], when one was captured.
    pub panic_backtrace: Option<String>,
    /// What the session was started with.
    pub options: PlayOptions,
}

impl PieSession {
    /// Pauses the session around a hot-reload swap and returns the mode to restore afterwards
    /// (`specs/20` § 3 step 3). A session the user paused stays paused.
    pub fn pause_for_reload(&mut self) -> PlayMode {
        std::mem::replace(&mut self.mode, PlayMode::Paused)
    }

    /// Restores the mode [`PieSession::pause_for_reload`] returned (`specs/20` § 3 step 10).
    pub fn resume_after_reload(&mut self, previous: PlayMode) {
        self.mode = previous;
    }

    /// Runs one frame while playing (a no-op while paused). Returns the panic message when game
    /// or scene code panicked (`specs/19` § 7); the caller stops the session.
    #[tracing::instrument(target = "regia_editor", skip_all, name = "pie_frame")]
    pub fn frame(&mut self) -> Option<String> {
        if self.mode != PlayMode::Playing {
            return None;
        }
        self.frames += 1;
        // A muted play world stays muted when the game builds or unmutes its audio (ADR-7829).
        crate::pie_controls::enforce_mute(self.app.world_mut());
        // Play systems on worker threads show under `pie_frame` in the Profiler (ADR-9740).
        let _context = regia_core::profiling::flame_context("pie_frame");
        let result = catch_in_pie(|| self.app.update());
        self.count_stand_ins();
        match result {
            Ok(()) => None,
            Err(caught) => Some(self.record_panic("PIE panic", caught)),
        }
    }

    /// Moves [`PieSession::entities`] by the stand-in cameras that came or went since it last
    /// counted them ([`PieStandInTally`]).
    fn count_stand_ins(&mut self) {
        let Some(mut tally) = self.app.world_mut().get_resource_mut::<PieStandInTally>() else {
            return;
        };
        let (live, counted) = (tally.live, tally.counted);
        if live == counted {
            return;
        }
        tally.counted = live;
        self.entities = (self.entities + live).saturating_sub(counted);
    }

    /// Logs a caught panic with its backtrace (`specs/19` § 7) and keeps both for the status.
    fn record_panic(&mut self, what: &str, caught: crate::panic_trace::CaughtPanic) -> String {
        tracing::error!(target: "regia_editor", "{what}: {}", caught.log_text());
        self.panic = Some(caught.message.clone());
        self.panic_backtrace = caught.backtrace;
        caught.message
    }

    /// Runs `frames` frames while paused (step). Returns the panic message, when one occurred.
    #[tracing::instrument(target = "regia_editor", skip_all, name = "pie_step")]
    pub fn step(&mut self, frames: u32) -> Option<String> {
        if self.mode != PlayMode::Paused {
            return None;
        }
        let _context = regia_core::profiling::flame_context("pie_step");
        for _ in 0..frames.max(1) {
            self.frames += 1;
            let result = catch_in_pie(|| self.app.update());
            self.count_stand_ins();
            if let Err(caught) = result {
                return Some(self.record_panic("PIE panic while stepping", caught));
            }
        }
        None
    }
}

/// Starts play: snapshots `world` (the edit world), registers `game` when a game crate is open,
/// spawns the copy into the fresh play app and calls the game's `on_load`. Once play has started, the edit world's selection
/// is cleared (it refers to edit-world entities that have no counterpart identity in the play
/// world); a play that fails to start leaves it as it was.
///
/// # Errors
///
/// A scene snapshot/spawn failure (message names the failing step).
pub fn start_play(world: &mut World, game: Option<CurrentGame>) -> Result<PieSession, String> {
    start_play_with(world, game, PlayOptions::default())
}

/// [`start_play`] with [`PlayOptions`]: Simulate joins no local player and leaves the camera free;
/// `players` above 1 joins that many local players for split-screen.
///
/// # Errors
///
/// A scene snapshot/spawn failure, or a first frame that panicked.
pub fn start_play_with(
    world: &mut World,
    game: Option<CurrentGame>,
    mut options: PlayOptions,
) -> Result<PieSession, String> {
    let started = Instant::now();
    // Authored content must carry scene identity, whichever path created it; the snapshot
    // addresses entities by guid (see `crate::visibility::ensure_scene_identity`). Only what the
    // snapshot needs is added, so the edit world's save order survives play.
    let tagged = crate::visibility::ensure_snapshot_identity(world);
    if tagged > 0 {
        tracing::debug!(target: "regia_editor", tagged, "PIE: tagged guid-less user entities");
    }

    let filter = SnapshotFilter {
        level: None,
        resources: vec![LEVEL_SETTINGS_PATH.to_string()],
    };
    // The sequence preview's values (transforms, a taken-over `VideoPlayer`) are lifted for the
    // snapshot, so play starts from the authored level (sprint-w9-rev-v06-lanes § 1 finding 2).
    let mut scene = crate::seq_tracks::with_preview_lifted(world, |world| snapshot_scene(world, &filter))
        .map_err(|e| format!("PIE snapshot: {e}"))?;
    // The snapshot is the play level's template: its entities are members of the play level, so
    // a layer that goes absent despawns them and brings them back as in a game (ADR-2161). The
    // level's layer binding comes along for a later rebuild (a definition asset changed).
    let level_id = play_level_id(world);
    if let Some(binding) = edit_level_binding(world, level_id) {
        scene.resources.insert(LEVEL_LAYERS_PATH.to_owned(), binding);
    }

    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    // Physics follows the game (ADR-7251): a game crate that wants physics adds `PhysicsPlugin`
    // in its `register` (the shipped templates do), and PIE adds none behind one that does not, so
    // play matches the shipped game. Without a game crate the play world simulates the level's
    // bodies itself. The spawn-order tracking the plugin needs starts before the level spawns
    // either way (`track_physics_spawn_order` below, ADR-9180).
    if game.is_none() {
        app.add_plugins(regia::physics::PhysicsPlugin);
    }
    if options.kind == PlayKind::Play && options.players > 1 {
        // The split-screen layout comes from the player manager's `PostUpdate` pass.
        app.add_plugins(regia_framework::player::PlayerPlugin);
    }
    // Functional tests run in the play world (the Tests panel, spec 41 § 6).
    app.add_plugins(regia_framework::functional::FunctionalTestPlugin);
    app.add_plugins(regia_framework::visual_log::VisualLogPlugin);
    app.world_mut()
        .resource_mut::<regia_framework::functional::Runner>()
        .set_hooks(regia_agent::functional_test_hooks());
    // Layer definitions carry over; the table is bound to the play level once it is recorded.
    if let Some(defs) = world
        .get_resource::<regia_scene::layers::LayerDefsLibrary>()
        .cloned()
    {
        app.insert_resource(defs);
    }
    // The editor viewport resources carry over so the view does not jump when play starts.
    if let Some(camera) = world.get_resource::<EditorCamera>().cloned() {
        app.insert_resource(camera);
    }
    if let Some(gizmo) = world.get_resource::<GizmoState>().cloned() {
        app.insert_resource(gizmo);
    }
    if let Some(settings) = world.get_resource::<ViewportSettings>().cloned() {
        app.insert_resource(settings);
    }
    // The viewport's layout, bookmarks and loaded meshes carry over too; the play world is
    // marked so a viewport may possess its game camera.
    if let Some(mut layout) = world.get_resource::<crate::views::ViewportLayout>().cloned() {
        // Simulate keeps the editor camera: nothing possesses a game camera.
        if options.kind == PlayKind::Simulate {
            layout.possessed = false;
        }
        app.insert_resource(layout);
    }
    if let Some(marks) = world.get_resource::<crate::views::CameraBookmarks>().cloned() {
        app.insert_resource(marks);
    }
    if let Some(library) = world.get_resource::<crate::mesh_source::MeshLibrary>().cloned() {
        app.insert_resource(library);
    }
    if let Some(project) = world.get_resource::<crate::agent_ext::EditorProject>().cloned() {
        app.insert_resource(project);
    }
    app.insert_resource(crate::render::PlayWorld);
    // The Static-move guard runs on the play world only (the edit world moves statics
    // legitimately; `specs/14` A1).
    crate::static_moves::install(&mut app);
    // Play renders through its own runtime renderer with the VFX layer, like the game.
    if world.contains_resource::<regia_render::RuntimeRenderer>() {
        if let Some(device) = world.get_resource::<regia_render::RenderDevice>().cloned() {
            app.insert_resource(device);
        }
        app.init_resource::<regia_render::RuntimeRenderer>()
            .init_resource::<regia_render::ExtractedFrame>()
            .add_plugins((regia_vfx::VfxPlugin, regia_vfx::VfxRuntimePlugin));
    }
    crate::mesh_source::register_level_types(app.world_mut());
    // The physics spawn-order tracking starts before any body or collider arrives, so the level's
    // content is numbered in spawn order (ADR-1347) whichever plugin adds physics, and a
    // `PhysicsPlugin` built later finds it on (ADR-9180). A game without physics only gets the
    // arrival numbering.
    if let Err(error) = regia::physics::track_physics_spawn_order(app.world_mut()) {
        tracing::warn!(target: "regia_editor", "PIE: {error}; a game that adds PhysicsPlugin will fail to register");
    }
    // Empty selection in the play world (the copy above would reference dead ids).
    app.init_resource::<Selection>();
    // Prefab instances reference templates by asset id; share the edit world's baked
    // templates (the library is `Arc`-backed, so this clones references, not data).
    if let Some(library) = world.get_resource::<PrefabLibrary>().cloned() {
        app.insert_resource(library);
    }

    // The game's plugins go in before the level is spawned, as in the shipped game (plugins
    // first, then the level loads; ADR-7251): the component types its `register` registers
    // (`RigidBody` through `PhysicsPlugin`, the game's own components) are then known when the
    // level's entities arrive, instead of staying unregistered blobs, and its systems find the
    // level's content. `on_load` runs once the level is in.
    let mut entities = 0;
    let registered = match game {
        Some(ref game) => {
            let before = world_entity_count(&app);
            let registered = register_game_plugins(&mut app, game);
            entities += world_entity_count(&app).saturating_sub(before);
            registered
        }
        None => false,
    };

    let opts = SpawnOptions {
        level: Some(level_id),
        persistent: false,
        unknown_types: UnknownTypePolicyOption::Preserve,
    };
    let spawned = spawn_scene(app.world_mut(), &scene, &opts).map_err(|e| format!("PIE spawn: {e}"))?;
    record_play_level(app.world_mut(), Level::from_scene_file(level_id, scene));
    // The start option (ADR-7829): only the chosen player start keeps its marker in the copy, before
    // the game's `on_load` and the first frame spawn a player.
    let start = options
        .start
        .or_else(|| {
            world
                .get_resource::<crate::pie_controls::PlayStartChoice>()
                .map(|choice| choice.0)
        })
        .unwrap_or_default();
    options.start = Some(start);
    let editor_camera = world.get_resource::<EditorCamera>().cloned();
    // A start the copy does not know (a game that does not register `PlayerStart`, so the level's
    // starts stay unknown-type data) plays from the default start rather than not at all.
    if let Err(error) = crate::pie_controls::apply_start(app.world_mut(), start, editor_camera.as_ref()) {
        tracing::warn!(target: "regia_editor", "play start: {error}; starting from the level's player start");
    }
    if start == crate::pie_controls::PlayStart::Camera {
        entities += 1;
    }
    // Layers start at the level's initial states, whatever the Layers panel toggled.
    let mut layers = play_layers(world, level_id);
    layers.bind_to_active_levels(app.world());
    app.insert_resource(layers);

    entities += spawned.entities;
    if let Some(ref game) = game
        && registered
    {
        // Only what the game's `on_load` adds: the world already holds the scene.
        let before = world_entity_count(&app);
        load_game(&mut app, game, options.kind);
        entities += world_entity_count(&app).saturating_sub(before);
    }
    match options.kind {
        PlayKind::Simulate => {
            forbid_local_players(app.world_mut());
            // The entities actually gone (a player whose controller is itself, or was already
            // despawned, is one entity; a despawn may take descendants with it).
            let before = world_entity_count(&app);
            remove_local_players(app.world_mut());
            entities = entities.saturating_sub(before.saturating_sub(world_entity_count(&app)));
        }
        PlayKind::Play if options.players > 1 => {
            entities += join_local_players(app.world_mut(), options.players);
            // The stand-ins are decided every frame, after the game's `Startup` and `Update`
            // ran (ADR-7466): a camera the game spawns then owns presentation.
            app.init_resource::<PieStandInTally>();
            app.add_systems(
                PostUpdate,
                maintain_stand_in_cameras.before(PostUpdateSet::TransformPropagate),
            );
        }
        PlayKind::Play => {}
    }
    let xr_simulator = crate::xr_simulator::install(world, &mut app);

    app.finish_plugins();
    // One settled frame: propagation, pending prefabs, the level lifecycle.
    if let Err(caught) = catch_in_pie(|| app.update()) {
        tracing::error!(target: "regia_editor", "PIE first frame panicked: {}", caught.log_text());
        return Err(format!("play-in-editor first frame panicked: {}", caught.message));
    }
    // The stand-in cameras the first frame spawned are play entities too (ADR-7466); later frames
    // move the count as they come and go (`PieSession::count_stand_ins`).
    if let Some(mut tally) = app.world_mut().get_resource_mut::<PieStandInTally>() {
        tally.counted = tally.live;
        entities += tally.live;
    }
    if xr_simulator {
        crate::xr_simulator::begin(&mut app);
    }
    // With no player after the first frame, the camera start places the play cameras (ADR-7829).
    if start == crate::pie_controls::PlayStart::Camera {
        crate::pie_controls::place_cameras_without_player(app.world_mut(), editor_camera.as_ref());
    }
    crate::pie_controls::set_time_scale(app.world_mut(), options.time_scale);
    if options.muted {
        crate::pie_controls::set_mute(app.world_mut(), true);
    }
    // The edit selection references edit entities; the play world is a separate id space.
    if let Some(mut selection) = world.get_resource_mut::<Selection>() {
        selection.clear();
    }

    let start_elapsed_ms = started.elapsed().as_millis();
    tracing::info!(
        target: "regia_editor",
        entities,
        start_ms = start_elapsed_ms,
        game = game.as_ref().map(|g| g.package.as_str()).unwrap_or(""),
        kind = options.kind.as_str(),
        players = options.players,
        "play-in-editor started"
    );
    Ok(PieSession {
        app,
        mode: PlayMode::Playing,
        frames: 1,
        started,
        start_elapsed_ms,
        entities,
        game,
        panic: None,
        panic_backtrace: None,
        options,
    })
}

/// Simulate: the play world's player manager takes no local player (`MaxLocalPlayers(0)`, and a
/// manager capped at 0 when the game has none yet), so a join press or a game's own spawn call gets
/// `None`.
fn forbid_local_players(world: &mut World) {
    use regia_framework::player::{MaxLocalPlayers, PlayerManager};
    world.insert_resource(MaxLocalPlayers(0));
    match world.get_resource_mut::<PlayerManager>() {
        Some(mut manager) => manager.max_local = 0,
        None => world.insert_resource(PlayerManager::new(0)),
    }
}

/// Simulate: removes every local player the game spawned before the cap applied (in `on_load`),
/// with its controller. The caller counts the entities that went.
fn remove_local_players(world: &mut World) {
    use regia_framework::player::{Player, PlayerKind, PlayerManager};
    let local: Vec<(u8, Entity, Entity)> = world
        .query::<(Entity, &Player)>()
        .iter(world)
        .filter(|(_, player)| player.kind == PlayerKind::Local)
        .map(|(entity, player)| (player.index, entity, player.controller))
        .collect();
    let removed = local.len();
    for (index, entity, controller) in local {
        // The manager despawns both when it knows the player; a stray one goes here.
        PlayerManager::remove_player_static(world, index);
        for gone in [entity, controller] {
            if world.get_entity(gone).is_ok() {
                world.despawn(gone);
            }
        }
    }
    if removed > 0 {
        tracing::info!(target: "regia_editor", removed, "simulate: removed the game's local players");
    }
}

/// Split-screen: joins local players until `players` are in, through the play world's
/// `PlayerManager` (inserted when the game has none), raising the local cap to fit. Returns the
/// entities spawned. The controllers PIE joins here are recorded in [`PieJoinedControllers`]:
/// only those may be stood apart for the stand-in cameras (ADR-7466).
fn join_local_players(world: &mut World, players: u8) -> usize {
    use regia_framework::player::{MaxLocalPlayers, Player, PlayerKind, PlayerManager};
    let players = players.min(MAX_PIE_PLAYERS);
    let cap = world
        .get_resource::<MaxLocalPlayers>()
        .map_or(players, |max| max.0.max(players));
    world.insert_resource(MaxLocalPlayers(cap));
    match world.get_resource_mut::<PlayerManager>() {
        Some(mut manager) => manager.max_local = manager.max_local.max(cap),
        None => world.insert_resource(PlayerManager::new(cap)),
    }
    let present = world
        .query::<&Player>()
        .iter(world)
        .filter(|player| player.kind == PlayerKind::Local)
        .count();
    let mut spawned = 0;
    let mut joined = PieJoinedControllers::default();
    for _ in present..usize::from(players) {
        let Some((_, controller)) = PlayerManager::spawn_player_static(world, PlayerKind::Local) else {
            tracing::warn!(target: "regia_editor", "split-screen: the player manager refused a local player");
            break;
        };
        joined.pending.push(controller);
        spawned += 2; // the player and its controller
    }
    if let Some(mut manager) = world.get_resource_mut::<PlayerManager>() {
        manager.compute_viewports();
    }
    world.insert_resource(joined);
    spawned
}

/// The controllers PIE's split-screen join spawned that have not been stood apart yet
/// (ADR-7466). A controller the game joined itself is never listed, so PIE never moves it.
#[derive(Resource, Debug, Default)]
struct PieJoinedControllers {
    /// Controllers PIE joined, waiting for their player's first stand-in camera.
    pending: Vec<Entity>,
}

/// How many stand-in cameras the play world holds (`live`, written by `maintain_stand_in_cameras`
/// each frame) and how many [`PieSession::entities`] counts (`counted`): the session moves its count
/// by the difference after every frame (rev-claude-58 play finding 3).
#[derive(Resource, Debug, Default)]
struct PieStandInTally {
    /// Stand-in cameras after the last `maintain_stand_in_cameras`.
    live: usize,
    /// Stand-in cameras the session's entity count includes.
    counted: usize,
}

/// Records `live` stand-in cameras in the play world's [`PieStandInTally`].
fn tally_stand_ins(world: &mut World, live: usize) {
    if let Some(mut tally) = world.get_resource_mut::<PieStandInTally>()
        && tally.live != live
    {
        tally.live = live;
    }
}

/// Keeps one stand-in camera per joined local player while the play world has no window camera
/// of its own (ADR-7395, ADR-7466). It runs every frame in `PostUpdate`, after the game's
/// `Startup`, `Update` and join observers, and before transforms propagate:
///
/// - a window camera the game or level authored (any frame) owns presentation: every stand-in
///   goes, and none comes back while that camera is there;
/// - otherwise each local player with a layout rect gets a camera welded to its controller, kept
///   at the player's current rect and draw order; a player who left loses it;
/// - only a controller PIE joined itself ([`PieJoinedControllers`]) is stood `STAND_IN_SPREAD`
///   meters apart by player index, when its first stand-in arrives and only while it still holds
///   the identity transform the player manager gave it, so the panes differ. A controller the
///   game joined keeps its transform. A game moves the controllers and the views follow.
fn maintain_stand_in_cameras(world: &mut World) {
    use regia_framework::player::{Player, PlayerManager};
    let stand_ins: Vec<(Entity, u8, Option<Entity>)> = world
        .query::<(Entity, &PiePlayerCamera, Option<&ChildOf>)>()
        .iter(world)
        .map(|(entity, marker, parent)| (entity, marker.player, parent.map(ChildOf::parent)))
        .collect();
    let authored = world
        .query_filtered::<&regia_render::Camera, Without<PiePlayerCamera>>()
        .iter(world)
        .any(|camera| matches!(camera.target, regia_render::RenderTarget::Window));
    if authored {
        if !stand_ins.is_empty() {
            for (entity, _, _) in &stand_ins {
                world.despawn(*entity);
            }
            tracing::info!(
                target: "regia_editor",
                removed = stand_ins.len(),
                "split-screen: the game's own camera replaced PIE's stand-in cameras"
            );
        }
        tally_stand_ins(world, 0);
        return;
    }
    let players: Vec<(u8, Entity, [f32; 4])> = {
        let Some(manager) = world.get_resource::<PlayerManager>() else {
            let live = stand_ins.len();
            tally_stand_ins(world, live);
            return;
        };
        manager
            .iter()
            .filter_map(|(index, player)| {
                let rect = manager
                    .layout
                    .viewports
                    .get(&index)
                    .map(|(x, y, w, h)| [*x, *y, *w, *h])?;
                let controller = world.get::<Player>(player).map(|p| p.controller)?;
                world.get_entity(controller).ok()?;
                Some((index, controller, rect))
            })
            .collect()
    };
    // Keep the stand-ins of present players at their current rect; drop the rest.
    let mut covered: Vec<u8> = Vec::with_capacity(players.len());
    for (entity, player, parent) in stand_ins {
        let current = players
            .iter()
            .find(|(index, controller, _)| *index == player && Some(*controller) == parent);
        match current {
            Some((index, _, rect)) if !covered.contains(index) => {
                covered.push(*index);
                if let Some(mut camera) = world.get_mut::<regia_render::Camera>(entity) {
                    let order = i32::from(*index);
                    if camera.viewport != *rect || camera.order != order {
                        camera.viewport = *rect;
                        camera.order = order;
                    }
                }
            }
            _ => {
                if world.get_entity(entity).is_ok() {
                    world.despawn(entity);
                }
            }
        }
    }
    let mut added = 0usize;
    for (index, controller, rect) in players {
        if covered.contains(&index) {
            continue;
        }
        stand_apart_if_pie_joined(world, controller, index);
        world.spawn((
            Name::new(format!("pie player {} camera", index + 1)),
            Transform::from_translation(STAND_IN_OFFSET),
            regia_scene::prelude::GlobalTransform::default(),
            ChildOf(controller),
            regia_render::Camera {
                viewport: rect,
                order: i32::from(index),
                ..regia_render::Camera::default()
            },
            PiePlayerCamera { player: index },
        ));
        added += 1;
    }
    tally_stand_ins(world, covered.len() + added);
    if added > 0 {
        tracing::info!(
            target: "regia_editor",
            players = added,
            "split-screen: PIE stood in a camera per player (the play world has none)"
        );
    }
}

/// Stands a controller PIE joined `STAND_IN_SPREAD` meters along +X per player index, once, and
/// only while it holds the player manager's identity transform (a game that placed it keeps its
/// placement). A controller the game joined is never touched (ADR-7466).
fn stand_apart_if_pie_joined(world: &mut World, controller: Entity, index: u8) {
    let Some(mut joined) = world.get_resource_mut::<PieJoinedControllers>() else {
        return;
    };
    let Some(at) = joined.pending.iter().position(|pending| *pending == controller) else {
        return;
    };
    joined.pending.swap_remove(at);
    let Some(mut transform) = world.get_mut::<Transform>(controller) else {
        return;
    };
    if *transform == Transform::default() {
        transform.translation = regia_core::math::Vec3::new(f32::from(index) * STAND_IN_SPREAD, 0.0, 0.0);
    }
}

/// The split-screen panes of `world`'s local players inside `rect` (screen space), when two or
/// more players share the view; empty otherwise. The viewport outlines and labels them.
#[must_use]
pub fn split_screen_panes(world: &World, rect: egui::Rect) -> Vec<(u8, egui::Rect)> {
    let views = player_views(world);
    if views.len() < 2 {
        return Vec::new();
    }
    views
        .into_iter()
        .map(|view| {
            let [x, y, w, h] = view.rect;
            let min = rect.min + egui::vec2(x * rect.width(), y * rect.height());
            (
                view.player,
                egui::Rect::from_min_size(min, egui::vec2(w * rect.width(), h * rect.height())),
            )
        })
        .collect()
}

/// The play world's local players and their split-screen viewports, in player order.
#[must_use]
pub fn player_views(world: &World) -> Vec<PlayerView> {
    let Some(manager) = world.get_resource::<regia_framework::player::PlayerManager>() else {
        return Vec::new();
    };
    manager
        .layout
        .viewports
        .iter()
        .map(|(player, (x, y, w, h))| PlayerView {
            player: *player,
            rect: [*x, *y, *w, *h],
        })
        .collect()
}

/// The id of the play level: the open level's, else the edit world's current level, else
/// `PLAY_LEVEL_FALLBACK`.
fn play_level_id(world: &World) -> LevelId {
    world
        .get_resource::<crate::level_io::OpenLevel>()
        .map(|open| open.id)
        .or_else(|| {
            world
                .get_resource::<LevelManager>()
                .and_then(LevelManager::current)
        })
        .unwrap_or(PLAY_LEVEL_FALLBACK)
}

/// The play level's id when the edit world has no level open.
const PLAY_LEVEL_FALLBACK: LevelId = LevelId(regia_core::id::Guid::from_u128(
    0x504c_4159_0000_4000_8000_0000_0000_0001,
));

/// The edit level's raw layer binding (`LevelLayers`), when it has one.
fn edit_level_binding(world: &World, level: LevelId) -> Option<Box<ron::value::RawValue>> {
    world
        .get_resource::<LevelLibrary>()?
        .get(level)?
        .scene
        .resources
        .get(LEVEL_LAYERS_PATH)
        .cloned()
}

/// Puts the play level's template into the play world's library and records it active (its
/// entities were spawned by [`start_play`] with the editor's unknown-type policy).
fn record_play_level(world: &mut World, level: Level) {
    if let Some(mut manager) = world.remove_resource::<LevelManager>() {
        manager.record_spawned(&level);
        world.insert_resource(manager);
    }
    world
        .get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new)
        .insert(level);
}

/// The layer table a play session starts from: the edit world's layers at the initial states of
/// `level`'s binding (the play level).
fn play_layers(world: &World, level: LevelId) -> regia_scene::layers::LayerTable {
    let binding = world
        .get_resource::<LevelLibrary>()
        .and_then(|library| library.get(level).cloned())
        .and_then(|level| regia_scene::layers::LevelLayers::from_scene(&level.scene))
        .and_then(Result::ok);
    world
        .get_resource::<regia_scene::layers::LayerTable>()
        .map(|table| table.at_initial_states(binding.as_ref()))
        .unwrap_or_default()
}

/// Stops play: joins game threads through `on_unload` (when a game is open) and drops the play
/// world. The edit world needs nothing restored — it was never touched.
pub fn stop_play(mut session: PieSession) {
    if let Some(game) = session.game
        && let Err(caught) = catch_in_pie(|| {
            (game.exports.on_unload)(session.app.world_mut());
        })
    {
        tracing::error!(
            target: "regia_editor",
            "game on_unload panicked while stopping play: {}",
            caught.log_text()
        );
    }
    tracing::info!(target: "regia_editor", frames = session.frames, "play-in-editor stopped");
}

// ---------------------------------------------------------------------------
// Agent surface state (the play world itself cannot be a resource; see the module docs)
// ---------------------------------------------------------------------------

/// One requested play transition, queued by the panels or the agent surface and applied by the
/// editor loop (or the integration tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PieRequest {
    /// Stop any running session and start a fresh one from the edit world.
    Play,
    /// [`PieRequest::Play`] with options: Simulate, a separate window, split-screen players.
    PlayWith(PlayOptions),
    /// Freeze the running session.
    Pause,
    /// Resume the paused session.
    Resume,
    /// End the session.
    Stop,
    /// Advance `n` frames while paused.
    Step(u32),
    /// Copy the play-world state of these guids (`None`: the play world's selection) back to the
    /// edit world as one undo step ([`crate::pie_keep`]).
    KeepSimulationChanges(Option<Vec<regia_core::id::Guid>>),
    /// Set the running session's gameplay time scale (ADR-7829).
    SetTimeScale(crate::pie_controls::TimeScale),
    /// Mute or unmute the running session's audio (ADR-7829).
    SetMute(bool),
}

/// What the last "keep simulation changes" did (`editor.play_state`'s `kept`).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub enum LastKeep {
    /// It copied state (possibly nothing, when nothing differed).
    Kept(crate::pie_keep::KeepReport),
    /// It was refused (`keep_nothing_selected`, `not_playing`).
    Refused(crate::pie_keep::KeepError),
}

/// The queue of play transitions. The editor loop drains it once per frame.
#[derive(Resource, Debug, Clone, Default)]
pub struct PieRequests {
    /// Pending requests, oldest first.
    pub queue: Vec<PieRequest>,
}

impl PieRequests {
    /// Queues one request (public for the panels; the agent methods call this too).
    pub fn push(&mut self, request: PieRequest) {
        self.queue.push(request);
    }
}

/// What the panels and the agent surface can see of play state (`specs/23` § 4: play state is
/// queryable). Written by the editor loop.
#[derive(Resource, Debug, Clone, Default)]
pub struct PieStatus {
    /// `None` = stopped.
    pub mode: Option<PlayMode>,
    /// Entities in the play world's snapshot ([`PieSession::entities`]: what play started with plus
    /// the stand-in cameras there are now).
    pub entities: usize,
    /// Frames simulated.
    pub frames: u64,
    /// Wall time of the last start, milliseconds (the `< 1 s` measure).
    pub start_elapsed_ms: u64,
    /// The registered game crate, when one is open.
    pub game: Option<String>,
    /// The panic that ended the last session, when one did.
    pub panic: Option<String>,
    /// The play world's XR status line, when it runs XR.
    pub xr: Option<String>,
    /// `Static` entities that moved at runtime since play started (`specs/14` A1; see
    /// [`crate::static_moves::StaticMoveWarnings`]).
    pub static_move_warnings: u64,
    /// Play or Simulate (`None` = stopped).
    pub kind: Option<PlayKind>,
    /// The backtrace of [`PieStatus::panic`], when one was captured.
    pub panic_backtrace: Option<String>,
    /// Whether the session asked for a separate play window.
    pub window: bool,
    /// The play world's local players and their split-screen viewports.
    pub player_views: Vec<PlayerView>,
    /// The play world's gameplay time scale (0 while stopped; ADR-7829).
    pub time_scale: f32,
    /// Whether the play world's audio is muted (ADR-7829).
    pub muted: bool,
    /// Where the session's player started (`None` while stopped; ADR-7829).
    pub start: Option<crate::pie_controls::PlayStart>,
}

/// The panic that ended the last session, kept for [`PieStatus`] after the session is gone (the
/// next play start clears it).
#[derive(Resource, Debug, Clone, Default)]
pub struct LastPiePanic {
    /// The panic message.
    pub message: String,
    /// Its backtrace, when one was captured.
    pub backtrace: Option<String>,
}

/// Stops a session that panicked (`specs/19` § 7), keeping its panic and backtrace in
/// [`LastPiePanic`] so the status still shows them. Both editor loops call this.
pub fn stop_after_panic(world: &mut World, session: PieSession) {
    if let Some(message) = session.panic.clone() {
        world.insert_resource(LastPiePanic {
            message,
            backtrace: session.panic_backtrace.clone(),
        });
    }
    stop_play(session);
}

/// Mirrors `session` into the world's [`PieStatus`] so panels and agents see it.
pub fn sync_status(world: &mut World, session: Option<&PieSession>) {
    let status = match session {
        Some(session) => PieStatus {
            mode: Some(session.mode),
            entities: session.entities,
            frames: session.frames,
            start_elapsed_ms: u64::try_from(session.start_elapsed_ms).unwrap_or(u64::MAX),
            game: session.game.as_ref().map(|g| g.package.clone()),
            panic: session.panic.clone(),
            xr: crate::xr_settings::xr_status_line(session.app.world()),
            static_move_warnings: session
                .app
                .world()
                .get_resource::<crate::static_moves::StaticMoveWarnings>()
                .map_or(0, |warnings| warnings.count),
            kind: Some(session.options.kind),
            panic_backtrace: session.panic_backtrace.clone(),
            window: session.options.window,
            player_views: player_views(session.app.world()),
            time_scale: crate::pie_controls::time_scale(session.app.world()),
            muted: crate::pie_controls::muted(session.app.world()),
            start: session.options.start,
        },
        None => match world.get_resource::<LastPiePanic>() {
            Some(last) => PieStatus {
                panic: Some(last.message.clone()),
                panic_backtrace: last.backtrace.clone(),
                ..PieStatus::default()
            },
            None => PieStatus::default(),
        },
    };
    world.insert_resource(status);
    // The Profiler's play source shows the play world's entity counts (ADR-9740); counted only
    // while that source is selected and the panel is open.
    let wanted = world
        .get_resource::<crate::profiler_panel::ProfilerPanel>()
        .is_some_and(|p| p.open)
        && world
            .get_resource::<crate::profiler_source::ProfilerView>()
            .is_some_and(|v| *v.source() == crate::profiler_source::ProfilerSource::Pie);
    if wanted {
        let counts = session.map(|s| {
            crate::profiler_model::entity_counts(s.app.world(), crate::profiler_model::TOP_COMPONENTS)
        });
        if let Some(mut view) = world.get_resource_mut::<crate::profiler_source::ProfilerView>() {
            view.pie_counts = counts;
        }
        // The Profiler's module tabs sample the play world too (P-307, ADR-8141).
        if let Some(session) = session {
            crate::profiler_modules::note_play_world(world, session.app.world());
        }
    }
}

/// Applies one queued request against the session slot. Returns a short toast line for the
/// status bar. Used by the editor loop each frame and by the integration tests directly.
///
/// `game` is the game library to register when a session starts (the host's current one).
#[allow(clippy::needless_pass_by_value)] // the queue item is consumed, not borrowed
pub fn apply_request(
    world: &mut World,
    session: &mut Option<PieSession>,
    request: PieRequest,
    game: Option<CurrentGame>,
) -> String {
    match request {
        PieRequest::Play | PieRequest::PlayWith(_) => {
            let options = match request {
                PieRequest::PlayWith(options) => options,
                _ => PlayOptions::default(),
            };
            if let Some(old) = session.take() {
                crate::pie_worlds::end_before_restart(world);
                stop_play(old);
            }
            world.remove_resource::<LastPiePanic>();
            match start_play_with(world, game, options) {
                Ok(new) => {
                    let line = match options.kind {
                        PlayKind::Simulate => format!("▶ simulating ({} entities)", new.entities),
                        PlayKind::Play => format!("▶ playing ({} entities)", new.entities),
                    };
                    *session = Some(new);
                    line
                }
                Err(err) => {
                    tracing::error!(target: "regia_editor", "{err}");
                    err
                }
            }
        }
        PieRequest::Pause => match session.as_mut() {
            Some(session) if session.mode == PlayMode::Playing => {
                session.mode = PlayMode::Paused;
                "⏸ play paused".to_string()
            }
            _ => "play is not running".to_string(),
        },
        PieRequest::Resume => match session.as_mut() {
            Some(session) if session.mode == PlayMode::Paused => {
                session.mode = PlayMode::Playing;
                "▶ play resumed".to_string()
            }
            _ => "play is not paused".to_string(),
        },
        PieRequest::Stop => match session.take() {
            Some(old) => {
                let frames = old.frames;
                stop_play(old);
                format!("⏹ play stopped ({frames} frames)")
            }
            None => "play is not running".to_string(),
        },
        PieRequest::Step(frames) => match session.as_mut() {
            Some(sess) if sess.mode != PlayMode::Paused => {
                "play is not paused; pause it before stepping".to_string()
            }
            Some(sess) => match crate::pie_worlds::step_play(world, sess, frames) {
                Some(panic) => format!("⏸ play paused after a panic: {panic}"),
                None => format!("⏭ stepped {} frame(s)", frames.max(1)),
            },
            None => "play is not running".to_string(),
        },
        PieRequest::SetTimeScale(scale) => match session.as_mut() {
            Some(sess) => {
                crate::pie_controls::set_time_scale(sess.app.world_mut(), scale);
                format!("play speed {:.2}×", scale.get())
            }
            None => "play is not running".to_string(),
        },
        PieRequest::SetMute(muted) => match session.as_mut() {
            Some(sess) => {
                crate::pie_controls::set_mute(sess.app.world_mut(), muted);
                if muted {
                    "play audio muted"
                } else {
                    "play audio unmuted"
                }
                .to_string()
            }
            None => "play is not running".to_string(),
        },
        PieRequest::KeepSimulationChanges(guids) => {
            let Some(sess) = session.as_mut() else {
                world.insert_resource(LastKeep::Refused(crate::pie_keep::KeepError {
                    code: "not_playing",
                    message: "play stopped before the changes could be kept".to_string(),
                }));
                return "play is not running".to_string();
            };
            match crate::pie_keep::keep_simulation_changes(world, sess.app.world_mut(), guids.as_deref()) {
                Ok(report) => {
                    let line = format!(
                        "kept simulation changes: {} component(s) on {} entit(ies)",
                        report.components,
                        report.entities.len()
                    );
                    world.insert_resource(LastKeep::Kept(report));
                    line
                }
                Err(err) => {
                    let line = err.message.clone();
                    world.insert_resource(LastKeep::Refused(err));
                    line
                }
            }
        }
    }
}

/// Registers the game plugin into the play app under its generation's owner, before the level is
/// spawned. A panic is caught and logged, and answers `false`: the play world then runs without
/// game code (and without `on_load`), not dead (`specs/19` § 7).
fn register_game_plugins(app: &mut App, game: &CurrentGame) -> bool {
    let owner = crate::engine::ecs::prelude::TypeOwner::Game(game.generation);
    let register = game.exports.register;
    if let Err(caught) = catch_in_pie(|| {
        crate::hot_reload::with_game_owner(app, owner, register);
    }) {
        tracing::error!(
            target: "regia_editor",
            "game register panicked during PIE start: {}",
            caught.log_text()
        );
        return false;
    }
    true
}

/// Calls the registered game's `on_load` once the level is in the play world (threads restart
/// from the restored resources); Simulate caps local players first. A panic is caught and logged.
fn load_game(app: &mut App, game: &CurrentGame, kind: PlayKind) {
    if kind == PlayKind::Simulate {
        // Before `on_load`: a game that spawns its player there gets `None` from the manager.
        forbid_local_players(app.world_mut());
    }
    let on_load = game.exports.on_load;
    if let Err(caught) = catch_in_pie(|| on_load(app.world_mut())) {
        tracing::error!(
            target: "regia_editor",
            "game on_load panicked during PIE start: {}",
            caught.log_text()
        );
    }
}

/// Catches a panic in game code with its backtrace ([`crate::panic_trace::catch`], ADR-7205) and
/// files the crash report as `pie` (ADR-3931). The several-client worlds of `crate::pie_worlds`
/// run their game code through this too: a client panics on its own world, with its backtrace
/// (ADR-7340).
pub(crate) fn catch_in_pie(body: impl FnOnce()) -> Result<(), crate::panic_trace::CaughtPanic> {
    crate::crash_reports::with_pie_kind(|| crate::panic_trace::catch(body))
}

/// Entity count of the play world, taken around the game's register and `on_load` (which may
/// spawn) before the first settled frame. Resources, observers and registered systems are
/// entities in bevy_ecs 0.20 (`iter_entities` includes them); they are not what the game spawned,
/// so they are not counted.
fn world_entity_count(app: &App) -> usize {
    use crate::engine::ecs::ecs::observer::Observer;
    use crate::engine::ecs::ecs::resource::IsResource;
    use crate::engine::ecs::ecs::system::SystemIdMarker;
    app.world()
        .iter_entities()
        .filter(|e| {
            !e.contains::<IsResource>() && !e.contains::<Observer>() && !e.contains::<SystemIdMarker>()
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::layers::{LayerParked, LayerState, LayerTable, Side};
    use regia_scene::level::{Level, LevelLibrary, LoadMode, open_level};
    use regia_scene::prelude::{LevelId, Name, Transform};

    fn app_with_scene() -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let level_text = r#"
        (
            format_version: 1,
            entities: [
                (guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Origin",
                 components: { "regia_scene::transform::Transform": () }),
                (guid: "8b6f2a11-0000-4000-8000-000000000002", name: "Rock",
                 components: { "regia_scene::transform::Transform": () }),
            ],
        )
        "#;
        let id = LevelId(regia_core::id::Guid::new_random());
        let level = Level::from_ron_str(id, level_text).expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app
    }

    fn snapshot_text(app: &mut App) -> String {
        let filter = SnapshotFilter {
            level: None,
            resources: vec![LEVEL_SETTINGS_PATH.to_string()],
        };
        let file = snapshot_scene(app.world_mut(), &filter).expect("snapshot");
        file.to_ron_string().expect("ron")
    }

    #[test]
    fn play_runs_a_copy_and_stop_restores_the_edit_world_byte_identically() {
        let mut app = app_with_scene();
        // An editor camera move is edit-world state; play must not disturb it either.
        app.insert_resource(EditorCamera {
            distance: 4.0,
            ..EditorCamera::default()
        });
        let before = snapshot_text(&mut app);

        let mut session = start_play(app.world_mut(), None).expect("play");
        assert_eq!(session.mode, PlayMode::Playing);
        assert_eq!(session.entities, 2, "the play world holds the copy");
        let play_names = names(session.app.world_mut());
        assert!(play_names.contains(&"Rock".to_string()), "{play_names:?}");

        // Simulate: the play world advances; the edit world does not.
        session.frame();
        session.frame();
        assert_eq!(session.frames, 3, "one settled frame plus two simulated");

        // Edit the play world (edits during play are discarded on stop).
        session
            .app
            .world_mut()
            .spawn((Name::new("PlayOnly"), Transform::default()));

        stop_play(session);

        let after = snapshot_text(&mut app);
        assert!(
            before == after,
            "stop must restore the edit world byte-identically:\n--- before\n{before}\n--- after\n{after}"
        );
    }

    #[test]
    fn level_cameras_and_lights_reach_the_play_world() {
        let mut app = app_with_scene();
        crate::mesh_source::register_level_types(app.world_mut());
        app.world_mut().spawn((
            Name::new("Cam"),
            regia_scene::components::EntityGuid(regia_core::id::Guid::new_random()),
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 2.0, 6.0)),
            regia_render::Camera::default(),
        ));
        app.world_mut().spawn((
            Name::new("Bulb"),
            regia_scene::components::EntityGuid(regia_core::id::Guid::new_random()),
            Transform::default(),
            regia_render::PointLight::default(),
        ));
        let mut session = start_play(app.world_mut(), None).expect("play");
        let world = session.app.world_mut();
        let mut cameras = world.query::<(&Name, &regia_render::Camera)>();
        let cameras: Vec<String> = cameras.iter(world).map(|(n, _)| n.as_str().to_string()).collect();
        assert_eq!(cameras, vec!["Cam".to_string()], "the level camera is in play");
        let mut lights = world.query::<&regia_render::PointLight>();
        assert_eq!(lights.iter(world).count(), 1, "the light is in play");
    }

    #[test]
    fn step_while_playing_reports_that_nothing_ran() {
        let mut app = app_with_scene();
        let mut session = Some(start_play(app.world_mut(), None).expect("play"));
        let line = apply_request(app.world_mut(), &mut session, PieRequest::Step(3), None);
        assert!(line.contains("not paused"), "{line}");
        assert_eq!(session.as_ref().expect("session").frames, 1, "no frame ran");
    }

    #[test]
    fn pause_and_step_control_the_session() {
        let mut app = app_with_scene();
        let session = start_play(app.world_mut(), None).expect("play");
        assert_eq!(session.mode, PlayMode::Playing);

        // The request state machine through the shared apply path.
        let mut slot = Some(session);
        let world = app.world_mut();
        assert_eq!(
            apply_request(world, &mut slot, PieRequest::Pause, None),
            "⏸ play paused"
        );
        assert_eq!(slot.as_ref().expect("session").mode, PlayMode::Paused);
        let frames_before = slot.as_ref().expect("session").frames;
        // A paused session does not advance in `frame`.
        let mut session = slot.take().expect("session");
        assert!(session.frame().is_none());
        assert_eq!(session.frames, frames_before);
        assert!(session.step(3).is_none());
        assert_eq!(session.frames, frames_before + 3);
        slot = Some(session);
        assert_eq!(
            apply_request(world, &mut slot, PieRequest::Resume, None),
            "▶ play resumed"
        );
        assert_eq!(
            apply_request(world, &mut slot, PieRequest::Stop, None),
            "⏹ play stopped (4 frames)"
        );
        assert!(slot.is_none());
        // Requests against no session are clean refusals.
        assert_eq!(
            apply_request(world, &mut slot, PieRequest::Pause, None),
            "play is not running"
        );
    }

    #[test]
    fn paused_play_survives_a_reload_bracket_with_no_reload() {
        let mut app = app_with_scene();
        let mut session = start_play(app.world_mut(), None).expect("play");
        session.mode = PlayMode::Paused;
        // `start_play` already ran the settling first frame.
        let frames = session.frames;
        let previous = session.pause_for_reload();
        session.resume_after_reload(previous);
        assert_eq!(session.mode, PlayMode::Paused);
        assert_eq!(session.frame(), None);
        assert_eq!(
            session.frames, frames,
            "a paused session must not step on a redraw"
        );

        // A playing session is paused during the swap and resumes afterwards.
        session.mode = PlayMode::Playing;
        let previous = session.pause_for_reload();
        assert_eq!(session.mode, PlayMode::Paused);
        session.resume_after_reload(previous);
        assert_eq!(session.mode, PlayMode::Playing);
    }

    #[test]
    fn play_status_round_trips_for_the_agent_surface() {
        let mut app = app_with_scene();
        sync_status(app.world_mut(), None);
        assert_eq!(app.world().resource::<PieStatus>().mode, None);

        let session = start_play(app.world_mut(), None).expect("play");
        let status_session = PieSession {
            mode: PlayMode::Paused,
            frames: session.frames,
            start_elapsed_ms: session.start_elapsed_ms,
            entities: session.entities,
            started: session.started,
            app: session.app,
            game: session.game,
            panic: session.panic,
            panic_backtrace: session.panic_backtrace,
            options: session.options,
        };
        sync_status(app.world_mut(), Some(&status_session));
        let status = app.world().resource::<PieStatus>().clone();
        assert_eq!(status.mode, Some(PlayMode::Paused));
        assert_eq!(status.entities, 2);
        assert_eq!(status.game, None);
    }

    // The M11 acceptance (PIE start < 1 s for `samples/sim_bridge`) is counted, not timed, in
    // `tests/pie_start_work.rs` (ADR-7703): it needs the counting allocator of its own binary.

    /// An edit world with a level that binds `night` (initial `Absent`) and one member in it,
    /// shown in the edit world as an editor would.
    fn app_with_layers() -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let level_text = r#"
        (
            format_version: 1,
            entities: [
                (guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Ground",
                 components: { "regia_scene::transform::Transform": () }),
                (guid: "8b6f2a11-0000-4000-8000-000000000002", name: "Lamp",
                 components: {
                    "regia_scene::transform::Transform": (),
                    "regia_scene::layers::LayerMembership": (layers: ["night"]),
                 }),
            ],
            resources: {
                "regia_scene::layers::LevelLayers": (
                    embedded: [(format_version: 1, layers: [
                        (name: "night", kind: Runtime, debug_color: (0.1, 0.1, 0.5), parent: None),
                    ])],
                    entries: [
                        (layer: "night", initial: Absent, editor_visible: true, editor_resident: true, peers: Everyone),
                    ],
                ),
            },
        )
        "#;
        let id = LevelId(regia_core::id::Guid::from_u128(0x1e));
        let level = Level::from_ron_str(id, level_text).expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        app.world_mut()
            .resource_mut::<LayerTable>()
            .request("night", LayerState::Live, false, Side::Local)
            .expect("night");
        app.update();
        app
    }

    fn lamp_state(world: &mut World) -> Option<bool> {
        let mut query = world.query::<(&Name, Option<&LayerParked>)>();
        query
            .iter(world)
            .find(|(name, _)| name.as_str() == "Lamp")
            .map(|(_, parked)| parked.is_some())
    }

    #[test]
    fn an_absent_layer_in_play_despawns_its_members_and_brings_them_back() {
        let mut app = app_with_layers();
        assert_eq!(
            lamp_state(app.world_mut()),
            Some(false),
            "the editor shows the lamp"
        );
        let before = snapshot_text(&mut app);

        let mut session = start_play(app.world_mut(), None).expect("play");
        let world = session.app.world_mut();
        // Play starts from the initial states: the lamp is not spawned (not parked, ADR-2161).
        assert_eq!(lamp_state(world), None, "{:?}", names(world));
        assert!(names(world).contains(&"Ground".to_owned()));
        let table = world.resource::<LayerTable>();
        assert_eq!(table.despawned_count(), 1);
        assert_eq!(table.effective("night"), Some(LayerState::Absent));
        let level = world.resource::<LevelManager>().current();
        assert_eq!(
            level,
            Some(LevelId(regia_core::id::Guid::from_u128(0x1e))),
            "the open level's id"
        );

        let request = |session: &mut PieSession, state: LayerState| {
            session
                .app
                .world_mut()
                .resource_mut::<LayerTable>()
                .request("night", state, false, Side::Local)
                .expect("night");
            session.frame();
        };
        request(&mut session, LayerState::Live);
        assert_eq!(lamp_state(session.app.world_mut()), Some(false), "respawned live");
        request(&mut session, LayerState::Parked);
        assert_eq!(lamp_state(session.app.world_mut()), Some(true), "parked");
        request(&mut session, LayerState::Absent);
        assert_eq!(lamp_state(session.app.world_mut()), None, "despawned again");
        assert_eq!(session.app.world().resource::<LayerTable>().despawned_count(), 1);

        // A member gameplay spawned has no template record: it still parks.
        let spawned = session
            .app
            .world_mut()
            .spawn((
                Name::new("Spark"),
                Transform::default(),
                regia_scene::components::EntityGuid(regia_core::id::Guid::from_u128(0x99)),
                regia_scene::layers::LayerMembership {
                    layers: vec!["night".to_owned()],
                },
            ))
            .id();
        session.app.world_mut().resource_mut::<LayerTable>().mark_dirty();
        session.frame();
        assert!(session.app.world().get::<LayerParked>(spawned).is_some());

        stop_play(session);
        assert_eq!(before, snapshot_text(&mut app), "the edit world is untouched");
        assert_eq!(lamp_state(app.world_mut()), Some(false));
    }

    fn names(world: &mut World) -> Vec<String> {
        world
            .query::<&Name>()
            .iter(world)
            .map(|n| n.as_str().to_string())
            .collect()
    }

    fn no_register(_: &mut App) {}

    fn no_world_hook(_: &mut World) {}

    /// A game whose first frame panics: play cannot start.
    fn register_panicking_frame(app: &mut App) {
        app.add_systems(crate::engine::app::Update, panicking_system);
    }

    fn panicking_system() {
        panic!("the first play frame fails");
    }

    /// A game whose `on_load` spawns three entities.
    fn spawn_three(world: &mut World) {
        for i in 0..3 {
            world.spawn(Name::new(format!("GameSpawned{i}")));
        }
    }

    fn test_game(register: fn(&mut App), on_load: fn(&mut World)) -> CurrentGame {
        CurrentGame {
            exports: GameExports {
                register,
                on_unload: no_world_hook,
                on_load,
            },
            generation: 1,
            package: "pie_test".to_owned(),
        }
    }

    #[test]
    fn failed_play_keeps_the_edit_selection() {
        let mut app = app_with_scene();
        let rock = {
            let world = app.world_mut();
            let mut q = world.query::<(Entity, &Name)>();
            q.iter(world)
                .find(|(_, n)| n.as_str() == "Rock")
                .map(|(e, _)| e)
                .expect("rock")
        };
        let mut selection = Selection::default();
        selection.select(rock, crate::selection::SelectMode::Replace);
        app.insert_resource(selection);

        let game = test_game(register_panicking_frame, no_world_hook);
        let Err(err) = start_play(app.world_mut(), Some(game)) else {
            panic!("play started although its first frame panicked");
        };
        assert!(err.contains("first frame panicked"), "{err}");
        assert_eq!(
            app.world().resource::<Selection>().entities(),
            &[rock],
            "a failed play leaves the edit selection as it was"
        );

        // A play that starts does clear it (its ids are edit-world ids).
        let _session = start_play(app.world_mut(), None).expect("play");
        assert!(app.world().resource::<Selection>().is_empty());
    }

    #[derive(Resource, Default)]
    struct GameSettings;

    /// A game whose register adds a resource and an observer, and spawns nothing.
    fn register_resource_and_observer(app: &mut App) {
        app.insert_resource(GameSettings);
        app.world_mut().add_observer(|_: On<Add<Name>>| {});
    }

    #[test]
    fn play_entity_count_skips_the_games_resources_and_observers() {
        // bevy_ecs 0.20 stores resources and observers as entities: counting every entity
        // reported them as spawned by the game.
        let mut app = app_with_scene();
        let session = start_play(
            app.world_mut(),
            Some(test_game(register_resource_and_observer, no_world_hook)),
        )
        .expect("play");
        assert_eq!(
            session.entities, 2,
            "a resource and an observer are not spawned entities"
        );
    }

    #[test]
    fn play_entity_count_adds_only_what_the_game_spawned() {
        let mut app = app_with_scene();
        let session = start_play(app.world_mut(), Some(test_game(no_register, spawn_three))).expect("play");
        assert_eq!(session.entities, 2 + 3, "the scene's two plus the game's three");

        let mut app = app_with_scene();
        let session = start_play(app.world_mut(), Some(test_game(no_register, no_world_hook))).expect("play");
        assert_eq!(session.entities, 2, "a game that spawns nothing adds nothing");
    }
}
