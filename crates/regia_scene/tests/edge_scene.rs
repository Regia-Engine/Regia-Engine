// SPDX-License-Identifier: MPL-2.0
//! Edge cases of the scene crate's data paths: transform decomposition of degenerate matrices,
//! the format migration chain from every old version, level settings round trips, and small
//! component helpers. Deterministic, no GPU.

use bevy_ecs::hierarchy::ChildOf;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::{MAX_MORPH_TARGETS, MorphTargetNames, MorphWeights, Name, Tags};
use regia_scene::cooked::{
    COOKED_SCENE_VERSION, CookedEntity, CookedParent, CookedScene, CookedSceneError, Payload,
};
use regia_scene::format::{
    SCENE_FORMAT_VERSION, SceneFile, SceneFormatError, builtin_migrations, migrate_to_latest,
    scene_migration_sources,
};
use regia_scene::level::{AtmospherePreset, Level, LevelId, LevelSettings};
use regia_scene::prefab::PrefabLibrary;
use regia_scene::spawn::SpawnOptions;
use regia_scene::{GlobalTransform, Transform};

fn level_id() -> LevelId {
    LevelId(Guid::from_u128(0xA11CE))
}

// ---------------------------------------------------------------- transform

fn close(a: Vec3, b: Vec3, tol: f32) -> bool {
    (a - b).length() <= tol
}

/// A matrix with one non-zero axis column keeps the direction of that column through a
/// decompose/compose cycle (the roll about it is arbitrary, the direction is not).
#[test]
fn a_single_surviving_axis_keeps_its_direction() {
    let dir = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    for (scale, basis) in [
        (Vec3::new(2.0, 0.0, 0.0), Vec3::X),
        (Vec3::new(0.0, 3.0, 0.0), Vec3::Y),
        (Vec3::new(0.0, 0.0, 4.0), Vec3::Z),
    ] {
        let t = Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: dir,
            scale,
        };
        let back = Transform::from_affine(t.compute_affine());
        let want = t.transform_point(basis) - t.translation;
        let got = back.transform_point(basis) - back.translation;
        assert!(close(got, want, 1.0e-4), "scale {scale}: want {want}, got {got}");
    }
}

/// Fully collapsed and non-finite matrices decompose without panicking and keep their translation.
#[test]
fn degenerate_matrices_decompose_without_panicking() {
    let zero = Transform::from_affine(Affine3A::from_scale_rotation_translation(
        Vec3::ZERO,
        Quat::from_rotation_x(1.0),
        Vec3::new(4.0, 5.0, 6.0),
    ));
    assert_eq!(zero.translation, Vec3::new(4.0, 5.0, 6.0));
    assert_eq!(zero.scale, Vec3::ZERO);
    assert_eq!(zero.rotation, Quat::IDENTITY);
    let nan = Transform::from_affine(Affine3A::from_scale_rotation_translation(
        Vec3::new(f32::NAN, 1.0, 1.0),
        Quat::IDENTITY,
        Vec3::ZERO,
    ));
    assert!(nan.scale.x.is_nan());
    let inf = Transform::from_affine(Affine3A::from_translation(Vec3::splat(f32::INFINITY)));
    assert!(inf.translation.x.is_infinite());
}

