// SPDX-License-Identifier: MPL-2.0
//! Heightmap export/import and the shared wind on the agent protocol
//! (`docs/specs/82-terrain-editing.md` § 5, ADR-1853; `book/agents/terrain-editing.md`,
//! `book/agents/terrain-wind.md`): `terrain.export_heights` writes a terrain's 16-bit grid to a
//! PNG or RAW file under the project root, `terrain.import_heights` reads one back onto the
//! terrain as one undoable edit, `terrain.wind` / `terrain.set_wind` read and set the
//! [`TerrainWind`] the grass sways in. `terrain.save_paint` / `terrain.load_paint` write and read
//! the painted layer weights.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::Heightfield;
use regia_terrain::{
    HeightIoError, LayerWeightMap, RawEndian, Terrain, TerrainEdits, TerrainGrassHandle, TerrainPaint,
    TerrainStrokeLog, TerrainWind, decode_r16, encode_r16, export_heights, import_heights,
    layer_weights_path, square_side,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Largest heightmap side read or written (16 Ki samples).
const MAX_SIDE: u32 = 16_384;
/// Largest file read: a RAW of `MAX_SIDE`² samples (a PNG of that grid is smaller or close).
const MAX_FILE_BYTES: u64 = (MAX_SIDE as u64) * (MAX_SIDE as u64) * 2 + (1 << 20);

/// Largest `.weights.bin` read by `terrain.load_paint`: the largest weight map `LayerWeightMap`
/// accepts (2^30 one-byte weights) plus 1 MiB for its header, so whatever `terrain.save_paint`
/// wrote loads back (ADR-5815). The heightmap cap (`MAX_FILE_BYTES`) is about half that.
const fn paint_read_cap() -> u64 {
    (1 << 30) + (1 << 20)
}

const HINT: &str = "see book/agents/terrain-editing.md";

fn coded(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, reason, message, HINT)
}

fn io_error(e: &HeightIoError) -> RpcError {
    coded(protocol::INVALID_PARAMS, e.code(), e.to_string())
}

/// File format of a heightmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Png,
    R16,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::R16 => "r16",
        }
    }
}

fn format_of(params: &Value, path: &str) -> Result<Format, RpcError> {
    let named = crate::params::opt_str(params, "format")?;
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match (named, ext.as_deref()) {
        (Some("png"), _) | (None, Some("png")) => Ok(Format::Png),
        (Some("r16" | "raw"), _) | (None, Some("r16" | "raw")) => Ok(Format::R16),
        (Some(other), _) => Err(coded(
            protocol::INVALID_PARAMS,
            "height_io_bad_format",
            format!("unknown format `{other}`: png | r16"),
        )),
        (None, _) => Err(coded(
            protocol::INVALID_PARAMS,
            "height_io_bad_format",
            format!(
                "cannot tell the format of `{path}`: pass `format` (png | r16) or use a .png / .r16 name"
            ),
        )),
    }
}

fn endian_of(params: &Value) -> Result<RawEndian, RpcError> {
    match crate::params::opt_str(params, "endian")? {
        None | Some("little") => Ok(RawEndian::Little),
        Some("big") => Ok(RawEndian::Big),
        Some(other) => Err(coded(
            protocol::INVALID_PARAMS,
            "height_io_bad_format",
            format!("unknown endian `{other}`: little | big"),
        )),
    }
}

/// `path` (relative, no `..`) joined to the project root of the world's `AssetServer`.
pub(crate) fn project_file(world: &World, path: &str) -> Result<PathBuf, RpcError> {
    let Ok(relative) = regia_core::project_path::check_relative(path) else {
        return Err(RpcError::invalid_params(
            "`path` must be relative to the project root, without `..`",
        ));
    };
    let root = world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .ok_or_else(|| {
            RpcError::not_found(
                "no AssetServer resource: no project root to read or write under",
                "run the method in an app with a project (the editor or a game)",
            )
        })?;
    let file = root.join(relative);
    // A symlink under the root may point outside it: check the deepest part that exists (the
    // file itself, or the directory an export would create the rest under).
    if let Ok(root) = root.canonicalize()
        && let Some(real) = file.ancestors().find_map(|p| p.canonicalize().ok())
        && !real.starts_with(&root)
    {
        return Err(RpcError::invalid_params(format!(
            "{path} resolves outside the project root"
        )));
    }
    Ok(file)
}

