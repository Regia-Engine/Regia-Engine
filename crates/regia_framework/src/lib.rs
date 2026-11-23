// SPDX-License-Identifier: MPL-2.0
//! Gameplay framework: sim bridge, scripts, game rules, players/controllers, characters,
//! cameras, save games.
//!
//! Specification: `docs/specs/14-scene-and-framework.md` and
//! `docs/specs/25-sim-bridge-and-do27.md` § 3.
//!
//! # How the pieces fit
//!
//! A game starts at [`GamePlugin`] — the unit of registration the launcher, the
//! Android shell and the hot-reload editor all load the same way (`export_game!` emits the dylib
//! symbols). Its [`register`](game::GamePlugin::register) adds engine plugins and the game's own
//! systems. Behaviour lives in [`Script`](script::Script)s attached to entities; flow is governed
//! by [`GameRules`](rules::GameRules) (spawning, win/lose), with any rule state kept in reflected
//! resources so it survives a reload. [`PlayerController`](player::PlayerController) drives a
//! [`Character`](character::Character) from a player's input and hands the
//! [`CameraRig`](camera::CameraRig) its view target. [`timers`] and [`tween`] schedule and animate
//! gameplay, and [`coroutine`]s sequence it over many frames; [`settings`] persists live-applied
//! configuration; [`save_game`] snapshots
//! [`Saveable`](save_game::Saveable) entities and [`SaveableResource`](save_game::SaveableResource)s
//! to disk; [`sim_bridge`] mirrors an authoritative external simulation. The one rule tying them
//! together: anything that must survive a hot reload is reflected state (a component, resource or
//! script), never a closure or a local.
//!
//! Modules:
//! - [`game`]: the [`GamePlugin`] entry trait and [`export_game!`] (§ B1).
//! - [`flow`]: the [`GameFlow`](flow::GameFlow) state machine (Boot → MainMenu → Loading →
//!   Playing ⇄ Paused) on `regia_ecs::state`.
//! - [`script`]: [`Script`](script::Script) behaviours, take-run-reinsert execution, messages and
//!   events (§ B2).
//! - [`camera`]: [`CameraRig`](camera::CameraRig) modes (chase lag/lean, overhead, first-person,
//!   free) and the [`CameraDirector`](camera::CameraDirector) (§ B5).
//! - [`shots`]: camera paths and shot sequences (`.shots.ron`): keyed moves, cuts, fades, field of
//!   view and depth of field, sampled deterministically from a time.
//! - [`sequencer`]: the cinematics sequencer data model (`.seq.ron`): one master clock of named
//!   tracks (a float, vec3 or bool channel keyed onto a target's property) and markers, sampled
//!   deterministically from a time (ADR-1507).
//! - [`timers`] and [`tween`]: gameplay utilities (§ B6).
//! - [`coroutine`]: `async` gameplay sequences polled once per frame, owner-bound like timers (§ B6).
//! - [`settings`]: [`GameSettings`](settings::GameSettings), persisted and applied live (§ B7).
//! - [`calibration_screen`]: the player display calibration screen (black level, gamma, HDR).
//! - [`feedback`]: the in-game feedback prompt, bug-report bundles and the automatic crash bundle.
//! - [`sim_bridge`]: external-simulation bridge (`specs/25` § 3).
//! - [`sim_host`]: a threaded fixed-step host for an external sim that publishes its last two
//!   frames as one pair (ADR-8083).
//! - [`player`]: players, controllers, split-screen layout (§ B3).
//! - [`rules`]: the [`rules::GameRules`] trait, default rules (§ B3).
//! - [`character`]: character movement state machine, pluggable collision query (§ B4).
//! - [`save_game`]: save/load with atomic writes, migration, autosave (§ B7).
//! - [`save_flow`]: save stores, save requests, quicksave, blobs and slot info (spec 91).
//! - [`character_control`]: [`MoveTo`](character_control::MoveTo) and the camera-relative stick mapping (spec 91).
//! - [`damage`]: the [`damage::Health`]/[`damage::DamageEvent`]/[`damage::Died`] helper (§ B6).
//! - [`pool`]: [`pool::PrefabPool`] for frequent spawns such as projectiles (§ B6).
//! - [`input_recording`]: input recording/playback for deterministic replays (`06` § 6).
//! - [`glass_breach`]: the ground-coupled sound of a `regia_vfx` glass breach.
//! - [`level_assets`]: levels in either form with their layer definitions and placed levels.
//! - [`level_switch`]: a level switch with cancel; one level read at a time, the replaced one freed on a worker.
//! - [`start_level`]: `regia run --level L` opens `L` after the game's startup and re-opens
//!   it when the file is saved.
//! - [`skinning`]: [`SkinnedMeshPlugin`](skinning::SkinnedMeshPlugin) hands each
//!   `regia_animation::SkinnedMesh` joint palette to the renderer (Tincal C1).
//! - [`debug_gizmos`]: physics-collider, audio-emitter and nav-mesh debug overlays drawn with
//!   `regia_render::gizmos` (disabled until toggled, e.g. by the agent's `debug.set_gizmos`).
//! - [`tags`]: hierarchical gameplay tags (`Combat.Damage.Fire`), tag sets, queries and the
//!   `*.tags.ron` table.
//! - [`baked_gi_level`]: installs the open level's baked GI volume (`LevelLook::baked_gi`).
//! - `lightmap_level`: installs the open level's lightmap (`LevelLook::lightmap`), attaches its
//!   receivers for every mesh part and asks the renderer to warm the lightmap variants.
//! - [`curve_table`]: named float curve tables (`.curves.ron`, `.curves.csv`) sampled by name
//!   (spec 51, ADR-1649).
//! - [`chooser`]: context-driven result tables (`.chooser.ron`) with gates, costs, weighted draws
//!   and a decision trace; per-entity bindings (spec 51, ADR-1649).
//! - [`abilities`]: attributes, effects, abilities and cues (`docs/specs/53-abilities-attributes-effects.md`).
//! - [`demo_world`] and [`demo_scrub`]: record, replay and scrub a whole ECS world (`DemoTracked`
//!   entities, demo resources) with a runtime seek bar (ADR-1878); [`demo`] holds the file format.
//!
//! [`export_game!`] is re-exported at the crate root; `regia_framework::Script` names the
//! `#[derive(Script)]` macro (the [`Script`](script::Script) trait lives in [`script`]).
//!
//! The default feature `client` owns the modules that need a player's machine (rendering, audio,
//! UI, cameras, feedback, settings); without it the simulation modules build alone, for the
//! dedicated-server build (spec 76 § 2.1).
//!
//! See `book/user-guide/gameplay.md` (players, controllers, rules, scripts, save games) and
//! `book/agents/framework.md`.

