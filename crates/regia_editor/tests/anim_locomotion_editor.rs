// SPDX-License-Identifier: MPL-2.0
//! The animation editor's locomotion authoring (ADR-8806): the test bed walks a gait over every
//! ground preset with deterministic metrics that match `anim.gait_simulate`, GaitRig and
//! FootPlacement edits from the Inspector sections undo, saving writes a `.gait.ron` the loader
//! reads back bit for bit, and `editor.anim_locomotion_preview` answers metrics and every
//! documented error code. No window; the screenshot test needs a GPU adapter (lavapipe).
//!
//! This binary installs a counting global allocator (per thread) for the no-allocation test.

#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_agent::{LocalReply, call_local};
use regia_animation::foot_ik::{FootLegDesc, FootPlacement};
use regia_animation::gait::GaitParams;
use regia_animation::gait_asset::{GaitPatternSpec, load_gait_file};
use regia_animation::gait_rig::{GaitLeg, GaitRig};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_editor::agent_ext::EditorProject;
use regia_editor::anim::AnimEditor;
use regia_editor::anim::locomotion_panel::{
    GroundPreset, LocomotionDriver, LocomotionMetrics, LocomotionTestBed, apply_pattern_preset, default_gait,
    foot_placement_section, gait_rig_section, load_gait_into, save_gait_ron, set_foot_setting, set_gait_leg,
    set_gait_setting,
};
use regia_editor::command::UndoStack;
use serde_json::{Value, json};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator and counts calls on the calling thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

/// Six legs named like a hexapod's joint tree.
fn hexapod_legs() -> Vec<GaitLeg> {
    (0..6)
        .map(|i| GaitLeg::new(format!("coxa{i}"), format!("foot{i}")))
        .collect()
}

fn editor_world() -> World {
    let mut world = World::new();
    world.init_resource::<AppTypeRegistry>();
    {
        let registry = world.resource::<AppTypeRegistry>().clone();
        let mut registry = registry.write();
        registry.register::<GaitRig>();
        registry.register::<FootPlacement>();
    }
    world.init_resource::<AnimEditor>();
    world.init_resource::<UndoStack>();
    world
}

fn walker(world: &mut World) -> Entity {
    let mut rig = GaitRig::new(default_gait(), hexapod_legs());
    rig.auto_hips = false;
    world.spawn(rig).id()
}

fn undo(world: &mut World) -> bool {
    world.resource_scope(|world, mut stack: Mut<UndoStack>| stack.undo(world))
}

fn redo(world: &mut World) -> bool {
    world.resource_scope(|world, mut stack: Mut<UndoStack>| stack.redo(world))
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let handler = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .map(|(_, h)| h)
        .unwrap_or_else(|| panic!("no {method}"));
    handler(world, &params)
}

fn ok(world: &mut World, params: Value) -> Value {
    call(world, "editor.anim_locomotion_preview", params.clone())
        .unwrap_or_else(|e| panic!("editor.anim_locomotion_preview {params}: {e:?}"))
}

/// The protocol code of a refused call (the `data.code`), after checking it carries a hint.
fn code(world: &mut World, params: Value) -> String {
    match call(world, "editor.anim_locomotion_preview", params.clone()) {
        Ok(value) => panic!("{params} was not refused: {value}"),
        Err(error) => {
            let data = error.data.as_ref().expect("error data");
            assert!(data.get("hint").and_then(Value::as_str).is_some(), "{error:?}");
            data["code"].as_str().expect("code").to_owned()
        }
    }
}

fn refuse(world: &mut World, params: Value, want: &str) {
    assert_eq!(code(world, params.clone()), want, "{params}");
}

fn f(value: &Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
}