pub(crate) fn target_terrain(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    if let Some(bits) = crate::params::opt_u64(params, "entity")? {
        let entity = Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
        if world.get::<Terrain>(entity).is_none() {
            return Err(RpcError::not_found(
                format!("entity {bits} has no Terrain"),
                "omit `entity` to use the lowest-entity-id terrain",
            ));
        }
        return Ok(entity);
    }
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    query.iter(world).min_by_key(|e| e.to_bits()).ok_or_else(|| {
        RpcError::not_found(
            "no Terrain in this world",
            "spawn an entity with regia_terrain::Terrain (the level's heightfield)",
        )
    })
}

/// A 16-bit grayscale PNG of `hf` (big-endian samples, PNG § 7.1).
fn encode_png16(hf: &Heightfield) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, hf.width, hf.height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&encode_r16(hf, RawEndian::Big))
            .map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(bytes)
}

/// Decodes a 16-bit grayscale PNG; other color types and depths are refused.
fn decode_png16(bytes: &[u8]) -> Result<Heightfield, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w < 2 || h < 2 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("the png is {w}x{h}; each side must be 2..={MAX_SIDE}"));
    }
    let (color, depth) = (reader.info().color_type, reader.info().bit_depth);
    if color != png::ColorType::Grayscale || depth != png::BitDepth::Sixteen {
        return Err(format!(
            "the png is {color:?} at {depth:?} bits; a heightmap is 16-bit grayscale"
        ));
    }
    let size = reader.output_buffer_size().ok_or("png: no output buffer")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let data = buf.get(..info.buffer_size()).ok_or("png: short frame")?;
    decode_r16(data, w, h, RawEndian::Big).map_err(|e| e.to_string())
}

fn write_file(file: &Path, bytes: &[u8]) -> Result<(), RpcError> {
    crate::bake_export::write_atomic(file, bytes).map_err(|e| {
        coded(
            protocol::INTERNAL_ERROR,
            "height_io_write",
            format!("{}: {e}", file.display()),
        )
    })
}

/// `terrain.export_heights`: writes the terrain's 16-bit grid (`entity`, or the lowest-entity-id terrain)
/// to `path` under the project root as a 16-bit grayscale PNG or a headerless RAW file.
pub fn export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (relative to the project root) is required"))?;
    let format = format_of(params, path)?;
    let endian = endian_of(params)?;
    let file = project_file(world, path)?;
    let entity = target_terrain(world, params)?;
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| RpcError::internal("terrain vanished mid-call"))?;
    let out = export_heights(&terrain.map);
    let bytes = match format {
        Format::Png => encode_png16(&out.heightfield).map_err(|e| {
            coded(
                protocol::INTERNAL_ERROR,
                "height_io_write",
                format!("png encode: {e}"),
            )
        })?,
        Format::R16 => encode_r16(&out.heightfield, endian),
    };
    write_file(&file, &bytes)?;
    Ok(json!({
        "entity": entity.to_bits(),
        "path": path,
        "format": format.name(),
        "width": out.heightfield.width,
        "height": out.heightfield.height,
        "bytes": bytes.len(),
        "clamped_low": out.clamped_low,
        "clamped_high": out.clamped_high,
    }))
}

