// SPDX-License-Identifier: MPL-2.0
//! Scene export tests: a fixture project is exported, re-read with the `gltf` crate, and checked
//! against positions, transforms, material factors and texture pixels computed independently.

use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_core::math::{Mat4, Vec3};
use serde_json::Value;

use super::*;
use crate::gltf_write::{GlbNode, GlbScene};

fn temp_project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets/meshes")).expect("dir");
    dir
}

fn guid(n: u128) -> Guid {
    Guid::from_u128(n)
}

/// A 4x4 RGBA image with a different value in every channel of every pixel.
fn test_png() -> (Vec<u8>, image::RgbaImage) {
    let img = image::RgbaImage::from_fn(4, 4, |x, y| {
        image::Rgba([
            (x * 60 + 7) as u8,
            (y * 50 + 3) as u8,
            ((x + y) * 30 + 11) as u8,
            (200 + x + y) as u8,
        ])
    });
    let bytes = encode_png(&image::DynamicImage::ImageRgba8(img.clone())).expect("png");
    (bytes, img)
}

/// A source prop: a node under a translated parent, one quad with every attribute.
fn prop_glb(with_transform_ext: bool) -> Vec<u8> {
    let (png, _) = test_png();
    let scene = GlbScene {
        generator: "fixture".into(),
        nodes: vec![
            GlbNode {
                name: "Parent".into(),
                children: vec![1],
                translation: [1.0, 0.0, 0.5],
                ..GlbNode::default()
            },
            GlbNode {
                name: "Quad".into(),
                mesh: Some(0),
                translation: [0.0, 2.0, 0.0],
                rotation: [0.0, 0.38268343, 0.0, 0.9238795],
                scale: [1.0, 1.5, 1.0],
                ..GlbNode::default()
            },
        ],
        roots: vec![0],
        meshes: vec![crate::gltf_write::GlbMesh {
            name: "Quad".into(),
            primitives: vec![crate::gltf_write::GlbPrimitive {
                positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 4],
                uvs: vec![
                    vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                    vec![[0.25, 0.25], [0.75, 0.25], [0.75, 0.75], [0.25, 0.75]],
                ],
                colors: vec![[1.0, 0.5, 0.25, 1.0]; 4],
                indices: vec![0, 1, 2, 0, 2, 3],
                material: Some(0),
                ..crate::gltf_write::GlbPrimitive::default()
            }],
            ..crate::gltf_write::GlbMesh::default()
        }],
        materials: vec![GlbMaterial {
            name: "Painted".into(),
            base_color: [0.8, 0.4, 0.2, 1.0],
            metallic: 0.25,
            roughness: 0.5,
            emissive: [0.1, 0.2, 0.3],
            base_color_texture: Some(GlbTextureRef {
                image: 0,
                tex_coord: 1,
            }),
            double_sided: true,
            ..GlbMaterial::default()
        }],
        images: vec![GlbImage::Embedded {
            bytes: png,
            mime: "image/png",
        }],
        ..GlbScene::default()
    };
    let mut extras = GlbExtras::default();
    extras.tangents.insert((0, 0), vec![[1.0, 0.0, 0.0, 1.0]; 4]);
    if with_transform_ext {
        extras.tex_transforms.insert(
            (0, GlbTexSlot::BaseColor),
            GlbTexTransform {
                offset: [0.5, 0.25],
                rotation: 0.5,
                scale: [2.0, 3.0],
            },
        );
    }
    scene.to_glb_with(&extras).expect("writes")
}

const T: &str = "regia_scene::transform::Transform";

fn level_text() -> String {
    format!(
        r#"SceneFile(
    format_version: 1,
    entities: [
        (guid: "{yard}", name: "Yard", components: {{
            "{T}": (translation: (10.0, 0.0, -4.0), rotation: (0.0, 0.7071068, 0.0, 0.7071068), scale: (1.0, 1.0, 1.0)),
        }}),
        (guid: "{prop}", name: "Prop", parent: Some("{yard}"), components: {{
            "{T}": (translation: (0.0, 1.0, 2.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (2.0, 2.0, 2.0)),
            "regia_render::runtime::MeshSource": (mesh: "assets/meshes/prop.glb", material: ""),
        }}),
        (guid: "{cube}", name: "Box", parent: Some("{yard}"), components: {{
            "{T}": (translation: (3.0, 0.5, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            "regia_render::runtime::MeshSource": (mesh: "builtin:cube", material: ""),
        }}),
        (guid: "{lamp}", name: "Lamp", parent: Some("{yard}"), components: {{
            "{T}": (translation: (0.0, 3.0, 0.0)),
            "regia_render::lights::PointLight": (position: (0.0, 0.0, 0.0), color: (1.0, 0.5, 0.25), intensity: 1256.6371, radius: 12.0, cast_shadows: true),
        }}),
        (guid: "{spot}", name: "Spot", components: {{
            "{T}": (translation: (0.0, 5.0, 0.0)),
            "regia_render::lights::SpotLight": (color: (1.0, 1.0, 1.0), intensity: 500.0, outer_angle: 0.6, inner_angle: 0.3, range: 20.0),
        }}),
        (guid: "{sun}", name: "Sun", components: {{
            "regia_render::lights::DirectionalLight": (direction: (0.0, -1.0, 0.0), color: (1.0, 0.95, 0.9), illuminance: 100000.0, cast_shadows: true, static_shadow: false),
        }}),
        (guid: "{cam}", name: "Camera", components: {{
            "{T}": (translation: (0.0, 2.0, 8.0)),
            "regia_render::camera::Camera": (fov_y: 1.0, near: 0.2, far: 500.0),
        }}),
    ],
)"#,
        yard = guid(1),
        prop = guid(2),
        cube = guid(3),
        lamp = guid(4),
        spot = guid(5),
        sun = guid(6),
        cam = guid(7),
    )
}

fn export_fixture(options: &ExportOptions) -> (PathBuf, Vec<u8>, ExportReport) {
    let root = temp_project("scene_export");
    std::fs::write(root.join("assets/meshes/prop.glb"), prop_glb(false)).expect("prop");
    let scene = SceneFile::from_ron_str(&level_text()).expect("level parses");
    let (glb, report) = export_scene(&scene, &root, options).expect("export");
    (root, glb, report)
}

struct Read {
    document: gltf::Document,
    buffers: Vec<gltf::buffer::Data>,
    images: Vec<gltf::image::Data>,
}

fn read(glb: &[u8]) -> Read {
    let (document, buffers, images) = gltf::import_slice(glb).expect("the exported file imports");
    Read {
        document,
        buffers,
        images,
    }
}

/// World matrix of every node, by node index.
fn node_worlds(doc: &gltf::Document) -> Vec<Mat4> {
    let mut worlds = vec![Mat4::IDENTITY; doc.nodes().count()];
    let mut stack: Vec<(gltf::Node, Mat4)> = doc
        .default_scene()
        .expect("scene")
        .nodes()
        .map(|n| (n, Mat4::IDENTITY))
        .collect();
    while let Some((node, parent)) = stack.pop() {
        let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        worlds[node.index()] = world;
        stack.extend(node.children().map(|c| (c, world)));
    }
    worlds
}

fn yaw() -> Mat4 {
    Mat4::from_scale(Vec3::new(-1.0, 1.0, -1.0))
}

fn near(a: Mat4, b: Mat4, eps: f32) -> bool {
    a.to_cols_array()
        .iter()
        .zip(b.to_cols_array())
        .all(|(x, y)| (x - y).abs() <= eps)
}

fn node_named<'a>(doc: &'a gltf::Document, name: &str) -> gltf::Node<'a> {
    doc.nodes()
        .find(|n| n.name() == Some(name))
        .unwrap_or_else(|| panic!("node {name}"))
}

