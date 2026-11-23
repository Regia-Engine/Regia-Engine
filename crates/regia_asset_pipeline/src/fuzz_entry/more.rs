// SPDX-License-Identifier: MPL-2.0
//! Fuzz entry points for the importers beyond glTF, OBJ, BVH and fonts (ADR-3262): Radiance HDR, OpenEXR, Tiled
//! (`.tmx`, `.tmj`, `.tsx`, `.tsj`, with the Wang sets they carry), IES photometric files, the audio cook inputs
//! (WAV, FLAC, Ogg Vorbis), WebM, raw 16-bit heightmaps, `.atlas.ron` and FBX. Each has a valid fixture built in code
//! (the one exception, an Ogg Vorbis tone of 5.5 KB, is a file; no encoder exists in the tree) and a loader that runs the
//! whole import on the bytes. The loaders that read files (Tiled, the atlas) write the input into a private scratch
//! directory first, next to the fixed neighbours the format names (an external tileset, the sprite images).
//!
//! The entries are plain function pointers, in [`more_entries`], so the editor (which registers them in the agent's
//! fuzz registry) and `xtask` (the nightly fuzzer) share one list.
//!
//! Input limits the fuzzing led to are constants in the importers, named in `docs/decisions.md` ADR-3262.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_asset::AssetId;
use regia_asset::hdr_texture::HdrImage;

use crate::{Forward, TextureCookQuality};

/// How one entry takes its input.
#[derive(Clone, Copy)]
pub enum ImportEntryKind {
    /// Bytes in, an encoding (usually empty) out.
    Bytes {
        /// A valid input.
        fixture: fn() -> Vec<u8>,
        /// The whole import.
        load: fn(&[u8]) -> Result<Vec<u8>, String>,
    },
    /// Text in, an encoding (usually empty) out.
    Text {
        /// A valid input.
        fixture: fn() -> String,
        /// The whole import.
        load: fn(&str) -> Result<String, String>,
    },
}

/// A named importer entry point.
#[derive(Clone, Copy)]
pub struct ImportEntry {
    /// The registry name, `import.<what>` or `content.gltf_bounded`.
    pub name: &'static str,
    /// The input kind, fixture and loader.
    pub kind: ImportEntryKind,
}

/// The importer entries of ADR-3262, in registry order.
#[must_use]
pub fn more_entries() -> Vec<ImportEntry> {
    use ImportEntryKind::{Bytes, Text};
    let bytes = |name, fixture, load| ImportEntry {
        name,
        kind: Bytes { fixture, load },
    };
    let text = |name, fixture, load| ImportEntry {
        name,
        kind: Text { fixture, load },
    };
    vec![
        bytes("import.hdr", hdr_fixture, hdr_load),
        bytes("import.exr", exr_fixture, exr_load),
        text("import.tiled_tmx", tmx_fixture, tmx_load),
        text("import.tiled_tmj", tmj_fixture, tmj_load),
        text("import.tiled_tsx", tsx_fixture, tsx_load),
        text("import.tiled_tsj", tsj_fixture, tsj_load),
        text("import.ies", ies_fixture, ies_load),
        bytes("import.audio_wav", wav_fixture, audio_load),
        bytes("import.audio_flac", flac_fixture, audio_load),
        bytes("import.audio_ogg", ogg_fixture, audio_load),
        bytes("import.webm", webm_fixture, webm_load),
        bytes("import.heightmap_r16", r16_fixture, r16_load),
        text("import.atlas_ron", atlas_fixture, atlas_load),
        text("import.fbx", fbx_fixture, fbx_load),
        bytes(
            "content.gltf_bounded",
            super::gltf_ext_fixture,
            super::bounded_gltf_load,
        ),
    ]
}

/// A private scratch directory, removed when dropped (a run that aborts leaves it in the temp directory).
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Result<Self, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-fuzz-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("scratch directory: {e}"))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).map_err(|e| format!("scratch file {name}: {e}"))?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A PNG of `width x height` RGBA8 with a gradient (the sprite sheet and tileset images).
fn png_fixture(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([
            (x * 8) as u8,
            (y * 8) as u8,
            ((x + y) * 4) as u8,
            if x < 2 { 0 } else { 255 },
        ])
    });
    let mut out = Cursor::new(Vec::new());
    match image.write_to(&mut out, image::ImageFormat::Png) {
        Ok(()) => out.into_inner(),
        Err(_) => Vec::new(),
    }
}

