// SPDX-License-Identifier: MPL-2.0
//! GLB fixtures for the collision-proxy tests (ADR-2341): named root nodes with box and octahedron
//! meshes. Shared by `collision_proxies.rs` and `gltf_no_proxy_identity.rs`.

/// One fixture node: a name, a translation and an optional triangle mesh.
pub struct Node {
    pub name: &'static str,
    pub translation: [f32; 3],
    pub mesh: Option<(Vec<[f32; 3]>, Vec<u32>)>,
}

/// The corners of the box `lo..hi` and its twelve outward triangles.
pub fn cuboid(lo: [f32; 3], hi: [f32; 3]) -> (Vec<[f32; 3]>, Vec<u32>) {
    let corners = (0..8u32)
        .map(|i| {
            [
                if i & 1 == 0 { lo[0] } else { hi[0] },
                if i & 2 == 0 { lo[1] } else { hi[1] },
                if i & 4 == 0 { lo[2] } else { hi[2] },
            ]
        })
        .collect();
    let faces = [
        [0, 2, 3, 1],
        [4, 5, 7, 6],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 4, 6, 2],
        [1, 3, 7, 5],
    ];
    let indices = faces
        .iter()
        .flat_map(|[a, b, c, d]| [*a, *b, *c, *a, *c, *d])
        .collect();
    (corners, indices)
}

/// An octahedron of `radius` about the origin.
pub fn octahedron(radius: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let r = radius;
    let v = vec![
        [r, 0.0, 0.0],
        [-r, 0.0, 0.0],
        [0.0, r, 0.0],
        [0.0, -r, 0.0],
        [0.0, 0.0, r],
        [0.0, 0.0, -r],
    ];
    let i = vec![
        0, 2, 4, 4, 2, 1, 1, 2, 5, 5, 2, 0, 4, 3, 0, 1, 3, 4, 5, 3, 1, 0, 3, 5,
    ];
    (v, i)
}

/// A GLB 2.0 holding `nodes` as scene roots, one mesh per node with a mesh.
pub fn glb(nodes: &[Node]) -> Vec<u8> {
    let mut bin: Vec<u8> = Vec::new();
    let (mut accessors, mut views, mut meshes, mut gltf_nodes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for node in nodes {
        let mut entry = format!(
            r#""name":"{}","translation":[{},{},{}]"#,
            node.name, node.translation[0], node.translation[1], node.translation[2]
        );
        if let Some((positions, indices)) = &node.mesh {
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            let start = bin.len();
            for p in positions {
                for k in 0..3 {
                    lo[k] = lo[k].min(p[k]);
                    hi[k] = hi[k].max(p[k]);
                    bin.extend_from_slice(&p[k].to_le_bytes());
                }
            }
            views.push(format!(
                r#"{{"buffer":0,"byteOffset":{start},"byteLength":{},"target":34962}}"#,
                bin.len() - start
            ));
            accessors.push(format!(
                r#"{{"bufferView":{},"componentType":5126,"count":{},"type":"VEC3","min":[{},{},{}],"max":[{},{},{}]}}"#,
                views.len() - 1,
                positions.len(),
                lo[0],
                lo[1],
                lo[2],
                hi[0],
                hi[1],
                hi[2]
            ));
            let start = bin.len();
            for i in indices {
                bin.extend_from_slice(&i.to_le_bytes());
            }
            views.push(format!(
                r#"{{"buffer":0,"byteOffset":{start},"byteLength":{},"target":34963}}"#,
                bin.len() - start
            ));
            accessors.push(format!(
                r#"{{"bufferView":{},"componentType":5125,"count":{},"type":"SCALAR"}}"#,
                views.len() - 1,
                indices.len()
            ));
            meshes.push(format!(
                r#"{{"primitives":[{{"attributes":{{"POSITION":{}}},"indices":{},"mode":4}}]}}"#,
                accessors.len() - 2,
                accessors.len() - 1
            ));
            entry.push_str(&format!(r#","mesh":{}"#, meshes.len() - 1));
        }
        gltf_nodes.push(format!("{{{entry}}}"));
    }
    let roots: Vec<String> = (0..nodes.len()).map(|i| i.to_string()).collect();
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[{}]}}],"nodes":[{}],"meshes":[{}],"accessors":[{}],"bufferViews":[{}],"buffers":[{{"byteLength":{}}}]}}"#,
        roots.join(","),
        gltf_nodes.join(","),
        meshes.join(","),
        accessors.join(","),
        views.join(","),
        bin.len()
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&((12 + 8 + json.len() + 8 + bin.len()) as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}
