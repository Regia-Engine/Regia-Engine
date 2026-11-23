// SPDX-License-Identifier: MPL-2.0
//! The terrain editing remainder on the agent protocol (ADR-2411, `book/agents/level-terrain.md`):
//! the terrain a level names (`terrain.level_terrain`, `terrain.set_level_terrain`,
//! `terrain.load_level_terrain`), the grass density brush and its `<map>.grassdensity.bin`
//! (`terrain.grass_density_stroke`, `terrain.undo_grass_density`, `terrain.redo_grass_density`,
//! `terrain.grass_density`, `terrain.save_grass_density`, `terrain.load_grass_density`), and
//! `.scatter.ron` rule editing (`terrain.scatter_rules`, `terrain.set_scatter_rule`).

use std::path::Path;

use regia_app::prelude::*;
use regia_scene::level::{LevelSettings, LevelTerrain};
use regia_terrain::{
    GrassDensityFileRefused, GrassDensityPaint, GrassDensityStroke, GrassDensityUndo, LevelTerrainError,
    LevelTerrainFiles, LevelTerrainRoots, LevelTerrainSource, ScatterSet, Terrain, grass_density_path,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};
use crate::terrain_heights::{project_file, target_terrain};

const HINT: &str = "see book/agents/level-terrain.md";

/// Records each of the density undo and redo stacks keeps.
const DENSITY_LOG_CAP: usize = 256;

/// Bytes the density undo records may hold together (each keeps the touched rectangle twice).
const DENSITY_LOG_BYTES: usize = 256 << 20;

/// Largest density or scatter file read, bytes.
const MAX_FILE_BYTES: u64 = 64 << 20;

fn coded(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, reason, message, HINT)
}

fn level_error(e: &LevelTerrainError) -> RpcError {
    let code = match e {
        LevelTerrainError::Invalid(_) => protocol::INVALID_PARAMS,
        LevelTerrainError::Read { .. } => protocol::NOT_FOUND,
        LevelTerrainError::Decode { .. } => protocol::INVALID_PARAMS,
        LevelTerrainError::Mapping(_) => protocol::INVALID_PARAMS,
    };
    coded(code, e.code(), e.to_string())
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a string) is required")))
}

