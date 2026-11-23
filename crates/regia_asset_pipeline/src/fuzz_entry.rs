// SPDX-License-Identifier: MPL-2.0
//! Fuzz entry points for the glTF, OBJ, BVH, USD (ADR-3235), font and MaterialX importers (ADR-2084, ADR-3341): a valid fixture and a loader each, as plain
//! `fn(&[u8])` / `fn(&str)` so the editor can register them in the agent's fuzz registry
//! (`regia_agent::fuzz_targets::FuzzTargetExtensions`; this crate does not depend on `regia_agent`) and a nightly
//! fuzzer could wrap them later. The loaders run the whole import: parse, validate, convert axes, build the cooked
//! mesh and its sub-assets; OBJ first converts to GLB and then imports that.

mod more;

pub use more::*;

use regia_asset::AssetId;

use crate::obj_import::{ObjImportSettings, convert_obj_to_glb};
use crate::{Forward, TextureCookQuality, import_gltf_glb_bytes};

/// The parent id every fuzzed import uses (fixed, so a run repeats).
fn fuzz_parent() -> AssetId {
    AssetId::sub_asset(AssetId::default(), "fuzz")
}

/// Little-endian `f32`s.
fn floats(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A valid GLB 2.0: one node, one mesh with one indexed triangle (positions, normals, texture coordinates) and one
/// material, the JSON and BIN chunks padded to four bytes.
pub fn glb_fixture() -> Vec<u8> {
    let mut bin = floats(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    bin.extend(floats(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0]));
    bin.extend(floats(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0]));
    bin.extend([0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()));
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let json = format!(
        concat!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"#,
            r#""nodes":[{{"mesh":0,"name":"tri"}}],"#,
            r#""materials":[{{"name":"red","pbrMetallicRoughness":{{"baseColorFactor":[1,0,0,1],"metallicFactor":0,"roughnessFactor":0.5}}}}],"#,
            r#""meshes":[{{"name":"tri","primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1,"TEXCOORD_0":2}},"indices":3,"material":0}}]}}],"#,
            r#""buffers":[{{"byteLength":{len}}}],"#,
            r#""bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36,"target":34962}},"#,
            r#"{{"buffer":0,"byteOffset":36,"byteLength":36,"target":34962}},"#,
            r#"{{"buffer":0,"byteOffset":72,"byteLength":24,"target":34962}},"#,
            r#"{{"buffer":0,"byteOffset":96,"byteLength":6,"target":34963}}],"#,
            r#""accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},"#,
            r#"{{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}},"#,
            r#"{{"bufferView":2,"componentType":5126,"count":3,"type":"VEC2"}},"#,
            r#"{{"bufferView":3,"componentType":5123,"count":3,"type":"SCALAR"}}]}}"#
        ),
        len = bin.len()
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(total).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&u32::try_from(json.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&u32::try_from(bin.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

/// The whole glTF/GLB import of `bytes`. The importer has no writer, so the encoding is empty.
pub fn glb_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    import_gltf_glb_bytes(
        bytes,
        fuzz_parent(),
        Forward::Convert,
        "desktop",
        TextureCookQuality::High,
    )
    .map(|_| Vec::new())
    .map_err(|e| e.to_string())
}

/// A valid `EXT_meshopt_compression` stream with its parameters: `[mode, filter, stride, count (u16 LE)]`,
/// then the encoded bytes of 64 vertices of 12 bytes.
pub fn meshopt_fixture() -> Vec<u8> {
    let vertices: Vec<[u8; 12]> = (0..64u8)
        .map(|i| std::array::from_fn(|b| i.wrapping_mul(3).wrapping_add(b as u8)))
        .collect();
    let mut out = vec![0, 0, 12, 64, 0];
    // The encoder has no failure path for a vertex stream.
    out.extend(meshopt::encoding::encode_vertex_buffer(&vertices).unwrap_or_default());
    out
}

/// The whole `EXT_meshopt_compression` decode of `bytes`: the first five bytes pick the mode, the filter,
/// the stride and the element count, and the rest is the compressed stream. No writer, so the encoding
/// is empty.
pub fn meshopt_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use crate::gltf_meshopt::{MeshoptFilter, MeshoptMode, decode_stream};
    let [mode, filter, stride, count_low, count_high, encoded @ ..] = bytes else {
        return Err("shorter than its five parameter bytes".to_string());
    };
    let mode = match mode % 3 {
        0 => MeshoptMode::Attributes,
        1 => MeshoptMode::Triangles,
        _ => MeshoptMode::Indices,
    };
    let filter = match filter % 4 {
        0 => MeshoptFilter::None,
        1 => MeshoptFilter::Octahedral,
        2 => MeshoptFilter::Quaternion,
        _ => MeshoptFilter::Exponential,
    };
    let count = u64::from(u16::from_le_bytes([*count_low, *count_high]));
    decode_stream(mode, filter, u64::from(*stride), count, encoded)
        .map(|_| Vec::new())
        .map_err(|e| e.to_string())
}

/// A valid GLB that uses every data-layout extension: quantized positions, compressed by
/// `EXT_meshopt_compression` over a fallback buffer, drawn through `EXT_mesh_gpu_instancing`.
pub fn gltf_ext_fixture() -> Vec<u8> {
    let positions: [[i16; 4]; 3] = [[0, 0, 0, 0], [16384, 0, 0, 0], [0, 16384, 0, 0]];
    let encoded = meshopt::encoding::encode_vertex_buffer(&positions).unwrap_or_default();
    let mut bin = encoded.clone();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let translations = bin.len();
    bin.extend(floats(&[0.0, 0.0, 0.0, 2.0, 0.0, 0.0]));
    let json = format!(
        concat!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"#,
            r#""extensionsUsed":["KHR_mesh_quantization","EXT_meshopt_compression","EXT_mesh_gpu_instancing"],"#,
            r#""extensionsRequired":["KHR_mesh_quantization","EXT_meshopt_compression"],"#,
            r#""nodes":[{{"mesh":0,"name":"tri","extensions":{{"EXT_mesh_gpu_instancing":{{"attributes":{{"TRANSLATION":1}}}}}}}}],"#,
            r#""meshes":[{{"name":"tri","primitives":[{{"attributes":{{"POSITION":0}}}}]}}],"#,
            r#""buffers":[{{"byteLength":{len}}},{{"byteLength":24,"extensions":{{"EXT_meshopt_compression":{{"fallback":true}}}}}}],"#,
            r#""bufferViews":[{{"buffer":1,"byteOffset":0,"byteLength":24,"byteStride":8,"extensions":"#,
            r#"{{"EXT_meshopt_compression":{{"buffer":0,"byteOffset":0,"byteLength":{encoded},"byteStride":8,"mode":"ATTRIBUTES","count":3}}}}}},"#,
            r#"{{"buffer":0,"byteOffset":{translations},"byteLength":24}}],"#,
            r#""accessors":[{{"bufferView":0,"componentType":5122,"normalized":true,"count":3,"type":"VEC3","min":[0,0,0],"max":[16384,16384,0]}},"#,
            r#"{{"bufferView":1,"componentType":5126,"count":2,"type":"VEC3"}}]}}"#
        ),
        len = bin.len(),
        encoded = encoded.len(),
        translations = translations,
    );
    glb_of(json.into_bytes(), &bin)
}

