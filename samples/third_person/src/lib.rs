// SPDX-License-Identifier: MIT-0
//! `third_person` (M20, `docs/specs/14` acceptance): a playable third-person slice built on
//! the framework — rapier-driven character movement (move, jump), a boom camera that shortens
//! on collision, the cooked Khronos Fox animated from `CharacterState` with foot IK on
//! stairs and slopes, collectibles via trigger scripts, a HUD counter, a pause menu, and
//! save/load that restores collected state. The game flow is `regia_framework`'s [`GameFlow`]
//! with a title screen (Boot → MainMenu → Loading → Playing ⇄ Paused; a host that inserts
//! `GameFlowSettings { skip_main_menu: true, .. }` before the plugin skips it); the Esc menu and
//! `GameFlow::Paused` stay in step both ways, so an agent's `state.set GameFlow Paused` opens the
//! menu.
//!
//! Combat (ADR-2596): two enemy kinds driven by `.bt.ron` behavior trees over a navmesh crowd
//! ([`enemy`]), health and hits through the framework's abilities and attributes ([`combat`]), a
//! montage-timed three-hit melee combo ([`melee`]), ragdoll deaths ([`ragdoll_fx`]), checkpoint
//! respawns ([`checkpoint`]), a settings menu ([`menu`]), procedural sound ([`sound`]) and saves
//! through the save flow ([`save`]). Two local players play split-screen: the `PlayerManager` layout is written to each
//! `Camera.viewport` (the renderer composites one view per camera) and mirrored onto the
//! reflected [`camera::SplitViewport`] anchor.
//!
//! The Fox glTF is fetched (CC0, Khronos sample models) and cooked at startup through the
//! CLI, then the skeleton, clips and mesh are loaded from the cooked cache.

use regia::prelude::*;
use regia_framework::prelude::{
    CameraPlugin, CharacterPlugin, GameFlow, GameFlowPlugin, GameFlowSettings, PlayerPlugin,
    RegisterSaveableExt, RegisterScriptExt, RulesPlugin, SaveDirectory, SavePlugin, ScriptPlugin,
    TimerPlugin, TweenPlugin,
};

pub mod anim;
pub mod assets;
pub mod camera;
pub mod car;
pub mod character;
pub mod checkpoint;
pub mod cloth_fx;
pub mod collectible;
pub mod combat;
pub mod demo_fx;
pub mod enemy;
pub mod hud;
pub mod input;
pub mod level;
pub mod melee;
pub mod menu;
pub mod npc;
pub mod ragdoll_fx;
pub mod save;
pub mod sound;

/// Where the sample keeps its saves (`saves/`) and settings (`settings.ron`). Insert before the
/// plugin to move them (tests use a temporary directory); the default is the sample's `.regia/`.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct SampleDataDir(pub std::path::PathBuf);

impl Default for SampleDataDir {
    fn default() -> Self {
        Self(assets::project_dir().join(".regia"))
    }
}

/// Gameplay state the HUD, save system and agent `play.expect` checks read.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct SampleState {
    /// How many collectibles have been picked up.
    pub collected: u32,
    /// How many collectibles the level holds.
    pub total: u32,
    /// Whether the game is paused (mirrored into `Time::paused`).
    pub paused: bool,
}

/// The sample's plugin: framework + physics + animation + UI plus the game systems.
#[derive(Default)]
pub struct ThirdPersonSample;