/// A seeded sweep of rotations and scales (including mirrored ones) survives
/// compose -> decompose -> compose to within f32 noise.
#[test]
fn affine_round_trip_over_a_fixed_seed_sweep() {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 24) as f32
    };
    for i in 0..500 {
        let axis = Vec3::new(next() - 0.5, next() - 0.5, next() - 0.5);
        let axis = if axis.length() < 1.0e-3 {
            Vec3::Y
        } else {
            axis.normalize()
        };
        let t = Transform {
            translation: Vec3::new(
                next() * 200.0 - 100.0,
                next() * 200.0 - 100.0,
                next() * 200.0 - 100.0,
            ),
            rotation: Quat::from_axis_angle(axis, next() * std::f32::consts::TAU),
            scale: Vec3::new(0.1 + next() * 5.0, 0.1 + next() * 5.0, 0.1 + next() * 5.0)
                * if i % 5 == 0 {
                    Vec3::new(-1.0, 1.0, 1.0)
                } else {
                    Vec3::ONE
                },
        };
        let a = t.compute_affine();
        let b = Transform::from_affine(a).compute_affine();
        for p in [Vec3::X, Vec3::Y, Vec3::Z, Vec3::new(1.0, -2.0, 3.0)] {
            let (pa, pb) = (a.transform_point3(p), b.transform_point3(p));
            assert!(
                close(pa, pb, 2.0e-3 * (1.0 + pa.length())),
                "iteration {i}: {pa} vs {pb}"
            );
        }
    }
}

/// `GlobalTransform` composition is associative with the identity on both sides.
#[test]
fn global_transform_identity_laws() {
    let t = Transform {
        translation: Vec3::new(1.0, 2.0, 3.0),
        rotation: Quat::from_rotation_y(0.7),
        scale: Vec3::new(1.0, 2.0, 3.0),
    };
    let g = GlobalTransform::IDENTITY * t;
    assert_eq!(
        GlobalTransform::IDENTITY * Transform::IDENTITY,
        GlobalTransform::IDENTITY
    );
    assert_eq!(g * Transform::IDENTITY, g);
    assert!(close(g.to_transform().translation, t.translation, 1.0e-6));
}

// ---------------------------------------------------------------- format migration

/// A level text written as format version `v` with a settings resource that names only a gravity.
fn level_text(v: u32) -> String {
    format!(
        r#"(
    format_version: {v},
    entities: [],
    resources: {{
        "regia_scene::level::LevelSettings": (gravity: (0.0, -3.7, 0.0)),
    }},
)"#
    )
}

/// Every version 1..=current loads, ends at the current version, and the migrated files all
/// spell out the same settings (the chain from any start reaches the same document).
#[test]
fn every_old_version_migrates_to_the_same_document() {
    let reference = Level::from_ron_str(level_id(), &level_text(SCENE_FORMAT_VERSION)).expect("current");
    let want = reference.to_ron_string().expect("ron");
    for v in 1..=SCENE_FORMAT_VERSION {
        let level = Level::from_ron_str(level_id(), &level_text(v)).unwrap_or_else(|e| panic!("v{v}: {e}"));
        assert_eq!(level.scene.format_version, SCENE_FORMAT_VERSION, "v{v}");
        assert_eq!(level.settings.gravity, Vec3::new(0.0, -3.7, 0.0), "v{v}");
        assert_eq!(
            level.to_ron_string().expect("ron"),
            want,
            "v{v} differs from current"
        );
    }
}

/// The chain is gapless, steps by one, and starts at 1; version 0 and a missing-resource file
/// behave as documented.
#[test]
fn the_builtin_chain_is_complete() {
    let chain = builtin_migrations();
    assert_eq!(chain.len() as u32, SCENE_FORMAT_VERSION - 1);
    for (i, step) in chain.iter().enumerate() {
        assert_eq!(step.from, i as u32 + 1);
        assert_eq!(step.to, step.from + 1);
    }
    assert_eq!(
        scene_migration_sources(),
        (1..SCENE_FORMAT_VERSION).collect::<Vec<_>>()
    );
    // Version 0 never existed: there is no step, so the load says so rather than guessing.
    match SceneFile::from_ron_str("(format_version: 0, entities: [])") {
        Err(SceneFormatError::Migration { from: 0, .. }) => {}
        other => panic!("{other:?}"),
    }
    // A file without any level settings migrates by changing nothing but the version.
    let mut file = SceneFile::new();
    file.format_version = 1;
    assert_eq!(
        migrate_to_latest(&mut file).expect("migrate"),
        SCENE_FORMAT_VERSION
    );
    assert!(file.resources.is_empty());
    // Migrating twice is a no-op.
    let before = file.clone();
    assert_eq!(migrate_to_latest(&mut file).expect("again"), SCENE_FORMAT_VERSION);
    assert_eq!(file, before);
}