pub mod abilities;
#[cfg(feature = "client")]
pub mod auto_quality;
#[cfg(feature = "client")]
pub mod baked_gi_level;
#[cfg(feature = "client")]
pub mod baked_gi_stream;
#[cfg(feature = "client")]
pub mod bird;
#[cfg(feature = "client")]
pub mod buoyancy;
// The blast gust bends the terrain grass on the shake's clock (ADR-1497).
#[cfg(feature = "client")]
pub mod blast_gust;
#[cfg(feature = "client")]
pub mod calibration_screen;
#[cfg(feature = "client")]
pub mod camera;
// The camera as a physical actor: wake, push, spray, proximity, clearance (ADR-1502).
#[cfg(feature = "client")]
pub mod camera_body;
pub mod character;
pub mod character_control;
pub mod chooser;
pub mod chunk_level_warnings;
#[cfg(feature = "client")]
pub mod conversation;
pub mod coroutine;
pub mod curve_table;
pub mod damage;
#[cfg(feature = "client")]
pub mod debug_gizmos;
pub mod demo;
#[cfg(feature = "client")]
pub mod demo_scrub;
pub mod demo_world;
#[cfg(feature = "net")]
pub mod destruction_net;
// Sound, VFX and decals for destruction from a `.destructionfx.ron` table (ADR-2734); playback
// needs the `destruction-fx` feature.
pub mod destruction_fx;
pub mod explosion;
#[cfg(feature = "client")]
pub mod exposure_zones;
#[cfg(feature = "client")]
pub mod extension;
#[cfg(feature = "client")]
pub mod feedback;
pub mod flow;
#[cfg(feature = "functional-tests")]
pub mod functional;
pub mod game;
// Scoreboard from the game rules, lobby list from `regia_net` (ADR-2958).
#[cfg(feature = "client")]
pub mod game_ui;
#[cfg(feature = "client")]
pub mod glass_breach;
pub mod ground_shock;
pub mod input_recording;
pub mod level_assets;
pub mod level_switch;
#[cfg(feature = "client")]
pub mod meshlet_bridge;
#[cfg(feature = "client")]
pub mod meshlet_stream;
#[cfg(feature = "client")]
pub mod motion_matching_gizmos;
pub mod nav_world;
#[cfg(feature = "net")]
pub mod net_bots;
#[cfg(feature = "net")]
pub mod net_character;
#[cfg(feature = "net")]
pub mod net_replay;
#[cfg(feature = "net")]
pub mod net_session_bridge;
#[cfg(all(feature = "net", feature = "client"))]
pub mod net_stats_overlay;
#[cfg(feature = "net")]
pub mod net_vehicle;
#[cfg(feature = "client")]
pub mod perf_overlay;
pub mod player;
pub mod pool;
pub mod ragdoll;
pub mod replay_sim;
pub mod rules;
pub mod save_flow;
pub mod save_game;
pub mod script;
#[cfg(feature = "client")]
pub mod sea_weather;
#[cfg(feature = "client")]
pub mod season;
pub mod sequencer;
#[cfg(feature = "client")]
pub mod settings;
#[cfg(feature = "client")]
pub mod shock_shake;
pub mod shots;
pub mod sim_bridge;
#[cfg(feature = "client")]
pub mod skinning;
pub mod spectator;
pub mod start_level;
pub mod state_hash;
pub mod table_source;
pub mod tags;
#[cfg(feature = "client")]
pub mod thermal;
pub mod timers;
pub mod tween;
// Cloth and soft-body glue (spec 55, ADR-2083): draws through the renderer, so client only.
#[cfg(feature = "client")]
pub mod cloth;
// The dialogue line player (spec 36 § 7.3, ADR-1882).
#[cfg(feature = "client")]
pub mod dialogue;
// A level's lightmap, its receivers and the warm request on level open (ADR-2313).
#[cfg(feature = "client")]
pub mod lightmap_level;
// The chunk draw path of destruction level 2: draws broken destructibles with their cooked chunk meshes.
#[cfg(feature = "client")]
pub mod destruction_draw;
// Platform-neutral leaderboards: the game-facing resource, the service seam and an in-memory
// service (spec 24 § 2a, ADR-2340).
pub mod leaderboard;
// Boot movies from `Regia.toml` and the `skip_video` action (specs/50 § 2.7, ADR-2410).
#[cfg(feature = "media")]
pub mod boot_movies;
// Footsteps from animation foot events: surface lookup, speed from the stride (ADR-2442).
// Bird calls heard where the birds are seen: binds the audio's bird calls to the visible birds of
// `regia_vfx`'s ambient life (ADR-3024).
#[cfg(feature = "client")]
pub mod footsteps;
// Wheel smoke, dust, spray, skid marks and ruts from a vehicle's wheels (ADR-3274).
#[cfg(feature = "client")]
pub mod vehicle_fx;
// Networked rigid bodies: server-driven pose, velocity and sleep; kinematic proxies (ADR-2902).
#[cfg(feature = "net")]
pub mod net_body;
// Character input to the motion-matching trajectory predictor (spec 52 § 2.6a, ADR-2858).
pub mod motion_matching_character;
// Fixed-wing aircraft: the fixed-step aero update, input mapping and rate-damping assist (ADR-3374).
pub mod aircraft;
// AI drivers for physical vehicles on lane graphs: pure pursuit, a speed plan, IDM following, stuck
// recovery (ADR-3117).
pub mod vehicle_ai;
// Publishes the open level's local reflection capture bake (`.fcap`, ADR-2860).
#[cfg(feature = "client")]
pub mod local_capture_level;
// A dedicated server's world kept across restarts: autosave, rotation, fallback restore, per-player
// state (ADR-3354).
#[cfg(feature = "net")]
pub mod server_persistence;
// Fixed-count scalar kernels for deterministic simulation paths (ADR-4645).
mod detmath;

