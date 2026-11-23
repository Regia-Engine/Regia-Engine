// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep 2: deterministic tests for public `regia_scene` functions no test called
//! before (spline names, segment math and distance lookup, layer names and initial states, level
//! directory helpers, placement checks, HLOD distances, transforms). Pure CPU, fixed inputs.

use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_scene::hlod::HlodNode;
use regia_scene::layers::{LayerEntry, LayerPeers, LayerState, LayerTable, LevelLayers, valid_layer_name};
use regia_scene::level::{Level, LevelId, LevelLibrary};
use regia_scene::level_dir::{LevelDirtyTracker, containing_level_dir, header_text, parse_header};
use regia_scene::placed::{MAX_PLACED_DEPTH, check_placement};
use regia_scene::prelude::*;
use regia_scene::spline::{
    Spline, SplineAttrDecl, SplineAttrType, SplinePoint, SplineValue, TangentMode, catmull_rom_segment_with,
    centripetal_knot_step,
};

fn close(a: Vec3, b: Vec3, eps: f32) -> bool {
    (a - b).length() <= eps
}

/// A `Linear` (straight-segment) spline: constant speed per segment, so lengths are exact.
fn linear(points: &[Vec3]) -> Spline {
    Spline {
        points: points
            .iter()
            .map(|p| SplinePoint::at(*p).with_mode(TangentMode::Linear))
            .collect(),
        ..Spline::default()
    }
}

/// Every tangent mode parses back from its agent name, in any letter case; unknown names do not.
#[test]
fn tangent_mode_names_round_trip_case_insensitively() {
    for mode in [
        TangentMode::Auto,
        TangentMode::Linear,
        TangentMode::Free,
        TangentMode::Aligned,
        TangentMode::Step,
    ] {
        assert_eq!(TangentMode::from_name(mode.name()), Some(mode));
        assert_eq!(
            TangentMode::from_name(&mode.name().to_ascii_uppercase()),
            Some(mode)
        );
        assert_eq!(TangentMode::from_name(&format!("{mode:?}")), Some(mode));
    }
    assert_eq!(TangentMode::from_name(""), None);
    assert_eq!(TangentMode::from_name("bezier"), None);
    assert_eq!(TangentMode::from_name(" auto"), None);
}

/// Attribute types parse back from their names; `default_of` yields a value of that type that is
/// zero / empty.
#[test]
fn attr_type_names_and_defaults_agree() {
    for ty in [SplineAttrType::F32, SplineAttrType::Vec3, SplineAttrType::Name] {
        assert_eq!(SplineAttrType::from_name(ty.name()), Some(ty));
        assert_eq!(SplineAttrType::from_name(&format!("{ty:?}")), Some(ty));
        assert_eq!(SplineValue::default_of(ty).ty(), ty);
    }
    assert_eq!(
        SplineValue::default_of(SplineAttrType::F32),
        SplineValue::F32(0.0)
    );
    assert_eq!(
        SplineValue::default_of(SplineAttrType::Vec3),
        SplineValue::Vec3(Vec3::ZERO)
    );
    assert_eq!(
        SplineValue::default_of(SplineAttrType::Name),
        SplineValue::Name(String::new())
    );
    assert_eq!(SplineAttrType::from_name("f64"), None);
}