/// The project root and cooked cache: the asset server's, else the agent's project directory
/// and its cooked cache for this platform.
fn roots(world: &World, level: &str) -> LevelTerrainRoots {
    let (root, cooked) = match world.get_resource::<regia_asset::AssetServer>() {
        Some(server) => (
            server.project_root().to_path_buf(),
            server.cache_dir().to_path_buf(),
        ),
        None => {
            let root = crate::req_path::project_root(world);
            let cooked = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
            (root, cooked)
        }
    };
    LevelTerrainRoots {
        assets: root.join("assets"),
        cooked,
        level: level.to_owned(),
    }
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn files_json(root: &Path, files: &LevelTerrainFiles) -> Value {
    let entry = |p: &Path| json!({ "path": rel(root, p), "exists": p.exists() });
    json!({
        "layers": files.layers.as_deref().map(entry),
        "map": rel(root, &files.map),
        "holes": entry(&files.holes),
        "weights": entry(&files.weights),
        "scatter_paint": entry(&files.scatter_paint),
        "grass_density": entry(&files.grass_density),
    })
}

fn terrain_json(t: Option<&LevelTerrain>) -> Value {
    t.map_or(Value::Null, |t| {
        json!({
            "layers": t.layers,
            "map": t.map,
            "world_width_m": t.world_width_m,
            "height_scale_m": t.height_scale_m,
            "height_offset_m": t.height_offset_m,
            "sample_layout": t.sample_layout.as_str(),
        })
    })
}

/// `terrain.level_terrain`: the terrain the open level's `LevelSettings` names, the files it
/// resolves to for `level` (when given), and the level terrains loaded in the world.
pub fn level_terrain(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let block = world
        .get_resource::<LevelSettings>()
        .and_then(|s| s.terrain.clone());
    let files = match crate::params::opt_str(params, "level")? {
        None => Value::Null,
        Some(level) => {
            let roots = roots(world, level);
            let files = LevelTerrainFiles::resolve(&roots.assets, level, block.as_ref())
                .map_err(|e| level_error(&e))?;
            files_json(&roots.assets, &files)
        }
    };
    let mut loaded: Vec<Value> = world
        .query::<(Entity, &LevelTerrainSource)>()
        .iter(world)
        .map(|(e, s)| {
            json!({
                "entity": e.to_bits(),
                "level": s.level,
                "digest": format!("{:016x}", s.digest),
                "map_id": s.map_id.map(|id| id.to_string()),
            })
        })
        .collect();
    loaded.sort_by_key(|v| v["entity"].as_u64());
    let status = world
        .get_resource::<regia_terrain::LevelTerrainStatus>()
        .map(|s| {
            json!({
                "level": s.level,
                "error": s.error.as_ref().map(|(code, message)| json!({ "code": code, "message": message })),
                "digest": s.digest.map(|d| format!("{d:016x}")),
            })
        });
    Ok(json!({
        "terrain": terrain_json(block.as_ref()),
        "files": files,
        "loaded": loaded,
        "status": status,
    }))
}

/// `terrain.set_level_terrain`: sets (or with `terrain: null` clears) the open level's
/// `LevelSettings::terrain`; the next save writes it (scene format v12).
pub fn set_level_terrain(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let raw = params
        .get("terrain")
        .ok_or_else(|| RpcError::invalid_params("`terrain` (an object, or null to clear) is required"))?;
    let block: Option<LevelTerrain> = if raw.is_null() {
        None
    } else {
        let block: LevelTerrain = serde_json::from_value(raw.clone()).map_err(|e| {
            coded(
                protocol::INVALID_PARAMS,
                "level_terrain_invalid",
                format!("terrain: {e}"),
            )
        })?;
        block
            .validate()
            .map_err(|e| coded(protocol::INVALID_PARAMS, "level_terrain_invalid", e))?;
        Some(block)
    };
    let mut settings = world.get_resource::<LevelSettings>().cloned().unwrap_or_default();
    let changed = settings.terrain != block;
    settings.terrain = block;
    if changed {
        world.insert_resource(settings.clone());
    }
    Ok(json!({ "terrain": terrain_json(settings.terrain.as_ref()), "changed": changed }))
}

/// `terrain.load_level_terrain`: builds the terrain the open level names for `level` (relative to
/// `assets/`) from its cooked heightfield and side files, replacing an earlier level terrain.
pub fn load_level_terrain(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let level = str_param(params, "level")?.to_owned();
    let roots = roots(world, &level);
    let settings = world.get_resource::<LevelSettings>().cloned().unwrap_or_default();
    let spawn = regia_terrain::spawn_level_terrain(world, &roots, &settings).map_err(|e| level_error(&e))?;
    let source = spawn
        .entity
        .and_then(|e| world.get::<LevelTerrainSource>(e))
        .cloned();
    let files = LevelTerrainFiles::resolve(&roots.assets, &level, settings.terrain.as_ref())
        .map_err(|e| level_error(&e))?;
    Ok(json!({
        "entity": spawn.entity.map(Entity::to_bits),
        "replaced": spawn.replaced,
        "digest": format!("{:016x}", spawn.digest),
        "map_id": source.as_ref().and_then(|s| s.map_id).map(|id| id.to_string()),
        "cooked_heightfield": source
            .as_ref()
            .and_then(|s| s.cooked_heightfield.as_deref())
            .map(|p| p.to_string_lossy().replace('\\', "/")),
        "files": files_json(&roots.assets, &files),
        "source_mapping": source
            .as_ref()
            .and_then(|s| s.source_mapping)
            .map(|m| crate::terrain_source::mapping_json(&m)),
    }))
}

/// The agent's grass density undo and redo records (`terrain.undo_grass_density`).
#[derive(Resource, Default)]
pub struct GrassDensityLog {
    undo: Vec<(Entity, GrassDensityUndo)>,
    redo: Vec<(Entity, GrassDensityUndo)>,
}

fn f32_param(params: &Value, key: &str, default: f32) -> Result<f32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|v| v as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
    }
}

fn point(v: &Value) -> Option<[f32; 2]> {
    let a = v.as_array()?;
    if a.len() != 2 {
        return None;
    }
    Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32])
}