// --- Radiance HDR and OpenEXR ---------------------------------------------------------------------------------------

/// A smooth sky, `width x height` linear RGB.
fn sky_texels(width: u32, height: u32) -> Vec<[f32; 3]> {
    (0..width * height)
        .map(|i| {
            let (x, y) = (
                (i % width) as f32 / width as f32,
                (i / width) as f32 / height as f32,
            );
            [2.0 + 1.5 * y + 0.3 * x, 3.0 + 2.0 * y, 5.0 + 3.0 * y - 0.5 * x]
        })
        .collect()
}

/// A valid Radiance `.hdr` (RLE scanlines), 16 x 8.
pub fn hdr_fixture() -> Vec<u8> {
    let pixels: Vec<image::Rgb<f32>> = sky_texels(16, 8).into_iter().map(image::Rgb).collect();
    let mut out = Vec::new();
    match image::codecs::hdr::HdrEncoder::new(&mut out).encode(&pixels, 16, 8) {
        Ok(()) => out,
        Err(_) => Vec::new(),
    }
}

/// A valid OpenEXR file (RGBA, 32-bit float), 8 x 4.
pub fn exr_fixture() -> Vec<u8> {
    use image::ImageEncoder;
    let texels: Vec<u8> = sky_texels(8, 4)
        .into_iter()
        .flat_map(|[r, g, b]| [r, g, b, 1.0])
        .flat_map(f32::to_ne_bytes)
        .collect();
    let mut out = Cursor::new(Vec::new());
    match image::codecs::openexr::OpenExrEncoder::new(&mut out).write_image(
        &texels,
        8,
        4,
        image::ExtendedColorType::Rgba32F,
    ) {
        Ok(()) => out.into_inner(),
        Err(_) => Vec::new(),
    }
}

/// The whole HDR cook of a decoded source: validate, then the 2D cook and the six-face cube cook (BC6H, uncompressed
/// half float).
fn hdr_cook(bytes: &[u8], ext: &str) -> Result<Vec<u8>, String> {
    use crate::hdr_import::{cook_hdr_ktx2, decode_hdr_source, equirect_to_cube};
    use crate::texture_settings::{HdrEncodingSetting, TextureImporterSettings};
    let source = decode_hdr_source(bytes, ext).map_err(|e| e.to_string())?;
    let flat = TextureImporterSettings {
        hdr_encoding: Some(HdrEncodingSetting::Auto),
        ..TextureImporterSettings::default()
    };
    cook_hdr_ktx2(&source, &flat, "linux").map_err(|e| e.to_string())?;
    let cube = TextureImporterSettings {
        cube_face_px: Some(4),
        compress: Some(false),
        ..TextureImporterSettings::default()
    };
    cook_hdr_ktx2(&source, &cube, "android").map_err(|e| e.to_string())?;
    let equirect = HdrImage {
        width: source.width,
        height: source.height,
        texels: source.texels.iter().map(|t| [t[0], t[1], t[2]]).collect(),
    };
    let faces = equirect_to_cube(&equirect, 4);
    if faces.len() != 6 {
        return Err(format!("cube cook made {} faces", faces.len()));
    }
    Ok(Vec::new())
}

/// The whole import of a `.hdr`.
pub fn hdr_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    hdr_cook(bytes, "hdr")
}

/// The whole import of an `.exr`.
pub fn exr_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    hdr_cook(bytes, "exr")
}

// --- Tiled ----------------------------------------------------------------------------------------------------------

/// The Wang corner tiles of the fixtures: tile `(mask * 7) % 16` shows the corners in `mask` of one color.
fn wang_tiles() -> Vec<(u32, [u8; 8])> {
    (1..16u32)
        .map(|mask| {
            let on = |bit: u32| u8::from(mask & bit != 0);
            ((mask * 7) % 16, [0, on(2), 0, on(4), 0, on(8), 0, on(1)])
        })
        .collect()
}