// ----- structural checks from the glTF 2.0 specification -----

/// Checks the container and the JSON against the specification's required fields, accessor
/// bounds and buffer view alignment. Returns the first problem.
pub(super) fn check_structure(glb: &[u8]) -> Result<(), String> {
    if glb.len() < 20 || &glb[0..4] != b"glTF" {
        return Err("missing glTF magic".into());
    }
    let u32_at = |o: usize| u32::from_le_bytes([glb[o], glb[o + 1], glb[o + 2], glb[o + 3]]) as usize;
    if u32_at(4) != 2 {
        return Err("container version is not 2".into());
    }
    if u32_at(8) != glb.len() {
        return Err("header length differs from the file length".into());
    }
    if !glb.len().is_multiple_of(4) {
        return Err("file length is not a multiple of 4".into());
    }
    let json_len = u32_at(12);
    if &glb[16..20] != b"JSON" || !json_len.is_multiple_of(4) {
        return Err("first chunk is not an aligned JSON chunk".into());
    }
    let json: Value = serde_json::from_slice(glb.get(20..20 + json_len).ok_or("JSON chunk past the end")?)
        .map_err(|e| e.to_string())?;
    let mut bin_len = 0;
    let rest = 20 + json_len;
    if rest < glb.len() {
        let len = u32_at(rest);
        if &glb[rest + 4..rest + 8] != b"BIN\0" {
            return Err("second chunk is not BIN".into());
        }
        if !len.is_multiple_of(4) || rest + 8 + len != glb.len() {
            return Err("BIN chunk is misaligned or does not end the file".into());
        }
        bin_len = len;
    }
    if json["asset"]["version"] != "2.0" {
        return Err("asset.version must be \"2.0\"".into());
    }
    let arr = |key: &str| json[key].as_array().cloned().unwrap_or_default();
    let (nodes, meshes, materials) = (arr("nodes"), arr("meshes"), arr("materials"));
    let (images, textures, accessors, views) = (
        arr("images"),
        arr("textures"),
        arr("accessors"),
        arr("bufferViews"),
    );
    let (cameras, skins) = (arr("cameras"), arr("skins"));
    let lights = json["extensions"]["KHR_lights_punctual"]["lights"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let used: Vec<&str> = json["extensionsUsed"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    // Buffers and views.
    if !views.is_empty() {
        let declared = json["buffers"][0]["byteLength"]
            .as_u64()
            .ok_or("buffers[0].byteLength missing")? as usize;
        if declared != bin_len {
            return Err(format!("buffers[0].byteLength {declared} != BIN chunk {bin_len}"));
        }
    }
    for (i, v) in views.iter().enumerate() {
        let off = v["byteOffset"].as_u64().unwrap_or(0) as usize;
        let len = v["byteLength"]
            .as_u64()
            .ok_or(format!("bufferViews[{i}].byteLength missing"))? as usize;
        if v["buffer"].as_u64() != Some(0) || off + len > bin_len {
            return Err(format!("bufferViews[{i}] is outside the buffer"));
        }
        if !off.is_multiple_of(4) {
            return Err(format!("bufferViews[{i}].byteOffset {off} is not 4-byte aligned"));
        }
    }
    // Accessors.
    for (i, a) in accessors.iter().enumerate() {
        let comp = a["componentType"]
            .as_u64()
            .ok_or(format!("accessors[{i}].componentType missing"))?;
        let size = match comp {
            5120 | 5121 => 1,
            5122 | 5123 => 2,
            5125 | 5126 => 4,
            _ => return Err(format!("accessors[{i}] has component type {comp}")),
        };
        let count = a["count"]
            .as_u64()
            .filter(|c| *c > 0)
            .ok_or(format!("accessors[{i}].count must be >= 1"))? as usize;
        let n = match a["type"].as_str() {
            Some("SCALAR") => 1,
            Some("VEC2") => 2,
            Some("VEC3") => 3,
            Some("VEC4" | "MAT2") => 4,
            Some("MAT3") => 9,
            Some("MAT4") => 16,
            _ => return Err(format!("accessors[{i}].type missing")),
        };
        let view = views
            .get(
                a["bufferView"]
                    .as_u64()
                    .ok_or(format!("accessors[{i}].bufferView missing"))? as usize,
            )
            .ok_or(format!("accessors[{i}].bufferView out of range"))?;
        let off = a["byteOffset"].as_u64().unwrap_or(0) as usize;
        if !off.is_multiple_of(size) {
            return Err(format!(
                "accessors[{i}].byteOffset is not a multiple of the component size"
            ));
        }
        if off + count * size * n > view["byteLength"].as_u64().unwrap_or(0) as usize {
            return Err(format!("accessors[{i}] reads past its buffer view"));
        }
    }
    let acc = |i: &Value, what: &str| -> Result<Value, String> {
        accessors
            .get(i.as_u64().ok_or(format!("{what}: accessor index missing"))? as usize)
            .cloned()
            .ok_or(format!("{what}: accessor out of range"))
    };
    // Meshes.
    for (m, mesh) in meshes.iter().enumerate() {
        for (p, prim) in mesh["primitives"]
            .as_array()
            .ok_or(format!("meshes[{m}].primitives missing"))?
            .iter()
            .enumerate()
        {
            let pos = acc(&prim["attributes"]["POSITION"], "POSITION")?;
            let count = pos["count"].as_u64().unwrap_or(0);
            if pos["min"].as_array().map(Vec::len) != Some(3)
                || pos["max"].as_array().map(Vec::len) != Some(3)
            {
                return Err(format!("meshes[{m}].primitives[{p}] POSITION lacks min/max"));
            }
            for (name, a) in prim["attributes"].as_object().ok_or("attributes missing")? {
                if acc(a, name)?["count"].as_u64() != Some(count) {
                    return Err(format!(
                        "meshes[{m}].primitives[{p}] {name} count differs from POSITION"
                    ));
                }
            }
            if let Some(i) = prim.get("indices") {
                let idx = acc(i, "indices")?;
                if idx["type"] != "SCALAR"
                    || !matches!(idx["componentType"].as_u64(), Some(5121 | 5123 | 5125))
                {
                    return Err("indices must be unsigned scalar".into());
                }
                if idx["count"].as_u64().unwrap_or(1) % 3 != 0 {
                    return Err("triangle index count is not a multiple of 3".into());
                }
            }
            if let Some(mat) = prim.get("material").and_then(Value::as_u64)
                && mat as usize >= materials.len()
            {
                return Err("primitive material out of range".into());
            }
        }
    }
    // Nodes and scenes.
    for (i, n) in nodes.iter().enumerate() {
        for c in n["children"].as_array().cloned().unwrap_or_default() {
            if c.as_u64()
                .is_none_or(|c| c as usize >= nodes.len() || c as usize == i)
            {
                return Err(format!("nodes[{i}] has a bad child"));
            }
        }
        for (key, len) in [
            ("mesh", meshes.len()),
            ("camera", cameras.len()),
            ("skin", skins.len()),
        ] {
            if let Some(v) = n.get(key).and_then(Value::as_u64)
                && v as usize >= len
            {
                return Err(format!("nodes[{i}].{key} out of range"));
            }
        }
        if let Some(l) = n["extensions"]["KHR_lights_punctual"]["light"].as_u64() {
            if l as usize >= lights.len() {
                return Err(format!("nodes[{i}] light out of range"));
            }
            if !used.contains(&"KHR_lights_punctual") {
                return Err("KHR_lights_punctual used but not listed".into());
            }
        }
        if let Some(q) = n["rotation"].as_array() {
            let len = q
                .iter()
                .filter_map(Value::as_f64)
                .map(|c| c * c)
                .sum::<f64>()
                .sqrt();
            if (len - 1.0).abs() > 1e-5 {
                return Err(format!("nodes[{i}].rotation is not a unit quaternion"));
            }
        }
    }
    // A node has at most one parent.
    let mut parents = vec![0u32; nodes.len()];
    for n in &nodes {
        for c in n["children"].as_array().cloned().unwrap_or_default() {
            parents[c.as_u64().unwrap_or(0) as usize] += 1;
        }
    }
    if parents.iter().any(|p| *p > 1) {
        return Err("a node has two parents".into());
    }
    for n in json["scenes"][0]["nodes"]
        .as_array()
        .ok_or("scenes[0].nodes missing")?
    {
        if n.as_u64()
            .is_none_or(|n| n as usize >= nodes.len() || parents[n as usize] != 0)
        {
            return Err("scene root is out of range or has a parent".into());
        }
    }
    // Materials, textures, images.
    for (i, m) in materials.iter().enumerate() {
        for tex in [
            &m["pbrMetallicRoughness"]["baseColorTexture"],
            &m["pbrMetallicRoughness"]["metallicRoughnessTexture"],
            &m["normalTexture"],
            &m["emissiveTexture"],
            &m["occlusionTexture"],
        ] {
            if let Some(index) = tex.get("index").and_then(Value::as_u64) {
                if index as usize >= textures.len() {
                    return Err(format!("materials[{i}] texture out of range"));
                }
                if tex.get("extensions").is_some() && !used.contains(&"KHR_texture_transform") {
                    return Err("KHR_texture_transform used but not listed".into());
                }
            }
        }
    }
    for t in &textures {
        if t["source"].as_u64().is_none_or(|s| s as usize >= images.len()) {
            return Err("texture source out of range".into());
        }
    }
    for im in &images {
        if im.get("uri").is_none() == im.get("bufferView").is_none() {
            return Err("an image needs exactly one of uri and bufferView".into());
        }
        if im.get("bufferView").is_some() && im["mimeType"].as_str().is_none() {
            return Err("an embedded image needs a mimeType".into());
        }
    }
    // Skins and animations.
    for (i, skin) in skins.iter().enumerate() {
        let joints = skin["joints"]
            .as_array()
            .ok_or(format!("skins[{i}].joints missing"))?;
        if joints.is_empty()
            || joints
                .iter()
                .any(|j| j.as_u64().is_none_or(|j| j as usize >= nodes.len()))
        {
            return Err(format!("skins[{i}] has a bad joint"));
        }
        if let Some(ibm) = skin.get("inverseBindMatrices") {
            let a = acc(ibm, "inverseBindMatrices")?;
            if a["type"] != "MAT4" || a["count"].as_u64() != Some(joints.len() as u64) {
                return Err(format!(
                    "skins[{i}] inverse bind matrices do not match the joints"
                ));
            }
        }
    }
    for (i, anim) in arr("animations").iter().enumerate() {
        let samplers = anim["samplers"]
            .as_array()
            .ok_or(format!("animations[{i}].samplers"))?;
        for ch in anim["channels"]
            .as_array()
            .ok_or(format!("animations[{i}].channels"))?
        {
            let sampler = samplers
                .get(ch["sampler"].as_u64().ok_or("channel sampler")? as usize)
                .ok_or("channel sampler out of range")?;
            let input = acc(&sampler["input"], "sampler input")?;
            let output = acc(&sampler["output"], "sampler output")?;
            if input["min"].as_array().map(Vec::len) != Some(1)
                || input["max"].as_array().map(Vec::len) != Some(1)
            {
                return Err("animation input accessor lacks min/max".into());
            }
            if ch["target"]["node"]
                .as_u64()
                .is_none_or(|n| n as usize >= nodes.len())
            {
                return Err("channel target node out of range".into());
            }
            let per = match ch["target"]["path"].as_str() {
                Some("rotation") => 4,
                Some("translation" | "scale") => 3,
                Some("weights") => 1,
                _ => return Err("channel target path".into()),
            };
            let (ic, oc) = (
                input["count"].as_u64().unwrap_or(0),
                output["count"].as_u64().unwrap_or(0),
            );
            if oc % ic != 0 || (per != 1 && oc != ic) {
                return Err(format!(
                    "animations[{i}] output count {oc} does not fit {ic} keys"
                ));
            }
        }
    }
    // Lights and cameras.
    for (i, l) in lights.iter().enumerate() {
        if !matches!(l["type"].as_str(), Some("directional" | "point" | "spot")) {
            return Err(format!("lights[{i}].type"));
        }
        if l["type"] == "spot" {
            let (inner, outer) = (
                l["spot"]["innerConeAngle"].as_f64().unwrap_or(0.0),
                l["spot"]["outerConeAngle"]
                    .as_f64()
                    .unwrap_or(std::f64::consts::FRAC_PI_4),
            );
            if !(0.0 <= inner && inner < outer && outer <= std::f64::consts::FRAC_PI_2 + 1e-9) {
                return Err(format!(
                    "lights[{i}] cone angles violate 0 <= inner < outer <= pi/2"
                ));
            }
        }
    }
    for (i, c) in cameras.iter().enumerate() {
        match c["type"].as_str() {
            Some("perspective") => {
                let p = &c["perspective"];
                if p["yfov"].as_f64().is_none_or(|v| v <= 0.0) || p["znear"].as_f64().is_none_or(|v| v <= 0.0)
                {
                    return Err(format!("cameras[{i}] perspective needs yfov > 0 and znear > 0"));
                }
                if let Some(far) = p["zfar"].as_f64()
                    && far <= p["znear"].as_f64().unwrap_or(0.0)
                {
                    return Err(format!("cameras[{i}] zfar <= znear"));
                }
            }
            Some("orthographic") => {
                let o = &c["orthographic"];
                if ["xmag", "ymag", "znear", "zfar"]
                    .iter()
                    .any(|k| o[*k].as_f64().is_none())
                {
                    return Err(format!("cameras[{i}] orthographic fields"));
                }
            }
            _ => return Err(format!("cameras[{i}].type")),
        }
    }
    Ok(())
}

// ----- tests -----

#[test]
fn the_fixture_export_passes_the_structural_checks() {
    let (_root, glb, report) = export_fixture(&ExportOptions::default());
    check_structure(&glb).expect("structure");
    // Wrapper + 7 entities.
    assert_eq!(report.nodes, 8, "{report:?}");
    assert_eq!(report.lights, 3);
    assert_eq!(report.cameras, 1);
    assert_eq!(report.meshes, 2);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn export_is_deterministic() {
    let (_a, one, _) = export_fixture(&ExportOptions::default());
    let (_b, two, _) = export_fixture(&ExportOptions::default());
    assert_eq!(one, two, "identical bytes for identical input");
}

#[test]
fn node_transforms_round_trip_through_the_axis_conversion() {
    let (_root, glb, _) = export_fixture(&ExportOptions::default());
    let r = read(&glb);
    let worlds = node_worlds(&r.document);
    // Regia world of each entity, computed here from the level text.
    let yard = Mat4::from_rotation_translation(
        regia_core::math::Quat::from_xyzw(
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
        )
        .normalize(),
        Vec3::new(10.0, 0.0, -4.0),
    );
    let prop = yard
        * Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            regia_core::math::Quat::IDENTITY,
            Vec3::new(0.0, 1.0, 2.0),
        );
    // The importer puts a half turn above the scene: Regia world = yaw * glTF world.
    for (name, expected) in [("Yard", yard), ("Prop", prop)] {
        let node = node_named(&r.document, name);
        let regia = yaw() * worlds[node.index()];
        assert!(near(regia, expected, 1e-4), "{name}: {regia:?} vs {expected:?}");
    }
    // The wrapper is the root and the only scene node.
    let scene_roots: Vec<_> = r.document.default_scene().expect("scene").nodes().collect();
    assert_eq!(scene_roots.len(), 1);
    assert_eq!(scene_roots[0].name(), Some(AXIS_NODE_NAME));
}

#[test]
fn mesh_vertices_match_the_cooked_regia_space_positions() {
    let (_root, glb, _) = export_fixture(&ExportOptions::default());
    let r = read(&glb);
    let worlds = node_worlds(&r.document);
    let prop_node = node_named(&r.document, "Prop");
    let mesh = prop_node.mesh().expect("prop mesh");
    let prim = mesh.primitives().next().expect("primitive");
    let reader = prim.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    let got: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();

    // Expected, from first principles: the source quad under its two nodes, then the importer's
    // half turn (Regia vertex = yaw * source world), then the entity's Regia world.
    let q = regia_core::math::Quat::from_xyzw(0.0, 0.38268343, 0.0, 0.9238795);
    let source_world = Mat4::from_translation(Vec3::new(1.0, 0.0, 0.5))
        * Mat4::from_scale_rotation_translation(Vec3::new(1.0, 1.5, 1.0), q, Vec3::new(0.0, 2.0, 0.0));
    let yard = Mat4::from_rotation_translation(
        regia_core::math::Quat::from_xyzw(
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
        )
        .normalize(),
        Vec3::new(10.0, 0.0, -4.0),
    );
    let entity = yard
        * Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            regia_core::math::Quat::IDENTITY,
            Vec3::new(0.0, 1.0, 2.0),
        );
    let src = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
    for (g, s) in got.iter().zip(src) {
        let expected_regia = entity
            .transform_point3(yaw().transform_point3(source_world.transform_point3(Vec3::from_array(s))));
        // What a re-import computes: yaw * (glTF world * vertex).
        let reimported =
            yaw().transform_point3(worlds[prop_node.index()].transform_point3(Vec3::from_array(*g)));
        assert!(
            (reimported - expected_regia).length() < 1e-4,
            "{reimported:?} vs {expected_regia:?}"
        );
    }
    assert_eq!(got.len(), 4);
}

#[test]
fn vertex_attributes_survive() {
    let (_root, glb, _) = export_fixture(&ExportOptions::default());
    let r = read(&glb);
    let mesh = node_named(&r.document, "Prop").mesh().expect("mesh");
    let prim = mesh.primitives().next().expect("prim");
    let reader = prim.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    assert_eq!(reader.read_normals().expect("normals").count(), 4);
    let tangents: Vec<[f32; 4]> = reader.read_tangents().expect("tangents").collect();
    // Tangent (1,0,0) is turned by the source node's 45 degree yaw to (0.707, 0, -0.707), then by the
    // half turn about +Y to (-0.707, 0, 0.707); handedness is kept.
    let want = [
        -std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
    ];
    for (got, want) in tangents[0][..3].iter().zip(want) {
        assert!((got - want).abs() < 1e-5, "{:?}", tangents[0]);
    }
    assert_eq!(tangents[0][3], 1.0);
    let uv0: Vec<[f32; 2]> = reader.read_tex_coords(0).expect("uv0").into_f32().collect();
    let uv1: Vec<[f32; 2]> = reader.read_tex_coords(1).expect("uv1").into_f32().collect();
    assert_eq!(uv0[2], [1.0, 1.0]);
    assert_eq!(uv1[0], [0.25, 0.25]);
    let colors: Vec<[f32; 4]> = reader.read_colors(0).expect("colors").into_rgba_f32().collect();
    assert_eq!(colors[0], [1.0, 0.5, 0.25, 1.0]);
    assert_eq!(reader.read_indices().expect("indices").into_u32().count(), 6);
}

#[test]
fn material_factors_and_the_texture_round_trip() {
    let (_root, glb, _) = export_fixture(&ExportOptions::default());
    let r = read(&glb);
    let material = r.document.materials().next().expect("material");
    let pbr = material.pbr_metallic_roughness();
    assert_eq!(material.name(), Some("Painted"));
    assert_eq!(pbr.base_color_factor(), [0.8, 0.4, 0.2, 1.0]);
    assert_eq!(pbr.metallic_factor(), 0.25);
    assert_eq!(pbr.roughness_factor(), 0.5);
    assert_eq!(material.emissive_factor(), [0.1, 0.2, 0.3]);
    assert!(material.double_sided());
    let tex = pbr.base_color_texture().expect("base color texture");
    assert_eq!(tex.tex_coord(), 1, "the UV set the slot samples is kept");
    let data = &r.images[tex.texture().source().index()];
    let (_, expected) = test_png();
    assert_eq!((data.width, data.height), (4, 4));
    assert_eq!(data.pixels, expected.into_raw(), "pixels survive PNG embedding");
}

#[test]
fn an_exported_file_imports_its_own_lights() {
    let (_root, glb, _) = export_fixture(&ExportOptions::default());
    let (lights, node_lights) = crate::gltf_extensions::punctual_lights(&glb);
    let by_name = |n: &str| {
        lights
            .iter()
            .find(|l| l.name.as_deref() == Some(n))
            .unwrap_or_else(|| panic!("{n}"))
    };
    assert!((by_name("Lamp").intensity - 100.0).abs() < 1e-3);
    assert_eq!(by_name("Lamp").range, Some(12.0));
    assert_eq!(by_name("Sun").intensity, 100000.0);
    assert_eq!(
        by_name("Sun").kind,
        crate::gltf_extensions::GltfLightKind::Directional
    );
    assert!(matches!(
        by_name("Spot").kind,
        crate::gltf_extensions::GltfLightKind::Spot { .. }
    ));
    assert_eq!(node_lights.len(), lights.len(), "every light is on its node");
}

#[test]
fn lights_use_punctual_units_and_aim_down_minus_z() {
    let (_root, glb, _) = export_fixture(&ExportOptions::default());
    let json: Value = {
        let len = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
        serde_json::from_slice(&glb[20..20 + len]).expect("json")
    };
    let lights = json["extensions"]["KHR_lights_punctual"]["lights"]
        .as_array()
        .expect("lights");
    let by_name = |n: &str| {
        lights
            .iter()
            .find(|l| l["name"] == n)
            .unwrap_or_else(|| panic!("{n}"))
    };
    // 1256.6371 lm / (4 pi) = 100 cd.
    assert!((by_name("Lamp")["intensity"].as_f64().expect("i") - 100.0).abs() < 1e-3);
    assert_eq!(by_name("Lamp")["range"], 12.0);
    assert_eq!(
        by_name("Spot")["spot"]["outerConeAngle"]
            .as_f64()
            .map(|v| (v * 10.0).round()),
        Some(6.0)
    );
    assert_eq!(by_name("Sun")["intensity"], 100000.0);
    assert_eq!(by_name("Sun")["type"], "directional");

    let r = read(&glb);
    let worlds = node_worlds(&r.document);
    // The sun shines down Regia -Y; its node must send local -Z there (in Regia axes).
    let sun = node_named(&r.document, "Sun");
    let regia = yaw() * worlds[sun.index()];
    let aim = regia.transform_vector3(Vec3::NEG_Z);
    assert!((aim - Vec3::NEG_Y).length() < 1e-5, "{aim:?}");
    let cam = node_named(&r.document, "Camera");
    let c = cam.camera().expect("camera");
    match c.projection() {
        gltf::camera::Projection::Perspective(p) => {
            assert!((p.yfov() - 1.0).abs() < 1e-6);
            assert!((p.znear() - 0.2).abs() < 1e-6);
            assert_eq!(p.zfar(), Some(500.0));
        }
        gltf::camera::Projection::Orthographic(_) => panic!("perspective expected"),
    }
}

#[test]
fn a_selection_keeps_world_placement() {
    let options = ExportOptions {
        entities: Some(vec![guid(2)]),
        ..ExportOptions::default()
    };
    let (_root, glb, report) = export_fixture(&options);
    check_structure(&glb).expect("structure");
    assert_eq!(report.nodes, 2, "wrapper plus the prop");
    let r = read(&glb);
    let worlds = node_worlds(&r.document);
    let prop = node_named(&r.document, "Prop");
    let yard = Mat4::from_rotation_translation(
        regia_core::math::Quat::from_xyzw(
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
        )
        .normalize(),
        Vec3::new(10.0, 0.0, -4.0),
    );
    let expected = yard
        * Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            regia_core::math::Quat::IDENTITY,
            Vec3::new(0.0, 1.0, 2.0),
        );
    assert!(near(yaw() * worlds[prop.index()], expected, 1e-4));
}

