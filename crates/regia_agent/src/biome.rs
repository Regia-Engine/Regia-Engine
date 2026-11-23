// SPDX-License-Identifier: MPL-2.0
//! `biome.*`: biome areas, claims, statistics and definitions (specs/65 § 10, book
//! `agents/biomes.md`, guide topic `biomes`).
//!
//! Errors answer `F0002` (invalid params or definition) or `F0003` (unknown path or entity) with
//! `data.reason` set to one of `biome_unknown`, `biome_invalid` (with `data.errors`),
//! `biome_area_open`, `biome_entry_unknown`, `biome_not_generator`, `biome_pointer_invalid`,
//! `ecology_invalid` (`biome.validate_ecology`, ADR-2456).

use std::collections::BTreeMap;

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_procgen::biome::def::{BiomeDef, BiomeEntry, BiomeIssue};
use regia_procgen::biome::ecs::{AreaShape, BiomeArea, biome_world_for, capture_biomes_with, load_def};
use regia_procgen::biome::{BiomeWorld, Claim};
use regia_procgen::world::{ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphOverlay, ProcGraphSource};
use regia_scene::components::{EntityGuid, Name};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most `def_edit` ops per call.
pub(crate) const MAX_OPS: usize = 256;

fn reason(code: i64, regia_code: &str, why: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": why, "hint": "see book/agents/biomes.md" })),
    }
}

fn bad(why: &str, message: impl Into<String>) -> RpcError {
    reason(INVALID_PARAMS, "F0002", why, message)
}

fn missing(why: &str, message: impl Into<String>) -> RpcError {
    reason(NOT_FOUND, "F0003", why, message)
}

fn issues_json(issues: &[BiomeIssue]) -> Value {
    Value::Array(
        issues
            .iter()
            .map(|i| json!({ "reason": i.code, "path": i.path, "message": i.message }))
            .collect(),
    )
}

fn invalid(issues: &[BiomeIssue]) -> RpcError {
    let code = issues.first().map_or("biome_invalid", |i| i.code.as_str());
    let mut e = bad(
        code,
        format!("{} problem(s) in the biome definition", issues.len()),
    );
    if let Some(Value::Object(d)) = e.data.as_mut() {
        d.insert("errors".into(), issues_json(issues));
    }
    e
}

fn available(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<ProcGenSettings>() {
        Ok(())
    } else {
        Err(missing(
            "biome_unknown",
            "procedural generation is not installed (ProcGenPlugin)",
        ))
    }
}

fn entity_param(params: &Value, key: &str) -> Result<Option<Entity>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let bits = v
                .as_u64()
                .ok_or_else(|| bad("biome_invalid", format!("`{key}` must be an entity id")))?;
            Entity::try_from_bits(bits)
                .map(Some)
                .ok_or_else(|| bad("biome_invalid", format!("{bits} is not an entity id")))
        }
    }
}

fn generator_param(world: &World, params: &Value, required: bool) -> Result<Option<Entity>, RpcError> {
    let e = entity_param(params, "generator")?;
    match e {
        Some(e) if world.get::<ProcGenerator>(e).is_none() => Err(missing(
            "biome_not_generator",
            format!("entity {} has no ProcGenerator", e.to_bits()),
        )),
        None if required => Err(bad(
            "biome_not_generator",
            "`generator` (an entity id) is required",
        )),
        other => Ok(other),
    }
}

/// The biome world of a generator, or of every area in the level.
fn capture(
    world: &mut World,
    generator: Option<Entity>,
) -> (BiomeWorld, Vec<regia_procgen::biome::ecs::AreaProblem>) {
    match generator {
        Some(g) => biome_world_for(world, g),
        None => {
            let mut deps = Vec::new();
            capture_biomes_with(world, [f32::MIN, f32::MIN, f32::MAX, f32::MAX], &mut deps, false)
        }
    }
}

fn entity_of(world: &mut World, guid: Guid) -> Option<u64> {
    let mut q = world.query::<(Entity, &EntityGuid)>();
    q.iter(world).find(|(_, g)| g.0 == guid).map(|(e, _)| e.to_bits())
}

fn shape_kind(shape: &AreaShape) -> &'static str {
    match shape {
        AreaShape::FromShape => "shape",
        AreaShape::FromSpline => "spline",
        AreaShape::Map { .. } => "map",
    }
}