/// A tileset in `.tsx`: 16 tiles of 8 x 8 in a 32 x 32 image, a solid tile, an animated tile, a property and a Wang set.
pub fn tsx_fixture() -> String {
    let tiles: String = wang_tiles()
        .iter()
        .map(|(id, w)| {
            let ids: Vec<String> = w.iter().map(ToString::to_string).collect();
            format!("    <wangtile tileid=\"{id}\" wangid=\"{}\"/>\n", ids.join(","))
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <tileset version=\"1.10\" tiledversion=\"1.10.2\" name=\"t\" tilewidth=\"8\" tileheight=\"8\" tilecount=\"16\" columns=\"4\">\n \
         <image source=\"t.png\" width=\"32\" height=\"32\"/>\n \
         <tile id=\"1\" type=\"solid\"/>\n \
         <tile id=\"2\"><properties><property name=\"collision\" value=\"one_way\"/></properties>\
         <animation><frame tileid=\"2\" duration=\"100\"/><frame tileid=\"3\" duration=\"100\"/></animation></tile>\n \
         <wangsets>\n  <wangset name=\"Ground\" type=\"corner\" tile=\"-1\">\n   \
         <wangcolor name=\"Grass\" color=\"#00ff00\" tile=\"-1\" probability=\"1\"/>\n{tiles}  </wangset>\n </wangsets>\n</tileset>\n"
    )
}

/// The same tileset in `.tsj`.
pub fn tsj_fixture() -> String {
    let wangtiles: Vec<serde_json::Value> = wang_tiles()
        .iter()
        .map(|(id, w)| serde_json::json!({ "tileid": id, "wangid": w }))
        .collect();
    serde_json::json!({
        "type": "tileset", "version": "1.10", "name": "t", "image": "t.png",
        "imagewidth": 32, "imageheight": 32, "tilewidth": 8, "tileheight": 8, "tilecount": 16, "columns": 4,
        "tiles": [
            { "id": 1, "type": "solid" },
            { "id": 2, "properties": [{ "name": "collision", "type": "string", "value": "one_way" }],
              "animation": [{ "tileid": 2, "duration": 100 }, { "tileid": 3, "duration": 100 }] }
        ],
        "wangsets": [{
            "name": "Ground", "type": "corner", "tile": -1,
            "colors": [{ "name": "Grass", "color": "#00ff00", "tile": -1, "probability": 1 }],
            "wangtiles": wangtiles
        }]
    })
    .to_string()
}

/// A base64 (uncompressed) tile layer: `width x height` little-endian global ids.
fn base64_layer(ids: &[u32]) -> String {
    use base64::Engine;
    let raw: Vec<u8> = ids.iter().flat_map(|id| id.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(raw)
}

/// A `.tmx` with an inline tileset, a CSV layer, a base64 layer with a flipped tile, and an object layer.
pub fn tmx_fixture() -> String {
    let flipped = base64_layer(&[
        1,
        2 | 0x8000_0000,
        3 | 0x4000_0000,
        0,
        5,
        6,
        0xA000_0007,
        8,
        9,
        10,
        11,
        12,
    ]);
    let set = tsx_fixture();
    let inline = set
        .split_once("?>\n")
        .map_or(set.as_str(), |(_, rest)| rest)
        .replace("<tileset ", "<tileset firstgid=\"1\" ");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <map version=\"1.10\" tiledversion=\"1.10.2\" orientation=\"orthogonal\" renderorder=\"right-down\" width=\"4\" height=\"3\" \
         tilewidth=\"8\" tileheight=\"8\" infinite=\"0\" backgroundcolor=\"#102030\" nextlayerid=\"4\" nextobjectid=\"3\">\n\
         <properties><property name=\"gravity\" type=\"float\" value=\"9.8\"/></properties>\n{inline}\
         <layer id=\"1\" name=\"ground\" width=\"4\" height=\"3\"><data encoding=\"csv\">\n1,2,3,4,\n5,6,7,8,\n9,10,11,12\n</data></layer>\n\
         <layer id=\"2\" name=\"flips\" width=\"4\" height=\"3\"><data encoding=\"base64\">{flipped}</data></layer>\n\
         <objectgroup id=\"3\" name=\"things\"><object id=\"1\" name=\"start\" x=\"4\" y=\"4\"><point/></object>\
         <object id=\"2\" name=\"zone\" x=\"0\" y=\"0\" width=\"8\" height=\"8\"/></objectgroup>\n</map>\n"
    )
}

/// The same map as `.tmj`, with an inline tileset.
pub fn tmj_fixture() -> String {
    let mut set: serde_json::Value = serde_json::from_str(&tsj_fixture()).unwrap_or(serde_json::Value::Null);
    if let Some(object) = set.as_object_mut() {
        object.insert("firstgid".into(), 1.into());
    }
    serde_json::json!({
        "type": "map", "version": "1.10", "orientation": "orthogonal", "renderorder": "right-down",
        "width": 4, "height": 3, "tilewidth": 8, "tileheight": 8, "infinite": false,
        "nextlayerid": 4, "nextobjectid": 3, "backgroundcolor": "#102030",
        "properties": [{ "name": "gravity", "type": "float", "value": 9.8 }],
        "tilesets": [set],
        "layers": [
            { "id": 1, "name": "ground", "type": "tilelayer", "width": 4, "height": 3, "x": 0, "y": 0,
              "visible": true, "opacity": 1, "data": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] },
            { "id": 2, "name": "flips", "type": "tilelayer", "width": 4, "height": 3, "x": 0, "y": 0,
              "visible": true, "opacity": 0.5,
              "data": [1, 2_147_483_650u32, 1_073_741_827u32, 0, 5, 6, 2_684_354_567u32, 8, 9, 10, 11, 12] },
            { "id": 3, "name": "things", "type": "objectgroup", "x": 0, "y": 0, "visible": true, "opacity": 1,
              "objects": [
                { "id": 1, "name": "start", "x": 4, "y": 4, "point": true, "width": 0, "height": 0 },
                { "id": 2, "name": "zone", "x": 0, "y": 0, "width": 8, "height": 8 }
              ] }
        ]
    })
    .to_string()
}