// Secondary rigid debris spawn cap (ADR-3929). The caller spawns; this module only admits.
pub mod destruction_debris;

// Vehicle audio: engine layers, tires, wind and the voice budget from physics vehicles (ADR-3271).
#[cfg(feature = "client")]
pub mod vehicle_audio;

// Vehicle feedback: steering torque and wheel jolts to gamepad rumble (ADR-3276).
pub mod vehicle_feedback;

// Swimming, ladders, mantles and vaults, and the nav bridge (`docs/specs/traversal.md`, ADR-2971).
pub mod traversal;
// Bird calls heard where the birds are seen: the vfx flocks joined to audio (ADR-3024). Client
// only: it joins the optional `regia_vfx` and `regia_audio`, which the server build compiles
// without (ADR-7386, ADR-7490).
#[cfg(feature = "client")]
pub mod ambient_bird_calls;
// Damaged player files are moved aside to `<name>.bad-<n>`, never overwritten (ADR-3102).
pub mod user_file;
// Stable entity references in save games: save ids, reflection remap, format 1 conversion (ADR-3239).
pub mod save_entity_refs;
// Startup playback of a `.frec` recording for packaged smoke runs (ADR-3968).
pub mod startup_playback;
// What made this session's replays not reproducible (an extension hot reload, ADR-9850).
pub mod session_determinism;
// Per-system lockstep desync hashes of ECS components and resources, in entity-id order (ADR-8080).
pub mod desync_hash;
// Leashed photo mode, HUD hide and tick-named captures (ADR-8415).
pub mod photo_mode;

