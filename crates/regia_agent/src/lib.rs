// SPDX-License-Identifier: MPL-2.0
//! Agent protocol server: the JSON-RPC 2.0 surface over the engine for AI agents and tools
//! (MCP adapter in `regia_cli`).
//!
//! Specification: `docs/specs/23-agent-native.md`; transport: ADR-014. [`AgentPlugin`] starts a
//! loopback WebSocket server, writes `<project>/.regia/agent/endpoint.json` and a bearer token,
//! and answers requests at frame boundaries. Every method of the protocol is described in
//! [`schema`] — the MCP tool list and `agent.schema` are generated from the same table, so they
//! cannot drift apart.
//!
//! The runtime handlers live in private area modules: `discovery` (§ 3.1), `scene` (§ 3.2),
//! `assets` (§ 3.3), `seeing` (§ 3.4), `playtest` (§ 3.5), `events` (§ 3.7) and `framework`
//! (specs/14 M9); `safety` holds the permission profiles, audit log and provenance records
//! (§ 5). Editor-side tools (import, cook, staged review) arrive with the editor surface.
//!
//! # How the pieces fit
//!
//! [`AgentPlugin`] starts a loopback WebSocket server at `finish`, writes the endpoint/token into
//! `.regia/agent/`, and injects an [`AgentInbox`]. Each frame `PreUpdate` drains requests and
//! routes them to the area handlers — `play.*` ([`AgentTimeControl`] drives pause, step, time
//! scale, `wait_until`, `expect`, input injection) and `seeing.*` (screenshots, image comparison)
//! are the two most-used groups. [`schema`] describes every method and feeds `agent.schema`,
//! `agent.hello` and the MCP tool list from one table.
//!
//! See `book/user-guide/agent-protocol.md` and `book/agents/protocol.md`.