/// `terrain.import_heights`: reads a 16-bit PNG or RAW file under the project root onto the
/// terrain's heights. The grid must match the terrain's; the change is one undo step in the
/// editor (logged like a stroke).
pub fn import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (relative to the project root) is required"))?;
    let format = format_of(params, path)?;
    let endian = endian_of(params)?;
    let file = project_file(world, path)?;
    let entity = target_terrain(world, params)?;
    let read_error = |e: std::io::Error| coded(protocol::NOT_FOUND, "height_io_read", format!("{path}: {e}"));
    let size = std::fs::metadata(&file).map_err(read_error)?.len();
    if size > MAX_FILE_BYTES {
        return Err(coded(
            protocol::INVALID_PARAMS,
            "height_io_decode",
            format!("{path} is {size} bytes; the largest heightmap read is {MAX_FILE_BYTES}"),
        ));
    }
    let bytes = std::fs::read(&file).map_err(read_error)?;
    let hf = match format {
        Format::Png => {
            decode_png16(&bytes).map_err(|e| coded(protocol::INVALID_PARAMS, "height_io_decode", e))?
        }
        Format::R16 => {
            let side = |key: &str| -> Result<Option<u32>, RpcError> {
                Ok(crate::params::opt_u64(params, key)?.map(|v| v.min(u64::from(MAX_SIDE)) as u32))
            };
            let (w, h) = match (side("width")?, side("height")?) {
                (Some(w), Some(h)) => (w, h),
                (None, None) => {
                    let n = square_side(bytes.len()).ok_or_else(|| {
                        coded(
                            protocol::INVALID_PARAMS,
                            "height_io_decode",
                            format!(
                                "{path} holds {} bytes, not a square grid of 16-bit samples: pass `width` and `height`",
                                bytes.len()
                            ),
                        )
                    })?;
                    (n, n)
                }
                _ => {
                    return Err(RpcError::invalid_params(
                        "pass both `width` and `height`, or neither",
                    ));
                }
            };
            decode_r16(&bytes, w, h, endian).map_err(|e| io_error(&e))?
        }
    };
    if !world.entity(entity).contains::<TerrainEdits>() {
        world.entity_mut(entity).insert(TerrainEdits::default());
    }
    let mut query = world.query::<(&mut Terrain, &mut TerrainEdits)>();
    let Ok((mut terrain, mut edits)) = query.get_mut(world, entity) else {
        return Err(RpcError::internal("terrain components vanished mid-call"));
    };
    let out = import_heights(&mut terrain, &mut edits, &hf).map_err(|e| io_error(&e))?;
    let undoable = out.undo.is_some();
    if let Some(undo) = out.undo {
        let (undo, layer) = regia_terrain::absorb_world_stroke(world, entity, Some(None), undo)
            .map_err(|e| crate::terrain_edit_layers::layer_error(&e))?;
        if let Some(layer) = layer {
            world
                .get_resource_or_insert_with::<regia_terrain::TerrainEditLayerLog>(Default::default)
                .push(regia_terrain::EditLayerLogEntry::Stroke {
                    entity,
                    stroke: undo,
                    layer,
                });
        } else {
            // Under an editor every import becomes an undo step, however many arrive in one frame.
            world
                .get_resource_or_insert_with::<TerrainStrokeLog>(Default::default)
                .push(entity, undo);
        }
    } else if out.changed > 0 {
        regia_terrain::edit_layers::reconcile_world_base(world, entity)
            .map_err(|e| crate::terrain_edit_layers::layer_error(&e))?;
    }
    let d = out.dirty;
    Ok(json!({
        "entity": entity.to_bits(),
        "path": path,
        "format": format.name(),
        "width": hf.width,
        "height": hf.height,
        "changed": out.changed,
        "undoable": undoable,
        "dirty": { "col_start": d.col_start, "col_end": d.col_end,
                   "row_start": d.row_start, "row_end": d.row_end },
    }))
}

/// The weights file of `path`: a name ending in `.weights.bin` as it is, a map or level file's
/// [`layer_weights_path`].
fn weights_name(params: &Value) -> Result<String, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (relative to the project root) is required"))?;
    Ok(if path.to_ascii_lowercase().ends_with(".weights.bin") {
        path.to_owned()
    } else {
        layer_weights_path(Path::new(path))
            .to_string_lossy()
            .replace('\\', "/")
    })
}