pub use game::{GameExports, GamePlugin, GamePluginEntry, abi_hash, abi_hash_from};
pub use regia_macros::Script;

/// Everything a game normally needs from this crate. `Script` appears in both namespaces: the
/// trait ([`script::Script`]) and the derive macro.
pub mod prelude {
    #[cfg(feature = "client")]
    pub use crate::calibration_screen::{
        CalibrationScreen, CalibrationScreenClosed, CalibrationScreenPlugin,
    };
    #[cfg(feature = "client")]
    pub use crate::camera::{
        CameraDirector, CameraMode, CameraPlugin, CameraRig, CameraSet, ChaseCamera, FreeLookInput,
    };
    pub use crate::character::{
        Character, CharacterCollisionQuery, CharacterInput, CharacterMovement, CharacterPlugin,
        CharacterState, CollisionQuery, FlatPlaneQuery, MoveResult, MovementMode, RootMotionApply,
        RootMotionDriven, RotationMode,
    };
    pub use crate::character_control::{CharacterControlPlugin, MoveTo, MoveToArrived, camera_relative_move};
    pub use crate::coroutine::{Co, Coroutine, CoroutinePlugin, Coroutines, start_coroutine};
    pub use crate::damage::{
        DamageEvent, DamagePlugin, Died, DiedEvent, FriendlyFire, Health, SelfDamage, ShotFired,
        SpawnProtected, SpawnProtection, damage,
    };
    #[cfg(feature = "client")]
    pub use crate::debug_gizmos::DebugGizmosPlugin;
    pub use crate::export_game;
    #[cfg(feature = "client")]
    pub use crate::feedback::{
        Feedback, FeedbackCategory, FeedbackConfig, FeedbackHost, FeedbackPlugin, FeedbackWritten,
        open_prompt, spawn_feedback_button,
    };
    pub use crate::flow::{GameFlow, GameFlowPlugin, GameFlowSettings, InGame, LoadingTasks, PausePage};
    pub use crate::game::{GamePlugin, GamePluginEntry, abi_hash};
    pub use crate::input_recording::{InputRecorder, InputRecording, InputRecordingPlugin, RecorderMode};
    pub use crate::player::{
        AiController, GAMEPLAY_CONTEXT, MaxLocalPlayers, Player, PlayerController, PlayerKind, PlayerManager,
        PlayerPlugin, PlayerStart, SpawnPoint, SplitScreenLayout,
    };
    pub use crate::pool::{Pooled, PooledInactive, PrefabPool};
    pub use crate::ragdoll::{Ragdoll, RagdollBone, RagdollHit, RagdollMode, RagdollPlugin, RagdollProfile};
    pub use crate::replay_sim::{FullCodec, ReplayError, ReplayFile, ReplaySim};
    pub use crate::rules::{
        DefaultRules, GameRules, GameRulesRegistry, GameRulesResource, LedgerSlot, RulesCtx, RulesPlugin,
        SpawnRequest, TeamId, score_sheet,
    };
    pub use crate::save_flow::{
        FileSaveStorage, QuickSaveConfig, SaveBlobs, SaveFlowPlugin, SaveInfo, SaveOp, SaveOutcome,
        SaveRequest, SaveStorage, SaveStore,
    };
    pub use crate::save_game::{
        MigrationCtx, RegisterSaveableExt, SaveDirectory, SaveError, SaveGame, SaveHeader, SaveMigrations,
        SavePlugin, SaveResult, SaveSlot, SaveSpec, Saveable, SaveableResource, SaveableResources,
    };
    pub use crate::script::{
        RegisterScriptExt, Script, ScriptCtx, ScriptEvent, ScriptPlugin, ScriptRegistry, ScriptUiEvent,
        ScriptUiValue, Scripts,
    };
    #[cfg(feature = "client")]
    pub use crate::settings::{CalibrationSettings, GameSettings, SettingsPath, SettingsPlugin};
    pub use crate::shots::ShotSample;
    pub use crate::shots::ShotSequence;
    #[cfg(feature = "client")]
    pub use crate::shots::{ShotPlayer, ShotPlugin};
    pub use crate::sim_bridge::{
        BridgeInput, EventRouter, ExternalSim, KindRegistry, SimBridgePlugin, SimEntityMap, SimEntityView,
        SimEvent, SimId, SimSnapshot,
    };
    #[cfg(feature = "client")]
    pub use crate::skinning::SkinnedMeshPlugin;
    pub use crate::timers::{TimerHub, TimerPlugin};
    pub use crate::tween::{Tween, TweenPlugin};
    pub use regia_macros::Script;
    // Spec 51 and 53 (ADR-1649): plugins, components, request messages and app extensions only.
    pub use crate::abilities::{
        AbilitiesLocked, AbilityBook, AbilityInput, AbilityPlugin, AbilitySignal, AppliedEffects,
        ApplyEffect, Attributes, CancelAbility, HealthBridge, StartAbility, TagCounts,
    };
    pub use crate::chooser::{
        ChooserAppExt, ChooserBinding, ChooserBridgePlugin, ChooserPicked, ChooserPlugin,
    };
    pub use crate::curve_table::{CurveTableAppExt, CurveTablePlugin, CurveTables};
}

