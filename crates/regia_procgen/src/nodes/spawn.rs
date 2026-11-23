// SPDX-License-Identifier: MPL-2.0
//! Spawning and outputs (`spawn.*`, `out.*`, spec 63 § 13).

use regia_core::math::Vec3;

use crate::rot::Rot as Quat;

use crate::data::{AttrColumn, Bundle, DataItem, Payload, Table, quat};
use crate::exec::{Collision, FiniteCheck, Instance, InstanceSet, NodeCtx, PrefabSpawn};
use crate::nodes::common::{euler_deg, read_num};
use crate::nodes::{
    ANY, BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, pin_req, pins_points_inout, set,
};
use crate::value::SettingValue;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::in_place(
        "spawn.mesh",
        "spawn",
        "Picks a mesh entry per point (Weighted, ByAttr, GroupThenWeighted) and adds instances to the output",
        &[
            set(
                "pick",
                T::Any,
                "Weighted",
                "Weighted | ByAttr(attr: \"kind\") | GroupThenWeighted(group_attr: \"biome\")",
            ),
            set("entries", T::Any, "[]", "[(mesh: \"a.glb\", weight: 1.0, ...)]"),
            set(
                "write_attr",
                T::Name,
                "\"mesh\"",
                "Attribute that records the chosen mesh",
            ),
            set(
                "color_from",
                T::Name,
                "\"color\"",
                "Per-instance tint selector (empty = white)",
            ),
            set(
                "custom",
                T::NameList,
                "[]",
                "Up to 4 float attributes passed as custom data",
            ),
            set(
                "target_layer",
                T::Any,
                "None",
                "Data layer name (spec 37): the instance sets join it and draw only while it is Live",
            ),
        ],
        pins_points_inout,
        eval_mesh,
    )
    .uncached()
    .world()
    .checked(check_mesh),
    BuiltinNode::new(
        "spawn.prefab",
        "spawn",
        "One prefab instance per point with density > 0, under the generator's output root",
        &[
            set("prefab", T::Asset, "\"\"", "Prefab asset (or use pick + entries)"),
            set("pick", T::Any, "Weighted", "As spawn.mesh"),
            set(
                "entries",
                T::Any,
                "[]",
                "[(prefab: \"p.prefab.ron\", weight: 1.0, key: \"\", group: \"\")]",
            ),
            set(
                "name",
                T::Name,
                "\"spawned_{index}\"",
                "Entity name pattern ({index})",
            ),
            set("tags", T::NameList, "[]", "Tags added to spawned entities"),
            set(
                "overrides",
                T::Any,
                "[]",
                "[(attr: \"color\", path: \"Component.field\")]: point attribute → reflected field of each instance",
            ),
        ],
        pins_points_inout,
        eval_prefab,
    )
    .uncached()
    .world()
    .checked(check_prefab),
    BuiltinNode::new(
        "out.table",
        "out",
        "Exports tables (points become tables) under a name",
        &[set("name", T::Name, "\"table\"", "Export name")],
        |_| PinSpec::new(vec![pin_req("in", ANY)], vec![]),
        eval_out_table,
    )
    .uncached(),
    BuiltinNode::new(
        "out.points",
        "out",
        "Exports points under a name (readable by other generators)",
        &[set("name", T::Name, "\"points\"", "Export name")],
        |_| PinSpec::new(vec![pin_req("in", POINTS)], vec![]),
        eval_out_points,
    )
    .uncached(),
];

/// The pick rule.
#[derive(Clone, Debug, PartialEq)]
enum Pick {
    Weighted,
    ByAttr(String),
    GroupThenWeighted(String),
}