impl Plugin for ThirdPersonSample {
    fn build(&self, app: &mut App) {
        let data_dir = app
            .world()
            .get_resource::<SampleDataDir>()
            .cloned()
            .unwrap_or_default();
        app.insert_resource(data_dir.clone());
        if !app
            .world()
            .contains_resource::<regia_framework::settings::SettingsPath>()
        {
            app.insert_resource(regia_framework::settings::SettingsPath(Some(
                data_dir.0.join("settings.ron"),
            )));
        }
        // Headless hosts (tests) have no audio plugin: add one without a device.
        if !app.has_plugin(std::any::type_name::<regia_audio::AudioPlugin>()) {
            if !app.world().contains_resource::<regia_audio::AudioSettings>() {
                app.insert_resource(regia_audio::AudioSettings {
                    headless: true,
                    ..Default::default()
                });
            }
            app.add_plugins(regia_audio::AudioPlugin);
        }
        app.add_plugins((
            regia_scene::ScenePlugin,
            regia_animation::AnimationPlugin,
            regia_physics::PhysicsPlugin,
            ScriptPlugin,
            RulesPlugin,
            PlayerPlugin,
        ))
        .add_plugins((
            CameraPlugin,
            CharacterPlugin,
            TimerPlugin,
            TweenPlugin,
            SavePlugin::default(),
        ))
        .add_plugins(regia_framework::save_flow::SaveFlowPlugin);
        // The title screen shows unless the host skipped it. The Fox loads synchronously in
        // `Startup`, so Loading has no tasks and lasts one frame.
        if !app.world().contains_resource::<GameFlowSettings>() {
            app.insert_resource(GameFlowSettings {
                skip_main_menu: false,
                ..Default::default()
            });
        }
        // Subtitles and conversations (the guide NPC), cloth and soft bodies (the cape and the
        // ball), demo recording: each only when the host did not add it already.
        if !app
            .world()
            .contains_resource::<regia_ui::captions::CaptionBoard>()
        {
            app.add_plugins(regia_ui::captions::CaptionPlugin);
        }
        if !app
            .world()
            .contains_resource::<regia_framework::conversation::ConversationRunner>()
        {
            app.add_plugins(regia_framework::conversation::ConversationPlugin);
        }
        if !app
            .world()
            .contains_resource::<regia_framework::cloth::ClothBudget>()
        {
            app.add_plugins(regia_framework::cloth::ClothGluePlugin);
        }
        demo_fx::add_plugins(app);
        app.add_plugins(GameFlowPlugin)
            .add_plugins(regia_framework::settings::SettingsPlugin)
            // A drivable car with a camera-driver chase camera, and gameplay tags (ADR-2134).
            .add_plugins(car::CarPlugin)
            // Combat after the car: it validates its data against the car's tag registry.
            .add_plugins((
                combat::CombatPlugin,
                ragdoll_fx::RagdollFxPlugin,
                enemy::EnemyPlugin,
                melee::MeleePlugin,
                sound::SoundPlugin,
            ))
            .add_systems(OnEnter(GameFlow::Paused), |world: &mut World| {
                hud::apply_pause(world, true)
            })
            .add_systems(OnExit(GameFlow::Paused), |world: &mut World| {
                hud::apply_pause(world, false)
            })
            .register_type::<SampleState>()
            .register_type::<collectible::CollectibleId>()
            .register_type::<camera::SplitViewport>()
            .register_type::<hud::HudLabel>()
            .register_type::<anim::SolvedFoot>()
            .register_type::<checkpoint::Checkpoint>()
            .register_type::<menu::MenuState>()
            .register_type::<save::SaveStatus>()
            .register_type::<hud::DamageNumbers>()
            .register_type::<npc::Npc>()
            .register_type::<npc::TalkState>()
            .register_type::<cloth_fx::Cape>()
            .register_type::<cloth_fx::SoftProp>()
            .register_saveable_resource::<checkpoint::CombatProgress>()
            .init_resource::<checkpoint::CombatProgress>()
            .init_resource::<checkpoint::CheckpointToast>()
            .init_resource::<save::SaveStatus>()
            .register_script::<collectible::CollectScript>()
            .register_saveable_resource::<collectible::CollectProgress>()
            .insert_resource(collectible::CollectProgress::default())
            .insert_resource(collectible::StateHandle::default())
            .insert_resource(SaveDirectory(data_dir.0.join("saves")))
            .insert_resource(forward_only_settings())
            .init_resource::<input::LookInput>()
            .init_resource::<input::ActionKeys>()
            .init_resource::<input::InputCursors>()
            .init_resource::<input::SplitOnStart>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::MouseMotion>()
            .add_systems(
                Startup,
                (
                    level::setup,
                    |world: &mut World| {
                        car::spawn_car(world);
                    },
                    hud::setup,
                    load_fox_assets,
                    checkpoint::setup,
                    enemy::setup,
                    npc::setup,
                    cloth_fx::setup,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (
                    menu::menu_clicks,
                    menu::sync_menus,
                    checkpoint::touch_pads,
                    save::after_save_flow,
                    hud::refresh_combat,
                    combat::summarize,
                )
                    .chain()
                    .after(regia_framework::script::ScriptsSet::Update),
            )
            .add_systems(PostUpdate, save::prepare_loads)
            .add_systems(
                Update,
                keep_forward_only.after(regia_framework::settings::SettingsSystems::Apply),
            )
            .add_systems(
                Update,
                collectible::scan_triggers.before(regia_framework::script::ScriptsSet::Update),
            )
            .add_systems(
                Update,
                (
                    input::handle_global_keys,
                    hud::menu_clicks,
                    character::spawn_for_players,
                    input::join_if_requested,
                    camera::sync_viewports,
                    collectible::reconcile,
                    collectible::animate,
                )
                    .chain()
                    .after(regia_framework::script::ScriptsSet::Update),
            )
            .add_systems(Update, input::drive_input)
            // The engine pause screen's Quit to menu (ADR-10104).
            .add_systems(Update, input::quit_to_menu)
            .add_systems(Update, npc::track.after(input::handle_global_keys))
            .add_systems(
                Update,
                anim::drive_rig.before(regia_animation::systems::prepare_foot_placement),
            )
            .add_systems(
                PostUpdate,
                anim::publish_solved_feet.after(regia_animation::systems::apply_animation),
            )
            // After scripts, so a coin collected this frame is already in the HUD string.
            .add_systems(
                Update,
                hud::refresh_counter.after(regia_framework::script::ScriptsSet::Update),
            )
            // The rigs' `SkinnedMesh` palettes reach the renderer through the engine bridge (C1).
            .add_plugins(regia_framework::skinning::SkinnedMeshPlugin);
        regia_framework::character::use_rapier_character_controller(app);
    }
}

/// Post stack off, camera EV applied in the forward shader.
///
/// `regia_render` sizes the HDR target to each view's pixel size and then sets a
/// window-space scissor (`frame.rs` `render_into`). The second split view's rect
/// `{x: 640, y: 0, w: 640, h: 720}` is not inside that 640×720 target, so the
/// submit panics. With the stack disabled the colour pass draws into the full
/// window (the scissor fits) and `lab_forward.wgsl` applies the camera EV and AgX.
/// This sample does not patch the renderer.
fn forward_only_settings() -> regia_render::RenderSettings {
    let mut settings = regia_render::RenderSettings::default();
    settings.post.enabled = false;
    settings.post.auto_exposure = false;
    settings
}

/// `Update` (after the settings apply): a graphics preset chosen in the settings menu re-tiers
/// `RenderSettings`; keep the post stack off as [`forward_only_settings`] explains.
fn keep_forward_only(render: Option<ResMut<regia_render::RenderSettings>>) {
    if let Some(mut render) = render
        && (render.post.enabled || render.post.auto_exposure)
    {
        render.post.enabled = false;
        render.post.auto_exposure = false;
    }
}

/// `Startup`: load the cooked Fox (best effort — the sample stays playable without it).
fn load_fox_assets(world: &mut World) {
    match assets::load_fox() {
        Ok(fox) => {
            world.insert_resource(fox);
        }
        Err(err) => {
            tracing::warn!(target: "third_person", "Fox assets unavailable: {err}");
        }
    }
}
