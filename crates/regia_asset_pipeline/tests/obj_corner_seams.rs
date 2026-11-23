// SPDX-License-Identifier: MPL-2.0
//! Semantic OBJ corner identity checks through conversion and the public GLB importer.

use regia_asset_pipeline::obj_import::{ObjImportSettings, convert_obj_to_glb};
use regia_asset_pipeline::{Forward, import_gltf};

type Corner = ([i64; 3], [i64; 2], [i64; 3]);
type Triangle = [Corner; 3];

fn q3(v: [f32; 3]) -> [i64; 3] {
    v.map(|x| (x * 10_000.0).round() as i64)
}

fn q2(v: [f32; 2]) -> [i64; 2] {
    v.map(|x| (x * 10_000.0).round() as i64)
}

fn canonical(mut triangle: Triangle) -> Triangle {
    let first = (0..3).min_by_key(|&i| triangle[i]).unwrap_or(0);
    triangle.rotate_left(first);
    triangle
}

fn actual(text: &str) -> (Vec<Triangle>, Vec<u8>) {
    let conversion =
        convert_obj_to_glb(text.as_bytes(), None, &ObjImportSettings::default()).expect("OBJ conversion");
    let mesh = import_gltf(&conversion.glb, Forward::Keep).expect("GLB import");
    assert_eq!(mesh.positions.len(), mesh.uvs.len());
    assert_eq!(mesh.positions.len(), mesh.normals.len());
    let mut triangles: Vec<Triangle> = mesh
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|indices| {
            let corners: [u32; 3] = *indices;
            canonical(corners.map(|index| {
                let i = index as usize;
                (q3(mesh.positions[i]), q2(mesh.uvs[i]), q3(mesh.normals[i]))
            }))
        })
        .collect();
    // The cook may reorder triangles (vertex-cache and overdraw passes): compare as a set.
    triangles.sort();
    (triangles, conversion.glb)
}

fn expected(triangles: &[Triangle]) -> Vec<Triangle> {
    let mut triangles: Vec<_> = triangles.iter().copied().map(canonical).collect();
    triangles.sort();
    triangles
}

fn assert_semantics(text: &str, expected_triangles: &[Triangle]) -> Vec<u8> {
    let (got, glb) = actual(text);
    assert_eq!(
        got,
        expected(expected_triangles),
        "corner correspondence or winding"
    );
    let (again, second_glb) = actual(text);
    assert_eq!(glb, second_glb, "conversion bytes are deterministic");
    assert_eq!(got, again, "decoded semantics are deterministic");
    glb
}

const POS: [[i64; 3]; 4] = [[0, 0, 0], [10000, 0, 0], [0, 10000, 0], [10000, 10000, 0]];
const N: [[i64; 3]; 2] = [[0, 0, 10000], [0, 10000, 0]];
const UV: [[i64; 2]; 4] = [[0, 10000], [10000, 10000], [0, 0], [10000, 0]];

fn c(p: usize, uv: usize, n: usize) -> Corner {
    (POS[p], UV[uv], N[n])
}

#[test]
fn neighboring_faces_keep_distinct_uv_and_normal_corner_indices() {
    let text = r#"
v 0 0 0
v 1 0 0
v 0 1 0
v 1 1 0
vt 0 0
vt 1 0
vt 0 1
vt 1 1
vn 0 0 1
vn 0 1 0
f 1/1/1 2/2/1 3/3/1
f 2/4/2 4/2/2 3/3/2
"#;
    assert_semantics(
        text,
        &[
            [c(0, 0, 0), c(1, 1, 0), c(2, 2, 0)],
            [c(1, 3, 1), c(3, 1, 1), c(2, 2, 1)],
        ],
    );
}

/// Two `v` lines at the same coordinates are two OBJ position indices. Where two faces meet at
/// that point, each face keeps its own `vt` at the corner it names: `v 2` carries `vt 2` in the
/// first face and `v 5` (also at `1 0 0`) carries `vt 4` in the second. The faces do not share
/// three positions, so coincident-triangle cleanup does not apply.
#[test]
fn coincident_positions_with_distinct_obj_indices_remain_distinct_corners() {
    let text = r#"
v 0 0 0
v 1 0 0
v 0 1 0
v 1 1 0
v 1 0 0
vt 0 0
vt 1 0
vt 0 1
vt 1 1
vn 0 0 1
f 1/1/1 2/2/1 3/3/1
f 5/4/1 4/2/1 3/3/1
"#;
    assert_semantics(
        text,
        &[
            [c(0, 0, 0), c(1, 1, 0), c(2, 2, 0)],
            [c(1, 3, 0), c(3, 1, 0), c(2, 2, 0)],
        ],
    );
}

