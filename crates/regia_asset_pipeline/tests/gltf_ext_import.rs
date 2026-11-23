// SPDX-License-Identifier: MPL-2.0
//! glTF data-layout and selection extensions on import (T2.14, ADR-3016): `KHR_mesh_quantization`,
//! `EXT_meshopt_compression` with its fallback buffer, `EXT_mesh_gpu_instancing`,
//! `KHR_materials_variants`, `EXT_texture_webp`, and the refusal of a required extension the importer
//! cannot read. The fixtures are built here as JSON plus binary.

use regia_asset::{
    AssetId, CookedStandardMaterial, CpuMesh, MaterialVariantSet, PrefabInstance, PrefabInstanceSet,
    PrefabVariantSelection,
};
use regia_asset_pipeline::{
    Forward, GltfImportResult, ImportError, TextureCookQuality, import_gltf_glb_bytes, import_gltf_path,
    open_gltf,
};
use regia_test_utils::fuzz::fuzz_loader;
use serde_json::{Value, json};

/// A GLB of `json` with `bin` as its binary chunk (both padded to four bytes).
fn glb(json: &Value, bin: &[u8]) -> Vec<u8> {
    let mut json = serde_json::to_vec(json).expect("json");
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
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

fn import_keep(bytes: &[u8]) -> Result<GltfImportResult, ImportError> {
    import_gltf_glb_bytes(
        bytes,
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        TextureCookQuality::High,
    )
}

/// The mesh of the first `mesh/...` sub-asset, unpacked.
fn first_mesh(result: &GltfImportResult) -> CpuMesh {
    result.primary_mesh.to_cpu_mesh().expect("unpack")
}

fn floats(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A quad as reference floats: the positions, normals, UVs and colors the quantized forms encode.
const QUAD_POSITIONS: [[f32; 3]; 4] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
const QUAD_UVS: [[f32; 2]; 4] = [[0.0, 0.0], [0.5, 0.0], [0.5, 0.25], [0.0, 0.25]];
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

/// One attribute's encoding: its component type, whether it is normalized, and the encoder.
struct Quantized {
    component: u32,
    normalized: bool,
    encode: fn(f32) -> Vec<u8>,
    /// One quantization step in the attribute's float space.
    step: f32,
}

const I8_NORM: Quantized = Quantized {
    component: 5120,
    normalized: true,
    encode: |v| vec![(v * 127.0).round() as i8 as u8],
    step: 1.0 / 127.0,
};
const U8_NORM: Quantized = Quantized {
    component: 5121,
    normalized: true,
    encode: |v| vec![(v * 255.0).round() as u8],
    step: 1.0 / 255.0,
};
const I16_NORM: Quantized = Quantized {
    component: 5122,
    normalized: true,
    encode: |v| ((v * 32767.0).round() as i16).to_le_bytes().to_vec(),
    step: 1.0 / 32767.0,
};
const U16_NORM: Quantized = Quantized {
    component: 5123,
    normalized: true,
    encode: |v| ((v * 65535.0).round() as u16).to_le_bytes().to_vec(),
    step: 1.0 / 65535.0,
};

/// The file's scale for a plain integer form; the node's matrix undoes it, as a quantizer would write.
const RAW_SCALE: f32 = 64.0;
const I8_RAW: Quantized = Quantized {
    component: 5120,
    normalized: false,
    encode: |v| vec![(v * RAW_SCALE).round() as i8 as u8],
    step: 1.0 / RAW_SCALE,
};
const U8_RAW: Quantized = Quantized {
    component: 5121,
    normalized: false,
    encode: |v| vec![(v * RAW_SCALE).round() as u8],
    step: 1.0 / RAW_SCALE,
};
const I16_RAW: Quantized = Quantized {
    component: 5122,
    normalized: false,
    encode: |v| ((v * RAW_SCALE).round() as i16).to_le_bytes().to_vec(),
    step: 1.0 / RAW_SCALE,
};
const U16_RAW: Quantized = Quantized {
    component: 5123,
    normalized: false,
    encode: |v| ((v * RAW_SCALE).round() as u16).to_le_bytes().to_vec(),
    step: 1.0 / RAW_SCALE,
};

/// Appends an accessor's bytes at a 4-byte aligned, 4-byte strided view, as the extension requires.
struct Builder {
    bin: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Builder {
    fn new() -> Self {
        Self {
            bin: Vec::new(),
            views: Vec::new(),
            accessors: Vec::new(),
        }
    }

    fn add(&mut self, form: &Quantized, kind: &str, values: &[Vec<f32>]) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let start = self.bin.len();
        let mut stride = 0;
        for element in values {
            let mut bytes: Vec<u8> = element.iter().flat_map(|c| (form.encode)(*c)).collect();
            while !bytes.len().is_multiple_of(4) {
                bytes.push(0);
            }
            stride = bytes.len();
            self.bin.extend_from_slice(&bytes);
        }
        self.views.push(json!({
            "buffer": 0, "byteOffset": start, "byteLength": self.bin.len() - start, "byteStride": stride,
        }));
        let mut accessor = json!({
            "bufferView": self.views.len() - 1,
            "componentType": form.component,
            "normalized": form.normalized,
            "count": values.len(),
            "type": kind,
        });
        if kind == "VEC3" {
            // In the stored integers: `min` and `max` of a real writer. The importer must not use them as floats.
            accessor["min"] = json!([0, 0, 0]);
            accessor["max"] = json!([1000, 1000, 1000]);
        }
        self.accessors.push(accessor);
        self.accessors.len() - 1
    }

    fn add_indices(&mut self, indices: &[u16]) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let start = self.bin.len();
        for i in indices {
            self.bin.extend_from_slice(&i.to_le_bytes());
        }
        self.views
            .push(json!({"buffer": 0, "byteOffset": start, "byteLength": indices.len() * 2}));
        self.accessors.push(json!({
            "bufferView": self.views.len() - 1, "componentType": 5123, "count": indices.len(), "type": "SCALAR",
        }));
        self.accessors.len() - 1
    }

    fn add_float3(&mut self, kind: &str, values: &[f32]) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let start = self.bin.len();
        self.bin.extend(floats(values));
        self.views
            .push(json!({"buffer": 0, "byteOffset": start, "byteLength": values.len() * 4}));
        self.accessors.push(json!({
            "bufferView": self.views.len() - 1, "componentType": 5126,
            "count": values.len() / if kind == "VEC4" { 4 } else { 3 }, "type": kind,
        }));
        self.accessors.len() - 1
    }

    fn document(&self, attributes: Value, indices: usize, required: bool) -> Value {
        let mut document = json!({
            "asset": {"version": "2.0"},
            "extensionsUsed": ["KHR_mesh_quantization"],
            "scene": 0, "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "name": "quad"}],
            "meshes": [{"name": "m", "primitives": [{"attributes": attributes, "indices": indices}]}],
            "buffers": [{"byteLength": self.bin.len()}],
            "bufferViews": self.views,
            "accessors": self.accessors,
        });
        if required {
            document["extensionsRequired"] = json!(["KHR_mesh_quantization"]);
        }
        document
    }
}

fn as_vec3(values: &[[f32; 3]]) -> Vec<Vec<f32>> {
    values.iter().map(|v| v.to_vec()).collect()
}

fn as_vec2(values: &[[f32; 2]]) -> Vec<Vec<f32>> {
    values.iter().map(|v| v.to_vec()).collect()
}

/// `POSITION` in `form`, within one step of the reference floats (the node's matrix applies a plain
/// integer form's scale; the cooked mesh's own rounding is far below a step).
#[track_caller]
fn check_positions(form: &Quantized, name: &str) {
    let mut build = Builder::new();
    let position = build.add(form, "VEC3", &as_vec3(&QUAD_POSITIONS));
    let indices = build.add_indices(&QUAD_INDICES);
    let mut document = build.document(json!({"POSITION": position}), indices, true);
    if !form.normalized {
        let s = 1.0 / RAW_SCALE;
        document["nodes"][0]["scale"] = json!([s, s, s]);
    }
    let mesh = first_mesh(&import_keep(&glb(&document, &build.bin)).expect(name));
    assert_eq!(mesh.positions.len(), 4, "{name}");
    assert_eq!(mesh.indices.len(), 6, "{name}");
    for reference in QUAD_POSITIONS {
        let close = mesh
            .positions
            .iter()
            .any(|p| (0..3).all(|axis| (p[axis] - reference[axis]).abs() <= form.step * 1.01));
        assert!(
            close,
            "{name}: no vertex near {reference:?} in {:?}",
            mesh.positions
        );
    }
}

#[test]
fn quantized_positions_decode_to_the_reference_floats_within_one_step() {
    check_positions(&I8_NORM, "BYTE normalized");
    check_positions(&U8_NORM, "UNSIGNED_BYTE normalized");
    check_positions(&I16_NORM, "SHORT normalized");
    check_positions(&U16_NORM, "UNSIGNED_SHORT normalized");
    check_positions(&I8_RAW, "BYTE");
    check_positions(&U8_RAW, "UNSIGNED_BYTE");
    check_positions(&I16_RAW, "SHORT");
    check_positions(&U16_RAW, "UNSIGNED_SHORT");
}