/// `with_mode` changes only the mode; a spline of default values for its declared attributes
/// validates, and `attr_index` finds attributes by exact name (the first one on duplicates).
#[test]
fn with_mode_and_attr_index() {
    let free = SplinePoint::free(Vec3::X, Vec3::Y, Vec3::Z);
    let stepped = free.clone().with_mode(TangentMode::Step);
    assert_eq!(stepped.mode, TangentMode::Step);
    assert_eq!(
        (stepped.position, stepped.tangent_in, stepped.tangent_out),
        (Vec3::X, Vec3::Y, Vec3::Z)
    );
    assert_eq!(stepped.scale, free.scale);

    let attrs = vec![
        SplineAttrDecl {
            name: "width".into(),
            ty: SplineAttrType::F32,
        },
        SplineAttrDecl {
            name: "surface".into(),
            ty: SplineAttrType::Name,
        },
        SplineAttrDecl {
            name: "width".into(),
            ty: SplineAttrType::Vec3,
        },
    ];
    let mut spline = Spline::through([Vec3::ZERO, Vec3::X]);
    for p in &mut spline.points {
        p.values = attrs.iter().map(|a| SplineValue::default_of(a.ty)).collect();
    }
    spline.attrs = attrs;
    assert_eq!(spline.validate(), Ok(()));
    assert_eq!(spline.attr_index("width"), Some(0));
    assert_eq!(spline.attr_index("surface"), Some(1));
    assert_eq!(spline.attr_index("Width"), None);
    assert_eq!(spline.attr_index(""), None);
}

/// The Catmull-Rom segment returns its end points exactly (also for `u` outside `0..=1`) and
/// reproduces evenly spaced collinear points as a straight line under any constant knot step.
#[test]
fn catmull_rom_segment_ends_and_linear_precision() {
    let (p0, p1, p2, p3) = (
        Vec3::new(-1.0, 2.0, 0.5),
        Vec3::new(0.0, 2.0, 0.5),
        Vec3::new(1.0, 2.0, 0.5),
        Vec3::new(2.0, 2.0, 0.5),
    );
    for knot in [
        centripetal_knot_step as fn(Vec3, Vec3) -> f32,
        |_, _| 1.0,
        |a: Vec3, b: Vec3| a.distance(b),
    ] {
        assert_eq!(catmull_rom_segment_with(p0, p1, p2, p3, 0.0, knot), p1);
        assert_eq!(catmull_rom_segment_with(p0, p1, p2, p3, -3.0, knot), p1);
        assert_eq!(catmull_rom_segment_with(p0, p1, p2, p3, 1.0, knot), p2);
        assert_eq!(catmull_rom_segment_with(p0, p1, p2, p3, 7.5, knot), p2);
        for i in 1..10 {
            let u = i as f32 / 10.0;
            let got = catmull_rom_segment_with(p0, p1, p2, p3, u, knot);
            assert!(close(got, p1.lerp(p2, u), 1.0e-5), "u {u}: {got}");
        }
    }
}

/// With the centripetal knot step, coincident neighbors (zero-length knot intervals) still give
/// finite points: the step is clamped away from zero.
#[test]
fn catmull_rom_segment_survives_coincident_points() {
    let p = Vec3::new(3.0, -1.0, 2.0);
    let q = Vec3::new(4.0, 0.0, 2.0);
    for u in [0.1, 0.5, 0.9] {
        let a = catmull_rom_segment_with(p, p, q, q, u, centripetal_knot_step);
        assert!(a.is_finite(), "u {u}: {a}");
        let b = catmull_rom_segment_with(p, p, p, p, u, centripetal_knot_step);
        assert!(close(b, p, 1.0e-4), "u {u}: {b}");
    }
    assert!(centripetal_knot_step(p, p) > 0.0);
}

/// Straight segments: lengths are the chord lengths, the starts are their running sums, and the
/// Hermite form is `[p0, p1 - p0, p1, p1 - p0]`; out-of-range segments are `None` / zero.
#[test]
fn linear_curve_lengths_starts_and_hermite() {
    let pts = [Vec3::ZERO, Vec3::new(3.0, 0.0, 0.0), Vec3::new(3.0, 4.0, 0.0)];
    let curve = linear(&pts).curve();
    assert!(!curve.is_closed());
    assert_eq!(curve.segment_count(), 2);
    assert!(
        (curve.length_f64() - 7.0).abs() < 1.0e-9,
        "{}",
        curve.length_f64()
    );
    assert_eq!(f64::from(curve.length()), curve.length_f64() as f32 as f64);
    let starts = curve.segment_starts();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0], 0.0);
    assert!((starts[1] - 3.0).abs() < 1.0e-5);
    assert!((curve.segment_length(1) - 4.0).abs() < 1.0e-5);
    assert_eq!(curve.segment_length(2), 0.0);
    let h = curve.segment_hermite(1).expect("segment 1");
    assert!(close(h[0], pts[1], 1.0e-6) && close(h[2], pts[2], 1.0e-6));
    assert!(close(h[1], pts[2] - pts[1], 1.0e-6) && close(h[3], pts[2] - pts[1], 1.0e-6));
    assert_eq!(curve.segment_hermite(2), None);
}