fn density_json(entity: Entity, paint: &GrassDensityPaint) -> Value {
    json!({
        "entity": entity.to_bits(),
        "side": paint.side(),
        "painted": paint.painted_texels(),
        "digest": format!("{:016x}", paint.digest()),
    })
}

/// `terrain.grass_density_stroke`: one density stroke on the terrain's painted grass density
/// (a blank image over the map, `texel_m` metres per texel, is made on the first stroke).
pub fn grass_density_stroke(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target_terrain(world, params)?;
    let path: Vec<[f32; 2]> = match (params.get("path"), params.get("center")) {
        (Some(Value::Array(points)), _) => points
            .iter()
            .map(point)
            .collect::<Option<_>>()
            .ok_or_else(|| RpcError::invalid_params("`path` is a list of [x, z] points"))?,
        (None, Some(c)) => vec![point(c).ok_or_else(|| RpcError::invalid_params("`center` is [x, z]"))?],
        _ => {
            return Err(RpcError::invalid_params(
                "`path` ([[x, z], …]) or `center` ([x, z]) is required",
            ));
        }
    };
    let stroke = GrassDensityStroke {
        path,
        radius_m: f32_param(params, "radius", 8.0)?,
        hardness: f32_param(params, "hardness", 0.5)?,
        strength: f32_param(params, "strength", 1.0)?,
        target: f32_param(params, "target", 0.0)?,
    };
    let bad = |e: regia_terrain::GrassDensityError| {
        coded(
            protocol::INVALID_PARAMS,
            "grass_density_bad_stroke",
            e.to_string(),
        )
    };
    stroke.validate().map_err(bad)?;
    if world
        .get_resource::<GrassDensityFileRefused>()
        .is_some_and(|flag| flag.refused)
    {
        return Err(coded(
            protocol::INVALID_PARAMS,
            "grass_density_unreadable",
            "the open level's grass density file exists and cannot be read",
        ));
    }
    let texel = f32_param(params, "texel_m", 1.0)?;
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| RpcError::internal("terrain vanished mid-call"))?;
    let world_width = terrain.map.world_width();
    if !world.entity(entity).contains::<GrassDensityPaint>() {
        let blank = GrassDensityPaint::for_map(&terrain.map, texel).map_err(bad)?;
        world.entity_mut(entity).insert(blank);
    }
    let mut paint = world
        .get_mut::<GrassDensityPaint>(entity)
        .ok_or_else(|| RpcError::internal("density image vanished mid-call"))?;
    let undo = paint.stroke(&stroke, world_width).map_err(bad)?;
    let changed = undo.changed;
    let reply = density_json(entity, &paint);
    let mut log = world.get_resource_or_insert_with(GrassDensityLog::default);
    if changed > 0 {
        log.undo.push((entity, undo));
        let bytes = |log: &GrassDensityLog| {
            log.undo
                .iter()
                .map(|(_, u)| u.before.len() + u.after.len())
                .sum::<usize>()
        };
        while log.undo.len() > DENSITY_LOG_CAP || (log.undo.len() > 1 && bytes(&log) > DENSITY_LOG_BYTES) {
            log.undo.remove(0);
        }
        log.redo.clear();
    }
    let depth = log.undo.len();
    let mut reply = reply;
    reply["changed"] = json!(changed);
    reply["undo_depth"] = json!(depth);
    Ok(reply)
}

fn step_density(world: &mut World, redo: bool) -> Result<Value, RpcError> {
    let mut log = world.remove_resource::<GrassDensityLog>().unwrap_or_default();
    let record = if redo { log.redo.pop() } else { log.undo.pop() };
    let result = match record {
        None => Ok(json!({ "entity": null, "stepped": false })),
        Some((entity, undo)) => {
            let applied = match world.get_mut::<GrassDensityPaint>(entity) {
                Some(mut paint) => {
                    if redo {
                        undo.redo(&mut paint)
                    } else {
                        undo.undo(&mut paint)
                    }
                }
                None => false,
            };
            let reply = world
                .get::<GrassDensityPaint>(entity)
                .map(|p| density_json(entity, p));
            if applied {
                if redo {
                    log.undo.push((entity, undo));
                } else {
                    log.redo.push((entity, undo));
                }
            }
            let mut reply = reply.unwrap_or_else(|| json!({ "entity": entity.to_bits() }));
            reply["stepped"] = json!(applied);
            Ok(reply)
        }
    };
    let (u, r) = (log.undo.len(), log.redo.len());
    world.insert_resource(log);
    result.map(|mut v| {
        v["undo_depth"] = json!(u);
        v["redo_depth"] = json!(r);
        v
    })
}