/// `TEXCOORD_0` in `form`, against the importer's own path for the same data as floats.
#[track_caller]
fn check_uvs(form: &Quantized, scale: f32, name: &str) {
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &QUAD_POSITIONS.concat());
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let uv = build.add(form, "VEC2", &as_vec2(&QUAD_UVS));
    let indices = build.add_indices(&QUAD_INDICES);
    let document = build.document(json!({"POSITION": position, "TEXCOORD_0": uv}), indices, true);
    let mesh = first_mesh(&import_keep(&glb(&document, &build.bin)).expect(name));
    for (reference, position_reference) in QUAD_UVS.iter().zip(QUAD_POSITIONS) {
        let at = mesh
            .positions
            .iter()
            .position(|p| (0..3).all(|a| (p[a] - position_reference[a]).abs() < 1e-3))
            .expect("vertex");
        // The cooked UV is f16, whose error at these values is far below a step.
        for (axis, want) in reference.iter().enumerate() {
            let got = mesh.uvs[at][axis] / scale;
            assert!(
                (got - want).abs() <= form.step * 1.01,
                "{name}: uv {axis} is {got}, want {want}",
            );
        }
    }
}

#[test]
fn quantized_uvs_decode_within_one_step_normalized_or_not() {
    check_uvs(&I8_NORM, 1.0, "BYTE normalized");
    check_uvs(&U8_NORM, 1.0, "UNSIGNED_BYTE normalized");
    check_uvs(&I16_NORM, 1.0, "SHORT normalized");
    check_uvs(&U16_NORM, 1.0, "UNSIGNED_SHORT normalized");
    // A plain integer keeps its value: a texture transform or a shader scales it back.
    check_uvs(&I8_RAW, RAW_SCALE, "BYTE");
    check_uvs(&U8_RAW, RAW_SCALE, "UNSIGNED_BYTE");
    check_uvs(&I16_RAW, RAW_SCALE, "SHORT");
    check_uvs(&U16_RAW, RAW_SCALE, "UNSIGNED_SHORT");
}

/// A quad with `TEXCOORD_0` stored in `form` as `uv - shift`, and a material whose textures each carry
/// `transforms[i]` (`None`: no block), the way a quantizer writes UVs.
fn transformed_uv_file(form: &Quantized, shift: [f32; 2], transforms: &[Option<Value>]) -> Vec<u8> {
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &QUAD_POSITIONS.concat());
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let stored: Vec<Vec<f32>> = QUAD_UVS
        .iter()
        .map(|uv| vec![uv[0] - shift[0], uv[1] - shift[1]])
        .collect();
    let uv = build.add(form, "VEC2", &stored);
    let indices = build.add_indices(&QUAD_INDICES);
    let image = png([200, 100, 50, 255]);
    while !build.bin.len().is_multiple_of(4) {
        build.bin.push(0);
    }
    let at = build.bin.len();
    build.bin.extend_from_slice(&image);
    build
        .views
        .push(json!({"buffer": 0, "byteOffset": at, "byteLength": image.len()}));
    let image_view = build.views.len() - 1;
    let mut document = build.document(json!({"POSITION": position, "TEXCOORD_0": uv}), indices, true);
    document["meshes"][0]["primitives"][0]["material"] = json!(0);
    let info = |slot: usize| {
        let mut info = json!({"index": 0});
        if let Some(Some(block)) = transforms.get(slot) {
            info["extensions"] = json!({"KHR_texture_transform": block});
        }
        info
    };
    let mut material = json!({"name": "m", "pbrMetallicRoughness": {"baseColorTexture": info(0)}});
    if transforms.len() > 1 {
        material["emissiveTexture"] = info(1);
    }
    document["materials"] = json!([material]);
    document["textures"] = json!([{"source": 0}]);
    document["images"] = json!([{"bufferView": image_view, "mimeType": "image/png"}]);
    document["extensionsUsed"] = json!(["KHR_mesh_quantization", "KHR_texture_transform"]);
    glb(&document, &build.bin)
}

/// The cooked UVs at the quad's corners, in `QUAD_POSITIONS` order.
fn corner_uvs(mesh: &CpuMesh) -> Vec<[f32; 2]> {
    QUAD_POSITIONS
        .iter()
        .map(|corner| {
            let at = mesh
                .positions
                .iter()
                .position(|p| (0..3).all(|a| (p[a] - corner[a]).abs() < 1e-3))
                .expect("vertex");
            mesh.uvs[at]
        })
        .collect()
}

#[test]
fn a_quantized_uv_set_with_a_texture_transform_decodes_to_the_reference_floats() {
    // The transform is what brings these integers back to UVs, and the renderer has no UV transform.
    let shift = [-0.25, -0.125];
    let raw_scale = 1.0 / RAW_SCALE;
    for (form, scale, name) in [
        (&U16_RAW, raw_scale, "UNSIGNED_SHORT"),
        (&U8_RAW, raw_scale, "UNSIGNED_BYTE"),
        (&I16_RAW, raw_scale, "SHORT"),
        (&I8_RAW, raw_scale, "BYTE"),
        (&U16_NORM, 1.0, "UNSIGNED_SHORT normalized"),
        (&U8_NORM, 1.0, "UNSIGNED_BYTE normalized"),
    ] {
        let block = json!({"offset": shift, "scale": [scale, scale]});
        let file = transformed_uv_file(form, shift, &[Some(block)]);
        let result = import_keep(&file).expect(name);
        let uvs = corner_uvs(&first_mesh(&result));
        for (got, want) in uvs.iter().zip(QUAD_UVS) {
            for axis in 0..2 {
                // One step, plus the cooked UV's own f16 rounding at these values.
                assert!(
                    (got[axis] - want[axis]).abs() <= form.step * 1.01 + 5e-4,
                    "{name}: uv {got:?}, want {want:?}"
                );
            }
        }
    }
}

#[test]
fn a_baked_transform_applies_offset_and_rotation_in_the_extension_order() {
    let rotation = std::f32::consts::FRAC_PI_2;
    let block = json!({"offset": [0.5, 0.25], "rotation": rotation, "scale": [0.5, 2.0]});
    let file = transformed_uv_file(&U16_NORM, [0.0, 0.0], &[Some(block)]);
    let uvs = corner_uvs(&first_mesh(&import_keep(&file).expect("import")));
    for (got, uv) in uvs.iter().zip(QUAD_UVS) {
        // uv' = T R S uv with R = [[cos, sin], [-sin, cos]]: at 90 degrees x' = y and y' = -x.
        let (x, y) = (uv[0] * 0.5, uv[1] * 2.0);
        let want = [y + 0.5, -x + 0.25];
        for axis in 0..2 {
            assert!((got[axis] - want[axis]).abs() < 1e-3, "{got:?}, want {want:?}");
        }
    }
}

#[test]
fn a_float_uv_set_keeps_its_transform_in_the_side_table_only() {
    // Not quantized: nothing bakes, so such a file cooks as it did before this extension.
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &QUAD_POSITIONS.concat());
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    while !build.bin.len().is_multiple_of(4) {
        build.bin.push(0);
    }
    let at = build.bin.len();
    build.bin.extend(floats(&QUAD_UVS.concat()));
    build
        .views
        .push(json!({"buffer": 0, "byteOffset": at, "byteLength": 32}));
    build.accessors.push(
        json!({"bufferView": build.views.len() - 1, "componentType": 5126, "count": 4, "type": "VEC2"}),
    );
    let uv = build.accessors.len() - 1;
    let indices = build.add_indices(&QUAD_INDICES);
    let image = png([200, 100, 50, 255]);
    let at = build.bin.len();
    build.bin.extend_from_slice(&image);
    build
        .views
        .push(json!({"buffer": 0, "byteOffset": at, "byteLength": image.len()}));
    let image_view = build.views.len() - 1;
    let mut document = build.document(json!({"POSITION": position, "TEXCOORD_0": uv}), indices, false);
    document["meshes"][0]["primitives"][0]["material"] = json!(0);
    document["materials"] = json!([{"name": "m", "pbrMetallicRoughness": {"baseColorTexture": {
        "index": 0, "extensions": {"KHR_texture_transform": {"scale": [2.0, 2.0]}},
    }}}]);
    document["textures"] = json!([{"source": 0}]);
    document["images"] = json!([{"bufferView": image_view, "mimeType": "image/png"}]);
    let bytes = glb(&document, &build.bin);
    let result = import_keep(&bytes).expect("import");
    for (got, want) in corner_uvs(&first_mesh(&result)).iter().zip(QUAD_UVS) {
        for axis in 0..2 {
            assert!((got[axis] - want[axis]).abs() < 1e-3, "{got:?}, want {want:?}");
        }
    }
}