/// Per `biome/layer`: the counters of a generator's last report (`candidates`, `unassigned`, and
/// `placed` summed over the layer's entries).
fn layer_counts(world: &World, generator: Option<Entity>) -> BTreeMap<String, Map<String, Value>> {
    let mut out: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut add = |key: &str, stat: &str, n: u64| {
        let slot = out.entry(key.to_string()).or_default();
        let cur = slot.get(stat).and_then(Value::as_u64).unwrap_or(0);
        slot.insert(stat.to_string(), json!(cur + n));
    };
    let reports: Vec<&regia_procgen::RunReport> = match generator {
        Some(g) => world
            .get::<ProcGenState>(g)
            .and_then(|s| s.report.as_ref())
            .into_iter()
            .collect(),
        None => Vec::new(),
    };
    for r in reports {
        for n in &r.nodes {
            for (k, v) in &n.counters {
                let Some((path, stat)) = k.split_once('|') else {
                    continue;
                };
                let parts: Vec<&str> = path.split('/').collect();
                match (parts.len(), stat) {
                    (2, "candidates" | "unassigned") => add(path, stat, *v),
                    (3, "placed") => add(&format!("{}/{}", parts[0], parts[1]), "placed", *v),
                    _ => {}
                }
            }
        }
    }
    out
}

/// `biome.areas { generator? }`.
pub(crate) fn areas(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let generator = generator_param(world, params, false)?;
    let (bw, problems) = capture(world, generator);
    let counts = layer_counts(world, generator);
    let mut list = Vec::new();
    let mut q = world.query::<(Entity, &BiomeArea, Option<&EntityGuid>, Option<&Name>)>();
    let mut rows: Vec<(Entity, BiomeArea, Guid, String)> = q
        .iter(world)
        .map(|(e, a, g, n)| {
            (
                e,
                a.clone(),
                g.map_or(Guid::NIL, |g| g.0),
                n.map(|n| n.as_str().to_string()).unwrap_or_default(),
            )
        })
        .collect();
    rows.sort_by_key(|r| r.2);
    for (e, a, guid, name) in rows {
        let snap = bw.areas.iter().find(|s| s.guid == guid);
        let problem = problems.iter().find(|p| p.guid == guid);
        if generator.is_some() && snap.is_none() && problem.is_none() {
            continue; // outside the generator's bounds
        }
        let biomes: Vec<Value> = snap
            .map(|s| {
                s.biomes
                    .iter()
                    .enumerate()
                    .map(|(bi, (path, def))| {
                        let layers: Map<String, Value> = def
                            .layers
                            .iter()
                            .map(|l| {
                                let key = format!("{}/{}", def.name, l.name);
                                (l.name.clone(), Value::Object(counts.get(&key).cloned().unwrap_or_default()))
                            })
                            .collect();
                        json!({ "path": path, "name": def.name, "priority": s.priority(bi), "layers": layers })
                    })
                    .collect()
            })
            .unwrap_or_default();
        list.push(json!({
            "entity": e.to_bits(),
            "guid": guid.to_string(),
            "name": name,
            "biome": a.biome,
            "map_biomes": a.map_biomes,
            "shape": shape_kind(&a.shape),
            "blend_m": a.blend_m,
            "priority_offset": a.priority_offset,
            "preview_alone": a.preview_alone,
            "biomes": biomes,
            "error": problem.map(|p| json!({ "reason": p.code, "message": p.message })),
        }));
    }
    Ok(json!({ "areas": list, "unknown_map_texels": bw.unknown_map_texels }))
}

fn claim_json(world: &mut World, bw: &BiomeWorld, c: Option<Claim>) -> Value {
    let Some(c) = c else {
        return Value::Null;
    };
    let Some(area) = bw.areas.get(c.area) else {
        return Value::Null;
    };
    let (path, def) = match area.biomes.get(c.biome) {
        Some((p, d)) => (p.clone(), d.name.clone()),
        None => (String::new(), String::new()),
    };
    json!({
        "biome": def,
        "path": path,
        "area": entity_of(world, area.guid),
        "area_guid": area.guid.to_string(),
        "priority": area.priority(c.biome),
        "weight": c.weight,
    })
}