#[test]
fn an_unknown_entity_and_an_empty_scene_are_errors_with_codes() {
    let root = temp_project("scene_export_err");
    let scene = SceneFile::from_ron_str(&level_text()).expect("level");
    let bad = ExportOptions {
        entities: Some(vec![guid(999)]),
        ..ExportOptions::default()
    };
    let err = export_scene(&scene, &root, &bad).expect_err("unknown");
    assert_eq!(err.code(), "glb_unknown_entity");
    let empty = SceneFile::from_ron_str("SceneFile(format_version: 1, entities: [])").expect("empty");
    let err = export_scene(&empty, &root, &ExportOptions::default()).expect_err("empty");
    assert_eq!(err.code(), "glb_nothing_to_export");
    let err = read_scene(&root.join("missing.level.ron")).expect_err("missing");
    assert_eq!(err.code(), "glb_read_failed");
    std::fs::write(root.join("bad.level.ron"), "not a scene").expect("write");
    assert_eq!(
        read_scene(&root.join("bad.level.ron")).expect_err("bad").code(),
        "glb_parse_failed"
    );
}

#[test]
fn a_missing_mesh_is_a_warning_not_a_failure() {
    let root = temp_project("scene_export_missing");
    let text = format!(
        r#"SceneFile(format_version: 1, entities: [
            (guid: "{}", name: "Ghost", components: {{
                "regia_render::runtime::MeshSource": (mesh: "assets/meshes/nope.glb", material: ""),
                "regia_render::runtime::MeshSource2": (),
            }}),
            (guid: "{}", name: "Escape", components: {{
                "regia_render::runtime::MeshSource": (mesh: "../outside.glb", material: ""),
            }}),
        ])"#,
        guid(1),
        guid(2)
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let (glb, report) = export_scene(&scene, &root, &ExportOptions::default()).expect("export");
    check_structure(&glb).expect("structure");
    assert!(
        report.warnings.iter().any(|w| w.starts_with("mesh_missing")),
        "{:?}",
        report.warnings
    );
    assert!(
        report.warnings.iter().any(|w| w.starts_with("path_rejected")),
        "{:?}",
        report.warnings
    );
    assert_eq!(report.meshes, 0);
}