#[test]
fn the_tools_path_names_the_material_it_baked() {
    let block = json!({"scale": [0.5, 0.5]});
    let bytes = transformed_uv_file(&U16_NORM, [0.0, 0.0], &[Some(block)]);
    let root = regia_test_utils::unique_temp_dir("gltf_ext_baked_uv");
    let path = root.join("baked.glb");
    std::fs::write(&path, &bytes).expect("write");
    let opened = open_gltf(&path).expect("open");
    assert!(opened.load_data.baked_uv_materials.contains(&0));
    let report = regia_asset_pipeline::gltf_extension_report(&path).expect("report");
    assert_eq!(report.baked_uv_materials, vec![0]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disagreeing_transforms_on_a_quantized_set_bake_the_first_texture() {
    let first = json!({"scale": [0.5, 0.5]});
    let second = json!({"scale": [0.25, 0.25]});
    let file = transformed_uv_file(&U16_NORM, [0.0, 0.0], &[Some(first), Some(second)]);
    let uvs = corner_uvs(&first_mesh(&import_keep(&file).expect("import")));
    for (got, uv) in uvs.iter().zip(QUAD_UVS) {
        let want = [uv[0] * 0.5, uv[1] * 0.5];
        for axis in 0..2 {
            assert!((got[axis] - want[axis]).abs() < 1e-3, "{got:?}, want {want:?}");
        }
    }
}

#[test]
fn quantized_normals_decode_and_the_cook_keeps_them_within_one_step() {
    for (form, name) in [(&I8_NORM, "BYTE"), (&I16_NORM, "SHORT")] {
        let mut build = Builder::new();
        let position = build.add_float3("VEC3", &QUAD_POSITIONS.concat());
        build.accessors[position]["min"] = json!([0, 0, 0]);
        build.accessors[position]["max"] = json!([1, 1, 0]);
        let reference = [0.6f32, 0.0, 0.8];
        let normal = build.add(form, "VEC3", &vec![reference.to_vec(); 4]);
        let indices = build.add_indices(&QUAD_INDICES);
        let document = build.document(json!({"POSITION": position, "NORMAL": normal}), indices, true);
        let mesh = first_mesh(&import_keep(&glb(&document, &build.bin)).expect(name));
        for n in &mesh.normals {
            for axis in 0..3 {
                // The cooked normal is octahedral at 16 bits, so allow its error on top of a step.
                assert!(
                    (n[axis] - reference[axis]).abs() <= form.step * 1.01 + 1e-3,
                    "{name}: normal {n:?}"
                );
            }
        }
    }
}

#[test]
fn quantized_tangents_are_read_by_the_tools_path() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_tangents");
    for (form, name) in [(&I8_NORM, "byte"), (&I16_NORM, "short")] {
        let mut build = Builder::new();
        let position = build.add_float3("VEC3", &QUAD_POSITIONS.concat());
        build.accessors[position]["min"] = json!([0, 0, 0]);
        build.accessors[position]["max"] = json!([1, 1, 0]);
        let reference = [0.8f32, 0.6, 0.0, -1.0];
        let tangent = build.add(form, "VEC4", &vec![reference.to_vec(); 4]);
        let indices = build.add_indices(&QUAD_INDICES);
        let document = build.document(json!({"POSITION": position, "TANGENT": tangent}), indices, true);
        let path = root.join(format!("{name}.glb"));
        std::fs::write(&path, glb(&document, &build.bin)).expect("write");
        let opened = open_gltf(&path).expect("opens");
        let mesh = opened.document.meshes().next().expect("mesh");
        let primitive = mesh.primitives().next().expect("primitive");
        let reader = primitive.reader(|b| opened.buffers.get(b.index()).map(|d| d.0.as_slice()));
        let tangents: Vec<[f32; 4]> = reader.read_tangents().expect("tangents").collect();
        assert_eq!(tangents.len(), 4);
        for t in tangents {
            for axis in 0..4 {
                assert!(
                    (t[axis] - reference[axis]).abs() <= form.step * 1.01,
                    "{name}: tangent {t:?}"
                );
            }
        }
        assert_eq!(opened.load_data.quantized_accessors, 1);
    }
}

#[test]
fn colors_decode_for_every_integer_form_and_a_missing_normalized_flag_reads_as_normalized() {
    let reference = [0.25f32, 0.5, 1.0, 1.0];
    let unnormalized_u8 = Quantized {
        normalized: false,
        ..U8_NORM
    };
    let unnormalized_u16 = Quantized {
        normalized: false,
        ..U16_NORM
    };
    let file = |form: &Quantized| {
        let mut build = Builder::new();
        let position = build.add_float3("VEC3", &QUAD_POSITIONS.concat());
        build.accessors[position]["min"] = json!([0, 0, 0]);
        build.accessors[position]["max"] = json!([1, 1, 0]);
        let color = build.add(form, "VEC4", &vec![reference.to_vec(); 4]);
        let indices = build.add_indices(&QUAD_INDICES);
        let document = build.document(json!({"POSITION": position, "COLOR_0": color}), indices, false);
        glb(&document, &build.bin)
    };
    // ADR-5958: `KHR_mesh_quantization` widens POSITION, NORMAL, TANGENT and TEXCOORD_n only (its
    // table has no COLOR_n), so a signed color stays the component-type error of ADR-2723 even with
    // the extension declared, as core glTF 2.0 § 3.7.2.1 says. It is refused, never a panic.
    for (form, name) in [(&I8_NORM, "BYTE normalized"), (&I16_NORM, "SHORT normalized")] {
        let error = import_keep(&file(form)).expect_err(name);
        assert!(
            error
                .to_string()
                .contains("component type its use does not allow"),
            "{name}: {error}"
        );
    }
    for (form, name) in [
        (&U8_NORM, "UNSIGNED_BYTE normalized"),
        (&U16_NORM, "UNSIGNED_SHORT normalized"),
        (&unnormalized_u8, "UNSIGNED_BYTE without the flag"),
        (&unnormalized_u16, "UNSIGNED_SHORT without the flag"),
    ] {
        let mesh = first_mesh(&import_keep(&file(form)).expect(name));
        assert_eq!(mesh.colors.len(), 4, "{name}");
        for c in &mesh.colors {
            for channel in 0..4 {
                // Colors cook at 16 bits per channel.
                assert!(
                    (c[channel] - reference[channel]).abs() <= form.step * 1.01 + 1e-4,
                    "{name}: color {c:?}"
                );
            }
        }
    }
}

#[test]
fn quantized_morph_deltas_and_sparse_positions_decode() {
    let mut build = Builder::new();
    let position = build.add(&I16_NORM, "VEC3", &as_vec3(&QUAD_POSITIONS));
    let delta = build.add(
        &I16_NORM,
        "VEC3",
        &[
            vec![0.0, 0.0, 0.25],
            vec![0.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0],
        ],
    );
    let indices = build.add_indices(&QUAD_INDICES);
    // Move vertex 2 with a sparse edit in the same integers as the base.
    let at = build.bin.len();
    build.bin.extend_from_slice(&2u16.to_le_bytes());
    build.bin.extend_from_slice(&[0, 0]);
    let moved = at + 4;
    for c in [0.5f32, 0.5, 0.5] {
        build.bin.extend((I16_NORM.encode)(c));
    }
    build.bin.extend_from_slice(&[0, 0]);
    build
        .views
        .push(json!({"buffer": 0, "byteOffset": at, "byteLength": 4}));
    build
        .views
        .push(json!({"buffer": 0, "byteOffset": moved, "byteLength": 8}));
    let (index_view, value_view) = (build.views.len() - 2, build.views.len() - 1);
    build.accessors[position]["sparse"] = json!({
        "count": 1,
        "indices": {"bufferView": index_view, "componentType": 5123},
        "values": {"bufferView": value_view},
    });
    let mut document = build.document(json!({"POSITION": position}), indices, true);
    document["meshes"][0]["primitives"][0]["targets"] = json!([{"POSITION": delta}]);
    let mesh = first_mesh(&import_keep(&glb(&document, &build.bin)).expect("imports"));
    assert!(
        mesh.positions
            .iter()
            .any(|p| (0..3).all(|a| (p[a] - 0.5).abs() < 1e-3)),
        "the sparse edit applies: {:?}",
        mesh.positions
    );
    let target = mesh.morph_targets.first().expect("the morph target");
    assert_eq!(target.vertex_indices.len(), 1);
    assert!((target.position_deltas[0][2] - 0.25).abs() < 1e-3);
}

#[test]
fn a_u32_attribute_is_a_coded_error_not_a_crash() {
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &QUAD_POSITIONS.concat());
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let indices = build.add_indices(&QUAD_INDICES);
    build.accessors[position]["componentType"] = json!(5125);
    let document = build.document(json!({"POSITION": position}), indices, true);
    let error = import_keep(&glb(&document, &build.bin)).expect_err("refused");
    assert_eq!(error.code(), Some("gltf_quantization_invalid"), "{error}");
}

#[test]
fn quantized_values_on_the_grid_cook_byte_identical_to_floats() {
    // Same geometry through the rewrite and without it: integers that land exactly on the float values
    // give the same cooked mesh, and no instance set appears.
    let positions: [[f32; 3]; 4] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
    let mut quantized = Builder::new();
    let position = quantized.add(&U16_NORM, "VEC3", &as_vec3(&positions));
    let indices = quantized.add_indices(&QUAD_INDICES);
    let document = quantized.document(json!({"POSITION": position}), indices, true);
    let a = import_keep(&glb(&document, &quantized.bin)).expect("quantized");

    let mut plain = Builder::new();
    let position = plain.add_float3("VEC3", &positions.concat());
    plain.accessors[position]["min"] = json!([0, 0, 0]);
    plain.accessors[position]["max"] = json!([1, 1, 0]);
    let indices = plain.add_indices(&QUAD_INDICES);
    let mut document = plain.document(json!({"POSITION": position}), indices, false);
    document.as_object_mut().expect("object").remove("extensionsUsed");
    let b = import_keep(&glb(&document, &plain.bin)).expect("float");

    assert_eq!(
        a.primary_mesh.to_postcard().expect("a"),
        b.primary_mesh.to_postcard().expect("b")
    );
    assert!(
        a.instances.is_none() && b.instances.is_none(),
        "no instance set without the extension"
    );
    assert!(
        !b.sub_assets
            .iter()
            .any(|s| s.path == regia_asset::PREFAB_INSTANCES_SUB_PATH)
    );
}

// ---------------------------------------------------------------------------------------------
// EXT_meshopt_compression
// ---------------------------------------------------------------------------------------------

/// The plain GLB of a triangle fan and the same data through `EXT_meshopt_compression`.
struct Compressed {
    plain: Vec<u8>,
    compressed: Vec<u8>,
}

fn fan() -> (Vec<[f32; 4]>, Vec<u32>) {
    let mut vertices = vec![[0.0f32, 0.0, 0.0, 0.0]];
    for i in 0..300 {
        let angle = i as f32 * std::f32::consts::TAU / 300.0;
        vertices.push([angle.cos(), angle.sin(), 0.0, 0.0]);
    }
    let mut indices = Vec::new();
    for i in 1..300u32 {
        indices.extend([0, i, i + 1]);
    }
    (vertices, indices)
}