/// `terrain.save_paint`: writes the terrain's painted weights ([`LayerWeightMap::to_bytes`]) to
/// the `<map>.weights.bin` of `path` under the project root, atomically.
pub fn save_paint(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = weights_name(params)?;
    let file = project_file(world, &name)?;
    let entity = target_terrain(world, params)?;
    let Some(paint) = world.get::<TerrainPaint>(entity) else {
        return Err(RpcError::not_found(
            format!("entity {} has no TerrainPaint", entity.to_bits()),
            "paint a layer first, or insert regia_terrain::TerrainPaint",
        ));
    };
    let (width, height, layers) = (paint.0.width(), paint.0.height(), paint.0.layers());
    let bytes = paint.0.to_bytes().map_err(|e| {
        coded(
            protocol::INTERNAL_ERROR,
            "height_io_write",
            format!("weights encode: {e}"),
        )
    })?;
    write_file(&file, &bytes)?;
    if let Some(mut edits) = world.get_mut::<TerrainEdits>(entity) {
        edits.take_unsaved_paint();
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "path": name,
        "width": width,
        "height": height,
        "layers": layers,
        "bytes": bytes.len(),
    }))
}

/// `terrain.load_paint`: reads a `<map>.weights.bin` under the project root into the terrain's
/// [`TerrainPaint`] and marks the whole map for a GPU upload. The grid must match the terrain's.
pub fn load_paint(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = weights_name(params)?;
    let file = project_file(world, &name)?;
    let entity = target_terrain(world, params)?;
    let read_error = |e: std::io::Error| coded(protocol::NOT_FOUND, "height_io_read", format!("{name}: {e}"));
    let size = std::fs::metadata(&file).map_err(read_error)?.len();
    if size > paint_read_cap() {
        return Err(coded(
            protocol::INVALID_PARAMS,
            "height_io_decode",
            format!(
                "{name} is {size} bytes; the largest file read is {}",
                paint_read_cap()
            ),
        ));
    }
    let bytes = std::fs::read(&file).map_err(read_error)?;
    let map = LayerWeightMap::from_bytes(&bytes).map_err(|e| {
        coded(
            protocol::INVALID_PARAMS,
            "height_io_decode",
            format!("{name}: {e}"),
        )
    })?;
    let (width, height, layers) = (map.width(), map.height(), map.layers());
    let grid = world
        .get::<Terrain>(entity)
        .map(|t| (t.map.width(), t.map.height()))
        .ok_or_else(|| RpcError::internal("terrain vanished mid-call"))?;
    if (width, height) != grid {
        return Err(io_error(&HeightIoError::SizeMismatch {
            file: [width, height],
            map: [grid.0, grid.1],
        }));
    }
    let mut target = world.entity_mut(entity);
    if !target.contains::<TerrainEdits>() {
        target.insert(TerrainEdits::default());
    }
    target.insert(TerrainPaint(map));
    if let Some(mut edits) = target.get_mut::<TerrainEdits>() {
        edits.note_paint(regia_terrain::DirtyRect {
            col_start: 0,
            col_end: width,
            row_start: 0,
            row_end: height,
        });
        // What was just read is what the file holds.
        edits.take_unsaved_paint();
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "path": name,
        "width": width,
        "height": height,
        "layers": layers,
    }))
}

fn wind_json(wind: &TerrainWind) -> Value {
    json!({
        "direction": wind.direction,
        "unit_direction": wind.unit_direction(),
        "strength": wind.strength,
        "frequency_scale": wind.frequency_scale,
        "gust": wind.gust,
        "style": wind.style.name(),
    })
}

/// `terrain.wind`: the [`TerrainWind`] in effect (the default when none was set).
pub fn wind(world: &mut World) -> Result<Value, RpcError> {
    let wind = world.get_resource::<TerrainWind>().copied().unwrap_or_default();
    let mut out = wind_json(&wind);
    out["grass_linked"] = json!(
        world
            .get_resource::<TerrainGrassHandle>()
            .is_some_and(TerrainGrassHandle::is_linked)
    );
    // Spec 99 § 3.3: a scene wind that is not `Off` steers this wind by its mean.
    out["weather_steered"] = json!(
        world
            .get_resource::<regia_scene::weather::SceneWind>()
            .is_some_and(|w| w.mode != regia_scene::weather::SceneWindMode::Off)
    );
    Ok(out)
}