/// The JSON and BIN chunks of a GLB, each padded to four bytes.
fn glb_of(mut json: Vec<u8>, bin: &[u8]) -> Vec<u8> {
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(total).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&u32::try_from(json.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&u32::try_from(bin.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(bin);
    out
}

/// A valid OBJ: two objects (a quad and a triangle) with positions, texture coordinates, normals, two materials,
/// a smoothing group and negative (relative) indices.
pub fn obj_fixture() -> String {
    "# fuzz fixture\n\
     o quad\n\
     v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
     vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\n\
     vn 0 0 1\n\
     usemtl red\n\
     s 1\n\
     f 1/1/1 2/2/1 3/3/1 4/4/1\n\
     o tri\n\
     v 2 0 0\nv 3 0 0\nv 2 1 0\n\
     usemtl blue\n\
     f -3//1 -2//1 -1//1\n"
        .to_string()
}

/// OBJ to GLB (no `mtllib` directory: nothing outside the text is read), then the whole glTF import of the GLB. No
/// writer, so the encoding is empty.
pub fn obj_load(text: &str) -> Result<String, String> {
    let conversion = convert_obj_to_glb(text.as_bytes(), None, &ObjImportSettings::default())
        .map_err(|e| e.to_string())?;
    glb_load(&conversion.glb)?;
    Ok(String::new())
}

/// A valid font: the generated variable fixture (ADR-2697).
pub fn font_fixture() -> Vec<u8> {
    crate::font_subset_fixture::tiny_variable_font()
}

/// Shapes a text with the font and subsets to it (composites and a ligature in reach). An accepted result must read
/// back, its glyphs must draw or refuse without a panic, and it must shape too. Returns nothing: the writer
/// is not a round trip of its input.
///
/// # Errors
///
/// The subsetter's refusal, or a result that does not read back.
pub fn font_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use skrifa::MetadataProvider;
    use skrifa::instance::{LocationRef, Size};
    use skrifa::outline::DrawSettings;
    use skrifa::outline::pen::NullPen;
    use skrifa::raw::TableProvider;
    const TEXT: &str = "fie\u{113}xq";
    let keep = TEXT.chars().collect();
    let runs = std::collections::BTreeSet::from([TEXT.to_owned()]);
    let glyphs = crate::font_subset::shaped_glyph_ids(bytes, &runs);
    let output = crate::font_subset::subset_font_with(bytes, &keep, &glyphs)
        .map_err(|e| format!("{}: {e}", e.code()))?;
    if output.kept_whole.is_none() && output.bytes.len() < bytes.len() {
        let font =
            skrifa::raw::FontRef::new(&output.bytes).map_err(|e| format!("subset does not read: {e}"))?;
        let count = font
            .maxp()
            .map_err(|e| format!("subset has no maxp: {e}"))?
            .num_glyphs();
        let outlines = font.outline_glyphs();
        for glyph in 0..u32::from(count) {
            if let Some(outline) = outlines.get(glyph.into()) {
                let settings = DrawSettings::unhinted(Size::unscaled(), LocationRef::default());
                let _ = outline.draw(settings, &mut NullPen);
            }
        }
        let _ = crate::font_subset::shaped_glyph_ids(&output.bytes, &runs);
    }
    Ok(Vec::new())
}

/// A valid BVH: a hip and a spine with an end site, two frames (ADR-2532).
pub fn bvh_fixture() -> String {
    crate::bvh_import::FIXTURE_BVH.to_string()
}

/// A valid Aseprite file (ADR-3358): 8x8 RGBA, one layer, two frames with one tag, compressed
/// cels and a palette-free file (RGBA needs none). Built by hand — no third-party art.
pub fn aseprite_fixture() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: u16, body: &[u8]) {
        out.extend((body.len() as u32 + 6).to_le_bytes());
        out.extend(kind.to_le_bytes());
        out.extend(body);
    }
    let mut frame0 = Vec::new();
    let mut layer = Vec::new();
    layer.extend(1u16.to_le_bytes()); // Visible.
    layer.extend(0u16.to_le_bytes()); // Normal layer.
    layer.extend(0u16.to_le_bytes()); // Child level.
    layer.extend([0u8; 4]);
    layer.extend(0u16.to_le_bytes()); // Blend: Normal.
    layer.push(255); // Opacity.
    layer.extend([0; 3]);
    layer.extend(1u16.to_le_bytes());
    layer.extend(b"l");
    chunk(&mut frame0, 0x2004, &layer);
    let mut cel = Vec::new();
    cel.extend(0u16.to_le_bytes()); // Layer 0.
    cel.extend(0i16.to_le_bytes());
    cel.extend(0i16.to_le_bytes());
    cel.push(255);
    cel.extend(2u16.to_le_bytes()); // Compressed.
    cel.extend(0i16.to_le_bytes()); // Z-index.
    cel.extend([0; 5]);
    cel.extend(8u16.to_le_bytes());
    cel.extend(8u16.to_le_bytes());
    let pixels: Vec<u8> = (0..64).flat_map(|i| [i as u8, 2, 3, 255]).collect();
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    use std::io::Write as _;
    encoder.write_all(&pixels).expect("fixture compresses");
    cel.extend(encoder.finish().expect("fixture finishes"));
    chunk(&mut frame0, 0x2005, &cel);
    let mut frame1 = Vec::new();
    chunk(&mut frame1, 0x2005, &cel);
    let mut tags = Vec::new();
    tags.extend(1u16.to_le_bytes());
    tags.extend([0; 8]);
    tags.extend(0u16.to_le_bytes());
    tags.extend(1u16.to_le_bytes());
    tags.push(0); // Forward.
    tags.extend(0u16.to_le_bytes());
    tags.extend([0; 6]);
    tags.extend([0; 3]);
    tags.push(0);
    tags.extend(4u16.to_le_bytes());
    tags.extend(b"idle");
    chunk(&mut frame0, 0x2018, &tags);

    let mut file = Vec::new();
    let size = 128 + (16 + frame0.len()) + (16 + frame1.len());
    file.extend((size as u32).to_le_bytes());
    file.extend(0xA5E0u16.to_le_bytes());
    file.extend(2u16.to_le_bytes()); // Frames.
    file.extend(8u16.to_le_bytes());
    file.extend(8u16.to_le_bytes());
    file.extend(32u16.to_le_bytes());
    file.extend(1u32.to_le_bytes()); // Layer opacity valid.
    file.extend(100u16.to_le_bytes());
    file.extend([0; 8]);
    file.push(0);
    file.extend([0; 2 + 3 + 2 + 8 + 84]); // Color count, ignore, pixel ratio, grid.
    for (index, (body, chunks)) in [(frame0, 3u32), (frame1, 1u32)].iter().enumerate() {
        file.extend(((body.len() + 16) as u32).to_le_bytes());
        file.extend(0xF1FAu16.to_le_bytes());
        file.extend((*chunks as u16).to_le_bytes());
        file.extend((100u16 + index as u16).to_le_bytes());
        file.extend([0; 2]);
        file.extend(chunks.to_le_bytes());
        file.extend(body);
    }
    file
}