mod ai_reasoners;
mod ai_squads;
mod anim;
mod assets;
mod assets_texture_sets;
mod audio;
mod audio_convolution;
mod audio_emitters;
mod audio_loudness;
mod audio_meters;
mod audio_mix;
// The live program-loudness meter (ADR-7227).
mod audio_music;
mod audio_program_loudness;
mod audio_zones;
pub mod bake_export;
mod baked_gi_visibility;
mod bindless_report;
mod calibration;
mod camera_cut;
mod camera_raycast;
pub mod capabilities;
pub mod client;
mod clouds;
mod convex_decomposition;
#[cfg(test)]
mod coverage_calls;
mod crash;
mod demo;
mod discovery;
mod dispatch;
mod draw_integrity;
pub mod endpoint;
mod events;
mod explosion;
mod exposure;
mod exposure_settings;
pub mod extension;
mod feedback;
mod fire_shock;
mod forward_lite;
mod frame_gen;
mod framework;
mod functest;
mod output_dither;
mod sun_shadow_range;
/// The services the functional-test runner needs from the agent: a play session installs them too.
pub use functest::hooks as functional_test_hooks;
mod blue_noise;
mod frame_pipelining;
mod gi_debug;
mod gi_denoise;
mod gi_mode;
mod gizmos;
mod glass;
mod hair;
mod hitch;
mod hwrt;
mod image;
mod import_report;
mod inspector;
mod level_baked_gi;
mod level_baked_gi_stream;
mod lightmaps;
mod loading;
mod look;
mod mask_decals;
mod material;
mod material_clock;
mod material_rough_transmission;
mod material_shading;
mod material_thin_film;
mod material_vertex_ao;
mod memory;
mod mist;
mod mobile;
mod morph;
mod motion_matching;
mod nav;
mod nav_authoring;
mod nav_lanes;
mod nav_links_stream;
mod nav_world;
mod oit;
mod pacing;
mod params;
mod perf_overlay;
mod physics;
mod pick;
mod playtest;
pub mod plugins;
mod primitive;
mod probe_specular;
pub mod protocol;
mod quality;
mod quick;
mod quick_app;
mod reference;
mod render;
mod render_out;
mod render_profile;
mod render_stats;
mod replay;
mod rig;
mod runloop;
mod runtime;
mod safety;
mod scene;
pub mod schema;
mod seeing;
pub mod server;
mod shadow_stability;
mod shots;
mod skinning;
mod smart_object;
mod smart_object_v2;
mod spacemouse;
mod spatial_query;
pub(crate) mod sprite2d;
mod state_hash;
mod states;
mod subsurface;
// `render.set_light_profile`: IES light profiles (ADR-2592).
mod light_profiles;
mod tags;
mod telemetry;
mod temporal_chart;
mod temporal_quality;
mod terrain;
mod terrain_parity;
mod texture_sets;
// Bindless step 5 (ADR-1110): texture table growth and GPU residency feedback.
mod debris;
mod texture_residency;
mod ui;
mod ui_a11y;
mod ui_a11y_audit;
mod ui_a11y_platform;
mod ui_accessibility;
mod ui_icons;
mod ui_layers;
// `ui.screens` (book/agents/ui-screens.md, ADR-1563 l).
mod conversation;
mod dialogue_ui;
mod input_rebind;
mod ui_inspect;
mod ui_list;
mod ui_localization;
mod ui_motion;
mod ui_nav;
mod ui_parity;
mod ui_parity2;
// Lobby list, scoreboard, radar, world markers, target brackets (book/agents/ui-game-widgets.md, ADR-2958).
mod ui_game_widgets;
mod ui_screens;
mod ui_shortcuts;
mod ui_text;
mod ui_text_shape;
mod ui_theme_mode;
mod ui_tween;
mod upscalers;
mod vfx;
mod visbuffer;
mod weather;
mod weather_effects;
// Scroll containers (book/agents/ui-scroll.md, ADR-520).
mod ui_scroll;
// `render.swapchain` and `platform.paths` (ADR-525).
mod paths;
mod swapchain;
// `render.set_gi_sampling`: ZH3 or L1 evaluation of the baked GI SH (ADR-1377).
mod gi_sampling;
// Split baked GI volumes and the brick-loop probe (ADR-2618).
mod baked_gi_volumes;
// Per-method contract: example, error codes, limits, cost (ADR-1034).
pub mod contract;
// Froxel fog local-light pick (ADR-1365).
mod fog_lights;
mod fog_shadow;
// Draw counters and skinned instancing (book/agents/skinned-instancing.md, ADR-1368).
mod skinned_stats;
// Water bodies in the runtime frame (book/agents/water.md, ADR-1399).
mod water;
// Grass field (book/agents/terrain-grass.md, ADR-1400).
mod terrain_grass;
mod vfx_crash;
// The Cinematic offline profile's `render.set_profile` overrides and tier gate (ADR-1403).
mod render_cinematic;
// Terrain ground variation and late-winter surfaces (book/agents/terrain-late-winter.md, ADR-1408).
mod ground_look;
// The haze's Ångström colour (book/agents/low-sun.md, ADR-1408).
mod aerosol;
// The daylight grey-card calibration (book/agents/daylight-calibration.md, ADR-1425).
mod daylight;
// Terrain debug views on a running frame (book/agents/terrain-debug-views.md, ADR-1447).
mod terrain_debug;
// Post volumes and the physical camera (book/agents/post-volumes.md, ADR-1464).
mod post_volumes;
// The frame profiler: flame capture, the call tree, capture files (ADR-2323).
mod profiler;
// Montages, sync groups and motion warping (book/agents/montages.md, ADR-1465).
mod anim_montage;
mod anim_warps;
// Data tables: list, read, edit, validate, export (book/agents/data-tables.md, ADR-1466).
mod tables;
// Asset registry queries without loading assets (book/agents/asset-registry.md, ADR-1466).
mod registry;
// Localization: locale, fallback chain, resolve, pseudo, validator, plural (book/agents/localization.md, ADR-1466).
mod loc_tools;
mod localization;
// `physics.cloth_stats` for the XPBD cloth solver (book/agents/cloth.md).
pub mod breadcrumbs;
pub mod cloth;
// `abilities.*`: attributes, effects and abilities (book/agents/abilities.md, spec 53).
pub mod abilities;
pub mod trace;
// Ray-cast vehicles (book/agents/vehicles.md).
mod vehicles;
// Hover craft and `.vehicle.ron` (book/agents/vehicles.md, ADR-2301).
mod hovercraft;
// Scoped callers: out-of-process tools on their own tokens (book/agents/editor-tools.md, ADR-1523).
#[cfg(test)]
mod plugins_tests;
pub mod scope;
#[cfg(test)]
mod scope_tests;
// Voice capture and loopback (book/agents/voice.md, specs/16 § 3b).
mod voice;
mod voice_chat;
// World streaming and HLOD data model (book/agents/world-streaming.md, ADR-1566).
mod stream;
mod world_streaming;
// The OpenXR seam on the agent protocol (book/agents/xr.md, ADR-1571).
mod xr;
// The frame graph: passes, slots and registered host passes (book/agents/render-graph.md, ADR-1552).
mod render_graph;
// The `.render_job.ron` job file of sequence-driven render-out (book/agents/render-jobs.md, ADR-1616).
pub mod render_job;
// Sequence players: .seq.ron + .shots.ron playback and camera takeover (book/agents/sequence-player.md, ADR-1614).
mod sequence_player;
// Per-camera shake sensors and plain shakes (book/agents/camera-shake.md, ADR-1619).
mod camera_framework;
mod camera_interior;
mod camera_rail;
mod camera_shake;
pub mod panel_calls;
mod showcase_camera;
mod walk_bob;
// Per-object shadow LOD hints (book/agents/shadow-lod-hint.md, ADR-1622).
mod shadow_lod_hint;
// The shader-permutation policy (book/agents/permutation-policy.md, ADR-2026).
mod render_permutations;
// `curves.*` over curve tables (book/agents/curve-tables.md, spec 51, ADR-1649).
pub mod curve_tables;
// `chooser.*` over chooser tables and bindings (book/agents/choosers.md, spec 51, ADR-1649).
pub mod choosers;
// Named terrain holes and runtime height patches (book/agents/terrain-runtime.md, ADR-1595).
mod terrain_runtime;
// Mesh-layer seam report (book/agents/terrain-mesh-layers.md, ADR-8291).
mod terrain_seam;
// Heightmap export/import and the shared wind (book/agents/terrain-editing.md, ADR-1853).
mod terrain_heights;
// Tilemap writes and auto-tiling rules (book/agents/2d.md, ADR-1889).
mod tilemap_edit;
// Painted mesh instances (ADR-1936).
mod terrain_scatter_paint;
// Destruction level 2 on the agent protocol (book/agents/destruction.md, ADR-1597).
mod destruction;
// Networking status, peers and the network conditioner (book/agents/net.md, ADR-1600).
mod net;
// Lockstep desync diagnosis: net.desync, net.desync_inject (book/agents/net-desync.md, ADR-8455).
mod net_desync;
// Lockstep match replays: net.replays, net.replay_audit (book/agents/net-lsreplay.md, ADR-8513).
mod net_lsreplay;
// Published external replication stats: net.external_stats (book/agents/net-external-replication.md, ADR-8892).
mod net_external_stats;
// The net model and replication layer (book/agents/net-model.md, net-replication.md, ADR-1643).
mod net_model;
// State-tree compile / control / reload / debug (book/agents/state-tree.md, spec 38, ADR-1639).
mod logic;
mod state_tree;
// Behavior tree get / edit / validate / live (book/agents/behavior-tree-editor.md, ADR-2408).
mod ai_bt;
// The studio gradient / equirect environment (ADR-1653).
mod studio_environment;
// Foliage wind and the dynamic-caster shadow class (book/agents/foliage-wind.md, ADR-2028).
mod foliage_wind;
// `fuzz.targets` / `fuzz.run` (book/agents/fuzzing.md, ADR-1917).
mod fuzz;
pub mod fuzz_targets;
mod studio_lighting;
// Procedural generators and graphs (book/agents/procgen.md, ADR-1640).
mod procgen;
// Net relevance, priority and budgets (book/agents/net-relevance.md, ADR-1662).
mod net_relevance;
// `net.replay_*` (book/agents/net-replay.md, ADR-3021).
mod net_replay;
// Net proxy interpolation (book/agents/net-interpolation.md, ADR-1662).
mod net_interp;
// The net state channel and send-loop scale (book/agents/net-state-channel.md, ADR-1950).
mod net_state;
// The entropy stage's per-connection counters (book/agents/net-entropy.md, spec 72 § 18, ADR-3315).
mod net_entropy;
// Content chunks, DLC and chunk delivery (book/agents/content-chunks.md, spec 40, ADR-1667).
mod content;
// Level directories, data layers and placed levels (book/agents/levels-layers-instances.md, spec 37).
mod level_tools;
// The session lifecycle and the dedicated-server host (book/agents/net-session.md,
// net-dedicated-server.md, ADR-1663).
mod net_server;
// A dedicated server's kept world: server.save_now, server.persistence_status, server.restore
// (book/agents/net-server-persistence.md, ADR-3354).
mod net_server_persistence;
mod net_session;
// Spline components (book/agents/splines.md, specs/64 § 2.3, ADR-1664).
mod splines;
// Biome areas, claims, statistics and definitions (book/agents/biomes.md, ADR-1665).
mod biome;
mod procgen_tiles;
// Wave Function Collapse on an inline tileset or sample (book/agents/procgen.md).
mod procgen_wfc;
// Schedule profiling: a generator's graph at chosen thread counts (book/agents/procgen.md, ADR-2724).
mod procgen_profile;
// Video playback (specs/50, ADR-1670, book/agents/video-playback.md).
mod video;
// The video renderer half: frame source state and transport controls (ADR-2044, book/agents/media-render.md).
mod media;
// Save stores, quicksave, save status and character move-to (book/agents/save-flow.md, spec 91, ADR-1691).
mod save_flow;
// Character controller polish and the ragdoll budget (book/agents/character-physics.md, ADR-1693).
mod character_physics;
// Active ragdolls: PD motors, balance and get-up (book/agents/ragdoll-active.md, ADR-2498).
mod ragdoll_active;

