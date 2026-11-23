// SPDX-License-Identifier: MPL-2.0
//! GLB in and out for the cook: read one node's geometry from a part kit's model, and write the
//! stand-in kit's model (one node and mesh per part, embedded buffer, glTF 2.0 binary container
//! as in the Khronos glTF 2.0 specification § 4.4).

use glam::{Mat3, Mat4, Vec3};
use serde_json::json;

use crate::PackError;
use crate::mesh::MeshData;

/// Largest GLB the cook reads.
pub const MAX_GLB_BYTES: usize = 256 << 20;

fn pad4(v: &mut Vec<u8>, with: u8) {
    while !v.len().is_multiple_of(4) {
        v.push(with);
    }
}

/// Write meshes as a GLB: node and mesh `i` are named after `meshes[i].name`, one primitive each
/// (positions, normals, u32 indices, and `TEXCOORD_0` when the mesh has UVs), one material per
/// distinct `material_slot`.
pub fn write(meshes: &[MeshData], generator: &str) -> Result<Vec<u8>, PackError> {
    let mut bin: Vec<u8> = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut gl_meshes = Vec::new();
    let mut nodes = Vec::new();
    let mut materials: Vec<String> = Vec::new();
    for (i, m) in meshes.iter().enumerate() {
        m.validate()?;
        let (lo, hi) = m.bounds();
        let mut add = |data: Vec<u8>, target: u32| {
            let offset = bin.len();
            bin.extend_from_slice(&data);
            pad4(&mut bin, 0);
            views
                .push(json!({"buffer": 0, "byteOffset": offset, "byteLength": data.len(), "target": target}));
            views.len() - 1
        };
        let pos: Vec<u8> = m
            .positions
            .iter()
            .flatten()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let nrm: Vec<u8> = m.normals.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
        let idx: Vec<u8> = m.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        let vp = add(pos, 34962);
        let vn = add(nrm, 34962);
        // No TEXCOORD view when the mesh has no UVs, so a stand-in GLB stays byte-identical.
        let vu = m.uvs.as_ref().map(|uvs| {
            let data: Vec<u8> = uvs.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
            add(data, 34962)
        });
        let vi = add(idx, 34963);
        let a0 = accessors.len();
        accessors.push(
            json!({"bufferView": vp, "componentType": 5126, "count": m.positions.len(), "type": "VEC3",
                              "min": lo.to_array(), "max": hi.to_array()}),
        );
        accessors
            .push(json!({"bufferView": vn, "componentType": 5126, "count": m.normals.len(), "type": "VEC3"}));
        if let (Some(vu), Some(uvs)) = (vu, m.uvs.as_ref()) {
            accessors
                .push(json!({"bufferView": vu, "componentType": 5126, "count": uvs.len(), "type": "VEC2"}));
        }
        accessors.push(
            json!({"bufferView": vi, "componentType": 5125, "count": m.indices.len(), "type": "SCALAR"}),
        );
        let mat = match materials.iter().position(|s| *s == m.material_slot) {
            Some(k) => k,
            None => {
                materials.push(m.material_slot.clone());
                materials.len() - 1
            }
        };
        let primitive = if vu.is_some() {
            json!({"attributes": {"POSITION": a0, "NORMAL": a0 + 1, "TEXCOORD_0": a0 + 2},
                   "indices": a0 + 3, "material": mat})
        } else {
            json!({"attributes": {"POSITION": a0, "NORMAL": a0 + 1}, "indices": a0 + 2, "material": mat})
        };
        gl_meshes.push(json!({"name": m.name, "primitives": [primitive]}));
        nodes.push(json!({"name": m.name, "mesh": i}));
    }
    let doc = json!({
        "asset": {"version": "2.0", "generator": generator},
        "scene": 0,
        "scenes": [{"nodes": (0..meshes.len()).collect::<Vec<_>>()}],
        "nodes": nodes,
        "meshes": gl_meshes,
        "materials": materials.iter().map(|s| json!({"name": s, "pbrMetallicRoughness":
            {"baseColorFactor": [0.8, 0.8, 0.8, 1.0], "metallicFactor": 1.0, "roughnessFactor": 0.3}})).collect::<Vec<_>>(),
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{"byteLength": bin.len()}],
    });
    let mut js = serde_json::to_vec(&doc).map_err(|e| PackError::new("pack_io", e.to_string()))?;
    pad4(&mut js, b' ');
    let total = 12 + 8 + js.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(js.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&js);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    Ok(out)
}

