// SPDX-License-Identifier: MPL-2.0
//! Cooked foot-contact curves (ADR-8308, `docs/specs/17-animation.md` § 6): a glTF source whose
//! `.meta` sets `contact_curves` cooks a `contact.<foot>` curve per foot onto every skinned clip,
//! and the curve reads, at every frame, the motion-matching contact label of that foot computed
//! from the source clip. Without the setting the clip has no contact curves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use regia_animation::AnimationClip;
use regia_animation::foot_ik::contact::{contact_curve_name, contact_frame_times};
use regia_animation::motion_matching::ContactSettings;
use regia_animation::motion_matching::contacts::label_contacts;
use regia_asset::{AssetManifest, cooked_cache_dir};
use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
use regia_core::math::Vec3;
use serde_json::json;

const GLB_ID: &str = "2d7e3f40-5162-4c73-9d84-9ea5fb601137";
const KEYS: usize = 31;
/// Walk speed (the root joint carries the travel), m/s.
const SPEED: f32 = 1.2;
/// The soles are 5 cm above the rig origin.
const SOLE: f32 = 0.05;

fn le_f32(values: impl IntoIterator<Item = f32>) -> Vec<u8> {
    values.into_iter().flat_map(f32::to_le_bytes).collect()
}

fn pack_glb(json: &serde_json::Value, bin: &[u8]) -> Vec<u8> {
    let mut json = json.to_string().into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut glb = b"glTF".to_vec();
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(bin);
    glb
}

/// Root-relative position of a foot at phase `phase` of a one-second cycle: planted (sliding back
/// against the root's travel, so still in the world) for the first half, then a swing.
fn foot_local(phase: f32, side: f32) -> [f32; 3] {
    let p = phase.rem_euclid(1.0);
    if p < 0.5 {
        [0.1 * side, SOLE, 0.3 - SPEED * p]
    } else {
        let s = (p - 0.5) / 0.5;
        [
            0.1 * side,
            SOLE + 0.12 * (std::f32::consts::PI * s).sin(),
            -0.3 + 0.6 * s,
        ]
    }
}

/// A three-joint walker (`Root`, `FootL`, `FootR`): the root travels at `SPEED`, the feet take
/// turns planting, their soles 5 cm above the origin.
fn walker_glb() -> Vec<u8> {
    let times: Vec<f32> = (0..KEYS).map(|k| k as f32 / 30.0).collect();
    let root: Vec<f32> = times.iter().flat_map(|&t| [0.0, 0.0, SPEED * t]).collect();
    let foot = |side: f32, offset: f32| -> Vec<f32> {
        times.iter().flat_map(|&t| foot_local(t + offset, side)).collect()
    };
    let mut bin = le_f32([0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    for i in 0u32..3 {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    bin.extend(le_f32(times.iter().copied()));
    bin.extend(le_f32(root));
    bin.extend(le_f32(foot(-1.0, 0.0)));
    bin.extend(le_f32(foot(1.0, 0.5)));
    let t_len = KEYS * 4;
    let v_len = KEYS * 12;
    let doc = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, 3]}],
        "nodes": [
            {"name": "Root", "children": [1, 2]},
            {"name": "FootL", "translation": [-0.1, SOLE, 0.3]},
            {"name": "FootR", "translation": [0.1, SOLE, -0.3]},
            {"name": "Tri", "mesh": 0}
        ],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "skins": [{"joints": [0, 1, 2]}],
        "animations": [{
            "name": "walk",
            "samplers": [
                {"input": 2, "output": 3},
                {"input": 2, "output": 4},
                {"input": 2, "output": 5}
            ],
            "channels": [
                {"sampler": 0, "target": {"node": 0, "path": "translation"}},
                {"sampler": 1, "target": {"node": 1, "path": "translation"}},
                {"sampler": 2, "target": {"node": 2, "path": "translation"}}
            ]
        }],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
             "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]},
            {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"},
            {"bufferView": 2, "componentType": 5126, "count": KEYS, "type": "SCALAR",
             "min": [0.0], "max": [times[KEYS - 1]]},
            {"bufferView": 3, "componentType": 5126, "count": KEYS, "type": "VEC3"},
            {"bufferView": 4, "componentType": 5126, "count": KEYS, "type": "VEC3"},
            {"bufferView": 5, "componentType": 5126, "count": KEYS, "type": "VEC3"}
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 12},
            {"buffer": 0, "byteOffset": 48, "byteLength": t_len},
            {"buffer": 0, "byteOffset": 48 + t_len, "byteLength": v_len},
            {"buffer": 0, "byteOffset": 48 + t_len + v_len, "byteLength": v_len},
            {"buffer": 0, "byteOffset": 48 + t_len + 2 * v_len, "byteLength": v_len}
        ],
        "buffers": [{"byteLength": bin.len()}]
    });
    pack_glb(&doc, &bin)
}

