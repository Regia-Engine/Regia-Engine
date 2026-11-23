// SPDX-License-Identifier: MPL-2.0
//! Measure a glTF file into a delivery-contract observation (ADR-8450).
//!
//! The measurement is the generic half of a kit check: joint influences, node and socket names,
//! clips, materials, texture edges, the LOD0 footprint and a socket's opening. Class names and
//! numeric caps stay in the `.contract.ron`.

use std::path::Path;

use regia_asset::delivery_contract::{
    DeliveryObservation, MeasuredSkin, MeasuredSocket, MeasuredTexture, SignedAxis,
};

/// Why a glTF file could not be measured.
#[derive(Debug)]
pub struct ObserveError {
    message: String,
}

impl std::fmt::Display for ObserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ObserveError {}

/// Reads `path` (`.gltf` or `.glb`) into the observation [`check_delivery`] judges.
///
/// glTF is Y-up, so `authored_up` is `pos_y`. The forward axis is left unset. A node is a socket
/// when its name starts with `socket_` or its extras name a size, an opening or a bay. An extras
/// size is multiplied by the node's world scale. A socket with no size uses the world bounds of
/// that node and its descendants, and fails closed when those bounds are missing.
///
/// [`check_delivery`]: regia_asset::delivery_contract::check_delivery
///
/// # Errors
///
/// The file cannot be read or is not a glTF document the importer accepts.
pub fn observe_gltf(path: &Path) -> Result<DeliveryObservation, ObserveError> {
    let (document, buffers, images) = gltf::import(path).map_err(|err| ObserveError {
        message: format!("{}: {err}", path.display()),
    })?;
    let worlds = world_matrices(&document);
    let joint_counts = skin_joint_counts(&document);
    let mut observation = DeliveryObservation {
        authored_up: SignedAxis::PosY,
        asset_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        footprint_radius_m: Some(0.0),
        ..DeliveryObservation::default()
    };
    let mut lod0_min = [f64::MAX; 3];
    let mut lod0_max = [f64::MIN; 3];
    let mut saw_lod0 = false;
    let mut radius = 0.0_f64;

    for node in document.nodes() {
        let index = node.index();
        let label = node_label(node.name(), index, "node");
        observation.nodes.push(label.clone());
        let scale = local_scale(&node);
        observation.node_scales.push(scale);
        let world = worlds.get(index).copied().unwrap_or(IDENTITY);
        if let Some(socket) = measure_socket(&node, &label, world, &document, &buffers, &worlds) {
            observation.sockets.push(socket);
        }
        let Some(mesh) = node.mesh() else {
            continue;
        };
        let lod = lod_index(node.name()).or_else(|| lod_index(mesh.name()));
        let mesh_name = node_label(mesh.name(), mesh.index(), "mesh");
        let joint_count = joint_counts.get(mesh.index()).copied().flatten();
        for primitive in mesh.primitives() {
            let reader =
                primitive.reader(|buffer: gltf::Buffer| buffers.get(buffer.index()).map(|data| &data[..]));
            let positions: Vec<[f32; 3]> = reader
                .read_positions()
                .map(|iter| iter.collect())
                .unwrap_or_default();
            let index_count = reader.read_indices().map(|indices| indices.into_u32().count());
            let triangles = triangle_count(primitive.mode(), positions.len(), index_count);
            let key = match lod {
                Some(level) => format!("lod{level}"),
                None => "lod0".to_string(),
            };
            *observation.lod_triangles.entry(key).or_insert(0) += triangles;
            if let Some(skin) = measure_skin(&reader, &mesh_name, primitive.index() as u32, joint_count) {
                observation.skins.push(skin);
            }
            if lod.is_some_and(|level| level >= 1) {
                continue;
            }
            for position in positions {
                let world_position = transform_point(world, position);
                let point = [
                    f64::from(world_position[0]),
                    f64::from(world_position[1]),
                    f64::from(world_position[2]),
                ];
                if !saw_lod0 {
                    lod0_min = point;
                    lod0_max = point;
                    saw_lod0 = true;
                } else {
                    for axis in 0..3 {
                        lod0_min[axis] = lod0_min[axis].min(point[axis]);
                        lod0_max[axis] = lod0_max[axis].max(point[axis]);
                    }
                }
                radius = radius.max(point[0].hypot(point[2]));
            }
        }
    }

    if saw_lod0 {
        observation.bounds_min = lod0_min;
        observation.bounds_max = lod0_max;
        observation.bounds_present = true;
        observation.footprint_radius_m = Some(radius);
    }
    for material in document.materials() {
        let Some(index) = material.index() else {
            continue;
        };
        observation
            .material_names
            .push(node_label(material.name(), index, "material"));
    }
    observation.material_count = observation.material_names.len() as u32;
    for texture in document.textures() {
        let image = images.get(texture.source().index());
        let (width, height) = match image {
            Some(image) if image.width > 0 && image.height > 0 => (Some(image.width), Some(image.height)),
            _ => (None, None),
        };
        observation.textures.push(MeasuredTexture {
            name: node_label(texture.name(), texture.index(), "texture"),
            width,
            height,
        });
    }
    for animation in document.animations() {
        observation
            .clips
            .push(node_label(animation.name(), animation.index(), "clip"));
    }
    Ok(observation)
}

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn world_matrices(document: &gltf::Document) -> Vec<[[f32; 4]; 4]> {
    let mut worlds = vec![IDENTITY; document.nodes().len()];
    let mut seen = vec![false; document.nodes().len()];
    for scene in document.scenes() {
        for node in scene.nodes() {
            walk_world(&node, IDENTITY, &mut worlds, &mut seen);
        }
    }
    for node in document.nodes() {
        if !seen.get(node.index()).copied().unwrap_or(true) {
            worlds[node.index()] = node.transform().matrix();
        }
    }
    worlds
}

