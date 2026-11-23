// SPDX-License-Identifier: MPL-2.0
use super::*;

const T: &str = "regia_scene::transform::Transform";
const LAMP: &str = "game::Lamp";

fn g(n: u128) -> Guid {
    Guid::from_u128(n)
}

const BASE_ID: u128 = 0xB0;
const VARIANT_ID: u128 = 0xB1;
const A: u128 = 0xA1;
const B: u128 = 0xA2;
const C: u128 = 0xA3;
const D: u128 = 0xA4;

/// The base: three entities; `C` carries a lamp whose intensity is `lamp`.
fn base_text(lamp: f32) -> String {
    format!(
        r#"SceneFile(
    format_version: 13,
    entities: [
        (guid: "{a}", name: "A", components: {{ "{T}": (translation: (0.0, 0.0, 0.0), scale: (1.0, 1.0, 1.0)) }}),
        (guid: "{b}", name: "B", components: {{ "{T}": (translation: (1.0, 0.0, 0.0), scale: (1.0, 1.0, 1.0)) }}),
        (guid: "{c}", name: "C", components: {{ "{T}": (translation: (2.0, 0.0, 0.0), scale: (1.0, 1.0, 1.0)), "{LAMP}": (intensity: {lamp:?}, color: (1.0, 1.0, 1.0)) }}),
    ],
)
"#,
        a = g(A),
        b = g(B),
        c = g(C),
    )
}

fn base(lamp: f32) -> SceneFile {
    SceneFile::from_ron_str(&base_text(lamp)).expect("base parses")
}

/// The variant: moves `A`, removes `B`, adds `D`.
fn variant_text() -> String {
    format!(
        r#"LevelVariant(
    format_version: 1,
    base: "asset:{base}",
    overrides: [(target: "{a}", component: "{T}", path: ".translation", value: (5.0, 0.0, 0.0))],
    removed_entities: ["{b}"],
    added_entities: [(guid: "{d}", name: "D", components: {{ "{T}": (translation: (9.0, 0.0, 0.0)) }})],
)
"#,
        base = g(BASE_ID),
        a = g(A),
        b = g(B),
        d = g(D),
    )
}

fn variant() -> LevelVariantFile {
    LevelVariantFile::from_ron_str(&variant_text()).expect("variant parses")
}

fn resolve_with(file: LevelVariantFile, base_scene: SceneFile) -> Result<ResolvedLevel, LevelVariantError> {
    let mut load = move |id: Guid| {
        if id == g(BASE_ID) {
            Ok(LevelSource::Level(base_scene.clone()))
        } else {
            Err(LevelVariantError::BaseMissing(id, "not in the test".into()))
        }
    };
    resolve_level_variant(g(VARIANT_ID), LevelSource::Variant(file), &mut load)
}

/// The acceptance case (P-162): a variant that moves one entity, removes one and adds one
/// resolves to exactly the hand-written level, and a later change to the base's third entity
/// flows into it.
#[test]
fn a_variant_moves_removes_and_adds_and_resolves_to_the_expected_bytes() {
    let resolved = resolve_with(variant(), base(100.0)).expect("resolves");
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    assert_eq!(resolved.chain, vec![g(VARIANT_ID), g(BASE_ID)]);
    let expected = format!(
        r#"SceneFile(
    format_version: 13,
    entities: [
        (guid: "{a}", name: "A", components: {{ "{T}": (translation: (5.0, 0.0, 0.0), scale: (1.0, 1.0, 1.0)) }}),
        (guid: "{c}", name: "C", components: {{ "{T}": (translation: (2.0, 0.0, 0.0), scale: (1.0, 1.0, 1.0)), "{LAMP}": (intensity: 100.0, color: (1.0, 1.0, 1.0)) }}),
        (guid: "{d}", name: "D", components: {{ "{T}": (translation: (9.0, 0.0, 0.0)) }}),
    ],
)
"#,
        a = g(A),
        c = g(C),
        d = g(D),
    );
    let expected = SceneFile::from_ron_str(&expected).expect("expected parses");
    assert_eq!(
        resolved.scene.to_ron_string().expect("ron"),
        expected.to_ron_string().expect("ron"),
        "resolved bytes"
    );

    // The base changes its third entity: the variant follows.
    let later = resolve_with(variant(), base(250.0)).expect("resolves");
    let lamp = later
        .scene
        .entities
        .iter()
        .find(|e| e.guid == g(C))
        .and_then(|e| e.components.get(LAMP))
        .map(|v| v.get_ron().to_owned());
    assert_eq!(
        lamp.as_deref(),
        Some("(intensity: 250.0, color: (1.0, 1.0, 1.0))")
    );
}

