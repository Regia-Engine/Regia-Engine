// SPDX-License-Identifier: MIT-0
//! Desktop body of `anim_lab`. Android builds skip this module.
//!
//! glTF files are cooked into skeleton/clip sub-assets by `regia cook` (run as a
//! subprocess) and loaded from the cooked cache via `regia_animation::cooked`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::clip::{JointTrack, Notify, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::compress::{CompressionSettings, compress_clip, decompress_clip, raw_joint_bytes};
use regia_animation::cooked::{load_cooked_clip, load_cooked_skeleton};
use regia_animation::events::{AnimNotify, AnimStateEvent};
use regia_animation::graph::{
    AnimGraph, AnimNode, BlendSpace1DNode, ClipNode, IkTarget, JointMaskDef, LayerEntry, LayerMode,
    LayerNode, LookAtNode, TwoBoneIKNode,
};
use regia_animation::montage::{MontageNotify, MontageSection, SectionNext, WarpWindow};
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_animation::state_machine::{Condition, StateMachineDef, TransitionCurve, TransitionDef};
use regia_animation::{
    AnimationClip, AnimationPlugin, Animator, ClipLibrary, JointGlobals, Montage, MontagePlayer, PlayOptions,
    RootMotionOutput, Skeleton, SkinPalette, SkinnedMesh, SlotDef, Socket, WarpTarget,
};
use regia_app::{App, MinimalPlugins, TimeUpdateStrategy, Update};
use regia_asset::AssetManifest;
use regia_audio::{Audio, AudioPlugin, AudioSettings, SoundClip, drain_one_shots};
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::ecs as bevy_ecs;
use regia_ecs::prelude::{Entity, IntoScheduleConfigs, MessageReader, Res, ResMut, Resource};
use regia_scene::Transform;

const FOX_URL: &str =
    "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Assets/main/Models/Fox/glTF-Binary/Fox.glb";
const CESIUM_URL: &str = "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Assets/main/Models/CesiumMan/glTF-Binary/CesiumMan.glb";

const UPPER: &str = "b_RightUpperArm_06";
const FORE: &str = "b_RightForeArm_07";
const HAND: &str = "b_RightHand_08";
const HEAD: &str = "b_Head_05";
const TAIL: &str = "b_Tail01_012";
const SPINE: &str = "b_Spine02_03";

#[derive(Resource, Default)]
struct StateLog {
    events: Vec<(Option<String>, String)>,
}

#[derive(Resource)]
struct FootstepSound(SoundClip);

/// Run the lab and return a process exit code.
pub fn run() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    match run_lab() {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("anim_lab: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_lab() -> Result<String, String> {
    let example = load_example()?;
    // The montage-in-a-graph case needs no downloaded content: it runs first.
    let slot_line = crate::slot_graph::run()?;
    // The moving-target IK, look-at and turret case needs no downloaded content either (ADR-7216).
    let reach_line = anim_lab::reach::run()?;
    // The procedural walker case (GaitRig hexapod, FootPlacement biped on stairs) needs none either.
    let locomotion_line = anim_lab::locomotion::summary()?;
    let cache = cache_dir()?;
    let fox_path = ensure_glb(&cache, "Fox.glb", FOX_URL)?;
    let cesium_path = ensure_glb(&cache, "CesiumMan.glb", CESIUM_URL)?;

    let project_dir = cache.join("project");
    let cooked = cook_and_load(
        &project_dir,
        &[("Fox.glb", &fox_path), ("CesiumMan.glb", &cesium_path)],
    )?;
    // Reconstruct individual skins from the cooked list (order matches files).
    let fox = cooked
        .iter()
        .find(|(sk, _)| sk.joint_index("b_Spine02_03").is_some())
        .or_else(|| cooked.first())
        .ok_or("no Fox skeleton cooked")?;
    let cesium = cooked
        .iter()
        .find(|(sk, _)| sk.len() < fox.0.len())
        .or_else(|| cooked.get(1))
        .ok_or("no CesiumMan skeleton cooked")?;

    let sound = SoundClip::new(vec![0.2_f32; 48], 48_000).map_err(|e| e.to_string())?;
    let mut app = new_app(sound);

    let cesium_line = play_cesium(&mut app, (cesium.0.clone(), cesium.1.clone()))?;
    let fox_line = play_fox(&mut app, (fox.0.clone(), fox.1.clone()))?;
    turret(&mut app)?;

    let _ = example.root;
    Ok(format!(
        "anim_lab: parsed locomotion.animgraph.ron ({} parameters)\n{cesium_line}\n{fox_line}\nanim_lab: turret look-at followed AimTarget\nanim_lab: {slot_line}\n{reach_line}\n{locomotion_line}",
        example.parameters.len()
    ))
}

fn cache_dir() -> Result<PathBuf, String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/anim_cache");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn ensure_glb(dir: &Path, file: &str, url: &str) -> Result<PathBuf, String> {
    let path = dir.join(file);
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > 1000
    {
        return Ok(path);
    }
    let status = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&path)
        .arg(url)
        .status()
        .map_err(|e| format!("curl {url}: {e}"))?;
    if !status.success() {
        return Err(format!("curl exited {status} for {url}"));
    }
    Ok(path)
}