// Traversal moves: swim, ladders, mantle and vault (book/agents/character-traversal.md, ADR-2971).
mod character_traversal;
// Dynamic GI tiers over the SDF-traced probes (book/agents/dynamic-gi.md, ADR-1679).
mod dynamic_gi;
// Net client prediction (book/agents/net-prediction.md, ADR-1793).
mod net_predict;
// Net lag compensation (book/agents/net-lag-compensation.md, ADR-1752).
mod net_lagcomp;
// Montage authoring: describe, edit, slot tables, reverse and time-stretch (spec 34, ADR-1870).
mod anim_montage_edit;
// Retarget quality report and crowd animation status (book/agents/retarget-crowds.md, ADR-1857).
mod anim_crowd;
// Runtime lens effects: ghost flare, dirt, chromatic aberration (book/agents/lens-effects.md, ADR-1863).
mod lens_effects;
// The crash fireball as a level camera sees it (book/agents/crash-fireball.md, ADR-1498).
mod vfx_crash_fireball;
mod vfx_systems;
// Two-phase occlusion of the depth prepass (book/agents/occlusion.md, ADR-1887).
mod occlusion;
mod radiance_cache;
// Lobbies and matchmaking (book/agents/net-lobbies.md, ADR-1877).
mod net_lobby;
// Networked characters: moving bases and root motion (book/agents/net-character.md, ADR-1905).
mod net_character;
mod net_vehicle;
// Networked hover hulls (book/agents/net-hover.md, ADR-8819).
mod net_hover;
// Dialogue lines (book/agents/dialogue-subtitles.md, ADR-1882).
mod dialogue;
// Cue sets on the caption board (book/agents/dialogue-subtitles.md, ADR-1882).
mod subtitle_sets;
// The destruction chunk draw path (book/agents/destruction-draw.md).
mod destruction_draw;
// Start-up work and idle memory: `app.startup_work`, `memory.idle_ledger` (book/agents/startup-idle.md, ADR-2987).
pub mod startup_idle;
// Pak update diffs: what shipping new paks over old ones costs (book/agents/content-chunks.md,
// spec 22 T2.14, ADR-3322).
mod pak_diff;
// Sight acquire and lose cones (book/agents/perception-hysteresis.md, ADR-4586).
mod perception_hysteresis;
// The camera as a physical actor and the startled bird (book/agents/camera-body.md, ADR-1502).
mod bird;
mod camera_body;
// The local transport (spec 23 § 2): a Unix domain socket or a Windows named pipe beside the
// WebSocket listener, with the platform seam in `platform/` (book/agents/local-transport.md,
// ADR-5708).
pub mod local;
mod platform;
// Forest flythrough: placement queries, corridor clearance and carve, motion LOD, wake sway (book/agents/forest-flythrough.md, ADR-1503).
mod forest;
// Covered overview flags for one viewer (book/agents/covered-overview.md, ADR-8333).
mod unit_view;
// Presentation physics counters (book/agents/presentation-physics.md, ADR-8356).
mod presentation_physics;
// Dev versus package access traces (book/agents/package-parity.md, ADR-8428).
mod package_parity;
// Leashed photo mode (book/agents/photo-mode.md, ADR-8415).
mod photo;
// In-world AI debug layers (book/agents/ai.md, ADR-8463).
mod debug_layers;

use std::path::PathBuf;

use regia_app::prelude::*;

// The stats overlay and bandwidth view JSON, shared with the editor's play-in-editor methods.
pub use discovery::{GUIDE_TOPICS, GuideTopic, guide_mentions, mentions_token};
pub use dispatch::AgentInbox;
pub use dispatch::{LocalReply, call_local};
pub use events::AgentEventLog;
pub use hitch::{HitchDrive, HitchSessions};
pub use net_session::{stats_value as net_stats_value, traffic_value as net_traffic_value};
pub use playtest::AgentTimeControl;
pub use procgen::{PlantExportHook, ProcGraphDocRoute, ProcGraphRouteDeny};
pub use safety::params_hash;
pub use safety::{AgentConfig, AgentPermissions};
pub use safety::{AuditEntry, AuditLog, AuditObserver, ChangeLog, unix_ms_now};
pub use splines::{SplineEdit, SplineEditLog};
pub use ui::{UiAgentHost, UiHostCall, UiHostError};

/// Agent server configuration.
///
/// # Example
///
/// ```
/// use regia_agent::AgentSettings;
///
/// let settings = AgentSettings::default();
/// assert!(settings.project_dir.is_none()); // derive the project directory automatically
/// ```
#[derive(Resource, Debug, Clone, Default)]
pub struct AgentSettings {
    /// Project directory for `.regia/agent/` (see [`endpoint::resolve_project_dir`]).
    pub project_dir: Option<PathBuf>,
}

/// Where this app advertised its endpoint (removed on exit).
#[derive(Resource, Debug, Clone)]
pub struct AgentEndpoint {
    /// The `.regia/agent` directory.
    pub dir: PathBuf,
    /// The WebSocket URL.
    pub url: String,
}

/// Installs the built-in extension methods on the world's
/// [`AgentExtension`](extension::AgentExtension) (ADR-1809): `curves.*` (spec 51 curve tables),
/// `chooser.*` (spec 51 choosers), `abilities.*` (spec 53), `physics.cloth_stats`, and (ADR-2307)
/// `render.breadcrumbs` plus the `telemetry.mark`/`range_open`/`range_close`/`trace`/
/// `screenshot_mark` trace methods (spec 54). They live on
/// the extension (ADR-1649) rather than the runtime schema table, and answer
/// `{ available: false }` (`{ plugin: false }` for cloth) in an app without their plugin.
/// [`AgentPlugin`] calls this; calling it again (or a module's own installer) replaces the rows
/// with the same ones.
pub fn install_builtin_extensions(world: &mut World) {
    curve_tables::install_curve_table_methods(world);
    choosers::install_chooser_methods(world);
    abilities::install_ability_methods(world);
    cloth::install_cloth_methods(world);
    // GPU breadcrumbs and the trace recorder (spec 54, ADR-2307): registering costs nothing per
    // frame; the fault injector stays with test hosts (`breadcrumbs::install_breadcrumb_methods`).
    breadcrumbs::install_breadcrumb_read_methods(world);
    trace::install_trace_methods(world);
}

/// The names of the methods [`install_builtin_extensions`] registers, in registration order
/// (`cargo xtask agent-coverage` checks their guide topic, book row, tests and contract).
#[must_use]
pub fn builtin_extension_methods() -> Vec<&'static str> {
    let mut world = World::new();
    install_builtin_extensions(&mut world);
    world
        .get_resource::<extension::AgentExtension>()
        .map(extension::AgentExtension::method_names)
        .unwrap_or_default()
}

/// The name of the schedule the agent drains its request inbox in every frame
/// ([`dispatch::process_requests`], added to `PreUpdate` in [`AgentPlugin`]). While the
/// system-access record is still pending, `kinds::describe` names this schedule as `unread`
/// (spec 101 § 6.3) — one constant names it at both sites, and the debug-assert at the dispatch
/// site plus `kinds::tests::dispatch_schedule_name_matches` fail if they ever diverge.
pub(crate) const AGENT_DISPATCH_SCHEDULE: &str = "PreUpdate";