/// `terrain.set_wind`: changes any of `direction`, `strength`, `frequency_scale`, `gust`, `style`
/// (clamped), stores the [`TerrainWind`] resource and pushes it into the grass handle at once.
pub fn set_wind(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut wind = world.get_resource::<TerrainWind>().copied().unwrap_or_default();
    if let Some(d) = params.get("direction") {
        let pair: Option<[f32; 2]> = serde_json::from_value(d.clone()).ok();
        let Some(direction) = pair.filter(|d| TerrainWind::usable_direction(*d)) else {
            return Err(coded(
                protocol::INVALID_PARAMS,
                "wind_bad_direction",
                "`direction` must be [x, z] of length 1e-6..=1e6",
            ));
        };
        wind.direction = direction;
    }
    let field = |key: &str, slot: &mut f32| -> Result<(), RpcError> {
        if let Some(v) = params.get(key) {
            let v = v
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number")))?;
            *slot = v as f32;
        }
        Ok(())
    };
    if let Some(v) = params.get("style") {
        let Some(style) = v.as_str().and_then(regia_terrain::WindStyle::from_name) else {
            return Err(coded(
                protocol::INVALID_PARAMS,
                "wind_bad_style",
                "`style` must be \"classic\" or \"natural\"",
            ));
        };
        wind.style = style;
    }
    field("strength", &mut wind.strength)?;
    field("frequency_scale", &mut wind.frequency_scale)?;
    field("gust", &mut wind.gust)?;
    let wind = wind.clamped();
    world.insert_resource(wind);
    if let Some(grass) = world.get_resource::<TerrainGrassHandle>() {
        grass.set_wind(wind);
    }
    Ok(wind_json(&wind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{CdlodConfig, MinMaxPyramid, TerrainLayers, TerrainMap};

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn world_with_terrain(root: &Path, n: u32) -> (World, Entity) {
        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        world.insert_resource(server);
        let heights = (0..n * n)
            .map(|i| ((i % n) as f32 * 0.37).sin() * 4.0 + 10.0)
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, 64.0, 100.0, -20.0).expect("map");
        // Quantise as a load would, so export → import is the identity.
        let map = TerrainMap::new(&map.to_heightfield(), 64.0, 100.0, -20.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 2).expect("pyramid");
        let entity = world
            .spawn(Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level: 2,
                minmax,
                layers: TerrainLayers::default(),
                holes: None,
                water: None,
            })
            .id();
        (world, entity)
    }

    fn heights(world: &World, e: Entity) -> Vec<u32> {
        world
            .get::<Terrain>(e)
            .map(|t| t.map.heights().iter().map(|h| h.to_bits()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn png_and_r16_round_trip_through_files() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_heights");
        let (mut world, e) = world_with_terrain(&root, 33);
        let before = heights(&world, e);
        for (path, extra) in [
            ("maps/a.png", json!({})),
            ("maps/a.r16", json!({})),
            ("maps/b.raw", json!({ "endian": "big" })),
        ] {
            let mut params = json!({ "path": path });
            for (k, v) in extra.as_object().into_iter().flatten() {
                params[k] = v.clone();
            }
            let out = export(&mut world, &params).expect("export");
            assert_eq!(out["width"], 33);
            assert_eq!(out["clamped_low"], 0);
            assert!(root.join(path).is_file());
            let back = import(&mut world, &params).expect("import");
            assert_eq!(back["changed"], 0, "{path}: {back}");
            assert_eq!(heights(&world, e), before);
        }
        assert_eq!(
            std::fs::read(root.join("maps/a.r16"))
                .map(|b| b.len())
                .unwrap_or(0),
            33 * 33 * 2
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_import_changes_the_terrain_and_is_logged_for_undo() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_heights_import");
        let (mut world, e) = world_with_terrain(&root, 17);
        let flat = Heightfield {
            width: 17,
            height: 17,
            samples: vec![32768; 17 * 17],
        };
        std::fs::create_dir_all(root.join("src")).expect("dir");
        std::fs::write(root.join("src/flat.r16"), encode_r16(&flat, RawEndian::Little)).expect("write");
        let out = import(&mut world, &json!({ "path": "src/flat.r16" })).expect("import");
        assert_eq!(out["undoable"], true);
        assert!(out["changed"].as_u64().unwrap_or(0) > 200);
        let t = world.get::<Terrain>(e).expect("terrain");
        assert_eq!(t.map.to_heightfield(), flat);
        assert_eq!(world.resource::<TerrainStrokeLog>().strokes.len(), 1);
        assert!(world.get::<TerrainEdits>(e).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// deep-zai-terrain-stream #10: under an editor (`drained`) an import drops no older entry.
    #[test]
    fn an_import_under_an_editor_drops_no_logged_stroke() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_heights_drained");
        let (mut world, _) = world_with_terrain(&root, 17);
        std::fs::create_dir_all(root.join("src")).expect("dir");
        for (name, level) in [("a", 30000u16), ("b", 34000)] {
            let flat = Heightfield {
                width: 17,
                height: 17,
                samples: vec![level; 17 * 17],
            };
            std::fs::write(
                root.join(format!("src/{name}.r16")),
                encode_r16(&flat, RawEndian::Little),
            )
            .expect("write");
        }
        import(&mut world, &json!({ "path": "src/a.r16" })).expect("import");
        let entry = world.resource::<TerrainStrokeLog>().strokes[0].clone();
        {
            let mut log = world.resource_mut::<TerrainStrokeLog>();
            log.drained = true;
            log.strokes = vec![entry; 300];
        }
        import(&mut world, &json!({ "path": "src/b.r16" })).expect("import");
        assert_eq!(world.resource::<TerrainStrokeLog>().strokes.len(), 301);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_requests_get_their_codes() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_heights_err");
        let (mut world, _) = world_with_terrain(&root, 17);
        let err = |w: &mut World, p: Value| reason(&import(w, &p).expect_err("refused"));
        assert_eq!(
            err(&mut world, json!({ "path": "x.tif" })),
            "height_io_bad_format"
        );
        assert_eq!(
            err(&mut world, json!({ "path": "x.png", "format": "exr" })),
            "height_io_bad_format"
        );
        assert_eq!(
            err(&mut world, json!({ "path": "x.r16", "endian": "middle" })),
            "height_io_bad_format"
        );
        assert_eq!(
            err(&mut world, json!({ "path": "missing.r16" })),
            "height_io_read"
        );
        assert!(import(&mut world, &json!({ "path": "../up.r16" })).is_err());
        assert!(export(&mut world, &json!({ "path": "/abs.png" })).is_err());
        let small = Heightfield {
            width: 9,
            height: 9,
            samples: vec![0; 81],
        };
        std::fs::write(root.join("small.r16"), encode_r16(&small, RawEndian::Little)).expect("write");
        assert_eq!(
            err(&mut world, json!({ "path": "small.r16" })),
            "height_io_size_mismatch"
        );
        std::fs::write(root.join("odd.r16"), [0u8; 7]).expect("write");
        assert_eq!(err(&mut world, json!({ "path": "odd.r16" })), "height_io_decode");
        let encoded = encode_png16(&small).expect("png");
        std::fs::write(root.join("small.png"), encoded).expect("write");
        assert_eq!(
            err(&mut world, json!({ "path": "small.png" })),
            "height_io_size_mismatch"
        );
        std::fs::write(root.join("junk.png"), b"not a png").expect("write");
        assert_eq!(err(&mut world, json!({ "path": "junk.png" })), "height_io_decode");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Spec 82 § 5: a failed write is `height_io_write` for an export and for `save_paint`, and
    /// `save_paint` without painted weights is `F0003`; a blocked write leaves no file behind.
    #[test]
    fn failed_writes_and_missing_paint_get_their_codes() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_heights_write");
        let (mut world, e) = world_with_terrain(&root, 17);
        // A file where a directory has to be.
        std::fs::write(root.join("blocker"), b"file").expect("write");
        for format in ["png", "r16"] {
            let err = export(&mut world, &json!({ "path": format!("blocker/out.{format}") }))
                .expect_err("blocked export");
            assert_eq!(reason(&err), "height_io_write", "{format}: {err:?}");
        }
        let err = save_paint(&mut world, &json!({ "path": "maps/arena.level.ron" })).expect_err("no paint");
        assert_eq!(reason(&err), "F0003");
        let map = LayerWeightMap::new(17, 17, 2).expect("map");
        world.entity_mut(e).insert(TerrainPaint(map));
        let err =
            save_paint(&mut world, &json!({ "path": "blocker/arena.level.ron" })).expect_err("blocked paint");
        assert_eq!(reason(&err), "height_io_write", "{err:?}");
        assert!(root.join("blocker").is_file(), "the blocking file is untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What `terrain.save_paint` can write, `terrain.load_paint` reads back: the read cap of a
    /// weights file covers the largest weight map (1 GiB of weights plus its header), not only the
    /// heightmap cap. Past the weights cap the file is still refused before it is read.
    #[test]
    fn the_paint_read_cap_covers_the_largest_weight_map() {
        // `LayerWeightMap` refuses more than 2^30 weights; its postcard header is a few bytes.
        let largest_weights: u64 = 1 << 30;
        assert!(
            paint_read_cap() >= largest_weights + 64,
            "a saved weights file of {largest_weights} weights must load: the cap is {}",
            paint_read_cap()
        );
        let root = regia_test_utils::unique_temp_dir("agent_terrain_paint_big");
        let (mut world, _) = world_with_terrain(&root, 17);
        let file = std::fs::File::create(root.join("huge.weights.bin")).expect("create");
        file.set_len(paint_read_cap() + 1).expect("sparse file");
        let refused = load_paint(&mut world, &json!({ "path": "huge.weights.bin" })).expect_err("too big");
        assert_eq!(reason(&refused), "height_io_decode");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn painted_weights_save_and_load_back() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_paint");
        let (mut world, e) = world_with_terrain(&root, 17);
        let args = json!({ "path": "maps/arena.level.ron" });
        assert!(save_paint(&mut world, &args).is_err(), "no TerrainPaint to write");

        let mut map = LayerWeightMap::new(17, 17, 3).expect("map");
        map.set(2, 4, 5, 200);
        let mut edits = TerrainEdits::default();
        edits.note_paint(regia_terrain::DirtyRect {
            col_start: 4,
            col_end: 5,
            row_start: 5,
            row_end: 6,
        });
        world.entity_mut(e).insert((TerrainPaint(map.clone()), edits));
        let out = save_paint(&mut world, &args).expect("save");
        assert_eq!(out["path"], json!("maps/arena.weights.bin"), "{out}");
        assert_eq!(
            (out["width"].as_u64(), out["layers"].as_u64()),
            (Some(17), Some(3))
        );
        let saved = world.get::<TerrainEdits>(e).expect("edits");
        assert!(!saved.unsaved_paint(), "a save clears the unsaved mark");

        // Into a fresh terrain: the weights arrive, the GPU is told, nothing reads as unsaved.
        let (mut fresh, fe) = world_with_terrain(&root, 17);
        let out = load_paint(&mut fresh, &json!({ "path": "maps/arena.weights.bin" })).expect("load");
        assert_eq!(out["layers"], json!(3), "{out}");
        assert_eq!(fresh.get::<TerrainPaint>(fe).map(|p| &p.0), Some(&map));
        let mut edits = fresh.get_mut::<TerrainEdits>(fe).expect("edits");
        assert!(edits.take_paint_dirty().is_some());
        assert!(!edits.unsaved_paint());

        // Another grid, a missing file and a broken file get their codes.
        let (mut small, _) = world_with_terrain(&root, 9);
        let err = load_paint(&mut small, &args).expect_err("grid");
        assert_eq!(reason(&err), "height_io_size_mismatch");
        let err = load_paint(&mut small, &json!({ "path": "maps/none.level.ron" })).expect_err("missing");
        assert_eq!(reason(&err), "height_io_read");
        std::fs::write(root.join("maps/junk.weights.bin"), b"junk").expect("write");
        let err = load_paint(&mut small, &json!({ "path": "maps/junk.weights.bin" })).expect_err("junk");
        assert_eq!(reason(&err), "height_io_decode");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_past_the_read_limit_is_refused_before_it_is_read() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_heights_big");
        let (mut world, _) = world_with_terrain(&root, 17);
        let file = std::fs::File::create(root.join("huge.r16")).expect("create");
        file.set_len(MAX_FILE_BYTES + 1).expect("sparse file");
        let refused = import(&mut world, &json!({ "path": "huge.r16" })).expect_err("too big");
        assert_eq!(reason(&refused), "height_io_decode");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink under the project root may not lead an export or an import out of it, also
    /// through directories the export would still have to create.
    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_leave_the_project_root() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_heights_link");
        let outside = regia_test_utils::unique_temp_dir("agent_terrain_heights_outside");
        let (mut world, _) = world_with_terrain(&root, 17);
        std::fs::write(outside.join("secret.r16"), [0u8; 2 * 17 * 17]).expect("write");
        std::os::unix::fs::symlink(&outside, root.join("away")).expect("link dir");
        std::os::unix::fs::symlink(outside.join("secret.r16"), root.join("leaf.r16")).expect("link file");
        let ok = |r: Result<Value, RpcError>| r.is_ok();
        assert!(!ok(import(&mut world, &json!({ "path": "leaf.r16" }))));
        assert!(!ok(import(&mut world, &json!({ "path": "away/secret.r16" }))));
        assert!(!ok(export(
            &mut world,
            &json!({ "path": "away/new/deep/out.r16" })
        )));
        assert!(
            !outside.join("new").exists(),
            "no directory made outside the root"
        );
        // A directory that does not exist yet, inside the root, is created as before.
        assert!(ok(export(&mut world, &json!({ "path": "terrain/new/out.r16" }))));
        assert!(root.join("terrain/new/out.r16").is_file());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn png16_is_big_endian_grayscale_and_decodes_back() {
        let hf = Heightfield {
            width: 3,
            height: 2,
            samples: vec![0, 1, 0x1234, 0xfffe, 0xffff, 777],
        };
        let bytes = encode_png16(&hf).expect("png");
        assert_eq!(decode_png16(&bytes).expect("decode"), hf);
    }

    #[test]
    fn wind_get_set_clamps_and_reaches_the_grass_handle() {
        let mut world = World::new();
        let grass = TerrainGrassHandle::new_linked();
        world.insert_resource(grass.clone());
        let out = wind(&mut world).expect("wind");
        assert_eq!(out["gust"].as_f64().map(|g| (g * 100.0).round()), Some(35.0));
        assert_eq!(out["grass_linked"], true);
        assert_eq!(out["weather_steered"], false);
        let out = set_wind(
            &mut world,
            &json!({ "direction": [0.0, 3.0], "strength": 9.0, "gust": 0.5 }),
        )
        .expect("set");
        assert_eq!(out["unit_direction"], json!([0.0, 1.0]));
        assert_eq!(out["strength"], 4.0);
        assert_eq!(grass.wind().direction, [0.0, 3.0]);
        assert_eq!(grass.wind().gust, 0.5);
        let e = set_wind(&mut world, &json!({ "direction": [0.0, 0.0] })).expect_err("zero");
        assert_eq!(reason(&e), "wind_bad_direction");
        assert!(set_wind(&mut world, &json!({ "strength": "high" })).is_err());
        assert_eq!(world.resource::<TerrainWind>().strength, 4.0);
    }

    #[test]
    fn wind_style_defaults_to_classic_and_switches_to_natural() {
        let mut world = World::new();
        let grass = TerrainGrassHandle::new_linked();
        world.insert_resource(grass.clone());
        assert_eq!(wind(&mut world).expect("wind")["style"], "classic");
        let out = set_wind(&mut world, &json!({ "style": "natural" })).expect("natural");
        assert_eq!(out["style"], "natural");
        assert_eq!(grass.wind().style, regia_terrain::WindStyle::Natural);
        // Other fields keep the style.
        let out = set_wind(&mut world, &json!({ "strength": 2.0 })).expect("strength");
        assert_eq!(out["style"], "natural");
        for bad in [json!({ "style": "gale" }), json!({ "style": 3 })] {
            let e = set_wind(&mut world, &bad).expect_err("bad style");
            assert_eq!(reason(&e), "wind_bad_style");
        }
        assert_eq!(
            world.resource::<TerrainWind>().style,
            regia_terrain::WindStyle::Natural
        );
        assert_eq!(
            set_wind(&mut world, &json!({ "style": "classic" })).expect("back")["style"],
            "classic"
        );
    }
}
