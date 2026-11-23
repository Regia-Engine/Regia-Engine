// SPDX-License-Identifier: MPL-2.0
//! `.gait.ron` gait settings files (gap G7): format round trip, the same trace as the struct
//! literal, version refusal, the stepper's own error codes, the `GaitRig` handle and hot reload.
//! These tests fail to compile on the base, which has no `regia_animation::gait_asset`.
//!
//! This binary installs a counting global allocator for the allocation test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::gait::{
    GaitBody, GaitError, GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, TRIPOD_LEGS,
    leg_array, step_gait, step_gait_driven,
};
use regia_animation::gait_asset::{
    GAIT_FORMAT_VERSION, GaitAsset, GaitAssetError, GaitAssets, GaitFile, GaitPatternSpec,
    gait_migration_sources, gait_schema_fingerprint, migrate_gait_ron,
};
use regia_animation::gait_rig::{GaitLeg, GaitRig};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::*;
use regia_asset::SourceAssetChanged;
use regia_core::math::Vec3;
use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
use regia_scene::Transform;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const DT: f32 = 1.0 / 60.0;

/// The format corpus fixture: the `tripod_gait.rs` settings as a file.
const CORPUS_V1: &str = include_str!("../../regia_agent/tests/fixtures/format-corpus/gait/v1.ron");

/// The settings of `tests/tripod_gait.rs`, as a struct literal.
fn literal() -> GaitParams {
    GaitParams {
        forward_speed: 2.4,
        reverse_speed: 1.1,
        strafe_speed: 0.8,
        acceleration: 3.0,
        step_length: 0.6,
        clearance: 0.7,
        swing_lift: 0.18,
        max_slope: 0.9,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array(
            [
                [-0.4, 0.5],
                [0.4, 0.5],
                [-0.5, 0.0],
                [0.5, 0.0],
                [-0.4, -0.5],
                [0.4, -0.5],
            ],
            [0.0; 2],
        ),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [5.0; MAX_LEGS],
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
        hang_drop: 0.3,
    }
}

/// Every float of `params`, as bits, so two settings compare bit for bit.
fn bits(params: &GaitParams) -> String {
    format!("{params:?}")
}

fn legs(n: usize) -> Vec<GaitLeg> {
    (0..n)
        .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
        .collect()
}

/// A valid file around `pattern`, for its leg count, with every optional block present.
fn file_for(pattern: GaitPatternSpec) -> GaitFile {
    let legs = pattern.build().expect("preset").leg_count();
    let mut file = GaitFile::from_params(&literal(), &[], Some("body"));
    file.patterns = vec![pattern];
    file.switch_speeds.clear();
    file.hysteresis = 0.0;
    file.max_reach = Some(1.5);
    file.ground_band = None;
    file.posture = None;
    file.legs = (0..legs)
        .map(|i| regia_animation::gait_asset::GaitLegSpec {
            root: format!("hip{i}"),
            tip: format!("foot{i}"),
            hip_offset: (i % 2 == 0).then_some([0.1 * i as f32, -0.2]),
            max_reach: (i == 0).then_some(2.0),
        })
        .collect();
    file
}