/// `biome.claim { point, generator? }`.
pub(crate) fn claim(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let [x, _, z] = crate::params::opt_array(params, "point")?
        .filter(|a| a.len() == 3)
        .and_then(|a| {
            Some([
                a[0].as_f64()? as f32,
                a[1].as_f64()? as f32,
                a[2].as_f64()? as f32,
            ])
        })
        .filter(|p| p.iter().all(|v| v.is_finite()))
        .ok_or_else(|| {
            bad(
                "biome_invalid",
                "`point` must be [x, y, z], three finite numbers within f32 range",
            )
        })?;
    let generator = generator_param(world, params, false)?;
    let (bw, _) = capture(world, generator);
    let c = bw.claims_at(x, z);
    let owner = claim_json(world, &bw, c.owner);
    let second = claim_json(world, &bw, c.second);
    Ok(json!({ "owner": owner, "second": second }))
}

/// `biome.stats { generator }`.
pub(crate) fn stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let Some(g) = generator_param(world, params, true)? else {
        return Err(bad("biome_not_generator", "`generator` is required"));
    };
    let Some(report) = world.get::<ProcGenState>(g).and_then(|s| s.report.clone()) else {
        return Ok(json!({ "entries": {}, "layers": {}, "ran": false }));
    };
    let mut entries: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut layers: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for n in &report.nodes {
        for (k, v) in &n.counters {
            let Some((path, stat)) = k.split_once('|') else {
                continue;
            };
            let depth = path.split('/').count();
            let target = match depth {
                3 => entries.entry(path.to_string()).or_default(),
                2 => layers.entry(path.to_string()).or_default(),
                _ => continue,
            };
            if let Some(f) = stat.strip_prefix("filter:") {
                let filters = target
                    .entry("removed_by_filter")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(m) = filters {
                    let cur = m.get(f).and_then(Value::as_u64).unwrap_or(0);
                    m.insert(f.to_string(), json!(cur + v));
                }
            } else {
                let cur = target.get(stat).and_then(Value::as_u64).unwrap_or(0);
                target.insert(stat.to_string(), json!(cur + v));
            }
        }
    }
    // Every entry carries its layer's candidate count.
    for (path, m) in &mut entries {
        let layer = path.rsplit_once('/').map_or("", |(l, _)| l);
        let c = layers
            .get(layer)
            .and_then(|l| l.get("candidates"))
            .cloned()
            .unwrap_or(json!(0));
        m.insert("candidates".into(), c);
        for stat in [
            "assigned",
            "realized_out",
            "spaced_out",
            "children",
            "resolved_out",
            "excluded",
            "placed",
        ] {
            m.entry(stat).or_insert(json!(0));
        }
        m.entry("removed_by_filter").or_insert_with(|| json!({}));
    }
    Ok(json!({ "entries": entries, "layers": layers, "ran": true }))
}

fn path_param(params: &Value) -> Result<String, RpcError> {
    crate::params::opt_str(params, "path")?
        .map(str::to_string)
        .ok_or_else(|| bad("biome_invalid", "`path` (a .biome.ron) is required"))
}

fn read_def(world: &World, path: &str) -> Result<BiomeDef, RpcError> {
    load_def(world, path).map_err(|(code, message)| {
        if code == "biome_unknown" {
            missing(&code, message)
        } else {
            match regia_procgen::world::read_graph_text(world, path)
                .map(|t| BiomeDef::parse_unchecked(&t).map(|d| d.validate()))
            {
                Some(Ok(issues)) if !issues.is_empty() => invalid(&issues),
                _ => bad(&code, message),
            }
        }
    })
}

/// `biome.def_get { path }`.
pub(crate) fn def_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let path = path_param(params)?;
    let def = read_def(world, &path)?;
    let value = serde_json::to_value(&def).map_err(|e| bad("biome_invalid", e.to_string()))?;
    Ok(json!({ "path": path, "def": value }))
}

fn pointer_mut<'a>(v: &'a mut Value, pointer: &str) -> Result<&'a mut Value, RpcError> {
    v.pointer_mut(pointer)
        .ok_or_else(|| bad("biome_pointer_invalid", format!("no value at `{pointer}`")))
}

fn layer_index(v: &Value, layer: &str) -> Result<usize, RpcError> {
    crate::params::opt_array(v, "layers")?
        .and_then(|ls| {
            ls.iter()
                .position(|l| l.get("name").and_then(Value::as_str) == Some(layer))
        })
        .ok_or_else(|| bad("biome_invalid", format!("no layer `{layer}`")))
}