fn load_example() -> Result<AnimGraph, String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats/locomotion.animgraph.ron");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    AnimGraph::from_ron_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Cooked skin: skeleton + clips loaded from the cooked cache.
type CookedSkin = (Arc<Skeleton>, Vec<AnimationClip>);

fn cook_and_load(dir: &Path, files: &[(&str, &Path)]) -> Result<Vec<CookedSkin>, String> {
    // Set up a minimal project for the cook: <dir>/assets/*.glb
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).map_err(|e| format!("mkdir assets: {e}"))?;
    for (name, src) in files {
        let dst = assets.join(name);
        if !dst.is_file() {
            std::fs::copy(src, &dst).map_err(|e| format!("copy {name}: {e}"))?;
        }
    }

    // Run the cook subprocess. Uses `cargo run -p regia_cli` so the CLI is always fresh.
    let status = std::process::Command::new("cargo")
        .args(["run", "-p", "regia_cli", "--", "cook", "--project"])
        .arg(dir)
        .status()
        .map_err(|e| format!("cook subprocess: {e}"))?;
    if !status.success() {
        return Err(format!("regia cook exited {status}"));
    }

    // Read the manifest to locate skeleton/clip cooked assets.
    let platform = std::env::consts::OS;
    let cache = dir.join(".regia").join("cooked").join(platform);
    let manifest_path = cache.join("asset_manifest.bin");
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|e| format!("read manifest: {e}"))?;
    let manifest =
        AssetManifest::from_postcard(&manifest_bytes).map_err(|e| format!("manifest postcard: {e}"))?;

    // Group sub-assets by source path prefix to reconstruct skins.
    let mut skeletons: Vec<(Skeleton, Vec<AnimationClip>)> = Vec::new();
    let mut pending_clips: Vec<(String, AnimationClip)> = Vec::new();

    for entry in &manifest.entries {
        if entry.type_path == "regia_animation::Skeleton" {
            let fckd = cache.join(format!("{}.fckd", entry.id));
            let bytes = std::fs::read(&fckd).map_err(|e| format!("read skeleton: {e}"))?;
            let (_, skeleton) =
                load_cooked_skeleton(&bytes).map_err(|e| format!("load skeleton {}: {e}", entry.id))?;
            skeletons.push((skeleton, Vec::new()));
        } else if entry.type_path == "regia_animation::AnimationClip" {
            let fckd = cache.join(format!("{}.fckd", entry.id));
            let bytes = std::fs::read(&fckd).map_err(|e| format!("read clip: {e}"))?;
            let (_, clip) = load_cooked_clip(&bytes).map_err(|e| format!("load clip {}: {e}", entry.id))?;
            // Identify which source path this clip belongs to
            let source = entry.path.split('#').next().unwrap_or(&entry.path).to_string();
            pending_clips.push((source, clip));
        }
    }

    // Reconstruct: each skeleton is a separate skin; assign clips to the latest skeleton
    // loaded from the same source (simplification that works for single-skin files).
    for (_skeleton, clips) in &mut skeletons {
        clips.extend(
            pending_clips
                .iter()
                .filter(|(src, _)| !src.is_empty())
                .map(|(_, clip)| clip.clone()),
        );
    }
    // De-duplicate clips that were assigned everywhere
    for (_, clips) in &mut skeletons {
        let mut seen = std::collections::HashSet::new();
        clips.retain(|c| seen.insert(c.name.clone()));
    }

    if skeletons.is_empty() {
        return Err("no skeletons cooked from the glTF files".into());
    }
    Ok(skeletons
        .into_iter()
        .map(|(sk, clips)| (Arc::new(sk), clips))
        .collect())
}