/// The whole Aseprite import of `bytes`: parse, composite, expand tags, build the strips and
/// pack them. No writer, so the encoding is empty.
pub fn aseprite_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let imported = crate::aseprite_import::import_aseprite(bytes, "fuzz").map_err(|e| e.to_string())?;
    let entries: Vec<crate::atlas_cook::Entry> = imported
        .sprites
        .iter()
        .map(|sprite| {
            let image = image::RgbaImage::from_raw(
                sprite.image.width,
                sprite.image.height,
                sprite.image.rgba8.clone(),
            )
            .ok_or_else(|| format!("sprite `{}` has bad pixels", sprite.name))?;
            Ok(crate::atlas_cook::Entry::decoded(
                sprite.name.clone(),
                image,
                sprite.pivot,
                sprite.nine_slice,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    crate::atlas_cook::pack_entries(entries, 2048, 1, 0, false).map_err(|e| e.to_string())?;
    Ok(Vec::new())
}

/// A valid LDtk project (ADR-3358): one level with a tile layer and an entity, hand-written —
/// no third-party level files.
pub fn ldtk_fixture() -> String {
    r##"{
        "jsonVersion": "1.5.3",
        "externalLevels": false,
        "bgColor": "#000000",
        "iid": "fuzz",
        "toc": [],
        "worlds": [],
        "levels": [{
            "identifier": "L1",
            "iid": "f1",
            "uid": 1,
            "worldX": 0,
            "worldY": 0,
            "worldDepth": 0,
            "pxWid": 16,
            "pxHei": 8,
            "externalRelPath": null,
            "__bgColor": null,
            "bgRelPath": null,
            "__neighbours": [],
            "fieldInstances": [],
            "layerInstances": [{
                "__type": "Tiles",
                "__identifier": "ground",
                "__gridSize": 8,
                "__cWid": 2,
                "__cHei": 1,
                "__opacity": 1.0,
                "visible": true,
                "__pxTotalOffsetX": 0,
                "__pxTotalOffsetY": 0,
                "__tilesetDefUid": 7,
                "gridTiles": [
                    { "t": 0, "f": 0, "px": [0, 0], "src": [0, 0], "a": 1.0 },
                    { "t": 1, "f": 1, "px": [8, 0], "src": [8, 0], "a": 1.0 }
                ],
                "autoLayerTiles": [],
                "intGridCsv": [],
                "entityInstances": [],
                "iid": "fl",
                "layerDefUid": 1,
                "levelId": 1,
                "pxOffsetX": 0,
                "pxOffsetY": 0
            }]
        }],
        "defs": {
            "tilesets": [{
                "uid": 7,
                "identifier": "terrain",
                "relPath": "ts.png",
                "pxWid": 16,
                "pxHei": 8,
                "tileGridSize": 8,
                "__cWid": 2,
                "__cHei": 1,
                "spacing": 0,
                "padding": 0,
                "customData": [],
                "enumTags": [],
                "tags": []
            }],
            "layers": [],
            "entities": [],
            "enums": [],
            "externalEnums": [],
            "levelFields": []
        }
    }"##
    .to_string()
}

/// The whole LDtk import of `text`: parse into the merged map (validated inside) and every
/// level-local map (validated too). No writer, so the encoding is empty.
pub fn ldtk_load(text: &str) -> Result<String, String> {
    let imported = crate::ldtk_import::import_ldtk(text.as_bytes(), std::path::Path::new("."))
        .map_err(|e| e.to_string())?;
    if imported.levels.is_empty() {
        return Err("the import produced no level maps".to_string());
    }
    Ok(String::new())
}

/// The whole BVH import of `text`: parse, convert, reduce keys, and retarget the clip onto its own skeleton
/// (by name) when it has joints. No writer, so the encoding is empty.
pub fn bvh_load(text: &str) -> Result<String, String> {
    let settings = crate::bvh_import::BvhImportSettings::default();
    let import =
        crate::bvh_import::convert_bvh(text.as_bytes(), "fuzz", &settings).map_err(|e| e.to_string())?;
    crate::bvh_import::retarget_import(&import, &import.skeleton, &std::collections::BTreeMap::new())
        .map_err(|e| e.to_string())?;
    Ok(String::new())
}

/// A valid SVG: a path with a gradient fill, a stroked circle, a clip path, a local `<use>` reference and a
/// view box (ADR-3350).
pub fn svg_fixture() -> String {
    r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="24" height="24" viewBox="0 0 24 24">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#ff8800"/><stop offset="1" stop-color="#0044ff" stop-opacity="0.6"/></linearGradient>
    <clipPath id="c"><rect x="2" y="2" width="20" height="20"/></clipPath>
    <path id="p" d="M4 4 L20 4 L12 20 Z"/>
  </defs>
  <g clip-path="url(#c)"><use xlink:href="#p" fill="url(#g)"/></g>
  <circle cx="12" cy="12" r="6" fill="none" stroke="#202020" stroke-width="1.5" stroke-dasharray="2 1"/>
</svg>
"##
    .to_string()
}

/// The whole SVG import of `text`: the document checks, then a rasterization at the 1x and 2x atlas scales and at a
/// 16 px edge. An accepted document must give rasters of the planned size. No writer, so the encoding is empty.
pub fn svg_load(text: &str) -> Result<String, String> {
    #[cfg(not(feature = "svg_import"))]
    {
        let _ = text;
        Err(crate::disabled_importer("svg")
            .map(|e| e.to_string())
            .unwrap_or_default())
    }
    #[cfg(feature = "svg_import")]
    {
        svg_load_enabled(text)
    }
}

/// [`svg_load`] with the `svg_import` feature (ADR-7972).
#[cfg(feature = "svg_import")]
fn svg_load_enabled(text: &str) -> Result<String, String> {
    let doc = crate::svg_import::SvgDocument::parse(text.as_bytes()).map_err(|e| e.to_string())?;
    for edge in [16, 32] {
        let [w, h] = doc.size_for_edge(edge).map_err(|e| e.to_string())?;
        let raster = doc.rasterize(w, h).map_err(|e| e.to_string())?;
        if raster.rgba8.len() != (w as usize) * (h as usize) * 4 {
            return Err(format!("a {w}x{h} raster has {} bytes", raster.rgba8.len()));
        }
    }
    Ok(String::new())
}

/// A valid MaterialX document (ADR-3341): a node graph with an interface input, an image, a mix and a noise feeding a
/// Standard Surface, and its material.
pub fn materialx_fixture() -> String {
    r#"<?xml version="1.0"?>
<materialx version="1.38" fileprefix="textures/">
  <nodegraph name="NG">
    <input name="tint" type="color3" value="0.8, 0.4, 0.2" />
    <image name="tex" type="color3">
      <input name="file" type="filename" value="albedo.png" colorspace="srgb_texture" />
    </image>
    <noise2d name="n" type="float"><input name="amplitude" type="float" value="0.5" /></noise2d>
    <mix name="m" type="color3">
      <input name="fg" type="color3" nodename="tex" />
      <input name="bg" type="color3" interfacename="tint" />
      <input name="mix" type="float" nodename="n" />
    </mix>
    <output name="out" type="color3" nodename="m" />
  </nodegraph>
  <standard_surface name="SR" type="surfaceshader">
    <input name="base_color" type="color3" nodegraph="NG" output="out" />
    <input name="specular_roughness" type="float" value="0.4" />
  </standard_surface>
  <surfacematerial name="M" type="material">
    <input name="surfaceshader" type="surfaceshader" nodename="SR" />
  </surfacematerial>
</materialx>
"#
    .to_string()
}

/// The whole MaterialX import of `text` (parse, flatten, map) and an unvalidated compile of the graph it accepts:
/// an accepted document must give a graph the compiler takes or rejects without a panic. No writer, so the
/// encoding is empty.
///
/// # Errors
///
/// The importer's error code and message.
pub fn materialx_load(text: &str) -> Result<String, String> {
    let settings = crate::materialx_import::MaterialXSettings::default();
    let import = crate::materialx_import::import_materialx_str(text, "assets/materials", &settings)
        .map_err(|e| format!("{}: {e}", e.code()))?;
    let _ = regia_matgraph::compile_unvalidated(&import.graph);
    Ok(String::new())
}

/// A valid DDS fixture: a DX9 BC1 cube map, 4×4 faces with a three-level chain, each face a
/// different solid color.
#[must_use]
pub fn dds_fixture() -> Vec<u8> {
    let mut h = vec![0u8; 124];
    let put = |h: &mut Vec<u8>, at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put(&mut h, 0, 124);
    put(&mut h, 4, 0x2_100F);
    put(&mut h, 8, 4);
    put(&mut h, 12, 4);
    put(&mut h, 24, 3);
    put(&mut h, 72, 32);
    put(&mut h, 76, 0x4);
    h[80..84].copy_from_slice(b"DXT1");
    put(&mut h, 104, 0x1008);
    put(&mut h, 108, 0x200 | 0xFC00);
    let mut out = b"DDS ".to_vec();
    out.extend_from_slice(&h);
    for face in 0..6u16 {
        let color = 0x0841u16.wrapping_mul(face + 1);
        for blocks in [1usize, 1, 1] {
            for _ in 0..blocks {
                out.extend_from_slice(&color.to_le_bytes());
                out.extend_from_slice(&color.to_le_bytes());
                out.extend_from_slice(&[0; 4]);
            }
        }
    }
    out
}

/// The whole DDS import of `bytes` for the desktop and the Android cook; the encoding is the
/// two cooked payloads back to back, so a change to either shows.
pub fn dds_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = crate::dds_import::import_dds_bytes(bytes, "desktop")?;
    out.extend(crate::dds_import::import_dds_bytes(bytes, "android")?);
    Ok(out)
}

/// A valid binary PLY cube (ADR-3244).
pub fn ply_fixture() -> Vec<u8> {
    crate::ply_import::cube_fixture(crate::ply_import::PlyEncoding::BinaryLittleEndian)
}

/// PLY to GLB, then the whole glTF import of the GLB. No writer, so the encoding is empty.
pub fn ply_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let conversion =
        crate::ply_import::convert_ply_to_glb(bytes, &crate::ply_import::PlyImportSettings::default())
            .map_err(|e| e.to_string())?;
    glb_load(&conversion.glb)?;
    Ok(Vec::new())
}