// Contact sounds from physics: impacts, scrapes, rolls, breaks, footstep surfaces (ADR-3270).
#[cfg(feature = "client")]
pub mod physics_audio;
// Quadratic wind drag on rigid bodies from the scene wind (ADR-3348).
pub mod physics_wind;

#[cfg(test)]
mod test_alloc;
// A level's load progress for loading screens and `level.status` (spec 14 § A4, ADR-7203).
pub mod level_progress;
// `[runtime]` of `Regia.toml`: pause on focus loss, keep the screen on (spec 11 §§ 2-3, ADR-7070).
pub mod project_runtime;
// The project's loading-screen prefab, shown while a `Replace` level loads (spec 14 § A4, ADR-7263).
pub mod loading_screen;
// A threaded fixed-step host for an external sim with a two-frame publish (ADR-8083).
pub mod sim_host;

/// Navigation path debug drawing shared with the editor (client builds only).
#[cfg(feature = "client")]
pub mod path_debug;

#[cfg(any(feature = "visual-log", test))]
pub mod visual_log;

// Physics ground probe, grounded flag and respawn reset for `FootPlacement` (ADR-8388).
pub mod foot_probe;
// Install-tier residency caps (`tier.ron` version 1, ADR-8036).
pub mod install_tier;

/// Shipping visual-log call: discards every argument without evaluation or type checking.
/// The recorder module is absent unless the `visual-log` feature is enabled.
///
/// ```compile_fail
/// use regia_framework::visual_log::VisualLog;
/// ```
#[cfg(not(any(feature = "visual-log", test)))]
#[macro_export]
macro_rules! debug_log {
    ($($ignored:tt)*) => {{}};
}
// Per-viewer fog of war over the sim bridge (plan row S-16, ADR-8154).
pub mod fog_view;