fn parse_pick(v: Option<&SettingValue>) -> Result<Pick, String> {
    match v {
        None => Ok(Pick::Weighted),
        Some(SettingValue::Ident(n)) | Some(SettingValue::Str(n)) if n == "Weighted" => Ok(Pick::Weighted),
        Some(SettingValue::Record(Some(n), f)) if n == "ByAttr" => f
            .iter()
            .find(|(k, _)| k == "attr")
            .and_then(|(_, v)| v.as_text())
            .map(|a| Pick::ByAttr(a.to_string()))
            .ok_or_else(|| "ByAttr needs `attr`".to_string()),
        Some(SettingValue::Record(Some(n), f)) if n == "GroupThenWeighted" => f
            .iter()
            .find(|(k, _)| k == "group_attr")
            .and_then(|(_, v)| v.as_text())
            .map(|a| Pick::GroupThenWeighted(a.to_string()))
            .ok_or_else(|| "GroupThenWeighted needs `group_attr`".to_string()),
        Some(other) => Err(format!("unknown pick `{}`", other.write())),
    }
}

/// One spawn entry (mesh or prefab).
#[derive(Clone, Debug)]
struct Entry {
    asset: String,
    weight: f64,
    group: String,
    key: String,
    proto: InstanceSet,
    offset: (Vec3, Quat, Vec3),
    enabled: bool,
}

fn parse_entries(v: Option<&SettingValue>, asset_field: &str) -> Result<Vec<Entry>, String> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let items = v.as_items().ok_or("`entries` must be a list")?;
    items
        .iter()
        .map(|e| {
            let asset = e
                .field(asset_field)
                .and_then(SettingValue::as_text)
                .ok_or_else(|| format!("entry needs `{asset_field}`"))?
                .to_string();
            let num = |k: &str, d: f64| e.field(k).and_then(SettingValue::as_f64).unwrap_or(d);
            let text = |k: &str| {
                e.field(k)
                    .and_then(SettingValue::as_text)
                    .unwrap_or("")
                    .to_string()
            };
            let collision = match e.field("collision").and_then(SettingValue::as_text) {
                None | Some("None") => Collision::None,
                Some("Simple") => Collision::Simple,
                Some("Mesh") => Collision::Mesh,
                Some(o) => return Err(format!("unknown collision `{o}`")),
            };
            let material = match e.field("material") {
                Some(SettingValue::Call(n, a)) if n == "Some" => {
                    a.first().and_then(SettingValue::as_text).map(str::to_string)
                }
                Some(SettingValue::Str(s)) => Some(s.clone()),
                _ => None,
            };
            let off = e.field("offset");
            let ov = |k: &str, d: [f64; 3]| {
                off.and_then(|o| o.field(k))
                    .and_then(SettingValue::as_vec3)
                    .unwrap_or(d)
            };
            let weight = num("weight", 1.0);
            if !weight.is_finite() {
                return Err("entry weight must be finite".to_string());
            }
            // Every other number lands in instances and `.pgout` bakes, which the run's `attr_nan`
            // gate does not see: check them here, after the f32 narrowing (audit M7).
            for (k, d) in [
                ("draw_distance_m", 300.0),
                ("fade_m", 20.0),
                ("cull_below_px", 2.0),
            ] {
                if !(num(k, d) as f32).is_finite() {
                    return Err(format!("entry `{k}` must be finite"));
                }
            }
            for (k, d) in [("t", [0.0; 3]), ("r_deg", [0.0; 3]), ("s", [1.0; 3])] {
                if ov(k, d).iter().any(|x| !(*x as f32).is_finite()) {
                    return Err(format!("entry `offset.{k}` must be finite"));
                }
            }
            Ok(Entry {
                asset: asset.clone(),
                weight,
                group: text("group"),
                key: text("key"),
                proto: InstanceSet {
                    mesh: asset,
                    material,
                    cast_shadows: e
                        .field("cast_shadows")
                        .and_then(SettingValue::as_bool)
                        .unwrap_or(true),
                    collision,
                    draw_distance_m: num("draw_distance_m", 300.0) as f32,
                    fade_m: num("fade_m", 20.0) as f32,
                    cull_below_px: num("cull_below_px", 2.0) as f32,
                    instances: Vec::new(),
                    layer: None,
                },
                offset: (
                    Vec3::from(ov("t", [0.0; 3]).map(|x| x as f32)),
                    euler_deg(ov("r_deg", [0.0; 3])),
                    Vec3::from(ov("s", [1.0; 3]).map(|x| x as f32)),
                ),
                enabled: e.field("enabled").and_then(SettingValue::as_bool).unwrap_or(true),
            })
        })
        .collect()
}