/// A valid text STL cube (ADR-3244). Binary STL has no magic number for the harness's hostile-header check to break;
/// the binary reader is fuzzed by `tests/mesh_image_import.rs`.
pub fn stl_fixture() -> String {
    crate::stl_import::cube_fixture_ascii(10.0)
}

/// STL to GLB, then the whole glTF import of the GLB. No writer, so the encoding is empty.
pub fn stl_load(text: &str) -> Result<String, String> {
    let conversion = crate::stl_import::convert_stl_to_glb(
        text.as_bytes(),
        &crate::stl_import::StlImportSettings::default(),
    )
    .map_err(|e| e.to_string())?;
    glb_load(&conversion.glb)?;
    Ok(String::new())
}

/// A valid PSD: a 3 by 2 RGB composite with alpha and one named layer, RLE (ADR-3244).
pub fn psd_fixture() -> Vec<u8> {
    crate::psd_import::fixture::small()
}

/// Decodes the composite and then the named layer `Sprite` (both paths). No writer, so the
/// encoding is empty.
pub fn psd_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use crate::psd_import::{PsdImportSettings, decode_psd};
    let composite = decode_psd(bytes, &PsdImportSettings::default());
    let layer = decode_psd(
        bytes,
        &PsdImportSettings {
            layer: Some("Sprite".into()),
            ..PsdImportSettings::default()
        },
    );
    composite
        .and(layer)
        .map(|_| Vec::new())
        .map_err(|e| e.to_string())
}