/// `mode` is `TRIANGLES` or `INDICES`. The fallback buffer holds no data.
fn compressed_fan(mode: &str, required: bool) -> Compressed {
    let (vertices, indices) = fan();
    let vertex_bytes: Vec<u8> = vertices.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
    let index_bytes: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    let mut plain_data = vertex_bytes.clone();
    plain_data.extend_from_slice(&index_bytes);
    let accessors = json!([
        {"bufferView": 0, "componentType": 5126, "count": vertices.len(), "type": "VEC3",
         "min": [-1, -1, 0], "max": [1, 1, 0]},
        {"bufferView": 1, "componentType": 5125, "count": indices.len(), "type": "SCALAR"},
    ]);
    let body = |views: Value, buffers: Value| {
        json!({
            "asset": {"version": "2.0"},
            "scene": 0, "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "name": "fan"}],
            "meshes": [{"name": "m", "primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
            "buffers": buffers,
            "bufferViews": views,
            "accessors": accessors.clone(),
        })
    };
    let plain = glb(
        &body(
            json!([
                {"buffer": 0, "byteOffset": 0, "byteLength": vertex_bytes.len(), "byteStride": 16},
                {"buffer": 0, "byteOffset": vertex_bytes.len(), "byteLength": index_bytes.len()},
            ]),
            json!([{"byteLength": plain_data.len()}]),
        ),
        &plain_data,
    );

    let encoded_vertices = meshopt::encoding::encode_vertex_buffer(&vertices).expect("encode vertices");
    let encoded_indices = if mode == "TRIANGLES" {
        meshopt::encoding::encode_index_buffer(&indices, vertices.len()).expect("encode indices")
    } else {
        encode_sequence(&indices, vertices.len())
    };
    let mut bin = encoded_vertices.clone();
    let indices_at = bin.len();
    bin.extend_from_slice(&encoded_indices);
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let fallback = json!({"byteLength": plain_data.len(), "extensions": {"EXT_meshopt_compression": {"fallback": true}}});
    let views = json!([
        {"buffer": 1, "byteOffset": 0, "byteLength": vertex_bytes.len(), "byteStride": 16, "extensions": {
            "EXT_meshopt_compression": {"buffer": 0, "byteOffset": 0, "byteLength": encoded_vertices.len(),
                "byteStride": 16, "mode": "ATTRIBUTES", "count": vertices.len()}}},
        {"buffer": 1, "byteOffset": vertex_bytes.len(), "byteLength": index_bytes.len(), "extensions": {
            "EXT_meshopt_compression": {"buffer": 0, "byteOffset": indices_at, "byteLength": encoded_indices.len(),
                "byteStride": 4, "mode": mode, "count": indices.len()}}},
    ]);
    let mut document = body(views, json!([{"byteLength": bin.len()}, fallback]));
    document["extensionsUsed"] = json!(["EXT_meshopt_compression"]);
    if required {
        document["extensionsRequired"] = json!(["EXT_meshopt_compression"]);
    }
    Compressed {
        plain,
        compressed: glb(&document, &bin),
    }
}

/// The codec's sequence encoder, which the crate exposes only as a raw binding (ADR-3016).
#[allow(unsafe_code)]
fn encode_sequence(indices: &[u32], vertex_count: usize) -> Vec<u8> {
    // SAFETY: the bound sizes the output, and both pointers are valid for those lengths.
    unsafe {
        let bound = meshopt::ffi::meshopt_encodeIndexSequenceBound(indices.len(), vertex_count);
        let mut out = vec![0u8; bound];
        let size = meshopt::ffi::meshopt_encodeIndexSequence(
            out.as_mut_ptr(),
            out.len(),
            indices.as_ptr(),
            indices.len(),
        );
        out.truncate(size);
        out
    }
}

#[test]
fn a_compressed_buffer_decodes_byte_identical_to_the_uncompressed_reference() {
    for mode in ["TRIANGLES", "INDICES"] {
        let fan = compressed_fan(mode, true);
        let a = import_keep(&fan.plain).expect("plain");
        let b = import_keep(&fan.compressed).expect(mode);
        // The decoded view reaches the cook: the cooked mesh is the same bytes.
        assert_eq!(
            a.primary_mesh.to_postcard().expect("a"),
            b.primary_mesh.to_postcard().expect("b"),
            "{mode}"
        );
    }
}

#[test]
fn the_decoded_views_are_the_reference_bytes_on_the_tools_path() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_meshopt_bytes");
    let (vertices, indices) = fan();
    let reference_vertices: Vec<u8> = vertices.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
    let reference_indices: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    let path = root.join("fan.glb");
    std::fs::write(&path, compressed_fan("TRIANGLES", true).compressed).expect("write");
    let opened = open_gltf(&path).expect("opens");
    assert_eq!(opened.load_data.meshopt_views, 2);
    let mut view = opened.document.views();
    let (first, second) = (view.next().expect("view 0"), view.next().expect("view 1"));
    let bytes = |v: &gltf::buffer::View<'_>| {
        opened.buffers[v.buffer().index()].0[v.offset()..v.offset() + v.length()].to_vec()
    };
    assert_eq!(bytes(&first), reference_vertices);
    assert_eq!(bytes(&second), reference_indices);
}

#[test]
fn a_fallback_buffer_is_not_needed_whether_the_extension_is_required_or_optional() {
    // Required: the fallback buffer has no data at all (no URI, no bytes).
    let required = compressed_fan("TRIANGLES", true);
    assert!(import_keep(&required.compressed).is_ok());
    // Optional, with the flag: a reader of the extension still uses the compressed stream.
    let optional = compressed_fan("TRIANGLES", false);
    assert!(import_keep(&optional.compressed).is_ok());
}

#[test]
fn an_optional_file_with_real_fallback_bytes_uses_the_compressed_stream() {
    // No `fallback` flag: the view's own buffer holds bytes a reader without the extension would use.
    // They are wrong on purpose, so the test sees which ones the importer took.
    let (vertices, indices) = fan();
    let encoded = meshopt::encoding::encode_vertex_buffer(&vertices).expect("vertices");
    let index_bytes: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    let mut bin = encoded.clone();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let fallback_at = bin.len();
    bin.extend(vec![0x7fu8; vertices.len() * 16]);
    let index_at = bin.len();
    bin.extend_from_slice(&index_bytes);
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["EXT_meshopt_compression"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "fan"}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "buffers": [{"byteLength": bin.len()}],
        "bufferViews": [
            {"buffer": 0, "byteOffset": fallback_at, "byteLength": vertices.len() * 16, "byteStride": 16,
             "extensions": {"EXT_meshopt_compression": {"buffer": 0, "byteLength": encoded.len(), "byteStride": 16,
                 "mode": "ATTRIBUTES", "count": vertices.len()}}},
            {"buffer": 0, "byteOffset": index_at, "byteLength": index_bytes.len()},
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": vertices.len(), "type": "VEC3",
             "min": [-1, -1, 0], "max": [1, 1, 0]},
            {"bufferView": 1, "componentType": 5125, "count": indices.len(), "type": "SCALAR"},
        ],
    });
    let plain = compressed_fan("TRIANGLES", true).plain;
    let a = import_keep(&plain).expect("plain");
    let b = import_keep(&glb(&document, &bin)).expect("optional");
    assert_eq!(
        a.primary_mesh.to_postcard().expect("a"),
        b.primary_mesh.to_postcard().expect("b")
    );
}

#[test]
fn a_gltf_on_disk_does_not_read_a_fallback_buffer_with_a_uri() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_fallback_uri");
    let (vertices, indices) = fan();
    let encoded = meshopt::encoding::encode_vertex_buffer(&vertices).expect("vertices");
    let index_bytes: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    let mut bin = encoded.clone();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let index_at = bin.len();
    bin.extend_from_slice(&index_bytes);
    std::fs::write(root.join("data.bin"), &bin).expect("bin");
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["EXT_meshopt_compression"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "fan"}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "buffers": [
            {"byteLength": bin.len(), "uri": "data.bin"},
            // The file is not there, and must not be asked for.
            {"byteLength": vertices.len() * 16, "uri": "missing_fallback.bin",
             "extensions": {"EXT_meshopt_compression": {"fallback": true}}},
        ],
        "bufferViews": [
            {"buffer": 1, "byteLength": vertices.len() * 16, "byteStride": 16, "extensions": {
                "EXT_meshopt_compression": {"buffer": 0, "byteLength": encoded.len(), "byteStride": 16,
                    "mode": "ATTRIBUTES", "count": vertices.len()}}},
            {"buffer": 0, "byteOffset": index_at, "byteLength": index_bytes.len()},
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": vertices.len(), "type": "VEC3",
             "min": [-1, -1, 0], "max": [1, 1, 0]},
            {"bufferView": 1, "componentType": 5125, "count": indices.len(), "type": "SCALAR"},
        ],
    });
    let path = root.join("fan.gltf");
    std::fs::write(&path, serde_json::to_vec(&document).expect("json")).expect("gltf");
    let result = import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        None,
        TextureCookQuality::High,
    )
    .expect("imports without the fallback file");
    assert_eq!(first_mesh(&result).positions.len(), vertices.len());
}