/// A closed spline has one segment per point, the last one returning to the first point.
#[test]
fn closed_curve_wraps_its_last_segment() {
    let pts = [Vec3::ZERO, Vec3::new(3.0, 0.0, 0.0), Vec3::new(3.0, 4.0, 0.0)];
    let curve = linear(&pts).closed().curve();
    assert!(curve.is_closed());
    assert_eq!(curve.segment_count(), 3);
    let last = curve.segment_hermite(2).expect("closing segment");
    assert!(close(last[0], pts[2], 1.0e-6) && close(last[2], pts[0], 1.0e-6));
    assert!(
        (curve.length_f64() - 12.0).abs() < 1.0e-9,
        "{}",
        curve.length_f64()
    );
}

/// `locate_distance` clamps, maps NaN to the start, gives a boundary to the next segment and
/// inverts arc length on a straight segment; an empty curve answers `(0, 0)`.
#[test]
fn locate_distance_boundaries_clamps_and_nan() {
    let curve = linear(&[Vec3::ZERO, Vec3::new(3.0, 0.0, 0.0), Vec3::new(3.0, 4.0, 0.0)]).curve();
    let at = |d: f64| curve.locate_distance(d);
    assert_eq!(at(-5.0), (0, 0.0));
    assert_eq!(at(f64::NEG_INFINITY), (0, 0.0));
    assert_eq!(at(f64::NAN), (0, 0.0));
    let (i, u) = at(1.5);
    assert_eq!(i, 0);
    assert!((u - 0.5).abs() < 1.0e-9, "{u}");
    let (i, u) = at(3.0);
    assert_eq!(i, 1, "a boundary belongs to the next segment");
    assert!(u.abs() < 1.0e-9, "{u}");
    let (i, u) = at(5.0);
    assert_eq!(i, 1);
    assert!((u - 0.5).abs() < 1.0e-9, "{u}");
    assert_eq!(at(100.0), (1, 1.0));
    assert_eq!(at(f64::INFINITY), (1, 1.0));

    let empty = Spline::default().curve();
    assert_eq!(empty.locate_distance(4.0), (0, 0.0));
    assert_eq!(empty.segment_hermite(0), None);
    assert!(empty.segment_starts().is_empty());
    assert_eq!(empty.length_f64(), 0.0);
}

/// Layer names are `[a-z0-9_]{1,48}`: the bounds and every other character class are refused.
#[test]
fn layer_names_follow_the_documented_pattern() {
    assert!(valid_layer_name("a"));
    assert!(valid_layer_name("quest_2_boss"));
    assert!(valid_layer_name("_"));
    assert!(valid_layer_name(&"z".repeat(48)));
    assert!(!valid_layer_name(""));
    assert!(!valid_layer_name(&"z".repeat(49)));
    for bad in ["Night", "night-time", "night time", "nächt", "a.b", "a/b", "1\n"] {
        assert!(!valid_layer_name(bad), "{bad:?}");
    }
}