/// `terrain.undo_grass_density`: undoes the agent's last density stroke, restoring its bytes.
pub fn undo_grass_density(world: &mut World) -> Result<Value, RpcError> {
    step_density(world, false)
}

/// `terrain.redo_grass_density`: re-applies the last undone density stroke.
pub fn redo_grass_density(world: &mut World) -> Result<Value, RpcError> {
    step_density(world, true)
}

/// `terrain.grass_density`: the terrain's painted grass density (size, painted texels, digest).
pub fn grass_density(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target_terrain(world, params)?;
    match world.get::<GrassDensityPaint>(entity) {
        Some(paint) => Ok(density_json(entity, paint)),
        None => Err(coded(
            protocol::NOT_FOUND,
            "grass_density_none",
            format!("entity {} has no painted grass density", entity.to_bits()),
        )),
    }
}

/// The density file of `path`: a `.grassdensity.bin` name as it is, a map or level file's
/// [`grass_density_path`].
fn density_name(params: &Value) -> Result<String, RpcError> {
    let path = str_param(params, "path")?;
    Ok(if path.to_ascii_lowercase().ends_with(".grassdensity.bin") {
        path.to_owned()
    } else {
        grass_density_path(Path::new(path))
            .to_string_lossy()
            .replace('\\', "/")
    })
}

/// `terrain.save_grass_density`: writes the terrain's painted grass density to the
/// `<map>.grassdensity.bin` of `path` under the project root, atomically.
pub fn save_grass_density(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = density_name(params)?;
    let file = project_file(world, &name)?;
    let entity = target_terrain(world, params)?;
    let paint = world.get::<GrassDensityPaint>(entity).ok_or_else(|| {
        coded(
            protocol::NOT_FOUND,
            "grass_density_none",
            format!("entity {} has no painted grass density", entity.to_bits()),
        )
    })?;
    let bytes = paint
        .to_bytes()
        .map_err(|e| coded(protocol::INTERNAL_ERROR, "grass_density_write", e.to_string()))?;
    let side = paint.side();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            coded(
                protocol::INTERNAL_ERROR,
                "grass_density_write",
                format!("{name}: {e}"),
            )
        })?;
    }
    crate::bake_export::write_atomic(&file, &bytes).map_err(|e| {
        coded(
            protocol::INTERNAL_ERROR,
            "grass_density_write",
            format!("{name}: {e}"),
        )
    })?;
    Ok(json!({ "entity": entity.to_bits(), "path": name, "side": side, "bytes": bytes.len() }))
}

fn read_capped(world: &World, name: &str, code_read: &str, code_decode: &str) -> Result<Vec<u8>, RpcError> {
    let file = project_file(world, name)?;
    let read_error = |e: std::io::Error| coded(protocol::NOT_FOUND, code_read, format!("{name}: {e}"));
    let size = std::fs::metadata(&file).map_err(read_error)?.len();
    if size > MAX_FILE_BYTES {
        return Err(coded(
            protocol::INVALID_PARAMS,
            code_decode,
            format!("{name} is {size} bytes; the largest file read is {MAX_FILE_BYTES}"),
        ));
    }
    std::fs::read(&file).map_err(read_error)
}

/// `terrain.load_grass_density`: reads a `<map>.grassdensity.bin` under the project root into the
/// terrain's painted grass density (replacing it; the agent's density undo records are dropped).
pub fn load_grass_density(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = density_name(params)?;
    let entity = target_terrain(world, params)?;
    let bytes = read_capped(world, &name, "grass_density_read", "grass_density_decode")?;
    let paint = GrassDensityPaint::from_bytes(&bytes).map_err(|e| {
        coded(
            protocol::INVALID_PARAMS,
            "grass_density_decode",
            format!("{name}: {e}"),
        )
    })?;
    let mut reply = density_json(entity, &paint);
    world.entity_mut(entity).insert(paint);
    if let Some(mut log) = world.get_resource_mut::<GrassDensityLog>() {
        log.undo.retain(|(e, _)| *e != entity);
        log.redo.retain(|(e, _)| *e != entity);
    }
    reply["path"] = json!(name);
    Ok(reply)
}