fn new_app(sound: SoundClip) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.add_plugins((MinimalPlugins, AnimationPlugin, AudioPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.insert_resource(StateLog::default());
    app.insert_resource(FootstepSound(sound));
    app.add_systems(
        Update,
        (record_states, play_footsteps)
            .after(regia_animation::systems::evaluate_animators)
            .before(drain_one_shots),
    );
    app
}

fn record_states(mut reader: MessageReader<AnimStateEvent>, mut log: ResMut<StateLog>) {
    for event in reader.read() {
        log.events.push((event.from.clone(), event.to.clone()));
    }
}

fn play_footsteps(mut reader: MessageReader<AnimNotify>, mut audio: ResMut<Audio>, clip: Res<FootstepSound>) {
    for notify in reader.read() {
        if notify.name.starts_with("Footstep")
            && let Err(error) = audio.play(&clip.0)
        {
            tracing::warn!(target: "anim_lab", "footstep: {error}");
        }
    }
}

fn step(app: &mut App, frames: u32) {
    for _ in 0..frames {
        app.update();
    }
}

fn play_cesium(app: &mut App, (skeleton, clips): CookedSkin) -> Result<String, String> {
    let clip = clips
        .first()
        .cloned()
        .ok_or_else(|| "CesiumMan has no animation clip".to_string())?;
    let deviation = bind_palette_deviation(&skeleton);
    let joints = skeleton.len();
    let name = clip.name.clone();
    let rig = spawn_single(app, skeleton, clip, true);
    step(app, 2);
    let before = all_rotations(app, rig)?;
    step(app, 40);
    let after = all_rotations(app, rig)?;
    let moved = max_angle(&before, &after);
    if moved < 0.02 {
        return Err(format!(
            "CesiumMan clip {name} did not move a joint ({moved} rad)"
        ));
    }
    assert_palette(app, rig, joints)?;
    if deviation > 0.05 {
        return Err(format!(
            "CesiumMan bind-pose skin palette deviates from identity by {deviation}"
        ));
    }
    Ok(format!(
        "anim_lab: CesiumMan joints={joints} clip={name} moved={moved:.3} rad palette_err={deviation:.2e}"
    ))
}

fn play_fox(app: &mut App, (skeleton, clips): CookedSkin) -> Result<String, String> {
    for name in [UPPER, FORE, HAND, HEAD, TAIL, SPINE] {
        require_joint(&skeleton, name)?;
    }
    let deviation = bind_palette_deviation(&skeleton);
    if deviation > 0.05 {
        return Err(format!(
            "Fox bind-pose skin palette deviates from identity by {deviation}"
        ));
    }
    let mut survey = clip_named(&clips, "Survey")?;
    let mut walk = clip_named(&clips, "Walk")?;
    let mut run = clip_named(&clips, "Run")?;
    tag_root_motion(&mut survey);
    tag_root_motion(&mut walk);
    tag_root_motion(&mut run);
    walk.notifies.push(Notify {
        time: 0.05,
        name: "Footstep_L".into(),
        payload: None,
    });
    walk.notifies.push(Notify {
        time: (walk.duration * 0.5).max(0.05),
        name: "Footstep_R".into(),
        payload: None,
    });
    let ratio = compression_ratio(&walk, &skeleton);
    assert_compression_round_trip(&walk, &skeleton)?;
    let start = walk.slice("WalkStart", 0.0, 0.2_f32.min(walk.duration));
    let land_start = (walk.duration * 0.55).min(walk.duration);
    let land = walk.slice("WalkLand", land_start, walk.duration);

    let skeleton = Arc::clone(&skeleton);
    let wave = swing_clip("Wave", &skeleton, UPPER, 0.7)?;
    let lean = swing_clip("Lean", &skeleton, TAIL, 0.6)?;
    let mut library = ClipLibrary::new();
    for clip in [survey, walk, run, start, land, wave, lean] {
        library.insert(clip);
    }

    let hero_graph = fox_graph();
    let gait = gait_graph();
    let hero = spawn_graph(app, Arc::clone(&skeleton), hero_graph.clone(), library.clone())?;
    let wave_rig = spawn_graph(app, Arc::clone(&skeleton), hero_graph.clone(), library.clone())?;
    let look_rig = spawn_graph(app, Arc::clone(&skeleton), hero_graph.clone(), library.clone())?;
    let add_rig = spawn_graph(app, Arc::clone(&skeleton), hero_graph, library.clone())?;
    let idle = spawn_graph(app, Arc::clone(&skeleton), gait.clone(), library.clone())?;
    let mid = spawn_graph(app, Arc::clone(&skeleton), gait.clone(), library.clone())?;
    let pouncer = spawn_graph(app, Arc::clone(&skeleton), gait.clone(), library.clone())?;
    let fast = spawn_graph(app, Arc::clone(&skeleton), gait, library.clone())?;

    set_float(app, hero, "Speed", 1.6)?;
    set_float(app, wave_rig, "Speed", 1.6)?;
    set_float(app, wave_rig, "WaveWeight", 1.0)?;
    set_float(app, look_rig, "Speed", 1.6)?;
    set_float(app, look_rig, "LookWeight", 1.0)?;
    set_vec3(app, look_rig, "LookTarget", Vec3::new(3.0, 2.0, 1.0))?;
    set_float(app, add_rig, "Speed", 1.6)?;
    set_float(app, add_rig, "AddWeight", 1.0)?;
    set_float(app, idle, "Speed", 0.0)?;
    set_float(app, mid, "Speed", 1.6)?;
    set_float(app, fast, "Speed", 6.0)?;

    let weapon = attach(app, hero, "weapon", HAND)?;
    let pilot = attach(app, hero, "pilot", SPINE)?;

    let mut root_travel = 0.0_f32;
    for _ in 0..24 {
        app.update();
        if let Some(motion) = app.world().get::<RootMotionOutput>(hero) {
            if !motion.delta.is_finite() || !motion.rotation.is_finite() {
                return Err("root motion output is not finite".into());
            }
            root_travel += Vec3::new(motion.delta.x, 0.0, motion.delta.z).length();
        }
    }

    let gait_delta = max_angle(&all_rotations(app, idle)?, &all_rotations(app, fast)?);
    if gait_delta < 0.05 {
        return Err(format!(
            "idle/run blend poses match ({gait_delta} rad); walker gait did not separate"
        ));
    }
    let wave_delta = joint_rotation(app, hero, UPPER)?.angle_between(joint_rotation(app, wave_rig, UPPER)?);
    if wave_delta < 0.2 {
        return Err(format!("upper-body wave did not move the arm ({wave_delta} rad)"));
    }
    let lean_delta = joint_rotation(app, hero, TAIL)?.angle_between(joint_rotation(app, add_rig, TAIL)?);
    if lean_delta < 0.15 {
        return Err(format!("additive lean did not move the tail ({lean_delta} rad)"));
    }
    let look_delta = joint_rotation(app, hero, HEAD)?.angle_between(joint_rotation(app, look_rig, HEAD)?);
    if look_delta < 0.05 {
        return Err(format!("look-at did not turn the head ({look_delta} rad)"));
    }

    assert_palette(app, hero, skeleton.len())?;
    run_until_state(app, "Loop", 80)?;
    let played = app
        .world()
        .get_resource::<Audio>()
        .ok_or("no audio")?
        .played_count();
    if played == 0 {
        return Err("footstep notifies did not play a sound".into());
    }
    assert_socket_tracks(app, hero, weapon, HAND)?;

    reach_hand(app, hero)?;
    trigger(app, hero, "Land")?;
    run_until_state(app, "Land", 80)?;
    assert_pilot_follows(app, hero, pilot)?;

    let events = app
        .world()
        .get_resource::<StateLog>()
        .ok_or("no state log")?
        .events
        .len();
    let pounce_miss = pounce(app, pouncer, library)?;
    Ok(format!(
        "anim_lab: Fox joints={} palette_err={deviation:.2e} gait={gait_delta:.3} rad wave={wave_delta:.3} lean={lean_delta:.3} look={look_delta:.3} footsteps={played} states={events} root_xz={root_travel:.3} m walk_compression={ratio:.3} pounce_miss={pounce_miss:.3} m",
        skeleton.len()
    ))
}

/// The montage case (spec 17 § 4b): a full-body "pounce" montage (the run clip, faster, two laps)
/// with a branching point, a claw window and a warp window, played over the gait graph; the
/// rig's root motion is applied to its transform each frame (the controller's job) and the warp
/// must land it on a target off to the side. Returns how far it ended from the target.
fn pounce(app: &mut App, rig: Entity, library: ClipLibrary) -> Result<f32, String> {
    let run = library.get("Run").ok_or("no Run clip")?.duration;
    let lap = run / 1.5;
    let length = lap * 2.0;
    let warp_end = length * 0.8;
    let mut montage = Montage::single("pounce", "full_body", "Run");
    montage.tracks[0].segments[0].rate = 1.5;
    montage.tracks[0].segments[0].loops = 2;
    montage.sections = vec![
        MontageSection {
            follow: None,
            name: "leap".into(),
            start: 0.0,
            next: SectionNext::Continue,
        },
        MontageSection {
            follow: None,
            name: "land".into(),
            start: lap,
            next: SectionNext::Stop,
        },
    ];
    montage.notifies = vec![
        MontageNotify {
            follow: None,
            name: "claws".into(),
            time: lap * 0.5,
            duration: lap * 0.5,
            payload: None,
            branch: false,
        },
        MontageNotify {
            follow: None,
            name: "commit".into(),
            time: lap * 0.25,
            duration: 0.0,
            payload: None,
            branch: true,
        },
    ];
    montage.warps = vec![WarpWindow {
        target: "prey".into(),
        start: 0.05,
        end: warp_end,
        translation: true,
        vertical: false,
        rotation: true,
        ..WarpWindow::new("", 0.0, 0.0)
    }];
    montage.validate().map_err(|e| e.to_string())?;
    let target = Vec3::new(1.5, 0.0, -2.0);
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], ClipLibrary::new());
    player.set_warp_target("prey", WarpTarget::at(target));
    let id = player.play(Arc::new(montage), PlayOptions::default());
    app.world_mut().entity_mut(rig).insert(player);
    let mut landed = None;
    let mut kinds = Vec::new();
    for _ in 0..400 {
        app.update();
        // The controller: drain the rig's root motion into its transform.
        let (translation, rotation, _) = app
            .world_mut()
            .get_mut::<RootMotionOutput>(rig)
            .map(|mut o| o.take())
            .unwrap_or((Vec3::ZERO, Quat::IDENTITY, 0.0));
        let mut entity = app.world_mut().entity_mut(rig);
        if let Some(mut transform) = entity.get_mut::<Transform>() {
            let step = transform.rotation * (transform.scale * translation);
            transform.translation += step;
            transform.rotation = (transform.rotation * rotation).normalize();
        }
        let player = app.world().get::<MontagePlayer>(rig).ok_or("player vanished")?;
        if landed.is_none() && player.status(id).is_some_and(|s| s.time >= warp_end - 1e-4) {
            landed = app.world().get::<Transform>(rig).map(|t| t.translation);
        }
        if player.status(id).is_none() {
            kinds = player.history().map(|e| e.kind.as_str()).collect::<Vec<_>>();
            break;
        }
    }
    let landed = landed.ok_or("the pounce never reached its warp end")?;
    for kind in [
        "started",
        "branch_point",
        "window_begin",
        "section",
        "window_end",
        "ended",
    ] {
        if !kinds.contains(&kind) {
            return Err(format!("pounce events {kinds:?} miss {kind}"));
        }
    }
    let miss = (landed - target).length();
    if miss > 0.05 {
        return Err(format!("the pounce landed {miss:.3} m from its target"));
    }
    Ok(miss)
}