/// A settings resource that does not parse fails the migration with a named step, and the same
/// text at the current version fails in `Level::from_ron_str` instead of silently defaulting.
#[test]
fn unparseable_settings_fail_loudly() {
    let broken = |v: u32| {
        format!(
            r#"(
    format_version: {v},
    entities: [],
    resources: {{ "regia_scene::level::LevelSettings": (gravity: "up"), }},
)"#
        )
    };
    match Level::from_ron_str(level_id(), &broken(3)) {
        Err(SceneFormatError::Migration { from: 3, to: 4, .. }) => {}
        other => panic!("{other:?}"),
    }
    assert!(Level::from_ron_str(level_id(), &broken(SCENE_FORMAT_VERSION)).is_err());
    // The newer-version check wins over everything else, even a broken body.
    match SceneFile::from_ron_str(&broken(SCENE_FORMAT_VERSION + 1)) {
        Err(SceneFormatError::UnsupportedVersion { found, supported }) => {
            assert_eq!(
                (found, supported),
                (SCENE_FORMAT_VERSION + 1, SCENE_FORMAT_VERSION)
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        SceneFile::from_ron_str(&format!("(format_version: {}, entities: [])", u32::MAX)),
        Err(SceneFormatError::UnsupportedVersion { .. })
    ));
}

/// Postcard bytes of an old version migrate on load exactly as text does.
#[test]
fn postcard_of_an_old_version_migrates_too() {
    let mut file = SceneFile::from_ron_str(&level_text(SCENE_FORMAT_VERSION)).expect("current");
    file.format_version = 4;
    let bytes = file.to_postcard().expect("bytes");
    let back = SceneFile::from_postcard(&bytes).expect("migrates");
    assert_eq!(back.format_version, SCENE_FORMAT_VERSION);
    file.format_version = SCENE_FORMAT_VERSION + 3;
    let newer = file.to_postcard().expect("bytes");
    assert!(matches!(
        SceneFile::from_postcard(&newer),
        Err(SceneFormatError::UnsupportedVersion { .. })
    ));
}

// ---------------------------------------------------------------- level settings

/// Edited settings win over the stored resource on save, and the saved text reloads to the same value.
#[test]
fn level_settings_edits_survive_a_save_and_reload() {
    let mut level = Level::from_ron_str(level_id(), &level_text(SCENE_FORMAT_VERSION)).expect("level");
    level.settings.gravity = Vec3::new(0.0, -1.62, 0.0);
    level.settings.atmosphere.preset = AtmospherePreset::Airless;
    let text = level.to_ron_string().expect("save");
    let back = Level::from_ron_str(level_id(), &text).expect("reload");
    assert_eq!(back.settings, level.settings);
    // Saving the reloaded level again is byte-identical (canonical output).
    assert_eq!(back.to_ron_string().expect("save"), text);
}

/// Settings round-trip through RON for a few awkward values: NaN-free, negative zero, and the
/// extremes of the float range.
#[test]
fn level_settings_ron_round_trip_with_extreme_floats() {
    for g in [
        Vec3::new(0.0, -0.0, 0.0),
        Vec3::new(f32::MAX, f32::MIN, f32::MIN_POSITIVE),
        Vec3::new(1.0e-30, -1.0e30, 0.1),
    ] {
        let s = LevelSettings {
            gravity: g,
            ..LevelSettings::default()
        };
        let back = LevelSettings::from_ron(&s.to_ron().expect("ron")).expect("parse");
        assert_eq!(
            back.gravity.to_array().map(f32::to_bits),
            g.to_array().map(f32::to_bits)
        );
    }
    assert!(LevelSettings::from_ron("").is_err());
    assert!(LevelSettings::from_ron("(gravity: (1.0, 2.0))").is_err());
}