/// A valid USD text layer (ADR-3235): a Y-up, meter scene with an Xform holding a quad mesh (normals, `st` UVs) bound
/// to a preview-surface material, an internal reference, a variant set and a camera.
pub fn usd_fixture() -> String {
    crate::usd_import::FUZZ_USDA.to_string()
}

/// The whole USD import of `text`: walk, compose, convert to GLB, then the glTF import of the GLB. Nothing outside the
/// text is read. No writer, so the encoding is empty. The error carries the `usd_*` code.
pub fn usd_load(text: &str) -> Result<String, String> {
    usd_bytes_load("fuzz.usda", text.as_bytes())?;
    Ok(String::new())
}

/// A valid USD package (ADR-3235): the [`usd_fixture`] scene re-encoded as a binary crate layer, stored in a `.usdz`
/// with a PNG texture its material reads.
/// Empty without the `usd_import` feature (ADR-7972).
pub fn usdz_fixture() -> Vec<u8> {
    #[cfg(feature = "usd_import")]
    {
        crate::usd_import::fuzz_usdz_fixture()
    }
    #[cfg(not(feature = "usd_import"))]
    {
        Vec::new()
    }
}

/// The whole USD import of package bytes (crate or text layer inside), as [`usd_load`].
pub fn usdz_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    usd_bytes_load("fuzz.usdz", bytes)?;
    Ok(Vec::new())
}