/// `biome.def_edit { path, ops, save? }`.
pub(crate) fn def_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let path = path_param(params)?;
    let def = read_def(world, &path)?;
    let mut v = serde_json::to_value(&def).map_err(|e| bad("biome_invalid", e.to_string()))?;
    let ops = params
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("biome_invalid", "`ops` (a list) is required"))?;
    if ops.len() > MAX_OPS {
        return Err(bad("biome_invalid", format!("at most {MAX_OPS} ops per call")));
    }
    for op in ops {
        match crate::params::opt_str(op, "op")? {
            Some("set") => {
                let pointer = op
                    .get("pointer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad("biome_pointer_invalid", "`set` needs `pointer`"))?;
                let value = op.get("value").cloned().unwrap_or(Value::Null);
                *pointer_mut(&mut v, pointer)? = value;
            }
            Some("add_entry") => {
                let layer = crate::params::opt_str(op, "layer")?.unwrap_or("");
                let li = layer_index(&v, layer)?;
                let entry: BiomeEntry =
                    serde_json::from_value(op.get("entry").cloned().unwrap_or(Value::Null))
                        .map_err(|e| bad("biome_invalid", format!("add_entry: {e}")))?;
                let entry = serde_json::to_value(entry).map_err(|e| bad("biome_invalid", e.to_string()))?;
                if let Some(Value::Array(es)) = v.pointer_mut(&format!("/layers/{li}/entries")) {
                    es.push(entry);
                }
            }
            Some("remove_entry") => {
                let layer = crate::params::opt_str(op, "layer")?.unwrap_or("");
                let name = crate::params::opt_str(op, "name")?.unwrap_or("");
                let li = layer_index(&v, layer)?;
                let Some(Value::Array(es)) = v.pointer_mut(&format!("/layers/{li}/entries")) else {
                    return Err(bad("biome_invalid", "layer has no entries"));
                };
                let before = es.len();
                es.retain(|e| e.get("name").and_then(Value::as_str) != Some(name));
                if es.len() == before {
                    return Err(bad(
                        "biome_entry_unknown",
                        format!("no entry `{name}` in layer `{layer}`"),
                    ));
                }
            }
            other => {
                return Err(bad("biome_invalid", format!("unknown op {other:?}")));
            }
        }
    }
    let edited: BiomeDef = serde_json::from_value(v).map_err(|e| bad("biome_invalid", e.to_string()))?;
    let issues = edited.validate();
    if !issues.is_empty() {
        return Ok(json!({ "errors": issues_json(&issues), "saved": false, "applied": false }));
    }
    let text = edited.to_ron();
    let save = crate::params::opt_bool(params, "save")?.unwrap_or(false);
    let mut saved = false;
    if save {
        let source = world.resource::<ProcGenSettings>().source.clone();
        match source {
            ProcGraphSource::Dir(root) => {
                // Absolute, drive, UNC, `..` and symlink-escaping paths are refused before anything
                // is written, like `procgen.graph_edit` (ADR-1816, ADR-1875 (11)).
                let file = regia_core::project_path::resolve_write(&root, &path).map_err(|e| {
                    bad(
                        "biome_invalid",
                        format!("`path` must be project-relative ({}): {e}", e.code()),
                    )
                })?;
                regia_procgen::pgout::write_atomic(&file, text.as_bytes())
                    .map_err(|e| bad("biome_invalid", format!("write failed: {e}")))?;
                saved = true;
                if let Some(mut o) = world.get_resource_mut::<ProcGraphOverlay>() {
                    o.0.remove(&path);
                }
            }
            ProcGraphSource::Memory(_) => {}
        }
    }
    if !saved && let Some(mut o) = world.get_resource_mut::<ProcGraphOverlay>() {
        o.0.insert(path.clone(), text);
    }
    let stale = regia_procgen::biome::ecs::mark_readers_stale(world, &path);
    Ok(json!({ "errors": [], "saved": saved, "applied": true, "stale_generators": stale }))
}

/// `biome.preview { area, on }`.
pub(crate) fn preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let Some(e) = entity_param(params, "area")? else {
        return Err(bad("biome_invalid", "`area` (an entity id) is required"));
    };
    let on = params
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad("biome_invalid", "`on` (a bool) is required"))?;
    let Some(mut a) = world.get_mut::<BiomeArea>(e) else {
        return Err(missing(
            "biome_unknown",
            format!("entity {} has no BiomeArea", e.to_bits()),
        ));
    };
    a.preview_alone = on;
    Ok(json!({ "preview_alone": on }))
}