fn check_mesh(s: &Settings) -> Vec<(String, String)> {
    let mut e = Vec::new();
    if let Err(m) = parse_pick(s.get("pick")) {
        e.push(("setting_type".to_string(), m));
    }
    match parse_entries(s.get("entries"), "mesh") {
        Err(m) => e.push(("setting_type".to_string(), m)),
        Ok(list) if list.iter().any(|x| x.weight < 0.0) => {
            e.push((
                "setting_out_of_range".to_string(),
                "entry weights must be >= 0".to_string(),
            ));
        }
        Ok(_) => {}
    }
    if s.names("custom").len() > 4 {
        e.push((
            "setting_out_of_range".to_string(),
            "`custom` takes at most 4 attributes".to_string(),
        ));
    }
    if let Err(m) = target_layer(s.get("target_layer")) {
        e.push(("setting_type".to_string(), m));
    }
    e
}

/// The `target_layer` setting: `None` (or absent, or `""`) is no layer; a name, `"name"` or
/// `Some("name")` is that layer. A name that is not a valid layer name is refused.
fn target_layer(v: Option<&SettingValue>) -> Result<Option<String>, String> {
    let name = match v {
        None => return Ok(None),
        Some(SettingValue::Ident(i)) if i == "None" => return Ok(None),
        Some(SettingValue::Call(c, args)) if c == "Some" && args.len() == 1 => args[0].as_text(),
        Some(other) => other.as_text(),
    };
    match name {
        Some("") => Ok(None),
        Some(n) if regia_scene::layers::valid_layer_name(n) => Ok(Some(n.to_string())),
        Some(n) => Err(format!("`target_layer` `{n}` is not a valid layer name")),
        None => Err("`target_layer` is a layer name or None".to_string()),
    }
}

fn check_prefab(s: &Settings) -> Vec<(String, String)> {
    let mut e = Vec::new();
    if let Err(m) = parse_pick(s.get("pick")) {
        e.push(("setting_type".to_string(), m));
    }
    if let Err(m) = parse_entries(s.get("entries"), "prefab") {
        e.push(("setting_type".to_string(), m));
    }
    if let Err((code, m)) = parse_overrides(s.get("overrides")) {
        e.push((code.to_string(), m));
    }
    e
}

/// One `overrides` row of `spawn.prefab` (spec 63 § 13.2): the attribute selector and the
/// component and field it drives.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OverrideSpec {
    pub(crate) attr: crate::data::Selector,
    pub(crate) component: String,
    pub(crate) field: String,
}

/// Splits `Component.field.path` (the component may be a full type path with `::`): the component
/// ends at the first `.` after its last `::`. Field segments may index elements (`tint[1]`).
pub(crate) fn split_override_path(path: &str) -> Option<(&str, &str)> {
    let tail_start = path.rfind("::").map_or(0, |k| k + 2);
    let dot = tail_start + path[tail_start..].find('.')?;
    let (component, field) = (&path[..dot], &path[dot + 1..]);
    let ident = |t: &str| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let component_ok = component.split("::").all(ident);
    // A segment is a name or an index, optionally followed by element indices (`tint[1]`, the
    // `reflect_path` form the apply side resolves).
    let segment_ok = |f: &str| {
        let mut base = f;
        while let Some((inner, k)) = base.strip_suffix(']').and_then(|b| b.rfind('[').map(|k| (b, k))) {
            if inner[k + 1..].parse::<usize>().is_err() {
                return false;
            }
            base = &inner[..k];
        }
        ident(base) || base.parse::<usize>().is_ok()
    };
    let field_ok = field.split('.').all(segment_ok);
    (component_ok && field_ok).then_some((component, field))
}