fn project(label: &str) -> (PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(label);
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    (root, assets)
}

fn write_meta(source: &Path, settings: Option<&str>) {
    let settings = settings.map_or_else(String::new, |s| format!("    settings: {s:?},\n"));
    let text =
        format!("(\n    format_version: 2,\n    id: \"{GLB_ID}\",\n    importer: \"gltf\",\n{settings})\n");
    std::fs::write(regia_asset::meta_path_for(source), text).expect("meta");
}

/// The cooked `walk` clip of `assets/walker.glb` after a cook of `root`.
fn cook_walk(root: &Path) -> AnimationClip {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = cooked_cache_dir(root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    let id = manifest
        .id_for_path("assets/walker.glb#clip/walk")
        .expect("the walk clip is cooked");
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked clip");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("cooked file");
    AnimationClip::from_cooked_payload(payload).expect("decodes")
}

/// The contact labels of each foot of the source clip, computed here from first principles:
/// object-space foot positions by forward kinematics at the clip's frames, heights over each foot's
/// lowest point, speeds by central differences around the loop, then `label_contacts`.
fn expected_labels(glb: &[u8]) -> Vec<(String, Vec<bool>)> {
    let import = import_gltf_animation(glb, &GltfAnimSettings::default()).expect("import");
    let skin = &import.skins[0];
    let clip = skin.clips.iter().find(|c| c.name == "walk").expect("walk");
    let skeleton = &skin.skeleton;
    let times = contact_frame_times(clip);
    let n = times.len();
    let lap = n - 1;
    let dt = times[1] - times[0];
    ["FootL", "FootR"]
        .iter()
        .map(|name| {
            let joint = skeleton.joint_index(name).expect("foot joint") as usize;
            let positions: Vec<Vec3> = times
                .iter()
                .map(|&t| {
                    let mut pose = skeleton.bind_pose_pose();
                    clip.sample_pose(t, &mut pose);
                    skeleton.globals(&pose, None)[joint].w_axis.truncate()
                })
                .collect();
            let lowest = positions.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
            let heights: Vec<f32> = positions.iter().map(|p| p.y - lowest).collect();
            let speeds: Vec<f32> = (0..n)
                .map(|i| {
                    let before = if i == 0 { lap - 1 } else { i - 1 };
                    let after = if i >= lap { 1 } else { i + 1 };
                    ((positions[after] - positions[before]) / (2.0 * dt)).length()
                })
                .collect();
            (
                (*name).to_string(),
                label_contacts(&heights, &speeds, true, &ContactSettings::default()),
            )
        })
        .collect()
}

#[test]
fn cooked_walk_has_contact_curves_matching_its_labels() {
    let glb = walker_glb();
    let expected = expected_labels(&glb);
    for (name, labels) in &expected {
        let planted = labels.iter().filter(|&&l| l).count();
        let touchdowns = labels.windows(2).filter(|w| !w[0] && w[1]).count();
        assert!(
            (KEYS / 3..KEYS).contains(&planted) && touchdowns >= 1,
            "{name}: the walker plants and lifts ({labels:?})"
        );
    }

    let (root, assets) = project("contact-curves-cook");
    let source = assets.join("walker.glb");
    std::fs::write(&source, &glb).expect("glb");
    write_meta(
        &source,
        Some("(contact_curves: Some((feet: [\"FootL\", \"FootR\"])))"),
    );
    let clip = cook_walk(&root);
    let times = contact_frame_times(&clip);
    for (name, labels) in &expected {
        let curve_name = contact_curve_name(name);
        let curve = clip
            .curves
            .iter()
            .find(|c| c.name == curve_name)
            .unwrap_or_else(|| panic!("{curve_name} is cooked onto the clip"));
        assert_eq!(times.len(), labels.len());
        for (frame, (&t, &label)) in times.iter().zip(labels).enumerate() {
            assert_eq!(
                curve.curve.sample(t),
                if label { 1.0 } else { 0.0 },
                "{curve_name} frame {frame}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);

    // Without the setting the cooked clip carries no contact curves.
    let (root, assets) = project("contact-curves-off");
    let source = assets.join("walker.glb");
    std::fs::write(&source, &glb).expect("glb");
    write_meta(&source, None);
    let clip = cook_walk(&root);
    assert!(
        clip.curves.iter().all(|c| !c.name.starts_with("contact.")),
        "{:?}",
        clip.curves.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&root);
}