#[test]
fn gait_ron_round_trips() {
    let mut presets = GaitPatternSpec::presets();
    presets.extend([
        GaitPatternSpec::Ripple(8),
        GaitPatternSpec::Wave(4),
        GaitPatternSpec::Wave(16),
    ]);
    presets.push(GaitPatternSpec::Explicit {
        offsets: vec![0.0, 0.5, 0.25],
        duty: 0.7,
    });
    for preset in presets {
        let file = file_for(preset.clone());
        let asset = GaitAsset::from_file(file.clone()).unwrap_or_else(|e| panic!("{preset:?}: {e}"));
        let text = asset.to_ron().expect("writes");
        let back = GaitAsset::from_ron(&text).unwrap_or_else(|e| panic!("{preset:?}: {e}\n{text}"));
        assert_eq!(back.file, file, "{preset:?}: the document");
        assert_eq!(
            bits(&back.params),
            bits(&asset.params),
            "{preset:?}: the settings"
        );
        assert_eq!(
            back.to_ron().expect("writes"),
            text,
            "{preset:?}: one save is a fixed point"
        );
        // The explicit writer reproduces the same settings bit for bit.
        let explicit = GaitFile::from_params(&asset.params, &asset.legs(), asset.file.body.as_deref());
        let again = GaitAsset::from_ron(&explicit.to_ron().expect("writes")).expect("explicit reloads");
        assert_eq!(
            bits(&again.params),
            bits(&asset.params),
            "{preset:?}: explicit form"
        );
    }
    // A switching set and a posture survive too.
    let mut file = file_for(GaitPatternSpec::Wave(6));
    file.patterns.push(GaitPatternSpec::Ripple(6));
    file.patterns.push(GaitPatternSpec::Tripod);
    file.switch_speeds = vec![0.6, 1.4];
    file.hysteresis = 0.1;
    file.posture = Some(regia_animation::gait_asset::GaitPostureSpec {
        swing_weight: 0.5,
        height_tau: 0.08,
        height_slew: Some(0.6),
        attitude_weight: 1.0,
        max_pitch: 0.4,
        max_roll: 0.3,
        attitude_tau: 0.1,
    });
    let asset = GaitAsset::from_file(file.clone()).expect("valid");
    let back = GaitAsset::from_ron(&asset.to_ron().expect("writes")).expect("reloads");
    assert_eq!(back.file, file);
    assert_eq!(bits(&back.params), bits(&asset.params));
    assert_eq!(asset.params.patterns.count, 3);
    assert_eq!(asset.params.posture.height_slew, 0.6);
}

#[test]
fn gait_ron_drives_the_same_trace_as_the_literal() {
    let asset = GaitAsset::from_ron(CORPUS_V1).expect("the corpus fixture loads");
    let expected = literal();
    assert_eq!(asset.params, expected);
    assert_eq!(
        bits(&asset.params),
        bits(&expected),
        "the file is the literal bit for bit"
    );
    assert_eq!(asset.legs().len(), TRIPOD_LEGS);
    fn bumpy(x: f32, z: f32) -> f32 {
        0.04 * (1.3 * x).sin() + 0.03 * (0.9 * z).cos() + 0.01 * x
    }
    // Integrating mode.
    let mut a = GaitState::at_rest(&asset.params, [0.0; 3], 0.2, bumpy).expect("rest");
    let mut b = GaitState::at_rest(&expected, [0.0; 3], 0.2, bumpy).expect("rest");
    for i in 0..900 {
        let input = [
            1.0,
            0.3 * ((i / 120) % 2) as f32,
            if i > 450 { -0.6 } else { 0.4 },
        ];
        step_gait(&asset.params, &mut a, input, DT, bumpy).expect("step");
        step_gait(&expected, &mut b, input, DT, bumpy).expect("step");
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "integrating step {i}");
    }
    // Driven mode.
    let body = |i: usize| GaitBody {
        position: [0.0, 0.7, -1.2 * DT * i as f32],
        yaw: 0.01 * i as f32,
        velocity: [0.0, 0.0, -1.2],
    };
    let mut a = GaitState::at_rest_driven(&asset.params, body(0), bumpy).expect("rest");
    let mut b = GaitState::at_rest_driven(&expected, body(0), bumpy).expect("rest");
    for i in 1..600 {
        step_gait_driven(&asset.params, &mut a, body(i), DT, bumpy).expect("step");
        step_gait_driven(&expected, &mut b, body(i), DT, bumpy).expect("step");
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "driven step {i}");
    }
}