/// Parses the `overrides` setting: `setting_type` for a malformed row, `override_path_unknown` for a
/// path that is not `Component.field` (fields the prefab lacks are reported when the instance exists,
/// ADR-2724).
pub(crate) fn parse_overrides(v: Option<&SettingValue>) -> Result<Vec<OverrideSpec>, (&'static str, String)> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let items = v
        .as_items()
        .ok_or(("setting_type", "`overrides` must be a list".to_string()))?;
    items
        .iter()
        .map(|row| {
            let text = |k: &str| row.field(k).and_then(SettingValue::as_text);
            let (Some(attr), Some(path)) = (text("attr"), text("path")) else {
                return Err((
                    "setting_type",
                    "each override needs `attr` and `path`: (attr: \"color\", path: \"Component.field\")"
                        .to_string(),
                ));
            };
            let attr = crate::data::Selector::parse(attr)
                .map_err(|m| ("setting_type", format!("override attr `{attr}`: {m}")))?;
            let (component, field) = split_override_path(path).ok_or((
                "override_path_unknown",
                format!("override path `{path}` is not `Component.field`"),
            ))?;
            Ok(OverrideSpec {
                attr,
                component: component.to_string(),
                field: field.to_string(),
            })
        })
        .collect()
}

/// Per override: the attribute's value at every point of `item`.
fn override_columns(
    item: &DataItem,
    specs: &[OverrideSpec],
) -> Result<Vec<Vec<crate::data::AttrValue>>, NodeError> {
    use crate::data::AttrValue as V;
    let Payload::Points(p) = &item.payload else {
        return Ok(Vec::new());
    };
    specs
        .iter()
        .map(|spec| {
            // A whole attribute column of any type (names, booleans) reads as is; anything else
            // (built-ins, components like `position:x`, item scope) through the numeric reader.
            if !spec.attr.item_scope
                && spec.attr.parts.is_empty()
                && let Some(col) = p.attrs.get(&spec.attr.name)
            {
                return Ok((0..p.len())
                    .map(|i| col.get(i).unwrap_or(V::Float(0.0)))
                    .collect());
            }
            let num = read_num(item, &spec.attr)?;
            Ok((0..p.len())
                .map(|i| {
                    let v = num.get(i);
                    match v {
                        [x] => V::Float(*x),
                        [x, y] => V::Vec2([*x, *y]),
                        [x, y, z] => V::Vec3([*x, *y, *z]),
                        [x, y, z, w] => V::Vec4([*x, *y, *z, *w]),
                        _ => V::Float(v.first().copied().unwrap_or(0.0)),
                    }
                })
                .collect())
        })
        .collect()
}

/// Chooses an entry index for element `i` (spec 63 § 13.1). `None` = no spawn.
fn choose(
    ctx: &mut NodeCtx<'_>,
    pick: &Pick,
    entries: &[Entry],
    usable: &[usize],
    seed: u32,
    key: Option<&str>,
) -> Option<usize> {
    let weighted = |ctx: &NodeCtx<'_>, pool: &[usize]| -> Option<usize> {
        let total: f64 = pool.iter().map(|&k| entries[k].weight).sum();
        if total <= 0.0 {
            return None;
        }
        let u = ctx.draw(seed, 0) * total;
        let mut acc = 0.0;
        for &k in pool {
            acc += entries[k].weight;
            if u < acc {
                return Some(k);
            }
        }
        pool.last().copied()
    };
    match pick {
        Pick::Weighted => {
            let r = weighted(ctx, usable);
            if r.is_none() {
                ctx.warn("no_spawn_weight", "every entry has weight 0 or is disabled");
            }
            r
        }
        Pick::ByAttr(_) => {
            let k = key?;
            let r = (0..entries.len()).find(|j| entries[*j].enabled && entries[*j].key == k);
            if r.is_none() {
                ctx.count("unmatched", 1);
            }
            r
        }
        Pick::GroupThenWeighted(_) => {
            let k = key?;
            let pool: Vec<usize> = usable
                .iter()
                .copied()
                .filter(|j| entries[*j].group == k)
                .collect();
            let r = weighted(ctx, &pool);
            if r.is_none() {
                ctx.count("unmatched", 1);
            }
            r
        }
    }
}