/// `at_initial_states` starts each layer at its binding entry's `initial` (`Live` without one),
/// forgets requested states, and keeps parents and peers.
#[test]
fn at_initial_states_uses_binding_entries() {
    let mut table = LayerTable::default();
    table.insert_layer("night", None, LayerState::Parked, LayerPeers::Everyone);
    table.insert_layer(
        "night_boss",
        Some("night"),
        LayerState::Absent,
        LayerPeers::ServerOnly,
    );
    table.insert_layer("debug", None, LayerState::Absent, LayerPeers::ClientsOnly);

    let fresh = table.at_initial_states(None);
    for name in ["night", "night_boss", "debug"] {
        assert_eq!(fresh.requested(name), Some(LayerState::Live), "{name}");
    }
    assert_eq!(fresh.peers("night_boss"), Some(LayerPeers::ServerOnly));
    assert_eq!(fresh.layers().map(|l| l.member_count).sum::<usize>(), 0);

    let binding = LevelLayers {
        entries: vec![LayerEntry {
            layer: "night".into(),
            initial: LayerState::Absent,
            ..LayerEntry::default()
        }],
        ..LevelLayers::default()
    };
    let bound = table.at_initial_states(Some(&binding));
    assert_eq!(bound.requested("night"), Some(LayerState::Absent));
    assert_eq!(bound.requested("night_boss"), Some(LayerState::Live));
    // The child is clamped by its absent parent.
    assert_eq!(bound.effective("night_boss"), Some(LayerState::Absent));
    assert_eq!(bound.effective("debug"), Some(LayerState::Live));
    assert!(!bound.contains("unknown"));
}

/// A parent cycle cannot hang `effective`: the walk is bounded and reports `Absent`.
#[test]
fn layer_parent_cycle_is_bounded() {
    let mut table = LayerTable::default();
    table.insert_layer("a", Some("b"), LayerState::Live, LayerPeers::Everyone);
    table.insert_layer("b", Some("a"), LayerState::Live, LayerPeers::Everyone);
    assert_eq!(table.effective("a"), Some(LayerState::Absent));
    assert_eq!(table.effective("missing"), None);
}

/// `def_ids` accepts `asset:<guid>` and bare guids and skips anything unparsable, in order.
#[test]
fn level_layers_def_ids_skip_unparsable_entries() {
    let a = Guid::from_u128(0x1234);
    let b = Guid::from_u128(0xfeed_beef);
    let layers = LevelLayers {
        defs: vec![
            format!("asset:{a}"),
            "asset:not-a-guid".into(),
            b.to_string(),
            String::new(),
            "file:whatever".into(),
        ],
        ..LevelLayers::default()
    };
    assert_eq!(layers.def_ids(), vec![a, b]);
}

/// `containing_level_dir` finds the nearest `.level.d` ancestor (or the path itself), and nothing
/// for a path outside a level directory or a single-file level.
#[test]
fn containing_level_dir_finds_the_nearest_level_directory() {
    let p = Path::new("game/levels/forest.level.d/entities/ab/abcd.entity.ron");
    assert_eq!(
        containing_level_dir(p),
        Some(PathBuf::from("game/levels/forest.level.d"))
    );
    assert_eq!(
        containing_level_dir(Path::new("w/outer.level.d/inner.level.d/level.ron")),
        Some(PathBuf::from("w/outer.level.d/inner.level.d"))
    );
    assert_eq!(
        containing_level_dir(Path::new("forest.level.d")),
        Some(PathBuf::from("forest.level.d"))
    );
    assert_eq!(
        containing_level_dir(Path::new("game/levels/forest.level.ron")),
        None
    );
    assert_eq!(containing_level_dir(Path::new("game/level.d.notes/x.ron")), None);
    assert_eq!(containing_level_dir(Path::new("")), None);
}

/// The dirty tracker normalizes either spelling of a level path to one entry, and entity marks
/// accumulate without duplicates.
#[test]
fn dirty_tracker_merges_both_level_spellings() {
    let mut tracker = LevelDirtyTracker::default();
    let g = Guid::from_u128(7);
    tracker.mark_entity("lv/forest.level.ron", g);
    tracker.mark_entity("lv/forest.level.d/", g);
    tracker.mark_entity(Path::new("lv/forest.level.d"), Guid::from_u128(8));
    let set = tracker.get(Path::new("lv/forest.level.d")).expect("tracked");
    assert!(!set.header);
    assert_eq!(set.entities.len(), 2);
    tracker.mark_header("lv/forest");
    let set = tracker.get(Path::new("lv/forest.level.ron")).expect("tracked");
    assert!(set.header);
    assert!(!set.is_empty());
    let cleared = tracker.clear(Path::new("lv/forest")).expect("cleared");
    assert_eq!(cleared.entities.len(), 2);
    assert!(tracker.get(Path::new("lv/forest.level.d")).is_none());
}