/// Starts the protocol server. Included in `regia::DefaultPlugins` with the `agent` feature
/// (dev builds).
#[derive(Default)]
pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn build(&self, app: &mut App) {
        // `render.sequence` plays shot sequences on game time (no-op when a game added it).
        regia_framework::shots::player::install(app);
        // `sequence.player` runs sequence players and their camera takeover (no-op when a game added it).
        regia_framework::sequencer::player::install(app);
        // The resource inspector reads render targets back: they need COPY_SRC, which only a
        // dev build with the agent adds (`regia_render::resource_inspector`).
        regia_render::set_debug_readback(true);
        // Curve tables, choosers, abilities and cloth: extension methods every agent app lists
        // (ADR-1809); a host's own `AgentExtension` rows are added on top of these.
        install_builtin_extensions(app.world_mut());
        app.init_resource::<AgentSettings>()
            .init_resource::<AgentTimeControl>()
            .init_resource::<hitch::HitchSessions>()
            .init_resource::<render_out::RenderOutJobs>()
            .init_resource::<dispatch::RateLimit>()
            .init_resource::<safety::AgentSession>()
            .init_resource::<safety::ChangeLog>()
            .init_resource::<events::AgentEventLog>()
            .init_resource::<AppTypeRegistry>();
        // Off unless `Regia.toml [dcc_feed]` enables it; costs one atomic load when idle.
        #[cfg(feature = "dcc-feed")]
        app.add_plugins(dcc_feed::DccFeedPlugin::default());
        // `kinds::describe` names this schedule as `unread` while the system-access record is
        // pending, so `AGENT_DISPATCH_SCHEDULE` must match the label below (spec 101 § 6.3).
        debug_assert_eq!(format!("{:?}", PreUpdate), AGENT_DISPATCH_SCHEDULE);
        app
            // `quit.request` writes a close message and reads the policy; a host without
            // `PlatformPlugin` (editor, headless tools) still needs both registered (ADR-1582).
            .add_message::<regia_platform::WindowCloseRequested>()
            .init_resource::<regia_platform::ClosePolicy>()
            .add_systems(
                PreUpdate,
                // Requests that queue UI input (`ui.click`) must land before the UI drains its
                // bridge in the same frame; the drain is conditional, so without this edge it can
                // be skipped before the queue is filled and the input arrives a frame late.
                (
                    dispatch::process_requests,
                    // Scoped tokens (out-of-process tools, ADR-1523): no-op without a ScopedInbox.
                    dispatch::process_scoped_requests,
                )
                    .chain()
                    .before(regia_ui::agent_bridge::drain_bridge),
            )
            // `replay.play` on a player that has no sim bridge (ADR-1519).
            .add_systems(PreUpdate, replay::drive_unbridged_replay)
            .add_systems(
                Last,
                (
                    playtest::advance_step,
                    playtest::advance_waiters,
                    procgen_tiles::advance_tile_waits,
                    hitch::advance_hitch_report,
                    seeing::advance_captures,
                    render_out::advance_render_out,
                    loading::advance_offscreen_warm,
                    events::record_events,
                ),
            )
            // `input.pointer_claims` watches (ADR-7976): idle until a watch is armed.
            .add_systems(
                Last,
                input_pointer_claims::record_pointer_claim_watch
                    .run_if(resource_exists::<input_pointer_claims::PointerClaimWatch>),
            )
            // A `render.out_job` seeks the shot player to its render clock before the player
            // drives the camera (ADR-1616).
            .add_systems(
                PostUpdate,
                render_out::drive_job_shots.before(regia_framework::shots::ShotDriveSet),
            )
            // ... and every external-clock sequence player before the sequencer advances (ADR-1614).
            .add_systems(
                PostUpdate,
                render_out::drive_job_sequences.before(regia_framework::sequencer::SequencerSet::Advance),
            )
            .add_systems(
                PostUpdate,
                stream::draw_overlay
                    .run_if(stream::overlay_on)
                    .run_if(resource_exists::<regia_render::gizmos::GizmoConfigStore>),
            )
            .add_systems(Lifecycle, remove_endpoint_on_exit)
            .add_systems(Last, remove_endpoint_on_headless_exit);
        // `project.describe` runs inside `PreUpdate`: `Last` records that schedule's system access
        // once, after the first request asks for it (spec 101 § 6.3). Idle until then.
        app.init_resource::<regia_scene::kind::systems::SystemAccessRecord>()
            .add_systems(
                Last,
                regia_scene::kind::systems::record_system_access
                    .run_if(regia_scene::kind::systems::record_pending),
            );
        // `app.startup_work`'s snapshot and `memory.idle_ledger`'s frame count (ADR-2987).
        if !app.has_plugin(std::any::type_name::<startup_idle::StartupWorkPlugin>()) {
            app.add_plugins(startup_idle::StartupWorkPlugin);
        }
    }

    fn finish(&self, app: &mut App) {
        let settings = app.world().resource::<AgentSettings>().clone();
        let (dir, project) = match endpoint::resolve_project_dir(settings.project_dir.as_deref()) {
            Some(project) => (endpoint::agent_dir(&project), Some(project)),
            None => match default_data_dir(app) {
                Some(logs) => (logs.with_file_name("agent"), None),
                None => {
                    tracing::warn!(target: "regia_agent", "no project or data directory; agent server disabled");
                    return;
                }
            },
        };

        // Permissions (.regia/agent.toml) and the audit log live in the project (specs/23 § 5).
        if let Err(err) = endpoint::prepare_dir(&dir) {
            tracing::warn!(target: "regia_agent", "cannot create {}: {err}", dir.display());
        }
        let permissions = match &project {
            Some(project) => safety::AgentPermissions::load(project),
            None => safety::AgentPermissions::default(),
        };
        if permissions.config.read_only {
            tracing::info!(target: "regia_agent", "read_only mode: mutating calls will be denied");
        }
        app.insert_resource(permissions)
            .insert_resource(safety::AuditLog::open(dir.join("audit.jsonl")));
        if let Some(observer) = app.world().get_resource::<safety::AuditObserver>().copied() {
            (observer.ready)(app.world_mut());
        }

        // `plugin.*` read the project's extensions even when the game linked none of them. A game
        // with `regia::register_extensions!` installs the plugin itself (specs/57 § 3.1).
        if let Some(project) = &project
            && !app
                .world()
                .contains_resource::<regia_framework::extension::ExtensionRegistry>()
        {
            let mut plugin =
                regia_framework::extension::ExtensionPlugin::for_project(project.clone(), Vec::new());
            plugin.reserved_ids = Some(plugins::reserved_namespaces());
            plugin.build(app);
        }

        // A dev launch on a device (ADR-8472, dev builds only) names the loopback port the editor
        // forwards to and the one-time token it made; otherwise a random port and a new token.
        let dev_agent = app
            .world()
            .get_resource::<regia_framework::dev_launch::DevLaunch>()
            .and_then(|launch| launch.agent.clone())
            .filter(|_| regia_framework::dev_launch::ENABLED);
        let (token, port) = match dev_agent {
            Some(agent) => (agent.token, agent.port),
            None => (endpoint::new_token(), 0),
        };
        let server = match server::start_at(token.clone(), port) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(target: "regia_agent", "agent server not started (port {port}): {err}");
                return;
            }
        };
        // The local transport (spec 23 § 2): a Unix domain socket or a Windows named pipe in
        // the agent directory, same messages, no TCP port. Failure never takes the WebSocket
        // listener down; an unwritable advertisement just leaves the transport undiscoverable.
        let local = local::start(token.clone(), &dir, Some(server.progress.clone()));
        let url = format!("ws://{}", server.addr);
        let advertised = protocol::Endpoint {
            url: url.clone(),
            pid: std::process::id(),
            protocol: protocol::PROTOCOL_VERSION.into(),
            token_file: dir.join("token").display().to_string(),
            app: regia_platform::paths::exe_stem(),
        };
        if let Err(err) = endpoint::write(&dir, &advertised, &token) {
            tracing::warn!(target: "regia_agent", "cannot write {}: {err}", dir.display());
        }
        match local {
            Ok(Some(local)) => {
                if let Err(err) = endpoint::write_socket(&dir, &local.address) {
                    tracing::warn!(target: "regia_agent", "local transport not advertised: {err}");
                }
                app.insert_resource(local);
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(target: "regia_agent", "local transport not started: {err}");
            }
        }
        let endpoint_file = dir.join("endpoint.json");
        tracing::info!(
            target: "regia_agent",
            "agent protocol {} listening on {url} (endpoint: {})",
            protocol::PROTOCOL_VERSION,
            endpoint_file.display()
        );
        app.insert_resource(AgentInbox(server.inbox))
            .insert_resource(server.progress)
            .insert_resource(AgentEndpoint { dir, url });
    }
}

/// Fallback location when there is no project directory: next to the session logs.
pub(crate) fn default_data_dir(app: &App) -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        regia_platform::paths::default_log_dir()
    }
    #[cfg(target_os = "android")]
    {
        let android = app.world().get_resource::<regia_platform::AndroidAppHandle>()?;
        regia_platform::paths::default_log_dir(&android.0)
    }
}

/// Scratch output root for agent artifacts that have no explicit `out_dir` — theme previews,
/// WAV renders. The system temp directory on desktop; on Android the app's internal data
/// directory, because `std::env::temp_dir()` there falls back to `/data/local/tmp`, owned by
/// the shell user, which an app uid cannot create directories in (audit
/// sprint-audit-g-agent7-platform, finding 1: `ui.theme_preview` and `audio.render_check`
/// failed on device with `PermissionDenied`).
pub(crate) fn scratch_out_dir(world: &World, leaf: &str) -> PathBuf {
    scratch_out_dir_from(android_data_dir(world).as_deref(), leaf)
}

