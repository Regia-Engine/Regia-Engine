// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::agent_ext::EditorProject;
use regia_core::math::Affine3A;

/// Audit 11-03 editor L1: bake stdout is kept up to a cap and the rest is still drained.
#[test]
fn bake_stdout_is_capped_and_drained() {
    let mut pipe = std::io::Cursor::new(vec![b'x'; 100]);
    let kept = read_capped(&mut pipe, 10);
    assert_eq!(kept.len(), 10);
    assert_eq!(
        pipe.position(),
        100,
        "the tail must be read so the child cannot block"
    );
}

/// Audit 11-03 editor M7: a reader thread that died before it set the phase left the job
/// `Running` forever (`bake_busy` for the rest of the session, cancel could not clear it).
#[test]
fn a_dead_reader_thread_ends_the_job() {
    let shared = Arc::new(Mutex::new(JobShared {
        phase: JobPhase::Running,
        stage: String::new(),
        done: 0,
        total: 0,
        stdout: String::new(),
        stderr_tail: Vec::new(),
        exit_code: None,
        spawn_error: None,
    }));
    let thread = std::thread::spawn(|| {
        // What a refused inner thread spawn or any reader panic does.
        std::panic::resume_unwind(Box::new("reader thread died"));
    });
    while !thread.is_finished() {
        std::thread::yield_now();
    }
    let job = BakeJob {
        shared,
        child: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        thread: Some(thread),
        out: PathBuf::from("dead.fgiv"),
        dry_run: false,
    };
    assert_eq!(
        job.phase(),
        JobPhase::Failed,
        "a dead reader left the job running"
    );
}

fn unique_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-bake-panel-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn world_with_project(tag: &str) -> (World, PathBuf) {
    let root = unique_dir(tag);
    let mut world = World::new();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    (world, root)
}

fn call(world: &mut World, name: &str, params: &Value) -> Result<Value, RpcError> {
    let methods = methods();
    let (_, handler) = methods
        .iter()
        .find(|(schema, _)| schema.name == name)
        .expect("method exists");
    handler(world, params)
}

fn bake_error(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("bake_error"))
        .and_then(Value::as_str)
        .expect("bake_error in data")
        .to_string()
}

#[test]
fn counts_follow_the_baker_layout_rule() {
    let v = BakeVolume {
        center: [0.0; 3],
        size: [10.0, 4.0, 10.0],
        spacing: 2.0,
    };
    assert_eq!(v.counts(), [6, 3, 6]);
    assert_eq!(v.probe_count(), 108);
    // A box thinner than one spacing still gets two probes per axis.
    let flat = BakeVolume {
        size: [0.5, 0.5, 0.5],
        ..v
    };
    assert_eq!(flat.counts(), [2, 2, 2]);
    assert_eq!(v.min(), [-5.0, -2.0, -5.0]);
    assert_eq!(v.max(), [5.0, 2.0, 5.0]);
}

#[test]
fn volume_validation_names_the_problem() {
    let ok = BakeVolume::default();
    assert!(ok.validate().is_ok());
    for bad in [
        BakeVolume {
            size: [0.0, 1.0, 1.0],
            ..ok
        },
        BakeVolume {
            size: [f32::NAN, 1.0, 1.0],
            ..ok
        },
        BakeVolume {
            center: [f32::INFINITY, 0.0, 0.0],
            ..ok
        },
        BakeVolume { spacing: 0.0, ..ok },
        BakeVolume {
            spacing: 1000.0,
            ..ok
        },
        BakeVolume {
            size: [4000.0, 4000.0, 4000.0],
            spacing: 1.0,
            ..ok
        },
    ] {
        let err = bad.validate().expect_err("refused");
        assert_eq!(err.code, "bake_volume_invalid", "{bad:?}");
    }
}

