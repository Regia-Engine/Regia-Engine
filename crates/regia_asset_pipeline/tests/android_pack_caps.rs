// SPDX-License-Identifier: MPL-2.0
//! Android asset-pack size caps and the optional-reference warning
//! (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.2 and § 2.6, ADR-7236).

use regia_asset::AssetId;
use regia_asset::content::{ChunkEntry, ContentManifest, Delivery};
use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_asset_pipeline::chunks::{
    AndroidPackLimits, ChunkError, ChunkRules, OPTIONAL_REFERENCE_TAG, Severity, assign,
    check_android_pack_limits, check_references, check_references_with, optional_references,
};
use regia_core::id::Guid;

const MIB: u64 = 1 << 20;

fn entry(name: &str, delivery: Delivery, size: u64) -> ChunkEntry {
    ChunkEntry {
        name: name.into(),
        file: format!("{name}.fpak"),
        size,
        hash: String::new(),
        delivery,
        requires: Vec::new(),
        overrides: false,
        mount_order: 0,
        assets: 0,
        blocks: Vec::new(),
    }
}

fn manifest(chunks: &[(&str, Delivery, u64)]) -> ContentManifest {
    let mut m = ContentManifest::new("b", "android");
    for (name, delivery, size) in chunks {
        m.chunks.push(entry(name, *delivery, *size));
    }
    m
}

fn limits(max_pack_bytes: u64, max_total_bytes: u64) -> AndroidPackLimits {
    AndroidPackLimits {
        max_packs: 50,
        max_pack_bytes,
        max_total_bytes,
    }
}

/// One pack over the per-pack cap fails with `pack_limit` and names only that pack; a pack exactly
/// at the cap passes.
#[test]
fn a_pack_over_the_per_pack_cap_fails_with_pack_limit_and_names_it() {
    let at_cap = manifest(&[
        ("base", Delivery::Install, 900 * MIB),
        ("arena", Delivery::OnDemand, 100 * MIB),
        ("cave", Delivery::FastFollow, 100 * MIB),
    ]);
    assert!(check_android_pack_limits(&at_cap, &limits(100 * MIB, 4096 * MIB)).is_ok());

    let over = manifest(&[
        ("base", Delivery::Install, 900 * MIB),
        ("arena", Delivery::OnDemand, 100 * MIB + 1),
        ("cave", Delivery::FastFollow, 100 * MIB),
    ]);
    let err = check_android_pack_limits(&over, &limits(100 * MIB, 4096 * MIB)).expect_err("per-pack cap");
    assert_eq!(err.reason(), "pack_limit");
    match &err {
        ChunkError::PackSize {
            scope,
            chunks,
            bytes,
            limit,
        } => {
            assert_eq!(*scope, "pack");
            assert_eq!(chunks, &["arena".to_owned()], "only the offending pack");
            assert_eq!((*bytes, *limit), (100 * MIB + 1, 100 * MIB));
        }
        other => panic!("expected PackSize, got {other:?}"),
    }
    assert!(err.to_string().contains("arena"), "{err}");
    // The install chunk ships inside the base bundle, not as a pack: no per-pack cap on it.
    let big_base = manifest(&[("base", Delivery::Install, 4000 * MIB)]);
    assert!(check_android_pack_limits(&big_base, &limits(100 * MIB, 4096 * MIB)).is_ok());
}

/// Everything the store delivers (install, fast-follow and on-demand chunks) counts toward the total
/// cap; `external` chunks come from the game's own host and do not.
#[test]
fn the_total_cap_counts_store_chunks_and_skips_external_ones() {
    let at_total = manifest(&[
        ("base", Delivery::Install, 600 * MIB),
        ("arena", Delivery::OnDemand, 200 * MIB),
        ("cave", Delivery::FastFollow, 200 * MIB),
        ("dlc", Delivery::External, 5000 * MIB),
    ]);
    assert!(check_android_pack_limits(&at_total, &limits(512 * MIB, 1000 * MIB)).is_ok());

    let over = manifest(&[
        ("base", Delivery::Install, 600 * MIB),
        ("arena", Delivery::OnDemand, 200 * MIB),
        ("cave", Delivery::FastFollow, 200 * MIB + 1),
    ]);
    let err = check_android_pack_limits(&over, &limits(512 * MIB, 1000 * MIB)).expect_err("total cap");
    assert_eq!(err.reason(), "pack_limit");
    match &err {
        ChunkError::PackSize {
            scope,
            chunks,
            bytes,
            limit,
        } => {
            assert_eq!(*scope, "total");
            assert_eq!(chunks, &["arena".to_owned(), "base".into(), "cave".into()]);
            assert_eq!((*bytes, *limit), (1000 * MIB + 1, 1000 * MIB));
        }
        other => panic!("expected PackSize, got {other:?}"),
    }
}