/// A `.tmx` whose tileset is the external `set.tsx`.
fn tmx_with_external(source: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <map version=\"1.10\" orientation=\"orthogonal\" renderorder=\"right-down\" width=\"4\" height=\"3\" tilewidth=\"8\" \
         tileheight=\"8\" infinite=\"0\" nextlayerid=\"2\" nextobjectid=\"1\">\n\
         <tileset firstgid=\"1\" source=\"{source}\"/>\n\
         <layer id=\"1\" name=\"ground\" width=\"4\" height=\"3\"><data encoding=\"csv\">\n1,2,3,4,\n5,6,7,8,\n9,10,11,12\n</data></layer>\n</map>\n"
    )
}

/// What the Tiled importer does with the map at `map`: import, the `.tilemap.ron` round trip, the cook (tileset images
/// from the scratch directory) and the Wang rules of `rules_from`.
fn tiled_cook(map: &Path, rules_from: &Path) -> Result<Vec<u8>, String> {
    use regia_2d::tilemap::TileMapAsset;
    let imported = crate::tiled_import::import_tiled(map);
    let rules = crate::tiled_wang::import_wang_autotiles(rules_from);
    let imported = imported.map_err(|e| e.to_string())?;
    let ron = imported.map.to_ron().map_err(|e| e.to_string())?;
    TileMapAsset::from_ron(&ron).map_err(|e| format!("the .tilemap.ron does not read back: {e}"))?;
    let dir = map.parent().unwrap_or(Path::new("."));
    let _ = crate::tiled_import::cook_tilemap(imported.map, dir);
    for rule in rules.map_err(|e| e.to_string())? {
        rule.set.to_ron().map_err(|e| e.to_string())?;
    }
    Ok(Vec::new())
}

/// The whole import of a `.tmx` (its inline tilesets and Wang sets included).
pub fn tmx_load(text: &str) -> Result<String, String> {
    let dir = Scratch::new("tmx")?;
    dir.write("t.png", &png_fixture(32, 32))?;
    let map = dir.write("map.tmx", text.as_bytes())?;
    tiled_cook(&map, &map).map(|_| String::new())
}

/// The whole import of a `.tmj`.
pub fn tmj_load(text: &str) -> Result<String, String> {
    let dir = Scratch::new("tmj")?;
    dir.write("t.png", &png_fixture(32, 32))?;
    let map = dir.write("map.tmj", text.as_bytes())?;
    tiled_cook(&map, &map).map(|_| String::new())
}

/// The whole import of a `.tsx` through a map that uses it, and the Wang rules of the `.tsx` itself.
pub fn tsx_load(text: &str) -> Result<String, String> {
    let dir = Scratch::new("tsx")?;
    dir.write("t.png", &png_fixture(32, 32))?;
    let set = dir.write("set.tsx", text.as_bytes())?;
    let map = dir.write("map.tmx", tmx_with_external("set.tsx").as_bytes())?;
    tiled_cook(&map, &set).map(|_| String::new())
}