/// [`scratch_out_dir`]'s pure choice, split out so the Android shape is testable on any host:
/// an app-private data directory wins; without one, the system temp directory.
fn scratch_out_dir_from(android_data: Option<&std::path::Path>, leaf: &str) -> PathBuf {
    match android_data {
        Some(dir) => dir.join(leaf),
        None => std::env::temp_dir().join(leaf),
    }
}

/// The running activity's app-private internal data path on Android, `None` on other targets
/// and before the platform plugin saw the activity (the temp-dir fallback then stays).
#[cfg(target_os = "android")]
fn android_data_dir(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<regia_platform::AndroidAppHandle>()
        .and_then(|handle| handle.0.internal_data_path())
}

#[cfg(not(target_os = "android"))]
fn android_data_dir(_world: &World) -> Option<PathBuf> {
    None
}

fn remove_endpoint_on_exit(
    lifecycle: Res<regia_platform::AppLifecycle>,
    endpoint: Option<Res<AgentEndpoint>>,
) {
    if lifecycle.last_event == Some(regia_platform::LifecycleEvent::WillTerminate)
        && let Some(endpoint) = endpoint
    {
        // Only our own advertisement: a successor started during the overlap keeps its file.
        endpoint::remove_if_advertising(&endpoint.dir, &endpoint.url);
        endpoint::remove_socket_if_ours(&endpoint.dir);
    }
}

#[cfg(test)]
mod scratch_out_dir_tests {
    use super::{scratch_out_dir, scratch_out_dir_from};
    use std::path::Path;

    /// audit-agent7-platform (M): on Android `std::env::temp_dir()` falls back to the
    /// shell-owned `/data/local/tmp`, which an app uid cannot write; an app-private data dir
    /// must win. The choice is pure so the Android shape is testable on every host.
    #[test]
    fn app_private_data_wins_over_the_system_temp_dir() {
        let dir = scratch_out_dir_from(
            Some(Path::new("/data/user/0/com.example/files")),
            "regia-theme-preview",
        );
        assert_eq!(
            dir,
            Path::new("/data/user/0/com.example/files").join("regia-theme-preview")
        );
        assert!(!dir.starts_with("/data/local/tmp"), "{dir:?}");
    }

    #[test]
    fn without_a_platform_data_dir_the_system_temp_dir_is_used() {
        let dir = scratch_out_dir_from(None, "regia_audio_mix");
        assert_eq!(dir, std::env::temp_dir().join("regia_audio_mix"));
        // Desktop contract: the reply's `path`/`out_dir` stays under the system temp dir.
        let world = regia_app::prelude::World::new();
        assert_eq!(scratch_out_dir(&world, "regia_audio_mix"), dir);
    }
}

/// `Last`: an app without a window runner ends its run loop on the update that writes [`AppExit`]
/// (`quit.request`, `--exit-after-frames`) and never sees the window runner's `WillTerminate`, so
/// it removes its own endpoint here; clients then find no app instead of a refused connection
/// (ADR-5131). That is a headless renderer (`HeadlessRender(true)`), and an agent host without
/// the render plugin (a dedicated server, a CLI tool) unless a window runner gave it a primary
/// window. Windowed apps keep removing it at `WillTerminate` (an editor may still cancel a quit).
fn remove_endpoint_on_headless_exit(
    mut exits: MessageReader<AppExit>,
    headless: Option<Res<regia_render::HeadlessRender>>,
    windows: Query<(), With<regia_platform::PrimaryWindow>>,
    endpoint: Option<Res<AgentEndpoint>>,
) {
    if exits.read().count() == 0 {
        return;
    }
    let windowed = match headless {
        Some(headless) => !headless.0,
        None => !windows.is_empty(),
    };
    if windowed {
        return;
    }
    if let Some(endpoint) = endpoint {
        endpoint::remove_if_advertising(&endpoint.dir, &endpoint.url);
        endpoint::remove_socket_if_ours(&endpoint.dir);
    }
}

// The DCC scene feed (book/agents/dcc-feed.md, specs/60, ADR-1669, ADR-1891).
#[cfg(feature = "dcc-feed")]
pub mod dcc_feed;
mod dcc_feed_surface;
pub use scene::require_not_feed_owned;

// Confining file paths from requests (ADR-1816).
pub mod req_path;
// Post-volume format v3: per-volume LUT switch and gizmo outlines (book/agents/post-volumes.md, ADR-2323).
mod post_volume_extras;
pub use image::base64 as base64_encode;
pub use render_out::{BOARD_ANSWER_AFTER, RenderOutBoard, RenderOutFileSync, SyncFile, run_product_job};
pub use replay::ReplayRoots;
pub use seeing::GpuDisabled;
// The editor's Montage timeline calls the `anim.montage_*` handlers directly (ADR-2031).
pub use anim_montage_edit::{MontageEditHistory, montage_describe, montage_edit};
// The Montage panel's slot-table editor calls the slot handlers directly (sprint w9-anim-authoring).
pub use anim_montage_edit::{montage_slot_edit, montage_slots};
// The forward shader's frame-level studio axis (book/agents/forward-studio-axis.md, ADR-2308).
mod render_forward_studio;
// The terrain's caster proxy for the ray-traced sun (book/agents/hwrt.md, ADR-2310).
mod terrain_trace_proxy;
// Per-material foliage sway (book/agents/foliage-wind.md, ADR-2304).
mod material_sway;
// `assets.collision_report`: cooked `collision_*` proxies of a glTF (ADR-2341).
pub mod collision_report;
// Ambience that matches the picture: wind bed, point spots, footsteps (book/agents/audio-ambience.md, ADR-2442).
mod audio_ambience;
// `assets.voxel_far`: a cooked voxel far proxy's levels, memory and switch distances (ADR-2445).
pub mod voxel_far;
// The stochastic SSR cost probe and cost bound (book/agents/ssr-cost.md, ADR-2557).
mod ssr_cost;
// Ambient life: motes, insects, leaves and birds (book/agents/ambient-life.md, ADR-3024).
mod ambient_life;

// Spec 23 agent-protocol gaps (ADR-2346): long-running jobs (`agent.job_status`, `agent.job_cancel`),
// `agent.run_tests`, `agent.open_in_editor`, staged review (`review.*`) and the request side of
// `regia mcp --http`.
#[cfg(test)]
mod gaps_tests;
pub mod jobs;
pub mod mcp_http;
pub mod open_in_editor;
pub mod review;
pub mod run_tests;

// Spec 23 § 3.6 code methods (ADR-5707): `agent.build_game` and the agent side of
// `agent.hot_reload` (the editor hook lives in `regia_editor::code_reload`).
pub mod build_game;
pub mod hot_reload;

// `agent.golden_test`, `agent.bless_golden` and the owner-only accept behind `regia golden accept`
// (ADR-2821).
pub mod golden_tools;

// An exiting app removes only its own advertised endpoint (audit-concurrency-1103 agent M4).
#[cfg(test)]
mod endpoint_exit_tests {
    use super::*;

    fn advertised(url: &str, pid: u32) -> protocol::Endpoint {
        protocol::Endpoint {
            url: url.into(),
            pid,
            protocol: protocol::PROTOCOL_VERSION.into(),
            token_file: String::new(),
            app: "test".into(),
        }
    }

    fn exiting_app(dir: &std::path::Path, url: &str) -> App {
        let mut app = App::new();
        app.insert_resource(regia_platform::AppLifecycle {
            last_event: Some(regia_platform::LifecycleEvent::WillTerminate),
            ..Default::default()
        })
        .insert_resource(AgentEndpoint {
            dir: dir.to_path_buf(),
            url: url.into(),
        })
        .add_systems(Update, remove_endpoint_on_exit);
        app
    }