#[test]
fn octahedral_normals_and_exponential_positions_go_through_their_filters() {
    // Three vertices: filtered positions (exponent -8: 256 is 1.0), filtered normals (+Z).
    let exp = |v: i32| ((-8i32 as u32) << 24) | (v as u32 & 0x00ff_ffff);
    let positions: Vec<[u32; 4]> = vec![
        [exp(0), exp(0), exp(0), 0],
        [exp(256), exp(0), exp(0), 0],
        [exp(0), exp(256), exp(0), 0],
    ];
    let normals: Vec<[i16; 4]> = vec![[0, 0, 32767, 0]; 3];
    let encoded_positions = meshopt::encoding::encode_vertex_buffer(&positions).expect("positions");
    let encoded_normals = meshopt::encoding::encode_vertex_buffer(&normals).expect("normals");
    let mut bin = encoded_positions.clone();
    let normals_at = bin.len();
    bin.extend_from_slice(&encoded_normals);
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["EXT_meshopt_compression", "KHR_mesh_quantization"],
        "extensionsRequired": ["EXT_meshopt_compression", "KHR_mesh_quantization"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "tri"}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1}}]}],
        "buffers": [
            {"byteLength": bin.len()},
            {"byteLength": 72, "extensions": {"EXT_meshopt_compression": {"fallback": true}}},
        ],
        "bufferViews": [
            {"buffer": 1, "byteOffset": 0, "byteLength": 48, "byteStride": 16, "extensions": {
                "EXT_meshopt_compression": {"buffer": 0, "byteLength": encoded_positions.len(), "byteStride": 16,
                    "mode": "ATTRIBUTES", "filter": "EXPONENTIAL", "count": 3}}},
            {"buffer": 1, "byteOffset": 48, "byteLength": 24, "byteStride": 8, "extensions": {
                "EXT_meshopt_compression": {"buffer": 0, "byteOffset": normals_at, "byteLength": encoded_normals.len(),
                    "byteStride": 8, "mode": "ATTRIBUTES", "filter": "OCTAHEDRAL", "count": 3}}},
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]},
            {"bufferView": 1, "componentType": 5122, "normalized": true, "count": 3, "type": "VEC3"},
        ],
    });
    let mesh = first_mesh(&import_keep(&glb(&document, &bin)).expect("imports"));
    for reference in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        assert!(
            mesh.positions
                .iter()
                .any(|p| (0..3).all(|axis| (p[axis] - reference[axis]).abs() < 1e-5)),
            "{reference:?} in {:?}",
            mesh.positions
        );
    }
    for n in &mesh.normals {
        assert!(n[2] > 0.999, "{n:?}");
    }
}

/// `edit` breaks the compressed file; the result is a coded error.
#[track_caller]
fn assert_meshopt_refused(edit: impl Fn(&mut Value)) {
    let fan = compressed_fan("TRIANGLES", true);
    let (document, bin) = split_glb(&fan.compressed);
    let mut document = document;
    edit(&mut document);
    let error = import_keep(&glb(&document, &bin)).expect_err("refused");
    assert_eq!(error.code(), Some("gltf_meshopt_invalid"), "{error}");
}

fn split_glb(bytes: &[u8]) -> (Value, Vec<u8>) {
    let json_len = u32::from_le_bytes(bytes[12..16].try_into().expect("len")) as usize;
    let document = serde_json::from_slice(&bytes[20..20 + json_len]).expect("json");
    let bin = bytes[20 + json_len + 8..].to_vec();
    (document, bin)
}