fn walk_world(node: &gltf::Node, parent: [[f32; 4]; 4], worlds: &mut [[[f32; 4]; 4]], seen: &mut [bool]) {
    let world = mul(parent, node.transform().matrix());
    let index = node.index();
    if let Some(slot) = worlds.get_mut(index) {
        *slot = world;
    }
    if let Some(flag) = seen.get_mut(index) {
        *flag = true;
    }
    for child in node.children() {
        walk_world(&child, world, worlds, seen);
    }
}

fn mul(parent: [[f32; 4]; 4], local: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0; 4]; 4];
    for col in 0..4 {
        for row in 0..4 {
            out[col][row] = parent[0][row] * local[col][0]
                + parent[1][row] * local[col][1]
                + parent[2][row] * local[col][2]
                + parent[3][row] * local[col][3];
        }
    }
    out
}

fn transform_point(matrix: [[f32; 4]; 4], point: [f32; 3]) -> [f32; 3] {
    [
        matrix[0][0] * point[0] + matrix[1][0] * point[1] + matrix[2][0] * point[2] + matrix[3][0],
        matrix[0][1] * point[0] + matrix[1][1] * point[1] + matrix[2][1] * point[2] + matrix[3][1],
        matrix[0][2] * point[0] + matrix[1][2] * point[1] + matrix[2][2] * point[2] + matrix[3][2],
    ]
}

fn column_len(matrix: [[f32; 4]; 4], column: usize) -> f64 {
    let axis = matrix[column];
    let x = f64::from(axis[0]);
    let y = f64::from(axis[1]);
    let z = f64::from(axis[2]);
    x.hypot(y).hypot(z)
}

fn local_scale(node: &gltf::Node) -> [f64; 3] {
    match node.transform() {
        gltf::scene::Transform::Decomposed { scale, .. } => {
            [f64::from(scale[0]), f64::from(scale[1]), f64::from(scale[2])]
        }
        gltf::scene::Transform::Matrix { matrix } => [
            column_len(matrix, 0),
            column_len(matrix, 1),
            column_len(matrix, 2),
        ],
    }
}

fn node_label(name: Option<&str>, index: usize, kind: &str) -> String {
    match name {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => format!("{kind} {index}"),
    }
}