fn scatter_name(params: &Value) -> Result<String, RpcError> {
    let path = str_param(params, "path")?;
    if !path.to_ascii_lowercase().ends_with(".scatter.ron") {
        return Err(coded(
            protocol::INVALID_PARAMS,
            "scatter_rules_read",
            format!("`{path}` is not a .scatter.ron"),
        ));
    }
    Ok(path.to_owned())
}

pub(crate) fn read_scatter(world: &World, name: &str) -> Result<ScatterSet, RpcError> {
    let bytes = read_capped(world, name, "scatter_rules_read", "scatter_rules_invalid")?;
    let text = String::from_utf8(bytes).map_err(|e| {
        coded(
            protocol::INVALID_PARAMS,
            "scatter_rules_invalid",
            format!("{name}: {e}"),
        )
    })?;
    ScatterSet::from_ron(&text, None).map_err(|e| {
        coded(
            protocol::INVALID_PARAMS,
            "scatter_rules_invalid",
            format!("{name}: {e}"),
        )
    })
}

fn class_fields(class: &regia_terrain::ScatterClass) -> Value {
    let mut v = serde_json::to_value(class).unwrap_or(Value::Null);
    if let Some(map) = v.as_object_mut() {
        map.remove("name");
    }
    v
}

/// `terrain.scatter_rules`: the classes of a `.scatter.ron` and every rule field.
pub fn scatter_rules(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = scatter_name(params)?;
    let set = read_scatter(world, &name)?;
    let classes: Vec<Value> = set
        .classes
        .iter()
        .map(|c| json!({ "name": c.name, "fields": class_fields(c) }))
        .collect();
    Ok(json!({ "path": name, "version": set.version, "seed": set.seed, "classes": classes }))
}