/// The whole import of a `.tsj` through a `.tmj` that uses it, and the Wang rules of the `.tsj` itself.
pub fn tsj_load(text: &str) -> Result<String, String> {
    let dir = Scratch::new("tsj")?;
    dir.write("t.png", &png_fixture(32, 32))?;
    let set = dir.write("set.tsj", text.as_bytes())?;
    let map = serde_json::json!({
        "type": "map", "version": "1.10", "orientation": "orthogonal", "renderorder": "right-down",
        "width": 4, "height": 3, "tilewidth": 8, "tileheight": 8, "infinite": false,
        "nextlayerid": 2, "nextobjectid": 1,
        "tilesets": [{ "firstgid": 1, "source": "set.tsj" }],
        "layers": [{ "id": 1, "name": "ground", "type": "tilelayer", "width": 4, "height": 3, "x": 0, "y": 0,
                     "visible": true, "opacity": 1, "data": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] }]
    });
    let map = dir.write("map.tmj", map.to_string().as_bytes())?;
    tiled_cook(&map, &set).map(|_| String::new())
}

// --- IES ------------------------------------------------------------------------------------------------------------

/// The valid LM-63 example profile.
pub fn ies_fixture() -> String {
    regia_asset::ies::example_text()
}

/// The cook of an `.ies` file: parse, resample, and the cooked payload must read back.
pub fn ies_load(text: &str) -> Result<String, String> {
    let cooked = crate::ies_cook::cook_ies_bytes(text.as_bytes()).map_err(|e| e.to_string())?;
    regia_asset::ies::CookedIesProfile::from_bytes(&cooked.to_bytes())
        .map_err(|e| format!("the cooked profile does not read back: {e}"))?;
    Ok(String::new())
}

// --- Audio ----------------------------------------------------------------------------------------------------------

/// A mono 16-bit 8 kHz WAV of 400 samples of a sine.
pub fn wav_fixture() -> Vec<u8> {
    let data: Vec<u8> = (0..400)
        .flat_map(|i| (((i as f32 * 0.3).sin() * 12_000.0) as i16).to_le_bytes())
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&8000u32.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// CRC-8 (polynomial 0x07) of a FLAC frame header.
fn flac_crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut crc, &byte| {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
        crc
    })
}

/// CRC-16 (polynomial 0x8005) of a whole FLAC frame.
fn flac_crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut crc, &byte| {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
        crc
    })
}

/// One mono 16-bit fixed-blocksize FLAC frame of 128 samples, `number` in the header; `samples` is verbatim, or
/// a single constant sample when it has one entry.
fn flac_frame(number: u8, samples: &[i16]) -> Vec<u8> {
    // Sync 0xFFF8 (fixed blocksize), blocksize code 0b0110 (8 bits, `n - 1`), rate from STREAMINFO, mono, 16 bits.
    let mut frame = vec![0xFF, 0xF8, 0x60, 0x08, number, 127];
    frame.push(flac_crc8(&frame));
    if let [constant] = samples {
        frame.push(0x00);
        frame.extend_from_slice(&constant.to_be_bytes());
    } else {
        frame.push(0x02);
        for sample in samples {
            frame.extend_from_slice(&sample.to_be_bytes());
        }
    }
    let crc = flac_crc16(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame
}

/// A mono 16-bit 8 kHz FLAC of two frames: 128 verbatim samples, then 128 constant ones.
pub fn flac_fixture() -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    // STREAMINFO, the last metadata block: block sizes 128, frame sizes unknown, 8000 Hz, 1 channel, 16 bits, 256 samples.
    out.extend_from_slice(&[0x80, 0, 0, 34, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0]);
    // 8000 Hz (20 bits) | channels - 1 (3) | bits - 1 (5) | samples (36).
    let packed: u64 = (8000u64 << 44) | (15 << 36) | 256;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    let verbatim: Vec<i16> = (0..128)
        .map(|i| ((i as f32 * 0.3).sin() * 12_000.0) as i16)
        .collect();
    out.extend(flac_frame(0, &verbatim));
    out.extend(flac_frame(1, &[1000]));
    out
}

/// A one-second 1 kHz Ogg Vorbis tone, mono, 48 kHz (5.5 KB; written once with an external encoder, the tree has none).
pub fn ogg_fixture() -> Vec<u8> {
    include_bytes!("../../tests/fixtures/audio/tone_1k.ogg").to_vec()
}