#[test]
fn newer_gait_version_is_refused() {
    let newer = CORPUS_V1.replacen("format_version: 1", "format_version: 2", 1);
    let error = GaitAsset::from_ron(&newer).expect_err("v2 is newer than this build");
    assert_eq!(error.code(), "gait_version");
    assert_eq!(
        error,
        GaitAssetError::Version {
            found: 2,
            supported: GAIT_FORMAT_VERSION
        }
    );
    // The version is read before the typed parse: a newer file with a field this build does not
    // know still says gait_version, not gait_parse.
    let future = newer.replacen("speeds:", "gait_future_field: 3,\n    speeds:", 1);
    assert_eq!(
        GaitAsset::from_ron(&future).expect_err("newer").code(),
        "gait_version"
    );
    let zero = CORPUS_V1.replacen("format_version: 1", "format_version: 0", 1);
    assert_eq!(
        GaitAsset::from_ron(&zero).expect_err("v0 never existed").code(),
        "gait_version"
    );
    // The migrator: v1 passes through, newer is refused, nothing registered yet.
    assert_eq!(migrate_gait_ron(CORPUS_V1).expect("v1 passes"), CORPUS_V1);
    assert_eq!(
        migrate_gait_ron(&newer).expect_err("newer").code(),
        "gait_version"
    );
    assert!(gait_migration_sources().is_empty());
    // A file with no version is a parse error.
    let missing = CORPUS_V1.replacen("format_version: 1,", "", 1);
    assert_eq!(
        GaitAsset::from_ron(&missing).expect_err("no version").code(),
        "gait_parse"
    );
}

#[test]
fn invalid_gait_ron_reports_gait_settings() {
    // A negative step length: the stepper's own validate refuses it, with its own code.
    let bad = CORPUS_V1.replacen("step_length: 0.6", "step_length: -0.6", 1);
    assert_ne!(bad, CORPUS_V1);
    let error = GaitAsset::from_ron(&bad).expect_err("negative step length");
    assert_eq!(error.code(), "gait_settings");
    assert_eq!(error, GaitAssetError::Gait(GaitError::Settings));
    let mut params = literal();
    params.step_length = -0.6;
    assert_eq!(
        params.validate(),
        Err(GaitError::Settings),
        "the same verdict as the stepper"
    );
    assert!(error.to_string().starts_with("gait_settings"), "{error}");

    // Every other code a file can produce.
    let cases = [
        // A posture out of range is a stepper setting too.
        (
            CORPUS_V1.replacen(
                "    legs: [",
                "    posture: Some((swing_weight: 2.0, height_tau: 0.0, attitude_weight: 0.0, max_pitch: 0.0, max_roll: 0.0, attitude_tau: 0.0)),\n    legs: [",
                1,
            ),
            "gait_settings",
        ),
        (CORPUS_V1.replacen("patterns: [Tripod]", "patterns: [Wave(5)]", 1), "gait_pattern"),
        (CORPUS_V1.replacen("patterns: [Tripod]", "patterns: []", 1), "gait_pattern"),
        (
            CORPUS_V1.replacen("patterns: [Tripod]", "patterns: [Explicit(offsets: [0.0, 1.5], duty: 0.5)]", 1),
            "gait_pattern",
        ),
        (
            CORPUS_V1.replacen("patterns: [Tripod]", "patterns: [Tripod, Ripple(6)]", 1),
            "gait_pattern",
        ),
        (CORPUS_V1.replacen("patterns: [Tripod]", "patterns: [Trot]", 1), "gait_legs"),
        (CORPUS_V1.replacen("root: \"coxa_fl\"", "root: \"\"", 1), "gait_legs"),
        (CORPUS_V1.replacen("speeds:", "speed_typo: 1.0,\n    speeds:", 1), "gait_parse"),
        (CORPUS_V1.replacen("step_length: 0.6,", "", 1), "gait_parse"),
        ("not ron at all".to_string(), "gait_parse"),
    ];
    for (text, code) in cases {
        assert_ne!(text, CORPUS_V1, "the edit for {code} applied");
        let error = GaitAsset::from_ron(&text).expect_err(code);
        assert_eq!(error.code(), code, "{error}");
    }
}