/// Every preset, plus the documented defaults' variants the brief names.
fn grounds() -> Vec<GroundPreset> {
    vec![
        GroundPreset::Flat,
        GroundPreset::Slope { degrees: 12.0 },
        GroundPreset::Stairs {
            rise: 0.12,
            run: 0.35,
        },
        GroundPreset::Rubble {
            amplitude: 0.08,
            cell: 0.4,
            seed: 11,
        },
        GroundPreset::Tunnel { depth: 3.0 },
    ]
}

#[test]
fn locomotion_preview_reports_metrics_for_each_ground_preset() {
    for ground in grounds() {
        for driver in [
            LocomotionDriver {
                speed: 1.0,
                turn_rate: 0.0,
            },
            LocomotionDriver {
                speed: 0.8,
                turn_rate: 0.3,
            },
        ] {
            let mut bed = LocomotionTestBed::new(default_gait(), ground, driver);
            let metrics = bed
                .run(6.0)
                .unwrap_or_else(|e| panic!("{ground:?} {driver:?}: {e}"));
            let state = bed.state.expect("walker");
            assert_eq!(metrics.steps, 360, "{ground:?}");
            assert!(
                metrics.max_foot_jump > 0.0,
                "{ground:?} {driver:?}: the feet stepped"
            );
            assert!(metrics.max_foot_jump < 0.5, "{ground:?} {driver:?}: {metrics:?}");
            assert!(
                metrics.max_slide.is_finite() && metrics.max_slide < 1.0e-3,
                "{ground:?}: {metrics:?}"
            );
            assert!(metrics.min_planted >= 1, "{ground:?}: {metrics:?}");
            assert!(
                metrics.min_stability_margin.is_finite(),
                "{ground:?}: {metrics:?}"
            );
            // The body travelled about speed x time (an arc when turning).
            let travel = Vec3::new(state.position[0], 0.0, state.position[2]).length();
            assert!(
                travel > 0.5 * driver.speed * 6.0 * 0.8,
                "{ground:?} {driver:?}: {travel}"
            );
            // Deterministic: a second bed gives the same bits.
            let mut again = LocomotionTestBed::new(default_gait(), ground, driver);
            let second = again.run(6.0).expect("second run");
            assert_eq!(
                format!("{metrics:?}"),
                format!("{second:?}"),
                "{ground:?} {driver:?}"
            );
            assert_eq!(
                format!("{:?}", bed.state),
                format!("{:?}", again.state),
                "{ground:?}"
            );
            match ground {
                GroundPreset::Slope { .. } | GroundPreset::Stairs { .. } => {
                    assert!(
                        state.position[1] > default_gait().clearance + 0.2,
                        "{ground:?} climbs: {state:?}"
                    );
                }
                GroundPreset::Tunnel { depth } => {
                    // Every foot stays on the tunnel floor, never on the layer above.
                    for foot in state.feet() {
                        assert!(foot.position[1] < depth * 0.5, "{ground:?}: {foot:?}");
                    }
                }
                _ => {}
            }
        }
    }
}