/// A header written by `header_text` parses back with the same resources; newer versions are
/// `unsupported_version` even when the newer shape no longer parses; garbage is
/// `level_parse_error`.
#[test]
fn parse_header_round_trip_and_reasons() {
    let scene = SceneFile::from_ron_str(&format!(
        "SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [], resources: {{ \
         \"regia_scene::level::LevelSettings\": (gravity: (0.0, -2.0, 0.0)) }})"
    ))
    .expect("scene parses");
    let text = header_text(&scene).expect("header text");
    let path = Path::new("w/forest.level.d/level.ron");
    let header = parse_header(path, &text).expect("header parses");
    assert_eq!(header.scene_format_version, SCENE_FORMAT_VERSION);
    assert_eq!(
        header.resources.keys().collect::<Vec<_>>(),
        scene.resources.keys().collect::<Vec<_>>()
    );

    let newer = format!(
        "LevelDirFile(format_version: 99, scene_format_version: {SCENE_FORMAT_VERSION}, resources: {{}})"
    );
    assert_eq!(
        parse_header(path, &newer).map_err(|e| e.reason()).err(),
        Some("unsupported_version")
    );
    let newer_scene = "LevelDirFile(format_version: 1, scene_format_version: 9999, resources: {})";
    assert_eq!(
        parse_header(path, newer_scene).map_err(|e| e.reason()).err(),
        Some("unsupported_version")
    );
    let newer_shape = "LevelDirFile(format_version: 99, scene_format_version: 1, resources: 7)";
    assert_eq!(
        parse_header(path, newer_shape).map_err(|e| e.reason()).err(),
        Some("unsupported_version")
    );
    for garbage in ["", "LevelDirFile(", "SceneFile(format_version: 1)", "\u{0}\u{1}"] {
        assert_eq!(
            parse_header(path, garbage).map_err(|e| e.reason()).err(),
            Some("level_parse_error"),
            "{garbage:?}"
        );
    }
}

/// A level that places `nested` (or nothing) with `count` plain entities.
fn chain_level(id: u128, nested: &[u128], count: usize) -> Level {
    let mut entities = String::new();
    for i in 0..count {
        entities.push_str(&format!(
            "(guid: \"{}\", components: {{}}),",
            Guid::from_u128(id * 1000 + i as u128 + 1)
        ));
    }
    for (k, n) in nested.iter().enumerate() {
        entities.push_str(&format!(
            "(guid: \"{}\", components: {{ \"regia_scene::placed::PlacedLevel\": (level: \"asset:{}\", mode: Inline) }}),",
            Guid::from_u128(id * 1000 + 900 + k as u128),
            Guid::from_u128(*n)
        ));
    }
    let text =
        format!("SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [{entities}], resources: {{}})");
    Level::from_ron_str(LevelId(Guid::from_u128(id)), &text).expect("level parses")
}

fn id(v: u128) -> LevelId {
    LevelId(Guid::from_u128(v))
}

