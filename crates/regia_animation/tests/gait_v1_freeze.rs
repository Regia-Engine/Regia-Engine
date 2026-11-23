// SPDX-License-Identifier: MPL-2.0
//! `.gait.ron` v1 freeze proofs (format batch 2, ADR-10266, docs/specs/12-assets.md § 4a).
//!
//! The format corpus is what a future `gait` v2 migrator is tested against, so every shape v1 can
//! hold must be in it: every `GaitPatternSpec` variant and both sides of every optional field. The
//! match below has no wildcard, so a new variant does not compile until a corpus file covers it.
//! Every nested block refuses an unknown field (`gait_parse` naming it), every corpus file is
//! refused at version 0 and 2 (`gait_version`), and the v1 migration is the identity, so running it
//! twice equals once.

use std::collections::BTreeSet;
use std::path::PathBuf;

use regia_animation::gait_asset::{
    GAIT_FORMAT_VERSION, GaitAsset, GaitFile, GaitPatternSpec, gait_migration_sources, migrate_gait_ron,
};
use serde::Deserialize;

/// The corpus folder of the `gait` format.
fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../regia_agent/tests/fixtures/format-corpus/gait")
}

#[derive(Deserialize)]
struct Index {
    format: String,
    entries: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    version: u32,
    file: String,
    origin: String,
    exercises: String,
}

/// Every corpus file of the format: (file name, text), in `index.ron` order.
fn corpus() -> Vec<(String, String)> {
    let dir = corpus_dir();
    let index = std::fs::read_to_string(dir.join("index.ron"))
        .unwrap_or_else(|error| panic!("{}: {error}", dir.join("index.ron").display()));
    let index: Index = ron::from_str(&index).unwrap_or_else(|error| panic!("index.ron: {error}"));
    assert_eq!(index.format, "gait");
    index
        .entries
        .iter()
        .map(|entry| {
            assert_eq!(entry.version, GAIT_FORMAT_VERSION, "{}", entry.file);
            assert!(
                !entry.origin.is_empty() && !entry.exercises.is_empty(),
                "{}",
                entry.file
            );
            let path = dir.join(&entry.file);
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            (entry.file.clone(), text)
        })
        .collect()
}

/// The corpus file `name`.
fn corpus_file(name: &str) -> String {
    corpus()
        .into_iter()
        .find(|(file, _)| file == name)
        .map(|(_, text)| text)
        .unwrap_or_else(|| panic!("{name} is not listed in gait/index.ron"))
}

fn load(name: &str, text: &str) -> GaitAsset {
    GaitAsset::from_ron(text).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// The name of a pattern variant. No wildcard arm: a new variant must be added here, and then to a
/// corpus file, before this test compiles and passes.
fn variant(spec: &GaitPatternSpec) -> &'static str {
    match spec {
        GaitPatternSpec::Tripod => "Tripod",
        GaitPatternSpec::Ripple(_) => "Ripple",
        GaitPatternSpec::Wave(_) => "Wave",
        GaitPatternSpec::QuadrupedWalk => "QuadrupedWalk",
        GaitPatternSpec::Trot => "Trot",
        GaitPatternSpec::Pace => "Pace",
        GaitPatternSpec::Bound => "Bound",
        GaitPatternSpec::BipedWalk => "BipedWalk",
        GaitPatternSpec::Explicit { .. } => "Explicit",
    }
}

/// The shape facts of one file: every variant it uses and both sides of every optional field.
fn facts(file: &GaitFile) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for spec in &file.patterns {
        out.insert(format!("pattern {}", variant(spec)));
    }
    let side = |some: bool| if some { "some" } else { "none" };
    out.insert(format!("switch_speeds {}", side(!file.switch_speeds.is_empty())));
    out.insert(format!("hysteresis {}", side(file.hysteresis > 0.0)));
    out.insert(format!("max_reach {}", side(file.max_reach.is_some())));
    out.insert(format!("ground_band {}", side(file.ground_band.is_some())));
    out.insert(format!("posture {}", side(file.posture.is_some())));
    if let Some(posture) = &file.posture {
        out.insert(format!(
            "posture.height_slew {}",
            side(posture.height_slew.is_some())
        ));
    }
    out.insert(format!("body {}", side(file.body.is_some())));
    out.insert(format!(
        "foothold.candidates {}",
        side(file.foothold.candidates != 0)
    ));
    for leg in &file.legs {
        out.insert(format!("leg.hip_offset {}", side(leg.hip_offset.is_some())));
        out.insert(format!("leg.max_reach {}", side(leg.max_reach.is_some())));
    }
    out
}