/// The text of a biome or ecology file: the overlay, the project, then the shipped presets
/// (`starter/biomes/...`).
fn eco_text(world: &World, path: &str) -> Option<String> {
    regia_procgen::world::read_graph_text(world, path)
        .or_else(|| regia_procgen::biome::ecology::starter_text(path).map(str::to_string))
}

/// One biome's ecology report as JSON (`biome.validate_ecology`, ADR-2456).
fn ecology_json(
    path: &str,
    eco_path: Option<&str>,
    r: &regia_procgen::biome::ecology::EcologyReport,
) -> Value {
    use regia_procgen::biome::ecology::{MapSource, Severity};
    let cat_names: Vec<Value> = r
        .species
        .iter()
        .map(|m| {
            json!({
                "entry": m.entry,
                "species": m.species,
                "role": m.role.as_str(),
                "share": m.share,
                "weight_share": m.weight_share,
                "source": match m.source { MapSource::Profile => "profile", MapSource::Name => "name" },
            })
        })
        .collect();
    let issues: Vec<Value> = r
        .issues
        .iter()
        .map(|i| {
            json!({
                "reason": i.code,
                "severity": match i.severity { Severity::Error => "error", Severity::Warning => "warning" },
                "path": i.path,
                "species": i.species,
                "message": i.message,
            })
        })
        .collect();
    json!({
        "path": path,
        "ecology": eco_path,
        "biome": r.biome,
        "plausible": r.plausible(),
        "site": r.site.as_ref().map(|s| json!({
            "moisture": [s.moisture.0, s.moisture.1],
            "reaction": [s.reaction.0, s.reaction.1],
            "altitude_m": [s.altitude_m.0, s.altitude_m.1],
            "slope_deg": [s.slope_deg.0, s.slope_deg.1],
            "soil": s.soil,
        })),
        "site_inferred": r.site_inferred,
        "floor_light": r.floor_light,
        "species": cat_names,
        "unmapped": r.unmapped,
        "issues": issues,
    })
}

/// Validates the biome at `path` with its ecology profile (`ecology`, default the file beside it).
fn validate_one(world: &World, path: &str, ecology: Option<&str>) -> Result<Value, RpcError> {
    use regia_procgen::biome::ecology::{EcologyProfile, SpeciesCatalog, ecology_path_for, validate_ecology};
    let text =
        eco_text(world, path).ok_or_else(|| missing("biome_unknown", format!("no biome file `{path}`")))?;
    let def = match BiomeDef::from_ron(&text) {
        Ok(d) => d,
        Err(regia_procgen::biome::BiomeDefError::Invalid(issues)) => return Err(invalid(&issues)),
        Err(e) => return Err(bad(e.code(), format!("`{path}`: {e}"))),
    };
    let eco_path = ecology.map_or_else(|| ecology_path_for(path), str::to_string);
    let profile = match eco_text(world, &eco_path) {
        Some(t) => Some(
            EcologyProfile::from_ron(&t).map_err(|e| bad("ecology_invalid", format!("`{eco_path}`: {e}")))?,
        ),
        None if ecology.is_some() => {
            return Err(missing("biome_unknown", format!("no ecology file `{eco_path}`")));
        }
        None => None,
    };
    let catalog = profile
        .as_ref()
        .map_or_else(|| SpeciesCatalog::builtin().clone(), EcologyProfile::catalog);
    let report = validate_ecology(&def, profile.as_ref(), &catalog);
    Ok(ecology_json(
        path,
        profile.as_ref().map(|_| eco_path.as_str()),
        &report,
    ))
}