/// The entries a weighted pick may choose, in declaration order.
fn usable_entries(entries: &[Entry]) -> Vec<usize> {
    (0..entries.len())
        .filter(|k| entries[*k].enabled && entries[*k].weight > 0.0)
        .collect()
}

/// The key text per element for ByAttr / GroupThenWeighted.
fn keys(item: &DataItem, pick: &Pick) -> Result<Option<Vec<String>>, NodeError> {
    let attr = match pick {
        Pick::Weighted => return Ok(None),
        Pick::ByAttr(a) | Pick::GroupThenWeighted(a) => a,
    };
    let Payload::Points(p) = &item.payload else {
        return Ok(None);
    };
    let col = p
        .attrs
        .get(attr)
        .ok_or_else(|| NodeError::new("attr_missing", format!("points have no attribute `{attr}`")))?;
    Ok(Some(
        (0..p.len())
            .map(|i| col.get(i).map(|v| v.key_text()).unwrap_or_default())
            .collect(),
    ))
}

fn eval_mesh(ctx: &mut NodeCtx<'_>, inputs: &mut [Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut out = crate::nodes::take_input(inputs, 0);
    let pick = parse_pick(s.get("pick")).map_err(|m| NodeError::new("setting_type", m))?;
    let mut entries =
        parse_entries(s.get("entries"), "mesh").map_err(|m| NodeError::new("setting_type", m))?;
    // `target_layer`: every set this node fills joins that data layer (spec 63 § 13.1, ADR-7214).
    let layer = target_layer(s.get("target_layer")).map_err(|m| NodeError::new("setting_type", m))?;
    for entry in &mut entries {
        entry.proto.layer.clone_from(&layer);
    }
    let usable = usable_entries(&entries);
    // The node writes no float column: `write_attr` is an asset name.
    ctx.finite_check = FiniteCheck::Nothing;
    let write_attr = s.text("write_attr").to_string();
    let color_sel = s.text("color_from").to_string();
    let custom = s.names("custom");
    // The instance set of each entry, looked up once per node instead of once per point.
    let mut set_of: Vec<Option<usize>> = vec![None; entries.len()];
    for item in &mut out.0 {
        let keys = keys(item, &pick)?;
        let colors = if color_sel.is_empty() || color_sel == "color" {
            None
        } else {
            Some(read_num(
                item,
                &crate::data::Selector::parse(&color_sel).map_err(|m| NodeError::new("setting_type", m))?,
            )?)
        };
        let customs = custom
            .iter()
            .map(|c| {
                read_num(
                    item,
                    &crate::data::Selector::parse(c).map_err(|m| NodeError::new("setting_type", m))?,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let Payload::Points(p) = &mut item.payload else {
            continue;
        };
        let records = !write_attr.is_empty();
        let mut chosen = if records {
            vec![String::new(); p.len()]
        } else {
            Vec::new()
        };
        for i in 0..p.len() {
            if p.density[i] <= 0.0 {
                continue;
            }
            let key = keys.as_ref().map(|k| k[i].as_str());
            let Some(k) = choose(ctx, &pick, &entries, &usable, p.seed[i], key) else {
                continue;
            };
            let e = &entries[k];
            let (pq, ps, pt) = (
                quat(p.rotation[i]),
                Vec3::from(p.scale[i]),
                Vec3::from(p.position[i]),
            );
            let (ot, oq, os) = e.offset;
            let color = match &colors {
                Some(c) => {
                    let v = c.get(i);
                    std::array::from_fn(|k| v.get(k).copied().unwrap_or(1.0) as f32)
                }
                None if color_sel.is_empty() => [1.0; 4],
                None => p.color[i],
            };
            let mut cdata = [0.0f32; 4];
            for (slot, col) in cdata.iter_mut().zip(&customs) {
                *slot = col.scalar(i) as f32;
            }
            let set = match set_of[k] {
                Some(set) => set,
                None => {
                    let set = ctx.output().set_index_for(&e.proto);
                    set_of[k] = Some(set);
                    set
                }
            };
            ctx.output().instance_sets[set].instances.push(Instance {
                translation: (pt + pq * (ps * ot)).to_array(),
                rotation: (pq * oq).normalize().to_array(),
                scale: (ps * os).to_array(),
                color,
                custom: cdata,
            });
            if records {
                chosen[i] = e.asset.clone();
            }
        }
        if records {
            p.attrs
                .insert(write_attr.clone(), AttrColumn::Asset(chosen.into()));
            item.last_written = Some(write_attr.clone());
        }
    }
    Ok(vec![out])
}

fn eval_prefab(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let pick = parse_pick(s.get("pick")).map_err(|m| NodeError::new("setting_type", m))?;
    let mut entries =
        parse_entries(s.get("entries"), "prefab").map_err(|m| NodeError::new("setting_type", m))?;
    let single = s.text("prefab").to_string();
    if entries.is_empty() && !single.is_empty() {
        entries = parse_entries(
            Some(&SettingValue::List(vec![SettingValue::Record(
                None,
                vec![("prefab".to_string(), SettingValue::Str(single))],
            )])),
            "prefab",
        )
        .map_err(|m| NodeError::new("setting_type", m))?;
    }
    let pattern = s.text("name").to_string();
    let tags = s.names("tags");
    let specs = parse_overrides(s.get("overrides")).map_err(|(code, m)| NodeError::new(code, m))?;
    let usable = usable_entries(&entries);
    let cap = ctx.limits().prefabs_per_generator;
    let out = crate::nodes::input(inputs, 0);
    for item in &out.0 {
        let keys = keys(item, &pick)?;
        let columns = override_columns(item, &specs)?;
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        for i in 0..p.len() {
            if p.density[i] <= 0.0 {
                continue;
            }
            let key = keys.as_ref().map(|k| k[i].as_str());
            let Some(k) = choose(ctx, &pick, &entries, &usable, p.seed[i], key) else {
                continue;
            };
            let index = ctx.output().prefabs.len();
            if index >= cap {
                return Err(NodeError::new(
                    "limit_exceeded",
                    format!("prefabs_per_generator: more than {cap}"),
                ));
            }
            let spawn = PrefabSpawn {
                prefab: entries[k].asset.clone(),
                name: pattern.replace("{index}", &index.to_string()),
                translation: p.position[i],
                rotation: p.rotation[i],
                scale: p.scale[i],
                tags: tags.clone(),
                overrides: specs
                    .iter()
                    .zip(&columns)
                    .map(|(spec, col)| crate::exec::PrefabFieldOverride {
                        component: spec.component.clone(),
                        field: spec.field.clone(),
                        value: col[i].clone(),
                    })
                    .collect(),
            };
            ctx.output().prefabs.push(spawn);
        }
    }
    Ok(vec![out])
}

fn eval_out_table(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let name = s.text("name").to_string();
    let mut merged: Option<Table> = None;
    for item in crate::nodes::input_items(inputs, 0) {
        let t = match &item.payload {
            Payload::Table(t) => t.clone(),
            Payload::Points(p) => Table {
                rows: p.len(),
                columns: p.attrs.iter().map(|(k, c)| (k.clone(), c.clone())).collect(),
            },
            _ => continue,
        };
        merged = Some(match merged {
            None => t,
            Some(mut m) => {
                for (k, c) in &mut m.columns {
                    let other = t.column(k);
                    if !c.extend_from(other, t.rows) {
                        return Err(NodeError::new(
                            "attr_type_conflict",
                            format!("column `{k}` has two types"),
                        ));
                    }
                }
                m.rows += t.rows;
                m
            }
        });
    }
    ctx.output().tables.insert(name, merged.unwrap_or_default());
    Ok(vec![])
}

fn eval_out_points(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let name = s.text("name").to_string();
    let mut merged = crate::data::PointSet::new();
    for item in crate::nodes::input_items(inputs, 0) {
        if let Payload::Points(p) = &item.payload {
            merged.append(p).map_err(|n| {
                NodeError::new("attr_type_conflict", format!("attribute `{n}` has two types"))
            })?;
        }
    }
    ctx.output().points.insert(name, merged);
    Ok(vec![])
}