/// Every node name in a GLB.
pub fn node_names(glb: &[u8]) -> Result<Vec<String>, PackError> {
    let g = parse(glb)?;
    Ok(g.document
        .nodes()
        .filter_map(|n| n.name().map(str::to_string))
        .collect())
}

fn parse(glb: &[u8]) -> Result<gltf::Gltf, PackError> {
    if glb.len() > MAX_GLB_BYTES {
        return Err(PackError::new(
            "pack_too_large",
            format!("GLB is {} bytes", glb.len()),
        ));
    }
    checked_gltf(glb).map_err(|e| PackError::new("pack_bad_kit", format!("GLB: {e}")))
}

/// `gltf::Gltf::from_slice` without what gltf 1.4 leaves to panics (ADR-5869): a GLB whose declared
/// length is below its 12-byte header underflows; validation reads `accessors[POSITION]` before it
/// checks the index; the readers slice views and accessors unchecked, assert each attribute's type,
/// and reach `unreachable!` on a component type glTF does not allow; and an accessor without a
/// view is `count` zeros backed by no bytes (sparse accessors are refused where this reader reads). A kit GLB comes from outside the engine, and under
/// `panic = "abort"` any of these ends the process. The same rules as the asset pipeline's checked
/// parse, for the attributes this reader uses (this crate also builds for the web player, so it
/// cannot depend on the pipeline).
fn checked_gltf(glb: &[u8]) -> Result<gltf::Gltf, String> {
    use gltf::json::accessor::{ComponentType as C, Type};
    use gltf::json::validation::Checked;
    if glb.starts_with(b"glTF") {
        let length = glb
            .get(8..12)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or("GLB header is truncated")?;
        if length < 12 {
            return Err(format!("GLB header declares length {length}"));
        }
    }
    let gltf::Gltf { document, blob } =
        gltf::Gltf::from_slice_without_validation(glb).map_err(|e| e.to_string())?;
    let json = document.into_json();
    for (v, view) in json.buffer_views.iter().enumerate() {
        let end = view
            .byte_offset
            .map_or(0, |o| o.0)
            .checked_add(view.byte_length.0);
        let buffer = json.buffers.get(view.buffer.value()).map(|b| b.byte_length.0);
        if end.is_none_or(|end| buffer.is_none_or(|len| end > len)) {
            return Err(format!("buffer view {v} reaches past its buffer"));
        }
    }
    for (i, a) in json.accessors.iter().enumerate() {
        let (Checked::Valid(kind), Checked::Valid(component)) = (&a.type_, &a.component_type) else {
            return Err(format!("accessor {i}: invalid type or component type"));
        };
        let elem = (component.0.size() * kind.multiplicity()) as u64;
        let n = a.count.0;
        // Without a view (and without sparse values, refused below for what this reader reads)
        // the readers answer `None`.
        let Some(view) = a.buffer_view else {
            continue;
        };
        let view = json
            .buffer_views
            .get(view.value())
            .ok_or_else(|| format!("accessor {i}: no buffer view {}", view.value()))?;
        let stride = view.byte_stride.map_or(elem, |s| s.0 as u64);
        let fits = n > 0
            && stride >= elem
            && stride
                .checked_mul(n - 1)
                .and_then(|b| b.checked_add(elem))
                .and_then(|b| b.checked_add(a.byte_offset.map_or(0, |o| o.0)))
                .is_some_and(|end| end <= view.byte_length.0);
        if !fits {
            return Err(format!(
                "accessor {i} reaches past its buffer view (or has no elements)"
            ));
        }
    }
    let accessor = |index: usize| json.accessors.get(index);
    for (m, mesh) in json.meshes.iter().enumerate() {
        for (p, primitive) in mesh.primitives.iter().enumerate() {
            let mut uses: Vec<(usize, &[Type], &[C])> = Vec::new();
            for (semantic, index) in &primitive.attributes {
                match semantic {
                    Checked::Valid(gltf::Semantic::Positions | gltf::Semantic::Normals) => {
                        uses.push((index.value(), &[Type::Vec3], &[C::F32]));
                    }
                    Checked::Valid(gltf::Semantic::TexCoords(_)) => {
                        uses.push((index.value(), &[Type::Vec2], &[C::U8, C::U16, C::F32]));
                    }
                    _ => {}
                }
            }
            if let Some(indices) = primitive.indices {
                uses.push((indices.value(), &[Type::Scalar], &[C::U8, C::U16, C::U32]));
            }
            for (index, types, components) in uses {
                // Sparse values are not read by this reader's callers' kits; their index and value
                // views go unchecked by the readers, and without a base view their `count` zeros
                // are backed by no bytes, so they are refused.
                let ok = accessor(index).is_some_and(|a| {
                    a.sparse.is_none()
                        && matches!(&a.type_, Checked::Valid(t) if types.contains(t))
                        && matches!(&a.component_type, Checked::Valid(c) if components.contains(&c.0))
                });
                if !ok {
                    return Err(format!(
                        "mesh {m} primitive {p}: accessor {index} is missing or has the wrong type for its use"
                    ));
                }
            }
        }
    }
    let document = gltf::Document::from_json(json).map_err(|e| e.to_string())?;
    Ok(gltf::Gltf { document, blob })
}