#[test]
fn the_frozen_format_is_covered() {
    let fingerprint = gait_schema_fingerprint();
    for needle in [
        "format_version",
        "patterns",
        "switch_speeds",
        "hysteresis",
        "speeds",
        "yaw_rate",
        "settle",
        "turn",
        "step_angle",
        "ground_band",
        "foothold",
        "candidates",
        "posture",
        "height_slew",
        "hip_offset",
        "max_reach",
        "Explicit",
        "BipedWalk",
    ] {
        assert!(fingerprint.contains(needle), "{needle} is not in {fingerprint}");
    }
    let doc = include_str!("../../../docs/formats/gait.example.ron");
    GaitAsset::from_ron(doc).expect("the documented example loads");
}

// ---- GaitRig on a handle ---------------------------------------------------------------------

const BODY_HEIGHT: f32 = 0.5;
const PAIR_Z: [f32; 3] = [-0.5, 0.0, 0.5];

/// A hexapod: `root` at the floor, `body` 0.5 m up, six two-bone legs with tips on the floor.
fn hexapod() -> Arc<Skeleton> {
    let mut names = vec!["root".to_string(), "body".to_string()];
    let mut parents = vec![None, Some(0)];
    let mut binds = vec![
        LocalBind::default(),
        LocalBind {
            translation: Vec3::new(0.0, BODY_HEIGHT, 0.0),
            ..LocalBind::default()
        },
    ];
    for leg in 0..TRIPOD_LEGS {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let segments = [
            Vec3::new(side * 0.25, 0.0, PAIR_Z[leg / 2]),
            Vec3::new(side * 0.2, 0.15, 0.0),
            Vec3::new(side * 0.15, -0.65, 0.0),
        ];
        let mut parent = 1_u32;
        for (j, translation) in segments.into_iter().enumerate() {
            names.push(match j {
                0 => format!("hip{leg}"),
                1 => format!("knee{leg}"),
                _ => format!("foot{leg}"),
            });
            parents.push(Some(parent));
            binds.push(LocalBind {
                translation,
                ..LocalBind::default()
            });
            parent = (names.len() - 1) as u32;
        }
    }
    Arc::new(Skeleton::from_local_bind(names, parents, binds).expect("hexapod"))
}

/// Rig settings as a file: the walker of `tests/gait_rig.rs`, hips measured from the bind pose.
fn rig_ron(step_length: f32) -> String {
    let legs: String = (0..TRIPOD_LEGS)
        .map(|i| format!("        (root: \"hip{i}\", tip: \"foot{i}\"),\n"))
        .collect();
    format!(
        "(
    format_version: 1,
    patterns: [Tripod],
    speeds: (forward: 0.6, reverse: 0.3, strafe: 0.3, acceleration: 2.0, idle: 0.02, yaw_rate: 0.8),
    step_length: {step_length:?},
    swing_lift: 0.1,
    clearance: 0.5,
    max_slope: 1.5,
    settle: (distance: 0.05, period: 0.3),
    turn: (step_distance: 0.1, step_angle: 0.35),
    ground_band: Some(0.6),
    foothold: (candidates: 0, spacing: 0.2),
    hang_drop: 0.2,
    body: Some(\"body\"),
    legs: [
{legs}    ],
)
"
    )
}

fn pin_schedules(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.add_message::<SourceAssetChanged>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    app
}

fn spawn(app: &mut App, rig: GaitRig) -> Entity {
    let animator = Animator::single(AnimationClip::empty("still", 1.0), true);
    let binding = spawn_skeleton(app.world_mut(), hexapod());
    app.world_mut()
        .spawn((Transform::default(), binding, animator, rig))
        .id()
}