/// `check_placement` counts every entity of every nesting level (placement records included),
/// treats missing levels as empty, and refuses cycles and over-deep nesting with their reasons.
#[test]
fn check_placement_totals_cycles_and_depth() {
    let mut library = LevelLibrary::default();
    library.insert(chain_level(1, &[2, 2], 3)); // 3 + 2 placements
    library.insert(chain_level(2, &[3], 4)); // 4 + 1 placement, twice
    library.insert(chain_level(3, &[], 2)); // 2, twice
    library.insert(chain_level(4, &[404], 1)); // places a missing level
    assert_eq!(check_placement(&library, id(3), &[]), Ok(2));
    assert_eq!(check_placement(&library, id(2), &[]), Ok(5 + 2));
    assert_eq!(check_placement(&library, id(1), &[]), Ok(5 + 2 * (5 + 2)));
    assert_eq!(check_placement(&library, id(4), &[]), Ok(2));
    assert_eq!(check_placement(&library, id(404), &[]), Ok(0));

    // Placing level 1 inside level 3 makes 1 -> 2 -> 3 -> 1 a cycle.
    let err = check_placement(&library, id(1), &[id(3)]).expect_err("cycle");
    assert_eq!(err.reason(), "placed_cycle");
    let mut cyclic = library.clone();
    cyclic.insert(chain_level(5, &[6], 0));
    cyclic.insert(chain_level(6, &[5], 0));
    assert_eq!(
        check_placement(&cyclic, id(5), &[]).map_err(|e| e.reason()),
        Err("placed_cycle")
    );
    // Placing a level in itself.
    assert_eq!(
        check_placement(&library, id(3), &[id(3)]).map_err(|e| e.reason()),
        Err("placed_cycle")
    );

    // A linear chain one deeper than the limit.
    let mut deep = LevelLibrary::default();
    let n = MAX_PLACED_DEPTH as u128 + 2;
    for i in 10..10 + n {
        deep.insert(chain_level(i, &[i + 1], 1));
    }
    deep.insert(chain_level(10 + n, &[], 1));
    assert_eq!(
        check_placement(&deep, id(10), &[]).map_err(|e| e.reason()),
        Err("placed_limit")
    );
    // Within the limit from deeper in the chain.
    let start = 10 + n - MAX_PLACED_DEPTH as u128;
    assert!(check_placement(&deep, id(start), &[]).is_ok());
}

/// The HLOD node distance is zero inside and on the box, axis distance on a face side and the
/// Euclidean corner distance outside a corner.
#[test]
fn hlod_node_distance_to_box() {
    let node = HlodNode {
        level: 0,
        cell: None,
        bounds_min: [0.0, 0.0, 0.0],
        bounds_max: [10.0, 4.0, 10.0],
        radius: 0.0,
        proxy_bytes: 0,
        children: Vec::new(),
        switch_distance: 0.0,
    };
    assert_eq!(node.distance_to([5.0, 2.0, 5.0]), 0.0);
    assert_eq!(node.distance_to([10.0, 4.0, 0.0]), 0.0);
    assert_eq!(node.distance_to([-3.0, 2.0, 5.0]), 3.0);
    assert_eq!(node.distance_to([5.0, 9.0, 5.0]), 5.0);
    assert_eq!(node.distance_to([13.0, 8.0, 10.0]), 5.0);
    assert!((node.distance_to([-1.0, -1.0, -1.0]) - 3f64.sqrt()).abs() < 1.0e-12);
    // Far coordinates stay exact in f64.
    assert_eq!(node.distance_to([1.0e12 + 10.0, 2.0, 5.0]), 1.0e12);
}

/// `from_rotation` keeps the origin and unit scale; it rotates points and composes like the quat.
#[test]
fn transform_from_rotation_rotates_about_the_origin() {
    let q = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    let t = Transform::from_rotation(q);
    assert_eq!(t.translation, Vec3::ZERO);
    assert_eq!(t.scale, Vec3::ONE);
    assert!(close(t.transform_point(Vec3::X), Vec3::Y, 1.0e-6));
    assert!(close(t.transform_point(Vec3::ZERO), Vec3::ZERO, 0.0));
    let twice = Transform::from_rotation(q * q);
    assert!(close(twice.transform_point(Vec3::X), -Vec3::X, 1.0e-6));
    assert_eq!(Transform::from_rotation(Quat::IDENTITY), Transform::IDENTITY);
}

// ---------------------------------------------------------------------------------------------
// Level forms and load generations (second batch)
// ---------------------------------------------------------------------------------------------