#[test]
fn settings_validation_and_presets() {
    for quality in BakeQuality::ALL {
        let s = BakeSettings::from_quality(quality);
        assert_eq!(s.quality(), Some(quality));
        assert!(s.validate().is_ok());
        assert_eq!(BakeQuality::from_name(quality.name()), Some(quality));
    }
    let mut s = BakeSettings::from_quality(BakeQuality::Normal);
    s.samples = 0;
    assert_eq!(s.validate().expect_err("zero").code, "bake_settings_invalid");
    assert_eq!(s.quality(), None);
    assert_eq!(BakeQuality::from_name("ultra"), None);
}

#[test]
fn progress_lines_parse() {
    assert_eq!(
        parse_progress_line("  trace: 128/4096 probes"),
        Some(("trace".to_string(), 128, 4096))
    );
    // The baker's relocation stages are named "relocate <round>".
    assert_eq!(
        parse_progress_line("  relocate 1: 0/12 probes"),
        Some(("relocate 1".to_string(), 0, 12))
    );
    for line in [
        "",
        "warning: something odd",
        "trace: x/4096 probes",
        "trace: 5/ probes",
        ": 1/2 probes",
        "1/2 probes",
    ] {
        assert_eq!(parse_progress_line(line), None, "{line:?}");
    }
}