#[test]
fn an_override_wins_over_a_base_change_to_the_same_field_only() {
    let mut file = variant();
    file.overrides.push(PrefabOverride {
        target: g(C),
        component: LAMP.into(),
        path: ".intensity".into(),
        value: raw("7.0").expect("raw"),
    });
    let resolved = resolve_with(file, base(250.0)).expect("resolves");
    let lamp = resolved
        .scene
        .entities
        .iter()
        .find(|e| e.guid == g(C))
        .expect("c")
        .components[LAMP]
        .get_ron();
    assert_eq!(lamp, "(intensity: 7.0, color: (1.0, 1.0, 1.0))");
}

#[test]
fn a_loop_is_refused_with_variant_loop() {
    // v1 -> v2 -> v1.
    let v1 = LevelVariantFile::new(g(0xC2));
    let v2 = LevelVariantFile::new(g(0xC1));
    let mut load = |id: Guid| match id.as_u128() {
        0xC1 => Ok(LevelSource::Variant(v1.clone())),
        0xC2 => Ok(LevelSource::Variant(v2.clone())),
        _ => Err(LevelVariantError::BaseMissing(id, "none".into())),
    };
    let err = resolve_level_variant(g(0xC1), LevelSource::Variant(v1.clone()), &mut load).expect_err("loop");
    assert_eq!(err.reason(), "variant_loop");
    assert!(matches!(&err, LevelVariantError::Loop(chain) if chain == &vec![g(0xC1), g(0xC2), g(0xC1)]));
    // A variant naming itself is the shortest loop.
    let own = LevelVariantFile::new(g(0xC1));
    let err = resolve_level_variant(g(0xC1), LevelSource::Variant(own), &mut load).expect_err("self");
    assert_eq!(err.reason(), "variant_loop");
}

#[test]
fn a_chain_of_variants_applies_base_most_first_and_stops_at_the_limit() {
    // v2 is a variant of the variant: it moves A again and renames C.
    let mut v2 = LevelVariantFile::new(g(VARIANT_ID));
    v2.overrides.push(PrefabOverride {
        target: g(A),
        component: T.into(),
        path: ".translation".into(),
        value: raw("(6.0, 0.0, 0.0)").expect("raw"),
    });
    v2.record_edits.push(LevelRecordEdit {
        target: g(C),
        field: "name".into(),
        value: raw("Some(\"Lamp post\")").expect("raw"),
    });
    let v1 = variant();
    let base_scene = base(100.0);
    let mut load = |id: Guid| match id.as_u128() {
        VARIANT_ID => Ok(LevelSource::Variant(v1.clone())),
        BASE_ID => Ok(LevelSource::Level(base_scene.clone())),
        _ => Err(LevelVariantError::BaseMissing(id, "none".into())),
    };
    let resolved = resolve_level_variant(g(0xB2), LevelSource::Variant(v2), &mut load).expect("chain");
    assert_eq!(resolved.chain, vec![g(0xB2), g(VARIANT_ID), g(BASE_ID)]);
    let a = resolved
        .scene
        .entities
        .iter()
        .find(|e| e.guid == g(A))
        .expect("a");
    assert_eq!(
        a.components[T].get_ron(),
        "(translation: (6.0, 0.0, 0.0), scale: (1.0, 1.0, 1.0))"
    );
    let c = resolved
        .scene
        .entities
        .iter()
        .find(|e| e.guid == g(C))
        .expect("c");
    assert_eq!(c.name.as_deref(), Some("Lamp post"));
    assert!(resolved.scene.entities.iter().all(|e| e.guid != g(B)));

    // A chain of 20 variants is cut off.
    let mut load = |id: Guid| -> Result<LevelSource, LevelVariantError> {
        Ok(LevelSource::Variant(LevelVariantFile::new(g(id.as_u128() + 1))))
    };
    let err = resolve_level_variant(g(1), LevelSource::Variant(LevelVariantFile::new(g(2))), &mut load)
        .expect_err("too long");
    assert_eq!(err.reason(), "chain_too_long");
}

#[test]
fn removing_an_entity_removes_its_descendants_and_missing_targets_warn() {
    let mut scene = base(1.0);
    scene.entities[2].parent = Some(g(B)); // C under B
    let mut file = LevelVariantFile::new(g(BASE_ID));
    file.removed_entities.push(g(B));
    file.removed_entities.push(g(0xDEAD));
    file.overrides.push(PrefabOverride {
        target: g(0xBEEF),
        component: T.into(),
        path: ".translation".into(),
        value: raw("(1.0, 1.0, 1.0)").expect("raw"),
    });
    let resolved = resolve_with(file, scene).expect("resolves");
    let left: Vec<Guid> = resolved.scene.entities.iter().map(|e| e.guid).collect();
    assert_eq!(left, vec![g(A)]);
    assert_eq!(resolved.warnings.len(), 2, "{:?}", resolved.warnings);
}