#[test]
fn positive_and_equivalent_negative_corner_references_decode_identically() {
    let positive = r#"
v 0 0 0
v 1 0 0
v 0 1 0
vt 0 0
vt 1 0
vt 0 1
vn 0 0 1
f 1/1/1 2/2/1 3/3/1
"#;
    let negative = r#"
v 0 0 0
v 1 0 0
v 0 1 0
vt 0 0
vt 1 0
vt 0 1
vn 0 0 1
f -3/-3/-1 -2/-2/-1 -1/-1/-1
"#;
    let expected = [[c(0, 0, 0), c(1, 1, 0), c(2, 2, 0)]];
    let pos = assert_semantics(positive, &expected);
    let neg = assert_semantics(negative, &expected);
    assert_eq!(pos, neg);
}

#[test]
fn two_material_primitives_share_edge_without_crossing_face_attributes() {
    let text = r#"
v 0 0 0
v 1 0 0
v 0 1 0
v 1 1 0
vt 0 0
vt 1 0
vt 0 1
vt 1 1
vn 0 0 1
vn 0 1 0
usemtl left
f 1/1/1 2/2/1 3/3/1
usemtl right
f 2/4/2 4/2/2 3/3/2
"#;
    // `import_gltf` unpacks the first primitive only (the `usemtl left` face).
    let glb = assert_semantics(text, &[[c(0, 0, 0), c(1, 1, 0), c(2, 2, 0)]]);
    let doc = gltf::Gltf::from_slice(&glb).expect("valid GLB");
    let primitives: Vec<_> = doc.meshes().next().expect("mesh").primitives().collect();
    assert_eq!(primitives.len(), 2);
    assert!(
        primitives
            .iter()
            .all(|primitive| primitive.material().index().is_none())
    );
    // The `usemtl right` face, read from the GLB: its corners keep their own `vt` and `vn` on the
    // edge it shares with the first face.
    assert_eq!(
        glb_triangles(&doc, &primitives[1]),
        expected(&[[c(1, 3, 1), c(3, 1, 1), c(2, 2, 1)]])
    );
}

/// One GLB primitive's triangles, canonical and sorted like [`expected`].
fn glb_triangles(doc: &gltf::Gltf, primitive: &gltf::Primitive<'_>) -> Vec<Triangle> {
    let blob = doc.blob.as_deref();
    let reader = primitive.reader(|buffer| match buffer.source() {
        gltf::buffer::Source::Bin => blob,
        gltf::buffer::Source::Uri(_) => None,
    });
    let positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
    let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).expect("uv0").into_f32().collect();
    let normals: Vec<[f32; 3]> = reader.read_normals().expect("normals").collect();
    let indices: Vec<u32> = reader.read_indices().expect("indices").into_u32().collect();
    let mut triangles: Vec<Triangle> = indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|corner| {
            let corners: [u32; 3] = *corner;
            canonical(corners.map(|index| {
                let i = index as usize;
                (q3(positions[i]), q2(uvs[i]), q3(normals[i]))
            }))
        })
        .collect();
    triangles.sort();
    triangles
}

/// A quad fans into `(1, 2, 3)` and `(1, 3, 4)`, and each corner keeps its own `vt`: the quad's
/// fifth UV survives on the fourth corner, and the corners the two triangles share (1 and 3) carry
/// the same UV in both.
#[test]
fn fan_triangulation_keeps_each_polygon_corner_uv() {
    let text = r#"
v 0 0 0
v 1 0 0
v 1 1 0
v 0 1 0
vt 0 0
vt 1 0
vt 1 1
vt 0 1
vt 0.25 0.25
vn 0 0 1
f 1/1/1 2/2/1 3/3/1 4/5/1
"#;
    // `vt 0.25 0.25` with V flipped.
    let fifth_uv = [2500, 7500];
    let corner = |p: usize, uv: [i64; 2]| (POS[p], uv, N[0]);
    // OBJ `v 3` is (1, 1, 0) = POS[3]; `v 4` is (0, 1, 0) = POS[2]. `vt 3` flips to UV[3].
    assert_semantics(
        text,
        &[
            [corner(0, UV[0]), corner(1, UV[1]), corner(3, UV[3])],
            [corner(0, UV[0]), corner(3, UV[3]), corner(2, fifth_uv)],
        ],
    );
}