// Packaged startup defaults and their origins (ADR-7893).
#[cfg(any(feature = "client", feature = "media"))]
pub mod packaged_config;

/// Local log, crash, feedback and pipeline-cache artifact discovery.
#[cfg(feature = "client")]
pub mod report_sources;
// The sim bridge's handedness setting and the winding flip (plan row P-02, ADR-8456).
pub mod sim_coords;
// The sim bridge's per-kind blend policy and presentation settings (plan row P-02, ADR-8456).
pub mod sim_blend;
// Fog of war over the network: the fog relevance filter (audit gap FW-5).
#[cfg(feature = "net")]
pub mod fog_relevance;
// Fog of war presentation: the per-viewer fog texture (audit gap FW-1).
pub mod fog_texture;

#[cfg(feature = "client")]
pub mod loading_widget;
// The editor's dev launch options on a device: start level, arguments, agent transport (ADR-8472).
pub mod dev_launch;

// Debug draw of the deterministic simulation indexes (ADR-8562).
pub mod det_debug;
// Lockstep hover hulls in an engine-native app: component, FixedUpdate system, terrain floor (plan row mp-hover-ecs-agent).
// Its `hull` submodule is the `.hoverhull.ron` v1 file (ADR-8512), re-exported here (ADR-8637).
pub mod det_hover;
// Spawn-point selection over the ECS world: team sides, policies, blocked and sight tests (ADR-8647).
pub mod spawn_select;
// Rule sets as content: `.rules.ron` v1, the shipped defaults and the app's registry (ADR-8694).
pub mod ruleset_file;
// Hover hulls in regia_net's predicted schedule (spec 73 § 9.2, ADR-8819).
#[cfg(feature = "net")]
pub mod net_hover;
// Finished-match records, sinks and the match history folder (audit gaps GR-10.1 to GR-10.4).
pub mod match_results;

// The `locomotion` debug overlay: gait, foot placement and leg IK gizmos (ADR-8729).
#[cfg(feature = "client")]
pub mod locomotion_gizmos;
// The game pause key and the built-in "Paused" screen, `[game] pause` in Regia.toml (ADR-10104).
#[cfg(feature = "client")]
pub mod game_pause;
