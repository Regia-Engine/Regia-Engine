// SPDX-License-Identifier: MPL-2.0
//! `.meta` v2 (ADR-7819, `docs/specs/12-assets.md` § 4a, `docs/formats/meta.md`): the per-asset
//! `lossless_clips` import setting. v2 is the first version whose importer settings carry the switch;
//! a v1 sidecar migrates with it off (v1 importers ignored or refused the key), every other byte of
//! its settings kept, and a newer version is refused.

use regia_asset::{LOSSLESS_CLIPS_KEY, META_FORMAT_VERSION, MetaError, MetaSidecar, meta_migration_sources};

const ID: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

fn meta_text(version: u32, settings: Option<&str>) -> String {
    let settings = settings.map_or_else(String::new, |s| format!("    settings: {s:?},\n"));
    format!("(\n    format_version: {version},\n    id: \"{ID}\",\n    importer: \"gltf\",\n{settings})\n")
}

#[test]
fn the_meta_format_is_v2_with_a_v1_migrator() {
    assert_eq!(META_FORMAT_VERSION, 2);
    assert_eq!(meta_migration_sources(), vec![1]);
    assert_eq!(LOSSLESS_CLIPS_KEY, "lossless_clips");
}

/// Acceptance: an older settings file migrates with the switch off. A v1 file that already named
/// the key (a v1 glTF importer ignored it with a warning; a v1 BVH importer refused it) loads at v2
/// with the key gone and every other setting kept in order.
#[test]
fn a_v1_meta_migrates_with_the_lossless_switch_off() {
    let v1 = meta_text(1, Some("(meshlets: true, lossless_clips: true, scale: 0.01)"));
    let meta = MetaSidecar::from_ron(v1.as_bytes()).expect("a v1 meta loads");
    assert_eq!(meta.format_version, 2);
    assert!(!meta.lossless_clips(), "the switch is off after migration");
    assert_eq!(meta.settings.as_deref(), Some("(meshlets: true, scale: 0.01)"));

    // A v1 file whose only setting was the key migrates to no settings at all.
    let only = meta_text(1, Some("(lossless_clips: true)"));
    let meta = MetaSidecar::from_ron(only.as_bytes()).expect("loads");
    assert!(!meta.lossless_clips());
    assert_eq!(meta.settings, None);

    // A v1 file without the key: only the version moves.
    let plain = meta_text(1, Some("(meshlets: true)"));
    let meta = MetaSidecar::from_ron(plain.as_bytes()).expect("loads");
    assert_eq!(meta.format_version, 2);
    assert!(!meta.lossless_clips());
    assert_eq!(meta.settings.as_deref(), Some("(meshlets: true)"));
    let bare = MetaSidecar::from_ron(meta_text(1, None).as_bytes()).expect("loads");
    assert_eq!(bare.settings, None);
    assert!(!bare.lossless_clips());
}

#[test]
fn a_v2_meta_reads_the_switch_and_round_trips() {
    let v2 = meta_text(2, Some("(lossless_clips: true, meshlets: true)"));
    let meta = MetaSidecar::from_ron(v2.as_bytes()).expect("a v2 meta loads");
    assert!(meta.lossless_clips());
    let text = meta.to_ron_string().expect("serialize");
    let back = MetaSidecar::from_ron(text.as_bytes()).expect("reads back");
    assert_eq!(back, meta);
    assert!(back.lossless_clips());

    let off = MetaSidecar::from_ron(meta_text(2, Some("(lossless_clips: false)")).as_bytes()).expect("loads");
    assert!(!off.lossless_clips());
    let unset = MetaSidecar::from_ron(meta_text(2, Some("(meshlets: true)")).as_bytes()).expect("loads");
    assert!(!unset.lossless_clips(), "absent means off");
}

#[test]
fn migrating_twice_changes_nothing() {
    let v1 = meta_text(1, Some("(lossless_clips: true, vertex_ao: true)"));
    let once = MetaSidecar::from_ron(v1.as_bytes()).expect("loads");
    let text = once.to_ron_string().expect("serialize");
    let twice = MetaSidecar::from_ron(text.as_bytes()).expect("loads again");
    assert_eq!(once, twice);
    assert_eq!(twice.to_ron_string().expect("serialize"), text);
}

#[test]
fn a_newer_meta_is_refused() {
    let v3 = meta_text(3, Some("(lossless_clips: true)"));
    match MetaSidecar::from_ron(v3.as_bytes()) {
        Err(MetaError::UnsupportedVersion {
            found: 3,
            supported: 2,
        }) => {}
        other => panic!("expected the v3 refusal, got {other:?}"),
    }
}

/// The switch is a boolean: anything else is a settings error the cook reports, never a silent
/// lossless (or lossy) cook.
#[test]
fn a_switch_that_is_not_a_boolean_is_an_error() {
    let bad = MetaSidecar::from_ron(meta_text(2, Some("(lossless_clips: yes)")).as_bytes()).expect("loads");
    assert!(regia_asset::clip_cook_lossless(bad.settings.as_deref()).is_err());
    assert_eq!(
        regia_asset::clip_cook_lossless(Some("(lossless_clips: true)")),
        Ok(true)
    );
    assert_eq!(
        regia_asset::clip_cook_lossless(Some("( lossless_clips : false )")),
        Ok(false)
    );
    assert_eq!(regia_asset::clip_cook_lossless(None), Ok(false));
    assert_eq!(regia_asset::clip_cook_lossless(Some("")), Ok(false));
}