#[test]
fn broken_compressed_views_are_coded_errors() {
    let block = "extensions";
    assert_meshopt_refused(|d| {
        d["bufferViews"][0][block]["EXT_meshopt_compression"]["byteStride"] = json!(6);
    });
    assert_meshopt_refused(|d| {
        d["bufferViews"][0][block]["EXT_meshopt_compression"]["mode"] = json!("BOGUS");
    });
    assert_meshopt_refused(|d| {
        d["bufferViews"][0][block]["EXT_meshopt_compression"]["buffer"] = json!(9);
    });
    assert_meshopt_refused(|d| {
        d["bufferViews"][0][block]["EXT_meshopt_compression"]["byteOffset"] = json!(1u64 << 60);
    });
    assert_meshopt_refused(|d| {
        d["bufferViews"][0]["byteLength"] = json!(16);
        d["accessors"][0]["count"] = json!(1);
    });
    // A small file asking for a huge output: refused before anything is allocated.
    assert_meshopt_refused(|d| {
        let count = 1u64 << 28;
        d["bufferViews"][0]["byteLength"] = json!(count * 16);
        d["buffers"][1]["byteLength"] = json!(count * 16 + 4096);
        d["bufferViews"][0][block]["EXT_meshopt_compression"]["count"] = json!(count);
    });
    // A view far past the data: refused before the fallback buffer grows that far.
    assert_meshopt_refused(|d| {
        d["buffers"][1]["byteLength"] = json!(1u64 << 40);
        d["bufferViews"][0]["byteOffset"] = json!((1u64 << 40) - 4816);
    });
    // Corrupt bytes.
    let fan = compressed_fan("TRIANGLES", true);
    let (document, mut bin) = split_glb(&fan.compressed);
    bin[0] = 0;
    let error = import_keep(&glb(&document, &bin)).expect_err("refused");
    assert_eq!(error.code(), Some("gltf_meshopt_invalid"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// EXT_mesh_gpu_instancing
// ---------------------------------------------------------------------------------------------

/// A triangle whose node (translated to `node_at`) has `instances` translations and, optionally,
/// rotations and scales.
fn instanced_file(node_at: [f32; 3], translations: &[[f32; 3]], extra: bool) -> Vec<u8> {
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let indices = build.add_indices(&[0, 1, 2]);
    let translation = build.add_float3("VEC3", &translations.concat());
    let mut attributes = json!({"TRANSLATION": translation});
    if extra {
        let half_turn_z: Vec<f32> = translations.iter().flat_map(|_| [0.0, 0.0, 1.0, 0.0]).collect();
        let doubled: Vec<f32> = translations.iter().flat_map(|_| [2.0, 2.0, 2.0]).collect();
        attributes["ROTATION"] = json!(build.add_float3("VEC4", &half_turn_z));
        attributes["SCALE"] = json!(build.add_float3("VEC3", &doubled));
    }
    let mut document = build.document(json!({"POSITION": position}), indices, false);
    document["extensionsUsed"] = json!(["EXT_mesh_gpu_instancing"]);
    document["nodes"][0]["translation"] = json!(node_at);
    document["nodes"][0]["name"] = json!("trees");
    document["nodes"][0]["extensions"] = json!({"EXT_mesh_gpu_instancing": {"attributes": attributes}});
    glb(&document, &build.bin)
}

fn point(matrix: &[f32; 16], p: [f32; 3]) -> [f32; 3] {
    [
        matrix[0] * p[0] + matrix[4] * p[1] + matrix[8] * p[2] + matrix[12],
        matrix[1] * p[0] + matrix[5] * p[1] + matrix[9] * p[2] + matrix[13],
        matrix[2] * p[0] + matrix[6] * p[1] + matrix[10] * p[2] + matrix[14],
    ]
}

#[test]
fn n_instances_give_n_transforms_on_the_prefab() {
    let translations: Vec<[f32; 3]> = (0..17).map(|i| [i as f32, 0.0, 0.0]).collect();
    let result = import_keep(&instanced_file([0.0; 3], &translations, false)).expect("imports");
    let set: &PrefabInstanceSet = result.instances.as_ref().expect("instances");
    assert_eq!(set.instance_count(), 17);
    let node = set.node("trees").expect("the node");
    assert_eq!(node.transforms.len(), 17);
    assert_eq!(node.parts.len(), 1);
    for (i, matrix) in node.transforms.iter().enumerate() {
        assert_eq!(point(matrix, [0.0; 3]), [i as f32, 0.0, 0.0], "instance {i}");
    }
    // The geometry is cooked once, and the sub-asset is in the cook.
    let meshes = result
        .sub_assets
        .iter()
        .filter(|s| s.path.starts_with("mesh/"))
        .count();
    assert_eq!(meshes, 1);
    let cooked = result
        .sub_assets
        .iter()
        .find(|s| s.path == regia_asset::PREFAB_INSTANCES_SUB_PATH)
        .expect("the sub-asset");
    assert_eq!(cooked.type_path, regia_asset::PREFAB_INSTANCES_TYPE_PATH);
    assert_eq!(cooked.dependencies, node.parts);
    assert_eq!(
        &PrefabInstanceSet::from_postcard(&cooked.payload).expect("decode"),
        set
    );
    assert!(
        result
            .meta_entries
            .iter()
            .any(|e| e.path == regia_asset::PREFAB_INSTANCES_SUB_PATH),
        "the .meta lists it"
    );
}

#[test]
fn an_instance_composes_with_the_node_under_the_axis_turn() {
    // glTF: world = node * instance * vertex. The baked part has the node and the turn in it.
    let node_at = [10.0f32, 1.0, 2.0];
    let instance = [3.0f32, 4.0, 5.0];
    let bytes = instanced_file(node_at, &[instance], true);
    let result = import_gltf_glb_bytes(
        &bytes,
        AssetId::new_random(),
        Forward::Convert,
        "desktop",
        TextureCookQuality::High,
    )
    .expect("imports");
    let set = result.instances.as_ref().expect("instances");
    let matrix = &set.nodes[0].transforms[0];
    // The vertex (1, 0, 0) of the source: scaled by 2, turned half about Z, moved by the instance
    // and the node, then turned to Regia axes.
    let want = {
        let p = [-2.0 + instance[0], 0.0 + instance[1], 0.0 + instance[2]];
        let p = [p[0] + node_at[0], p[1] + node_at[1], p[2] + node_at[2]];
        [-p[0], p[1], -p[2]]
    };
    // The same vertex as baked into the cooked part.
    let baked = [-(1.0 + node_at[0]), node_at[1], -node_at[2]];
    let got = point(matrix, baked);
    for axis in 0..3 {
        assert!((got[axis] - want[axis]).abs() < 1e-3, "{got:?} against {want:?}");
    }
}

#[test]
fn the_identity_instance_of_a_moved_node_is_the_identity() {
    let result = import_keep(&instanced_file([7.0, 8.0, 9.0], &[[0.0; 3]], false)).expect("imports");
    let matrix = result.instances.as_ref().expect("instances").nodes[0].transforms[0];
    let identity = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    for i in 0..16 {
        assert!((matrix[i] - identity[i]).abs() < 1e-5, "{matrix:?}");
    }
}

#[test]
fn quantized_and_compressed_instance_attributes_decode() {
    // Rotation as normalized shorts behind the quaternion filter is what an optimizer writes.
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let indices = build.add_indices(&[0, 1, 2]);
    let turn = build.add(
        &I16_NORM,
        "VEC4",
        &[vec![0.0, 0.0, 1.0, 0.0], vec![0.0, 0.0, 0.0, 1.0]],
    );
    let mut document = build.document(json!({"POSITION": position}), indices, false);
    document["nodes"][0]["extensions"] =
        json!({"EXT_mesh_gpu_instancing": {"attributes": {"ROTATION": turn}}});
    document["nodes"][0]["name"] = json!("spun");
    let result = import_keep(&glb(&document, &build.bin)).expect("imports");
    let node = result
        .instances
        .as_ref()
        .expect("instances")
        .node("spun")
        .cloned()
        .expect("node");
    assert_eq!(node.transforms.len(), 2);
    let turned = point(&node.transforms[0], [1.0, 0.0, 0.0]);
    assert!((turned[0] + 1.0).abs() < 1e-3, "half a turn: {turned:?}");
    let kept = point(&node.transforms[1], [1.0, 0.0, 0.0]);
    assert!((kept[0] - 1.0).abs() < 1e-3, "no turn: {kept:?}");
}

#[track_caller]
fn assert_instancing_refused(edit: impl Fn(&mut Value)) {
    let bytes = instanced_file([0.0; 3], &[[0.0; 3], [1.0, 0.0, 0.0]], true);
    let (mut document, bin) = split_glb(&bytes);
    edit(&mut document);
    let error = import_keep(&glb(&document, &bin)).expect_err("refused");
    assert_eq!(error.code(), Some("gltf_instancing_invalid"), "{error}");
}

#[test]
fn broken_instancing_blocks_are_coded_errors_or_draw_once() {
    let attributes = "/nodes/0/extensions/EXT_mesh_gpu_instancing/attributes";
    assert_instancing_refused(|d| {
        *d.pointer_mut(&format!("{attributes}/SCALE")).expect("scale") = json!(99);
    });
    assert_instancing_refused(|d| {
        // ROTATION points at a VEC3.
        let translation = d
            .pointer(&format!("{attributes}/TRANSLATION"))
            .expect("t")
            .clone();
        *d.pointer_mut(&format!("{attributes}/ROTATION"))
            .expect("rotation") = translation;
    });
    assert_instancing_refused(|d| {
        // The counts disagree.
        let scale = d
            .pointer(&format!("{attributes}/SCALE"))
            .and_then(Value::as_u64)
            .expect("scale") as usize;
        d["accessors"][scale]["count"] = json!(1);
    });
    // No attributes at all: the node draws once.
    let bytes = instanced_file([0.0; 3], &[[1.0, 0.0, 0.0]], false);
    let (mut document, bin) = split_glb(&bytes);
    document["nodes"][0]["extensions"]["EXT_mesh_gpu_instancing"]["attributes"] = json!({});
    let result = import_keep(&glb(&document, &bin)).expect("imports");
    assert!(result.instances.is_none());
}

// ---------------------------------------------------------------------------------------------
// KHR_materials_variants
// ---------------------------------------------------------------------------------------------

/// Two primitives over one triangle, materials `base`, `midnight`, `beach`; primitive 0 maps both
/// variants, primitive 1 only `beach`.
fn variants_file() -> Vec<u8> {
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let indices = build.add_indices(&[0, 1, 2]);
    let mut document = build.document(json!({"POSITION": position}), indices, false);
    document["extensionsUsed"] = json!(["KHR_materials_variants"]);
    document["extensions"] = json!({"KHR_materials_variants": {
        "variants": [{"name": "midnight"}, {"name": "beach"}]
    }});
    document["materials"] = json!([{"name": "base"}, {"name": "midnight"}, {"name": "beach"}]);
    let primitive = |material: usize, mappings: Value| {
        json!({
            "attributes": {"POSITION": position}, "indices": indices, "material": material,
            "extensions": {"KHR_materials_variants": {"mappings": mappings}},
        })
    };
    document["meshes"][0]["primitives"] = json!([
        primitive(
            0,
            json!([
                {"material": 1, "variants": [0]},
                {"material": 2, "variants": [1]},
            ])
        ),
        primitive(0, json!([{"material": 2, "variants": [1]}])),
    ]);
    glb(&document, &build.bin)
}

fn material_id(result: &GltfImportResult, name: &str) -> AssetId {
    result
        .sub_assets
        .iter()
        .find(|s| s.path == format!("material/{name}"))
        .expect("material")
        .id
}

#[test]
fn variant_names_map_to_sets_with_every_name_in_every_slot() {
    let result = import_keep(&variants_file()).expect("imports");
    let table = &result.extensions;
    assert_eq!(table.variant_names, vec!["midnight", "beach"]);
    assert_eq!(table.variants.len(), 2);
    assert!(table.unknown_of("KHR_materials_variants").next().is_none());
    assert!(!table.unknown_names.iter().any(|n| n == "KHR_materials_variants"));

    let (base, midnight, beach) = (
        material_id(&result, "base"),
        material_id(&result, "midnight"),
        material_id(&result, "beach"),
    );
    let sets: &[MaterialVariantSet] = &result.material_variants;
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].slot, "quad/primitive_0");
    assert_eq!(sets[0].get("midnight"), Some(midnight));
    assert_eq!(sets[0].get("beach"), Some(beach));
    assert_eq!(sets[1].slot, "quad/primitive_1");
    assert_eq!(sets[1].get("beach"), Some(beach));
    assert_eq!(
        sets[1].get("midnight"),
        Some(base),
        "an unmapped variant draws the primitive's default material"
    );
    // The slot names the part, and the sets are in the cook.
    for set in sets {
        let mesh = format!("mesh/{}", set.slot);
        assert!(result.sub_assets.iter().any(|s| s.path == mesh), "{mesh}");
        assert!(
            result
                .sub_assets
                .iter()
                .any(|s| s.type_path == "regia_asset::MaterialVariantSet"
                    && s.dependencies.contains(&set.variants[0].material))
        );
    }
}

#[test]
fn variant_switching_works_on_one_slot_and_on_the_whole_prefab() {
    let result = import_keep(&variants_file()).expect("imports");
    let (base, midnight, beach) = (
        material_id(&result, "base"),
        material_id(&result, "midnight"),
        material_id(&result, "beach"),
    );
    let mut slot = PrefabInstance::new(result.material_variants[0].clone(), "midnight").expect("slot");
    assert_eq!(slot.material, midnight);
    assert!(slot.select("beach"));
    assert_eq!(slot.material, beach);
    assert!(!slot.select("noon"));
    assert_eq!(slot.material, beach, "an unknown name changes nothing");

    let mut prefab = PrefabVariantSelection::new(result.material_variants.clone());
    assert_eq!(prefab.names(), vec!["beach".to_string(), "midnight".to_string()]);
    assert_eq!(prefab.material("quad/primitive_0"), None);
    assert!(prefab.select("midnight"));
    assert_eq!(prefab.material("quad/primitive_0"), Some(midnight));
    assert_eq!(prefab.material("quad/primitive_1"), Some(base));
    assert!(prefab.select("beach"));
    assert_eq!(prefab.material("quad/primitive_0"), Some(beach));
    assert_eq!(prefab.material("quad/primitive_1"), Some(beach));
}

#[test]
fn bad_variant_mappings_are_dropped_not_fatal() {
    let bytes = variants_file();
    let (mut document, bin) = split_glb(&bytes);
    document["meshes"][0]["primitives"][0]["extensions"]["KHR_materials_variants"]["mappings"] = json!([
        {"material": 99, "variants": [0]},
        {"material": 1, "variants": [0, 0, 77, "x"]},
        {"variants": [1]},
    ]);
    let result = import_keep(&glb(&document, &bin)).expect("imports");
    let mappings = result.extensions.primitive_variants(0, 0).expect("kept");
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].variants, vec![0]);
    let midnight = material_id(&result, "midnight");
    assert_eq!(result.material_variants[0].get("midnight"), Some(midnight));
}

#[test]
fn the_naming_convention_still_groups_materials_beside_the_extension() {
    let bytes = variants_file();
    let (mut document, bin) = split_glb(&bytes);
    document["materials"] = json!([{"name": "hull_Rem"}, {"name": "hull_A52"}, {"name": "beach"}]);
    let result = import_keep(&glb(&document, &bin)).expect("imports");
    assert!(result.material_variants.iter().any(|set| set.slot == "hull"));
    assert!(
        result
            .material_variants
            .iter()
            .any(|set| set.slot == "quad/primitive_0")
    );
}

// ---------------------------------------------------------------------------------------------
// EXT_texture_webp
// ---------------------------------------------------------------------------------------------

/// A lossless 8x8 WebP of one color (encoded by the `image` crate).
fn webp(color: [u8; 4]) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(8, 8, image::Rgba(color));
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::WebP)
        .expect("encode webp");
    out
}