#[test]
fn the_corpus_covers_every_v1_shape() {
    let mut seen = BTreeSet::new();
    let mut legs = BTreeSet::new();
    for (name, text) in corpus() {
        let asset = load(&name, &text);
        seen.extend(facts(&asset.file));
        legs.insert(asset.file.legs.len());
    }
    let mut want: BTreeSet<String> = [
        "Tripod",
        "Ripple",
        "Wave",
        "QuadrupedWalk",
        "Trot",
        "Pace",
        "Bound",
        "BipedWalk",
        "Explicit",
    ]
    .iter()
    .map(|v| format!("pattern {v}"))
    .collect();
    for optional in [
        "switch_speeds",
        "hysteresis",
        "max_reach",
        "ground_band",
        "posture",
        "posture.height_slew",
        "body",
        "foothold.candidates",
        "leg.hip_offset",
        "leg.max_reach",
    ] {
        want.insert(format!("{optional} some"));
        want.insert(format!("{optional} none"));
    }
    let missing: Vec<&String> = want.difference(&seen).collect();
    assert!(
        missing.is_empty(),
        "no gait corpus file exercises {missing:?} (add a `v1-<what>.ron` to gait/index.ron and FROZEN.txt)"
    );
    // Two, four and six legs: every preset family's leg count.
    for count in [2, 4, 6] {
        assert!(legs.contains(&count), "no corpus file has {count} legs: {legs:?}");
    }
}

/// `text` with `field: 1` spliced in right after the first `anchor`.
fn inject(text: &str, anchor: &str, field: &str) -> String {
    let at = text
        .find(anchor)
        .unwrap_or_else(|| panic!("{anchor:?} is not in the fixture"))
        + anchor.len();
    let mut out = String::with_capacity(text.len() + field.len() + 8);
    out.push_str(&text[..at]);
    out.push_str(field);
    out.push_str(": 1, ");
    out.push_str(&text[at..]);
    out
}

#[test]
fn every_nested_block_refuses_an_unknown_field() {
    let text = corpus_file("v1-hexapod-switching.ron");
    let _ = load("v1-hexapod-switching.ron", &text);
    // (where the field goes, the unknown field's name)
    let cases = [
        ("format_version: 1,", "extra_root"),
        ("speeds: (", "extra_speeds"),
        ("settle: (", "extra_settle"),
        ("turn: (", "extra_turn"),
        ("foothold: (", "extra_foothold"),
        ("posture: Some((", "extra_posture"),
        ("legs: [\n        (", "extra_leg"),
        ("Explicit(", "extra_explicit"),
    ];
    for (anchor, field) in cases {
        let dirty = inject(&text, anchor, field);
        match GaitAsset::from_ron(&dirty) {
            Ok(_) => panic!("{field}: an unknown field inside {anchor:?} was accepted"),
            Err(error) => {
                assert_eq!(error.code(), "gait_parse", "{field}: {error}");
                assert!(error.to_string().contains(field), "{field}: {error}");
            }
        }
    }
}

#[test]
fn every_corpus_file_refuses_other_versions_and_migrates_idempotently() {
    assert!(gait_migration_sources().is_empty(), "v1 is the first version");
    for (name, text) in corpus() {
        // The migration of a v1 file is the identity: twice equals once equals the input.
        let once = migrate_gait_ron(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
        let twice = migrate_gait_ron(&once).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(once, text, "{name}: v1 must pass through unchanged");
        assert_eq!(twice, once, "{name}: migrating twice differs from once");
        // A canonical save loads to the same document and settings.
        let asset = load(&name, &text);
        let saved = asset
            .to_ron()
            .unwrap_or_else(|error| panic!("{name}: save: {error}"));
        let again = load(&name, &saved);
        assert_eq!(again.file, asset.file, "{name}");
        assert_eq!(
            format!("{:?}", again.params),
            format!("{:?}", asset.params),
            "{name}"
        );
        // This build refuses every other version before any other field is read.
        for version in [0_u32, GAIT_FORMAT_VERSION + 1, u32::MAX] {
            let other = text.replacen("format_version: 1,", &format!("format_version: {version},"), 1);
            assert_ne!(other, text, "{name}: no `format_version: 1,` line");
            for (what, result) in [
                ("load", GaitAsset::from_ron(&other).map(|_| ())),
                ("migrate", migrate_gait_ron(&other).map(|_| ())),
            ] {
                let error = result
                    .err()
                    .unwrap_or_else(|| panic!("{name}: {what} accepted format_version {version}"));
                assert_eq!(error.code(), "gait_version", "{name} {what} v{version}: {error}");
            }
        }
    }
}