#[test]
fn desc_text_is_deterministic_and_matches_the_v1_shape() {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Desc {
        format_version: u32,
        sun: Option<Sun>,
        meshes: Vec<Mesh>,
        layout: Layout,
        bake: Bake,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Sun {
        toward: [f32; 3],
        illuminance: f32,
        angular_radius: f32,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Mesh {
        id: Option<String>,
        source: String,
        transform: [f32; 16],
        layer: Option<String>,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Layout {
        bricks: Vec<Brick>,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Brick {
        min: [f32; 3],
        max: [f32; 3],
        spacing: f32,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Bake {
        rays_per_probe: u32,
        samples: u32,
        max_bounces: u32,
        seed: u32,
        filter: bool,
    }

    let volume = BakeVolume {
        center: [1.0, 2.0, 3.0],
        size: [8.0, 4.0, 6.0],
        spacing: 1.5,
    };
    let settings = BakeSettings::from_quality(BakeQuality::High);
    let sun = BakeSun::default();
    let mut transform = [0.0f32; 16];
    transform[0] = 1.0;
    transform[5] = 1.0;
    transform[10] = 1.0;
    transform[15] = 1.0;
    transform[12] = 5.5;
    let meshes = vec![BakeMesh {
        id: "wall \"a\"#7".to_string(),
        source: "/proj/assets/wall.gltf".to_string(),
        transform,
    }];
    let text = desc_ron(&volume, &settings, &sun, &meshes);
    assert_eq!(text, desc_ron(&volume, &settings, &sun, &meshes));
    let desc: Desc = ron::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(desc.format_version, 1);
    let parsed_sun = desc.sun.expect("sun");
    assert_eq!(parsed_sun.toward, sun.toward);
    assert_eq!(parsed_sun.illuminance, sun.illuminance);
    assert!(parsed_sun.angular_radius > 0.0);
    assert_eq!(desc.meshes.len(), 1);
    assert_eq!(desc.meshes[0].id.as_deref(), Some("wall \"a\"#7"));
    assert_eq!(desc.meshes[0].source, "/proj/assets/wall.gltf");
    assert_eq!(desc.meshes[0].transform, transform);
    assert_eq!(desc.meshes[0].layer.as_deref(), Some("setpiece"));
    assert_eq!(desc.layout.bricks.len(), 1);
    assert_eq!(desc.layout.bricks[0].min, volume.min());
    assert_eq!(desc.layout.bricks[0].max, volume.max());
    assert_eq!(desc.layout.bricks[0].spacing, 1.5);
    assert_eq!(desc.bake.rays_per_probe, 256);
    assert_eq!(desc.bake.samples, 16);
    assert_eq!(desc.bake.max_bounces, 8);
    assert_eq!(desc.bake.seed, 1);
    assert!(desc.bake.filter);

    // No meshes is a valid description too.
    let bare = desc_ron(&volume, &settings, &sun, &[]);
    let desc: Desc = ron::from_str(&bare).unwrap_or_else(|e| panic!("{e}\n{bare}"));
    assert!(desc.meshes.is_empty());
}

#[test]
fn the_gizmo_is_a_box_with_a_lattice_and_follows_the_flags() {
    let volume = BakeVolume {
        center: [0.0; 3],
        size: [4.0, 2.0, 4.0],
        spacing: 2.0,
    };
    let segments = volume_segments(&volume);
    let edges = segments.iter().filter(|s| s.2 == BAKE_VOLUME).count();
    let lattice = segments.iter().filter(|s| s.2 == BAKE_LATTICE).count();
    assert_eq!(edges, 12);
    // 3 probes along X and Z: three lines each way.
    assert_eq!(lattice, 6);
    for (a, b, color) in &segments {
        if *color == BAKE_VOLUME {
            // Every edge runs along exactly one axis.
            let d = (*b - *a).abs();
            assert_eq!(d.to_array().iter().filter(|c| **c > 0.0).count(), 1);
        }
    }

    let mut world = World::new();
    assert!(bake_gizmo_segments(&world).is_empty(), "no state, no gizmo");
    world.insert_resource(BakePanelState {
        volume,
        ..BakePanelState::default()
    });
    assert_eq!(bake_gizmo_segments(&world).len(), 18);
    world.resource_mut::<BakePanelState>().show_gizmo = false;
    assert!(bake_gizmo_segments(&world).is_empty());
    {
        let mut state = world.resource_mut::<BakePanelState>();
        state.show_gizmo = true;
        state.volume.size = [0.0, 1.0, 1.0];
    }
    assert!(
        bake_gizmo_segments(&world).is_empty(),
        "an invalid volume draws nothing"
    );

    // A huge lattice is skipped, the box stays.
    let big = BakeVolume {
        size: [400.0, 10.0, 400.0],
        spacing: 1.0,
        ..volume
    };
    assert_eq!(volume_segments(&big).len(), 12);
}

#[test]
fn fit_wraps_the_points_with_a_margin() {
    assert_eq!(fit_to_points(&[], 2.0, 1.0), None);
    let fit = fit_to_points(&[Vec3::new(0.0, 0.0, 0.0), Vec3::new(10.0, 4.0, -6.0)], 2.0, 1.0).expect("fit");
    assert_eq!(fit.center, [5.0, 2.0, -3.0]);
    assert_eq!(fit.size, [14.0, 8.0, 10.0]);
    // One point still yields a box of at least the margin on each side.
    let one = fit_to_points(&[Vec3::new(1.0, 1.0, 1.0)], 0.0, 2.0).expect("fit");
    assert_eq!(one.size, [2.0, 2.0, 2.0]);
}

#[test]
fn level_inputs_collect_static_gltf_meshes_and_the_light() {
    let mut world = World::new();
    world.spawn((
        MeshSource::new("assets/wall.gltf", ""),
        GiMobility::Static,
        GlobalTransform(Affine3A::from_translation(Vec3::new(3.0, 0.0, 0.0))),
    ));
    world.spawn((
        MeshSource::new("assets/moving.gltf", ""),
        GiMobility::Dynamic,
        GlobalTransform::default(),
    ));
    world.spawn((
        MeshSource::new(MeshSource::BUILTIN_CUBE, ""),
        GiMobility::Static,
        GlobalTransform::default(),
    ));
    world.spawn(DirectionalLight {
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        illuminance: 12_345.0,
        cast_shadows: true,
        static_shadow: false,
    });
    let (meshes, sun) = level_inputs(&world, BakeSun::default());
    assert_eq!(meshes.len(), 1, "{meshes:?}");
    assert!(
        meshes[0].source.ends_with("assets/wall.gltf"),
        "{}",
        meshes[0].source
    );
    assert_eq!(meshes[0].transform[12], 3.0);
    assert_eq!(sun.toward, [0.0, 1.0, 0.0]);
    assert_eq!(sun.illuminance, 12_345.0);

    let (_, fallback) = level_inputs(&World::new(), BakeSun::default());
    assert_eq!(fallback, BakeSun::default());
}

/// Review finding 5 (w6-post05b): the bake took the first `DirectionalLight` whatever its
/// visibility, so a sun the live frame drops (hidden itself or under a hidden group) still lit the
/// bake. The light here has no visibility component of its own; the scene's real propagation
/// resolves it from the hidden group.
#[test]
fn level_inputs_skip_a_sun_under_a_hidden_group() {
    use regia_scene::components::Visibility;
    let sun = |lux: f32| DirectionalLight {
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        illuminance: lux,
        cast_shadows: true,
        static_shadow: false,
    };
    let mut world = World::new();
    let group = world
        .spawn((regia_scene::Transform::default(), Visibility::Hidden))
        .id();
    let grouped = world
        .spawn((regia_scene::Transform::default(), sun(99_000.0)))
        .insert(ChildOf(group))
        .id();
    regia_scene::propagate::propagate_transforms(&mut world);
    let fallback = BakeSun {
        toward: [0.0, 0.0, 1.0],
        illuminance: 7.0,
    };
    let (_, picked) = level_inputs(&world, fallback);
    assert_eq!(picked, fallback, "the hidden sun does not light the bake");

    // A visible sun spawned after it is the bake's sun, as it is the live frame's.
    let loose = world.spawn(sun(5_000.0)).id();
    let (_, picked) = level_inputs(&world, fallback);
    assert_eq!(picked.illuminance, 5_000.0);

    // Shown again, both suns are drawn: the bake takes the lowest entity, the frame's rule.
    world.entity_mut(group).insert(Visibility::Inherited);
    regia_scene::propagate::propagate_transforms(&mut world);
    let lowest = if grouped.to_bits() < loose.to_bits() {
        99_000.0
    } else {
        5_000.0
    };
    let (_, picked) = level_inputs(&world, fallback);
    assert_eq!(picked.illuminance, lowest);

    // With the loose sun gone, the shown group's sun lights the bake.
    world.despawn(loose);
    let (_, picked) = level_inputs(&world, fallback);
    assert_eq!(picked.illuminance, 99_000.0);
}

#[test]
fn paths_need_a_project_and_use_the_level_name() {
    let world = World::new();
    assert_eq!(
        bake_paths(&world, None).expect_err("no project").code,
        "bake_no_project"
    );

    let (mut world, root) = world_with_project("paths");
    let p = bake_paths(&world, None).expect("paths");
    assert_eq!(p.desc, root.join("bake").join("level.editor.bake.ron"));
    assert_eq!(p.out, root.join("assets").join("maps").join("level.fgiv"));
    world.insert_resource(crate::level_io::OpenLevel {
        id: regia_scene::prelude::LevelId(regia_core::id::Guid::new_random()),
        path: Some(root.join("assets/maps/bore_twelve.level.ron")),
    });
    let p = bake_paths(&world, Some(Path::new("custom/out.fgiv"))).expect("paths");
    assert_eq!(p.desc, root.join("bake").join("bore_twelve.editor.bake.ron"));
    assert_eq!(p.out, root.join("custom/out.fgiv"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn set_get_roundtrip_and_refusals() {
    let (mut world, root) = world_with_project("set");
    let got = call(&mut world, "editor.bake_get", &json!({})).expect("get");
    assert_eq!(got["running"], false);
    assert_eq!(got["quality"], "normal");
    assert_eq!(got["volume"]["valid"], true);

    let set = call(
        &mut world,
        "editor.bake_set",
        &json!({ "center": [1, 2, 3], "size": [10, 4, 10], "spacing": 2, "quality": "high", "seed": 9, "filter": false }),
    )
    .expect("set");
    assert_eq!(set["volume"]["counts"], json!([6, 3, 6]));
    assert_eq!(set["volume"]["probes"], 108);
    assert_eq!(set["quality"], "high");
    assert_eq!(set["settings"]["seed"], 9);
    assert_eq!(set["settings"]["filter"], false);
    assert_eq!(set["volume"]["min"], json!([-4.0, 0.0, -2.0]));

    // An explicit rays count leaves the preset.
    let set = call(&mut world, "editor.bake_set", &json!({ "rays_per_probe": 100 })).expect("set");
    assert_eq!(set["quality"], Value::Null);

    // A refused change changes nothing.
    let err = call(
        &mut world,
        "editor.bake_set",
        &json!({
            "size": [0, 1, 1],
            "seed": 5,
            "sun_toward": [1.0, 0.0, 0.0],
            "sun_illuminance": 1.0,
            "show_gizmo": false,
            "assign": true
        }),
    )
    .expect_err("refused");
    assert_eq!(bake_error(&err), "bake_volume_invalid");
    let err = call(&mut world, "editor.bake_set", &json!({ "samples": 0 })).expect_err("refused");
    assert_eq!(bake_error(&err), "bake_settings_invalid");
    // A bad `out` is checked after the sun fields are parsed, and still writes nothing.
    let err = call(
        &mut world,
        "editor.bake_set",
        &json!({ "sun_toward": [0.0, 1.0, 0.0], "out": 4 }),
    )
    .expect_err("out");
    assert!(err.message.contains("`out`"));
    let after = call(&mut world, "editor.bake_get", &json!({})).expect("get");
    assert_eq!(after["settings"]["seed"], 9);
    assert_eq!(after["volume"]["size"], json!([10.0, 4.0, 10.0]));
    // `BakeSun::toward` is `[f32; 3]`: the JSON carries the f32 values widened to f64.
    assert_eq!(after["sun"]["toward"], json!([0.3_f32, 0.8_f32, 0.5_f32]));
    assert_eq!(after["sun"]["illuminance"], json!(50_000.0));
    assert_eq!(after["show_gizmo"], json!(true));
    assert_eq!(after["assign"], json!(false));
    let set = call(
        &mut world,
        "editor.bake_set",
        &json!({ "sun_toward": [1.0, 0.0, 0.0], "sun_illuminance": 10.0 }),
    )
    .expect("sun");
    assert_eq!(set["sun"]["toward"], json!([1.0, 0.0, 0.0]));
    assert_eq!(set["sun"]["illuminance"], json!(10.0));

    // Shape errors are plain invalid params.
    assert!(call(&mut world, "editor.bake_set", &json!({ "center": [1, 2] })).is_err());
    assert!(call(&mut world, "editor.bake_set", &json!({ "quality": "ultra" })).is_err());
    assert!(call(&mut world, "editor.bake_set", &json!({ "seed": -1 })).is_err());

    // Nothing selected: fit_selection says so.
    let err = call(&mut world, "editor.bake_set", &json!({ "fit_selection": true })).expect_err("empty");
    assert_eq!(bake_error(&err), "bake_selection_empty");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn start_is_refused_without_a_project_or_with_a_bad_volume() {
    let mut world = World::new();
    let err = call(&mut world, "editor.bake_start", &json!({ "dry_run": true })).expect_err("no project");
    assert_eq!(bake_error(&err), "bake_no_project");

    let (mut world, root) = world_with_project("start-bad");
    world.insert_resource(BakePanelState {
        volume: BakeVolume {
            size: [0.0, 1.0, 1.0],
            ..BakeVolume::default()
        },
        ..BakePanelState::default()
    });
    let err = call(&mut world, "editor.bake_start", &json!({})).expect_err("bad volume");
    assert_eq!(bake_error(&err), "bake_volume_invalid");
    assert!(
        !root.join("bake").exists(),
        "nothing is written for a refused request"
    );

    let err = call(&mut world, "editor.bake_cancel", &json!({})).expect_err("not running");
    assert_eq!(bake_error(&err), "bake_not_running");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_missing_program_is_a_spawn_error() {
    let (mut world, root) = world_with_project("spawn");
    world.insert_resource(BakePanelState {
        program: root.join("no-such-regia"),
        ..BakePanelState::default()
    });
    let err = call(&mut world, "editor.bake_start", &json!({})).expect_err("spawn");
    assert_eq!(bake_error(&err), "bake_spawn_failed");
    let state = world.resource::<BakePanelState>();
    assert!(state.job.is_none());
    let _ = std::fs::remove_dir_all(root);
}

/// A shell that stands in for `regia`: `$1` is `bake-gi`, `$2` the description, `$4` the output.
#[cfg(unix)]
fn fake_regia(script: &str) -> BakePanelState {
    BakePanelState {
        program: PathBuf::from("sh"),
        prefix_args: vec!["-c".to_string(), script.to_string(), "fake-regia".to_string()],
        ..BakePanelState::default()
    }
}

#[cfg(unix)]
#[test]
fn a_bake_runs_reports_progress_and_names_the_volume() {
    let (mut world, root) = world_with_project("run");
    world.insert_resource(LevelSettings::default());
    // Writes the volume file, prints progress on stderr and the summary on stdout.
    let script = r#"
        echo "  relocate: 2/8 probes" >&2
        echo "  trace: 8/8 probes" >&2
        echo "noise on stderr" >&2
        printf 'volume' > "$4"
        echo '{"ok": true, "seconds": 1.5}'
    "#;
    world.insert_resource(fake_regia(script));
    let started = call(
        &mut world,
        "editor.bake_start",
        &json!({ "wait": true, "assign": true }),
    )
    .expect("start");
    assert_eq!(started["running"], false);
    let last = &started["last"];
    assert_eq!(last["phase"], "succeeded", "{last}");
    assert_eq!(last["dry_run"], false);
    assert_eq!(last["assigned"], true);
    assert_eq!(last["summary"]["seconds"], 1.5);
    let volume = root.join("assets").join("maps").join("level.fgiv");
    assert_eq!(last["volume"], volume.display().to_string());
    assert_eq!(std::fs::read(&volume).expect("volume written"), b"volume");
    // The description was written next to the project, outside assets/.
    let desc = std::fs::read_to_string(root.join("bake").join("level.editor.bake.ron")).expect("desc");
    assert!(desc.contains("format_version: 1"));
    assert!(!root.join("bake").join("level.editor.bake.ron.tmp").exists());
    let level = world.resource::<LevelSettings>();
    assert_eq!(
        level.look.baked_gi.as_ref().map(|b| b.volume.as_str()),
        Some("maps/level.fgiv")
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn progress_is_read_from_stderr() {
    let (mut world, root) = world_with_project("progress");
    let script = r#"
        echo "  trace: 3/12 probes" >&2
        echo '{"ok": true}'
    "#;
    world.insert_resource(fake_regia(script));
    let mut state = world.remove_resource::<BakePanelState>().expect("state");
    state.start(&mut world, true).expect("start");
    let job = state.job.as_mut().expect("job");
    job.wait();
    assert_eq!(job.progress(), ("trace".to_string(), 3, 12));
    state.poll(&mut world);
    let last = state.last.as_ref().expect("outcome");
    assert_eq!(last.phase, JobPhase::Succeeded);
    assert!(last.dry_run);
    assert_eq!(last.volume, None, "a dry run names no volume");
    assert!(
        !root.join("assets").exists(),
        "a dry run creates no output directory"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn a_failing_bake_keeps_the_commands_error_code() {
    let (mut world, root) = world_with_project("fail");
    let script = r#"
        echo '{"ok": false, "code": "bake_layout", "path": "layout", "message": "the layout holds no probes"}'
        exit 1
    "#;
    world.insert_resource(fake_regia(script));
    let got = call(&mut world, "editor.bake_start", &json!({ "wait": true })).expect("start");
    let last = &got["last"];
    assert_eq!(last["phase"], "failed");
    assert_eq!(last["code"], "bake_layout");
    assert_eq!(last["message"], "the layout holds no probes");
    assert_eq!(last["volume"], Value::Null);

    // No JSON at all: bake_failed with the stderr lines.
    world.insert_resource(fake_regia("echo 'no GPU adapter' >&2; exit 3"));
    let got = call(&mut world, "editor.bake_start", &json!({ "wait": true })).expect("start");
    assert_eq!(got["last"]["code"], "bake_failed");
    assert_eq!(got["last"]["message"], "no GPU adapter");

    // Exit 0 but ok:false is still a failure.
    world.insert_resource(fake_regia(
        r#"echo '{"ok": false, "code": "bake_mesh_load", "message": "m"}'"#,
    ));
    let got = call(&mut world, "editor.bake_start", &json!({ "wait": true })).expect("start");
    assert_eq!(got["last"]["phase"], "failed");
    assert_eq!(got["last"]["code"], "bake_mesh_load");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn cancel_kills_the_child_and_a_second_start_is_busy_meanwhile() {
    let (mut world, root) = world_with_project("cancel");
    world.insert_resource(fake_regia("exec sleep 600"));
    let started = call(&mut world, "editor.bake_start", &json!({})).expect("start");
    assert_eq!(started["running"], true);
    assert_eq!(started["job"]["phase"], "running");

    let err = call(&mut world, "editor.bake_start", &json!({})).expect_err("busy");
    assert_eq!(bake_error(&err), "bake_busy");
    let err = call(&mut world, "editor.bake_set", &json!({ "spacing": 3 })).expect_err("busy");
    assert_eq!(bake_error(&err), "bake_busy");

    let cancelled = call(&mut world, "editor.bake_cancel", &json!({})).expect("cancel");
    assert_eq!(cancelled["running"], false);
    assert_eq!(cancelled["last"]["phase"], "cancelled");
    assert_eq!(cancelled["last"]["volume"], Value::Null);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn dropping_the_state_kills_a_running_child() {
    let (mut world, root) = world_with_project("drop");
    world.insert_resource(fake_regia("exec sleep 600"));
    let mut state = world.remove_resource::<BakePanelState>().expect("state");
    state.start(&mut world, false).expect("start");
    assert!(state.running());
    // Drop joins the reader thread, which only returns once the child is gone.
    drop(state);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_methods_follow_the_naming_rules() {
    let methods = methods();
    let names: Vec<&str> = methods.iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        [
            "editor.bake_get",
            "editor.bake_set",
            "editor.bake_start",
            "editor.bake_cancel"
        ]
    );
    for (m, _) in &methods {
        assert!(m.name.starts_with("editor."));
        assert!(
            m.tool.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{}",
            m.tool
        );
    }
    // Every error code the module can produce is lowercase ascii, digits and `_`.
    for code in [
        "bake_volume_invalid",
        "bake_settings_invalid",
        "bake_busy",
        "bake_not_running",
        "bake_no_project",
        "bake_write_failed",
        "bake_spawn_failed",
        "bake_selection_empty",
        "bake_failed",
    ] {
        assert!(
            code.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{code}"
        );
    }
}

/// Audit G finding 5 (Windows-only scenario): a bake `out` whose casing differs from the
/// project root must still be named into the level — the bake wrote it through filesystem
/// APIs that fold case. The folded compare itself is proven on every host in `path_norm`.
#[cfg(windows)]
#[test]
fn assign_to_level_accepts_case_variant_volume_on_windows() {
    let mut world = World::new();
    world.insert_resource(EditorProject {
        root: Some(std::path::PathBuf::from("C:/Dev/Game")),
    });
    world.insert_resource(regia_scene::level::LevelSettings::default());
    assert!(assign_to_level(
        &mut world,
        std::path::Path::new("c:/dev/game/assets/maps/arena.fgiv")
    ));
    let level = world.resource::<regia_scene::level::LevelSettings>();
    assert_eq!(
        level.look.baked_gi.as_ref().map(|entry| entry.volume.as_str()),
        Some("maps/arena.fgiv")
    );
    assert!(!assign_to_level(
        &mut world,
        std::path::Path::new("d:/other/x.fgiv")
    ));
}

/// Audit G finding 10 (Windows-only scenario): a read-only destination (Perforce-style sync)
/// refuses the replacing rename; the rewrite succeeds after the attribute is cleared once.
#[cfg(windows)]
#[test]
fn write_atomically_replaces_read_only_destination() {
    let dir = regia_test_utils::unique_temp_dir("bake_panel_readonly_dest");
    let path = dir.join("arena.editor.bake.ron");
    write_atomically(&path, b"first").expect("first write");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).expect("mark read-only");
    write_atomically(&path, b"second").expect("rewrite over read-only");
    assert_eq!(std::fs::read(&path).expect("read"), b"second");
}