/// Walk `rig` forward (−Z) for `frames` frames.
fn walk(app: &mut App, rig: Entity, from: usize, frames: usize) {
    for frame in from..from + frames {
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("transform")
            .translation
            .z = -0.4 * DT * frame as f32;
        app.update();
    }
}

#[test]
fn gait_rig_uses_a_gait_asset_handle() {
    // A rig on a handle walks exactly like the same rig built from the struct literal.
    let mut app_asset = app();
    let handle = app_asset
        .world_mut()
        .resource_mut::<GaitAssets>()
        .load_text("units/walker.gait.ron", &rig_ron(0.3))
        .expect("loads");
    let asset = app_asset
        .world()
        .resource::<GaitAssets>()
        .get(&handle)
        .expect("stored")
        .clone();
    assert_eq!(asset.file.body.as_deref(), Some("body"));
    let on_handle = spawn(&mut app_asset, GaitRig::from_asset(handle.clone(), &asset));
    let rig = app_asset.world().get::<GaitRig>(on_handle).expect("rig");
    assert_eq!(rig.asset, Some(handle.clone()));
    assert_eq!(rig.body.as_deref(), Some("body"));
    assert_eq!(rig.legs, legs(TRIPOD_LEGS));

    let mut app_literal = app();
    let literal = spawn(
        &mut app_literal,
        GaitRig::new(asset.params, legs(TRIPOD_LEGS)).with_body("body"),
    );
    walk(&mut app_asset, on_handle, 0, 90);
    walk(&mut app_literal, literal, 0, 90);
    let a = app_asset.world().get::<GaitState>(on_handle).expect("state");
    let b = app_literal.world().get::<GaitState>(literal).expect("state");
    assert!(a.position[2] < -0.5, "the rig walked: {:?}", a.position);
    assert_eq!(format!("{a:?}"), format!("{b:?}"), "the same walk bit for bit");
    assert_eq!(
        app_asset
            .world()
            .resource::<GaitAssets>()
            .handle("units/walker.gait.ron"),
        Some(handle)
    );
}

#[test]
fn hip_offset_overrides_keep_their_value() {
    // Leg 0 gets a hip offset in the file; the others are measured from the bind pose.
    let text = rig_ron(0.3).replacen(
        "(root: \"hip0\", tip: \"foot0\")",
        "(root: \"hip0\", tip: \"foot0\", hip_offset: Some((-0.9, 0.7)))",
        1,
    );
    let mut with_override = app();
    let handle = with_override
        .world_mut()
        .resource_mut::<GaitAssets>()
        .load_text("w.gait.ron", &text)
        .expect("loads");
    let asset = with_override
        .world()
        .resource::<GaitAssets>()
        .get(&handle)
        .expect("stored")
        .clone();
    assert_eq!(asset.hip_override_mask(), 0b1);
    let rig = spawn(&mut with_override, GaitRig::from_asset(handle, &asset));
    walk(&mut with_override, rig, 0, 1);
    let state = with_override.world().get::<GaitState>(rig).expect("state");
    // At rest the planted foot is its neutral: the hip offset in the stepper's frame (driven:
    // [right, forward], forward −Z), the rig at the origin facing −Z.
    let foot0 = state.feet[0].position;
    assert!(
        (foot0[0] - -0.9).abs() < 1e-5 && (foot0[2] - -0.7).abs() < 1e-5,
        "{foot0:?}"
    );
    // Leg 1 kept the measured offset: its bind tip at x = +0.6, z = -0.5.
    let foot1 = state.feet[1].position;
    assert!(
        (foot1[0] - 0.6).abs() < 1e-5 && (foot1[2] - -0.5).abs() < 1e-5,
        "{foot1:?}"
    );
}

fn write_atomic(path: &Path, text: &str) {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, text).expect("write");
    std::fs::rename(&temp, path).expect("rename");
}