fn png(color: [u8; 4]) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(8, 8, image::Rgba(color));
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encode png");
    out
}

/// A triangle with a base color texture whose image is `images`; `texture` is the texture object.
fn textured_file(images: &[Vec<u8>], mimes: &[&str], texture: Value, required: bool) -> Vec<u8> {
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let indices = build.add_indices(&[0, 1, 2]);
    let mut image_json = Vec::new();
    for (bytes, mime) in images.iter().zip(mimes) {
        while !build.bin.len().is_multiple_of(4) {
            build.bin.push(0);
        }
        let at = build.bin.len();
        build.bin.extend_from_slice(bytes);
        build
            .views
            .push(json!({"buffer": 0, "byteOffset": at, "byteLength": bytes.len()}));
        image_json.push(json!({"bufferView": build.views.len() - 1, "mimeType": mime}));
    }
    let mut document = build.document(json!({"POSITION": position}), indices, false);
    document["meshes"][0]["primitives"][0]["material"] = json!(0);
    document["materials"] =
        json!([{"name": "m", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}]);
    document["textures"] = json!([texture]);
    document["images"] = json!(image_json);
    document["extensionsUsed"] = json!(["EXT_texture_webp"]);
    if required {
        document["extensionsRequired"] = json!(["EXT_texture_webp"]);
    }
    glb(&document, &build.bin)
}

/// The base color map's sub-asset id of the file's only material.
fn base_color_slot(result: &GltfImportResult) -> AssetId {
    let material = result
        .sub_assets
        .iter()
        .find(|s| s.path == "material/m")
        .expect("material");
    let cooked = CookedStandardMaterial::from_postcard(&material.payload).expect("decode");
    match cooked.authored.base.base_color_map {
        Some(regia_asset::TextureSlot::Asset(id)) => id,
        other => panic!("no base color map: {other:?}"),
    }
}

#[test]
fn a_webp_texture_imports_required_with_no_other_source() {
    let bytes = textured_file(
        &[webp([10, 200, 30, 255])],
        &["image/webp"],
        json!({"extensions": {"EXT_texture_webp": {"source": 0}}}),
        true,
    );
    let result = import_keep(&bytes).expect("imports");
    let texture = result
        .sub_assets
        .iter()
        .find(|s| s.path == "texture/0")
        .expect("the cooked texture");
    assert_eq!(base_color_slot(&result), texture.id);
    assert!(
        !result
            .extensions
            .unknown_names
            .iter()
            .any(|n| n == "EXT_texture_webp")
    );
    // The pixels reached the cook: the 8x8 image is small, so it stays block-compressed or plain.
    assert!(!texture.payload.is_empty());
}

#[test]
fn webp_wins_over_the_fallback_source_and_the_fallback_image_stays_at_its_index() {
    let bytes = textured_file(
        &[png([255, 0, 0, 255]), webp([0, 255, 0, 255])],
        &["image/png", "image/webp"],
        json!({"source": 0, "extensions": {"EXT_texture_webp": {"source": 1}}}),
        false,
    );
    let result = import_keep(&bytes).expect("imports");
    let webp_texture = result
        .sub_assets
        .iter()
        .find(|s| s.path == "texture/1")
        .expect("the webp image");
    assert_eq!(base_color_slot(&result), webp_texture.id);
}

#[test]
fn a_webp_image_on_disk_goes_through_the_path_import_with_its_pixels() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_webp_disk");
    std::fs::write(root.join("tex.webp"), webp([40, 80, 160, 255])).expect("webp");
    let bin = floats(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    std::fs::write(root.join("data.bin"), &bin).expect("bin");
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["EXT_texture_webp"],
        "extensionsRequired": ["EXT_texture_webp"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "tri"}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "material": 0}]}],
        "materials": [{"name": "m", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
        "textures": [{"extensions": {"EXT_texture_webp": {"source": 0}}}],
        "images": [{"uri": "tex.webp", "mimeType": "image/webp"}],
        "buffers": [{"byteLength": bin.len(), "uri": "data.bin"}],
        "bufferViews": [{"buffer": 0, "byteLength": bin.len()}],
        "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
                       "min": [0, 0, 0], "max": [1, 1, 0]}],
    });
    let path = root.join("tri.gltf");
    std::fs::write(&path, serde_json::to_vec(&document).expect("json")).expect("gltf");
    let opened = open_gltf(&path).expect("opens");
    assert_eq!((opened.images[0].width, opened.images[0].height), (8, 8));
    assert_eq!(&opened.images[0].pixels[..4], &[40, 80, 160, 255]);
    let result = import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        None,
        TextureCookQuality::High,
    )
    .expect("imports");
    assert_eq!(
        base_color_slot(&result),
        result
            .sub_assets
            .iter()
            .find(|s| s.path == "texture/0")
            .expect("t")
            .id
    );
}

#[test]
fn a_damaged_webp_is_an_image_error() {
    let mut damaged = webp([1, 2, 3, 255]);
    damaged.truncate(16);
    let bytes = textured_file(
        &[damaged],
        &["image/webp"],
        json!({"extensions": {"EXT_texture_webp": {"source": 0}}}),
        true,
    );
    assert!(matches!(import_keep(&bytes), Err(ImportError::Image(_))));
}

#[test]
fn a_webp_source_out_of_range_is_an_error() {
    let bytes = textured_file(
        &[webp([1, 2, 3, 255])],
        &["image/webp"],
        json!({"extensions": {"EXT_texture_webp": {"source": 7}}}),
        true,
    );
    assert!(import_keep(&bytes).is_err());
    let bytes = textured_file(
        &[webp([1, 2, 3, 255])],
        &["image/webp"],
        json!({"extensions": {"EXT_texture_webp": {}}}),
        true,
    );
    let error = import_keep(&bytes).expect_err("no source");
    assert_eq!(error.code(), Some("gltf_ext_unsupported"));
}

// ---------------------------------------------------------------------------------------------
// Required and optional extensions
// ---------------------------------------------------------------------------------------------

fn triangle_document() -> (Value, Vec<u8>) {
    let mut build = Builder::new();
    let position = build.add_float3("VEC3", &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    build.accessors[position]["min"] = json!([0, 0, 0]);
    build.accessors[position]["max"] = json!([1, 1, 0]);
    let indices = build.add_indices(&[0, 1, 2]);
    let mut document = build.document(json!({"POSITION": position}), indices, false);
    document.as_object_mut().expect("object").remove("extensionsUsed");
    (document, build.bin)
}

#[track_caller]
fn assert_refused_naming(document: &Value, bin: &[u8], extension: &str) {
    let error = import_keep(&glb(document, bin)).expect_err("refused");
    assert_eq!(error.code(), Some("gltf_ext_unsupported"), "{error}");
    let text = error.to_string();
    assert!(text.starts_with("gltf_ext_unsupported"), "{text}");
    assert!(text.contains(extension), "{text}");
    assert!(
        matches!(&error, ImportError::GltfExtension { extension: name, .. } if name == extension),
        "{error:?}"
    );
}

#[test]
fn a_required_extension_the_importer_cannot_read_is_the_coded_error_naming_it() {
    for name in [
        "KHR_draco_mesh_compression",
        "KHR_texture_basisu",
        "VENDOR_never_heard_of",
        "KHR_materials_pbrSpecularGlossiness",
    ] {
        let (mut document, bin) = triangle_document();
        document["extensionsUsed"] = json!([name]);
        document["extensionsRequired"] = json!([name]);
        assert_refused_naming(&document, &bin, name);
    }
}

#[test]
fn the_error_also_reaches_the_disk_path_and_the_tools_path() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_refusal_paths");
    let (mut document, bin) = triangle_document();
    document["extensionsUsed"] = json!(["KHR_draco_mesh_compression"]);
    document["extensionsRequired"] = json!(["KHR_draco_mesh_compression"]);
    let path = root.join("draco.glb");
    std::fs::write(&path, glb(&document, &bin)).expect("write");
    let error = import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        None,
        TextureCookQuality::High,
    )
    .expect_err("refused");
    assert_eq!(error.code(), Some("gltf_ext_unsupported"));
    let message = regia_asset_pipeline::open_gltf_checked(&path).expect_err("refused");
    assert!(
        message.contains("gltf_ext_unsupported") && message.contains("KHR_draco_mesh_compression"),
        "{message}"
    );
}

#[test]
fn every_required_extension_the_importer_reads_passes() {
    for name in [
        "KHR_mesh_quantization",
        "EXT_meshopt_compression",
        "EXT_mesh_gpu_instancing",
        "EXT_texture_webp",
        "KHR_materials_variants",
        "KHR_materials_clearcoat",
        "KHR_materials_anisotropy",
        "KHR_materials_emissive_strength",
        "KHR_materials_iridescence",
        "KHR_materials_ior",
        "KHR_materials_transmission",
        "KHR_materials_volume",
        "KHR_materials_sheen",
        "KHR_materials_specular",
        "KHR_texture_transform",
        "KHR_lights_punctual",
        "REGIA_materials_detail",
    ] {
        let (mut document, bin) = triangle_document();
        document["extensionsUsed"] = json!([name]);
        document["extensionsRequired"] = json!([name]);
        let result = import_keep(&glb(&document, &bin));
        assert!(result.is_ok(), "{name}: {:?}", result.err());
    }
}