fn reach_hand(app: &mut App, rig: Entity) -> Result<(), String> {
    let shoulder = joint_position(app, rig, UPPER)?;
    let elbow = joint_position(app, rig, FORE)?;
    let hand = joint_position(app, rig, HAND)?;
    let reach = (elbow - shoulder).length() + (hand - elbow).length();
    let direction = (hand - shoulder).normalize_or_zero();
    if direction.length_squared() < 0.5 || reach < 1e-3 {
        return Err(format!("fox arm has no reach (len {reach})"));
    }
    let target = shoulder + direction * (reach * 0.75);
    set_vec3(app, rig, "Pole", elbow)?;
    set_vec3(app, rig, "HandTarget", target)?;
    set_float(app, rig, "IkWeight", 1.0)?;
    app.update();
    let solved = joint_position(app, rig, HAND)?;
    let error = (solved - target).length();
    if error > 0.01 {
        return Err(format!(
            "two-bone IK missed the hand target by {error:.4} m (reach {reach:.3} m)"
        ));
    }
    Ok(())
}

fn turret(app: &mut App) -> Result<(), String> {
    let skeleton = Skeleton::from_local_bind(
        vec!["base".into(), "yaw".into(), "barrel".into()],
        vec![None, Some(0), Some(1)],
        vec![
            LocalBind::default(),
            LocalBind {
                translation: Vec3::new(0.0, 0.2, 0.0),
                ..LocalBind::default()
            },
            LocalBind {
                translation: Vec3::new(0.0, 0.0, -0.5),
                ..LocalBind::default()
            },
        ],
    )
    .map_err(|e| e.to_string())?;
    let mut library = ClipLibrary::new();
    library.insert(AnimationClip::empty("Idle", 1.0));
    let graph = AnimGraph {
        parameters: vec![ParamDecl(
            "AimTarget".into(),
            ParamValue::Vec3(Vec3::new(0.0, 0.2, -2.0)),
        )],
        root: AnimNode::LookAt(LookAtNode {
            inner: Box::new(clip_node("Idle", true)),
            joint: "barrel".into(),
            target: IkTarget::Param("AimTarget".into()),
            max_yaw: 1.4,
            max_pitch: 0.8,
            weight_param: None,
        }),
    };
    let rig = spawn_graph(app, Arc::new(skeleton), graph, library)?;
    set_vec3(app, rig, "AimTarget", Vec3::new(0.0, 0.2, -3.0))?;
    step(app, 3);
    let ahead = joint_rotation(app, rig, "barrel")?;
    set_vec3(app, rig, "AimTarget", Vec3::new(2.0, 0.2, -0.5))?;
    step(app, 3);
    let side = joint_rotation(app, rig, "barrel")?;
    let delta = ahead.angle_between(side);
    if delta < 0.05 {
        return Err(format!("turret did not follow AimTarget ({delta} rad)"));
    }
    Ok(())
}