/// `terrain.set_scatter_rule`: sets one rule field of one class of a `.scatter.ron` (the value
/// must have the field's kind and the set must still validate) and saves the file as canonical
/// RON, atomically.
pub fn set_scatter_rule(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = scatter_name(params)?;
    let class = str_param(params, "class")?;
    let field = str_param(params, "field")?;
    let value = params
        .get("value")
        .cloned()
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let field_error = |m: String| coded(protocol::INVALID_PARAMS, "scatter_rules_field", m);
    if field == "name" || field == "meshes" {
        return Err(field_error(format!("`{field}` is not a rule field")));
    }
    let mut set = read_scatter(world, &name)?;
    let index = set
        .classes
        .iter()
        .position(|c| c.name == class)
        .ok_or_else(|| field_error(format!("no class `{class}` in {name}")))?;
    let mut json_class =
        serde_json::to_value(&set.classes[index]).map_err(|e| RpcError::internal(e.to_string()))?;
    let slot = json_class
        .get_mut(field)
        .ok_or_else(|| field_error(format!("class `{class}` has no field `{field}`")))?;
    let kind = |v: &Value| match v {
        Value::Number(_) => 0,
        Value::Bool(_) => 1,
        Value::String(_) | Value::Null => 2,
        Value::Array(_) => 3,
        Value::Object(_) => 4,
    };
    if kind(&*slot) != kind(&value) {
        return Err(field_error(format!(
            "`{field}` is {slot}; {value} is another kind"
        )));
    }
    *slot = value.clone();
    set.classes[index] =
        serde_json::from_value(json_class).map_err(|e| field_error(format!("`{field}`: {e}")))?;
    let text = set
        .to_ron()
        .map_err(|e| coded(protocol::INVALID_PARAMS, "scatter_rules_invalid", e.to_string()))?;
    let file = project_file(world, &name)?;
    crate::bake_export::write_atomic(&file, text.as_bytes()).map_err(|e| {
        coded(
            protocol::INTERNAL_ERROR,
            "scatter_rules_write",
            format!("{name}: {e}"),
        )
    })?;
    Ok(json!({
        "path": name,
        "class": class,
        "field": field,
        "value": class_fields(&set.classes[index]).get(field).cloned(),
        "bytes": text.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap};

    trait Reason {
        fn reason(&self) -> Option<&str>;
    }

    impl Reason for RpcError {
        fn reason(&self) -> Option<&str> {
            self.data.as_ref().and_then(|d| d["code"].as_str())
        }
    }

    fn world_with_terrain(root: &Path) -> (World, Entity) {
        let mut world = World::new();
        std::fs::create_dir_all(root).expect("root");
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        world.insert_resource(server);
        let n = 33u32;
        let heights = (0..n * n).map(|i| (i % n) as f32 * 0.1).collect();
        let map = TerrainMap::from_heights(n, n, heights, 32.0, 10.0, 0.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            32.0,
            10.0,
            0.0,
            CdlodConfig::default(),
            1,
            TerrainLayers::default(),
        )
        .expect("terrain");
        let e = world.spawn(terrain).id();
        (world, e)
    }

    #[test]
    fn density_strokes_are_deterministic_undoable_and_saved() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_density");
        std::fs::create_dir_all(&root).expect("dir");
        let stroke = json!({ "path": [[-6.0, 0.0], [6.0, 4.0]], "radius": 5.0, "target": 0.0 });
        let (mut a, ea) = world_with_terrain(&root);
        let (mut b, _) = world_with_terrain(&root);
        let ra = grass_density_stroke(&mut a, &stroke).expect("stroke");
        let rb = grass_density_stroke(&mut b, &stroke).expect("stroke");
        assert_eq!(ra["digest"], rb["digest"], "the same stroke, the same bytes");
        assert!(ra["changed"].as_u64().unwrap_or(0) > 0, "{ra}");
        let blank = GrassDensityPaint::for_map(&a.get::<Terrain>(ea).expect("t").map, 1.0).expect("blank");
        let undone = undo_grass_density(&mut a).expect("undo");
        assert_eq!(undone["stepped"], true);
        assert_eq!(
            a.get::<GrassDensityPaint>(ea).expect("p").texels(),
            blank.texels()
        );
        let redone = redo_grass_density(&mut a).expect("redo");
        assert_eq!(redone["digest"], ra["digest"]);
        assert_eq!(
            undo_grass_density(&mut World::new()).expect("empty")["stepped"],
            false
        );

        let saved =
            save_grass_density(&mut a, &json!({ "path": "assets/maps/arena.level.ron" })).expect("save");
        assert_eq!(saved["path"], "assets/maps/arena.grassdensity.bin");
        let (mut fresh, _) = world_with_terrain(&root);
        let loaded = load_grass_density(
            &mut fresh,
            &json!({ "path": "assets/maps/arena.grassdensity.bin" }),
        )
        .expect("load");
        assert_eq!(loaded["digest"], ra["digest"]);
        assert_eq!(
            grass_density(&mut fresh, &json!({})).expect("get")["digest"],
            ra["digest"]
        );

        let err =
            grass_density_stroke(&mut a, &json!({ "center": [0.0, 0.0], "radius": -1.0 })).expect_err("bad");
        assert_eq!(err.reason(), Some("grass_density_bad_stroke"));
        std::fs::write(root.join("assets/maps/junk.grassdensity.bin"), b"junk").expect("write");
        let err = load_grass_density(&mut a, &json!({ "path": "assets/maps/junk.grassdensity.bin" }))
            .expect_err("junk");
        assert_eq!(err.reason(), Some("grass_density_decode"));
        let err = load_grass_density(&mut a, &json!({ "path": "assets/maps/none.grassdensity.bin" }))
            .expect_err("none");
        assert_eq!(err.reason(), Some("grass_density_read"));
        let (mut bare, _) = world_with_terrain(&root);
        assert_eq!(
            grass_density(&mut bare, &json!({})).expect_err("none").reason(),
            Some("grass_density_none")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refused_density_file_blocks_the_stroke() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_density_refused");
        let (mut world, entity) = world_with_terrain(&root);
        world.insert_resource(GrassDensityFileRefused { refused: true });
        let err = grass_density_stroke(&mut world, &json!({ "center": [0.0, 0.0], "radius": 1.0 }))
            .expect_err("refused");
        assert_eq!(err.reason(), Some("grass_density_unreadable"));
        assert!(world.get::<GrassDensityPaint>(entity).is_none());
        world.resource_mut::<GrassDensityFileRefused>().refused = false;
        grass_density_stroke(&mut world, &json!({ "center": [0.0, 0.0], "radius": 1.0 }))
            .expect("stroke once the file is readable");
        assert!(world.get::<GrassDensityPaint>(entity).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scatter_rules_edit_and_save_canonical_ron() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_scatter_rules");
        std::fs::create_dir_all(root.join("assets")).expect("dir");
        let class = regia_terrain::ScatterClass {
            name: "rocks".into(),
            meshes: vec![regia_terrain::ScatterMeshRef {
                mesh: "models/rock.gltf".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 1.0,
                scale_bias: 1.0,
            }],
            ..regia_terrain::ScatterClass::default()
        };
        let set = ScatterSet {
            classes: vec![class],
            ..ScatterSet::default()
        };
        std::fs::write(root.join("assets/a.scatter.ron"), set.to_ron().expect("ron")).expect("write");
        let (mut world, _) = world_with_terrain(&root);
        let p = json!({ "path": "assets/a.scatter.ron" });
        let rules = scatter_rules(&mut world, &p).expect("rules");
        assert_eq!(rules["classes"][0]["name"], "rocks");
        let out = set_scatter_rule(
            &mut world,
            &json!({ "path": "assets/a.scatter.ron", "class": "rocks", "field": "density", "value": 0.5 }),
        )
        .expect("set");
        assert_eq!(out["value"], 0.5);
        let text = std::fs::read_to_string(root.join("assets/a.scatter.ron")).expect("read");
        let back = ScatterSet::from_ron(&text, None).expect("parse");
        assert_eq!(text, back.to_ron().expect("ron"), "canonical");
        assert!((back.classes[0].density - 0.5).abs() < 1e-6);
        let mut field = |f: &str, v: Value| {
            set_scatter_rule(
                &mut world,
                &json!({ "path": "assets/a.scatter.ron", "class": "rocks", "field": f, "value": v }),
            )
            .expect_err("refused")
            .reason()
            .map(str::to_owned)
        };
        assert_eq!(field("density", json!("x")), Some("scatter_rules_field".into()));
        assert_eq!(field("nope", json!(1)), Some("scatter_rules_field".into()));
        assert_eq!(field("name", json!("x")), Some("scatter_rules_field".into()));
        let err = scatter_rules(&mut world, &json!({ "path": "assets/a.ron" })).expect_err("suffix");
        assert_eq!(err.reason(), Some("scatter_rules_read"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_level_terrain_validates_and_reports() {
        let root = regia_test_utils::unique_temp_dir("agent_level_terrain");
        let (mut world, _) = world_with_terrain(&root);
        let out = set_level_terrain(
            &mut world,
            &json!({ "terrain": { "layers": "maps/a/terrain.layers.ron", "height_scale_m": 80.0 } }),
        )
        .expect("set");
        assert_eq!(out["changed"], true);
        assert_eq!(out["terrain"]["height_scale_m"], 80.0);
        let info = level_terrain(&mut world, &json!({ "level": "maps/a.level.ron" })).expect("info");
        assert_eq!(info["terrain"]["layers"], "maps/a/terrain.layers.ron");
        assert_eq!(info["files"]["holes"]["path"], "maps/a.holes.ron");
        assert_eq!(info["files"]["grass_density"]["exists"], false);
        let err = set_level_terrain(
            &mut world,
            &json!({ "terrain": { "layers": "../x.terrain.layers.ron" } }),
        )
        .expect_err("escape");
        assert_eq!(err.reason(), Some("level_terrain_invalid"));
        let err =
            load_level_terrain(&mut world, &json!({ "level": "maps/a.level.ron" })).expect_err("missing");
        assert_eq!(err.reason(), Some("level_terrain_read"));
        let cleared = set_level_terrain(&mut world, &json!({ "terrain": null })).expect("clear");
        assert_eq!(cleared["terrain"], Value::Null);
    }
}
