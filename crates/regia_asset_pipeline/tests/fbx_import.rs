// SPDX-License-Identifier: MPL-2.0
//! FBX import (ADR-1602): meshes, skins, morph targets, materials with texture references, the
//! node hierarchy, animation clips, unit and axis conversion, UV sets and vertex colors, the
//! comparison with the same mesh imported as glTF, the cook, and hostile input.
//!
//! Every FBX here is written by the small ASCII writer below (FBX 7.4 ASCII), from public
//! descriptions of the format's block layout; no file from another engine, game or exporter.

use std::collections::BTreeMap;
use std::path::Path;

use regia_asset::{AssetId, CookedMesh, CpuMesh};
use regia_asset_pipeline::fbx_import::{
    FbxError, FbxImportSettings, FbxLimits, FbxTextureStatus, convert_fbx_to_glb, import_fbx_bytes,
    import_fbx_path, prescan_fbx,
};
use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};
use regia_asset_pipeline::{Forward, ImportError, TextureCookQuality, import_gltf_glb_bytes};

/// FBX time units per second (FBX 7).
const KTIME_SECOND: i64 = 46_186_158_000;

/// One skin cluster: (cluster id, bone id, control points, weights, the bone's bind world
/// matrix).
type Cluster = (i64, i64, Vec<i32>, Vec<f64>, [f64; 16]);

/// Writes an ASCII FBX 7.4 file. Objects and connections are appended as text blocks.
struct FbxWriter {
    /// `UnitScaleFactor`: centimeters per unit (1 = cm, 100 = m).
    unit_cm: f64,
    /// Up axis: 1 = Y, 2 = Z.
    up: u8,
    objects: Vec<String>,
    connections: Vec<String>,
}

fn list<T: std::fmt::Display>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn array<T: std::fmt::Display>(name: &str, values: &[T]) -> String {
    format!(
        "\t\t{name}: *{} {{\n\t\t\ta: {}\n\t\t}}\n",
        values.len(),
        list(values)
    )
}

impl FbxWriter {
    fn new(unit_cm: f64, up: u8) -> Self {
        Self {
            unit_cm,
            up,
            objects: Vec::new(),
            connections: Vec::new(),
        }
    }

    fn connect(&mut self, child: i64, parent: i64) {
        self.connections.push(format!("\tC: \"OO\",{child},{parent}\n"));
    }

    fn connect_prop(&mut self, child: i64, parent: i64, prop: &str) {
        self.connections
            .push(format!("\tC: \"OP\",{child},{parent}, \"{prop}\"\n"));
    }

    /// A `Model` (`kind` = `Mesh`, `Null`, `LimbNode`) with a local translation, Euler rotation
    /// in degrees and scale.
    fn model(&mut self, id: i64, name: &str, kind: &str, t: [f64; 3], r: [f64; 3], s: [f64; 3]) {
        self.objects.push(format!(
            "\tModel: {id}, \"Model::{name}\", \"{kind}\" {{\n\t\tVersion: 232\n\t\tProperties70:  {{\n\
             \t\t\tP: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",{},{},{}\n\
             \t\t\tP: \"Lcl Rotation\", \"Lcl Rotation\", \"\", \"A\",{},{},{}\n\
             \t\t\tP: \"Lcl Scaling\", \"Lcl Scaling\", \"\", \"A\",{},{},{}\n\
             \t\t}}\n\t\tShading: Y\n\t\tCulling: \"CullingOff\"\n\t}}\n",
            t[0], t[1], t[2], r[0], r[1], r[2], s[0], s[1], s[2]
        ));
    }

    /// A mesh `Geometry`: control points, polygons (each a list of control-point indices),
    /// per-polygon-vertex normals, UV sets (name, per polygon-vertex UVs) and an optional color
    /// set (per polygon-vertex RGBA). One material on every polygon.
    #[allow(clippy::too_many_arguments)] // A test fixture: one argument per FBX layer element.
    fn geometry(
        &mut self,
        id: i64,
        name: &str,
        points: &[[f64; 3]],
        polygons: &[Vec<i32>],
        normals: Option<&[[f64; 3]]>,
        uv_sets: &[(&str, Vec<[f64; 2]>)],
        colors: Option<&[[f64; 4]]>,
    ) {
        let mut text = format!("\tGeometry: {id}, \"Geometry::{name}\", \"Mesh\" {{\n");
        let flat: Vec<f64> = points.iter().flatten().copied().collect();
        text += &array("Vertices", &flat);
        let mut index = Vec::new();
        for polygon in polygons {
            for (k, v) in polygon.iter().enumerate() {
                // The last corner of each polygon is stored as `-(index + 1)`.
                index.push(if k + 1 == polygon.len() { -(v + 1) } else { *v });
            }
        }
        text += &array("PolygonVertexIndex", &index);
        text += "\t\tGeometryVersion: 124\n";
        let mut layers = Vec::new();
        if let Some(normals) = normals {
            let flat: Vec<f64> = normals.iter().flatten().copied().collect();
            text += "\t\tLayerElementNormal: 0 {\n\t\t\tVersion: 102\n\t\t\tName: \"\"\n\
                     \t\t\tMappingInformationType: \"ByPolygonVertex\"\n\
                     \t\t\tReferenceInformationType: \"Direct\"\n";
            text += &array("Normals", &flat).replace("\t\t", "\t\t\t");
            text += "\t\t}\n";
            layers.push((0, "LayerElementNormal"));
        }
        for (set, (set_name, uvs)) in uv_sets.iter().enumerate() {
            let flat: Vec<f64> = uvs.iter().flatten().copied().collect();
            let indices: Vec<usize> = (0..uvs.len()).collect();
            text += &format!(
                "\t\tLayerElementUV: {set} {{\n\t\t\tVersion: 101\n\t\t\tName: \"{set_name}\"\n\
                 \t\t\tMappingInformationType: \"ByPolygonVertex\"\n\
                 \t\t\tReferenceInformationType: \"IndexToDirect\"\n"
            );
            text += &array("UV", &flat).replace("\t\t", "\t\t\t");
            text += &array("UVIndex", &indices).replace("\t\t", "\t\t\t");
            text += "\t\t}\n";
            layers.push((set, "LayerElementUV"));
        }
        if let Some(colors) = colors {
            let flat: Vec<f64> = colors.iter().flatten().copied().collect();
            let indices: Vec<usize> = (0..colors.len()).collect();
            text += "\t\tLayerElementColor: 0 {\n\t\t\tVersion: 101\n\t\t\tName: \"colors\"\n\
                     \t\t\tMappingInformationType: \"ByPolygonVertex\"\n\
                     \t\t\tReferenceInformationType: \"IndexToDirect\"\n";
            text += &array("Colors", &flat).replace("\t\t", "\t\t\t");
            text += &array("ColorIndex", &indices).replace("\t\t", "\t\t\t");
            text += "\t\t}\n";
            layers.push((0, "LayerElementColor"));
        }
        text += "\t\tLayerElementMaterial: 0 {\n\t\t\tVersion: 101\n\t\t\tName: \"\"\n\
                 \t\t\tMappingInformationType: \"AllSame\"\n\
                 \t\t\tReferenceInformationType: \"IndexToDirect\"\n\
                 \t\t\tMaterials: *1 {\n\t\t\t\ta: 0\n\t\t\t}\n\t\t}\n";
        layers.push((0, "LayerElementMaterial"));
        let max_layer = layers.iter().map(|l| l.0).max().unwrap_or(0);
        for layer in 0..=max_layer {
            text += &format!("\t\tLayer: {layer} {{\n\t\t\tVersion: 100\n");
            for (_, kind) in layers.iter().filter(|l| l.0 == layer) {
                text += &format!(
                    "\t\t\tLayerElement:  {{\n\t\t\t\tType: \"{kind}\"\n\t\t\t\tTypedIndex: {layer}\n\t\t\t}}\n"
                );
            }
            text += "\t\t}\n";
        }
        text += "\t}\n";
        self.objects.push(text);
    }

    /// A Lambert material with a diffuse color.
    fn material(&mut self, id: i64, name: &str, diffuse: [f64; 3]) {
        self.objects.push(format!(
            "\tMaterial: {id}, \"Material::{name}\", \"\" {{\n\t\tVersion: 102\n\t\tShadingModel: \"lambert\"\n\
             \t\tMultiLayer: 0\n\t\tProperties70:  {{\n\
             \t\t\tP: \"DiffuseColor\", \"Color\", \"\", \"A\",{},{},{}\n\t\t}}\n\t}}\n",
            diffuse[0], diffuse[1], diffuse[2]
        ));
    }

    /// A file texture (`relative` beside the FBX; `absolute` as the artist's machine named it).
    fn texture(&mut self, id: i64, name: &str, relative: &str, absolute: &str) {
        self.objects.push(format!(
            "\tTexture: {id}, \"Texture::{name}\", \"\" {{\n\t\tType: \"TextureVideoClip\"\n\t\tVersion: 202\n\
             \t\tTextureName: \"Texture::{name}\"\n\t\tFileName: \"{absolute}\"\n\
             \t\tRelativeFilename: \"{relative}\"\n\t}}\n"
        ));
    }

    /// A skin deformer with one cluster per bone: (cluster id, bone id, control points,
    /// weights, the bone's bind world matrix). The mesh sits at the origin. A cluster stores
    /// `TransformLink` (the bone's world matrix at bind) and `Transform` (mesh space to bone
    /// space at bind, here the link's inverse); matrices are 16 numbers, column by column.
    /// Links are translations only, so the inverse negates the translation.
    fn skin(&mut self, skin_id: i64, geometry: i64, clusters: &[Cluster]) {
        self.objects.push(format!(
            "\tDeformer: {skin_id}, \"Deformer::Skin\", \"Skin\" {{\n\t\tVersion: 101\n\t\tLink_DeformAcuracy: 50\n\t}}\n"
        ));
        self.connect(skin_id, geometry);
        let identity = [1.0, 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.];
        for (cluster, bone, indexes, weights, link) in clusters {
            let mut text = format!(
                "\tDeformer: {cluster}, \"SubDeformer::Cluster{cluster}\", \"Cluster\" {{\n\t\tVersion: 100\n\t\tUserData: \"\", \"\"\n"
            );
            text += &array("Indexes", indexes);
            text += &array("Weights", weights);
            let mut to_bone = identity;
            for (t, l) in to_bone[12..15].iter_mut().zip(&link[12..15]) {
                *t = -l;
            }
            text += &array("Transform", &to_bone);
            text += &array("TransformLink", link);
            text += "\t}\n";
            self.objects.push(text);
            self.connect(*cluster, skin_id);
            self.connect(*bone, *cluster);
        }
    }

