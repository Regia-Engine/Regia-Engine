// SPDX-License-Identifier: MPL-2.0
//! Spec 55 test 15: the checked-in v1 `.cloth.bin` and `.soft.bin` fixtures load, equal a fresh
//! cook byte for byte (so a layout change without a version bump fails here), and a newer
//! payload version answers `cloth_unsupported_version`.
//!
//! Regenerate only with a format bump: `REGIA_WRITE_CLOTH_FIXTURES=1 cargo test -p regia_physics
//! --test cloth_formats`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_asset::AssetId;
use regia_asset::{COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version};
use regia_core::id::TypePathHash;
use regia_core::math::Vec3;
use regia_physics::cloth::asset::{
    COOKED_CLOTH_TYPE_PATH, COOKED_SOFT_TYPE_PATH, ClothSource, SimMeshInput, SoftSource,
    cooked_cloth_file_version, cooked_soft_file_version,
};
use regia_physics::cloth::{ClothAsset, SoftAsset};

const CLOTH_V1: &[u8] = include_bytes!("fixtures/cloth/flag-v1.cloth.bin");
const SOFT_V1: &[u8] = include_bytes!("fixtures/cloth/ball-v1.soft.bin");

fn wrap(type_path: &str, name: &str, payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(type_path),
        asset_id: AssetId::sub_asset(AssetId::NIL, name),
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("cooked file")
}

fn cloth() -> ClothAsset {
    let mut mesh = SimMeshInput::default();
    for r in 0..4u32 {
        for c in 0..4u32 {
            mesh.positions
                .push(Vec3::new(c as f32 * 0.1, -(r as f32) * 0.1, 0.0));
            mesh.uvs.push([c as f32 / 3.0, r as f32 / 3.0]);
            mesh.joints.push([0, 1, 0, 0]);
            mesh.weights.push([0.75, 0.25, 0.0, 0.0]);
        }
    }
    for r in 0..3u32 {
        for c in 0..3u32 {
            let i = r * 4 + c;
            mesh.triangles.push([i, i + 4, i + 1]);
            mesh.triangles.push([i + 1, i + 4, i + 5]);
        }
    }
    let source = ClothSource::from_ron_str(
        r#"(format_version: 1, name: "flag", skeleton: Some("hero.glb"), sim_mesh: (source: "flag.glb", node: "Flag"),
            params: (substeps: 6, pull_compliance: 0.002, tear: Some((strain: 2.0, max_per_step: 4))),
            maps: {
                "reach": Sparse(default: 0.25, entries: [(0, 0.0), (1, 0.0), (2, 0.0), (3, 0.0)]),
                "standoff": Sparse(default: 0.01, entries: []),
                "pose_pull": Sparse(default: 0.5, entries: [(15, 0.0)]),
            },
            proxies: ["spine_cap"])"#,
    )
    .expect("parses");
    ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks")
}

fn soft() -> SoftAsset {
    let surface = SimMeshInput {
        positions: vec![Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z],
        triangles: vec![
            [0, 2, 4],
            [2, 1, 4],
            [1, 3, 4],
            [3, 0, 4],
            [2, 0, 5],
            [1, 2, 5],
            [3, 1, 5],
            [0, 3, 5],
        ],
        joints: vec![[0, 1, 0, 0]; 6],
        weights: vec![[0.75, 0.25, 0.0, 0.0]; 6],
        ..SimMeshInput::default()
    };
    let source = SoftSource::from_ron_str(
        r#"(format_version: 1, name: "ball", model: Pressure(pressure: 1.2, volume_compliance: 0.0),
            surface: (source: "ball.glb", node: "Ball"), params: (substeps: 6))"#,
    )
    .expect("parses");
    SoftAsset::cook(&source, &surface, None).expect("cooks")
}

fn cloth_file() -> Vec<u8> {
    wrap(
        COOKED_CLOTH_TYPE_PATH,
        "cloth/flag",
        &cloth().to_bytes().expect("payload"),
    )
}

fn soft_file() -> Vec<u8> {
    wrap(
        COOKED_SOFT_TYPE_PATH,
        "soft/ball",
        &soft().to_bytes().expect("payload"),
    )
}

/// The byte offset of the payload's version field in a cooked file (`magic` + `version`).
fn payload_version_at(file: &[u8]) -> usize {
    let (_, payload) = regia_asset::decode_cooked_file(file).expect("wrapper");
    file.len() - payload.len() + 4
}

#[test]
fn fixtures_are_current() {
    if regia_core::brand::env_var("REGIA_WRITE_CLOTH_FIXTURES").is_ok() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cloth");
        std::fs::write(dir.join("flag-v1.cloth.bin"), cloth_file()).expect("write");
        std::fs::write(dir.join("ball-v1.soft.bin"), soft_file()).expect("write");
        return;
    }
    assert_eq!(
        CLOTH_V1,
        cloth_file().as_slice(),
        "the cloth payload layout changed: bump the version"
    );
    assert_eq!(
        SOFT_V1,
        soft_file().as_slice(),
        "the soft payload layout changed: bump the version"
    );
}

#[test]
fn v1_fixtures_load_with_their_content() {
    let asset = ClothAsset::from_cooked_file(CLOTH_V1).expect("loads");
    assert_eq!(asset, cloth());
    assert_eq!(asset.name, "flag");
    assert_eq!(asset.proxies, vec!["spine_cap".to_string()]);
    assert_eq!(asset.lod0().rest_positions.len(), 16);
    assert!(asset.is_skinned());
    let solver = asset
        .build_solver(0, 1.0 / 60.0, regia_core::math::Affine3A::IDENTITY)
        .expect("solver");
    assert_eq!(solver.particles().len(), 16);
    assert_eq!(cooked_cloth_file_version(CLOTH_V1), Ok(Some(1)));
    assert_eq!(
        cooked_soft_file_version(CLOTH_V1),
        Ok(None),
        "another cooked type"
    );

    let ball = SoftAsset::from_cooked_file(SOFT_V1).expect("loads");
    assert_eq!(ball, soft());
    assert!(ball.is_skinned());
    assert_eq!(cooked_soft_file_version(SOFT_V1), Ok(Some(1)));
}

#[test]
fn newer_payloads_and_wrong_types_are_refused() {
    let mut newer = CLOTH_V1.to_vec();
    let at = payload_version_at(&newer);
    newer[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        ClothAsset::from_cooked_file(&newer).expect_err("newer").code(),
        "cloth_unsupported_version"
    );
    let mut newer = SOFT_V1.to_vec();
    let at = payload_version_at(&newer);
    newer[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        SoftAsset::from_cooked_file(&newer).expect_err("newer").code(),
        "cloth_unsupported_version"
    );
    assert_eq!(
        ClothAsset::from_cooked_file(SOFT_V1).expect_err("soft").code(),
        "cloth_asset_invalid"
    );
    assert_eq!(
        SoftAsset::from_cooked_file(CLOTH_V1).expect_err("cloth").code(),
        "cloth_asset_invalid"
    );
    assert_eq!(
        ClothAsset::from_cooked_file(b"nope").expect_err("garbage").code(),
        "cloth_asset_invalid"
    );
    // Truncation anywhere in the payload is an error, not a panic.
    let start = payload_version_at(CLOTH_V1) - 4;
    for cut in (start..CLOTH_V1.len()).step_by(7) {
        assert!(
            ClothAsset::from_cooked_file(&CLOTH_V1[..cut]).is_err(),
            "cut at {cut}"
        );
    }
}