/// Straight runs over the four grounds `anim.gait_simulate` knows give its metrics bit for bit
/// (the test bed measures with the agent's definitions on the agent's ground expressions).
#[test]
fn test_bed_metrics_match_anim_gait_simulate() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let degrees = 12.0_f32;
    let cases = [
        (GroundPreset::Flat, json!({ "kind": "flat" })),
        (
            GroundPreset::Slope { degrees },
            json!({ "kind": "slope", "rise_z": -degrees.to_radians().tan() }),
        ),
        (
            GroundPreset::Stairs {
                rise: 0.12,
                run: 0.35,
            },
            json!({ "kind": "steps", "rise": -0.12_f32, "run": 0.35_f32, "axis": "z" }),
        ),
        (
            GroundPreset::Rubble {
                amplitude: 0.08,
                cell: 0.4,
                seed: 11,
            },
            json!({ "kind": "rubble", "amplitude": 0.08_f32, "cell": 0.4_f32, "seed": 11 }),
        ),
    ];
    for (ground, agent_ground) in cases {
        let driver = LocomotionDriver {
            speed: 1.0,
            turn_rate: 0.0,
        };
        let mut bed = LocomotionTestBed::new(default_gait(), ground, driver);
        let m = bed.run(5.0).expect("run");
        let params = json!({
            "steps": m.steps,
            "dt": 1.0_f32 / 60.0,
            "ground": agent_ground,
            "body_path": { "points": [[0.0, 0.0], [0.0, -4096.0]], "speed": 1.0 },
        });
        let reply = match call_local(
            app.world_mut(),
            "anim.gait_simulate",
            params,
            Some("locomotion-editor-test"),
        ) {
            LocalReply::Done(Ok(value)) => value,
            other => panic!("anim.gait_simulate: {other:?}"),
        };
        let agent = &reply["metrics"];
        let same = |ours: f32, key: &str| {
            assert_eq!(
                ours,
                f(&agent[key]) as f32,
                "{ground:?}: {key} ({ours} vs {})",
                agent[key]
            );
        };
        same(m.max_slide, "max_planted_slip");
        same(m.max_foot_jump, "max_foot_jump");
        same(m.min_stability_margin, "min_stability_margin");
        assert_eq!(
            u64::from(m.forced_steps),
            agent["forced_steps"].as_u64().expect("forced"),
            "{ground:?}"
        );
        assert_eq!(
            u64::from(m.refused),
            agent["refused"].as_u64().expect("refused"),
            "{ground:?}"
        );
        assert_eq!(
            m.min_planted as u64,
            agent["min_planted"].as_u64().expect("planted"),
            "{ground:?}"
        );
        let fin = bed.state.expect("state");
        for (k, v) in fin.position.iter().enumerate() {
            assert_eq!(
                *v,
                f(&reply["final"]["position"][k]) as f32,
                "{ground:?}: position {k}"
            );
        }
    }
}

#[test]
fn test_bed_step_allocates_nothing() {
    let mut bed = LocomotionTestBed::new(
        default_gait(),
        GroundPreset::Rubble {
            amplitude: 0.1,
            cell: 0.3,
            seed: 3,
        },
        LocomotionDriver {
            speed: 1.2,
            turn_rate: 0.4,
        },
    );
    for _ in 0..60 {
        bed.step().expect("warm-up");
    }
    let before = allocations();
    for _ in 0..600 {
        bed.step().expect("step");
    }
    assert_eq!(allocations() - before, 0, "600 test bed steps allocated");
}