fn fox_graph() -> AnimGraph {
    let locomotion = AnimNode::BlendSpace1D(BlendSpace1DNode {
        space: BlendSpace1D {
            param: "Speed".into(),
            samples: vec![
                BlendSample1D {
                    value: 0.0,
                    clip: "Survey".into(),
                },
                BlendSample1D {
                    value: 1.6,
                    clip: "Walk".into(),
                },
                BlendSample1D {
                    value: 6.0,
                    clip: "Run".into(),
                },
            ],
        },
        sync_group: Some("Locomotion".into()),
    });
    AnimGraph {
        parameters: vec![
            ParamDecl("Speed".into(), ParamValue::Float(0.0)),
            ParamDecl("Land".into(), ParamValue::Trigger),
            ParamDecl("WaveWeight".into(), ParamValue::Float(0.0)),
            ParamDecl("AddWeight".into(), ParamValue::Float(0.0)),
            ParamDecl("IkWeight".into(), ParamValue::Float(0.0)),
            ParamDecl("HandTarget".into(), ParamValue::Vec3(Vec3::ZERO)),
            ParamDecl("Pole".into(), ParamValue::Vec3(Vec3::ZERO)),
            ParamDecl("LookWeight".into(), ParamValue::Float(0.0)),
            ParamDecl("LookTarget".into(), ParamValue::Vec3(Vec3::new(0.0, 1.0, -2.0))),
        ],
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(AnimNode::LookAt(LookAtNode {
                inner: Box::new(AnimNode::Layer(LayerNode {
                    base: Box::new(AnimNode::StateMachine(StateMachineDef {
                        initial: "Start".into(),
                        states: vec![
                            ("Start".into(), clip_node("WalkStart", false)),
                            ("Loop".into(), locomotion),
                            ("Land".into(), clip_node("WalkLand", false)),
                        ],
                        transitions: vec![
                            TransitionDef {
                                from: Some("Start".into()),
                                to: "Loop".into(),
                                conditions: vec![Condition::ClipFinished],
                                duration: 0.12,
                                curve: TransitionCurve::Smooth,
                                interruptible: false,
                                blend: Default::default(),
                            },
                            TransitionDef {
                                from: Some("Loop".into()),
                                to: "Land".into(),
                                conditions: vec![Condition::Trigger { param: "Land".into() }],
                                duration: 0.12,
                                curve: TransitionCurve::Linear,
                                interruptible: false,
                                blend: Default::default(),
                            },
                        ],
                    })),
                    layers: vec![
                        LayerEntry {
                            graph: Box::new(clip_node("Wave", true)),
                            mask: JointMaskDef {
                                joints: vec![UPPER.into()],
                                include_children: false,
                            },
                            mode: LayerMode::Override,
                            weight_param: "WaveWeight".into(),
                            weight_inertialize: 0.0,
                        },
                        LayerEntry {
                            graph: Box::new(clip_node("Lean", true)),
                            mask: JointMaskDef {
                                joints: vec![TAIL.into()],
                                include_children: false,
                            },
                            mode: LayerMode::Additive,
                            weight_param: "AddWeight".into(),
                            weight_inertialize: 0.0,
                        },
                    ],
                })),
                joint: HEAD.into(),
                target: IkTarget::Param("LookTarget".into()),
                max_yaw: 1.2,
                max_pitch: 1.0,
                weight_param: Some("LookWeight".into()),
            })),
            chain: [UPPER.into(), FORE.into(), HAND.into()],
            target: IkTarget::Param("HandTarget".into()),
            pole: Some("Pole".into()),
            weight_param: Some("IkWeight".into()),
        }),
    }
}