/// `_LOD` plus digits, case-insensitive. `None` when the name has no such suffix.
fn lod_index(name: Option<&str>) -> Option<u32> {
    let name = name?;
    let lower = name.to_ascii_lowercase();
    let rest = lower.rfind("_lod").map(|index| &lower[index + 4..])?;
    if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn triangle_count(mode: gltf::mesh::Mode, vertices: usize, indices: Option<usize>) -> u64 {
    let count = indices.unwrap_or(vertices);
    let triangles = match mode {
        gltf::mesh::Mode::Triangles => count / 3,
        gltf::mesh::Mode::TriangleStrip | gltf::mesh::Mode::TriangleFan => count.saturating_sub(2),
        _ => 0,
    };
    triangles as u64
}

fn skin_joint_counts(document: &gltf::Document) -> Vec<Option<usize>> {
    let mut counts = vec![None; document.meshes().len()];
    for node in document.nodes() {
        let (Some(mesh), Some(skin)) = (node.mesh(), node.skin()) else {
            continue;
        };
        let count = skin.joints().count();
        if let Some(slot) = counts.get_mut(mesh.index()) {
            *slot = Some(slot.unwrap_or(usize::MAX).min(count));
        }
    }
    counts
}

fn measure_skin<'a, 's, F>(
    reader: &gltf::mesh::Reader<'a, 's, F>,
    mesh: &str,
    primitive: u32,
    joint_count: Option<usize>,
) -> Option<MeasuredSkin>
where
    F: Clone + Fn(gltf::Buffer<'a>) -> Option<&'s [u8]>,
{
    let set0 = influence_set(reader, 0);
    let set1 = influence_set(reader, 1);
    if set0.is_none() && set1.is_none() {
        return None;
    }
    let unpaired = match (&set0, &set1) {
        (Some(first), None) => first.unpaired,
        (None, Some(_)) => true,
        (Some(first), Some(second)) => first.unpaired || second.unpaired,
        (None, None) => false,
    };
    let mut skin = MeasuredSkin {
        mesh: mesh.to_string(),
        primitive,
        influences: 0,
        second_set: set1.is_some(),
        weight_sum: None,
        weight_out_of_range: false,
        joint_out_of_range: false,
        unpaired,
    };
    let vertex_count = set0
        .as_ref()
        .map(|set| set.weights.len())
        .or_else(|| set1.as_ref().map(|set| set.weights.len()))
        .unwrap_or(0);
    let mut most = 0_u32;
    for vertex in 0..vertex_count {
        let mut sum = 0.0_f64;
        let mut used = 0_u32;
        let mut finite = true;
        for set in [set0.as_ref(), set1.as_ref()].into_iter().flatten() {
            if set.unpaired {
                continue;
            }
            let Some(weights) = set.weights.get(vertex) else {
                continue;
            };
            for weight in weights {
                let value = f64::from(*weight);
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    skin.weight_out_of_range = true;
                }
                if !value.is_finite() {
                    finite = false;
                    continue;
                }
                if value.abs() > 1.0e-6 {
                    used += 1;
                }
                sum += value;
            }
            if let Some(joints) = set.joints.get(vertex)
                && let Some(count) = joint_count
                && joints.iter().any(|joint| usize::from(*joint) >= count)
            {
                skin.joint_out_of_range = true;
            }
        }
        most = most.max(used);
        if finite && skin.weight_sum.is_none() && (sum - 1.0).abs() > 0.01 {
            skin.weight_sum = Some(sum);
        }
    }
    skin.influences = most;
    Some(skin)
}

struct InfluenceSet {
    joints: Vec<[u16; 4]>,
    weights: Vec<[f32; 4]>,
    unpaired: bool,
}

fn influence_set<'a, 's, F>(reader: &gltf::mesh::Reader<'a, 's, F>, set: u32) -> Option<InfluenceSet>
where
    F: Clone + Fn(gltf::Buffer<'a>) -> Option<&'s [u8]>,
{
    let joints = reader.read_joints(set);
    let weights = reader.read_weights(set);
    if joints.is_none() && weights.is_none() {
        return None;
    }
    let joints: Vec<[u16; 4]> = joints
        .map(|joints| joints.into_u16().collect())
        .unwrap_or_default();
    let weights = weights.map(collect_weights).unwrap_or_default();
    let unpaired = joints.is_empty() || weights.is_empty() || joints.len() != weights.len();
    Some(InfluenceSet {
        joints,
        weights,
        unpaired,
    })
}

fn collect_weights(weights: gltf::mesh::util::ReadWeights) -> Vec<[f32; 4]> {
    match weights {
        gltf::mesh::util::ReadWeights::U8(iter) => iter
            .map(|weight| weight.map(|component| f32::from(component) / 255.0))
            .collect(),
        gltf::mesh::util::ReadWeights::U16(iter) => iter
            .map(|weight| weight.map(|component| f32::from(component) / 65535.0))
            .collect(),
        gltf::mesh::util::ReadWeights::F32(iter) => iter.collect(),
    }
}

fn measure_socket(
    node: &gltf::Node,
    label: &str,
    world: [[f32; 4]; 4],
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    worlds: &[[[f32; 4]; 4]],
) -> Option<MeasuredSocket> {
    let extras = node_extras(node);
    let named = label.starts_with("socket_");
    let bay = extras.as_ref().is_some_and(|value| value.get("bay").is_some());
    let authored = extras.as_ref().and_then(opening_from_extras);
    if !named && !bay && authored.is_none() {
        return None;
    }
    if let Some((width, height, source)) = authored {
        let scaled_w = width * column_len(world, 0);
        let scaled_h = height * column_len(world, 1);
        if scaled_w.is_finite() && scaled_h.is_finite() {
            return Some(MeasuredSocket {
                name: label.to_string(),
                width_m: Some(scaled_w),
                height_m: Some(scaled_h),
                source: source.to_string(),
            });
        }
    }
    if let Some((width, height)) = subtree_bounds(node, document, buffers, worlds) {
        return Some(MeasuredSocket {
            name: label.to_string(),
            width_m: Some(width),
            height_m: Some(height),
            source: "mesh-bounds".to_string(),
        });
    }
    Some(MeasuredSocket {
        name: label.to_string(),
        width_m: None,
        height_m: None,
        source: "unknown-size".to_string(),
    })
}

fn node_extras(node: &gltf::Node) -> Option<serde_json::Value> {
    let raw = node.extras().as_ref()?.get();
    serde_json::from_str(raw).ok()
}

fn opening_from_extras(extras: &serde_json::Value) -> Option<(f64, f64, &'static str)> {
    for (key, source) in [
        ("size", "extras.size"),
        ("wh", "extras.wh"),
        ("opening", "extras.opening"),
    ] {
        if let Some(pair) = extras.get(key).and_then(pair_of) {
            return Some((pair.0, pair.1, source));
        }
    }
    let width = extras.get("width").and_then(serde_json::Value::as_f64)?;
    let height = extras.get("height").and_then(serde_json::Value::as_f64)?;
    Some((width, height, "extras.size"))
}

fn pair_of(value: &serde_json::Value) -> Option<(f64, f64)> {
    if let Some(array) = value.as_array()
        && array.len() >= 2
    {
        return Some((array[0].as_f64()?, array[1].as_f64()?));
    }
    let width = value.get("width").and_then(serde_json::Value::as_f64)?;
    let height = value.get("height").and_then(serde_json::Value::as_f64)?;
    Some((width, height))
}

fn subtree_bounds(
    node: &gltf::Node,
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    worlds: &[[[f32; 4]; 4]],
) -> Option<(f64, f64)> {
    let mut indices = Vec::new();
    collect_subtree(node, &mut indices);
    let nodes: Vec<gltf::Node> = document.nodes().collect();
    let mut min_x = f64::MAX;
    let mut max_x = f64::MIN;
    let mut min_y = f64::MAX;
    let mut max_y = f64::MIN;
    let mut saw = false;
    for index in indices {
        let Some(scene_node) = nodes.get(index) else {
            continue;
        };
        let Some(mesh) = scene_node.mesh() else {
            continue;
        };
        let world = worlds.get(index).copied().unwrap_or(IDENTITY);
        for primitive in mesh.primitives() {
            let reader =
                primitive.reader(|buffer: gltf::Buffer| buffers.get(buffer.index()).map(|data| &data[..]));
            let Some(positions) = reader.read_positions() else {
                continue;
            };
            for position in positions {
                let point = transform_point(world, position);
                let x = f64::from(point[0]);
                let y = f64::from(point[1]);
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                saw = true;
            }
        }
    }
    if !saw {
        return None;
    }
    Some((max_x - min_x, max_y - min_y))
}

fn collect_subtree(node: &gltf::Node, out: &mut Vec<usize>) {
    out.push(node.index());
    for child in node.children() {
        collect_subtree(&child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::delivery_contract::{DeliveryContract, FootprintRule, SocketClearance, check_delivery};

    fn write_pair(dir: &std::path::Path, stem: &str, json: &str, bin: &[u8]) -> std::path::PathBuf {
        let bin_path = dir.join(format!("{stem}.bin"));
        let gltf_path = dir.join(format!("{stem}.gltf"));
        std::fs::write(&bin_path, bin).expect("bin");
        std::fs::write(&gltf_path, json).expect("gltf");
        gltf_path
    }

    fn vec3_bin(positions: &[[f32; 3]]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for position in positions {
            for component in position {
                bytes.extend(component.to_le_bytes());
            }
        }
        bytes
    }

    /// glTF POSITION accessors must carry min and max or `gltf::import` refuses the file.
    fn mesh_json(node_name: &str, positions: &[[f32; 3]], scale: &str, extras: &str) -> (String, Vec<u8>) {
        let bin = vec3_bin(positions);
        let (min, max) = position_bounds(positions);
        let byte_length = bin.len();
        let json = format!(
            r#"{{
                "asset": {{ "version": "2.0" }},
                "scene": 0,
                "scenes": [{{ "nodes": [0] }}],
                "nodes": [{{ "name": "{node_name}", "mesh": 0, "scale": {scale}{extras} }}],
                "meshes": [{{ "primitives": [{{ "attributes": {{ "POSITION": 0 }} }}] }}],
                "accessors": [{{
                    "bufferView": 0, "componentType": 5126, "count": {count}, "type": "VEC3",
                    "min": [{min0}, {min1}, {min2}],
                    "max": [{max0}, {max1}, {max2}]
                }}],
                "bufferViews": [{{ "buffer": 0, "byteOffset": 0, "byteLength": {byte_length} }}],
                "buffers": [{{ "uri": "mesh.bin", "byteLength": {byte_length} }}]
            }}"#,
            count = positions.len(),
            min0 = min[0],
            min1 = min[1],
            min2 = min[2],
            max0 = max[0],
            max1 = max[1],
            max2 = max[2],
        );
        (json, bin)
    }

    fn position_bounds(positions: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for position in positions {
            min[0] = min[0].min(position[0]);
            min[1] = min[1].min(position[1]);
            min[2] = min[2].min(position[2]);
            max[0] = max[0].max(position[0]);
            max[1] = max[1].max(position[1]);
            max[2] = max[2].max(position[2]);
        }
        (min, max)
    }

    #[test]
    fn a_square_footprint_is_the_lod0_radius_and_lod1_is_left_out() {
        let dir = regia_test_utils::unique_temp_dir("contract-observe-foot");
        let square = [
            [-2.0, 0.0, -2.0],
            [2.0, 0.0, -2.0],
            [2.0, 0.0, 2.0],
            [-2.0, 0.0, 2.0],
        ];
        let (json, bin) = mesh_json("hull", &square, "[1, 1, 1]", "");
        let path = write_pair(&dir, "mesh", &json, &bin);
        let observed = observe_gltf(&path).expect("observe");
        let expected = 2.828_427_124_746_190_3_f64;
        let measured = observed.footprint_radius_m.expect("radius");
        assert!((measured - expected).abs() < 1e-12, "{measured}");
        assert_eq!(observed.lod_triangles.get("lod0").copied(), Some(1));

        let (lod_json, lod_bin) = mesh_json("hull_LOD1", &square, "[1, 1, 1]", "");
        let lod1 = write_pair(&dir, "mesh", &lod_json, &lod_bin);
        let coarse = observe_gltf(&lod1).expect("observe lod1");
        assert_eq!(coarse.footprint_radius_m, Some(0.0));
        assert!(coarse.lod_triangles.contains_key("lod1"));
        assert!(!coarse.lod_triangles.contains_key("lod0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_extras_opening_scales_and_an_unknown_one_fails_closed() {
        let dir = regia_test_utils::unique_temp_dir("contract-observe-open");
        let square = [[0.0, 0.0, 0.0], [0.9, 0.0, 0.0], [0.9, 1.2, 0.0]];
        let extras = r#", "extras": { "size": [0.9, 1.2] }"#;
        let (json, bin) = mesh_json("socket_hatch_main", &square, "[2, 3, 1]", extras);
        let path = write_pair(&dir, "mesh", &json, &bin);
        let observed = observe_gltf(&path).expect("observe");
        let socket = observed
            .sockets
            .iter()
            .find(|socket| socket.name == "socket_hatch_main")
            .expect("socket");
        assert_eq!(socket.source, "extras.size");
        assert!((socket.width_m.expect("w") - 1.8).abs() < 1e-6);
        assert!((socket.height_m.expect("h") - 3.6).abs() < 1e-6);

        let (bare_json, bare_bin) = mesh_json("socket_hatch_bare", &square, "[1, 1, 1]", "");
        let bare = write_pair(&dir, "mesh", &bare_json, &bare_bin);
        let bare_obs = observe_gltf(&bare).expect("observe bare");
        let bare_socket = bare_obs
            .sockets
            .iter()
            .find(|socket| socket.name == "socket_hatch_bare")
            .expect("socket");
        assert_eq!(bare_socket.source, "mesh-bounds");
        assert!((bare_socket.width_m.expect("w") - 0.9).abs() < 1e-6);
        assert!((bare_socket.height_m.expect("h") - 1.2).abs() < 1e-6);

        let empty_path = dir.join("empty.gltf");
        std::fs::write(
            &empty_path,
            r#"{
                "asset": { "version": "2.0" },
                "scene": 0,
                "scenes": [{ "nodes": [0, 1] }],
                "nodes": [
                    { "name": "socket_hatch_empty" },
                    { "name": "bay_port", "extras": { "bay": true } }
                ]
            }"#,
        )
        .expect("empty gltf");
        let empty = observe_gltf(&empty_path).expect("observe empty");
        let missing = empty
            .sockets
            .iter()
            .find(|socket| socket.name == "socket_hatch_empty")
            .expect("named socket");
        assert_eq!(missing.source, "unknown-size");
        assert!(missing.width_m.is_none());
        assert!(empty.sockets.iter().any(|socket| socket.name == "bay_port"));

        let mut contract = contract_ron();
        contract.socket_clearance = vec![SocketClearance {
            pattern: "socket_hatch_empty".into(),
            min_width_m: 0.9,
            min_height_m: 1.2,
        }];
        let report = check_delivery(&contract, &empty).expect("judge");
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "socket_opening" && finding.message.contains("fails closed"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_four_metre_square_passes_a_tenth_suggestion_and_a_short_radius_does_not() {
        let dir = regia_test_utils::unique_temp_dir("contract-observe-radius");
        let square = [
            [-2.0, 0.0, -2.0],
            [2.0, 0.0, -2.0],
            [2.0, 0.0, 2.0],
            [-2.0, 0.0, 2.0],
        ];
        let (json, bin) = mesh_json("hull", &square, "[1, 1, 1]", "");
        let path = write_pair(&dir, "mesh", &json, &bin);
        let observed = observe_gltf(&path).expect("observe");
        let mut contract = contract_ron();
        contract.footprint = Some(FootprintRule {
            radius_m: 2.9,
            tolerance: 0.10,
        });
        let passed = check_delivery(&contract, &observed).expect("judge");
        assert!(
            passed
                .findings
                .iter()
                .all(|finding| finding.code != "footprint_radius"),
            "{:?}",
            passed.findings
        );
        contract.footprint = Some(FootprintRule {
            radius_m: 2.0,
            tolerance: 0.10,
        });
        let failed = check_delivery(&contract, &observed).expect("judge");
        let finding = failed
            .findings
            .iter()
            .find(|finding| finding.code == "footprint_radius")
            .expect("footprint");
        assert!(finding.message.contains("2.9"), "{}", finding.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn contract_ron() -> DeliveryContract {
        let text = r#"(
            format_version: 1,
            class_name: "prop",
            scale: (meters_per_unit: 1.0, identity_tolerance: 0.001),
            axes: (up: pos_y, forward: pos_z),
            origin: (ground_offset_m: 0.0, ground_tolerance_m: 0.05, coarse_floor_rise_m: 0.25, center_fraction: 0.1),
            lod: (required: ["lod0"], budget_lod: "lod0", lod0_min_triangles: 1, lod0_max_triangles: 100000, chain: ["lod0"], require_decreasing: true),
        )"#;
        DeliveryContract::from_ron_str(text).expect("contract")
    }
}