/// A cooked project holding the prop, with the level that names it.
fn cooked_project(tag: &str) -> (PathBuf, SceneFile) {
    let root = temp_project(tag);
    std::fs::write(root.join("assets/meshes/prop.glb"), prop_glb(false)).expect("prop");
    let report = crate::cook_project(crate::CookOptions {
        project_root: root.clone(),
        platform: crate::platform::host_platform().to_string(),
        incremental: false,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    (
        root,
        SceneFile::from_ron_str(&level_text()).expect("level parses"),
    )
}

/// Positions of the first primitive of the node, sorted so a reordered vertex list compares.
fn sorted_positions(glb: &[u8], node: &str) -> Vec<[f32; 3]> {
    let r = read(glb);
    let found = node_named(&r.document, node);
    let mesh = found.mesh().expect("the node has a mesh");
    let primitive = mesh.primitives().next().expect("primitive");
    let reader = primitive.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    let mut positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
    positions.sort_by(|a, b| {
        a.iter()
            .zip(b)
            .map(|(x, y)| x.total_cmp(y))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    positions
}

#[track_caller]
fn assert_same_positions(a: &[[f32; 3]], b: &[[f32; 3]]) {
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(b) {
        for axis in 0..3 {
            assert!((a[axis] - b[axis]).abs() < 1e-3, "{a:?} against {b:?}");
        }
    }
}

#[test]
fn a_missing_source_exports_the_cooked_mesh_in_both_axis_modes() {
    let (root, scene) = cooked_project("scene_export_cooked_mesh");
    for options in [
        ExportOptions::default(),
        ExportOptions {
            forward: Forward::Keep,
            ..ExportOptions::default()
        },
    ] {
        let (with_source, _) = export_scene(&scene, &root, &options).expect("source export");
        let moved = root.join("assets/meshes/prop.glb");
        let aside = root.join("prop.glb.aside");
        std::fs::rename(&moved, &aside).expect("move aside");
        let (glb, report) = export_scene(&scene, &root, &options).expect("cooked export");
        std::fs::rename(&aside, &moved).expect("move back");
        check_structure(&glb).expect("structure");
        assert!(
            report.warnings.iter().any(|w| w.starts_with("mesh_from_cooked:")),
            "{:?}",
            report.warnings
        );
        assert!(
            !report.warnings.iter().any(|w| w.starts_with("mesh_missing")),
            "{:?}",
            report.warnings
        );
        assert_same_positions(
            &sorted_positions(&glb, "Prop"),
            &sorted_positions(&with_source, "Prop"),
        );
    }
}

#[test]
fn the_cooked_mesh_carries_its_material_and_texture() {
    let (root, scene) = cooked_project("scene_export_cooked_texture");
    std::fs::remove_file(root.join("assets/meshes/prop.glb")).expect("remove");
    let (glb, report) = export_scene(&scene, &root, &ExportOptions::default()).expect("export");
    check_structure(&glb).expect("structure");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("texture_from_cooked:")),
        "{:?}",
        report.warnings
    );
    let r = read(&glb);
    let prop = node_named(&r.document, "Prop");
    let mesh = prop.mesh().expect("mesh");
    let primitive = mesh.primitives().next().expect("primitive");
    let material = primitive.material();
    let info = material
        .pbr_metallic_roughness()
        .base_color_texture()
        .expect("the cooked base color map");
    let pixels = &r.images[info.texture().source().index()];
    let (_, reference) = test_png();
    assert_eq!((pixels.width, pixels.height), reference.dimensions());
    let channels = pixels.pixels.len() / (pixels.width * pixels.height) as usize;
    // Block compression is lossy, and this image changes hard from texel to texel: the decoded level
    // stays near the source, on average.
    let mut difference = 0u32;
    for (y, row) in pixels
        .pixels
        .chunks_exact(pixels.width as usize * channels)
        .enumerate()
    {
        for (x, texel) in row.chunks_exact(channels).enumerate() {
            let want = reference.get_pixel(x as u32, y as u32);
            for (got, want) in texel.iter().zip(want.0).take(3) {
                difference += u32::from(got.abs_diff(want));
            }
        }
    }
    let mean = difference / (pixels.width * pixels.height * 3);
    assert!(mean <= 24, "mean difference {mean}");
}

#[test]
fn a_block_compressed_cooked_texture_decodes_for_the_export() {
    let root = temp_project("scene_export_cooked_bc");
    let reference = image::RgbaImage::from_fn(64, 64, |x, y| {
        image::Rgba([(x * 4) as u8, (y * 4) as u8, 128, 255])
    });
    std::fs::create_dir_all(root.join("assets/textures")).expect("dir");
    reference
        .save(root.join("assets/textures/gradient.png"))
        .expect("png");
    let report = crate::cook_project(crate::CookOptions {
        project_root: root.clone(),
        platform: crate::platform::host_platform().to_string(),
        incremental: false,
    })
    .expect("cook");
    assert!(
        !report
            .errors
            .iter()
            .any(|(path, _)| path.ends_with("gradient.png")),
        "{:?}",
        report.errors
    );
    std::fs::remove_file(root.join("assets/textures/gradient.png")).expect("remove");

    let options = ExportOptions::default();
    let mut exporter = Exporter::new(&root, &options);
    let slot = exporter
        .slot_texture(Some(&TextureSlot::Path("assets/textures/gradient.png".into())))
        .expect("the cooked texture stands in");
    let warnings = exporter.warnings.clone();
    assert!(
        warnings.iter().any(|w| w.starts_with("texture_from_cooked:")),
        "{warnings:?}"
    );
    let crate::gltf_write::GlbImage::Embedded { bytes, mime } = &exporter.scene.images[slot.image] else {
        panic!("embedded");
    };
    assert_eq!(*mime, "image/png");
    let decoded = image::load_from_memory(bytes).expect("png").to_rgba8();
    assert_eq!(decoded.dimensions(), reference.dimensions());
    // BC7 on a smooth gradient stays close to it.
    for (a, b) in decoded.pixels().zip(reference.pixels()) {
        for c in 0..3 {
            assert!(a.0[c].abs_diff(b.0[c]) <= 8, "{a:?} against {b:?}");
        }
    }
}

#[test]
fn with_the_meta_gone_too_the_database_row_finds_the_cook() {
    let (root, scene) = cooked_project("scene_export_cooked_db");
    std::fs::remove_file(root.join("assets/meshes/prop.glb")).expect("source");
    std::fs::remove_file(root.join("assets/meshes/prop.glb.meta")).expect("meta");
    let (_, report) = export_scene(&scene, &root, &ExportOptions::default()).expect("export");
    assert!(
        report.warnings.iter().any(|w| w.starts_with("mesh_from_cooked:")),
        "{:?}",
        report.warnings
    );
    assert!(report.meshes >= 1);
}

#[test]
fn a_cache_without_the_mesh_still_warns_mesh_missing() {
    let (root, scene) = cooked_project("scene_export_cooked_gone");
    std::fs::remove_file(root.join("assets/meshes/prop.glb")).expect("source");
    std::fs::remove_dir_all(regia_core::brand::project_state_dir(&root).join("cooked")).expect("cache");
    let (_, report) = export_scene(&scene, &root, &ExportOptions::default()).expect("export");
    assert!(
        report.warnings.iter().any(|w| w.starts_with("mesh_missing")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn an_instanced_source_exports_every_copy() {
    let root = temp_project("scene_export_instanced");
    let mut bin = Vec::new();
    for v in [
        0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 5.0, 0.0, 0.0, 9.0, 0.0, 0.0,
    ] {
        bin.extend_from_slice(&v.to_le_bytes());
    }
    let json = serde_json::json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["EXT_mesh_gpu_instancing"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "n",
                   "extensions": {"EXT_mesh_gpu_instancing": {"attributes": {"TRANSLATION": 1}}}}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
        "buffers": [{"byteLength": 72, "uri": "data.bin"}],
        "bufferViews": [{"buffer": 0, "byteLength": 36}, {"buffer": 0, "byteOffset": 36, "byteLength": 36}],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]},
            {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"},
        ],
    });
    std::fs::write(root.join("assets/meshes/data.bin"), &bin).expect("bin");
    std::fs::write(
        root.join("assets/meshes/forest.gltf"),
        serde_json::to_vec(&json).expect("json"),
    )
    .expect("gltf");
    let text = format!(
        r#"SceneFile(format_version: 1, entities: [
            (guid: "{}", name: "Forest", components: {{
                "regia_render::runtime::MeshSource": (mesh: "assets/meshes/forest.gltf", material: ""),
            }}),
        ])"#,
        guid(1)
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let options = ExportOptions {
        forward: Forward::Keep,
        ..ExportOptions::default()
    };
    let (glb, report) = export_scene(&scene, &root, &options).expect("export");
    check_structure(&glb).expect("structure");
    assert_eq!(report.meshes, 1);
    let r = read(&glb);
    let mesh = r.document.meshes().next().expect("mesh");
    assert_eq!(mesh.primitives().count(), 3, "one primitive per copy");
    let copy = mesh.primitives().nth(2).expect("copy 2");
    let reader = copy.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    let first = reader
        .read_positions()
        .expect("positions")
        .next()
        .expect("vertex");
    assert_eq!(first, [9.0, 0.0, 0.0]);
}