/// The caps come from `Regia.toml` `[content]` (bytes or a size with a binary unit); without the keys
/// the defaults apply; a bad value is `bad_params`; the checks still warn to verify the store's limits.
#[test]
fn caps_are_read_from_regia_toml_with_defaults() {
    let defaults = ChunkRules::from_manifest_toml("").expect("empty");
    let d = defaults.android_limits();
    assert_eq!(d, AndroidPackLimits::default());
    assert_eq!(d.max_packs, 50);
    assert!(d.max_pack_bytes > 0 && d.max_total_bytes >= d.max_pack_bytes);

    let set = ChunkRules::from_manifest_toml(
        "[content]\nandroid_pack_limit = 12\nandroid_pack_max_size = \"256M\"\nandroid_total_max_size = 3221225472\n",
    )
    .expect("caps");
    assert_eq!(
        set.android_limits(),
        AndroidPackLimits {
            max_packs: 12,
            max_pack_bytes: 256 * MIB,
            max_total_bytes: 3 << 30,
        }
    );
    for bad in [
        "[content]\nandroid_pack_max_size = \"lots\"\n",
        "[content]\nandroid_total_max_size = -1\n",
        "[content]\nandroid_pack_max_size = 0\n",
    ] {
        let err = ChunkRules::from_manifest_toml(bad).expect_err(bad);
        assert_eq!(err.reason(), "bad_params", "{bad}");
    }

    let ok = manifest(&[
        ("base", Delivery::Install, MIB),
        ("arena", Delivery::OnDemand, MIB),
    ]);
    let warnings = check_android_pack_limits(&ok, &d).expect("within limits");
    assert!(
        warnings.iter().any(|w| w.contains("verify")),
        "the store's limits change: {warnings:?}"
    );
}

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

fn rec(n: u128, path: &str, deps: &[u128], tags: &[(&str, &str)]) -> RegistryEntry {
    RegistryEntry {
        id: id(n),
        path: format!("assets/{path}"),
        type_path: "regia_asset::CpuMesh".into(),
        dependencies: deps.iter().map(|d| id(*d)).collect(),
        tags: tags
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        size: 0,
    }
}

const RULES: &str = r#"
[cook.chunks.arena]
include = ["levels/arena/**"]
delivery = "on_demand"
"#;

/// A base asset referencing an asset of another chunk through a reference marked optional is a
/// warning (`chunk_base_reference`, severity warning); the same reference without the mark stays a
/// cook error.
#[test]
fn an_optional_base_reference_is_a_warning_and_a_plain_one_stays_an_error() {
    let reg = AssetRegistry::from_entries([
        rec(
            1,
            "ui/menu.level.ron",
            &[3, 4],
            &[(OPTIONAL_REFERENCE_TAG, "levels/arena/trophy.glb")],
        ),
        rec(3, "levels/arena/trophy.glb", &[], &[]),
        rec(4, "levels/arena/boss.glb", &[], &[]),
    ]);
    let rules = ChunkRules::from_manifest_toml(RULES).expect("rules");
    let a = assign(&reg, &rules, &[]).expect("assign");
    let optional = optional_references(&reg);
    assert_eq!(optional.len(), 1, "{optional:?}");
    assert!(optional.contains(&(id(1), id(3))));

    let problems = check_references_with(&reg, &a, &rules, &optional);
    let rows: Vec<(AssetId, &str, Severity)> = problems
        .iter()
        .map(|p| (p.target, p.reason, p.severity))
        .collect();
    assert_eq!(
        rows,
        [
            (id(3), "chunk_base_reference", Severity::Warning),
            (id(4), "chunk_base_reference", Severity::Error),
        ]
    );
    assert!(
        problems[0].message.contains("optional"),
        "{}",
        problems[0].message
    );

    // Without the optional set (the old entry point) both stay errors.
    let plain = check_references(&reg, &a, &rules);
    assert!(plain.iter().all(|p| p.severity == Severity::Error), "{plain:?}");
}

/// The mark lists targets by path glob (comma separated); a glob that matches no dependency marks
/// nothing, and a mark never relaxes chunk-to-unlisted-chunk references (only base references are
/// warnings, per spec 40 § 2.2).
#[test]
fn optional_marks_match_dependency_paths_by_glob_and_only_relax_base_references() {
    let reg = AssetRegistry::from_entries([
        rec(
            1,
            "ui/menu.level.ron",
            &[3, 4],
            &[(OPTIONAL_REFERENCE_TAG, "levels/arena/**, nothing/*")],
        ),
        rec(
            2,
            "levels/forest/f.level.ron",
            &[3],
            &[(OPTIONAL_REFERENCE_TAG, "levels/arena/**")],
        ),
        rec(3, "levels/arena/trophy.glb", &[], &[]),
        rec(4, "levels/arena/boss.glb", &[], &[]),
    ]);
    let rules = ChunkRules::from_manifest_toml(&format!(
        "{RULES}\n[cook.chunks.forest]\ninclude = [\"levels/forest/**\"]\n"
    ))
    .expect("rules");
    let a = assign(&reg, &rules, &[]).expect("assign");
    let optional = optional_references(&reg);
    assert!(optional.contains(&(id(1), id(3))) && optional.contains(&(id(1), id(4))));
    let problems = check_references_with(&reg, &a, &rules, &optional);
    let rows: Vec<(&str, &str, Severity)> = problems
        .iter()
        .map(|p| (p.chunk.as_str(), p.reason, p.severity))
        .collect();
    assert_eq!(
        rows,
        [
            ("base", "chunk_base_reference", Severity::Warning),
            ("base", "chunk_base_reference", Severity::Warning),
            ("forest", "chunk_unlisted_reference", Severity::Error),
        ]
    );
}