/// `biome.validate_ecology { path?, ecology?, presets? }` (ADR-2456): whether each biome's species could
/// grow together on its site, with the reason for every implausible mix.
pub(crate) fn validate_ecology(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| bad("biome_invalid", "`path` must be a .biome.ron path"))?
                .to_string(),
        ),
    };
    let ecology = match params.get("ecology") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| bad("biome_invalid", "`ecology` must be an .ecology.ron path"))?
                .to_string(),
        ),
    };
    if ecology.is_some() && path.is_none() {
        return Err(bad("biome_invalid", "`ecology` needs `path`"));
    }
    let presets = match params.get("presets") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| bad("biome_invalid", "`presets` must be a bool"))?,
    };
    let mut paths: Vec<String> = Vec::new();
    match &path {
        Some(p) => paths.push(p.clone()),
        None if !presets => {
            // Every biome the level's areas name.
            available(world)?;
            let mut q = world.query::<&BiomeArea>();
            let mut set = std::collections::BTreeSet::new();
            for a in q.iter(world) {
                set.extend(a.biome.iter().cloned());
                set.extend(a.map_biomes.iter().cloned());
            }
            paths.extend(set);
        }
        None => {}
    }
    if presets {
        paths.extend(
            regia_procgen::biome::ecology::PRESETS
                .iter()
                .map(|p| p.path.to_string()),
        );
    }
    let mut results = Vec::with_capacity(paths.len());
    for p in &paths {
        results.push(validate_one(world, p, ecology.as_deref())?);
    }
    let plausible = results.iter().all(|r| r["plausible"] == json!(true));
    Ok(json!({ "results": results, "plausible": plausible }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::*;
    use regia_procgen::world::{ProcBounds, ProcGenPlugin, ProcGenTerrain, run_now};
    use regia_scene::{GlobalTransform, Transform};

    const MEADOW: &str = r#"(
        name: "Meadow",
        priority: 10,
        layers: [(
            name: "flowers",
            source: Ground(spacing_m: 2.0, jitter: 0.5),
            entries: [
                (name: "flower", spawn: Mesh("flower.glb"), weight: 3.0,
                 filters: [Altitude(min: -100.0, max: 100.0)],
                 footprint: (core_m: Some(0.1), spread_m: Some(0.3))),
            ],
        )],
    )"#;

    fn app() -> App {
        let mut files = BTreeMap::new();
        files.insert(
            "biomes.pgraph.ron".to_string(),
            regia_procgen::biome::STARTER_BIOMES_GRAPH.to_string(),
        );
        files.insert("meadow.biome.ron".to_string(), MEADOW.to_string());
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let map = regia_terrain::TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], 128.0, 1.0, 0.0)
            .expect("map");
        app.insert_resource(ProcGenTerrain(map));
        app
    }

    fn reason_of(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn setup(app: &mut App) -> (Entity, Entity) {
        let g = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: regia_core::math::Vec3::new(40.0, 20.0, 40.0),
                    },
                    ..ProcGenerator::new("biomes.pgraph.ron")
                },
                EntityGuid(Guid(1)),
                Transform::default(),
                GlobalTransform::default(),
            ))
            .id();
        let a = app
            .world_mut()
            .spawn((
                BiomeArea::new("meadow.biome.ron"),
                EntityGuid(Guid(2)),
                Transform {
                    scale: regia_core::math::Vec3::new(40.0, 10.0, 40.0),
                    ..Transform::default()
                },
            ))
            .id();
        (g, a)
    }

    #[test]
    fn audit_claim_requires_exactly_three_finite_f32_coordinates() {
        let mut app = app();
        let (_, area) = setup(&mut app);
        app.world_mut()
            .get_mut::<Transform>(area)
            .expect("area")
            .translation = regia_core::math::Vec3::new(10.0, 0.0, 20.0);
        app.world_mut().get_mut::<Transform>(area).expect("area").scale =
            regia_core::math::Vec3::new(4.0, 10.0, 4.0);
        let w = app.world_mut();
        let valid = claim(w, &json!({ "point": [10, 0, 20] })).expect("claim");
        assert_eq!(valid["owner"]["area"], area.to_bits());
        assert!(claim(w, &json!({ "point": [20, 0, 10] })).expect("outside")["owner"].is_null());
        for point in [
            json!([10, "oops", 0, 20]),
            json!([10, null, 0]),
            json!([10, 0, 20, 0]),
            json!([10, 1e308, 20]),
            json!([1e308, 0, 20]),
            json!([10, 0, -1e308]),
        ] {
            let err = claim(w, &json!({ "point": point })).expect_err("invalid point");
            assert_eq!(err.code, INVALID_PARAMS);
            assert_eq!(reason_of(&err).as_deref(), Some("biome_invalid"));
        }
        assert_eq!(
            claim(w, &json!({ "point": [10, 0, 20] })).expect("unchanged"),
            valid
        );
    }

    #[test]
    fn areas_claim_and_stats_after_a_run() {
        let mut app = app();
        let (g, a) = setup(&mut app);
        let w = app.world_mut();
        run_now(w, g);
        let areas = areas(w, &json!({ "generator": g.to_bits() })).expect("areas");
        assert_eq!(areas["areas"][0]["entity"], json!(a.to_bits()));
        assert_eq!(areas["areas"][0]["shape"], "shape");
        assert!(areas["areas"][0]["biomes"][0]["layers"]["flowers"]["placed"].as_u64() > Some(0));
        let c = claim(w, &json!({ "point": [0.0, 0.0, 0.0] })).expect("claim");
        assert_eq!(c["owner"]["biome"], "Meadow");
        assert_eq!(c["owner"]["area"], json!(a.to_bits()));
        assert_eq!(c["owner"]["weight"], json!(1.0));
        assert_eq!(c["second"], Value::Null);
        let none = claim(w, &json!({ "point": [100.0, 0.0, 0.0] })).expect("claim");
        assert_eq!(none["owner"], Value::Null);
        let s = stats(w, &json!({ "generator": g.to_bits() })).expect("stats");
        let e = &s["entries"]["Meadow/flowers/flower"];
        assert!(e["placed"].as_u64() > Some(0));
        assert_eq!(e["assigned"], e["candidates"]);
        assert!(e["removed_by_filter"].get("0:Altitude").is_some());
        // Errors.
        let err = stats(w, &json!({ "generator": a.to_bits() })).expect_err("not a generator");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_not_generator"));
        let err = claim(w, &json!({ "point": [1.0] })).expect_err("bad point");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_invalid"));
    }

    #[test]
    fn def_get_and_def_edit_with_pointers_and_entry_ops() {
        let mut app = app();
        let w = app.world_mut();
        let d = def_get(w, &json!({ "path": "meadow.biome.ron" })).expect("get");
        assert_eq!(
            d["def"]["layers"][0]["entries"][0]["render"]["draw_distance_m"],
            json!(300.0)
        );
        let err = def_get(w, &json!({ "path": "nope.biome.ron" })).expect_err("missing");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_unknown"));
        let r = def_edit(
            w,
            &json!({ "path": "meadow.biome.ron", "ops": [
                { "op": "set", "pointer": "/priority", "value": 42 },
                { "op": "add_entry", "layer": "flowers", "entry": { "name": "stone", "spawn": { "Mesh": "stone.glb" } } },
            ] }),
        )
        .expect("edit");
        assert_eq!(r["applied"], json!(true));
        let d = def_get(w, &json!({ "path": "meadow.biome.ron" })).expect("get");
        assert_eq!(d["def"]["priority"], json!(42));
        assert_eq!(d["def"]["layers"][0]["entries"][1]["name"], "stone");
        let err = def_edit(
            w,
            &json!({ "path": "meadow.biome.ron", "ops": [{ "op": "set", "pointer": "/nope/x", "value": 1 }] }),
        )
        .expect_err("pointer");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_pointer_invalid"));
        let err = def_edit(
            w,
            &json!({ "path": "meadow.biome.ron", "ops": [{ "op": "remove_entry", "layer": "flowers", "name": "ghost" }] }),
        )
        .expect_err("unknown entry");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_entry_unknown"));
        // An edit that breaks a rule is answered with the errors and not applied.
        let r = def_edit(
            w,
            &json!({ "path": "meadow.biome.ron", "ops": [
                { "op": "set", "pointer": "/layers/0/entries/0/children", "value": [{ "entry": "ghost" }] },
            ] }),
        )
        .expect("edit answered");
        assert_eq!(r["applied"], json!(false));
        assert_eq!(r["errors"][0]["reason"], "biome_entry_unknown");
    }

    #[test]
    fn preview_toggles_the_area_flag() {
        let mut app = app();
        let (g, a) = setup(&mut app);
        let w = app.world_mut();
        let r = preview(w, &json!({ "area": a.to_bits(), "on": true })).expect("preview");
        assert_eq!(r["preview_alone"], json!(true));
        assert!(w.get::<BiomeArea>(a).is_some_and(|x| x.preview_alone));
        let err = preview(w, &json!({ "area": g.to_bits(), "on": true })).expect_err("not an area");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_unknown"));
    }

    /// `save: true` writes only inside the project (ADR-1816, ADR-1875 (11)): an absolute path held
    /// in the unsaved overlay is refused with `biome_invalid` and nothing is written outside; a
    /// project-relative path is saved under the root.
    #[test]
    fn def_edit_save_refuses_a_path_outside_the_project() {
        let root = regia_test_utils::unique_temp_dir("biome-save-root");
        let outside = regia_test_utils::unique_temp_dir("biome-save-outside");
        std::fs::write(root.join("meadow.biome.ron"), MEADOW).expect("meadow");
        let target = outside.join("evil.biome.ron");
        let abs = target.to_string_lossy().into_owned();
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Dir(root.clone()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.insert_resource(ProcGraphOverlay(BTreeMap::from([(
            abs.clone(),
            MEADOW.to_string(),
        )])));
        let w = app.world_mut();
        let op = json!([{ "op": "set", "pointer": "/priority", "value": 7 }]);
        let err = def_edit(w, &json!({ "path": abs, "ops": op, "save": true })).expect_err("outside");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_invalid"));
        // `/tmp/...` on Unix, `C:\...` on Windows.
        assert!(
            ["path_absolute", "path_prefix"]
                .iter()
                .any(|c| err.message.contains(c)),
            "{}",
            err.message
        );
        assert!(!target.exists(), "nothing written outside the project");
        let r = def_edit(w, &json!({ "path": "meadow.biome.ron", "ops": op, "save": true })).expect("inside");
        assert_eq!(r["saved"], json!(true));
        let saved = std::fs::read_to_string(root.join("meadow.biome.ron")).expect("saved");
        assert!(saved.contains("priority: 7"), "{saved}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// ADR-2456: the shipped presets validate clean; a birch woodland with oak and maple added is
    /// implausible with the reasons; a level's areas are validated by name without a profile.
    #[test]
    fn validate_ecology_reports_presets_issues_and_level_biomes() {
        let mut app = app();
        let w = app.world_mut();
        let r = validate_ecology(w, &json!({ "presets": true })).expect("presets");
        assert_eq!(r["plausible"], json!(true), "{r:#}");
        assert_eq!(r["results"].as_array().map(Vec::len), Some(6));
        let birch = &r["results"][0];
        assert_eq!(birch["path"], "starter/biomes/birch_woodland.biome.ron");
        assert_eq!(birch["ecology"], "starter/biomes/birch_woodland.ecology.ron");
        assert_eq!(birch["species"][0]["role"], "dominant");
        // Oak and maple in the birch woodland, as an unsaved overlay of the preset.
        let mut text = regia_procgen::biome::ecology::PRESETS[0].biome.to_string();
        text = text.replacen(
            "entries: [",
            "entries: [(name: \"oak\", spawn: Mesh(\"trees/oak.glb\"), weight: 0.0), (name: \"maple\", spawn: Mesh(\"trees/maple.glb\"), weight: 0.0),",
            1,
        );
        w.insert_resource(ProcGraphOverlay(BTreeMap::from([(
            "starter/biomes/birch_woodland.biome.ron".to_string(),
            text,
        )])));
        let r =
            validate_ecology(w, &json!({ "path": "starter/biomes/birch_woodland.biome.ron" })).expect("one");
        assert_eq!(r["plausible"], json!(false));
        let issues = r["results"][0]["issues"].as_array().cloned().unwrap_or_default();
        let at = |p: &str| -> Vec<String> {
            issues
                .iter()
                .filter(|i| i["path"] == p)
                .filter_map(|i| i["reason"].as_str().map(str::to_string))
                .collect()
        };
        assert!(
            at("canopy/oak").contains(&"ecology_moisture".to_string()),
            "{issues:?}"
        );
        assert!(
            at("canopy/maple").contains(&"ecology_reaction".to_string()),
            "{issues:?}"
        );
        // The level's biomes: the meadow has no species, so it is plausible with everything unmapped.
        setup(&mut app);
        let r = validate_ecology(app.world_mut(), &json!({})).expect("level");
        assert_eq!(r["results"][0]["path"], "meadow.biome.ron");
        assert_eq!(r["results"][0]["site"], Value::Null);
        assert_eq!(r["results"][0]["unmapped"][0], "flowers/flower");
        // Errors.
        let w = app.world_mut();
        let err = validate_ecology(w, &json!({ "path": "nope.biome.ron" })).expect_err("unknown");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_unknown"));
        let err = validate_ecology(w, &json!({ "ecology": "x.ecology.ron" })).expect_err("needs path");
        assert_eq!(reason_of(&err).as_deref(), Some("biome_invalid"));
        w.insert_resource(ProcGraphOverlay(BTreeMap::from([(
            "meadow.ecology.ron".to_string(),
            "EcologyProfile(site: 3)".to_string(),
        )])));
        let err = validate_ecology(w, &json!({ "path": "meadow.biome.ron" })).expect_err("bad profile");
        assert_eq!(reason_of(&err).as_deref(), Some("ecology_invalid"));
    }
}