// ---------------------------------------------------------------- components

/// Named morph helpers: unknown names change nothing, duplicates resolve to the first, an index
/// past the cap is refused.
#[test]
fn morph_weights_named_access() {
    let names = MorphTargetNames::new(["smile", "blink", "smile"]);
    assert_eq!(names.len(), 3);
    assert!(!names.is_empty());
    assert!(MorphTargetNames::default().is_empty());
    assert_eq!(names.index_of("smile"), Some(0));
    assert_eq!(names.index_of("missing"), None);
    let mut w = MorphWeights::default();
    assert!(!w.set_named(&names, "missing", 1.0));
    assert!(w.0.is_empty());
    assert!(w.set_named(&names, "blink", 0.25));
    assert_eq!(w.0, vec![0.0, 0.25]);
    assert_eq!(w.get_named(&names, "blink"), Some(0.25));
    assert_eq!(w.get_named(&names, "missing"), None);
    assert_eq!(w.get(MAX_MORPH_TARGETS * 2), 0.0);
    // NaN is stored as given: the caller decides, `set` only guards the index.
    assert!(w.set(0, f32::NAN));
    assert!(w.get(0).is_nan());
}

/// Tag lookup is exact (case and whitespace matter) and an empty set contains nothing.
#[test]
fn tags_match_exactly() {
    let tags = Tags::from_iter_str(["Crate", "loot "]);
    assert!(tags.contains("Crate"));
    assert!(!tags.contains("crate"));
    assert!(!tags.contains("loot"));
    assert!(!tags.contains(""));
    assert!(!Tags::default().contains(""));
}

// ---------------------------------------------------------------- cooked scene, hostile structure

fn cooked_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
    }
    world.init_resource::<PrefabLibrary>();
    world
}

fn cooked_entity(guid: u128, name: Option<u32>, parent: Option<CookedParent>) -> CookedEntity {
    CookedEntity {
        guid: Guid::from_u128(guid),
        name,
        parent,
        components: Vec::new(),
        prefab: None,
    }
}

fn cooked(strings: &[&str], entities: Vec<CookedEntity>) -> CookedScene {
    CookedScene {
        format_version: COOKED_SCENE_VERSION,
        strings: strings.iter().map(ToString::to_string).collect(),
        entities,
        resources: Vec::new(),
    }
}

/// A newer cooked version is refused by the reader, with the version in the error.
#[test]
fn a_newer_cooked_version_is_refused() {
    let mut scene = cooked(&[], Vec::new());
    scene.format_version = COOKED_SCENE_VERSION + 1;
    let bytes = scene.to_bytes().expect("bytes");
    match CookedScene::from_bytes(&bytes) {
        Err(CookedSceneError::UnsupportedVersion(v)) => assert_eq!(v, COOKED_SCENE_VERSION + 1),
        other => panic!("{other:?}"),
    }
}