#[test]
fn an_optional_extension_falls_back_with_its_data_kept_for_hosts() {
    let (mut document, bin) = triangle_document();
    document["extensionsUsed"] = json!(["KHR_draco_mesh_compression", "VENDOR_extra"]);
    document["meshes"][0]["primitives"][0]["extensions"] = json!({
        "KHR_draco_mesh_compression": {"bufferView": 0, "attributes": {"POSITION": 0}},
    });
    document["nodes"][0]["extensions"] = json!({"VENDOR_extra": {"a": 1}});
    let result = import_keep(&glb(&document, &bin)).expect("falls back to the plain accessors");
    assert_eq!(first_mesh(&result).positions.len(), 3);
    assert_eq!(
        result.extensions.fallbacks(),
        vec![
            "KHR_draco_mesh_compression".to_string(),
            "VENDOR_extra".to_string()
        ]
    );
    assert_eq!(
        result.extensions.unknown_of("KHR_draco_mesh_compression").count(),
        1
    );
}

#[test]
fn an_optional_extension_with_no_fallback_data_is_still_the_coded_error() {
    // Draco listed as optional, but the accessors have no data to fall back on.
    let (mut document, bin) = triangle_document();
    document["extensionsUsed"] = json!(["KHR_draco_mesh_compression"]);
    document["meshes"][0]["primitives"][0]["extensions"] = json!({
        "KHR_draco_mesh_compression": {"bufferView": 0, "attributes": {"POSITION": 0}},
    });
    document["accessors"][0]
        .as_object_mut()
        .expect("object")
        .remove("bufferView");
    assert_refused_naming(&document, &bin, "KHR_draco_mesh_compression");

    // Basis textures: an optional one with no `source` has nothing to fall back on either.
    let basis = textured_file(
        &[vec![0xab; 16]],
        &["image/ktx2"],
        json!({"extensions": {"KHR_texture_basisu": {"source": 0}}}),
        false,
    );
    let (document, bin) = split_glb(&basis);
    assert_refused_naming(&document, &bin, "KHR_texture_basisu");

    // With a fallback `source` it imports with the fallback image.
    let fallback = textured_file(
        &[png([1, 2, 3, 255]), vec![0xab; 16]],
        &["image/png", "image/png"],
        json!({"source": 0, "extensions": {"KHR_texture_basisu": {"source": 1}}}),
        false,
    );
    let (mut document, bin) = split_glb(&fallback);
    document["images"].as_array_mut().expect("images").truncate(1);
    document["extensionsUsed"] = json!(["KHR_texture_basisu"]);
    let result = import_keep(&glb(&document, &bin)).expect("falls back");
    assert_eq!(
        result.extensions.fallbacks(),
        vec!["KHR_texture_basisu".to_string()]
    );
}

#[test]
fn support_comes_with_the_table_of_a_file_without_blocks() {
    let (mut document, bin) = triangle_document();
    document["extensionsUsed"] = json!(["KHR_mesh_quantization", "EXT_mesh_gpu_instancing"]);
    let result = import_keep(&glb(&document, &bin)).expect("imports");
    assert!(result.extensions.fallbacks().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------------------------------

#[test]
fn mutated_extension_files_never_panic() {
    let load = |bytes: &[u8]| {
        import_gltf_glb_bytes(
            bytes,
            AssetId::default(),
            Forward::Convert,
            "desktop",
            TextureCookQuality::High,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    };
    for (seed, fixture) in [
        (0x0e71, regia_asset_pipeline::fuzz_entry::gltf_ext_fixture()),
        (0x0e72, compressed_fan("TRIANGLES", true).compressed),
        (0x0e73, compressed_fan("INDICES", true).compressed),
        (0x0e74, variants_file()),
        (
            0x0e75,
            instanced_file([1.0, 2.0, 3.0], &[[1.0, 0.0, 0.0]; 4], true),
        ),
    ] {
        fuzz_loader(&fixture, seed, 300, load);
    }
}

#[test]
fn mutated_meshopt_streams_never_panic() {
    fuzz_loader(
        &regia_asset_pipeline::fuzz_entry::meshopt_fixture(),
        0x0e76,
        2000,
        regia_asset_pipeline::fuzz_entry::meshopt_load,
    );
}

/// Every number of `document`, replaced by each of `values`, one at a time. Returns how many files
/// were tried. A deterministic companion to the byte mutation above: counts, offsets, strides and
/// indices all get extreme values, and none may panic or allocate what the value asks for.
fn try_every_number(document: &Value, bin: &[u8], values: &[Value]) -> usize {
    fn paths(value: &Value, here: String, out: &mut Vec<String>) {
        match value {
            Value::Number(_) => out.push(here),
            Value::Array(list) => {
                for (i, item) in list.iter().enumerate() {
                    paths(item, format!("{here}/{i}"), out);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    paths(item, format!("{here}/{key}"), out);
                }
            }
            _ => {}
        }
    }
    let mut numbers = Vec::new();
    paths(document, String::new(), &mut numbers);
    let mut tried = 0;
    for path in &numbers {
        for value in values {
            let mut mutated = document.clone();
            *mutated.pointer_mut(path).expect("the path was just walked") = value.clone();
            // Ok or Err, either is fine: the test is that it returns.
            let _ = import_keep(&glb(&mutated, bin));
            tried += 1;
        }
    }
    tried
}

#[test]
fn every_number_of_an_extension_file_can_be_hostile() {
    let hostile = [
        json!(0),
        json!(1),
        json!(3),
        json!(u32::MAX),
        json!(1u64 << 40),
        json!(u64::MAX),
        json!(-1),
        json!(1.5),
    ];
    for bytes in [
        regia_asset_pipeline::fuzz_entry::gltf_ext_fixture(),
        compressed_fan("TRIANGLES", true).compressed,
        compressed_fan("INDICES", true).compressed,
        instanced_file([1.0, 2.0, 3.0], &[[1.0, 0.0, 0.0]; 2], true),
        variants_file(),
    ] {
        let (document, bin) = split_glb(&bytes);
        assert!(try_every_number(&document, &bin, &hostile) > 100);
    }
}

// ---------------------------------------------------------------------------------------------
// Through the cook
// ---------------------------------------------------------------------------------------------

fn cook(root: &std::path::Path) -> regia_asset_pipeline::CookReport {
    regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
        project_root: root.to_path_buf(),
        platform: "linux".to_string(),
        incremental: false,
    })
    .expect("the cook runs")
}

fn cooked_project(tag: &str, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    std::fs::write(root.join("assets").join(name), bytes).expect("write");
    root
}

#[test]
fn the_cook_writes_the_instance_set_and_a_reader_loads_it() {
    let bytes = instanced_file([0.0; 3], &[[0.0; 3], [2.0, 0.0, 0.0], [4.0, 0.0, 0.0]], false);
    let root = cooked_project("gltf_ext_cook_instances", "trees.glb", &bytes);
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let meta =
        regia_asset::MetaSidecar::from_ron(&std::fs::read(root.join("assets/trees.glb.meta")).expect("meta"))
            .expect("meta parses");
    let entry = meta
        .sub_assets
        .iter()
        .find(|e| e.path == regia_asset::PREFAB_INSTANCES_SUB_PATH)
        .expect("the .meta lists the set");
    let file = regia_asset::cooked_cache_dir(&root, "linux").join(regia_asset::cooked_file_name(entry.id));
    let (_, set) = regia_asset::load_prefab_instances_bytes(&std::fs::read(file).expect("cooked file"))
        .expect("the set loads");
    assert_eq!(set.instance_count(), 3);
    assert_eq!(set.nodes[0].node, "trees");
}

#[test]
fn the_cook_reports_a_refusal_with_its_code_and_cooks_the_rest() {
    let (mut document, bin) = triangle_document();
    document["extensionsUsed"] = json!(["KHR_draco_mesh_compression"]);
    document["extensionsRequired"] = json!(["KHR_draco_mesh_compression"]);
    let root = cooked_project("gltf_ext_cook_refused", "draco.glb", &glb(&document, &bin));
    let (plain, plain_bin) = triangle_document();
    std::fs::write(root.join("assets/plain.glb"), glb(&plain, &plain_bin)).expect("write");
    let report = cook(&root);
    let (path, message) = report
        .errors
        .iter()
        .find(|(path, _)| path.ends_with("draco.glb"))
        .expect("the refused file is an error");
    assert!(
        message.contains("gltf_ext_unsupported") && message.contains("KHR_draco_mesh_compression"),
        "{path}: {message}"
    );
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
}

#[test]
fn a_cook_of_a_compressed_file_needs_no_fallback_file_on_disk() {
    let root = regia_test_utils::unique_temp_dir("gltf_ext_cook_fallback");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    // No index accessor here, so the count has to make whole triangles.
    let (mut vertices, _) = fan();
    vertices.truncate(300);
    let encoded = meshopt::encoding::encode_vertex_buffer(&vertices).expect("vertices");
    std::fs::write(root.join("assets/data.bin"), &encoded).expect("bin");
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["EXT_meshopt_compression"],
        "extensionsRequired": ["EXT_meshopt_compression"],
        "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0, "name": "fan"}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
        "buffers": [
            {"byteLength": encoded.len(), "uri": "data.bin"},
            {"byteLength": vertices.len() * 16, "uri": "gone.bin",
             "extensions": {"EXT_meshopt_compression": {"fallback": true}}},
        ],
        "bufferViews": [{"buffer": 1, "byteLength": vertices.len() * 16, "byteStride": 16, "extensions": {
            "EXT_meshopt_compression": {"buffer": 0, "byteLength": encoded.len(), "byteStride": 16,
                "mode": "ATTRIBUTES", "count": vertices.len()}}}],
        "accessors": [{"bufferView": 0, "componentType": 5126, "count": vertices.len(), "type": "VEC3",
                       "min": [-1, -1, 0], "max": [1, 1, 0]}],
    });
    std::fs::write(
        root.join("assets/fan.gltf"),
        serde_json::to_vec(&document).expect("json"),
    )
    .expect("gltf");
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}