#[test]
fn an_added_entity_under_a_removed_one_is_refused_as_resolved_invalid() {
    let mut file = variant();
    file.added_entities[0].parent = Some(g(B));
    let err = resolve_with(file, base(1.0)).expect_err("orphan");
    assert_eq!(err.reason(), "resolved_invalid");
}

#[test]
fn the_file_round_trips_and_newer_or_broken_files_are_refused() {
    let file = variant();
    let text = file.to_ron_string().expect("ron");
    assert!(text.starts_with("LevelVariant("), "{text}");
    assert_eq!(LevelVariantFile::from_ron_str(&text).expect("again"), file);
    assert_eq!(
        LevelVariantFile::from_ron_str(&text)
            .expect("again")
            .to_ron_string()
            .expect("ron"),
        text
    );
    let newer = text.replace("format_version: 1", "format_version: 9");
    assert_eq!(
        LevelVariantFile::from_ron_str(&newer)
            .expect_err("newer")
            .reason(),
        "newer_version"
    );
    let versionless = text.replace("format_version: 1,", "");
    assert_eq!(
        LevelVariantFile::from_ron_str(&versionless)
            .expect("v0")
            .format_version,
        1
    );
    let nil = text.replace(&g(BASE_ID).to_string(), &Guid::from_u128(0).to_string());
    assert_eq!(
        LevelVariantFile::from_ron_str(&nil).expect_err("nil").reason(),
        "bad_variant"
    );
    let mut bad_field = file.clone();
    bad_field.record_edits.push(LevelRecordEdit {
        target: g(A),
        field: "components".into(),
        value: raw("None").expect("raw"),
    });
    assert!(bad_field.validate().is_err());
    let mut twice = file;
    twice.added_entities.push(twice.added_entities[0].clone());
    assert!(twice.validate().is_err());
}

#[test]
fn a_flattened_variant_reads_as_a_level() {
    let text = base(1.0).to_ron_string().expect("ron");
    assert!(matches!(read_source_text(&text), Ok(LevelSource::Level(_))));
    assert!(matches!(
        read_source_text(&variant_text()),
        Ok(LevelSource::Variant(_))
    ));
    assert!(read_source_text("nonsense").is_err());
}

#[test]
fn diff_then_resolve_gives_the_edited_level_back() {
    let resolved = resolve_with(variant(), base(100.0)).expect("resolves").scene;
    let diffed = diff_levels(g(BASE_ID), &base(100.0), &resolved);
    assert_eq!(diffed.removed_entities, vec![g(B)]);
    assert_eq!(diffed.added_entities.len(), 1);
    assert_eq!(diffed.overrides.len(), 1);
    assert_eq!(diffed.overrides[0].path, ".translation");
    // The diff of the file's own resolution is the file (same bytes).
    assert_eq!(
        diffed.to_ron_string().expect("ron"),
        variant().to_ron_string().expect("ron")
    );
    let again = resolve_with(diffed, base(100.0)).expect("again").scene;
    assert_eq!(
        again.to_ron_string().expect("ron"),
        resolved.to_ron_string().expect("ron")
    );
}

#[test]
fn revert_drops_entity_and_field_differences() {
    let mut file = variant();
    file.overrides.push(PrefabOverride {
        target: g(A),
        component: T.into(),
        path: ".scale".into(),
        value: raw("(2.0, 2.0, 2.0)").expect("raw"),
    });
    assert_eq!(file.mark(g(A)), EntityMark::Overridden);
    assert_eq!(file.mark(g(D)), EntityMark::Added);
    assert_eq!(file.mark(g(C)), EntityMark::Base);
    assert_eq!(
        file.overridden_fields(g(A)),
        vec![
            (T.to_owned(), ".scale".to_owned()),
            (T.to_owned(), ".translation".to_owned())
        ]
    );
    assert_eq!(file.revert_field(g(A), T, ".scale"), 1);
    assert_eq!(file.overrides.len(), 1);
    assert_eq!(file.revert_entity(g(A)), 1);
    assert_eq!(file.revert_entity(g(B)), 1, "the removal goes");
    assert_eq!(file.revert_entity(g(D)), 1, "the added entity goes");
    assert!(file.is_empty());
}

/// A tiny deterministic generator (no wall clock, no crate).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n
    }
}

