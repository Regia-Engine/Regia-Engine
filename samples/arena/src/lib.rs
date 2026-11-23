// SPDX-License-Identifier: MIT-0
//! `samples/arena` (M20): a first-person bot arena — a `GameRules` match flow on the hosted match
//! session (warm-up → round → match over, score limit + timer, respawn at `PlayerStart`s;
//! `regia_framework::rules::MatchSessionPlugin`), `AiController` bots that steer
//! and fire through line-of-sight raycasts, hitscan and pooled-projectile weapons, dynamic props
//! with joints and explosion impulses, VFX/decals/spatial-audio, and a gamepad- and touch-usable
//! HUD. No networking.
//!
//! See `docs/specs/14-scene-and-framework.md` (M20 `samples/arena`) and
//! `docs/specs/15-physics.md`.

pub mod audio;
pub mod bot_bt;
pub mod bots;
pub mod brain;
pub mod budget;
pub mod camera;
pub mod config;
pub mod hud;
pub mod loadout;
pub mod match_flow;
pub mod meshes;
pub mod modes;
pub mod navmesh;
pub mod pawn;
pub mod props;
#[cfg(feature = "agent")]
pub mod smoke;
pub mod spawn_drive;
pub mod spectate;
pub mod weapons;

use regia_app::prelude::*;
use regia_framework::rules::GameRulesResource;

use crate::audio::ArenaAudio;
use crate::match_flow::ArenaRules;

pub use config::{ArenaConfig, BotBrainKind, MatchRng};
pub use match_flow::{ArenaState, Phase};
pub use modes::{ArenaMatchMode, ArenaMode};
pub use pawn::Pawn;

/// Exits the app after this many frames (the `--exit-after-frames` smoke-run flag).
#[derive(Resource, Debug, Clone, Copy)]
pub struct ArenaExitAfterFrames(pub u64);

/// `Update`: honours [`ArenaExitAfterFrames`].
pub fn exit_after_frames(
    limit: Option<Res<ArenaExitAfterFrames>>,
    time: Res<Time>,
    mut exit: MessageWriter<AppExit>,
) {
    if let Some(limit) = limit
        && time.frame_count() >= limit.0
    {
        exit.write(AppExit::Success);
    }
}

/// Outdoor exposure that keeps the floor, crates and bots in range on lavapipe.
///
/// Auto exposure hunts on the first frames and clips a sunlit arena to white. The forward
/// shader applies the camera's EV when the post stack is off (the same setup `third_person`
/// uses for its screenshots).
fn readable_render_settings() -> regia_render::RenderSettings {
    let mut settings = regia_render::RenderSettings::default();
    // Android keeps the post stack: without it the renderer ignores the Mobile tiers' render scale
    // (ADR-1874), and the forward shader tonemaps every pixel itself.
    settings.post.enabled = cfg!(target_os = "android");
    settings.post.auto_exposure = false;
    settings.post.exposure_ev = camera::EXPOSURE_EV;
    settings
}

/// The arena's fixed-step phases, chained after input recording and the physics step (see
/// `ArenaSample::build`): bots steer and fire, weapons resolve, damage applies, the match
/// scores.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArenaSimSet {
    /// `bots::drive_bots`.
    Bots,
    /// Fire requests, projectiles, explosions.
    Weapons,
    /// Match conditions (the score limit) and deaths, before the session steps.
    Match,
    /// The session's results copied into `ArenaState`, after `regia_framework::rules::MatchRulesSet`.
    Report,
}

/// The `samples/arena` plugin: gameplay, physics, combat, VFX, audio, UI and the match rules.
pub struct ArenaSample;