#[test]
fn reloaded_gait_asset_applies_next_frame() {
    let dir = regia_test_utils::unique_temp_dir("gait-asset-reload");
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("walker.gait.ron");
    write_atomic(&path, &rig_ron(0.3));
    let mut app = app();
    let handle = app
        .world_mut()
        .resource_mut::<GaitAssets>()
        .load_file(&path)
        .expect("loads");
    let asset = app
        .world()
        .resource::<GaitAssets>()
        .get(&handle)
        .expect("stored")
        .clone();
    let rig = spawn(&mut app, GaitRig::from_asset(handle.clone(), &asset));
    walk(&mut app, rig, 0, 10);
    assert_eq!(app.world().resource::<GaitAssets>().generation(&handle), 1);

    // A good edit: the next frame's step uses it.
    write_atomic(&path, &rig_ron(0.45));
    app.world_mut()
        .write_message(SourceAssetChanged::new(path.clone()));
    walk(&mut app, rig, 10, 1);
    let gaits = app.world().resource::<GaitAssets>();
    assert_eq!(gaits.generation(&handle), 2);
    assert!(gaits.error(&handle).is_none());
    assert_eq!(
        app.world().get::<GaitRig>(rig).expect("rig").params.step_length,
        0.45
    );

    // A bad edit is validated on reload and refused: the rig keeps 0.45, the error is recorded.
    write_atomic(&path, &rig_ron(-1.0));
    app.world_mut()
        .write_message(SourceAssetChanged::new(path.clone()));
    walk(&mut app, rig, 11, 1);
    let gaits = app.world().resource::<GaitAssets>();
    assert_eq!(gaits.generation(&handle), 2, "a refused reload stores nothing");
    assert_eq!(
        gaits.error(&handle).map(GaitAssetError::code),
        Some("gait_settings")
    );
    assert_eq!(
        app.world().get::<GaitRig>(rig).expect("rig").params.step_length,
        0.45
    );

    // Fixed again: back to good, the error clears.
    write_atomic(&path, &rig_ron(0.35));
    app.world_mut()
        .write_message(SourceAssetChanged::new(path.clone()));
    walk(&mut app, rig, 12, 1);
    let gaits = app.world().resource::<GaitAssets>();
    assert_eq!(gaits.generation(&handle), 3);
    assert!(gaits.error(&handle).is_none());
    assert_eq!(
        app.world().get::<GaitRig>(rig).expect("rig").params.step_length,
        0.35
    );
    // Another file's change reads nothing.
    let other = dir.join("other.gait.ron");
    app.world_mut().write_message(SourceAssetChanged::new(other));
    walk(&mut app, rig, 13, 1);
    assert_eq!(app.world().resource::<GaitAssets>().generation(&handle), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gait_asset_rig_allocates_nothing_after_warmup() {
    fn frame_counts(on_asset: bool) -> Vec<u64> {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<GaitAssets>()
            .load_text("w.gait.ron", &rig_ron(0.3))
            .expect("loads");
        let asset = app
            .world()
            .resource::<GaitAssets>()
            .get(&handle)
            .expect("stored")
            .clone();
        let rig = if on_asset {
            GaitRig::from_asset(handle, &asset)
        } else {
            GaitRig::new(asset.params, legs(TRIPOD_LEGS)).with_body("body")
        };
        let entity = spawn(&mut app, rig);
        pin_schedules(&mut app);
        walk(&mut app, entity, 0, 120);
        (120..180)
            .map(|frame| {
                let ((), stats) = alloc_counter::measure(|| walk(&mut app, entity, frame, 1));
                stats.allocs
            })
            .collect()
    }
    let on_asset = frame_counts(true);
    let in_code = frame_counts(false);
    println!("gait asset rig allocs per frame: asset {on_asset:?}, code {in_code:?}");
    assert_eq!(
        on_asset.iter().sum::<u64>(),
        in_code.iter().sum::<u64>(),
        "a rig on a gait asset allocated: {on_asset:?} vs {in_code:?}"
    );
}