#[test]
fn editing_gait_settings_is_undoable() {
    let mut world = editor_world();
    let entity = walker(&mut world);
    let step = |w: &World| w.get::<GaitRig>(entity).expect("rig").params.step_length;
    assert!(set_gait_setting(&mut world, entity, "step_length", 0.8).expect("edit"));
    assert_eq!(step(&world), 0.8);
    assert!(undo(&mut world));
    assert_eq!(step(&world), 0.6);
    assert!(redo(&mut world));
    assert_eq!(step(&world), 0.8);
    // Out of range or refused by the stepper: no change, no undo entry.
    let depth = world.resource::<UndoStack>().undo_depth();
    assert!(set_gait_setting(&mut world, entity, "step_length", 50.0).is_err());
    assert!(set_gait_setting(&mut world, entity, "step_length", f32::NAN).is_err());
    assert!(set_gait_setting(&mut world, entity, "no_such", 1.0).is_err());
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
    assert_eq!(step(&world), 0.8);
    // The same value again is not an edit.
    assert!(!set_gait_setting(&mut world, entity, "step_length", 0.8).expect("same"));
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);

    // A pattern preset is one step; a preset for another leg count is refused.
    let tripod = world.get::<GaitRig>(entity).expect("rig").params.patterns;
    assert!(apply_pattern_preset(&mut world, entity, &GaitPatternSpec::Wave(0)).expect("wave"));
    let wave = world.get::<GaitRig>(entity).expect("rig").params.patterns;
    assert_ne!(wave, tripod);
    assert_eq!(wave.leg_count(), 6);
    let err = apply_pattern_preset(&mut world, entity, &GaitPatternSpec::Trot).expect_err("trot has 4 legs");
    assert!(err.contains("gait_pattern"), "{err}");
    assert!(undo(&mut world));
    assert_eq!(world.get::<GaitRig>(entity).expect("rig").params.patterns, tripod);

    // Leg chain pickers: rename a leg, then undo.
    assert!(set_gait_leg(&mut world, entity, 2, "hip_l", "toe_l").expect("leg"));
    assert_eq!(world.get::<GaitRig>(entity).expect("rig").legs[2].tip, "toe_l");
    assert!(undo(&mut world));
    assert_eq!(world.get::<GaitRig>(entity).expect("rig").legs[2].tip, "foot2");
    assert!(set_gait_leg(&mut world, entity, 9, "a", "b").is_err());

    // FootPlacement settings and legs undo the same way.
    let foot = world.spawn(FootPlacement::default()).id();
    let lift = world
        .get::<FootPlacement>(foot)
        .expect("foot")
        .settings
        .plant_lift;
    assert!(set_foot_setting(&mut world, foot, "plant_lift", 0.05).expect("foot edit"));
    assert_eq!(
        world
            .get::<FootPlacement>(foot)
            .expect("foot")
            .settings
            .plant_lift,
        0.05
    );
    assert!(undo(&mut world));
    assert_eq!(
        world
            .get::<FootPlacement>(foot)
            .expect("foot")
            .settings
            .plant_lift,
        lift
    );
    let leg = FootLegDesc {
        hip: "thigh_l".into(),
        knee: "shin_l".into(),
        ankle: "foot_l".into(),
        toe: None,
        pole_offset: Vec3::ZERO,
    };
    assert!(regia_editor::anim::locomotion_panel::set_foot_leg(&mut world, foot, 0, leg).expect("leg"));
    assert_eq!(world.get::<FootPlacement>(foot).expect("foot").legs.len(), 1);
    assert!(undo(&mut world));
    assert!(world.get::<FootPlacement>(foot).expect("foot").legs.is_empty());
}

#[test]
fn saving_writes_a_valid_gait_ron() {
    let dir = regia_test_utils::unique_temp_dir("anim-locomotion-save");
    let path = dir.join("walker.gait.ron");
    let mut world = editor_world();
    let entity = walker(&mut world);
    set_gait_setting(&mut world, entity, "swing_lift", 0.22).expect("edit");
    let bytes = save_gait_ron(&world, entity, &path).expect("save");
    assert!(bytes > 0);
    // Atomic: the target and nothing else (no temporary sibling left).
    let names: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("walker.gait.ron")]);
    let loaded = load_gait_file(&path).expect("the saved file loads");
    let rig = world.get::<GaitRig>(entity).expect("rig");
    assert_eq!(
        loaded.params, rig.params,
        "the file holds the rig's settings bit for bit"
    );
    let legs: Vec<_> = loaded.legs().into_iter().map(|l| (l.root, l.tip)).collect();
    let want: Vec<_> = rig.legs.iter().map(|l| (l.root.clone(), l.tip.clone())).collect();
    assert_eq!(legs, want);

    // The picker loads it into another rig as one undo step.
    let mut other = GaitRig::new(
        GaitParams {
            step_length: 0.3,
            ..default_gait()
        },
        hexapod_legs(),
    );
    other.auto_hips = false;
    let other = world.spawn(other).id();
    assert!(load_gait_into(&mut world, other, &path).expect("load"));
    assert_eq!(world.get::<GaitRig>(other).expect("rig").params, loaded.params);
    assert!(undo(&mut world));
    assert_eq!(world.get::<GaitRig>(other).expect("rig").params.step_length, 0.3);

    // Settings the loader would refuse are not written.
    let bad = world
        .spawn(GaitRig::new(default_gait(), hexapod_legs()[..5].to_vec()))
        .id();
    let refused = dir.join("bad.gait.ron");
    let err = save_gait_ron(&world, bad, &refused).expect_err("five legs for a tripod");
    assert_eq!(err.code(), "gait_legs", "{err}");
    assert!(!refused.exists());
}