impl Plugin for ArenaSample {
    fn build(&self, app: &mut App) {
        // Input actions: keyboard/mouse and gamepad for move, look, jump and fire.
        let config = regia_input::InputConfig::new()
            .action("Move", regia_input::ActionKind::Axis2D)
            .action("Look", regia_input::ActionKind::Axis2D)
            .action("Jump", regia_input::ActionKind::Button)
            .action("Fire", regia_input::ActionKind::Button)
            .binding(regia_input::Binding::new(
                "Move",
                regia_input::InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(regia_input::Binding::new(
                "Move",
                regia_input::InputSource::gamepad_stick(regia_input::Stick::Left),
            ))
            .binding(regia_input::Binding::new(
                "Look",
                regia_input::InputSource::MouseDelta,
            ))
            .binding(regia_input::Binding::new(
                "Look",
                regia_input::InputSource::gamepad_stick(regia_input::Stick::Right),
            ))
            .binding(regia_input::Binding::new(
                "Jump",
                regia_input::InputSource::key("Space"),
            ))
            .binding(regia_input::Binding::new(
                "Jump",
                regia_input::InputSource::gamepad_button(regia_platform::GamepadButton::South),
            ))
            .binding(regia_input::Binding::new(
                "Fire",
                regia_input::InputSource::mouse_button(regia_platform::MouseButton::Left),
            ))
            .binding(regia_input::Binding::new(
                "Fire",
                regia_input::InputSource::gamepad_button(regia_platform::GamepadButton::RightTrigger),
            ));

        app.add_plugins((
            regia_input::InputPlugin::new(config),
            regia_scene::ScenePlugin,
            regia_physics::PhysicsPlugin,
            regia_framework::camera::CameraPlugin,
            regia_framework::input_recording::InputRecordingPlugin,
            regia_framework::damage::DamagePlugin,
            regia_vfx::VfxPlugin,
            regia_audio::AudioPlugin,
        ));
        // `ArenaConfig` must exist before the nav/AI plugins decide whether to install. The
        // default is off; a game opts into navmesh pathing by inserting `ArenaConfig {
        // use_navmesh: true }` before adding `ArenaSample`. The nav/AI systems (in particular the
        // exclusive behaviour-tree tick) would otherwise perturb the FixedUpdate schedule and break
        // the bot match's determinism even when they are no-ops.
        app.init_resource::<ArenaConfig>();
        let config = app.world().resource::<ArenaConfig>().clone();
        if config.use_navmesh {
            app.add_plugins((regia_nav::NavPlugin, regia_ai::AiPlugin));
        } else if config.brain == BotBrainKind::StateTree {
            // State-tree bots tick in `AiStep`, which the fixed-step chain below orders; their
            // `Restock` task ranks the stations with a spatial query (run at once, inside the tick).
            if !app.has_plugin(std::any::type_name::<regia_ai::AiPlugin>()) {
                app.add_plugins(regia_ai::AiPlugin);
            }
            if !app.has_plugin(std::any::type_name::<regia_ai::SpatialQueryPlugin>()) {
                app.add_plugins(regia_ai::SpatialQueryPlugin);
            }
        }
        // Gameplay tags, abilities, the weapon chooser, smart objects and the spectator view
        // (ADR-2134). Installed in every mode so the agent methods answer; only the state-tree
        // brain uses them in the match.
        app.add_plugins((
            regia_ai::SmartObjectPlugin,
            loadout::LoadoutPlugin,
            spectate::SpectatePlugin,
        ));
        // The umbrella's `DefaultPlugins` already installs `UiPlugin`; keep an explicit install
        // for headless builds without it.
        if !app.has_plugin(std::any::type_name::<regia_ui::UiPlugin>()) {
            app.add_plugins(regia_ui::UiPlugin::code_only());
        }

        // A VFX scene with the canonical showcase effect set (impact, explosion, muzzle_flash…).
        let mut library = regia_vfx::scene::VfxLibrary::new();
        for def in regia_vfx::showcase_effects() {
            let _ = library.add(def);
        }
        app.insert_resource(regia_vfx::VfxScene::new(library));

        app.init_resource::<MatchRng>()
            .insert_resource(ArenaAudio::build())
            .insert_resource(readable_render_settings())
            .insert_resource(GameRulesResource {
                rules: Box::new(ArenaRules),
            });

        // Register the arena's reflected types so the agent protocol (`scene.get_entity`,
        // `resources.list`, `play.expect`) can inspect pawns, health and the match state.
        app.register_type::<ArenaState>()
            .register_type::<Phase>()
            .register_type::<Pawn>()
            .register_type::<weapons::Projectile>()
            .register_type::<weapons::Weapon>()
            .register_type::<pawn::BotBrain>()
            .register_type::<props::Prop>()
            .register_type::<config::ArenaBrainStatus>()
            .register_type::<modes::ArenaModeStatus>();

        app.add_plugins((
            bots::BotsPlugin,
            weapons::WeaponsPlugin,
            props::PropsPlugin,
            spawn_drive::SpawnDrivePlugin,
            camera::CameraPlugin,
            hud::HudPlugin,
        ));

        app.add_systems(
            Startup,
            (
                meshes::build_arena_assets,
                match_flow::spawn_world,
                camera::spawn_camera,
            )
                .chain(),
        );
        // The match is the framework's hosted session, stepped in `MatchRulesSet` (GR-1.3).
        app.add_plugins(regia_framework::rules::MatchSessionPlugin);
        // The arena's own rule sets (`ctf_quick`) next to the shipped defaults (`--ruleset`).
        if let Some(mut registry) = app
            .world_mut()
            .get_resource_mut::<regia_framework::ruleset_file::RuleSetRegistry>()
        {
            modes::load_arena_rule_sets(&mut registry);
        }
        app.add_systems(
            FixedUpdate,
            (
                match_flow::tick_match,
                match_flow::resolve_deaths,
                modes::tick_respawns,
            )
                .chain()
                .in_set(ArenaSimSet::Match),
        );
        // Team modes (`--mode ctf|koth|tdm`): the match core's mode-bot orders move the bots; a
        // no-op in the free-for-all.
        app.add_systems(FixedUpdate, modes::drive_mode_bots.in_set(ArenaSimSet::Bots));
        app.add_systems(
            FixedUpdate,
            match_flow::mirror_match_state.in_set(ArenaSimSet::Report),
        );
        // One total order for the fixed step. Left to the executor, these systems (which all
        // touch pawns, transforms, the RNG or the physics world) ran in whatever order the
        // schedule graph happened to pick, and an unrelated plugin adding a resource changed
        // that order between a recording and its replay (the determinism test broke when
        // `regia_vfx` registered one more resource).
        // The navmesh bots' tree tick plans paths that the nav step then follows; both are empty
        // sets (no-ops) when `use_navmesh` is off (ADR-867).
        app.configure_sets(
            FixedUpdate,
            (
                regia_framework::input_recording::InputRecordingSet,
                regia_physics::PhysicsStep,
                // Navmesh bots: their trees decide, then the nav agents move (no-ops without
                // `use_navmesh`).
                regia_ai::AiStep,
                regia_nav::NavStep,
                ArenaSimSet::Bots,
                ArenaSimSet::Weapons,
                regia_framework::damage::DamageSet,
                ArenaSimSet::Match,
                regia_framework::rules::MatchRulesSet,
                ArenaSimSet::Report,
            )
                .chain(),
        );
        app.add_systems(Update, exit_after_frames);
        app.add_systems(PostUpdate, meshes::attach_pawn_meshes);
    }
}

// The Rust examples of the capture-the-flag tutorial run as doctests (GR-8.3):
// `cargo test -p arena --doc doctests_in_capture_the_flag_tutorial_compile`. The sample compiles in
// its own copy of the book page (a sample reads no file outside its folder, ADR-1485);
// `tests/tutorial_copy.rs` keeps it byte-identical to `book/tutorials/capture-the-flag.md`.
#[cfg(doctest)]
#[doc = include_str!("../doc/capture-the-flag.md")]
mod doctests_in_capture_the_flag_tutorial_compile {}