    /// A blend shape channel `name` on `geometry` moving `points` by `offsets`.
    fn blend_shape(
        &mut self,
        ids: [i64; 3],
        geometry: i64,
        name: &str,
        points: &[i32],
        offsets: &[[f64; 3]],
    ) {
        let [deformer, channel, shape] = ids;
        let mut text = format!("\tGeometry: {shape}, \"Geometry::{name}\", \"Shape\" {{\n\t\tVersion: 100\n");
        text += &array("Indexes", points);
        let flat: Vec<f64> = offsets.iter().flatten().copied().collect();
        text += &array("Vertices", &flat);
        text += "\t}\n";
        self.objects.push(text);
        self.objects.push(format!(
            "\tDeformer: {deformer}, \"Deformer::Morph\", \"BlendShape\" {{\n\t\tVersion: 100\n\t}}\n"
        ));
        self.objects.push(format!(
            "\tDeformer: {channel}, \"SubDeformer::{name}\", \"BlendShapeChannel\" {{\n\t\tVersion: 100\n\
             \t\tDeformPercent: 0\n\t\tFullWeights: *1 {{\n\t\t\ta: 100\n\t\t}}\n\t}}\n"
        ));
        self.connect(deformer, geometry);
        self.connect(channel, deformer);
        self.connect(shape, channel);
    }

    /// An animation stack `name` with one layer; returns the layer id.
    fn anim_stack(&mut self, stack: i64, layer: i64, name: &str, seconds: f64) -> i64 {
        let stop = (seconds * KTIME_SECOND as f64) as i64;
        self.objects.push(format!(
            "\tAnimationStack: {stack}, \"AnimStack::{name}\", \"\" {{\n\t\tProperties70:  {{\n\
             \t\t\tP: \"LocalStop\", \"KTime\", \"Time\", \"\",{stop}\n\
             \t\t\tP: \"ReferenceStop\", \"KTime\", \"Time\", \"\",{stop}\n\t\t}}\n\t}}\n"
        ));
        self.objects.push(format!(
            "\tAnimationLayer: {layer}, \"AnimLayer::BaseLayer\", \"\" {{\n\t}}\n"
        ));
        self.connect(layer, stack);
        layer
    }

    /// Linear curves for `prop` (`Lcl Translation`, `DeformPercent` …) of `target`: one curve
    /// per component (`d|X` … or `d|DeformPercent`), keys `(seconds, value)`.
    fn anim_curves(
        &mut self,
        ids: i64,
        layer: i64,
        target: i64,
        prop: &str,
        components: &[(&str, Vec<(f64, f64)>)],
    ) {
        let node = ids;
        let mut text = format!(
            "\tAnimationCurveNode: {node}, \"AnimCurveNode::{prop}\", \"\" {{\n\t\tProperties70:  {{\n"
        );
        for (component, keys) in components {
            text += &format!(
                "\t\t\tP: \"d|{component}\", \"Number\", \"\", \"A\",{}\n",
                keys.first().map_or(0.0, |k| k.1)
            );
        }
        text += "\t\t}\n\t}\n";
        self.objects.push(text);
        self.connect(node, layer);
        self.connect_prop(node, target, prop);
        for (k, (component, keys)) in components.iter().enumerate() {
            let curve = ids + 1 + k as i64;
            let times: Vec<i64> = keys
                .iter()
                .map(|(t, _)| (t * KTIME_SECOND as f64) as i64)
                .collect();
            let values: Vec<f64> = keys.iter().map(|(_, v)| *v).collect();
            let mut text = format!(
                "\tAnimationCurve: {curve}, \"AnimCurve::\", \"\" {{\n\t\tDefault: 0\n\t\tKeyVer: 4009\n"
            );
            text += &array("KeyTime", &times);
            text += &array("KeyValueFloat", &values);
            // Interpolation flag 0x4: linear.
            text += &array("KeyAttrFlags", &[4]);
            text += &array("KeyAttrDataFloat", &[0, 0, 0, 0]);
            text += &array("KeyAttrRefCount", &[keys.len()]);
            text += "\t}\n";
            self.objects.push(text);
            self.connect_prop(curve, node, &format!("d|{component}"));
        }
    }

    fn finish(&self) -> String {
        let (up, front) = if self.up == 2 { (2, 1) } else { (1, 2) };
        // A Z-up right-handed file (3ds Max style) has front -Y.
        let front_sign = if self.up == 2 { -1 } else { 1 };
        let mut out = String::from(
            "; FBX 7.4.0 project file\n; Written by the Regia Engine FBX import test.\n\
             FBXHeaderExtension:  {\n\tFBXHeaderVersion: 1003\n\tFBXVersion: 7400\n\tCreator: \"Regia test writer\"\n}\n",
        );
        out += &format!(
            "GlobalSettings:  {{\n\tVersion: 1000\n\tProperties70:  {{\n\
             \t\tP: \"UpAxis\", \"int\", \"Integer\", \"\",{up}\n\
             \t\tP: \"UpAxisSign\", \"int\", \"Integer\", \"\",1\n\
             \t\tP: \"FrontAxis\", \"int\", \"Integer\", \"\",{front}\n\
             \t\tP: \"FrontAxisSign\", \"int\", \"Integer\", \"\",{front_sign}\n\
             \t\tP: \"CoordAxis\", \"int\", \"Integer\", \"\",0\n\
             \t\tP: \"CoordAxisSign\", \"int\", \"Integer\", \"\",1\n\
             \t\tP: \"OriginalUpAxis\", \"int\", \"Integer\", \"\",{up}\n\
             \t\tP: \"OriginalUpAxisSign\", \"int\", \"Integer\", \"\",1\n\
             \t\tP: \"UnitScaleFactor\", \"double\", \"Number\", \"\",{}\n\
             \t\tP: \"OriginalUnitScaleFactor\", \"double\", \"Number\", \"\",{}\n\
             \t\tP: \"TimeMode\", \"enum\", \"\", \"\",6\n\t}}\n}}\n",
            self.unit_cm, self.unit_cm
        );
        out += "Objects:  {\n";
        for object in &self.objects {
            out += object;
        }
        out += "}\nConnections:  {\n";
        for connection in &self.connections {
            out += connection;
        }
        out += "}\n";
        out
    }
}

/// The 8 corners of a cube of side `size` from the origin, and its 6 quads (outward winding).
fn cube(size: f64) -> (Vec<[f64; 3]>, Vec<Vec<i32>>) {
    let s = size;
    let points = vec![
        [0., 0., 0.],
        [s, 0., 0.],
        [s, s, 0.],
        [0., s, 0.],
        [0., 0., s],
        [s, 0., s],
        [s, s, s],
        [0., s, s],
    ];
    let quads = vec![
        vec![0, 3, 2, 1], // -Z
        vec![4, 5, 6, 7], // +Z
        vec![0, 1, 5, 4], // -Y
        vec![3, 7, 6, 2], // +Y
        vec![0, 4, 7, 3], // -X
        vec![1, 2, 6, 5], // +X
    ];
    (points, quads)
}

/// Face normals of `quads`, one per polygon vertex.
fn quad_normals(points: &[[f64; 3]], quads: &[Vec<i32>]) -> Vec<[f64; 3]> {
    let mut out = Vec::new();
    for quad in quads {
        let p = |i: usize| points[quad[i] as usize];
        let (a, b, c) = (p(0), p(1), p(2));
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        for _ in quad {
            out.push([n[0] / len, n[1] / len, n[2] / len]);
        }
    }
    out
}