mod level_forms {
    use regia_app::prelude::*;
    use regia_core::id::Guid;
    use regia_scene::level::{LevelId, LevelLibrary, LevelManager, LoadMode, open_level};
    use regia_scene::level_dir::{
        LEVEL_DIR_FORMAT_VERSION, level_dir_versions, read_level_as_stored, read_level_source,
        write_level_dir,
    };
    use regia_scene::prelude::*;
    use regia_test_utils::unique_temp_dir;

    /// Records in descending guid order, so "stored order" and "guid order" differ.
    fn scene() -> SceneFile {
        let text = format!(
            "SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [{}], resources: {{}})",
            [0x30u128, 0x10, 0x20]
                .iter()
                .map(|g| format!("(guid: \"{}\", components: {{}}),", Guid::from_u128(*g)))
                .collect::<String>()
        );
        SceneFile::from_ron_str(&text).expect("scene")
    }

    fn guids(scene: &SceneFile) -> Vec<u128> {
        scene.entities.iter().map(|e| e.guid.as_u128()).collect()
    }

    /// The file form keeps its record order, the directory form is in guid order, and the cook
    /// input (`read_level_source`) is guid order for both. Both forms at once are refused, and the
    /// directory reports its versions.
    #[test]
    fn stored_order_per_form_and_versions() {
        let root = unique_temp_dir("tqs2_level_forms");
        let file = root.join("a.level.ron");
        std::fs::write(&file, scene().to_ron_string().expect("ron")).expect("write");
        assert_eq!(
            guids(&read_level_as_stored(&file).expect("file form")),
            vec![0x30, 0x10, 0x20]
        );
        assert_eq!(
            guids(&read_level_source(&file).expect("file form")),
            vec![0x10, 0x20, 0x30]
        );

        let dir = root.join("b.level.d");
        write_level_dir(&dir, &scene()).expect("dir");
        assert_eq!(
            guids(&read_level_as_stored(&dir).expect("dir form")),
            vec![0x10, 0x20, 0x30]
        );
        // Either spelling names the directory form.
        assert_eq!(
            guids(&read_level_as_stored(&root.join("b.level.ron")).expect("either spelling")),
            vec![0x10, 0x20, 0x30]
        );
        assert_eq!(
            level_dir_versions(&dir).expect("versions"),
            (
                LEVEL_DIR_FORMAT_VERSION,
                SCENE_FORMAT_VERSION,
                SCENE_FORMAT_VERSION
            )
        );

        write_level_dir(&root.join("a.level.d"), &scene()).expect("second form");
        assert_eq!(
            read_level_as_stored(&file).map_err(|e| e.reason()).err(),
            Some("ambiguous_level_form")
        );
        assert_eq!(
            read_level_as_stored(&root.join("missing.level.ron"))
                .map_err(|e| e.reason())
                .err(),
            Some("level_not_found")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The load generation is 0 for a level that is not active, moves on every open of the same
    /// id, and is per level.
    #[test]
    fn load_generation_moves_per_open() {
        let a = LevelId(Guid::from_u128(0xa));
        let b = LevelId(Guid::from_u128(0xb));
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        for id in [a, b] {
            let text =
                format!("SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [], resources: {{}})");
            let level = Level::from_ron_str(id, &text).expect("level");
            app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        }
        let generation = |app: &App, id| app.world().resource::<LevelManager>().load_generation(id);
        assert_eq!(generation(&app, a), 0);
        open_level(app.world_mut(), a, LoadMode::Replace).expect("open a");
        app.update();
        let first = generation(&app, a);
        assert!(first > 0);
        assert_eq!(generation(&app, b), 0);
        open_level(app.world_mut(), a, LoadMode::Replace).expect("reopen a");
        app.update();
        let second = generation(&app, a);
        assert!(second > first, "{second} > {first}");
        open_level(app.world_mut(), b, LoadMode::Additive).expect("open b");
        app.update();
        assert!(generation(&app, b) > 0);
        assert_eq!(generation(&app, a), second, "opening b leaves a alone");
    }
}