/// 50 random edits of the resolved level (move, rename, reparent, add, remove, component add and
/// drop, resource set) each round-trip through diff and resolve to the same bytes, and undoing
/// them all (back to the opened level) writes the original variant's bytes again.
#[test]
fn fifty_random_edits_round_trip_and_a_full_undo_restores_the_original_bytes() {
    let base_scene = base(100.0);
    let original = variant();
    let original_bytes = original.to_ron_string().expect("ron");
    let opened = resolve_with(original, base_scene.clone())
        .expect("resolves")
        .scene;
    let mut rng = Lcg(0x5EED);
    let mut history: Vec<SceneFile> = Vec::new();
    let mut current = opened.clone();
    let mut next_guid = 0xE000u128;
    for step in 0..50 {
        history.push(current.clone());
        let n = current.entities.len();
        match rng.next(7) {
            0 if n > 0 => {
                let i = rng.next(n);
                let text = format!(
                    "(translation: ({}.0, 1.0, 0.0), scale: (1.0, 1.0, 1.0))",
                    rng.next(100)
                );
                current.entities[i]
                    .components
                    .insert(T.into(), raw(&text).expect("raw"));
            }
            1 if n > 0 => {
                let i = rng.next(n);
                current.entities[i].name = Some(format!("renamed {step}"));
            }
            2 if n > 1 => {
                // Parent to a root other than itself (roots never form a cycle).
                let i = rng.next(n);
                let roots: Vec<Guid> = current
                    .entities
                    .iter()
                    .filter(|e| e.parent.is_none() && e.guid != current.entities[i].guid)
                    .map(|e| e.guid)
                    .collect();
                if !roots.is_empty()
                    && !current
                        .entities
                        .iter()
                        .any(|e| e.parent == Some(current.entities[i].guid))
                {
                    current.entities[i].parent = Some(roots[rng.next(roots.len())]);
                }
            }
            3 => {
                next_guid += 1;
                current.entities.push(SceneEntity {
                    guid: g(next_guid),
                    name: Some(format!("new {step}")),
                    ..SceneEntity::default()
                });
            }
            4 if n > 0 => {
                let i = rng.next(n);
                let gone = current.entities[i].guid;
                let mut doomed = BTreeSet::from([gone]);
                loop {
                    let k = doomed.len();
                    for e in &current.entities {
                        if e.parent.is_some_and(|p| doomed.contains(&p)) {
                            doomed.insert(e.guid);
                        }
                    }
                    if doomed.len() == k {
                        break;
                    }
                }
                current.entities.retain(|e| !doomed.contains(&e.guid));
            }
            5 if n > 0 => {
                let i = rng.next(n);
                let lamp = format!("(intensity: {}.5, color: (1.0, 1.0, 1.0))", rng.next(9));
                let entity = &mut current.entities[i];
                if entity.components.remove(LAMP).is_none() {
                    entity.components.insert(LAMP.into(), raw(&lamp).expect("raw"));
                }
            }
            _ => {
                current.resources.insert(
                    "game::Weather".into(),
                    raw(&format!("(rain: {})", rng.next(2) == 1)).expect("raw"),
                );
            }
        }
        current.validate().expect("the edit keeps the level valid");
        let file = diff_levels(g(BASE_ID), &base_scene, &current);
        let file = LevelVariantFile::from_ron_str(&file.to_ron_string().expect("ron")).expect("re-reads");
        let resolved = resolve_with(file, base_scene.clone()).expect("resolves");
        assert!(
            resolved.warnings.is_empty(),
            "step {step}: {:?}",
            resolved.warnings
        );
        // Entity order: base survivors first, then additions; compare as guid-sorted bytes.
        let sorted = |scene: &SceneFile| {
            let mut scene = scene.clone();
            scene.entities.sort_by_key(|e| e.guid);
            scene.to_ron_string().expect("ron")
        };
        assert_eq!(sorted(&resolved.scene), sorted(&current), "step {step}");
    }
    while let Some(previous) = history.pop() {
        current = previous;
    }
    assert_eq!(current, opened);
    let undone = diff_levels(g(BASE_ID), &base_scene, &current);
    assert_eq!(undone.to_ron_string().expect("ron"), original_bytes);
}

#[test]
fn the_fingerprint_names_the_document_fields() {
    let fingerprint = level_variant_schema_fingerprint();
    for field in [
        "base",
        "overrides",
        "record_edits",
        "added_entities",
        "removed_entities",
        "removed_resources",
    ] {
        assert!(fingerprint.contains(field), "{field}: {fingerprint}");
    }
    assert_eq!(level_variant_migration_sources(), vec![0]);
}