/// Read the geometry of the node named `node` (and its children) in that node's frame, merged into one
/// mesh named `name` with material slot `slot`. Buffers must be embedded (an outside file is
/// refused); triangle lists only.
pub fn read_node(glb: &[u8], node: &str, name: &str, slot: &str) -> Result<MeshData, PackError> {
    let g = parse(glb)?;
    let blob = g.blob.as_deref();
    // World transforms: walk every scene from its roots.
    let mut world: Vec<Option<Mat4>> = vec![None; g.document.nodes().len()];
    fn walk(n: gltf::Node, parent: Mat4, world: &mut [Option<Mat4>], depth: usize) {
        // First visit wins. A node that lists itself (or a cycle through children) is not walked again.
        if depth > 64 || world[n.index()].is_some() {
            return;
        }
        let m = parent * Mat4::from_cols_array_2d(&n.transform().matrix());
        world[n.index()] = Some(m);
        for c in n.children() {
            walk(c, m, world, depth + 1);
        }
    }
    for s in g.document.scenes() {
        for n in s.nodes() {
            walk(n, Mat4::IDENTITY, &mut world, 0);
        }
    }
    let target = g
        .document
        .nodes()
        .find(|n| n.name() == Some(node))
        .ok_or_else(|| PackError::new("pack_bad_kit", format!("the GLB has no node {node:?}")))?;
    // The node's own frame is the part's frame: geometry below it is read relative to it.
    let root = world[target.index()].unwrap_or(Mat4::IDENTITY);
    let to_part = root.inverse();
    let mut out = MeshData {
        name: name.into(),
        material_slot: slot.into(),
        ..MeshData::default()
    };
    let mut seen = vec![false; g.document.nodes().len()];
    let mut stack = vec![(target, 0usize)];
    while let Some((n, depth)) = stack.pop() {
        if depth > 64 || seen[n.index()] {
            continue;
        }
        seen[n.index()] = true;
        let m = to_part * world[n.index()].unwrap_or(Mat4::IDENTITY);
        let nm = Mat3::from_mat4(m).inverse().transpose();
        if let Some(mesh) = n.mesh() {
            for p in mesh.primitives() {
                if p.mode() != gltf::mesh::Mode::Triangles {
                    return Err(PackError::new(
                        "pack_bad_kit",
                        format!("node {node:?}: only triangle lists"),
                    ));
                }
                let reader = p.reader(|b| match b.source() {
                    gltf::buffer::Source::Bin => blob,
                    gltf::buffer::Source::Uri(_) => None,
                });
                let pos: Vec<[f32; 3]> = reader
                    .read_positions()
                    .ok_or_else(|| {
                        PackError::new(
                            "pack_bad_kit",
                            format!("node {node:?}: no positions (or an outside buffer)"),
                        )
                    })?
                    .collect();
                let nrm: Vec<[f32; 3]> = match reader.read_normals() {
                    Some(n) => n.collect(),
                    None => vec![[0.0, 0.0, 1.0]; pos.len()],
                };
                let idx: Vec<u32> = match reader.read_indices() {
                    Some(i) => i.into_u32().collect(),
                    None => (0..pos.len() as u32).collect(),
                };
                if nrm.len() != pos.len() || idx.iter().any(|&i| i as usize >= pos.len()) {
                    return Err(PackError::new(
                        "pack_bad_kit",
                        format!("node {node:?}: inconsistent primitive"),
                    ));
                }
                let uvs = match reader.read_tex_coords(0) {
                    Some(c) => {
                        let uvs: Vec<[f32; 2]> = c.into_f32().collect();
                        if uvs.len() != pos.len() {
                            return Err(PackError::new(
                                "pack_bad_kit",
                                format!("node {node:?}: texture coordinates do not match positions"),
                            ));
                        }
                        Some(uvs)
                    }
                    None => None,
                };
                let base = out.positions.len() as u32;
                match (&mut out.uvs, uvs) {
                    (slot, Some(uvs)) => {
                        let dst = slot.get_or_insert_with(|| vec![[0.0, 0.0]; out.positions.len()]);
                        dst.extend(uvs);
                    }
                    (Some(dst), None) => dst.resize(out.positions.len() + pos.len(), [0.0, 0.0]),
                    (None, None) => {}
                }
                out.positions
                    .extend(pos.iter().map(|p| m.transform_point3(Vec3::from(*p)).to_array()));
                out.normals.extend(
                    nrm.iter()
                        .map(|n| (nm * Vec3::from(*n)).normalize_or(Vec3::Z).to_array()),
                );
                out.indices.extend(idx.iter().map(|i| base + i));
            }
        }
        stack.extend(n.children().map(|c| (c, depth + 1)));
    }
    if out.indices.is_empty() {
        return Err(PackError::new(
            "pack_bad_kit",
            format!("node {node:?} has no triangles"),
        ));
    }
    out.validate()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom;

    #[test]
    fn write_then_read_a_node() {
        let a = geom::ball("ball", 3.0, geom::Detail::Fine);
        let b = geom::post("post", 1.2, 8.0, geom::Detail::Cheap);
        let glb = write(&[a.clone(), b.clone()], "test").expect("ok");
        assert_eq!(node_names(&glb).expect("ok"), vec!["ball", "post"]);
        let back = read_node(&glb, "post", "part:post", "body").expect("ok");
        assert_eq!(back.positions, b.positions);
        assert_eq!(back.indices, b.indices);
        assert!(read_node(&glb, "nope", "x", "y").is_err());
        assert!(read_node(b"glTF junk", "post", "x", "y").is_err());
    }

    /// Rebuild a GLB this crate wrote, with one JSON edit, keeping the BIN chunk.
    fn repack(glb: &[u8], edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
        let json_len = u32::from_le_bytes(glb[12..16].try_into().expect("len")) as usize;
        // The JSON chunk is space-padded; trim the padding before parsing.
        let raw = &glb[20..20 + json_len];
        let end = raw.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);
        let mut doc: serde_json::Value = serde_json::from_slice(&raw[..end]).expect("json");
        edit(&mut doc);
        let mut js = serde_json::to_vec(&doc).expect("json");
        while !js.len().is_multiple_of(4) {
            js.push(b' ');
        }
        let bin_off = 20 + json_len;
        let bin_len = u32::from_le_bytes(glb[bin_off..bin_off + 4].try_into().expect("bin")) as usize;
        let bin = &glb[bin_off + 8..bin_off + 8 + bin_len];
        let total = 12 + 8 + js.len() + 8 + bin.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(js.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&js);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(bin);
        out
    }

    /// A node that lists itself as a child is one part, not a copy per depth step.
    #[test]
    fn audit_cyclic_node_is_read_once() {
        let part = geom::post("post", 1.2, 8.0, geom::Detail::Cheap);
        let glb = repack(&write(std::slice::from_ref(&part), "test").expect("glb"), |doc| {
            doc["nodes"][0]["children"] = serde_json::json!([0]);
        });
        let back = read_node(&glb, "post", "part:post", "body").expect("read");
        assert_eq!(back.triangles(), part.triangles());
        assert_eq!(back.positions.len(), part.positions.len());
    }

    /// Texture coordinates on a part survive the GLB the cook writes and reads.
    #[test]
    fn audit_texture_coordinates_round_trip() {
        let mut part = geom::post("post", 1.2, 8.0, geom::Detail::Cheap);
        part.uvs = Some(
            (0..part.positions.len())
                .map(|i| [i as f32 / part.positions.len() as f32, 0.25])
                .collect(),
        );
        let glb = write(std::slice::from_ref(&part), "test").expect("glb");
        let back = read_node(&glb, "post", "part:post", "body").expect("read");
        let uvs = back.uvs.expect("uvs");
        assert_eq!(uvs.len(), part.positions.len());
        for (a, b) in part.uvs.as_ref().expect("written uvs").iter().zip(&uvs) {
            assert!(
                (a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5,
                "{a:?} {b:?}"
            );
        }
    }

    /// sec-review-assets-agent (ADR-5869): a kit GLB was parsed with gltf 1.4's `from_slice`, whose
    /// validation indexes `accessors[POSITION]` unchecked, and read with readers that slice views,
    /// assert attribute types and reach `unreachable!` on a component type glTF does not allow.
    /// Each case is now `pack_bad_kit`, not a panic (an abort under `panic = "abort"`).
    #[test]
    fn hostile_kit_glbs_are_errors_not_panics() {
        let part = geom::post("post", 1.2, 8.0, geom::Detail::Cheap);
        let good = write(std::slice::from_ref(&part), "test").expect("glb");
        type Edit = fn(&mut serde_json::Value);
        let cases: [(&str, Edit); 5] = [
            ("POSITION past the accessors", |doc| {
                doc["meshes"][0]["primitives"][0]["attributes"]["POSITION"] = serde_json::json!(99);
            }),
            ("u32 texture coordinates", |doc| {
                let uv = doc["accessors"].as_array().expect("accessors").len();
                let mut accessor = doc["accessors"][0].clone();
                accessor["type"] = serde_json::json!("VEC2");
                accessor["componentType"] = serde_json::json!(5125);
                accessor.as_object_mut().expect("object").remove("min");
                accessor.as_object_mut().expect("object").remove("max");
                doc["accessors"].as_array_mut().expect("accessors").push(accessor);
                doc["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_0"] = serde_json::json!(uv);
            }),
            ("an accessor past its view", |doc| {
                doc["accessors"][0]["count"] = serde_json::json!(1_000_000_000u64);
            }),
            ("a stride below the element size", |doc| {
                doc["bufferViews"][0]["byteStride"] = serde_json::json!(4);
            }),
            ("normals read as VEC2", |doc| {
                doc["accessors"][1]["type"] = serde_json::json!("VEC2");
            }),
        ];
        for (what, edit) in cases {
            let glb = repack(&good, edit);
            let result = std::panic::catch_unwind(|| read_node(&glb, "post", "part:post", "body"));
            match result {
                Ok(Err(e)) => assert_eq!(e.code, "pack_bad_kit", "{what}: {e:?}"),
                Ok(Ok(_)) => panic!("{what}: a hostile kit GLB was read"),
                Err(_) => panic!("{what}: the reader panicked"),
            }
        }
        // A GLB whose header declares a length below its own 12 bytes.
        let mut short = good.clone();
        short[8..12].copy_from_slice(&4u32.to_le_bytes());
        assert!(std::panic::catch_unwind(|| node_names(&short)).is_ok_and(|r| r.is_err()));
    }

    /// `iterations` seeded byte mutations of a kit GLB (bit flips, hostile words, truncation,
    /// spliced windows) through `node_names` and `read_node`; every one must return. Returns how
    /// many read and how many were refused.
    fn soak(iterations: u64, seed: u64) -> (u64, u64) {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let parts = [
            geom::post("post", 1.2, 8.0, geom::Detail::Cheap),
            geom::ball("ball", 3.0, geom::Detail::Cheap),
        ];
        let good = write(&parts, "soak").expect("glb");
        let (mut ok, mut refused) = (0_u64, 0_u64);
        for i in 0..iterations {
            let mut bytes = good.clone();
            let len = bytes.len() as u64;
            match next() % 4 {
                0 => {
                    for _ in 0..1 + next() % 8 {
                        let at = (next() % len) as usize;
                        bytes[at] ^= 1 << (next() % 8);
                    }
                }
                1 => {
                    let at = (next() % (len - 4)) as usize & !3;
                    let word: u32 = [0, 1, 0x7FFF_FFFF, 0xFFFF_FFFF, 0x8000_0000, 12][(next() % 6) as usize];
                    bytes[at..at + 4].copy_from_slice(&word.to_le_bytes());
                }
                2 => bytes.truncate((next() % len) as usize),
                _ => {
                    let from = (next() % len) as usize;
                    let to = (next() % len) as usize;
                    let n = ((next() % 64) as usize).min(bytes.len() - from.max(to));
                    let window = bytes[from..from + n].to_vec();
                    bytes[to..to + n].copy_from_slice(&window);
                }
            }
            let outcome = std::panic::catch_unwind(|| {
                let _ = node_names(&bytes);
                read_node(&bytes, "post", "part:post", "body").is_ok()
            });
            match outcome {
                Ok(true) => ok += 1,
                Ok(false) => refused += 1,
                Err(_) => panic!("kit GLB soak: mutation {i} panicked"),
            }
        }
        (ok, refused)
    }

    /// w7-runtime-smalls-z item 5: a bounded run of the soak below that the gate runs (fixed seed,
    /// 2,000 mutations, the same mutators). The long soak stays ignored.
    #[test]
    fn kit_glb_bounded_soak() {
        let (ok, refused) = soak(2_000, 1);
        assert_eq!(ok + refused, 2_000);
        assert!(
            ok > 0 && refused > 0,
            "read {ok}, refused {refused}: the mutators reach both outcomes"
        );
    }

    /// The v0.5.0 input-path review's soak (ADR-5869): `REGIA_FUZZ_SOAK_ITERATIONS` (default
    /// 20 000) seeded mutations through `soak`. Ignored in the gate (`kit_glb_bounded_soak` runs
    /// there);
    /// `cargo test -p regia_viewer_pack --features cook --lib -- --ignored kit_glb_soak --nocapture`.
    #[test]
    #[ignore = "long soak; run by hand"]
    fn kit_glb_soak() {
        let iterations: u64 = std::env::var("REGIA_FUZZ_SOAK_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000);
        let seed: u64 = std::env::var("REGIA_FUZZ_SOAK_FIRST_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let (ok, refused) = soak(iterations, seed);
        eprintln!("kit GLB soak: {iterations} mutations, read {ok}, refused {refused}");
    }
}