#[test]
fn a_baked_texture_transform_is_not_exported_a_second_time() {
    let root = temp_project("scene_export_baked_uv");
    let mut bin = Vec::new();
    for v in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        bin.extend_from_slice(&v.to_le_bytes());
    }
    // Stored 0, 64, 128 with a transform of 1/64 and an offset of 0.5: the UVs are 0.5, 1.5, 2.5.
    for v in [0u16, 0, 64, 0, 128, 0] {
        bin.extend_from_slice(&v.to_le_bytes());
    }
    let (image, _) = test_png();
    let image_at = bin.len();
    bin.extend_from_slice(&image);
    let json = serde_json::json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_mesh_quantization", "KHR_texture_transform"],
        "extensionsRequired": ["KHR_mesh_quantization"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "n"}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "TEXCOORD_0": 1}, "material": 0}]}],
        "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {
            "index": 0,
            "extensions": {"KHR_texture_transform": {"offset": [0.5, 0.0], "scale": [0.015625, 1.0]}},
        }}}],
        "textures": [{"source": 0}],
        "images": [{"bufferView": 2, "mimeType": "image/png"}],
        "buffers": [{"byteLength": bin.len(), "uri": "data.bin"}],
        "bufferViews": [
            {"buffer": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 12},
            {"buffer": 0, "byteOffset": image_at, "byteLength": image.len()},
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]},
            {"bufferView": 1, "componentType": 5123, "count": 3, "type": "VEC2"},
        ],
    });
    std::fs::write(root.join("assets/meshes/data.bin"), &bin).expect("bin");
    std::fs::write(
        root.join("assets/meshes/baked.gltf"),
        serde_json::to_vec(&json).expect("json"),
    )
    .expect("gltf");
    let text = format!(
        r#"SceneFile(format_version: 1, entities: [
            (guid: "{}", name: "Baked", components: {{
                "regia_render::runtime::MeshSource": (mesh: "assets/meshes/baked.gltf", material: ""),
            }}),
        ])"#,
        guid(1)
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let options = ExportOptions {
        forward: Forward::Keep,
        ..ExportOptions::default()
    };
    let (glb, _report) = export_scene(&scene, &root, &options).expect("export");
    check_structure(&glb).expect("structure");
    let r = read(&glb);
    let primitive = r
        .document
        .meshes()
        .next()
        .expect("mesh")
        .primitives()
        .next()
        .expect("primitive");
    let reader = primitive.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).expect("uvs").into_f32().collect();
    let u: Vec<f32> = uvs.iter().map(|uv| uv[0]).collect();
    for want in [0.5, 1.5, 2.5] {
        assert!(u.iter().any(|got| (got - want).abs() < 1e-3), "{u:?}");
    }
    let raw = gltf::Glb::from_slice(&glb).expect("glb");
    let raw: serde_json::Value = serde_json::from_slice(&raw.json).expect("json");
    assert!(
        raw["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["index"].is_u64(),
        "the texture is still there: {raw}"
    );
    assert!(
        raw["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["extensions"].is_null(),
        "the UVs already hold the transform: {raw}"
    );
}

#[test]
fn keep_writes_regia_axes_without_the_root_node() {
    let options = ExportOptions {
        forward: Forward::Keep,
        ..ExportOptions::default()
    };
    let (_root, glb, report) = export_fixture(&options);
    check_structure(&glb).expect("structure");
    assert_eq!(report.nodes, 7);
    let r = read(&glb);
    assert!(r.document.nodes().all(|n| n.name() != Some(AXIS_NODE_NAME)));
    let worlds = node_worlds(&r.document);
    let yard = Mat4::from_rotation_translation(
        regia_core::math::Quat::from_xyzw(
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
        )
        .normalize(),
        Vec3::new(10.0, 0.0, -4.0),
    );
    assert!(near(worlds[node_named(&r.document, "Yard").index()], yard, 1e-4));
}

#[test]
fn texture_transforms_are_kept_and_listed() {
    let root = temp_project("scene_export_tt");
    std::fs::write(root.join("assets/meshes/prop.glb"), prop_glb(true)).expect("prop");
    let scene = SceneFile::from_ron_str(&level_text()).expect("level");
    let (glb, _) = export_scene(&scene, &root, &ExportOptions::default()).expect("export");
    check_structure(&glb).expect("structure");
    let len = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
    let json: Value = serde_json::from_slice(&glb[20..20 + len]).expect("json");
    let t = &json["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["extensions"]["KHR_texture_transform"];
    assert_eq!(t["offset"], serde_json::json!([0.5, 0.25]));
    assert_eq!(t["scale"], serde_json::json!([2.0, 3.0]));
    assert_eq!(t["rotation"], 0.5);
}

#[test]
fn a_prefab_instance_expands_into_its_prefab_entities() {
    let root = temp_project("scene_export_prefab");
    std::fs::create_dir_all(root.join("assets/prefabs")).expect("dir");
    let prefab_id = guid(0x77);
    let prefab = format!(
        r#"SceneFile(format_version: 1, entities: [
            (guid: "{r}", name: "Lamp", components: {{ "{T}": (translation: (0.0, 0.0, 0.0)) }}),
            (guid: "{c}", name: "Bulb", parent: Some("{r}"), components: {{
                "{T}": (translation: (0.0, 1.5, 0.0)),
                "regia_render::lights::PointLight": (color: (1.0, 1.0, 1.0), intensity: 628.3185, radius: 5.0),
            }}),
        ])"#,
        r = guid(0x100),
        c = guid(0x101)
    );
    std::fs::write(root.join("assets/prefabs/lamp.prefab.ron"), prefab).expect("prefab");
    let meta = MetaSidecar::new("prefab", regia_asset::AssetId(prefab_id));
    std::fs::write(
        root.join("assets/prefabs/lamp.prefab.ron.meta"),
        meta.to_ron_string().expect("meta"),
    )
    .expect("meta");
    let level = format!(
        r#"SceneFile(format_version: 1, entities: [
            (guid: "{l}", name: "Post", components: {{ "{T}": (translation: (4.0, 0.0, 0.0)) }},
             prefab_instance: (prefab: "asset:{p}")),
        ])"#,
        l = guid(1),
        p = prefab_id
    );
    let scene = SceneFile::from_ron_str(&level).expect("level");
    let (glb, report) = export_scene(&scene, &root, &ExportOptions::default()).expect("export");
    check_structure(&glb).expect("structure");
    assert_eq!(report.lights, 1, "{:?}", report.warnings);
    let r = read(&glb);
    let worlds = node_worlds(&r.document);
    let bulb = node_named(&r.document, "Bulb");
    let p = (yaw() * worlds[bulb.index()]).transform_point3(Vec3::ZERO);
    assert!((p - Vec3::new(4.0, 1.5, 0.0)).length() < 1e-5, "{p:?}");
}