#[cfg(not(feature = "usd_import"))]
fn usd_bytes_load(name: &str, bytes: &[u8]) -> Result<(), String> {
    let _ = (name, bytes);
    Err(crate::disabled_importer("usd")
        .map(|e| e.to_string())
        .unwrap_or_default())
}

#[cfg(feature = "usd_import")]
fn usd_bytes_load(name: &str, bytes: &[u8]) -> Result<(), String> {
    use crate::usd_import::{UsdImportSettings, UsdSource, convert_usd_to_glb};
    let conversion = convert_usd_to_glb(UsdSource::Bytes { name, bytes }, &UsdImportSettings::default())
        .map_err(|e| format!("{e} ({})", e.code()))?;
    glb_load(&conversion.glb)?;
    Ok(())
}

/// Fixed-ceiling glTF lint harness: JSON/chunks/URI validation, buffer decoding
/// and normalization, with no external IO. Counters are asserted even on error.
///
/// # Errors
/// Malformed input, unsupported external IO or the fixed fuzz policy ceiling.
pub fn bounded_gltf_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use crate::content_lint::{ContentBudget, ContentLimits};
    let limits = ContentLimits {
        max_encoded_bytes: 128 * 1024,
        max_json_bytes: 64 * 1024,
        max_external_bytes: 0,
        max_decoded_bytes: 1024 * 1024,
        max_accessors: 1024,
        max_work: 64 * 1024,
        max_copy_bytes: 0,
    };
    let mut budget = ContentBudget::new(limits.clone(), &|| false);
    let result = if bytes.len() > 64 * 1024 {
        Err(crate::content_lint::ContentError::Limit("fuzz input bytes"))
    } else {
        crate::content_lint::loader::open_bytes(bytes, &mut budget)
    };
    let usage = budget.usage();
    assert!(usage.encoded_bytes <= limits.max_encoded_bytes);
    assert!(usage.external_bytes <= limits.max_external_bytes);
    assert!(usage.decoded_bytes <= limits.max_decoded_bytes);
    assert!(usage.buffer_bytes_allocated <= limits.max_decoded_bytes);
    assert!(usage.accessors <= limits.max_accessors);
    assert!(usage.work <= limits.max_work);
    result.map(|()| Vec::new()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixtures_import() {
        glb_load(&glb_fixture()).expect("the GLB fixture imports");
        glb_load(&gltf_ext_fixture()).expect("the extension fixture imports");
        meshopt_load(&meshopt_fixture()).expect("the meshopt fixture decodes");
        obj_load(&obj_fixture()).expect("the OBJ fixture imports");
        bvh_load(&bvh_fixture()).expect("the BVH fixture imports");
        #[cfg(feature = "svg_import")]
        svg_load(&svg_fixture()).expect("the SVG fixture imports");
        aseprite_load(&aseprite_fixture()).expect("the Aseprite fixture imports");
        ldtk_load(&ldtk_fixture()).expect("the LDtk fixture imports");
        materialx_load(&materialx_fixture()).expect("the MaterialX fixture imports");
        let import = crate::materialx_import::import_materialx_str(
            &materialx_fixture(),
            "assets/materials",
            &crate::materialx_import::MaterialXSettings::default(),
        )
        .expect("imports");
        regia_matgraph::compile(&import.graph).expect("the fixture's graph compiles and validates");
        ply_load(&ply_fixture()).expect("the PLY fixture imports");
        stl_load(&stl_fixture()).expect("the STL fixture imports");
        psd_load(&psd_fixture()).expect("the PSD fixture imports");
        #[cfg(feature = "usd_import")]
        usd_load(&usd_fixture()).expect("the USD fixture imports");
        #[cfg(feature = "usd_import")]
        usdz_load(&usdz_fixture()).expect("the USDZ fixture imports");
    }

    /// ADR-3262/8226: every shared entry loads its own fixture and has a unique registered name.
    #[test]
    fn the_more_entries_load_their_fixtures() {
        let entries = more_entries();
        let mut names = std::collections::BTreeSet::new();
        for entry in &entries {
            assert!(
                entry.name.starts_with("import.") || entry.name == "content.gltf_bounded",
                "{}",
                entry.name
            );
            assert!(names.insert(entry.name), "duplicate {}", entry.name);
            let loaded = match entry.kind {
                ImportEntryKind::Bytes { fixture, load } => load(&fixture()).map(drop),
                ImportEntryKind::Text { fixture, load } => load(&fixture()).map(drop),
            };
            match loaded {
                Ok(()) => {}
                // ADR-7972: the FBX entry stays in the table; a build without its parser answers
                // the documented code (the default build never takes this arm).
                Err(e)
                    if !cfg!(feature = "fbx_import")
                        && entry.name == "import.fbx"
                        && e.starts_with(crate::IMPORTER_DISABLED_CODE) => {}
                Err(e) => panic!("{}: {e}", entry.name),
            }
        }
        assert_eq!(entries.len(), 15);
    }
}