fn gait_graph() -> AnimGraph {
    AnimGraph {
        parameters: vec![ParamDecl("Speed".into(), ParamValue::Float(0.0))],
        root: AnimNode::BlendSpace1D(BlendSpace1DNode {
            space: BlendSpace1D {
                param: "Speed".into(),
                samples: vec![
                    BlendSample1D {
                        value: 0.0,
                        clip: "Survey".into(),
                    },
                    BlendSample1D {
                        value: 1.6,
                        clip: "Walk".into(),
                    },
                    BlendSample1D {
                        value: 6.0,
                        clip: "Run".into(),
                    },
                ],
            },
            sync_group: Some("Locomotion".into()),
        }),
    }
}

fn clip_node(name: &str, looping: bool) -> AnimNode {
    AnimNode::Clip(ClipNode {
        clip: name.into(),
        looping,
        speed: 1.0,
        sync_group: None,
    })
}

fn swing_clip(
    name: &str,
    skeleton: &Skeleton,
    joint_name: &str,
    radians: f32,
) -> Result<AnimationClip, String> {
    let joint = skeleton
        .joint_index(joint_name)
        .ok_or_else(|| format!("no joint {joint_name}"))?;
    let bind = skeleton.bind_pose[joint as usize];
    let swung = bind.rotation * Quat::from_rotation_x(radians);
    let mut clip = AnimationClip::empty(name, 0.8);
    clip.tracks.push(JointTrack {
        joint,
        translations: TrackCurve::Static(bind.translation),
        rotations: TrackCurve::Animated {
            times: vec![0.0, 0.4, 0.8],
            values: vec![bind.rotation, swung, bind.rotation],
            exact_rotation: false,
        },
        scales: TrackCurve::Static(bind.scale),
    });
    Ok(clip)
}