/// Parent indices out of range, a parent pointing at itself and a two-entity loop never panic or
/// hang; the offending links are skipped with a warning and every entity still spawns.
#[test]
fn cooked_parent_links_that_cannot_hold_are_skipped_with_warnings() {
    let scene = cooked(
        &["Root", "Child"],
        vec![
            cooked_entity(1, Some(0), None),
            cooked_entity(2, Some(1), Some(CookedParent::Index(0))),
            cooked_entity(3, None, Some(CookedParent::Index(99))),
            cooked_entity(4, None, Some(CookedParent::Index(3))),
            cooked_entity(5, None, Some(CookedParent::Index(5))),
            cooked_entity(6, None, Some(CookedParent::Index(4))),
            cooked_entity(7, None, Some(CookedParent::PrefabLocal(Guid::from_u128(0xDEAD)))),
        ],
    );
    let mut world = cooked_world();
    let spawned = scene.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
    assert_eq!(spawned.entities, 7);
    let joined = spawned.warnings.join("\n");
    assert!(joined.contains("not found"), "{joined}");
    assert!(joined.contains("cycle"), "{joined}");
    // The one named link took effect.
    let mut query = world.query::<(&Name, &ChildOf)>();
    let linked: Vec<_> = query.iter(&world).map(|(n, _)| n.as_str().to_string()).collect();
    assert_eq!(linked, vec!["Child".to_string()]);
    // Roots: Root, the two unresolved links, the self-parent, and at least one end of the loop.
    assert!(
        (5..=6).contains(&spawned.roots.len()),
        "{} roots",
        spawned.roots.len()
    );
}

/// Out-of-range string indices (component type, resource type) warn instead of panicking, and a
/// name index out of range leaves the entity unnamed.
#[test]
fn cooked_string_indices_out_of_range_warn() {
    let mut entity = cooked_entity(1, Some(50), None);
    entity.components.push((77, Payload::Raw("()".into())));
    let mut scene = cooked(&["regia_scene::transform::Transform"], vec![entity]);
    scene.resources.push((88, Payload::Raw("()".into())));
    let mut world = cooked_world();
    let spawned = scene.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
    assert_eq!(spawned.entities, 1);
    let joined = spawned.warnings.join("\n");
    assert!(joined.contains("component type-path string index 77"), "{joined}");
    assert!(joined.contains("resource type-path string index 88"), "{joined}");
    let mut names = world.query::<&Name>();
    assert_eq!(names.iter(&world).count(), 0);
}

/// A postcard payload with garbage bytes, and a raw payload that is not valid RON for its type,
/// each produce a warning naming the type; the entity itself survives with its other components.
#[test]
fn cooked_payloads_that_do_not_decode_warn_and_keep_the_entity() {
    let mut entity = cooked_entity(1, None, None);
    entity.components.push((
        0,
        Payload::Postcard {
            type_hash: 0,
            bytes: vec![0xFF; 3],
        },
    ));
    entity
        .components
        .push((0, Payload::Raw("(translation: \"far\")".into())));
    entity.components.push((1, Payload::Raw("\"Kept\"".into())));
    let scene = cooked(
        &[
            "regia_scene::transform::Transform",
            "regia_scene::components::Name",
        ],
        vec![entity],
    );
    let mut world = cooked_world();
    let spawned = scene.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
    assert_eq!(spawned.warnings.len(), 2, "{:?}", spawned.warnings);
    assert!(spawned.warnings.iter().all(|w| w.contains("Transform")));
    let mut names = world.query::<&Name>();
    assert_eq!(names.single(&world).map(Name::as_str).ok(), Some("Kept"));
}

/// Spawning the same cooked scene twice into one world gives two independent copies, and the same
/// scene into two fresh worlds gives the same warnings and counts (determinism).
#[test]
fn cooked_spawn_is_repeatable() {
    let scene = cooked(
        &["A"],
        vec![
            cooked_entity(1, Some(0), None),
            cooked_entity(2, None, Some(CookedParent::Index(0))),
            cooked_entity(3, None, Some(CookedParent::Index(9))),
        ],
    );
    let run = || {
        let mut world = cooked_world();
        let a = scene.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
        (a.entities, a.roots.len(), a.warnings)
    };
    assert_eq!(run(), run());
    let mut world = cooked_world();
    scene.spawn(&mut world, &SpawnOptions::default()).expect("first");
    scene.spawn(&mut world, &SpawnOptions::default()).expect("second");
    let mut guids = world.query::<&regia_scene::components::EntityGuid>();
    assert_eq!(guids.iter(&world).count(), 6);
}

// ---------------------------------------------------------------- scene text, lexical edge cases