/// Per polygon-vertex UVs of a quad mesh (each quad gets the unit square).
fn quad_uvs(quads: &[Vec<i32>]) -> Vec<[f64; 2]> {
    quads
        .iter()
        .flat_map(|_| [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
        .collect()
}

/// A one-mesh file: a cube of `size` units (in the file's unit) under model `Cube`.
fn cube_file(unit_cm: f64, up: u8, size: f64) -> String {
    let mut w = FbxWriter::new(unit_cm, up);
    let (points, quads) = cube(size);
    let normals = quad_normals(&points, &quads);
    w.geometry(
        100,
        "Cube",
        &points,
        &quads,
        Some(&normals),
        &[("map1", quad_uvs(&quads))],
        None,
    );
    w.model(200, "Cube", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.material(300, "Red", [1.0, 0.0, 0.0]);
    w.connect(200, 0);
    w.connect(100, 200);
    w.connect(300, 200);
    w.finish()
}

fn settings() -> FbxImportSettings {
    FbxImportSettings::default()
}

fn import(fbx: &str, forward: Forward) -> CpuMesh {
    let imported = import_fbx_bytes(
        fbx.as_bytes(),
        AssetId::new_random(),
        forward,
        "linux",
        TextureCookQuality::Low,
        &settings(),
    )
    .unwrap_or_else(|e| panic!("import: {e}\n{fbx}"));
    cpu(&imported.gltf.primary_mesh)
}

fn cpu(mesh: &CookedMesh) -> CpuMesh {
    let bytes = mesh.to_postcard().expect("encode");
    CookedMesh::from_postcard(&bytes)
        .expect("decode")
        .to_cpu_mesh()
        .expect("unpack")
}

fn extent(mesh: &CpuMesh) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in &mesh.positions {
        for c in 0..3 {
            min[c] = min[c].min(p[c]);
            max[c] = max[c].max(p[c]);
        }
    }
    (min, max)
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-4
}

/// Triangles as sorted lists of rounded corner positions (order-independent geometry).
fn triangle_set(mesh: &CpuMesh) -> Vec<[[i64; 3]; 3]> {
    let q = |p: [f32; 3]| p.map(|c| (c * 10_000.0).round() as i64);
    let mut out: Vec<[[i64; 3]; 3]> = mesh
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let mut corners = [
                q(mesh.positions[t[0] as usize]),
                q(mesh.positions[t[1] as usize]),
                q(mesh.positions[t[2] as usize]),
            ];
            // Keep the winding: rotate the smallest corner first.
            let first = (0..3).min_by_key(|&i| corners[i]).unwrap_or(0);
            corners.rotate_left(first);
            corners
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_centimeter_file_imports_in_meters() {
    // A 100-unit cube in a centimeter file is one meter on a side.
    let mesh = import(&cube_file(1.0, 1, 100.0), Forward::Keep);
    let (min, max) = extent(&mesh);
    for c in 0..3 {
        assert!(close(max[c] - min[c], 1.0), "extent {min:?}..{max:?}");
    }
    // A meter file of a 1-unit cube is the same size.
    let mesh = import(&cube_file(100.0, 1, 1.0), Forward::Keep);
    let (min, max) = extent(&mesh);
    assert!(close(max[0] - min[0], 1.0), "{min:?}..{max:?}");
    assert_eq!(mesh.indices.len(), 36, "6 quads triangulate to 12 triangles");
}

#[test]
fn the_scale_setting_multiplies_after_the_unit_conversion() {
    let imported = import_fbx_bytes(
        cube_file(1.0, 1, 100.0).as_bytes(),
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
        &FbxImportSettings {
            scale: 2.0,
            ..settings()
        },
    )
    .expect("import");
    let (min, max) = extent(&cpu(&imported.gltf.primary_mesh));
    assert!(close(max[1] - min[1], 2.0), "{min:?}..{max:?}");
    assert_eq!(imported.report.scale, 2.0);
}

#[test]
fn a_z_up_file_becomes_y_up() {
    // A tall box: 1 m wide, 3 m along the file's up axis (Z).
    let mut w = FbxWriter::new(100.0, 2);
    let points = vec![
        [0., 0., 0.],
        [1., 0., 0.],
        [1., 1., 0.],
        [0., 1., 0.],
        [0., 0., 3.],
        [1., 0., 3.],
        [1., 1., 3.],
        [0., 1., 3.],
    ];
    let (_, quads) = cube(1.0);
    w.geometry(100, "Tower", &points, &quads, None, &[], None);
    w.model(200, "Tower", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.connect(200, 0);
    w.connect(100, 200);
    let fbx = w.finish();
    let imported = import_fbx_bytes(
        fbx.as_bytes(),
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
        &settings(),
    )
    .expect("import");
    assert_eq!(imported.report.source_up_axis, "+Z");
    let (min, max) = extent(&cpu(&imported.gltf.primary_mesh));
    assert!(
        close(max[1] - min[1], 3.0),
        "the tall axis is +Y: {min:?}..{max:?}"
    );
    assert!(close(max[0] - min[0], 1.0), "{min:?}..{max:?}");
    assert!(close(max[2] - min[2], 1.0), "{min:?}..{max:?}");
    assert!(min[1] > -1e-4, "up stays up: {min:?}");
}

/// The same cube as a hand-built GLB (positions in meters, the same quads as triangles).
fn cube_glb() -> Vec<u8> {
    let (points, quads) = cube(1.0);
    let normals = quad_normals(&points, &quads);
    let mut positions = Vec::new();
    let mut normal_values = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    for (q, quad) in quads.iter().enumerate() {
        let base = positions.len() as u32 / 3;
        for (k, &v) in quad.iter().enumerate() {
            positions.extend(points[v as usize].map(|c| c as f32));
            normal_values.extend(normals[q * 4 + k].map(|c| c as f32));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    let mut bin: Vec<u8> = Vec::new();
    bin.extend(positions.iter().flat_map(|v| v.to_le_bytes()));
    let normal_offset = bin.len();
    bin.extend(normal_values.iter().flat_map(|v| v.to_le_bytes()));
    let index_offset = bin.len();
    bin.extend(indices.iter().flat_map(|v| v.to_le_bytes()));
    let count = positions.len() / 3;
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"name":"Cube","mesh":0}}],
        "meshes":[{{"name":"Cube","primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1}},"indices":2}}]}}],
        "buffers":[{{"byteLength":{len}}}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{normal_offset}}},
          {{"buffer":0,"byteOffset":{normal_offset},"byteLength":{nl}}},
          {{"buffer":0,"byteOffset":{index_offset},"byteLength":{il}}}],
        "accessors":[{{"bufferView":0,"componentType":5126,"count":{count},"type":"VEC3","min":[0,0,0],"max":[1,1,1]}},
          {{"bufferView":1,"componentType":5126,"count":{count},"type":"VEC3"}},
          {{"bufferView":2,"componentType":5125,"count":{ic},"type":"SCALAR"}}]}}"#,
        len = bin.len(),
        nl = index_offset - normal_offset,
        il = bin.len() - index_offset,
        ic = indices.len(),
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

#[test]
fn the_same_cube_as_gltf_and_fbx_cooks_equal_geometry() {
    let gltf = import_gltf_glb_bytes(
        &cube_glb(),
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        TextureCookQuality::Low,
    )
    .expect("glTF import");
    let gltf_mesh = cpu(&gltf.primary_mesh);
    // The FBX: the same cube in centimeters (100 units), Y up.
    let fbx_mesh = import(&cube_file(1.0, 1, 100.0), Forward::Convert);
    assert_eq!(fbx_mesh.indices.len(), gltf_mesh.indices.len());
    assert_eq!(fbx_mesh.positions.len(), gltf_mesh.positions.len());
    assert_eq!(triangle_set(&fbx_mesh), triangle_set(&gltf_mesh));
    // Normals per position agree too (flat-shaded cube: one normal per corner and face).
    let normals = |mesh: &CpuMesh| {
        let mut out: Vec<([i64; 3], [i64; 3])> = mesh
            .positions
            .iter()
            .zip(&mesh.normals)
            .map(|(p, n)| {
                (
                    p.map(|c| (c * 1e4).round() as i64),
                    n.map(|c| (c * 1e3).round() as i64),
                )
            })
            .collect();
        out.sort();
        out
    };
    assert_eq!(normals(&fbx_mesh), normals(&gltf_mesh));
}

#[test]
fn the_node_hierarchy_keeps_names_parents_and_transforms() {
    let mut w = FbxWriter::new(100.0, 1);
    let (points, quads) = cube(1.0);
    w.geometry(100, "Box", &points, &quads, None, &[], None);
    w.model(200, "Base", "Null", [0., 1., 0.], [0.; 3], [1.; 3]);
    w.model(201, "Arm", "Null", [2., 0., 0.], [0., 90., 0.], [1.; 3]);
    w.model(202, "Hand", "Mesh", [0., 0., 3.], [0.; 3], [1.; 3]);
    w.connect(200, 0);
    w.connect(201, 200);
    w.connect(202, 201);
    w.connect(100, 202);
    let conversion = convert_fbx_to_glb(w.finish().as_bytes(), None, &settings()).expect("convert");
    assert_eq!(conversion.report.nodes, 3);
    let gltf = gltf::Gltf::from_slice(&conversion.glb).expect("valid glTF");
    let by_name: BTreeMap<String, gltf::Node> = gltf
        .document
        .nodes()
        .map(|n| (n.name().unwrap_or_default().to_string(), n))
        .collect();
    let base = &by_name["Base"];
    let arm = &by_name["Arm"];
    let hand = &by_name["Hand"];
    assert_eq!(
        base.children().map(|c| c.index()).collect::<Vec<_>>(),
        vec![arm.index()]
    );
    assert_eq!(
        arm.children().map(|c| c.index()).collect::<Vec<_>>(),
        vec![hand.index()]
    );
    let roots: Vec<usize> = gltf
        .document
        .default_scene()
        .expect("scene")
        .nodes()
        .map(|n| n.index())
        .collect();
    assert_eq!(roots, vec![base.index()]);
    let (t, r, _) = arm.transform().decomposed();
    assert!(close(t[0], 2.0) && close(t[1], 0.0) && close(t[2], 0.0), "{t:?}");
    // 90° about +Y: (0, sin 45°, 0, cos 45°).
    let half = std::f32::consts::FRAC_1_SQRT_2;
    assert!(close(r[1].abs(), half) && close(r[3].abs(), half), "{r:?}");
    assert!(hand.mesh().is_some());
    // The glTF importer bakes the chain: the hand's cube ends at Base + Arm + rotated (0,0,3).
    let imported = import_fbx_bytes(
        w.finish().as_bytes(),
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
        &settings(),
    )
    .expect("import");
    let (min, _) = extent(&cpu(&imported.gltf.primary_mesh));
    // 90° about +Y maps (x, y, z) to (z, y, -x): the hand's origin (0,0,3) lands at (3,0,0),
    // plus Arm's (2,0,0) and Base's (0,1,0); the unit cube then spans x 5..6, y 1..2, z -1..0.
    assert!(close(min[0], 5.0), "{min:?}");
    assert!(close(min[1], 1.0), "{min:?}");
    assert!(close(min[2], -1.0), "{min:?}");
}

#[test]
fn uv_sets_and_vertex_colors_are_carried() {
    let mut w = FbxWriter::new(100.0, 1);
    let points = vec![[0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.]];
    let quads = vec![vec![0, 1, 2, 3]];
    let map1 = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let lightmap = vec![[0.25, 0.25], [0.5, 0.25], [0.5, 0.5], [0.25, 0.5]];
    let colors = [
        [1.0, 0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, 1.0],
        [1.0, 1.0, 1.0, 0.5],
    ];
    w.geometry(
        100,
        "Card",
        &points,
        &quads,
        None,
        &[("map1", map1), ("lightmap", lightmap)],
        Some(&colors),
    );
    w.model(200, "Card", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.connect(200, 0);
    w.connect(100, 200);
    let fbx = w.finish();
    let conversion = convert_fbx_to_glb(fbx.as_bytes(), None, &settings()).expect("convert");
    assert_eq!(
        conversion.report.uv_sets,
        vec!["map1".to_string(), "lightmap".to_string()]
    );
    let gltf = gltf::Gltf::from_slice(&conversion.glb).expect("valid glTF");
    let blob = gltf.blob.clone().expect("bin");
    let primitive = gltf
        .document
        .meshes()
        .next()
        .expect("mesh")
        .primitives()
        .next()
        .expect("prim");
    let reader = primitive.reader(|_| Some(blob.as_slice()));
    let uv0: Vec<[f32; 2]> = reader.read_tex_coords(0).expect("uv0").into_f32().collect();
    let uv1: Vec<[f32; 2]> = reader.read_tex_coords(1).expect("uv1").into_f32().collect();
    let positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
    let corner = positions
        .iter()
        .position(|p| close(p[0], 1.0) && close(p[1], 1.0))
        .expect("corner");
    // FBX V is flipped to glTF's top-left origin.
    assert!(
        close(uv0[corner][0], 1.0) && close(uv0[corner][1], 0.0),
        "{:?}",
        uv0[corner]
    );
    assert!(
        close(uv1[corner][0], 0.5) && close(uv1[corner][1], 0.5),
        "{:?}",
        uv1[corner]
    );
    let colors_out: Vec<[f32; 4]> = reader.read_colors(0).expect("colors").into_rgba_f32().collect();
    assert_eq!(colors_out.len(), positions.len());
    assert!(colors_out.iter().any(|c| close(c[3], 0.5)), "{colors_out:?}");

    // `uv_set` chooses the set the cook keeps as UV0.
    let chosen = convert_fbx_to_glb(
        fbx.as_bytes(),
        None,
        &FbxImportSettings {
            uv_set: Some("lightmap".into()),
            ..settings()
        },
    )
    .expect("convert");
    assert_eq!(chosen.report.uv_sets[0], "lightmap");
    let imported = import_fbx_bytes(
        fbx.as_bytes(),
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
        &settings(),
    )
    .expect("import");
    let mesh = cpu(&imported.gltf.primary_mesh);
    assert_eq!(mesh.colors.len(), mesh.positions.len(), "COLOR_0 is cooked");
}

/// A two-bone strip: vertices 0, 1 on `Root`, 2, 3 on `Tip` (one meter up).
fn skinned_file() -> String {
    // Centimeters: the strip is 1 m on a side, Tip sits 1 m above Root.
    let mut w = FbxWriter::new(1.0, 1);
    let points = vec![[0., 0., 0.], [100., 0., 0.], [100., 100., 0.], [0., 100., 0.]];
    let quads = vec![vec![0, 1, 2, 3]];
    w.geometry(100, "Strip", &points, &quads, None, &[], None);
    w.model(200, "Strip", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.model(300, "Root", "LimbNode", [0.; 3], [0.; 3], [1.; 3]);
    w.model(301, "Tip", "LimbNode", [0., 100., 0.], [0.; 3], [1.; 3]);
    w.connect(200, 0);
    w.connect(100, 200);
    w.connect(300, 0);
    w.connect(301, 300);
    let identity = [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.];
    let mut tip = identity;
    // FBX stores a matrix column by column: the translation is elements 12..15.
    tip[13] = 100.0;
    w.skin(
        400,
        100,
        &[
            (401, 300, vec![0, 1], vec![1.0, 1.0], identity),
            (402, 301, vec![2, 3], vec![1.0, 1.0], tip),
        ],
    );
    let layer = w.anim_stack(500, 501, "Wave", 1.0);
    w.anim_curves(
        510,
        layer,
        301,
        "Lcl Translation",
        &[
            ("X", vec![(0.0, 0.0), (1.0, 50.0)]),
            ("Y", vec![(0.0, 100.0), (1.0, 100.0)]),
            ("Z", vec![(0.0, 0.0), (1.0, 0.0)]),
        ],
    );
    w.finish()
}

#[test]
fn skins_import_with_joints_weights_and_inverse_binds() {
    let fbx = skinned_file();
    let imported = import_fbx_bytes(
        fbx.as_bytes(),
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
        &settings(),
    )
    .unwrap_or_else(|e| panic!("import: {e}"));
    assert_eq!(imported.report.skins, 1);
    assert_eq!(imported.report.joints, 2);
    let mesh = cpu(&imported.gltf.primary_mesh);
    let skin = mesh.skin.as_ref().expect("skinned mesh");
    assert_eq!(skin.joints.len(), mesh.positions.len());
    for weights in &skin.weights {
        // `unorm16` weights (cooked mesh v2) sum to one.
        let sum: u32 = weights.iter().map(|w| u32::from(*w)).sum();
        assert!(sum.abs_diff(65_535) <= 2, "weights sum to one: {weights:?}");
    }
    let animation = import_gltf_animation(&imported.glb, &GltfAnimSettings::default()).expect("anim import");
    assert_eq!(animation.skins.len(), 1);
    let mut names = animation.skins[0].skeleton.joint_names.clone();
    names.sort();
    assert_eq!(names, vec!["Root".to_string(), "Tip".to_string()]);
    // In the converted glTF, the tip's inverse bind undoes its bind translation of 1 m up.
    let gltf = gltf::Gltf::from_slice(&imported.glb).expect("valid glTF");
    let blob = gltf.blob.clone().expect("bin");
    let gltf_skin = gltf.document.skins().next().expect("skin");
    let ibms: Vec<[[f32; 4]; 4]> = gltf_skin
        .reader(|_| Some(blob.as_slice()))
        .read_inverse_bind_matrices()
        .expect("inverse binds")
        .collect();
    let slot = gltf_skin
        .joints()
        .position(|j| j.name() == Some("Tip"))
        .expect("tip joint");
    let ibm = ibms[slot];
    assert!(close(ibm[3][1], -1.0), "{ibm:?}");
    assert!(close(ibm[3][0], 0.0) && close(ibm[3][2], 0.0), "{ibm:?}");
    // Vertices at the top follow Tip, those at the bottom Root (cooked joint ids name the
    // mesh's joint list).
    let tip_id = skin.joint_names.iter().position(|n| n == "Tip").expect("tip") as u16;
    let root_id = skin.joint_names.iter().position(|n| n == "Root").expect("root") as u16;
    for (joints, position) in skin.joints.iter().zip(&mesh.positions) {
        let expected = if position[1] > 0.5 { tip_id } else { root_id };
        assert_eq!(joints[0], expected, "{position:?}");
    }
}

#[test]
fn animation_clips_import_in_seconds_and_meters() {
    let conversion = convert_fbx_to_glb(skinned_file().as_bytes(), None, &settings()).expect("convert");
    assert_eq!(conversion.report.clips.len(), 1);
    let clip = &conversion.report.clips[0];
    assert_eq!(clip.name, "Wave");
    assert!(close(clip.duration_seconds, 1.0), "{clip:?}");
    let gltf = gltf::Gltf::from_slice(&conversion.glb).expect("valid glTF");
    let blob = gltf.blob.clone().expect("bin");
    let animation = gltf.document.animations().next().expect("animation");
    let channel = animation
        .channels()
        .find(|c| c.target().property() == gltf::animation::Property::Translation)
        .expect("translation channel");
    assert_eq!(channel.target().node().name(), Some("Tip"));
    let reader = channel.reader(|_| Some(blob.as_slice()));
    let times: Vec<f32> = reader.read_inputs().expect("inputs").collect();
    let values: Vec<[f32; 3]> = match reader.read_outputs().expect("outputs") {
        gltf::animation::util::ReadOutputs::Translations(t) => t.collect(),
        _ => panic!("translation outputs"),
    };
    assert!(
        close(times[0], 0.0) && close(*times.last().expect("last"), 1.0),
        "{times:?}"
    );
    // 50 cm along X at the end, 1 m up throughout.
    let last = values.last().expect("last");
    assert!(close(last[0], 0.5) && close(last[1], 1.0), "{values:?}");
    // The clip cooks through the glTF animation importer.
    let animation =
        import_gltf_animation(&conversion.glb, &GltfAnimSettings::default()).expect("anim import");
    let clips = &animation.skins[0].clips;
    assert_eq!(clips.len(), 1);
    assert!(close(clips[0].duration, 1.0), "{}", clips[0].duration);
    // `import_animation: false` drops it.
    let still = convert_fbx_to_glb(
        skinned_file().as_bytes(),
        None,
        &FbxImportSettings {
            import_animation: false,
            ..settings()
        },
    )
    .expect("convert");
    assert!(still.report.clips.is_empty());
}

#[test]
fn morph_targets_import_with_names_deltas_and_weight_curves() {
    let mut w = FbxWriter::new(1.0, 1);
    let points = vec![[0., 0., 0.], [100., 0., 0.], [100., 100., 0.], [0., 100., 0.]];
    let quads = vec![vec![0, 1, 2, 3]];
    w.geometry(100, "Face", &points, &quads, None, &[], None);
    w.model(200, "Face", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.connect(200, 0);
    w.connect(100, 200);
    w.blend_shape([300, 301, 302], 100, "Smile", &[2], &[[0., 0., 50.]]);
    let layer = w.anim_stack(500, 501, "Talk", 2.0);
    w.anim_curves(
        510,
        layer,
        301,
        "DeformPercent",
        &[("DeformPercent", vec![(0.0, 0.0), (2.0, 100.0)])],
    );
    let fbx = w.finish();
    let imported = import_fbx_bytes(
        fbx.as_bytes(),
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
        &settings(),
    )
    .unwrap_or_else(|e| panic!("import: {e}"));
    assert_eq!(imported.report.morph_targets, 1);
    let mesh = cpu(&imported.gltf.primary_mesh);
    assert_eq!(mesh.morph_targets.len(), 1);
    let target = &mesh.morph_targets[0];
    assert_eq!(target.name, "Smile");
    // The moved corner (1 m, 1 m) gets +0.5 m along Z (50 cm); the others stay. The cooked
    // target is sparse: only moved vertices are listed.
    assert!(!target.vertex_indices.is_empty());
    for (vertex, delta) in target.vertex_indices.iter().zip(&target.position_deltas) {
        let position = mesh.positions[*vertex as usize];
        let expected = if close(position[0], 1.0) && close(position[1], 1.0) {
            0.5
        } else {
            0.0
        };
        assert!(close(delta[2], expected), "{position:?} {delta:?}");
    }
    // The weight curve becomes a glTF `weights` channel, 0..1 over 2 s.
    let gltf = gltf::Gltf::from_slice(&imported.glb).expect("valid glTF");
    let animation = gltf.document.animations().next().expect("animation");
    let channel = animation.channels().next().expect("channel");
    assert_eq!(
        channel.target().property(),
        gltf::animation::Property::MorphTargetWeights
    );
    let blob = gltf.blob.clone().expect("bin");
    let reader = channel.reader(|_| Some(blob.as_slice()));
    let weights: Vec<f32> = match reader.read_outputs().expect("outputs") {
        gltf::animation::util::ReadOutputs::MorphTargetWeights(w) => w.into_f32().collect(),
        _ => panic!("weights"),
    };
    assert!(
        close(weights[0], 0.0) && close(*weights.last().expect("last"), 1.0),
        "{weights:?}"
    );
    let animation = import_gltf_animation(&imported.glb, &GltfAnimSettings::default()).expect("anim import");
    assert_eq!(animation.node_clips.len(), 1, "{:?}", animation.warnings);
}

/// A 2×2 RGBA PNG.
fn write_png(path: &Path) {
    let mut image = image::RgbaImage::new(2, 2);
    for pixel in image.pixels_mut() {
        *pixel = image::Rgba([200, 30, 30, 255]);
    }
    image.save(path).expect("png");
}

/// A quad with a red material that names `relative` (and an artist's absolute path).
fn textured_file(relative: &str, absolute: &str) -> String {
    let mut w = FbxWriter::new(100.0, 1);
    let points = vec![[0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.]];
    let quads = vec![vec![0, 1, 2, 3]];
    let normals = quad_normals(&points, &quads);
    w.geometry(
        100,
        "Card",
        &points,
        &quads,
        Some(&normals),
        &[("map1", quad_uvs(&quads))],
        None,
    );
    w.model(200, "Card", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.material(300, "Painted", [0.8, 0.1, 0.1]);
    w.texture(400, "Albedo", relative, absolute);
    w.connect(200, 0);
    w.connect(100, 200);
    w.connect(300, 200);
    w.connect_prop(400, 300, "DiffuseColor");
    w.finish()
}

#[test]
fn materials_carry_colors_and_texture_references() {
    let dir = regia_test_utils::unique_temp_dir("fbx-textures");
    std::fs::create_dir_all(dir.join("tex")).expect("dir");
    write_png(&dir.join("tex/albedo.png"));
    let fbx_path = dir.join("card.fbx");
    std::fs::write(
        &fbx_path,
        textured_file("tex/albedo.png", "C:/art/tex/albedo.png"),
    )
    .expect("write");
    let imported = import_fbx_path(
        &fbx_path,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::Low,
    )
    .unwrap_or_else(|e| panic!("import: {e}"));
    assert_eq!(imported.report.materials, 1);
    let texture = &imported.report.textures[0];
    assert_eq!(texture.status, FbxTextureStatus::External);
    assert_eq!(texture.file, "tex/albedo.png");
    assert_eq!(texture.role, "base_color");
    let types: Vec<&str> = imported
        .gltf
        .sub_assets
        .iter()
        .map(|s| s.type_path.as_str())
        .collect();
    assert!(
        types.iter().any(|t| t.ends_with("CookedStandardMaterial")),
        "{types:?}"
    );
    assert!(
        types
            .iter()
            .any(|t| t.ends_with("CookedImage") || t.ends_with("CookedTexture")),
        "{types:?}"
    );
    let material = imported
        .gltf
        .sub_assets
        .iter()
        .find(|s| s.type_path.ends_with("CookedStandardMaterial"))
        .expect("material");
    assert!(material.path.contains("Painted"), "{}", material.path);
    assert_eq!(
        material.dependencies.len(),
        1,
        "the material depends on its texture"
    );

    // Only the file name survives (an artist's absolute path): found beside the FBX.
    write_png(&dir.join("flat.png"));
    std::fs::write(&fbx_path, textured_file("", "D:\\\\art\\\\flat.png")).expect("write");
    let imported = import_fbx_path(
        &fbx_path,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::Low,
    )
    .expect("import");
    assert_eq!(imported.report.textures[0].status, FbxTextureStatus::External);
    assert_eq!(imported.report.textures[0].file, "flat.png");

    // A missing texture drops the slot with a warning; the import still succeeds.
    std::fs::write(&fbx_path, textured_file("tex/absent.png", "/nowhere/absent.png")).expect("write");
    let imported = import_fbx_path(
        &fbx_path,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::Low,
    )
    .expect("import");
    assert_eq!(imported.report.textures[0].status, FbxTextureStatus::Missing);
    assert!(imported.report.warnings.iter().any(|w| w.contains("absent.png")));
    // A path that leaves the directory is never followed.
    std::fs::write(dir.join("..").join("escape.png"), b"not read").ok();
    std::fs::write(&fbx_path, textured_file("../escape.png", "")).expect("write");
    let converted = convert_fbx_to_glb(&std::fs::read(&fbx_path).expect("read"), Some(&dir), &settings())
        .expect("convert");
    assert_eq!(converted.report.textures[0].status, FbxTextureStatus::Missing);
    let _ = std::fs::remove_file(dir.join("..").join("escape.png"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_fbx_cooks_through_the_project_cook_like_a_glb() {
    use regia_asset::{AssetManifest, cooked_cache_dir};
    use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
    let root = regia_test_utils::unique_temp_dir("fbx-cook");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join("assets/strip.fbx"), skinned_file()).expect("write");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let manifest = AssetManifest::read_from_cache(&cooked_cache_dir(&root, host_platform()))
        .expect("manifest")
        .expect("present");
    let types: Vec<&str> = manifest
        .entries
        .iter()
        .filter(|e| e.path.starts_with("assets/strip.fbx"))
        .map(|e| e.type_path.as_str())
        .collect();
    assert!(types.iter().any(|t| t.ends_with("CookedMesh")), "{types:?}");
    assert!(types.iter().any(|t| t.ends_with("CookedModel")), "{types:?}");
    assert!(types.iter().any(|t| t.contains("Skeleton")), "{types:?}");
    assert!(types.iter().any(|t| t.contains("AnimationClip")), "{types:?}");
    let meta = std::fs::read_to_string(root.join("assets/strip.fbx.meta")).expect("meta");
    assert!(meta.contains("fbx"), "{meta}");
    // A second cook skips the unchanged file.
    let again = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert!(again.skipped >= 1, "{again:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-7819 on the converted-source path: an FBX whose `.meta` says `lossless_clips: true` cooks its
/// clip as a `Raw` `cooked-clip` v3 body, bit-exact to the clip the glTF animation importer reads from
/// the converted file; without the switch the clip is compressed.
#[test]
fn an_fbx_with_lossless_clips_cooks_its_clip_bit_exact() {
    use regia_animation::AnimationClip;
    use regia_asset::{AssetManifest, MetaSidecar, cooked_cache_dir, meta_path_for};
    use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
    let root = regia_test_utils::unique_temp_dir("fbx-cook-lossless");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let source = root.join("assets/strip.fbx");
    std::fs::write(&source, skinned_file()).expect("write");
    let payload = |root: &Path| {
        let cache = cooked_cache_dir(root, host_platform());
        let manifest = AssetManifest::read_from_cache(&cache)
            .expect("manifest")
            .expect("present");
        let id = manifest.id_for_path("assets/strip.fbx#clip/Wave").expect("clip");
        let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked");
        regia_asset::decode_cooked_file(&bytes)
            .expect("decodes")
            .1
            .to_vec()
    };
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(AnimationClip::compressed_cooked_payload(&payload(&root)).is_some());

    let mut meta =
        MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("meta")).expect("parses");
    meta.settings = Some("(lossless_clips: true)".into());
    std::fs::write(meta_path_for(&source), meta.to_ron_string().expect("ron")).expect("meta");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let conversion = convert_fbx_to_glb(skinned_file().as_bytes(), Some(&root.join("assets")), &settings())
        .expect("convert");
    let animation =
        import_gltf_animation(&conversion.glb, &GltfAnimSettings::default()).expect("anim import");
    let source_clip = &animation.skins[0].clips[0];
    let cooked = payload(&root);
    assert!(AnimationClip::compressed_cooked_payload(&cooked).is_none());
    assert_eq!(cooked, source_clip.to_cooked_payload().expect("payload"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn limits_reject_oversized_scenes() {
    let fbx = cube_file(100.0, 1, 1.0);
    let with = |limits: FbxLimits| {
        convert_fbx_to_glb(fbx.as_bytes(), None, &FbxImportSettings { limits, ..settings() })
            .map(|_| ())
            .map_err(|e| e.code())
    };
    assert_eq!(with(FbxLimits::default()), Ok(()));
    assert_eq!(
        with(FbxLimits {
            max_vertices: 3,
            ..FbxLimits::default()
        }),
        Err("fbx_limit")
    );
    assert_eq!(
        with(FbxLimits {
            max_indices: 6,
            ..FbxLimits::default()
        }),
        Err("fbx_limit")
    );
    assert_eq!(
        with(FbxLimits {
            max_record_depth: 2,
            ..FbxLimits::default()
        }),
        Err("fbx_too_deep")
    );
    assert_eq!(
        with(FbxLimits {
            max_file_bytes: 64,
            ..FbxLimits::default()
        }),
        Err("fbx_too_large")
    );
    assert_eq!(
        with(FbxLimits {
            max_memory_bytes: 4096,
            ..FbxLimits::default()
        }),
        Err("fbx_limit")
    );
    // Too many nodes.
    let mut w = FbxWriter::new(100.0, 1);
    for i in 0..10 {
        w.model(200 + i, &format!("N{i}"), "Null", [0.; 3], [0.; 3], [1.; 3]);
        w.connect(200 + i, 0);
    }
    let (points, quads) = cube(1.0);
    w.geometry(100, "Cube", &points, &quads, None, &[], None);
    w.connect(100, 200);
    let many = w.finish();
    let result = convert_fbx_to_glb(
        many.as_bytes(),
        None,
        &FbxImportSettings {
            limits: FbxLimits {
                max_nodes: 4,
                ..FbxLimits::default()
            },
            ..settings()
        },
    );
    assert_eq!(result.map(|_| ()).map_err(|e| e.code()), Err("fbx_limit"));
}

#[test]
fn a_scene_without_geometry_is_an_error_not_a_panic() {
    let mut w = FbxWriter::new(100.0, 1);
    w.model(200, "Empty", "Null", [0.; 3], [0.; 3], [1.; 3]);
    w.connect(200, 0);
    let result = convert_fbx_to_glb(w.finish().as_bytes(), None, &settings());
    assert_eq!(result.map(|_| ()).map_err(|e| e.code()), Err("fbx_no_mesh"));
    let err = import_fbx_bytes(
        w.finish().as_bytes(),
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
        &settings(),
    )
    .expect_err("no mesh");
    assert!(matches!(err, ImportError::Fbx(FbxError::NoMesh)), "{err}");
}

/// A deterministic byte generator (64-bit LCG, Knuth's MMIX constants).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Runs the whole import on `bytes`; it may fail, it must not panic.
fn survives(bytes: &[u8]) {
    let limits = FbxLimits {
        max_memory_bytes: 64 << 20,
        ..FbxLimits::default()
    };
    let settings = FbxImportSettings { limits, ..settings() };
    if let Ok(conversion) = convert_fbx_to_glb(bytes, None, &settings) {
        // What converts must also import (or fail cleanly) through the glTF importer.
        let _ = import_fbx_bytes(
            bytes,
            AssetId::new_random(),
            Forward::Convert,
            "linux",
            TextureCookQuality::Low,
            &settings,
        );
        assert!(
            gltf::Gltf::from_slice(&conversion.glb).is_ok(),
            "converted GLB must be valid"
        );
    }
}

#[test]
fn malformed_ascii_files_fail_cleanly() {
    let seeds = [
        skinned_file(),
        cube_file(1.0, 2, 50.0),
        textured_file("a.png", ""),
    ];
    for seed in &seeds {
        let bytes = seed.as_bytes();
        // Truncations.
        for cut in (0..bytes.len()).step_by(97) {
            survives(&bytes[..cut]);
        }
        // Byte replacements at deterministic positions.
        let mut rng = Lcg(0x5EED);
        for _ in 0..200 {
            let mut bad = bytes.to_vec();
            for _ in 0..4 {
                let at = (rng.next() as usize) % bad.len();
                bad[at] = (rng.next() & 0xFF) as u8;
            }
            survives(&bad);
        }
    }
    // Hand-made hostile cases: out-of-range polygon indices, absurd array counts, NaN and huge
    // values, negative-only polygons, a deep block nest, a missing closing brace.
    let cube = cube_file(1.0, 1, 1.0);
    let hostile = [
        cube.replace("a: 0,3,2,-2", "a: 0,99999,2,-70000"),
        cube.replace("Vertices: *24", "Vertices: *2147483647"),
        cube.replace("PolygonVertexIndex: *24", "PolygonVertexIndex: *18446744073709551615"),
        cube.replace("a: 0,0,0,1,0,0", "a: nan,inf,-inf,1e308,0,0"),
        cube.replace("a: 0,3,2,-2", "a: -1,-1,-1,-1"),
        format!("{}{}", "A: {".repeat(100_000), "}".repeat(100_000)),
        cube.trim_end_matches(['}', '\n']).to_string(),
        "Objects: { Geometry: 1, \"Geometry::G\", \"Mesh\" { Vertices: *3 { a: 1,2,3 } PolygonVertexIndex: *3 { a: 0,0,-1 } } }\nConnections: { C: \"OO\",1,1 }".to_string(),
    ];
    for case in &hostile {
        survives(case.as_bytes());
    }
    // A deep nest is refused by the Rust pre-scan, before the C parser.
    let deep = format!("{}{}", "A: {".repeat(1000), "}".repeat(1000));
    assert_eq!(
        prescan_fbx(deep.as_bytes(), &FbxLimits::default()).map_err(|e| e.code()),
        Err("fbx_too_deep")
    );
}

/// A binary FBX 7.4 file with one empty top-level record per name, for mutation.
fn binary_seed() -> Vec<u8> {
    let mut out = b"Kaydara FBX Binary  \x00\x1a\x00".to_vec();
    out.extend_from_slice(&7400u32.to_le_bytes());
    for name in ["FBXHeaderExtension", "GlobalSettings", "Objects", "Connections"] {
        // One child record with an `I` (i32) property, then the null terminator.
        let start = out.len();
        let child_start = start + 13 + name.len();
        let child_end = child_start + 13 + 1 + 5;
        let end = child_end + 13;
        out.extend_from_slice(&(end as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(child_end as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&5u32.to_le_bytes());
        out.push(1);
        out.push(b'V');
        out.push(b'I');
        out.extend_from_slice(&7400i32.to_le_bytes());
        out.extend_from_slice(&[0u8; 13]);
    }
    out.extend_from_slice(&[0u8; 13]);
    out
}

#[test]
fn malformed_binary_files_fail_cleanly() {
    let seed = binary_seed();
    assert!(prescan_fbx(&seed, &FbxLimits::default()).is_ok());
    survives(&seed);
    for cut in 0..seed.len() {
        survives(&seed[..cut]);
    }
    let mut rng = Lcg(0xB1AA);
    for _ in 0..400 {
        let mut bad = seed.clone();
        for _ in 0..3 {
            let at = 23 + (rng.next() as usize) % (bad.len() - 23);
            bad[at] = (rng.next() & 0xFF) as u8;
        }
        survives(&bad);
    }
    // Pure noise after the magic.
    let mut noise = b"Kaydara FBX Binary  \x00\x1a\x00".to_vec();
    noise.extend_from_slice(&7500u32.to_le_bytes());
    for _ in 0..4096 {
        noise.push((rng.next() & 0xFF) as u8);
    }
    survives(&noise);
}

// --- OBJ (pure Rust, same converted-glTF path) -------------------------------------------------

/// The unit cube as OBJ text (the same quads as [`cube`], 1-based indices, face normals).
fn cube_obj() -> String {
    let (points, quads) = cube(1.0);
    let normals = quad_normals(&points, &quads);
    let mut out = String::from("# unit cube\nmtllib cube.mtl\no Cube\nusemtl Red\n");
    for p in &points {
        out += &format!("v {} {} {}\n", p[0], p[1], p[2]);
    }
    for (q, _) in quads.iter().enumerate() {
        let n = normals[q * 4];
        out += &format!("vn {} {} {}\n", n[0], n[1], n[2]);
    }
    for (q, quad) in quads.iter().enumerate() {
        let corners: Vec<String> = quad.iter().map(|v| format!("{}//{}", v + 1, q + 1)).collect();
        out += &format!("f {}\n", corners.join(" "));
    }
    out
}

#[test]
fn the_same_cube_as_gltf_and_obj_cooks_equal_geometry() {
    use regia_asset_pipeline::obj_import::import_obj_path;
    let dir = regia_test_utils::unique_temp_dir("obj-cube");
    std::fs::write(dir.join("cube.obj"), cube_obj()).expect("obj");
    std::fs::write(dir.join("cube.mtl"), "newmtl Red\nKd 1 0 0\nNs 10\n").expect("mtl");
    let obj = import_obj_path(
        &dir.join("cube.obj"),
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::Low,
    )
    .unwrap_or_else(|e| panic!("OBJ import: {e}"));
    assert_eq!(obj.report.objects, 1);
    assert_eq!(obj.report.materials, 1);
    assert_eq!(obj.report.triangles, 12);
    let gltf = import_gltf_glb_bytes(
        &cube_glb(),
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        TextureCookQuality::Low,
    )
    .expect("glTF import");
    let obj_mesh = cpu(&obj.gltf.primary_mesh);
    let gltf_mesh = cpu(&gltf.primary_mesh);
    assert_eq!(triangle_set(&obj_mesh), triangle_set(&gltf_mesh));
    assert!(
        obj.gltf
            .sub_assets
            .iter()
            .any(|s| s.type_path.ends_with("CookedStandardMaterial") && s.path.contains("Red"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn obj_negative_indices_texcoords_colors_and_textures() {
    use regia_asset_pipeline::obj_import::{ObjImportSettings, convert_obj_to_glb};
    let dir = regia_test_utils::unique_temp_dir("obj-features");
    write_png(&dir.join("albedo.png"));
    std::fs::write(
        dir.join("card.mtl"),
        "newmtl Paint\nKd 1 1 1\nmap_Kd -o 0 0 albedo.png\nmap_Bump missing.png\n",
    )
    .expect("mtl");
    let obj = "mtllib card.mtl ../outside.mtl\no Card\nusemtl Paint\n\
               v 0 0 0 1 0 0\nv 1 0 0 0 1 0\nv 1 1 0 0 0 1\nv 0 1 0 1 1 1\n\
               vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\n\
               f -4/-4 -3/-3 -2/-2 -1/-1\n";
    let conversion =
        convert_obj_to_glb(obj.as_bytes(), Some(&dir), &ObjImportSettings::default()).expect("convert");
    assert_eq!(conversion.report.triangles, 2);
    assert_eq!(conversion.report.textures, vec!["albedo.png".to_string()]);
    assert!(
        conversion
            .report
            .warnings
            .iter()
            .any(|w| w.contains("missing.png")),
        "{:?}",
        conversion.report.warnings
    );
    assert!(
        conversion
            .report
            .warnings
            .iter()
            .any(|w| w.contains("outside.mtl")),
        "{:?}",
        conversion.report.warnings
    );
    let gltf = gltf::Gltf::from_slice(&conversion.glb).expect("valid glTF");
    let blob = gltf.blob.clone().expect("bin");
    let primitive = gltf
        .document
        .meshes()
        .next()
        .expect("mesh")
        .primitives()
        .next()
        .expect("prim");
    let reader = primitive.reader(|_| Some(blob.as_slice()));
    let positions: Vec<[f32; 3]> = reader.read_positions().expect("positions").collect();
    let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).expect("uv").into_f32().collect();
    let colors: Vec<[f32; 4]> = reader.read_colors(0).expect("colors").into_rgba_f32().collect();
    let corner = positions
        .iter()
        .position(|p| close(p[0], 1.0) && close(p[1], 1.0))
        .expect("corner");
    assert!(
        close(uvs[corner][0], 1.0) && close(uvs[corner][1], 0.0),
        "V flipped: {:?}",
        uvs[corner]
    );
    assert!(close(colors[corner][2], 1.0), "{:?}", colors[corner]);
    let material = gltf.document.materials().next().expect("material");
    assert!(material.pbr_metallic_roughness().base_color_texture().is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_obj_files_fail_cleanly() {
    use regia_asset_pipeline::obj_import::{ObjImportSettings, ObjLimits, convert_obj_to_glb};
    let settings = ObjImportSettings::default();
    let code = |text: &str| {
        convert_obj_to_glb(text.as_bytes(), None, &settings)
            .map(|_| ())
            .map_err(|e| e.code())
    };
    assert_eq!(code("v 0 0 0\nv 1 0 0\nf 1 2 3\n"), Err("obj_parse"));
    assert_eq!(code("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2\n"), Err("obj_parse"));
    assert_eq!(code("v 0 0 x\n"), Err("obj_parse"));
    assert_eq!(code("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1/9 2 3\n"), Err("obj_parse"));
    assert_eq!(code("v 0 0 0\n"), Err("obj_no_mesh"));
    assert_eq!(code(""), Err("obj_no_mesh"));
    assert_eq!(code("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n"), Ok(()));
    let limited = ObjImportSettings {
        limits: ObjLimits {
            max_vertices: 2,
            ..ObjLimits::default()
        },
        ..ObjImportSettings::default()
    };
    assert_eq!(
        convert_obj_to_glb(b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n", None, &limited)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("obj_limit")
    );
    // Mutations of a valid file: never a panic.
    let seed = cube_obj();
    let mut rng = Lcg(0x0B1);
    for _ in 0..500 {
        let mut bad = seed.clone().into_bytes();
        for _ in 0..3 {
            let at = (rng.next() as usize) % bad.len();
            bad[at] = b"0123456789-/ \nvfo#."[(rng.next() as usize) % 19];
        }
        if let Ok(conversion) = convert_obj_to_glb(&bad, None, &settings) {
            assert!(gltf::Gltf::from_slice(&conversion.glb).is_ok());
        }
    }
    for cut in 0..seed.len() {
        let _ = convert_obj_to_glb(&seed.as_bytes()[..cut], None, &settings);
    }
}

// --- Binary FBX: the ASCII writer's output re-encoded as FBX 7.4 binary -------------------------

/// A property of a binary FBX record.
enum BinProp {
    Int(i64),
    Real(f64),
    Str(Vec<u8>),
    Reals(Vec<f64>),
    Ints(Vec<i32>),
    Longs(Vec<i64>),
}

/// One record: name, properties, children.
struct BinNode {
    name: String,
    props: Vec<BinProp>,
    children: Vec<BinNode>,
}

/// Splits `a, "b, c", 3` at top-level commas.
fn split_values(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out.into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// A scalar property; `Class::Name` object names become the binary `Name\0\x01Class` form.
fn scalar(value: &str, object_name: bool) -> BinProp {
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        if object_name && let Some((class, name)) = inner.split_once("::") {
            let mut bytes = name.as_bytes().to_vec();
            bytes.extend_from_slice(b"\x00\x01");
            bytes.extend_from_slice(class.as_bytes());
            return BinProp::Str(bytes);
        }
        return BinProp::Str(inner.as_bytes().to_vec());
    }
    if let Ok(i) = value.parse::<i64>() {
        return BinProp::Int(i);
    }
    BinProp::Real(value.parse::<f64>().unwrap_or(0.0))
}

/// Parses the ASCII writer's output (`Name: values {` … `}`, `*N { a: … }` arrays, `;` comments).
fn parse_ascii(text: &str) -> Vec<BinNode> {
    let mut stack: Vec<BinNode> = vec![BinNode {
        name: String::new(),
        props: Vec::new(),
        children: Vec::new(),
    }];
    // The name of an array node waiting for its `a:` line.
    let mut array: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line == "}" {
            // Closes a block or an array node alike.
            array = None;
            let node = stack.pop().expect("balanced");
            stack.last_mut().expect("parent").children.push(node);
            continue;
        }
        let (name, rest) = line.split_once(':').expect("Name: values");
        let rest = rest.trim();
        if let Some(name_of_array) = array.as_ref()
            && name == "a"
        {
            let values = split_values(rest);
            let prop = match name_of_array.as_str() {
                "PolygonVertexIndex" | "UVIndex" | "ColorIndex" | "Indexes" | "Materials"
                | "KeyAttrFlags" | "KeyAttrRefCount" => {
                    BinProp::Ints(values.iter().map(|v| v.parse().unwrap_or(0)).collect())
                }
                "KeyTime" => BinProp::Longs(values.iter().map(|v| v.parse().unwrap_or(0)).collect()),
                _ => BinProp::Reals(values.iter().map(|v| v.parse().unwrap_or(0.0)).collect()),
            };
            stack.last_mut().expect("array node").props.push(prop);
            continue;
        }
        if rest.starts_with('*') {
            // `Name: *N {` opens an array node; its values come on the `a:` line.
            stack.push(BinNode {
                name: name.to_string(),
                props: Vec::new(),
                children: Vec::new(),
            });
            array = Some(name.to_string());
            continue;
        }
        let opens = rest.ends_with('{');
        let values = rest.trim_end_matches('{');
        let in_objects = stack.len() == 2 && stack[1].name == "Objects";
        let props = split_values(values)
            .iter()
            .enumerate()
            .map(|(k, v)| scalar(v, in_objects && k == 1))
            .collect();
        let node = BinNode {
            name: name.to_string(),
            props,
            children: Vec::new(),
        };
        if opens {
            stack.push(node);
        } else {
            stack.last_mut().expect("parent").children.push(node);
        }
    }
    stack.pop().expect("root").children
}

fn encode_prop(prop: &BinProp, out: &mut Vec<u8>) {
    match prop {
        BinProp::Int(i) => {
            out.push(b'L');
            out.extend_from_slice(&i.to_le_bytes());
        }
        BinProp::Real(f) => {
            out.push(b'D');
            out.extend_from_slice(&f.to_le_bytes());
        }
        BinProp::Str(bytes) => {
            out.push(b'S');
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        BinProp::Reals(values) => {
            out.push(b'd');
            out.extend_from_slice(&(values.len() as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&((values.len() * 8) as u32).to_le_bytes());
            values
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        }
        BinProp::Ints(values) => {
            out.push(b'i');
            out.extend_from_slice(&(values.len() as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&((values.len() * 4) as u32).to_le_bytes());
            values
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        }
        BinProp::Longs(values) => {
            out.push(b'l');
            out.extend_from_slice(&(values.len() as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&((values.len() * 8) as u32).to_le_bytes());
            values
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        }
    }
}

/// Appends one 7.4 record (32-bit header fields) at the end of `out`.
fn encode_node(node: &BinNode, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0u8; 12]);
    out.push(node.name.len() as u8);
    out.extend_from_slice(node.name.as_bytes());
    let props_start = out.len();
    for prop in &node.props {
        encode_prop(prop, out);
    }
    let props_len = out.len() - props_start;
    for child in &node.children {
        encode_node(child, out);
    }
    // A child list (or an empty record) ends with a null record.
    if !node.children.is_empty() || node.props.is_empty() {
        out.extend_from_slice(&[0u8; 13]);
    }
    let end = out.len() as u32;
    out[start..start + 4].copy_from_slice(&end.to_le_bytes());
    out[start + 4..start + 8].copy_from_slice(&(node.props.len() as u32).to_le_bytes());
    out[start + 8..start + 12].copy_from_slice(&(props_len as u32).to_le_bytes());
}

/// The ASCII file `text` as binary FBX 7.4.
fn to_binary(text: &str) -> Vec<u8> {
    let mut out = b"Kaydara FBX Binary  \x00\x1a\x00".to_vec();
    out.extend_from_slice(&7400u32.to_le_bytes());
    for node in parse_ascii(text) {
        encode_node(&node, &mut out);
    }
    out.extend_from_slice(&[0u8; 13]);
    out
}

#[test]
fn binary_and_ascii_encodings_of_one_scene_import_the_same() {
    for (label, ascii) in [
        ("cube", cube_file(1.0, 1, 100.0)),
        ("z-up cube", cube_file(100.0, 2, 1.0)),
        ("skinned", skinned_file()),
    ] {
        let binary = to_binary(&ascii);
        assert_eq!(
            prescan_fbx(&binary, &FbxLimits::default()),
            Ok(regia_asset_pipeline::fbx_import::FbxEncoding::Binary { version: 7400 }),
            "{label}"
        );
        let from_ascii = convert_fbx_to_glb(ascii.as_bytes(), None, &settings()).expect("ascii");
        let from_binary =
            convert_fbx_to_glb(&binary, None, &settings()).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert!(!from_binary.report.ascii, "{label}");
        let mut a = from_ascii.report.clone();
        let mut b = from_binary.report.clone();
        for r in [&mut a, &mut b] {
            r.ascii = false;
            r.warnings.clear();
        }
        assert_eq!(a, b, "{label}: same report from both encodings");
        let mesh = |bytes: &[u8]| {
            let imported = import_fbx_bytes(
                bytes,
                AssetId::new_random(),
                Forward::Convert,
                "linux",
                TextureCookQuality::Low,
                &settings(),
            )
            .expect("import");
            cpu(&imported.gltf.primary_mesh)
        };
        assert_eq!(
            triangle_set(&mesh(ascii.as_bytes())),
            triangle_set(&mesh(&binary)),
            "{label}"
        );
    }
}

#[test]
fn malformed_realistic_binary_files_fail_cleanly() {
    let seed = to_binary(&skinned_file());
    survives(&seed);
    let mut rng = Lcg(0xFB7);
    for _ in 0..400 {
        let mut bad = seed.clone();
        for _ in 0..2 {
            let at = 27 + (rng.next() as usize) % (bad.len() - 27);
            bad[at] = (rng.next() & 0xFF) as u8;
        }
        survives(&bad);
    }
    for cut in (27..seed.len()).step_by(11) {
        survives(&seed[..cut]);
    }
}

/// A 4×4 gray PNG of `value`.
fn write_gray_png(path: &Path, value: u8) {
    image::GrayImage::from_pixel(4, 4, image::Luma([value]))
        .save(path)
        .expect("png");
}

#[test]
fn pbr_roughness_and_metalness_maps_pack_into_one_metallic_roughness_texture() {
    let dir = regia_test_utils::unique_temp_dir("fbx-orm");
    write_gray_png(&dir.join("rough.png"), 64);
    write_gray_png(&dir.join("metal.png"), 200);
    let mut w = FbxWriter::new(100.0, 1);
    let points = vec![[0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.]];
    let quads = vec![vec![0, 1, 2, 3]];
    w.geometry(
        100,
        "Card",
        &points,
        &quads,
        None,
        &[("map1", quad_uvs(&quads))],
        None,
    );
    w.model(200, "Card", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    // A 3ds Max "PBR Material (Metal/Rough)": class id 0xd00f1e00 / 0xbe77e500, `3dsMax|main|` props,
    // maps on the `_map` properties.
    w.objects.push(
        "\tMaterial: 300, \"Material::Steel\", \"\" {\n\t\tVersion: 102\n\t\tShadingModel: \"unknown\"\n\
         \t\tProperties70:  {\n\
         \t\t\tP: \"3dsMax|ClassIDa\", \"int\", \"Integer\", \"\",3490651648\n\
         \t\t\tP: \"3dsMax|ClassIDb\", \"int\", \"Integer\", \"\",3195528448\n\
         \t\t\tP: \"3dsMax|main|base_color\", \"ColorAndAlpha\", \"\", \"A\",0.5,0.5,0.5,1\n\
         \t\t\tP: \"3dsMax|main|roughness\", \"Float\", \"\", \"A\",0.5\n\
         \t\t\tP: \"3dsMax|main|metalness\", \"Float\", \"\", \"A\",1\n\
         \t\t}\n\t}\n"
            .to_string(),
    );
    w.texture(400, "Rough", "rough.png", "");
    w.texture(401, "Metal", "metal.png", "");
    w.connect(200, 0);
    w.connect(100, 200);
    w.connect(300, 200);
    w.connect_prop(400, 300, "3dsMax|main|roughness_map");
    w.connect_prop(401, 300, "3dsMax|main|metalness_map");
    let fbx = w.finish();
    let conversion = convert_fbx_to_glb(fbx.as_bytes(), Some(&dir), &settings()).expect("convert");
    let roles: Vec<&str> = conversion
        .report
        .textures
        .iter()
        .map(|t| t.role.as_str())
        .collect();
    assert!(
        roles.contains(&"roughness") && roles.contains(&"metalness"),
        "{:?}",
        conversion.report
    );
    let gltf = gltf::Gltf::from_slice(&conversion.glb).expect("valid glTF");
    let blob = gltf.blob.clone().expect("bin");
    let material = gltf.document.materials().next().expect("material");
    let pbr = material.pbr_metallic_roughness();
    assert_eq!(pbr.roughness_factor(), 1.0);
    assert_eq!(pbr.metallic_factor(), 1.0);
    let info = pbr.metallic_roughness_texture().expect("packed map");
    let gltf::image::Source::View { view, .. } = info.texture().source().source() else {
        panic!("embedded image");
    };
    let bytes = &blob[view.offset()..view.offset() + view.length()];
    let packed = image::load_from_memory(bytes).expect("png").to_rgba8();
    let texel = packed.get_pixel(1, 1);
    assert_eq!((texel[1], texel[2]), (64, 200), "roughness in G, metalness in B");
    // The cook packs it into the ORM map like any glTF metallic-roughness texture.
    let fbx_path = dir.join("steel.fbx");
    std::fs::write(&fbx_path, &fbx).expect("write");
    let imported = import_fbx_path(
        &fbx_path,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::Low,
    )
    .unwrap_or_else(|e| panic!("import: {e}"));
    let material = imported
        .gltf
        .sub_assets
        .iter()
        .find(|s| s.type_path.ends_with("CookedStandardMaterial"))
        .expect("material");
    assert!(
        !material.dependencies.is_empty(),
        "the material depends on its packed map"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_shininess_maps_are_reported_not_packed() {
    let dir = regia_test_utils::unique_temp_dir("fbx-shininess");
    write_gray_png(&dir.join("gloss.png"), 128);
    let mut w = FbxWriter::new(100.0, 1);
    let points = vec![[0., 0., 0.], [1., 0., 0.], [1., 1., 0.]];
    w.geometry(100, "Tri", &points, &[vec![0, 1, 2]], None, &[], None);
    w.model(200, "Tri", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.objects.push(
        "\tMaterial: 300, \"Material::Shiny\", \"\" {\n\t\tVersion: 102\n\t\tShadingModel: \"phong\"\n\t}\n"
            .to_string(),
    );
    w.texture(400, "Gloss", "gloss.png", "");
    w.connect(200, 0);
    w.connect(100, 200);
    w.connect(300, 200);
    w.connect_prop(400, 300, "ShininessExponent");
    let conversion = convert_fbx_to_glb(w.finish().as_bytes(), Some(&dir), &settings()).expect("convert");
    let gltf = gltf::Gltf::from_slice(&conversion.glb).expect("valid glTF");
    let material = gltf.document.materials().next().expect("material");
    assert!(
        material
            .pbr_metallic_roughness()
            .metallic_roughness_texture()
            .is_none()
    );
    assert!(
        conversion.report.warnings.iter().any(|w| w.contains("shininess")),
        "{:?}",
        conversion.report.warnings
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn animation_in_a_z_up_centimeter_file_moves_along_y_in_meters() {
    // A box under node `Lift` that rises 100 cm along the file's up axis (Z) over 1 s.
    let mut w = FbxWriter::new(1.0, 2);
    let (points, quads) = cube(10.0);
    w.geometry(100, "Box", &points, &quads, None, &[], None);
    w.model(200, "Lift", "Null", [0.; 3], [0.; 3], [1.; 3]);
    w.model(201, "Box", "Mesh", [0.; 3], [0.; 3], [1.; 3]);
    w.connect(200, 0);
    w.connect(201, 200);
    w.connect(100, 201);
    let layer = w.anim_stack(500, 501, "Rise", 1.0);
    w.anim_curves(
        510,
        layer,
        200,
        "Lcl Translation",
        &[
            ("X", vec![(0.0, 0.0), (1.0, 0.0)]),
            ("Y", vec![(0.0, 0.0), (1.0, 0.0)]),
            ("Z", vec![(0.0, 0.0), (1.0, 100.0)]),
        ],
    );
    for (label, bytes) in [
        ("ascii", w.finish().into_bytes()),
        ("binary", to_binary(&w.finish())),
    ] {
        let conversion =
            convert_fbx_to_glb(&bytes, None, &settings()).unwrap_or_else(|e| panic!("{label}: {e}"));
        let gltf = gltf::Gltf::from_slice(&conversion.glb).expect("valid glTF");
        let blob = gltf.blob.clone().expect("bin");
        let animation = gltf.document.animations().next().expect("animation");
        let channel = animation
            .channels()
            .find(|c| c.target().property() == gltf::animation::Property::Translation)
            .expect("translation");
        assert_eq!(channel.target().node().name(), Some("Lift"), "{label}");
        let reader = channel.reader(|_| Some(blob.as_slice()));
        let values: Vec<[f32; 3]> = match reader.read_outputs().expect("outputs") {
            gltf::animation::util::ReadOutputs::Translations(t) => t.collect(),
            _ => panic!("translations"),
        };
        // The rise is +Y in the converted frame, 1 m at the end. The root's axis conversion may
        // sit on the animated node's rest rotation; its translation is in its parent's frame.
        let first = values.first().expect("first");
        let last = values.last().expect("last");
        let delta = [last[0] - first[0], last[1] - first[1], last[2] - first[2]];
        assert!(
            close(delta[1], 1.0) && close(delta[0], 0.0) && close(delta[2], 0.0),
            "{label}: {values:?}"
        );
    }
}

/// v0.5 acceptance (evidence row B17, ADR-4898): the skinned FBX cooks through the project cook,
/// the shipping pak writer and the pak reader with its skeleton, skin and clip intact; a later
/// broken edit is a per-file error that leaves those cooked files as they were.
#[test]
fn a_skinned_fbx_ships_its_skeleton_skin_and_clip_through_the_pak() {
    use regia_asset::{PakReader, cooked_cache_dir, decode_cooked_file};
    use regia_asset_pipeline::{CookOptions, cook_project, host_platform, pack_cooked_directory_with};
    let root = regia_test_utils::unique_temp_dir("fbx-pak");
    std::fs::create_dir_all(root.join("assets/chars")).expect("assets");
    std::fs::write(root.join("assets/chars/strip.fbx"), skinned_file()).expect("write");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = cooked_cache_dir(&root, host_platform());
    let out = root.join("build/paks");
    let paks = pack_cooked_directory_with(&cache, &out, &BTreeMap::new(), true, false).expect("pack");
    let reader = PakReader::open(&paks[0]).expect("open pak");
    let mut shipped: BTreeMap<String, (String, Vec<u8>)> = BTreeMap::new();
    for entry in reader.manifest() {
        let bytes = reader.read_decompressed(entry.id).expect("read");
        let (_, payload) = decode_cooked_file(&bytes).expect("cooked envelope");
        shipped.insert(entry.path.clone(), (entry.type_path.clone(), payload.to_vec()));
    }
    let of_type = |suffix: &str| -> Vec<&Vec<u8>> {
        shipped
            .iter()
            .filter(|(path, (ty, _))| path.starts_with("assets/chars/strip.fbx") && ty.ends_with(suffix))
            .map(|(_, (_, payload))| payload)
            .collect()
    };
    let skeletons = of_type("::Skeleton");
    assert_eq!(skeletons.len(), 1, "{:?}", shipped.keys());
    let skeleton: regia_animation::Skeleton = postcard::from_bytes(skeletons[0]).expect("skeleton decodes");
    let mut joints = skeleton.joint_names.clone();
    joints.sort();
    assert_eq!(joints, vec!["Root".to_string(), "Tip".to_string()]);
    let tip = skeleton.joint_names.iter().position(|n| n == "Tip").expect("tip");
    let root_joint = skeleton
        .joint_names
        .iter()
        .position(|n| n == "Root")
        .expect("root");
    assert_eq!(
        skeleton.parents[tip],
        Some(root_joint as u32),
        "Tip stays Root's child"
    );
    assert_eq!(skeleton.parents[root_joint], None);
    assert!(close(skeleton.inverse_bind_matrices[tip].w_axis.y, -1.0));

    // The mesh sub-asset (the primary row is the same mesh under the file's own path).
    let meshes: Vec<&Vec<u8>> = shipped
        .iter()
        .filter(|(path, (ty, _))| {
            path.starts_with("assets/chars/strip.fbx#mesh/") && ty.ends_with("::CookedMesh")
        })
        .map(|(_, (_, payload))| payload)
        .collect();
    assert_eq!(meshes.len(), 1, "{:?}", shipped.keys());
    let mesh = CookedMesh::from_postcard(meshes[0])
        .expect("mesh decodes")
        .to_cpu_mesh()
        .expect("unpack");
    let skin = mesh.skin.as_ref().expect("the shipped mesh is skinned");
    assert_eq!(skin.joints.len(), mesh.positions.len());
    let mut skin_joints = skin.joint_names.clone();
    skin_joints.sort();
    assert_eq!(skin_joints, joints);

    let clips = of_type("::AnimationClip");
    assert_eq!(clips.len(), 1, "{:?}", shipped.keys());
    let clip = regia_animation::AnimationClip::from_cooked_payload(clips[0]).expect("clip decodes");
    assert!(close(clip.duration, 1.0), "{}", clip.duration);
    assert!(!clip.tracks.is_empty());

    // A broken edit: one per-file error, and the cooked files the pak was built from stay.
    let before: Vec<(std::path::PathBuf, Vec<u8>)> = std::fs::read_dir(&cache)
        .expect("cache")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "fckd"))
        .map(|p| {
            let bytes = std::fs::read(&p).expect("read");
            (p, bytes)
        })
        .collect();
    std::fs::write(
        root.join("assets/chars/strip.fbx"),
        "; FBX 7.4.0 project file\nObjects: {",
    )
    .expect("broken");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("the cook runs");
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(
        report.errors[0].0.ends_with("chars/strip.fbx"),
        "{:?}",
        report.errors
    );
    for (path, bytes) in &before {
        assert_eq!(&std::fs::read(path).expect("kept"), bytes, "{}", path.display());
    }
    let _ = std::fs::remove_dir_all(&root);
}