fn tag_root_motion(clip: &mut AnimationClip) {
    let mut best_joint = 0_u32;
    let mut best = 0.0_f32;
    for track in &clip.tracks {
        let distance =
            (track.translations.sample(clip.duration * 0.5) - track.translations.sample(0.0)).length();
        if distance > best {
            best = distance;
            best_joint = track.joint;
        }
    }
    if best > 1e-4 {
        clip.root_motion = Some(RootMotionTrack {
            joint: best_joint,
            mode: RootMotionMode::XZ,
        });
    }
}

fn compression_ratio(clip: &AnimationClip, skeleton: &Skeleton) -> f32 {
    let raw = raw_joint_bytes(clip).max(1);
    let compressed = compress_clip(clip, Some(skeleton), &CompressionSettings::default());
    compressed.compressed_size() as f32 / raw as f32
}

fn assert_compression_round_trip(clip: &AnimationClip, skeleton: &Skeleton) -> Result<(), String> {
    let compressed = compress_clip(clip, Some(skeleton), &CompressionSettings::default());
    let restored = decompress_clip(&compressed);
    let mut worst_rotation = 0.0_f32;
    let mut worst_translation = 0.0_f32;
    for step_index in 0..=8 {
        let time = clip.duration * (step_index as f32) / 8.0;
        let mut original = skeleton.bind_pose_pose();
        let mut round_trip = skeleton.bind_pose_pose();
        clip.sample_pose(time, &mut original);
        restored.sample_pose(time, &mut round_trip);
        worst_rotation = worst_of_angles(&original, &round_trip).max(worst_rotation);
        worst_translation = worst_of_translations(&original, &round_trip).max(worst_translation);
    }
    if worst_rotation > 0.05 || worst_translation > 0.02 {
        return Err(format!(
            "compressed Fox walk drifted {worst_rotation:.4} rad / {worst_translation:.4} m"
        ));
    }
    Ok(())
}

fn worst_of_angles(a: &Pose, b: &Pose) -> f32 {
    a.rotations
        .iter()
        .zip(&b.rotations)
        .map(|(left, right)| left.angle_between(*right))
        .fold(0.0, f32::max)
}

fn worst_of_translations(a: &Pose, b: &Pose) -> f32 {
    a.translations
        .iter()
        .zip(&b.translations)
        .map(|(left, right)| (*left - *right).length())
        .fold(0.0, f32::max)
}

fn bind_palette_deviation(skeleton: &Skeleton) -> f32 {
    let pose = skeleton.bind_pose_pose();
    let globals = skeleton.globals(&pose, None);
    let palette = skeleton.skin_palette(&globals);
    let mut max = 0.0_f32;
    for matrix in palette {
        for value in (matrix - Mat4::IDENTITY).to_cols_array() {
            max = max.max(value.abs());
        }
    }
    max
}

fn clip_named(clips: &[AnimationClip], name: &str) -> Result<AnimationClip, String> {
    clips
        .iter()
        .find(|clip| clip.name == name)
        .cloned()
        .ok_or_else(|| {
            let have: Vec<&str> = clips.iter().map(|clip| clip.name.as_str()).collect();
            format!("missing clip {name}; have {have:?}")
        })
}

fn require_joint(skeleton: &Skeleton, name: &str) -> Result<(), String> {
    if skeleton.joint_index(name).is_some() {
        Ok(())
    } else {
        Err(format!("missing joint {name}; have {:?}", skeleton.joint_names))
    }
}

fn spawn_single(app: &mut App, skeleton: Arc<Skeleton>, clip: AnimationClip, looping: bool) -> Entity {
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, Animator::single(clip, looping)));
    rig
}

fn spawn_graph(
    app: &mut App,
    skeleton: Arc<Skeleton>,
    graph: AnimGraph,
    library: ClipLibrary,
) -> Result<Entity, String> {
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, Animator::graph(graph, library)));
    Ok(rig)
}

fn attach(app: &mut App, rig: Entity, name: &str, joint: &str) -> Result<Entity, String> {
    let _ = joint_entity(app, rig, joint)?;
    Ok(app
        .world_mut()
        .spawn((
            Transform::default(),
            Socket {
                rig,
                name: name.to_string(),
                joint: joint.to_string(),
                offset: Transform::default(),
            },
        ))
        .id())
}

fn set_float(app: &mut App, rig: Entity, name: &str, value: f32) -> Result<(), String> {
    let mut animator = app.world_mut().get_mut::<Animator>(rig).ok_or("no animator")?;
    animator.set(name, value);
    Ok(())
}

fn set_vec3(app: &mut App, rig: Entity, name: &str, value: Vec3) -> Result<(), String> {
    let mut animator = app.world_mut().get_mut::<Animator>(rig).ok_or("no animator")?;
    animator.set(name, value);
    Ok(())
}