/// The audio cook's decode and loudness measurement of a sound file (WAV, FLAC, Ogg Vorbis, MP3).
pub fn audio_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    regia_audio::measure_sound_bytes(bytes.to_vec(), "fuzz")
        .map(|_| Vec::new())
        .map_err(|e| e.to_string())
}

// --- WebM -----------------------------------------------------------------------------------------------------------

/// A raw-video WebM of 3 frames, 16 x 16, with cues.
pub fn webm_fixture() -> Vec<u8> {
    regia_media::mux::PatternClip::new(16, 16, 3, 30).to_webm()
}

/// The video cook: container validation, then the cooked payload (the `.meta` and `.video.ron` sidecars of the real
/// cook are file writes only).
pub fn webm_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let report = regia_media::validate(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    regia_media::validate::encode_cooked_video(&report, bytes)
        .map(|_| Vec::new())
        .map_err(|e| e.to_string())
}

// --- Heightmaps -----------------------------------------------------------------------------------------------------

/// A raw 16-bit little-endian 8 x 8 heightmap (a RAW file without a size is a square from its byte count).
pub fn r16_fixture() -> Vec<u8> {
    (0..64u16).flat_map(|i| (i * 1000).to_le_bytes()).collect()
}

/// The terrain data cook of a raw heightmap in both byte orders: the square side from the length, the 16-bit grid, and
/// the tiling into 4-sample and 256-sample tiles with their min and max.
pub fn r16_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use regia_terrain::{RawEndian, decode_r16, square_side};
    let side = square_side(bytes.len())
        .ok_or_else(|| format!("{} bytes is not a square 16-bit grid", bytes.len()))?;
    for endian in [RawEndian::Little, RawEndian::Big] {
        let grid = decode_r16(bytes, side, side, endian).map_err(|e| e.to_string())?;
        for tile in [4, 256] {
            let tiled =
                crate::terrain_data::cook_tiled_heightfield(&grid, tile).map_err(|e| e.to_string())?;
            tiled.to_postcard().map_err(|e| e.to_string())?;
        }
    }
    Ok(Vec::new())
}

// --- Sprite atlas ---------------------------------------------------------------------------------------------------

/// An `.atlas.ron` of a whole sprite, a nine-slice sprite and a grid of cells.
pub fn atlas_fixture() -> String {
    "(version: 1, max_size: 64, padding: 1, extrude: 1, trim: true, sprites: [\
     (name: \"a\", image: \"a.png\", pivot: (0.5, 0.0)), \
     (name: \"panel\", image: \"b.png\", nine_slice: Some((4, 4, 4, 4))), \
     (name: \"cell\", image: \"b.png\", grid: Some((8, 8)))])"
        .to_string()
}

/// The atlas cook: parse, load the sprite images from the scratch directory (a PNG named `a.png` and `b.png`), pack.
pub fn atlas_load(text: &str) -> Result<String, String> {
    let dir = Scratch::new("atlas")?;
    dir.write("a.png", &png_fixture(12, 10))?;
    dir.write("b.png", &png_fixture(16, 16))?;
    crate::atlas_cook::cook_atlas(text, dir.path())
        .map(|_| String::new())
        .map_err(|e| e.to_string())
}

// --- FBX ------------------------------------------------------------------------------------------------------------

