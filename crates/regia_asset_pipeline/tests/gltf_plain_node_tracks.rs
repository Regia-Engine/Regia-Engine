// SPDX-License-Identifier: MPL-2.0
//! Procedural glTF regression fixtures for plain-node animation (ADR-8157).

use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};

fn translation_glb() -> Vec<u8> {
    let mut json = br#"{"asset":{"version":"2.0"},"nodes":[{"name":"hinge"}],
"animations":[{"name":"move","channels":[{"sampler":0,"target":{"node":0,"path":"translation"}}],"samplers":[{"input":0,"output":1}]}],
"buffers":[{"byteLength":32}],"bufferViews":[{"buffer":0,"byteLength":8},{"buffer":0,"byteOffset":8,"byteLength":24}],
"accessors":[{"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]},{"bufferView":1,"componentType":5126,"count":2,"type":"VEC3"}]}"#.to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let bin: Vec<u8> = [0.0f32, 1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
    let mut glb = Vec::new();
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&((28 + json.len() + bin.len()) as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend(json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend(bin);
    glb
}

#[test]
fn plain_node_channel_animates() {
    let imported = import_gltf_animation(&translation_glb(), &GltfAnimSettings::default()).expect("import");
    assert!(imported.skins.is_empty());
    assert_eq!(
        imported.node_clips.len(),
        1,
        "plain-node TRS clip must survive import"
    );
    let clip = &imported.node_clips[0];
    let mut samples = Vec::new();
    clip.node_animation
        .as_ref()
        .expect("node tracks")
        .sample(0.5, &mut samples)
        .expect("sample");
    assert_eq!(samples[0].index, 0);
    assert!(
        samples[0]
            .world
            .w_axis
            .truncate()
            .abs_diff_eq(regia_core::math::Vec3::NEG_X, 1e-6)
    );
}

use regia_animation::clip::AnimationClip;
use regia_animation::compress::CompressionSettings;
use regia_asset_pipeline::Forward;
use regia_core::math::{Mat4, Quat, Vec3};
use serde_json::{Value, json};

struct Channel<'a> {
    node: usize,
    path: &'a str,
    interpolation: &'a str,
    times: &'a [f32],
    values: &'a [f32],
}

fn fixture(nodes: Value, channels: &[Channel<'_>], skins: Option<Value>) -> (Value, Vec<u8>) {
    let mut bin = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut targets = Vec::new();
    let mut samplers = Vec::new();
    for (i, channel) in channels.iter().enumerate() {
        let (components, output_type) = match channel.path {
            "rotation" => (4, "VEC4"),
            "weights" => (1, "SCALAR"),
            _ => (3, "VEC3"),
        };
        for (values, ty, count) in [
            (channel.times, "SCALAR", channel.times.len()),
            (channel.values, output_type, channel.values.len() / components),
        ] {
            views.push(json!({"buffer":0, "byteOffset":bin.len(), "byteLength":values.len()*4}));
            bin.extend(values.iter().flat_map(|v| v.to_le_bytes()));
            let mut accessor =
                json!({"bufferView":views.len()-1, "componentType":5126, "count":count, "type":ty});
            if ty == "SCALAR" {
                accessor["min"] = json!([values.iter().copied().fold(f32::INFINITY, f32::min)]);
                accessor["max"] = json!([values.iter().copied().fold(f32::NEG_INFINITY, f32::max)]);
            }
            accessors.push(accessor);
        }
        targets.push(json!({"sampler":i,"target":{"node":channel.node,"path":channel.path}}));
        samplers.push(json!({"input":i*2,"output":i*2+1,"interpolation":channel.interpolation}));
    }
    let mut doc = json!({"asset":{"version":"2.0"}, "nodes":nodes,
        "buffers":[{"byteLength":bin.len()}], "bufferViews":views, "accessors":accessors,
        "animations":[{"name":"motion","channels":targets,"samplers":samplers}]});
    if let Some(skins) = skins {
        doc["skins"] = skins;
    }
    (doc, bin)
}

fn pack(doc: &Value, bin: &[u8]) -> Vec<u8> {
    let mut json = serde_json::to_vec(doc).expect("json");
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut glb = b"glTF".to_vec();
    glb.extend(2u32.to_le_bytes());
    glb.extend(((28 + json.len() + bin.len()) as u32).to_le_bytes());
    glb.extend((json.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(json);
    glb.extend((bin.len() as u32).to_le_bytes());
    glb.extend(b"BIN\0");
    glb.extend(bin);
    glb
}

fn keep() -> GltfAnimSettings {
    GltfAnimSettings {
        forward: Forward::Keep,
        ..Default::default()
    }
}

fn clip(nodes: Value, channels: &[Channel<'_>]) -> AnimationClip {
    let (doc, bin) = fixture(nodes, channels, None);
    import_gltf_animation(&pack(&doc, &bin), &keep())
        .expect("import")
        .node_clips
        .remove(0)
}

fn world(clip: &AnimationClip, node: u32, time: f32) -> Mat4 {
    let mut out = Vec::new();
    clip.node_animation
        .as_ref()
        .expect("nodes")
        .sample(time, &mut out)
        .expect("sample");
    out.iter().find(|s| s.index == node).expect("node identity").world
}

#[test]
fn parent_order_duplicate_names_bind_defaults_and_three_channels() {
    let clip = clip(
        json!([
            {"name":"same","translation":[0,2,0],"scale":[2,3,4]},
            {"name":"same","children":[0],"translation":[10,0,0]}
        ]),
        &[
            Channel {
                node: 1,
                path: "translation",
                interpolation: "LINEAR",
                times: &[0.0, 2.0],
                values: &[10., 0., 0., 14., 0., 0.],
            },
            Channel {
                node: 0,
                path: "rotation",
                interpolation: "LINEAR",
                times: &[0., 2.],
                values: &[0., 0., 0., 1., 0., 0., 1., 0.],
            },
            Channel {
                node: 0,
                path: "scale",
                interpolation: "LINEAR",
                times: &[0., 2.],
                values: &[2., 3., 4., 4., 5., 6.],
            },
        ],
    );
    let nodes = &clip.node_animation.as_ref().expect("nodes").nodes;
    assert_eq!(nodes.iter().map(|n| n.node.index).collect::<Vec<_>>(), [1, 0]);
    assert_eq!(nodes[1].parent, Some(0));
    let expected = Mat4::from_scale_rotation_translation(
        Vec3::new(3., 4., 5.),
        Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
        Vec3::new(12., 2., 0.),
    );
    assert!(world(&clip, 0, 1.).abs_diff_eq(expected, 1e-5));
    assert_eq!(clip.duration, 2.);
}

#[test]
fn step_holds_until_the_exact_key_and_constant_tracks_keep_duration() {
    let clip = clip(
        json!([{}]),
        &[
            Channel {
                node: 0,
                path: "translation",
                interpolation: "STEP",
                times: &[0., 1., 2.],
                values: &[0., 0., 0., 2., 0., 0., 4., 0., 0.],
            },
            Channel {
                node: 0,
                path: "scale",
                interpolation: "LINEAR",
                times: &[0., 2.],
                values: &[1., 1., 1., 1., 1., 1.],
            },
        ],
    );
    assert_eq!(world(&clip, 0, 1.0f32.next_down()).w_axis.x, 0.);
    assert_eq!(world(&clip, 0, 1.).w_axis.x, 2.);
    assert_eq!(world(&clip, 0, 2.0f32.next_down()).w_axis.x, 2.);
    assert_eq!(world(&clip, 0, 2.).w_axis.x, 4.);
    let constant = clip.slice("hold", 1.1, 1.9);
    assert!((constant.duration - 0.8).abs() < 1e-6);
    assert_eq!(world(&constant, 0, 99.).w_axis.x, 2.);
}

#[test]
fn cubic_tangents_are_kept_and_slices_sample_the_original_curve() {
    // p0=0, p1=2, m0=2/sec, m1=0, dt=2: x(0.5s)=0.875.
    let clip = clip(
        json!([{}]),
        &[Channel {
            node: 0,
            path: "translation",
            interpolation: "CUBICSPLINE",
            times: &[0., 2.],
            values: &[
                0., 0., 0., 0., 0., 0., 2., 0., 0., 0., 0., 0., 2., 0., 0., 0., 0., 0.,
            ],
        }],
    );
    assert!((world(&clip, 0, 0.5).w_axis.x - 0.875).abs() < 1e-6);
    let slice = clip.slice("middle", 0.25, 1.25);
    for time in [0., 0.123, 0.25, 0.75, 1.] {
        assert!(world(&slice, 0, time).abs_diff_eq(world(&clip, 0, time + 0.25), 1e-6));
    }
    let nested = slice.slice("nested", 0.25, 0.75);
    assert!(world(&nested, 0, 0.1).abs_diff_eq(world(&clip, 0, 0.6), 1e-6));
}

#[test]
fn cubic_rotation_normalizes_the_hermite_value_not_its_tangents() {
    let clip = clip(
        json!([{}]),
        &[Channel {
            node: 0,
            path: "rotation",
            interpolation: "CUBICSPLINE",
            times: &[0., 2.],
            values: &[
                0., 0., 0., 0., 0., 0., 0., 1., 0., 0., 1., 0., 0., 0., 0., 0., 0., 0., 1., 0., 0., 0., 0.,
                0.,
            ],
        }],
    );
    // At t=.25: h00=.84375, h10*dt=.28125, h01=.15625.
    let q = Quat::from_xyzw(0., 0., 0.4375, 0.84375).normalize();
    assert!(world(&clip, 0, 0.5).abs_diff_eq(Mat4::from_quat(q), 1e-6));
}

#[test]
fn single_key_channels_and_singular_bind_are_supported() {
    let clip = clip(
        json!([{"scale":[0,1,1]}]),
        &[Channel {
            node: 0,
            path: "translation",
            interpolation: "CUBICSPLINE",
            times: &[2.],
            values: &[0., 0., 0., 3., 0., 0., 0., 0., 0.],
        }],
    );
    let mut samples = Vec::new();
    clip.node_animation
        .as_ref()
        .expect("nodes")
        .sample(0., &mut samples)
        .expect("sample");
    assert_eq!(samples[0].world.w_axis.x, 3.);
    assert_eq!(samples[0].delta, None);
    assert_eq!(clip.duration, 2.);
}

#[test]
fn scene_conversion_and_delta_apply_once_to_baked_vertices() {
    let (doc, bin) = fixture(
        json!([{"translation":[1,2,3],"children":[1]}, {"translation":[2,0,0]}]),
        &[Channel {
            node: 0,
            path: "translation",
            interpolation: "LINEAR",
            times: &[0., 1.],
            values: &[1., 2., 3., 3., 2., 3.],
        }],
        None,
    );
    let imported = import_gltf_animation(
        &pack(&doc, &bin),
        &GltfAnimSettings {
            scale: 2.,
            ..Default::default()
        },
    )
    .expect("import");
    let mut samples = Vec::new();
    imported.node_clips[0]
        .node_animation
        .as_ref()
        .expect("nodes")
        .sample(0.5, &mut samples)
        .expect("sample");
    let child = &samples[1];
    assert!(
        child
            .world
            .w_axis
            .truncate()
            .abs_diff_eq(Vec3::new(-8., 4., -6.), 1e-6)
    );
    let vertex = Vec3::new(0.3, 0.7, 0.2);
    let moved = child
        .delta
        .expect("invertible")
        .transform_point3(child.bind_world.transform_point3(vertex));
    assert!(moved.abs_diff_eq(child.world.transform_point3(vertex), 1e-5));
}

#[test]
fn mixed_skin_and_plain_tracks_keep_distinct_identity() {
    let (doc, bin) = fixture(
        json!([{"name":"same"},{"name":"same","children":[2]},{}]),
        &[
            Channel {
                node: 0,
                path: "translation",
                interpolation: "LINEAR",
                times: &[0., 1.],
                values: &[0., 0., 0., 2., 0., 0.],
            },
            Channel {
                node: 1,
                path: "translation",
                interpolation: "LINEAR",
                times: &[0., 1.],
                values: &[0., 0., 0., 0., 4., 0.],
            },
        ],
        Some(json!([{"joints":[0]}])),
    );
    let imported = import_gltf_animation(&pack(&doc, &bin), &keep()).expect("import");
    assert_eq!(imported.skins[0].clips[0].tracks.len(), 1);
    let mut out = Vec::new();
    imported.node_clips[0]
        .node_animation
        .as_ref()
        .expect("nodes")
        .sample(0.5, &mut out)
        .expect("sample");
    assert!(out[0].is_joint);
    assert!(!out[1].is_joint);
    assert_eq!(out[2].world.w_axis.y, 2.);
}

#[test]
fn both_cooked_writers_and_compressor_preserve_node_tracks() {
    let imported = import_gltf_animation(&translation_glb(), &keep()).expect("import");
    let clip = &imported.node_clips[0];
    let raw = clip.to_cooked_payload().expect("raw");
    let compressed = clip
        .to_compressed_cooked_payload(None, &CompressionSettings::default())
        .expect("compressed");
    for bytes in [raw, compressed] {
        let decoded = AnimationClip::from_cooked_payload(&bytes).expect("load");
        assert_eq!(decoded, *clip);
        assert_eq!(world(&decoded, 0, 0.25).w_axis.x, 0.5);
        assert_eq!(
            bytes,
            if bytes[8] == 2 {
                decoded.to_cooked_payload().expect("encode")
            } else {
                decoded
                    .to_compressed_cooked_payload(None, &CompressionSettings::default())
                    .expect("encode")
            }
        );
    }
    let packed = regia_animation::compress::compress_clip(clip, None, &CompressionSettings::default());
    assert_eq!(
        regia_animation::compress::decompress_clip(&packed).node_animation,
        clip.node_animation
    );
}

#[test]
fn longer_morph_channel_and_plain_trs_share_one_cooked_clip() {
    let (mut doc, mut bin) = fixture(
        json!([{"mesh":0}]),
        &[
            Channel {
                node: 0,
                path: "translation",
                interpolation: "LINEAR",
                times: &[0., 2.],
                values: &[0., 0., 0., 2., 0., 0.],
            },
            Channel {
                node: 0,
                path: "weights",
                interpolation: "LINEAR",
                times: &[0., 4.],
                values: &[0., 1.],
            },
        ],
        None,
    );
    let view = doc["bufferViews"].as_array().expect("views").len();
    let accessor = doc["accessors"].as_array().expect("accessors").len();
    doc["bufferViews"]
        .as_array_mut()
        .expect("views")
        .push(json!({"buffer":0,"byteOffset":bin.len(),"byteLength":36}));
    bin.extend([0u8; 36]);
    doc["buffers"][0]["byteLength"] = json!(bin.len());
    doc["accessors"].as_array_mut().expect("accessors").push(
        json!({"bufferView":view,"componentType":5126,"count":3,"type":"VEC3",
            "min":[0,0,0],"max":[0,0,0]}),
    );
    doc["meshes"] = json!([{"primitives":[{"attributes":{"POSITION":accessor},
        "targets":[{"POSITION":accessor}]}]}]);
    let imported = import_gltf_animation(&pack(&doc, &bin), &keep()).expect("mixed import");
    assert_eq!(imported.node_clips.len(), 1);
    let clip = &imported.node_clips[0];
    assert_eq!(clip.duration, 4.0);
    assert_eq!(clip.node_animation.as_ref().expect("nodes").end, 4.0);
    let payload = clip
        .to_compressed_cooked_payload(None, &Default::default())
        .expect("cook");
    let loaded = AnimationClip::from_cooked_payload(&payload).expect("load");
    let slice = loaded.slice("tail", 3.0, 4.0);
    assert_eq!(world(&slice, 0, 0.5).w_axis.x, 2.0);
    assert_eq!(slice.morph_tracks.len(), 1);
    assert_eq!(slice.morph_tracks[0].node.as_ref().expect("identity").index, 0);
    assert_eq!(slice.morph_tracks[0].weights.sample(0.5), 0.875);
}

#[test]
fn invalid_channel_times_and_values_are_rejected() {
    for (times, values) in [
        (vec![0., 0.], vec![0., 0., 0., 1., 0., 0.]),
        (vec![1., 0.], vec![0., 0., 0., 1., 0., 0.]),
        (vec![0., 1.], vec![0., 0., 0., f32::NAN, 0., 0.]),
    ] {
        let (doc, bin) = fixture(
            json!([{}]),
            &[Channel {
                node: 0,
                path: "translation",
                interpolation: "LINEAR",
                times: &times,
                values: &values,
            }],
            None,
        );
        assert!(import_gltf_animation(&pack(&doc, &bin), &keep()).is_err());
    }
    let (doc, bin) = fixture(
        json!([{}]),
        &[Channel {
            node: 0,
            path: "rotation",
            interpolation: "LINEAR",
            times: &[0., 1.],
            values: &[0.; 8],
        }],
        None,
    );
    assert!(import_gltf_animation(&pack(&doc, &bin), &keep()).is_err());
}

#[test]
fn external_buffers_load_the_same_node_clip() {
    let (mut doc, bin) = fixture(
        json!([{}]),
        &[Channel {
            node: 0,
            path: "translation",
            interpolation: "LINEAR",
            times: &[0., 1.],
            values: &[0., 0., 0., 2., 0., 0.],
        }],
        None,
    );
    let expected = import_gltf_animation(&pack(&doc, &bin), &keep()).expect("glb");
    let dir = regia_test_utils::unique_temp_dir("plain_node_buffers");
    std::fs::write(dir.join("data.tmp"), &bin).expect("write");
    std::fs::rename(dir.join("data.tmp"), dir.join("data.bin")).expect("publish");
    doc["buffers"][0]["uri"] = json!("data.bin");
    let imported = regia_asset_pipeline::gltf_anim::import_gltf_animation_with_base(
        &serde_json::to_vec(&doc).expect("json"),
        Some(&dir),
        &keep(),
    )
    .expect("external import");
    assert_eq!(imported.node_clips, expected.node_clips);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn project_cook_publishes_plain_node_subasset_for_both_storage_modes() {
    let (mut doc, mut bin) = fixture(
        json!([{"mesh":0}]),
        &[Channel {
            node: 0,
            path: "translation",
            interpolation: "LINEAR",
            times: &[0., 1.],
            values: &[0., 0., 0., 2., 0., 0.],
        }],
        None,
    );
    let view = doc["bufferViews"].as_array_mut().expect("views");
    let positions_view = view.len();
    view.push(json!({"buffer":0,"byteOffset":bin.len(),"byteLength":36}));
    bin.extend(
        [0.0f32, 0., 0., 1., 0., 0., 0., 1., 0.]
            .into_iter()
            .flat_map(f32::to_le_bytes),
    );
    let accessors = doc["accessors"].as_array_mut().expect("accessors");
    let positions = accessors.len();
    accessors.push(json!({"bufferView":positions_view,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}));
    doc["buffers"][0]["byteLength"] = json!(bin.len());
    doc["meshes"] = json!([{"primitives":[{"attributes":{"POSITION":positions}}]}]);
    doc["scenes"] = json!([{"nodes":[0]}]);
    doc["scene"] = json!(0);
    for lossless in [false, true] {
        let root = regia_test_utils::unique_temp_dir("plain_node_cook");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let source = assets.join("door.glb");
        std::fs::write(assets.join("door.tmp"), pack(&doc, &bin)).expect("write");
        std::fs::rename(assets.join("door.tmp"), &source).expect("publish");
        let settings = format!("(lossless_clips: {lossless})");
        let meta = format!(
            "(format_version: 2, id: \"1c6d2e3f-4051-4b62-8c73-8d94ea5f1026\", importer: \"gltf\", settings: {settings:?})"
        );
        std::fs::write(regia_asset::meta_path_for(&source), meta).expect("meta");
        let report =
            regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(root.clone()))
                .expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
        let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
            .expect("manifest")
            .expect("present");
        let id = manifest
            .id_for_path("assets/door.glb#nodeclip/motion")
            .expect("node subasset");
        let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked file");
        let (header, clip) = regia_animation::cooked::load_cooked_clip(&bytes).expect("runtime loader");
        assert!(header.dependencies.is_empty(), "no skeleton dependency");
        assert!((world(&clip, 0, 0.5).w_axis.x + 1.).abs() < 1e-6);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