fn trigger(app: &mut App, rig: Entity, name: &str) -> Result<(), String> {
    let mut animator = app.world_mut().get_mut::<Animator>(rig).ok_or("no animator")?;
    animator.trigger(name);
    Ok(())
}

fn run_until_state(app: &mut App, to: &str, max_frames: u32) -> Result<(), String> {
    for _ in 0..max_frames {
        app.update();
        if saw_state(app, to)? {
            return Ok(());
        }
    }
    let seen = app
        .world()
        .get_resource::<StateLog>()
        .map(|log| log.events.clone());
    Err(format!("never entered {to}; saw {seen:?}"))
}

fn saw_state(app: &App, to: &str) -> Result<bool, String> {
    let log = app.world().get_resource::<StateLog>().ok_or("no state log")?;
    Ok(log.events.iter().any(|(_, name)| name == to))
}

fn assert_socket_tracks(app: &App, rig: Entity, socket: Entity, joint: &str) -> Result<(), String> {
    let expected = joint_position(app, rig, joint)?;
    let actual = app
        .world()
        .get::<Transform>(socket)
        .ok_or("socket has no transform")?
        .translation;
    let error = (actual - expected).length();
    if error > 1e-3 {
        return Err(format!("socket {joint} is {error} m from the joint"));
    }
    Ok(())
}

fn assert_pilot_follows(app: &mut App, rig: Entity, pilot: Entity) -> Result<(), String> {
    if let Some(mut animator) = app.world_mut().get_mut::<Animator>(rig) {
        animator.speed = 0.0;
    }
    app.update();
    let before = app
        .world()
        .get::<Transform>(pilot)
        .ok_or("pilot has no transform")?
        .translation;
    {
        let mut rig_transform = app
            .world_mut()
            .get_mut::<Transform>(rig)
            .ok_or("rig has no transform")?;
        rig_transform.translation.x += 4.0;
    }
    app.update();
    let after = app
        .world()
        .get::<Transform>(pilot)
        .ok_or("pilot has no transform")?
        .translation;
    let moved = after.x - before.x;
    if (moved - 4.0).abs() > 1e-2 {
        return Err(format!("pilot moved {moved} m when the rig moved 4 m"));
    }
    Ok(())
}

fn assert_palette(app: &App, rig: Entity, joints: usize) -> Result<(), String> {
    let palette = app
        .world()
        .get::<SkinPalette>(rig)
        .ok_or("rig has no skin palette")?;
    if palette.matrices.len() != joints {
        return Err(format!(
            "palette has {} matrices, skeleton has {joints}",
            palette.matrices.len()
        ));
    }
    let finite = palette
        .matrices
        .iter()
        .all(|matrix| matrix.to_cols_array().iter().all(|value| value.is_finite()));
    if !finite {
        return Err("skin palette has a non-finite matrix".into());
    }
    Ok(())
}

fn joint_entity(app: &App, rig: Entity, name: &str) -> Result<Entity, String> {
    let binding = app.world().get::<SkinnedMesh>(rig).ok_or("rig has no skeleton")?;
    let index = binding
        .skeleton
        .joint_index(name)
        .ok_or_else(|| format!("missing joint {name}"))?;
    binding
        .joints
        .get(index as usize)
        .copied()
        .ok_or_else(|| format!("joint {name} has no entity"))
}

fn joint_rotation(app: &App, rig: Entity, name: &str) -> Result<Quat, String> {
    let entity = joint_entity(app, rig, name)?;
    app.world()
        .get::<Transform>(entity)
        .map(|transform| transform.rotation)
        .ok_or_else(|| format!("joint {name} has no transform"))
}

fn joint_position(app: &App, rig: Entity, name: &str) -> Result<Vec3, String> {
    let index = {
        let binding = app.world().get::<SkinnedMesh>(rig).ok_or("rig has no skeleton")?;
        binding
            .skeleton
            .joint_index(name)
            .ok_or_else(|| format!("missing joint {name}"))?
    };
    let globals = app
        .world()
        .get::<JointGlobals>(rig)
        .ok_or("rig has no joint globals")?;
    globals
        .globals
        .get(index as usize)
        .map(|matrix| matrix.w_axis.truncate())
        .ok_or_else(|| format!("joint {name} has no global"))
}

fn all_rotations(app: &App, rig: Entity) -> Result<Vec<Quat>, String> {
    let joints = app
        .world()
        .get::<SkinnedMesh>(rig)
        .ok_or("rig has no skeleton")?
        .joints
        .clone();
    let mut rotations = Vec::with_capacity(joints.len());
    for joint in joints {
        let rotation = app
            .world()
            .get::<Transform>(joint)
            .ok_or("joint has no transform")?
            .rotation;
        rotations.push(rotation);
    }
    Ok(rotations)
}

fn max_angle(a: &[Quat], b: &[Quat]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(left, right)| left.angle_between(*right))
        .fold(0.0, f32::max)
}