    /// During a restart overlap the successor has already advertised itself; the old instance's
    /// exit used to delete the successor's `endpoint.json`, so clients found no app.
    #[test]
    fn exit_keeps_a_successors_endpoint_and_removes_its_own() {
        let dir = regia_test_utils::unique_temp_dir("agent-endpoint-exit");
        let ours = "ws://127.0.0.1:40001";
        let successor = "ws://127.0.0.1:40002";
        endpoint::write(
            &dir,
            &advertised(successor, std::process::id().wrapping_add(1)),
            "next",
        )
        .expect("successor writes");
        let mut app = exiting_app(&dir, ours);
        app.update();
        let (kept, token) = endpoint::read(&dir).expect("the successor's endpoint is still there");
        assert_eq!(kept.url, successor);
        assert_eq!(token, "next");

        endpoint::write(&dir, &advertised(ours, std::process::id()), "ours").expect("we write");
        app.update();
        assert!(
            !dir.join("endpoint.json").exists(),
            "an app's own endpoint is removed at exit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn headless_app(dir: &std::path::Path, url: &str, headless: bool) -> App {
        let mut app = App::new();
        app.add_message::<AppExit>()
            .insert_resource(regia_render::HeadlessRender(headless))
            .insert_resource(AgentEndpoint {
                dir: dir.to_path_buf(),
                url: url.into(),
            })
            .add_systems(Update, remove_endpoint_on_headless_exit);
        app
    }

    /// ADR-5131 (demo defect 6): a headless app's exit left its `endpoint.json` behind, so the
    /// next client got "connection refused". It is removed on the update that writes `AppExit`;
    /// a windowed app keeps it until `WillTerminate`, and nothing is removed without an exit.
    #[test]
    fn a_headless_exit_removes_its_own_endpoint() {
        let dir = regia_test_utils::unique_temp_dir("agent-endpoint-headless-exit");
        let ours = "ws://127.0.0.1:40003";
        endpoint::write(&dir, &advertised(ours, std::process::id()), "ours").expect("we write");
        let mut windowed = headless_app(&dir, ours, false);
        windowed.world_mut().write_message(AppExit::Success);
        windowed.update();
        assert!(
            dir.join("endpoint.json").exists(),
            "the window runner removes it later"
        );

        let mut app = headless_app(&dir, ours, true);
        app.update();
        assert!(dir.join("endpoint.json").exists(), "no exit, no removal");
        app.world_mut().write_message(AppExit::Success);
        app.update();
        assert!(
            !dir.join("endpoint.json").exists(),
            "removed at the headless exit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An agent host without the render plugin (a dedicated server, a CLI tool): no
    /// `HeadlessRender`, and a primary window only when a window runner made one.
    fn render_less_app(dir: &std::path::Path, url: &str, window: bool) -> App {
        let mut app = App::new();
        app.add_message::<AppExit>()
            .insert_resource(AgentEndpoint {
                dir: dir.to_path_buf(),
                url: url.into(),
            })
            .add_systems(Update, remove_endpoint_on_headless_exit);
        if window {
            app.world_mut()
                .spawn((regia_platform::Window::default(), regia_platform::PrimaryWindow));
        }
        app
    }

    /// fixrev-demo-warm-k (review finding 6): the exit removal keyed on `HeadlessRender(true)`,
    /// so an agent host without the render plugin (no window runner, no `WillTerminate`) still
    /// left its `endpoint.json` behind. Without a window it is removed at its exit; a render-less
    /// app that has a window keeps it until `WillTerminate`, like a windowed renderer.
    #[test]
    fn a_render_less_agent_host_removes_its_own_endpoint_at_exit() {
        let dir = regia_test_utils::unique_temp_dir("agent-endpoint-render-less-exit");
        let ours = "ws://127.0.0.1:40004";
        endpoint::write(&dir, &advertised(ours, std::process::id()), "ours").expect("we write");
        let mut windowed = render_less_app(&dir, ours, true);
        windowed.world_mut().write_message(AppExit::Success);
        windowed.update();
        assert!(
            dir.join("endpoint.json").exists(),
            "a window runner removes it later"
        );

        let mut app = render_less_app(&dir, ours, false);
        app.update();
        assert!(dir.join("endpoint.json").exists(), "no exit, no removal");
        app.world_mut().write_message(AppExit::Success);
        app.update();
        assert!(
            !dir.join("endpoint.json").exists(),
            "removed at the render-less exit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
// physics.time_of_impact: angular CCD sweep query (ADR-2423).
mod physics_toi;
// The particle liquid's `fluid.*` methods (book/agents/fluid.md, ADR-2441).
pub mod fluid;
// The trust-on-first-use store of server keys (book/agents/net-trust.md, ADR-2416).
mod net_trust;
// The terrain editing remainder: the level's terrain, grass density, scatter rules (ADR-2411).
mod terrain_level;
// Boot movies, text cues, visibility pause, decode pool and file validation (ADR-2410,
// book/agents/video-boot-and-cues.md).
mod media_boot;
// The read-only `mods://` file system: mods.list/mount/unmount/order/hash/read (book/agents/mods.md, ADR-2418).
mod mods;
// Product viewer contracts: configurator.contract_check / contract_schema (book/agents/configurator-contracts.md, ADR-2480).
mod configurator;
// Product configurator runtime: configurator.open_product/options/set_pick/build/resolve (book/agents/configurator.md, ADR-2534).
mod configurator_runtime;
// Product viewer drag view and packages: viewer.view_simulate / pack_verify (book/agents/viewer-view.md, ADR-2540).
mod viewer_view_pack;
// The product viewer's renderer: viewer.capabilities/render_still/env_info (book/agents/viewer-renderer.md, ADR-2539).
mod viewer;
// The product viewer's cheap looks: looks.list/material/cut/render_board (book/agents/product-looks.md, ADR-2538).
mod looks;
// XR walk mode (book/agents/xr-walk.md, ADR-2457).
mod xr_walk;
// Stochastic many-light sampling: the evaluation pass and the CPU reference
// (book/agents/many-light-sampling.md, ADR-2470).
mod many_light;
// Entity kinds and the project map: kinds.list/of/lint, project.describe (book/agents/entity-kinds.md, ADR-2488).
mod kinds;
// The shared cook cache: cook.cache_stats (book/agents/cook-cache.md, ADR-2612).
mod cook_cache;
// Wheel surface effects: vfx.vehicle_fx, vfx.vehicle_fx_set (book/agents/vehicle-fx.md, ADR-3274).
mod vehicle_fx;
// Player accessibility assists (ADR-2646): input.assists, audio.mono, render.color_vision.
mod audio_mono;
mod input_assists;
mod input_haptics;
mod render_color_vision;
// Culture formatting preview: ui.format_preview (book/agents/ui-localization.md, ADR-2698).
mod ui_format;
// `render.frame_work`: device-independent frame work counts (book/agents/frame-work.md, ADR-2711).
mod frame_work;
// Trailers: physics.trailers, trailer_couple, trailer_uncouple, trailer_config_check (ADR-3281).
mod trailers;
// Networked rigid bodies (book/agents/net-bodies.md, ADR-2902).
mod net_body;
// `ui.components`: reusable .ui.ron components (book/agents/ui-components.md, ADR-3147).
mod ui_components;
// Contact-scale calibration opt-ins: render.get/set_contact_calibration (book/agents/contact-calibration.md, ADR-3202).
mod contact_calibration;
// Fixed-wing aircraft: physics.aircraft*, `.aircraft.ron` (book/agents/aircraft.md, ADR-3374).
mod aircraft;
// AI drivers for physical vehicles on lane graphs: vehicle_ai.drive_to/status/set_params/stop
// (book/agents/vehicle-ai.md, ADR-3117).
mod vehicle_ai;
// Local reflection captures as level content: `.fcap` save, load and status (ADR-2860).
mod reflection_captures;
// ReSTIR GI over hardware ray queries (book/agents/restir-gi.md, ADR-3349).
mod restir_gi;

// Secondary rigid debris spawn cap: physics.debris_budget (book/agents/destruction-debris.md, ADR-3929).
mod destruction_debris;

// Vehicle audio: audio.vehicle_audio, audio.vehicle_audio_set, audio.vehicle_audio_probe (book/agents/vehicle-audio.md, ADR-3271).
mod vehicle_audio;

// Cell sleep for a speed grid: fluid.sleep_mask (book/agents/fluid.md, ADR-3949).
mod fluid_sleep;

// Two-wheel balance: physics.bike_step and physics.bike_state (book/agents/bike.md, ADR-4017).
mod bike;

// `platform.power_state`: host battery and AC supply (book/agents/platform-power.md, ADR-3997).
mod platform_power;
// `window.*` and `vfs.*` (book/agents/window-and-vfs.md, ADR-7232).
mod window_vfs;
// `assets.push`: assets the editor pushes into a launched game (book/agents/asset-push.md, ADR-7836).
mod assets_push;

mod minspec_explain;
// `geometry.body_probe` (W-30, ADR-8323).
mod geometry_probe;

// `net.quic_status`: QUIC datagram socket counters (book/agents/net-quic.md, ADR-3970).
mod net_quic;

// Photosensitivity check: seeing.flash_check (book/agents/seeing.md, ADR-3804).
mod seeing_flash;

// `render.get_visbuffer_residency` / `render.set_visbuffer_residency`: cluster residency (ADR-3118).
mod visbuffer_residency;

// Plates and the reference overlay of the look reviews (book/agents/plates.md, ADR-2463).
mod plates;
mod reference_overlay;
pub mod shot_list;
// WAV cue points: audio.cues (book/agents/audio.md, ADR-4632).
mod audio_cues;
// Marker sheet CSV: sequencer.marker_sheet (book/agents/sequence-player.md, ADR-4597).
mod marker_sheet;
// The outdoor composite's aerial perspective inside interior volumes (book/agents/aerial-interiors.md, ADR-4757).
mod aerial_interiors;
// The direct BRDF's energy model (book/agents/brdf-energy.md, ADR-2632).
mod render_brdf_energy;
// `seeing.frame_aov`: the frame's shading normal and roughness (book/agents/frame-aov.md, ADR-2951).
mod frame_aov;
// `assets.import_rules`: effective import settings of one source (book/agents/import-rules.md, ADR-3321).
mod import_rules;
// `import.materialx_report`: the MaterialX importer's mapping report (book/agents/materialx-import.md, ADR-3341).
pub mod materialx_report;
// Connectionless server bots (book/agents/net-session-players.md, ADR-3346).
mod net_bots;
// Contact sounds from physics: audio.impacts, audio.impact_probe, audio.impacts_set (book/agents/physics-audio.md, ADR-3270).
mod physics_audio;
// Gameplay force fields: physics.fields, physics.field_set, physics.field_probe (ADR-3348).
mod physics_fields;
// The schedule executor: executor.get / executor.set (book/agents/schedule-executor.md, ADR-4933).
mod executor;
// `assets.terrain_tiles`: tile counts and bcn/astc for a cooked terrain (book/agents/terrain-tiles.md, ADR-5294).
mod terrain_tiles;

// A host's own quality detection for `quality.get_detected` (`host`, ADR-5131).
pub use quality::HostQualityDetection;

// Windows owner-only agent files read back with icacls (ADR-5489).
#[cfg(test)]
mod owner_only_tests;
// Stable entity references in saves: `save.inspect` (ADR-3239).
mod save_refs;
// The cutscene flow around a sequence player (book/agents/cutscene.md, ADR-2948).
mod cutscene;
// The sun's specular as a disc (book/agents/sun-specular-disc.md, ADR-3190).
mod render_sun_specular_disc;
// Procedural evaluation digest (book/agents/procgen.md, ADR-4593).
mod procgen_fingerprint;
// Loaded-cell relevance: stream.cell_relevance (book/agents/stream-cell-relevance.md, ADR-4011).
mod stream_cell_relevance;

pub use stream_cell_relevance::{
    CellRelevanceCall, CellRelevanceHost, CellRelevanceReply, cell_relevance, cell_relevance_peer,
};
// assets.texture_coverage, assets.texpack_dry_run (book/agents/texture-coverage.md, ADR-3090).
mod texture_coverage;
// Skid-steer tracks (book/agents/vehicle-track.md, ADR-4629).
mod vehicle_track;
// `input.glyph`: the button-prompt glyph per action and device family (book/agents/input.md, ADR-7070).
mod input_glyph;
// `render.lights`: directional lights and ambient volumes (book/agents/render-lights.md, ADR-7211).
mod render_lights;
// Parametric audio patches: `audio.patch_state`, `audio.patch_trigger` (ADR-7226).
mod audio_patch;
// What the last frame drew on screen: `render.visible_entities` (spec 14 § A1, ADR-7200).
mod visible_entities;
// Indoor cells and portals: `render.viscells`, `render.set_viscells` (spec 25 § 7.7, ADR-7586).
mod viscells;
// Screenshot debug overlays: colliders, light radii, bounds, grid (ADR-7217, book/agents/seeing.md).
pub mod seeing_overlays;
// `agent.cook` / `agent.package` jobs over the `regia` CLI (ADR-7217, book/agents/cook-package.md).
pub mod cook_package;
// Procedural mesh recipes: `gen.mesh` (book/agents/gen-mesh.md, ADR-7240).
mod gen_mesh;
// Marching-squares contours: `gen.contour` (book/agents/gen-mesh.md, ADR-8044).
mod gen_contour;

/// `memory.report` called in process (ADR-9740): the editor Profiler's Memory view reads the edit
/// world through it, so the panel and the protocol report the same numbers.
///
/// # Errors
/// `params` is not a valid `memory.report` request (`top` not a non-negative integer).
pub fn memory_report(
    world: &mut regia_app::prelude::World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, protocol::RpcError> {
    memory::report(world, params)
}
// `ui.glyph_atlas`: glyph atlas pages, recycles and thrash (book/agents/ui-glyph-atlas.md, ADR-8832).
mod ui_glyph_atlas;
// Procedural textures and heightfields: `gen.texture`, `gen.heightfield` (book/agents/gen-textures.md, ADR-7280).
mod gen_texture;

/// `render.out_job`, `render.out_progress` and `render.out_cancel` called in process (ADR-7726):
/// the editor's Sequence ▸ Render movie starts the same job an agent starts, on the editor's own
/// renderer, and follows it from its frame loop without a connection.
pub mod render_out_host {
    use regia_app::prelude::World;
    use serde_json::Value;

    use crate::protocol::RpcError;

    /// `render.out_job { job | file, date?, dry_run? }` (book/agents/render-jobs.md).
    ///
    /// # Errors
    /// What `render.out_job` answers: the job's refusal codes, `editor_headless`, `F0011`, `F0018`.
    pub fn out_job(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        crate::render_out::out_job(world, params)
    }

    /// `render.out_progress { job? }`.
    ///
    /// # Errors
    /// `NOT_FOUND` when no such job exists.
    pub fn progress(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        crate::render_out::progress(world, params)
    }

    /// `render.out_cancel { job? }`: the running job ends at the next frame and keeps the frames
    /// delivered so far (each written whole: `.part` then a rename).
    ///
    /// # Errors
    /// `NOT_FOUND` when no such job exists.
    pub fn cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        crate::render_out::note_cancel_request();
        crate::render_out::cancel(world, params)
    }

    /// Whether a render-out job holds the app's clock.
    #[must_use]
    pub fn job_running(world: &World) -> bool {
        crate::render_out::job_running(world)
    }
}

/// Optional scene authoring integration for editor hosts.
pub use scene::{SceneEditHandler, SceneEditHook};
// Non-destructive terrain edit layers: terrain.edit_layer_* (P-236, ADR-7825).
mod terrain_edit_layers;
// Match rules core: rules.match_* (ADR-8102, book/agents/match-rules.md).
mod match_results;
mod match_rules;
// `play.frame` / `play.mute`: a game seen and driven from the editor's Game tabs (ADR-7831).
pub mod play_view;
// NM-1/NM-2 nav queries: nav.raycast, nav.random_point, nav.crowd_set_stuck (ADR-8380).
mod nav_query;
// `sim.desync_rows`: the per-system lockstep desync rows of `regia_framework::desync_hash` (ADR-8080).
mod desync_rows;
// `assets.check_contract`: a `.contract.ron` judged against one observation (ADR-8401).
pub mod check_contract;
// The host's keyed draws without entities: `render.draw_list` (ADR-8087).
mod draw_list;
// Overlay asset roots: cook.overlay_roots (book/agents/cook-overlay-roots.md, ADR-8155).
mod cook_overlay;
// `content.check_placement` and `content.install_tier` (ADR-8036).
mod content_smalls;
// `terrain.surface_at`, `terrain.mesh_layers`, `terrain.bake_report`, `terrain.blockers`
// (spec 121 § 12, ADR-8386).
pub mod terrain_mesh_layers;
// Trusted editor hosts apply an additional per-session permission policy (ADR-7844).
pub use dispatch::process_hosted_request;

/// The ECS counts of `world` with its `top` component types ranked by entity count (spawned
/// entities, archetypes, `(name, entities)` most first): what `profiler.frame { entities }`
/// answers and the editor Profiler's in-process Entities tab shows (ADR-7849).
#[must_use]
pub fn profiler_entity_counts(
    world: &regia_app::prelude::World,
    top: usize,
) -> (u32, usize, Vec<(String, u64)>) {
    profiler::entity_counts(world, top)
}
// `fog.status` / `fog.set_viewer`: per-viewer fog of war over the sim bridge (plan row S-16, ADR-8154).
mod fog;

// Count-based terrain sources and overview baking (ADR-8063).
mod terrain_source;
// render.scene_hold, render.set_scene_hold (book/agents/frame-pacing.md, ADR-7890).
mod scene_hold;
// play.freeze_state: what the gameplay pause holds (book/agents/game-pause-freeze.md, ADR-10105).
mod pause_freeze;
// Frame caps per app state and the dynamic-resolution hold: app.pacing, app.set_pacing_state,
// render.dynres (book/agents/frame-pacing.md, ADR-7889).
mod app_pacing;
// The runtime renderer's per-frame load budget (book/agents/instant-load.md, ADR-7884).
mod load_budget;
// `net.socket_buffers`: UDP buffer read-back and DSCP EF (book/agents/net-socket-buffers.md, ADR-7998).
mod net_socket_buffers;

// Local diagnostic artifact inventory (ADR-8058).
mod report_sources;
// `net.relay_status`: relay allocations, caps and refusals (book/agents/net-relay.md, ADR-8183).
mod net_relay;

// `nav.det_agents`: the host's deterministic agents report (plan row S-03, ADR-8405).
mod nav_det_agents;
// `assets.ids_export` / `assets.ids_import`: the `.assetids.ron` asset id manifest (ADR-8460).
mod asset_ids;
// Plain-node hinges, spins and slews (ADR-8410, book/agents/node-drivers.md).
mod node_drivers;

pub mod log_triage;
pub mod perf_gate;
// Vertex-cluster far LOD (book/agents/cluster-lod.md, ADR-8464).
mod cluster_lod;

// `assets.diet_report`: what the cook diet did to a build profile's pak (row C-04, ADR-8461).
pub mod diet_report;

// `terrain.apply_operator`: heightfield operators over the first terrain (book/agents/terrain-ops.md, ADR-8465).
mod terrain_ops;
// The incremental report outbox: `report.outbox_status` (book/agents/report-outbox.md, ADR-8402).
mod report_outbox;

// Host project ceilings for count-based source imports (ADR-8362).
pub use terrain_source::TerrainSourcePolicy;
// Scatter gameplay colliders: terrain.scatter_colliders (book/agents/terrain-scatter-colliders.md, ADR-8029).
mod terrain_scatter_colliders;
// Lockstep projectiles and the aim solver: detsim.projectile_simulate, detsim.aim_solve
// (book/agents/det-projectiles.md, ADR-8509).
mod det_projectiles;
// `net.host_check`: port mapping, port-forward self-check, capacity (book/agents/net-host-check.md,
// ADR-8370).
mod net_host_check;
// Baked section volumes on `render.get_exposure` (plan row W-24, ADR-8384).
pub use exposure::BakedZoneList;
mod keyboard_layout;
// UI pointer claims: input.pointer_claims (book/agents/ui-input.md, ADR-7976).
mod input_pointer_claims;
// `detsim.hover_*`: the lockstep hover on the protocol (book/agents/det-hover.md, plan row mp-hover-ecs-agent),
// and `detsim.hover_hull_check`, the `.hoverhull.ron` load check (ADR-8512) in its `hull_check` submodule (ADR-8637).
mod det_hover;
// `lobby.hub_status`: the self-hosted lobby hub (book/agents/net-lobby-hub.md, ADR-8559).
mod net_lobby_hub;
// Lockstep ground vehicles: detsim.ground_vehicle_simulate (book/agents/det-ground-vehicles.md, ADR-8551).
mod det_ground_vehicle;
// Client capture on a NetLink: net.capture_start, net.capture_save (book/agents/net-capture.md, ADR-8890).
mod net_capture;
// `sim.grid_flow`: flow fields on the host's published deterministic grid (book/agents/det-grid.md, ADR-8839).
mod sim_grid_flow;
// Locomotion quality: anim.locomotion_state, anim.gait_simulate, anim.foot_plant_simulate, anim.ik_report
// (book/agents/locomotion-quality.md, ADR-8708).
mod locomotion;
// `anim.gait_validate`: the `.gait.ron` load check (book/agents/procedural-gait.md, gap G7).
mod gait_asset;

// The shared count-source service (ADR-8541): the editor's Terrain ▸ Import source workflow and
// `regia terrain source-inspect` / `source-import` read and preview sources through it.
pub use terrain_source::{
    ProjectSource, mapping_document, publish_source_asset, read_project_source, resolve_project_path,
    source_inputs_digest, source_summary,
};
// Source tile sets (ADR-8595): the editor import reports tile progress and cancels through it.
pub use terrain_source::{inspect_project_source, read_project_source_observed};
// Registered terrain overviews (ADR-8596): `terrain.bake_overview`, `regia terrain source-overview` and
// the editor's `terrain_overview:<n>` job share the settings, markers and publication.
pub use terrain_source::{
    MAX_OVERVIEW_MARKERS, overview_markers, overview_settings, publish_project_overview,
};
// `rules.ruleset_*` and `rules.match_setup { ruleset }`: rule sets as content (ADR-8694).
mod ruleset;

// `assets.provenance_import`: pack provenance records into `.meta` provenance (ADR-8834).
mod provenance_import;

// `game.pause`, `game.resume`, `game.pause_state`: the game pause key and its screen (book/agents/game-pause.md, ADR-10104).
mod game_pause;