#[test]
fn inspector_sections_draw_for_gait_rig_and_foot_placement() {
    let mut world = editor_world();
    let entity = walker(&mut world);
    let foot = world.spawn(FootPlacement::default()).id();
    let ctx = egui::Context::default();
    for _ in 0..2 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 1400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                gait_rig_section(ui, &mut world, entity);
                foot_placement_section(ui, &mut world, foot);
                regia_editor::anim::locomotion_panel::anim_locomotion_panel(ui, &mut world);
            });
        });
        output.textures_delta.clear();
    }
    // Drawing is not an edit.
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
}

fn write(dir: &std::path::Path, name: &str, text: &str) {
    std::fs::write(dir.join("assets").join(name), text).expect("write fixture");
}

#[test]
fn editor_anim_locomotion_preview() {
    let root = regia_test_utils::unique_temp_dir("anim-locomotion-preview");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let mut world = editor_world();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    let entity = walker(&mut world);

    // Every ground preset by name and by object; metrics present, deterministic.
    for name in ["flat", "slope", "stairs", "rubble", "tunnel"] {
        let a = ok(&mut world, json!({ "ground": name, "seconds": 3 }));
        let b = ok(&mut world, json!({ "ground": name, "seconds": 3 }));
        assert_eq!(a, b, "{name}");
        assert_eq!(a["ground"]["kind"], name);
        assert_eq!(a["source"], "default");
        assert_eq!(a["steps"], 180);
        assert_eq!(a["legs"], 6);
        let m = &a["metrics"];
        for key in ["max_slide", "max_foot_jump"] {
            assert!(f(&m[key]) >= 0.0, "{name}: {key}");
        }
        assert!(f(&m["max_foot_jump"]) > 0.0, "{name}: the walker stepped");
        assert!(
            m["forced_steps"].is_u64() && m["min_planted"].is_u64(),
            "{name}: {m}"
        );
        assert!(a["screenshot"].is_null());
    }
    let stairs = ok(
        &mut world,
        json!({ "ground": { "kind": "stairs", "rise": 0.1, "run": 0.4 }, "speed": 0.6, "turn_rate": 0.2, "seconds": 4 }),
    );
    assert_eq!(stairs["ground"]["rise"].as_f64().map(|v| v as f32), Some(0.1));
    assert_eq!(stairs["turn_rate"].as_f64().map(|v| v as f32), Some(0.2));
    // The run is now the panel's test bed.
    let bed = world.resource::<AnimEditor>().locomotion.clone();
    assert_eq!(bed.ground, GroundPreset::Stairs { rise: 0.1, run: 0.4 });
    assert_eq!(bed.metrics.steps, 240);
    let metrics: LocomotionMetrics = bed.metrics;
    assert_eq!(f(&stairs["metrics"]["max_slide"]) as f32, metrics.max_slide);

    // An entity's GaitRig, and a saved .gait.ron of it, walk the same.
    let from_entity = ok(&mut world, json!({ "entity": entity.to_bits(), "seconds": 2 }));
    assert_eq!(from_entity["source"]["entity"], entity.to_bits());
    save_gait_ron(&world, entity, &root.join("assets/walker.gait.ron")).expect("save");
    let from_asset = ok(
        &mut world,
        json!({ "asset": "assets/walker.gait.ron", "seconds": 2 }),
    );
    assert_eq!(from_asset["source"]["asset"], "assets/walker.gait.ron");
    assert_eq!(from_entity["metrics"], from_asset["metrics"]);

    // Error codes.
    refuse(&mut world, json!({ "ground": "lava" }), "bad_ground");
    refuse(
        &mut world,
        json!({ "ground": { "kind": "slope", "degrees": 80 } }),
        "bad_ground",
    );
    refuse(
        &mut world,
        json!({ "ground": { "kind": "stairs", "depth": 1 } }),
        "bad_ground",
    );
    refuse(&mut world, json!({ "ground": 4 }), "bad_ground");
    refuse(&mut world, json!({ "speed": 25 }), "bad_driver");
    refuse(&mut world, json!({ "speed": "fast" }), "bad_driver");
    refuse(&mut world, json!({ "turn_rate": -9 }), "bad_driver");
    refuse(&mut world, json!({ "seconds": 301 }), "simulate_too_long");
    refuse(&mut world, json!({ "seconds": 0 }), "F0002");
    refuse(
        &mut world,
        json!({ "entity": entity.to_bits(), "asset": "assets/walker.gait.ron" }),
        "F0002",
    );
    refuse(&mut world, json!({ "entity": 999_999_999u64 }), "no_such_entity");
    let plain = world.spawn_empty().id();
    refuse(&mut world, json!({ "entity": plain.to_bits() }), "no_gait_rig");
    let mut broken = GaitRig::new(default_gait(), hexapod_legs());
    broken.params.step_length = -1.0;
    let broken = world.spawn(broken).id();
    refuse(&mut world, json!({ "entity": broken.to_bits() }), "gait_settings");
    refuse(
        &mut world,
        json!({ "asset": "assets/missing.gait.ron" }),
        "gait_io",
    );
    write(&root, "garbage.gait.ron", "not ron at all");
    refuse(
        &mut world,
        json!({ "asset": "assets/garbage.gait.ron" }),
        "gait_parse",
    );
    let saved = std::fs::read_to_string(root.join("assets/walker.gait.ron")).expect("saved");
    assert!(saved.contains("format_version: 1"), "{saved}");
    write(
        &root,
        "future.gait.ron",
        &saved.replace("format_version: 1", "format_version: 2"),
    );
    refuse(
        &mut world,
        json!({ "asset": "assets/future.gait.ron" }),
        "gait_version",
    );
    // A leg with an empty joint name.
    let mut file =
        regia_editor::anim::locomotion_panel::gait_file_of(world.get::<GaitRig>(entity).expect("rig"));
    file.legs[5].root.clear();
    write(&root, "noname.gait.ron", &file.to_ron().expect("ron"));
    refuse(
        &mut world,
        json!({ "asset": "assets/noname.gait.ron" }),
        "gait_legs",
    );
    // A path out of the project is the protocol's F0002 with `reason: path_outside_project`.
    let error = call(
        &mut world,
        "editor.anim_locomotion_preview",
        json!({ "asset": "../outside.gait.ron" }),
    )
    .expect_err("outside the project");
    let data = error.data.expect("data");
    assert_eq!(
        (data["code"].as_str(), data["reason"].as_str()),
        (Some("F0002"), Some("path_outside_project"))
    );
}

#[test]
fn locomotion_preview_writes_a_screenshot() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let root = regia_test_utils::unique_temp_dir("anim-locomotion-shot");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let mut world = editor_world();
    world.insert_resource(device);
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    let shot = ok(
        &mut world,
        json!({ "ground": "stairs", "seconds": 2, "screenshot": "assets/stairs.png" }),
    );
    assert_eq!(shot["screenshot"]["path"], "assets/stairs.png");
    assert_eq!(shot["screenshot"]["width"], 640);
    let bytes = std::fs::read(root.join("assets/stairs.png")).expect("png written");
    assert!(bytes.starts_with(b"\x89PNG"), "a PNG");
    // TODO(lead): bless a golden of the stairs preset with gizmos on from this capture on the
    // the build server lavapipe reference (the lane cannot create goldens).
}