fn entity_names(file: &SceneFile) -> Vec<String> {
    file.entities
        .iter()
        .map(|e| e.name.clone().unwrap_or_default())
        .collect()
}

/// Documents the lexical splitter must neither mis-split nor reject: brackets in raw strings and
/// chars, comments in odd places, CRLF, Windows line ends inside strings, tabs, a trailing comma,
/// non-ASCII names, and the bare (unnamed) struct form.
#[test]
fn tricky_but_valid_documents_load_the_same_entities() {
    let guid = |n: u128| Guid::from_u128(n);
    let docs = [
        // Raw string with a closing bracket and a hash quote inside.
        format!(
            "(format_version: 10, entities: [(guid: \"{}\", name: Some(r##\"a]\"#b\"##)),],)",
            guid(1)
        ),
        // Block comment between key and colon, line comment after the list.
        format!(
            "(format_version: 10, entities /* x ] */ : [ (guid: \"{}\", name: Some(\"n\")) ] // ]\n)",
            guid(1)
        ),
        // CRLF everywhere, tabs.
        format!(
            "(\r\n\tformat_version: 10,\r\n\tentities: [\r\n\t\t(guid: \"{}\", name: Some(\"n\")),\r\n\t],\r\n)",
            guid(1)
        ),
        // Char literal holding a bracket, non-ASCII name.
        format!(
            "(format_version: 10, entities: [(guid: \"{}\", name: Some(\"na\u{ef}ve \u{1f600}\"))])",
            guid(1)
        ),
        // Named struct form.
        format!(
            "SceneFile(format_version: 10, entities: [(guid: \"{}\", name: Some(\"n\"))])",
            guid(1)
        ),
    ];
    let want = ["a]\"#b", "n", "n", "na\u{ef}ve \u{1f600}", "n"];
    for (doc, want) in docs.iter().zip(want) {
        let file = SceneFile::from_ron_str(doc).unwrap_or_else(|e| panic!("{doc:?}: {e}"));
        assert_eq!(entity_names(&file), vec![want.to_string()], "{doc:?}");
    }
}

/// Malformed lists are errors from the whole-document fallback, never a partial load: a missing
/// bracket, a stray close, a second `entities` key, and an unterminated string.
#[test]
fn malformed_entity_lists_are_errors() {
    let e = |n: u128| format!("(guid: \"{}\")", Guid::from_u128(n));
    for doc in [
        format!("(format_version: 10, entities: [{}", e(1)),
        format!("(format_version: 10, entities: [{}]]", e(1)),
        format!("(format_version: 10, entities: [{}], entities: [{}])", e(1), e(2)),
        format!(
            "(format_version: 10, entities: [(guid: \"{}, name: Some(\"x\"))])",
            Guid::from_u128(1)
        ),
        "(format_version: 10, entities: [(guid: \"00000000000000000000000000000001\"),,])".to_string(),
        "(format_version: 10, entities: 5)".to_string(),
        String::new(),
        "[]".to_string(),
    ] {
        assert!(SceneFile::from_ron_str(&doc).is_err(), "{doc:?}");
    }
}

/// A parse error in the Nth entity is positioned in the whole text, on the right line, whether
/// the list is split or parsed whole (an extension line forces the whole-document path).
#[test]
fn entity_error_positions_do_not_depend_on_the_split() {
    let body = |bad: &str| {
        format!(
            "(\n    format_version: 10,\n    entities: [\n        (guid: \"{}\"),\n        (guid: \"{}\"{bad}),\n    ],\n)",
            Guid::from_u128(1),
            Guid::from_u128(2)
        )
    };
    let text = body(", name: Some(oops)");
    let err = SceneFile::from_ron_str(&text).expect_err("bad name");
    match err {
        SceneFormatError::RonParse(e) => assert_eq!(e.span.start.line, 5, "{e}"),
        other => panic!("{other:?}"),
    }
}