#[test]
fn a_written_file_is_the_exported_bytes() {
    let root = temp_project("scene_export_write");
    let out = root.join("out/level.glb");
    std::fs::write(root.join("a.level.ron"), level_text()).expect("level");
    std::fs::write(root.join("assets/meshes/prop.glb"), prop_glb(false)).expect("prop");
    let report =
        export_file(&root.join("a.level.ron"), &root, &out, &ExportOptions::default()).expect("export");
    let bytes = std::fs::read(&out).expect("read");
    assert_eq!(bytes.len(), report.bytes);
    check_structure(&bytes).expect("structure");
    assert!(Path::new(&out).is_file());
}

#[test]
fn the_structural_checker_rejects_broken_files() {
    let (_root, glb, _) = export_fixture(&ExportOptions::default());
    let mut short = glb.clone();
    short.truncate(short.len() - 4);
    assert!(check_structure(&short).is_err());
    let mut bad_version = glb.clone();
    bad_version[4] = 3;
    assert!(check_structure(&bad_version).is_err());
}

// ----- skins and clips -----

/// A source rig: root "Rig" with joints Hip (0,1,0) and Knee (0,1,0 below Hip), a skinned
/// triangle, and a clip turning the knee.
fn rig_glb() -> Vec<u8> {
    use crate::gltf_write::{GlbAnimation, GlbChannel, GlbMesh, GlbPath, GlbPrimitive, GlbSkin};
    let ibm_hip = Mat4::from_translation(Vec3::new(0.0, -1.0, 0.0)).to_cols_array();
    let ibm_knee = Mat4::from_translation(Vec3::new(0.0, -2.0, 0.0)).to_cols_array();
    GlbScene {
        generator: "fixture".into(),
        nodes: vec![
            GlbNode {
                name: "Rig".into(),
                children: vec![1],
                ..GlbNode::default()
            },
            GlbNode {
                name: "Hip".into(),
                children: vec![2],
                translation: [0.0, 1.0, 0.0],
                ..GlbNode::default()
            },
            GlbNode {
                name: "Knee".into(),
                translation: [0.0, 1.0, 0.0],
                ..GlbNode::default()
            },
            GlbNode {
                name: "Body".into(),
                mesh: Some(0),
                skin: Some(0),
                translation: [9.0, 9.0, 9.0], // ignored by the specification for a skinned node
                ..GlbNode::default()
            },
        ],
        roots: vec![0, 3],
        meshes: vec![GlbMesh {
            name: "Body".into(),
            primitives: vec![GlbPrimitive {
                positions: vec![[0.0, 1.0, 0.0], [0.5, 2.0, 0.0], [-0.5, 2.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                joints: vec![vec![[0, 1, 0, 0]; 3]],
                weights: vec![vec![[0.25, 0.75, 0.0, 0.0]; 3]],
                indices: vec![0, 1, 2],
                ..GlbPrimitive::default()
            }],
            ..GlbMesh::default()
        }],
        skins: vec![GlbSkin {
            name: "Legs".into(),
            joints: vec![1, 2],
            inverse_bind: vec![ibm_hip, ibm_knee],
        }],
        animations: vec![GlbAnimation {
            name: "Bend".into(),
            channels: vec![GlbChannel {
                node: 2,
                path: GlbPath::Rotation,
                times: vec![0.0, 0.5, 1.0],
                values: vec![
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                    0.0,
                    0.0,
                    0.3826834,
                    0.9238795,
                    0.0,
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ],
            }],
        }],
        ..GlbScene::default()
    }
    .to_glb()
    .expect("writes")
}

fn rig_scene_export(options: &ExportOptions) -> (Vec<u8>, ExportReport) {
    let root = temp_project("scene_export_rig");
    std::fs::write(root.join("assets/meshes/rig.glb"), rig_glb()).expect("rig");
    let text = format!(
        r#"SceneFile(format_version: 1, entities: [
            (guid: "{}", name: "Walker", components: {{
                "{T}": (translation: (5.0, 0.0, 0.0)),
                "regia_render::runtime::MeshSource": (mesh: "assets/meshes/rig.glb", material: ""),
            }}),
        ])"#,
        guid(1)
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    export_scene(&scene, &root, options).expect("export")
}

#[test]
fn skins_and_clips_are_written_when_asked() {
    let options = ExportOptions {
        skins: true,
        animations: true,
        ..ExportOptions::default()
    };
    let (glb, report) = rig_scene_export(&options);
    check_structure(&glb).expect("structure");
    assert_eq!((report.skins, report.animations), (1, 1), "{:?}", report.warnings);
    let r = read(&glb);
    let skin = r.document.skins().next().expect("skin");
    assert_eq!(skin.joints().count(), 2);
    let reader = skin.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    let ibm: Vec<[[f32; 4]; 4]> = reader.read_inverse_bind_matrices().expect("ibm").collect();
    assert_eq!(Mat4::from_cols_array_2d(&ibm[1]).w_axis.y, -2.0);
    let body = node_named(&r.document, "Body");
    assert!(body.skin().is_some());
    // Vertices stay raw (bind space) and the weights survive.
    let prim = body.mesh().expect("mesh").primitives().next().expect("prim");
    let reader = prim.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    let weights: Vec<[f32; 4]> = reader.read_weights(0).expect("weights").into_f32().collect();
    assert_eq!(weights[0], [0.25, 0.75, 0.0, 0.0]);
    let positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
    assert_eq!(positions[1], [0.5, 2.0, 0.0]);
    // The clip drives the knee; times and rotations survive.
    let anim = r.document.animations().next().expect("clip");
    assert_eq!(anim.name(), Some("Walker/Bend"));
    let channel = anim.channels().next().expect("channel");
    assert_eq!(channel.target().node().name(), Some("Knee"));
    let creader = channel.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    assert_eq!(
        creader.read_inputs().expect("times").collect::<Vec<_>>(),
        vec![0.0, 0.5, 1.0]
    );
    // The Knee joint in Regia space: entity * yaw * (Hip * Knee) = (5, 2, 0) with the half turn.
    let worlds = node_worlds(&r.document);
    let knee = yaw() * worlds[node_named(&r.document, "Knee").index()];
    assert!(
        (knee.w_axis.truncate() - Vec3::new(5.0, 2.0, 0.0)).length() < 1e-4,
        "{:?}",
        knee.w_axis
    );
    assert!(
        r.document
            .nodes()
            .any(|n| n.name() == Some(skin::IMPORT_AXIS_NODE_NAME))
    );
}

#[test]
fn without_the_options_a_rig_is_written_as_a_baked_bind_pose() {
    let (glb, report) = rig_scene_export(&ExportOptions::default());
    check_structure(&glb).expect("structure");
    assert_eq!((report.skins, report.animations), (0, 0));
    let r = read(&glb);
    assert!(r.document.skins().next().is_none() && r.document.animations().next().is_none());
    let worlds = node_worlds(&r.document);
    let walker = node_named(&r.document, "Walker");
    let prim = walker.mesh().expect("mesh").primitives().next().expect("prim");
    let reader = prim.reader(|b| Some(r.buffers[b.index()].0.as_slice()));
    let p: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
    // Bind-pose vertex (0.5, 2, 0) is baked with the half turn only (the node's own (9,9,9) is
    // ignored for a skinned node), then placed by the entity.
    let regia = yaw().transform_point3(worlds[walker.index()].transform_point3(Vec3::from_array(p[1])));
    assert!(
        (regia - Vec3::new(5.0 - 0.5, 2.0, 0.0)).length() < 1e-4,
        "{regia:?}"
    );
}

/// sec-review-assets-agent (ADR-5869): a scene's source glTF was parsed with a plain
/// `from_slice` and its buffers loaded from any URI, so a level naming a mesh whose buffer is an
/// absolute path copied that host file into the exported GLB; and the node walks looped on a cycle.
/// The source now goes through the importer's checks.
#[test]
fn a_hostile_source_gltf_is_refused() {
    let dir = temp_project("scene_export_hostile_source");
    let secret = dir.join("secret.bin");
    std::fs::write(&secret, [0_u8; 36]).expect("secret");
    let gltf = |uri: &str, node: &str| {
        format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{node}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}}],
"buffers":[{{"uri":"{uri}","byteLength":36}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[0,0,0]}}]}}"#
        )
    };
    let write = |name: &str, text: String| {
        let path = dir.join("assets/meshes").join(name);
        std::fs::write(&path, text).expect("gltf");
        path
    };
    let absolute = secret.to_string_lossy().replace('\\', "/");
    let absolute = if absolute.starts_with('/') {
        absolute
    } else {
        format!("/{absolute}")
    };
    let escaped = write("abs.gltf", gltf(&absolute, r#"{"mesh":0}"#));
    let err = load_source(&escaped, "abs")
        .err()
        .expect("an absolute buffer URI is refused");
    assert!(
        err.contains("absolute") || err.contains("not a relative path"),
        "{err}"
    );
    let cyclic = write(
        "cycle.gltf",
        gltf("../../secret.bin", r#"{"mesh":0,"children":[0]}"#),
    );
    let err = load_source(&cyclic, "cycle")
        .err()
        .expect("a node cycle is refused");
    assert!(err.contains("own child"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