/// A valid ASCII FBX 7.4: one mesh of a quad (two polygons' worth of control points, one triangle and one quad), normals,
/// a UV set, a material and a Z-up meter-scaled global setting.
pub fn fbx_fixture() -> String {
    "; FBX 7.4.0 project file\n\
     FBXHeaderExtension:  {\n\tFBXHeaderVersion: 1003\n\tFBXVersion: 7400\n\tCreator: \"Regia fuzz fixture\"\n}\n\
     GlobalSettings:  {\n\tVersion: 1000\n\tProperties70:  {\n\
     \t\tP: \"UpAxis\", \"int\", \"Integer\", \"\",1\n\
     \t\tP: \"UpAxisSign\", \"int\", \"Integer\", \"\",1\n\
     \t\tP: \"FrontAxis\", \"int\", \"Integer\", \"\",2\n\
     \t\tP: \"FrontAxisSign\", \"int\", \"Integer\", \"\",1\n\
     \t\tP: \"CoordAxis\", \"int\", \"Integer\", \"\",0\n\
     \t\tP: \"CoordAxisSign\", \"int\", \"Integer\", \"\",1\n\
     \t\tP: \"UnitScaleFactor\", \"double\", \"Number\", \"\",100\n\t}\n}\n\
     Objects:  {\n\
     \tGeometry: 1, \"Geometry::quad\", \"Mesh\" {\n\
     \t\tVertices: *15 {\n\t\t\ta: 0,0,0,1,0,0,1,1,0,0,1,0,2,0,0\n\t\t}\n\
     \t\tPolygonVertexIndex: *7 {\n\t\t\ta: 0,1,2,-4,1,4,-3\n\t\t}\n\
     \t\tGeometryVersion: 124\n\
     \t\tLayerElementNormal: 0 {\n\t\t\tVersion: 102\n\t\t\tName: \"\"\n\t\t\tMappingInformationType: \"ByPolygonVertex\"\n\
     \t\t\tReferenceInformationType: \"Direct\"\n\t\t\tNormals: *21 {\n\t\t\t\ta: 0,0,1,0,0,1,0,0,1,0,0,1,0,0,1,0,0,1,0,0,1\n\t\t\t}\n\t\t}\n\
     \t\tLayerElementUV: 0 {\n\t\t\tVersion: 101\n\t\t\tName: \"uv\"\n\t\t\tMappingInformationType: \"ByPolygonVertex\"\n\
     \t\t\tReferenceInformationType: \"IndexToDirect\"\n\t\t\tUV: *10 {\n\t\t\t\ta: 0,0,1,0,1,1,0,1,0.5,0.5\n\t\t\t}\n\
     \t\t\tUVIndex: *7 {\n\t\t\t\ta: 0,1,2,3,1,4,2\n\t\t\t}\n\t\t}\n\
     \t\tLayerElementMaterial: 0 {\n\t\t\tVersion: 101\n\t\t\tName: \"\"\n\t\t\tMappingInformationType: \"AllSame\"\n\
     \t\t\tReferenceInformationType: \"IndexToDirect\"\n\t\t\tMaterials: *1 {\n\t\t\t\ta: 0\n\t\t\t}\n\t\t}\n\
     \t\tLayer: 0 {\n\t\t\tVersion: 100\n\
     \t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementNormal\"\n\t\t\t\tTypedIndex: 0\n\t\t\t}\n\
     \t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementUV\"\n\t\t\t\tTypedIndex: 0\n\t\t\t}\n\
     \t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementMaterial\"\n\t\t\t\tTypedIndex: 0\n\t\t\t}\n\t\t}\n\t}\n\
     \tModel: 2, \"Model::quad\", \"Mesh\" {\n\t\tVersion: 232\n\t\tProperties70:  {\n\
     \t\t\tP: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",1,2,3\n\
     \t\t\tP: \"Lcl Rotation\", \"Lcl Rotation\", \"\", \"A\",0,90,0\n\
     \t\t\tP: \"Lcl Scaling\", \"Lcl Scaling\", \"\", \"A\",1,1,1\n\t\t}\n\t\tShading: Y\n\t\tCulling: \"CullingOff\"\n\t}\n\
     \tMaterial: 3, \"Material::red\", \"\" {\n\t\tVersion: 102\n\t\tShadingModel: \"lambert\"\n\t\tMultiLayer: 0\n\
     \t\tProperties70:  {\n\t\t\tP: \"DiffuseColor\", \"Color\", \"\", \"A\",1,0,0\n\t\t}\n\t}\n\
     }\n\
     Connections:  {\n\tC: \"OO\",1,2\n\tC: \"OO\",3,2\n\tC: \"OO\",2,0\n}\n"
        .to_string()
}

/// The whole FBX import: pre-scan, the C parser's scene copied into Regia's, the GLB it writes, and the glTF import of
/// that GLB. The wrapper is fuzzed here; `ufbx` itself is C, so an AddressSanitizer run (`cargo xtask fuzz run
/// --target import.fbx`) is what covers the parser's own memory safety.
pub fn fbx_load(text: &str) -> Result<String, String> {
    crate::fbx_import::import_fbx_bytes(
        text.as_bytes(),
        AssetId::sub_asset(AssetId::default(), "fuzz"),
        Forward::Convert,
        "desktop",
        TextureCookQuality::High,
        &crate::fbx_import::FbxImportSettings::default(),
    )
    .map(|_| String::new())
    .map_err(|e| e.to_string())
}
