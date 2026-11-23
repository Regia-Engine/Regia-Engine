// SPDX-License-Identifier: MPL-2.0
//! Meshing and dressing plants (spec 68 §§ 6-7): `plant.mesh` (generalized cylinders with
//! parallel-transport frames), `plant.leaves` (leaves and cards by site rules) and `plant.export`
//! (records an [`ExportJob`] for the host). Pipeline builds only.
//!
//! Ring frames use the projection form of parallel transport (Bishop, "There is more than one way
//! to frame a curve", 1975); leaves along branches follow the placement idea of Weber and Penn
//! (SIGGRAPH 1995). The wind layout is Regia's own (`docs/formats/plant-wind.md`). Original code.

use std::collections::HashMap;
use std::sync::Arc;

use regia_core::math::hash::mix;

use crate::data::{Bundle, DataItem, DataKind, KindSet, Payload};
use crate::detmath;
use crate::exec::NodeCtx;
use crate::nodes::common::{Combine, Region};
use crate::nodes::plant::{
    V3, add, cross, dot, len, mul, norm_or, perp_basis, rotate, skeleton_items, sub, tilt_toward,
};
use crate::nodes::{
    BuiltinNode, NodeError, PinSpec, SettingType as T, Settings, eval_curve, input, kind, num, pin_multi,
    pin_out, pin_req, set,
};
use crate::plant::{
    ExportCollision, ExportJob, ExportSettings, Plant, PlantMesh, PlantMeshSlot, PlantSkeleton, SlotKind,
};
use crate::value::SettingValue;

/// `PlantSkeleton | PlantMesh` (bits of `DataKind::ALL` order).
const SKEL_OR_MESH: KindSet =
    KindSet((1 << DataKind::PlantSkeleton as u16) | (1 << DataKind::PlantMesh as u16));

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "plant.mesh",
        "plant",
        "Sweeps a ring along every branch (parallel-transport frames, sides by radius); bark slots with wind data",
        &[
            num("side_len_m", T::Float, "0.04", 0.0001, 100.0, "Target ring edge length: sides = clamp(round(2 pi r / side_len_m), 3, 24)"),
            set("profile", T::Curve, "[]", "Periodic radius multiplier around the ring (u 0..1); empty = round"),
            num("noise_m", T::Float, "0.0", 0.0, 10.0, "Seeded radial noise amplitude"),
            num("bark_tile_m", T::Float, "1.0", 0.001, 1000.0, "V = distance along the branch / bark_tile_m"),
            set("materials", T::Any, "by_order([\"bark\"])", "by_order([..]) or by_radius([(max_r, material)..])"),
            num("min_radius_m", T::Float, "0.003", 0.0, 10.0, "Thinner branches are not meshed"),
        ],
        |_| PinSpec::new(vec![pin_req("in", SKEL_OR_MESH)], vec![pin_out("out", kind(DataKind::PlantMesh))]),
        eval_mesh,
    )
    .plant()
    .checked(check_materials),
    BuiltinNode::new(
        "plant.leaves",
        "plant",
        "Places leaf meshes or cards at branch sites; each site picks the choice whose rules score highest",
        &[
            set(
                "choices",
                T::Any,
                "[(card: true, material: \"leaf\", weight: 1.0, size_m: [0.06, 0.1], rules: [])]",
                "[(card: true | mesh: \"leaf.glb\", material, weight, size_m: [lo, hi], rules: [height(min, max, weight)..], leave_bare)]",
            ),
            set("where", T::Enum(&["tips", "along", "both"]), "both", "Sites at tips, along branches, or both"),
            set("orders", T::Range2, "[1.0, 16.0]", "Branch orders that get sites"),
            num("spacing_m", T::Float, "0.08", 0.001, 100.0, "Site spacing along branches"),
            num("per_branch_max", T::Int, "200", 0.0, 100_000.0, "Most sites per branch"),
            set("orient", T::Enum(&["sun", "up", "along", "random"]), "sun", "Leaf orientation"),
            num("orient_jitter_deg", T::Float, "15.0", 0.0, 180.0, "Random orientation jitter"),
            set("size_to_tip", T::Curve, "[]", "Size factor by distance from the branch tip (empty = 1)"),
            num("keep_clear_m", T::Float, "0.0", 0.0, 100.0, "Distance kept from leaves of earlier passes"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", SKEL_OR_MESH), pin_multi("mask", kind(DataKind::Shape))],
                vec![pin_out("out", kind(DataKind::PlantMesh))],
            )
        },
        eval_leaves,
    )
    .plant()
    .reads_world_data()
    .checked(check_choices),
    BuiltinNode::new(
        "plant.export",
        "plant",
        "Validates a meshed plant and records an export job (LODs, billboard, glTF) for the host to write",
        &[
            set("lods", T::Any, "[1.0, 0.5, 0.2, 0.05]", "Triangle ratio per LOD, LOD 0 first, decreasing in (0, 1]"),
            set("billboard", T::Bool, "true", "Impostor billboard at the last LOD"),
            set("lod_screen_size", T::Any, "[]", "Screen size per LOD (empty = convention defaults)"),
            num("shadow_lod", T::Int, "1", 0.0, 16.0, "LOD shadows use"),
            set("collision", T::Enum(&["none", "capsules"]), "none", "Capsules per trunk and order-1 branch over 5 cm"),
            set("out", T::Asset, "\"\"", "Output .glb path (empty = <name>.glb)"),
            set("name", T::Name, "\"plant\"", "Asset stem (<name>_LOD<n> nodes)"),
        ],
        |_| PinSpec::new(vec![pin_req("in", SKEL_OR_MESH)], vec![pin_out("out", kind(DataKind::PlantMesh))]),
        eval_export,
    )
    .plant()
    .uncached()
    .checked(check_export),
];

// ---------------------------------------------------------------------------------------------------
// Axes: a plant as chains of nodes. An axis starts at the root or at a node whose parent's main
// continuation (first child of the same order) is another node.

use crate::plant::PlantAxis as Axis;

fn axes(p: &Plant) -> Vec<Axis> {
    p.axes()
}

/// The axis polyline: the attachment point (inside the parent tube) then every node, with the axis
/// radius at each point.
fn polyline(p: &Plant, axis: &Axis) -> (Vec<V3>, Vec<f64>) {
    let mut pts = Vec::with_capacity(axis.nodes.len() + 1);
    let mut rad = Vec::with_capacity(axis.nodes.len() + 1);
    if let Some(a) = axis.attach {
        pts.push(p.nodes[a].position);
        rad.push(p.nodes[axis.nodes[0]].radius);
    }
    for &i in &axis.nodes {
        pts.push(p.nodes[i].position);
        rad.push(p.nodes[i].radius);
    }
    (pts, rad)
}

fn turn_deg(a: V3, b: V3, c: V3) -> f64 {
    let d1 = norm_or(sub(b, a), [0.0; 3]);
    let d2 = norm_or(sub(c, b), [0.0; 3]);
    detmath::acos(dot(d1, d2).clamp(-1.0, 1.0)).to_degrees()
}

/// Ring sides for a radius (spec 68 § 6.1).
pub(crate) fn ring_sides(radius: f64, side_len: f64) -> u32 {
    let s = (std::f64::consts::TAU * radius / side_len).round();
    s.clamp(3.0, 24.0) as u32
}

/// Per-branch wind values: `(phase, stiffness)`.
fn branch_wind(p: &Plant, start: usize) -> (f32, f32) {
    let phase = f64::from(mix(p.seed, start as u32)) / f64::from(u32::MAX);
    let trunk = p.nodes.first().map_or(1.0, |n| n.radius).max(1e-9);
    let stiff = (p.nodes[start].radius / trunk).clamp(0.0, 1.0);
    (phase as f32, stiff as f32)
}

// ---------------------------------------------------------------------------------------------------
// plant.mesh

/// Resolved `plant.mesh` settings.
pub(crate) struct MeshSettings {
    pub(crate) side_len_m: f64,
    pub(crate) profile: Vec<(f64, f64)>,
    pub(crate) noise_m: f64,
    pub(crate) bark_tile_m: f64,
    pub(crate) materials: Materials,
    pub(crate) min_radius_m: f64,
}

/// Bark material choice.
pub(crate) enum Materials {
    /// Indexed by branch order (clamped to the last).
    ByOrder(Vec<String>),
    /// First `(max_r, material)` with the branch radius ≤ `max_r`, else the last.
    ByRadius(Vec<(f64, String)>),
}

impl Materials {
    fn parse(v: Option<&SettingValue>) -> Result<Self, String> {
        let names = |l: &SettingValue| -> Vec<String> {
            l.as_items()
                .map(|xs| {
                    xs.iter()
                        .filter_map(|x| x.as_text().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        match v {
            None => Ok(Self::ByOrder(vec!["bark".into()])),
            Some(SettingValue::Call(n, a)) if n == "by_order" => {
                let l = a.first().map(names).unwrap_or_default();
                if l.is_empty() {
                    Err("by_order needs at least one material".into())
                } else {
                    Ok(Self::ByOrder(l))
                }
            }
            Some(SettingValue::Call(n, a)) if n == "by_radius" => {
                let mut out = Vec::new();
                for e in a.first().and_then(SettingValue::as_items).unwrap_or(&[]) {
                    let xs = e.as_items().unwrap_or(&[]);
                    let (Some(r), Some(m)) = (
                        xs.first().and_then(SettingValue::as_f64),
                        xs.get(1).and_then(SettingValue::as_text),
                    ) else {
                        return Err("by_radius entries are (max_r, material)".into());
                    };
                    out.push((r, m.to_string()));
                }
                if out.is_empty() {
                    Err("by_radius needs at least one entry".into())
                } else {
                    Ok(Self::ByRadius(out))
                }
            }
            Some(l @ SettingValue::List(_)) => {
                let l = names(l);
                if l.is_empty() {
                    Err("materials needs at least one material".into())
                } else {
                    Ok(Self::ByOrder(l))
                }
            }
            Some(_) => Err("materials is by_order([..]) or by_radius([(max_r, material)..])".into()),
        }
    }

    fn pick(&self, order: u32, radius: f64) -> &str {
        match self {
            Self::ByOrder(l) => l.get(order as usize).or(l.last()).map_or("bark", String::as_str),
            Self::ByRadius(l) => l
                .iter()
                .find(|(r, _)| radius <= *r)
                .or(l.last())
                .map_or("bark", |(_, m)| m.as_str()),
        }
    }
}

fn check_materials(s: &Settings) -> Vec<(String, String)> {
    match Materials::parse(s.get("materials")) {
        Ok(_) => Vec::new(),
        Err(m) => vec![("setting_type".into(), m)],
    }
}

impl MeshSettings {
    fn from(s: &Settings) -> Self {
        Self {
            side_len_m: s.f("side_len_m"),
            profile: s.curve("profile"),
            noise_m: s.f("noise_m"),
            bark_tile_m: s.f("bark_tile_m"),
            materials: Materials::parse(s.get("materials"))
                .unwrap_or(Materials::ByOrder(vec!["bark".into()])),
            min_radius_m: s.f("min_radius_m"),
        }
    }
}

/// Meshes every plant of a skeleton (spec 68 § 6.1).
pub(crate) fn mesh_skeleton(sk: &PlantSkeleton, ms: &MeshSettings, node_seed: u32) -> PlantMesh {
    let mut mesh = PlantMesh {
        skeleton: Arc::new(sk.clone()),
        ..PlantMesh::default()
    };
    for p in &sk.plants {
        for axis in axes(p) {
            let start = axis.nodes[0];
            let r0 = p.nodes[start].radius;
            if r0 < ms.min_radius_m {
                continue;
            }
            let (mut pts, mut rad) = polyline(p, &axis);
            // Stop the tube where the branch gets thinner than min_radius_m.
            if let Some(cut) = rad.iter().position(|r| *r < ms.min_radius_m)
                && cut >= 2
            {
                pts.truncate(cut);
                rad.truncate(cut);
            }
            if pts.len() < 2 {
                continue;
            }
            let (pts, rad) = add_turn_rings(&pts, &rad);
            let order = p.nodes[start].order;
            let material = ms.materials.pick(order, r0).to_string();
            let (phase, stiff) = branch_wind(p, start);
            let wind = [order.min(3) as f32, phase, stiff, 0.0];
            let pivot = pts[0].map(|x| x as f32);
            let sides = ring_sides(r0, ms.side_len_m);
            let slot = mesh.slot_mut(&material, SlotKind::Bark);
            let tube = Tube {
                sides,
                branch_seed: mix(p.seed, start as u32),
                node_seed,
                wind,
                pivot,
            };
            sweep(slot, &pts, &rad, ms, &tube);
        }
    }
    mesh.plant_bounds = sk
        .plants
        .iter()
        .map(|p| {
            PlantSkeleton {
                plants: vec![p.clone()],
            }
            .bounds()
        })
        .collect();
    mesh
}

/// Inserts a ring at the middle of both segments next to a node that turns by more than 10°.
fn add_turn_rings(pts: &[V3], rad: &[f64]) -> (Vec<V3>, Vec<f64>) {
    let n = pts.len();
    let sharp: Vec<bool> = (0..n)
        .map(|k| k > 0 && k + 1 < n && turn_deg(pts[k - 1], pts[k], pts[k + 1]) > 10.0)
        .collect();
    let mut op = Vec::with_capacity(n * 2);
    let mut or = Vec::with_capacity(n * 2);
    for k in 0..n {
        op.push(pts[k]);
        or.push(rad[k]);
        if k + 1 < n && (sharp[k] || sharp[k + 1]) {
            op.push(mul(add(pts[k], pts[k + 1]), 0.5));
            or.push(0.5 * (rad[k] + rad[k + 1]));
        }
    }
    (op, or)
}

/// Per-tube constants of a sweep.
#[derive(Clone, Copy)]
struct Tube {
    sides: u32,
    branch_seed: u32,
    node_seed: u32,
    wind: [f32; 4],
    pivot: [f32; 3],
}

/// One tube: `sides + 1` vertices per ring (the seam vertex repeats the first with U = 1).
fn sweep(slot: &mut PlantMeshSlot, pts: &[V3], rad: &[f64], ms: &MeshSettings, tube: &Tube) {
    let Tube {
        sides,
        branch_seed,
        node_seed,
        wind,
        pivot,
    } = *tube;
    let n = pts.len();
    let tangent = |k: usize| -> V3 {
        let a = pts[k.saturating_sub(1)];
        let b = pts[(k + 1).min(n - 1)];
        norm_or(sub(b, a), [0.0, 1.0, 0.0])
    };
    let mut normal = perp_basis(tangent(0)).0;
    let mut dist = 0.0f64;
    let base = slot.positions.len() as u32;
    let ring = sides + 1;
    for k in 0..n {
        let t = tangent(k);
        if k > 0 {
            dist += len(sub(pts[k], pts[k - 1]));
            normal = norm_or(sub(normal, mul(t, dot(normal, t))), perp_basis(t).0);
        }
        let bin = cross(t, normal);
        for s in 0..=sides {
            let si = s % sides;
            let (sn, cs) = detmath::sin_cos_deg(360.0 * f64::from(si) / f64::from(sides));
            let off = add(mul(normal, cs), mul(bin, sn));
            let u = f64::from(si) / f64::from(sides);
            let mut r = rad[k];
            if !ms.profile.is_empty() {
                r *= eval_curve(&ms.profile, u);
            }
            if ms.noise_m > 0.0 {
                let e = mix(branch_seed, (k as u32).wrapping_mul(64).wrapping_add(si));
                r += ms.noise_m * (2.0 * crate::seeds::draw(e, node_seed, 0) - 1.0);
            }
            let r = r.max(0.0);
            slot.positions.push(add(pts[k], mul(off, r)).map(|x| x as f32));
            slot.normals.push(off.map(|x| x as f32));
            slot.uvs
                .push([f64::from(s) as f32 / sides as f32, (dist / ms.bark_tile_m) as f32]);
            slot.wind.push(wind);
            slot.wind_pivot.push(pivot);
        }
    }
    for k in 0..n as u32 - 1 {
        for s in 0..sides {
            let a = base + k * ring + s;
            let b = a + 1;
            let c = a + ring;
            let d = c + 1;
            slot.indices.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
}

fn eval_mesh(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let ms = MeshSettings::from(s);
    let mut out = Bundle::empty();
    for (item, sk) in skeleton_items(&input(inputs, 0)) {
        ctx.check_cancel()?;
        let mesh = mesh_skeleton(&sk, &ms, ctx.node_seed);
        ctx.count("triangles", mesh.triangles() as u64);
        out.0.push(item.with_payload(Payload::PlantMesh(Arc::new(mesh))));
    }
    Ok(vec![out])
}

// ---------------------------------------------------------------------------------------------------
// plant.leaves

/// One rule of a leaf choice: the site value it tests, its range and the weight it adds.
#[derive(Clone, Debug)]
struct Rule {
    what: String,
    min: f64,
    max: f64,
    weight: f64,
}

/// One leaf choice.
#[derive(Clone, Debug)]
struct Choice {
    mesh: Option<String>,
    material: String,
    weight: f64,
    size: [f64; 2],
    rules: Vec<Rule>,
    leave_bare: bool,
}

const RULES: [&str; 4] = ["height", "light", "order", "radius"];

fn parse_choices(v: Option<&SettingValue>) -> Result<Vec<Choice>, String> {
    let Some(items) = v.and_then(SettingValue::as_items) else {
        return Err("choices is a list of (card | mesh, weight, size_m, rules) records".into());
    };
    let mut out = Vec::new();
    for c in items {
        let f = |k: &str| c.field(k);
        let mesh = f("mesh").and_then(SettingValue::as_text).map(str::to_string);
        let size = f("size_m")
            .and_then(SettingValue::as_items)
            .and_then(|x| Some([x.first()?.as_f64()?, x.get(1)?.as_f64()?]))
            .unwrap_or([0.06, 0.1]);
        let mut rules = Vec::new();
        for r in f("rules").and_then(SettingValue::as_items).unwrap_or(&[]) {
            let (name, args): (&str, Vec<f64>) = match r {
                SettingValue::Call(n, a) => (n.as_str(), a.iter().filter_map(SettingValue::as_f64).collect()),
                SettingValue::Record(Some(n), fields) => (
                    n.as_str(),
                    ["min", "max", "weight"]
                        .iter()
                        .filter_map(|k| {
                            fields
                                .iter()
                                .find(|(fk, _)| fk == k)
                                .and_then(|(_, v)| v.as_f64())
                        })
                        .collect(),
                ),
                _ => return Err("a rule is height|light|order|radius(min, max[, weight])".into()),
            };
            if !RULES.contains(&name) || args.len() < 2 {
                return Err(format!("unknown or incomplete rule `{name}`"));
            }
            rules.push(Rule {
                what: name.to_string(),
                min: args[0],
                max: args[1],
                weight: args.get(2).copied().unwrap_or(1.0),
            });
        }
        out.push(Choice {
            mesh,
            material: f("material")
                .and_then(SettingValue::as_text)
                .unwrap_or("leaf")
                .to_string(),
            weight: f("weight").and_then(SettingValue::as_f64).unwrap_or(1.0),
            size,
            rules,
            leave_bare: f("leave_bare").and_then(SettingValue::as_bool).unwrap_or(false),
        });
    }
    Ok(out)
}

fn check_choices(s: &Settings) -> Vec<(String, String)> {
    match parse_choices(s.get("choices")) {
        Ok(_) => Vec::new(),
        Err(m) => vec![("setting_type".into(), m)],
    }
}

/// One leaf site on a branch.
struct Site {
    pos: V3,
    tangent: V3,
    height: f64,
    light: f64,
    order: u32,
    radius: f64,
    from_tip: f64,
    axis_start: usize,
    pivot: V3,
}

fn sites(p: &Plant, where_: &str, orders: [f64; 2], spacing: f64, per_branch_max: usize) -> Vec<Site> {
    let root_y = p.nodes.first().map_or(0.0, |n| n.position[1]);
    let mut out = Vec::new();
    for axis in axes(p) {
        let start = axis.nodes[0];
        let order = p.nodes[start].order;
        if !(orders[0]..=orders[1]).contains(&f64::from(order)) {
            continue;
        }
        let (pts, rad) = polyline(p, &axis);
        let node_of = |k: usize| -> usize {
            // pts[k] is node axis.nodes[k - 1] when there is an attachment point.
            let off = usize::from(axis.attach.is_some());
            axis.nodes[k.saturating_sub(off).min(axis.nodes.len() - 1)]
        };
        let total: f64 = pts.windows(2).map(|w| len(sub(w[1], w[0]))).sum();
        let pivot = pts[0];
        let mut count = 0usize;
        let push = |pos: V3, t: V3, r: f64, along: f64, node: usize, out: &mut Vec<Site>| {
            out.push(Site {
                pos,
                tangent: t,
                height: pos[1] - root_y,
                light: p.nodes[node].light,
                order,
                radius: r,
                from_tip: (total - along).max(0.0),
                axis_start: start,
                pivot,
            });
        };
        if where_ != "tips" && pts.len() >= 2 {
            let mut along = 0.0f64;
            let mut next = 0.5 * spacing;
            for k in 0..pts.len() - 1 {
                let seg = len(sub(pts[k + 1], pts[k]));
                let t = norm_or(sub(pts[k + 1], pts[k]), [0.0, 1.0, 0.0]);
                while next <= along + seg && count < per_branch_max {
                    let f = if seg > 0.0 { (next - along) / seg } else { 0.0 };
                    let pos = add(pts[k], mul(sub(pts[k + 1], pts[k]), f));
                    let r = rad[k] + (rad[k + 1] - rad[k]) * f;
                    push(pos, t, r, next, node_of(k + 1), &mut out);
                    count += 1;
                    next += spacing;
                }
                along += seg;
            }
        }
        if where_ != "along" && count < per_branch_max {
            let k = pts.len() - 1;
            let t = if k > 0 {
                norm_or(sub(pts[k], pts[k - 1]), [0.0, 1.0, 0.0])
            } else {
                p.nodes[start].dir
            };
            push(pts[k], t, rad[k], total, node_of(k), &mut out);
        }
    }
    out
}

fn score(c: &Choice, s: &Site) -> f64 {
    if c.rules.is_empty() {
        return c.weight;
    }
    let mut hit = false;
    let mut sum = c.weight;
    for r in &c.rules {
        let v = match r.what.as_str() {
            "height" => s.height,
            "light" => s.light,
            "order" => f64::from(s.order),
            _ => s.radius,
        };
        if (r.min..=r.max).contains(&v) {
            hit = true;
            sum += r.weight;
        }
    }
    if hit { sum } else { 0.0 }
}

/// A grid of earlier leaf centers for `keep_clear_m`.
struct ClearGrid {
    cell: f64,
    cells: HashMap<(i32, i32, i32), Vec<[f32; 3]>>,
}

impl ClearGrid {
    fn new(cell: f64, pts: &[[f32; 3]]) -> Self {
        let mut g = Self {
            cell: cell.max(1e-3),
            cells: HashMap::new(),
        };
        for p in pts {
            let k = g.key(p.map(f64::from));
            g.cells.entry(k).or_default().push(*p);
        }
        g
    }
    fn key(&self, p: V3) -> (i32, i32, i32) {
        let v = |x: f64| (x / self.cell).floor() as i32;
        (v(p[0]), v(p[1]), v(p[2]))
    }
    fn near(&self, p: V3, d: f64) -> bool {
        let c = self.key(p);
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if let Some(list) = self.cells.get(&(c.0 + dx, c.1 + dy, c.2 + dz))
                        && list.iter().any(|q| len(sub(q.map(f64::from), p)) < d)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

fn eval_leaves(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let choices = parse_choices(s.get("choices")).map_err(|m| NodeError::new("setting_type", m))?;
    let where_ = s.text("where").to_string();
    let orders = s.range2("orders");
    let spacing = s.f("spacing_m");
    let per_branch_max = usize::try_from(s.i("per_branch_max")).unwrap_or(0);
    let orient = s.text("orient").to_string();
    let jitter = s.f("orient_jitter_deg");
    let size_curve = s.curve("size_to_tip");
    let keep_clear = s.f("keep_clear_m");
    let mask_b = input(inputs, 1);
    let mask = if mask_b.0.is_empty() {
        None
    } else {
        Some(Region::new(&mask_b, Combine::Max)?)
    };
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let mut mesh = match &item.payload {
            Payload::PlantMesh(m) => (**m).clone(),
            Payload::PlantSkeleton(sk) => PlantMesh {
                skeleton: sk.clone(),
                plant_bounds: sk
                    .plants
                    .iter()
                    .map(|p| {
                        PlantSkeleton {
                            plants: vec![p.clone()],
                        }
                        .bounds()
                    })
                    .collect(),
                ..PlantMesh::default()
            },
            _ => continue,
        };
        let earlier = ClearGrid::new(keep_clear, &mesh.leaf_sites);
        let mut placed: Vec<[f32; 3]> = Vec::new();
        let skeleton = mesh.skeleton.clone();
        for p in &skeleton.plants {
            ctx.check_cancel()?;
            for (k, site) in sites(p, &where_, orders, spacing, per_branch_max)
                .into_iter()
                .enumerate()
            {
                if mask
                    .as_ref()
                    .is_some_and(|m| m.value(site.pos.map(|x| x as f32)) >= 0.5)
                {
                    continue;
                }
                // The highest score wins; ties keep the lower index; nothing scores above 0 → bare.
                let mut best: Option<(usize, f64)> = None;
                for (ci, c) in choices.iter().enumerate() {
                    let sc = score(c, &site);
                    if sc > 0.0 && best.is_none_or(|(_, b)| sc > b) {
                        best = Some((ci, sc));
                    }
                }
                let Some((ci, _)) = best else {
                    continue;
                };
                let choice = &choices[ci];
                if choice.leave_bare {
                    continue;
                }
                let e = mix(mix(p.seed, site.axis_start as u32), k as u32);
                let d = |slot: u32| ctx.draw(e, slot);
                let yaw = 360.0 * d(0);
                let outward = tilt_toward(site.tangent, yaw, 90.0);
                let (mut length_axis, mut face) = match orient.as_str() {
                    "along" => (site.tangent, outward),
                    "up" => (
                        [0.0, 1.0, 0.0],
                        norm_or([outward[0], 0.0, outward[2]], [1.0, 0.0, 0.0]),
                    ),
                    "random" => {
                        let r = tilt_toward([0.0, 1.0, 0.0], 360.0 * d(1), 180.0 * d(2));
                        (r, perp_basis(r).0)
                    }
                    _ => (
                        norm_or([outward[0], 0.0, outward[2]], [1.0, 0.0, 0.0]),
                        [0.0, 1.0, 0.0],
                    ),
                };
                if jitter > 0.0 {
                    let axis = tilt_toward(length_axis, 360.0 * d(3), 90.0);
                    let a = jitter * (2.0 * d(4) - 1.0);
                    length_axis = rotate(length_axis, axis, a);
                    face = rotate(face, axis, a);
                }
                let mut size = choice.size[0] + (choice.size[1] - choice.size[0]) * d(5);
                if !size_curve.is_empty() {
                    size *= eval_curve(&size_curve, site.from_tip);
                }
                if size <= 0.0 {
                    continue;
                }
                let center = add(site.pos, mul(length_axis, 0.5 * size));
                if keep_clear > 0.0 && earlier.near(center, keep_clear) {
                    continue;
                }
                let (phase, stiff) = branch_wind(p, site.axis_start);
                let wind = [3.0, phase, stiff, 1.0];
                let pivot = site.pivot.map(|x| x as f32);
                let side = norm_or(cross(face, length_axis), perp_basis(length_axis).0);
                let slot = mesh.slot_mut(&choice.material, SlotKind::Leaf);
                match &choice.mesh {
                    Some(path) => {
                        let Some(m) = ctx.world().mesh(path) else {
                            ctx.warn("mesh_missing", format!("leaf mesh `{path}` was not captured"));
                            continue;
                        };
                        let base = slot.positions.len() as u32;
                        for (vi, v) in m.positions.iter().enumerate() {
                            let local = add(
                                add(mul(side, f64::from(v[0])), mul(length_axis, f64::from(v[1]))),
                                mul(face, f64::from(v[2])),
                            );
                            slot.positions
                                .push(add(site.pos, mul(local, size)).map(|x| x as f32));
                            let nrm = m.normals.get(vi).copied().unwrap_or([0.0, 0.0, 1.0]);
                            let wn = add(
                                add(mul(side, f64::from(nrm[0])), mul(length_axis, f64::from(nrm[1]))),
                                mul(face, f64::from(nrm[2])),
                            );
                            slot.normals.push(norm_or(wn, face).map(|x| x as f32));
                            slot.uvs.push([0.0, 0.0]);
                            slot.wind.push(wind);
                            slot.wind_pivot.push(pivot);
                        }
                        slot.indices.extend(m.indices.iter().map(|i| base + i));
                    }
                    None => {
                        let base = slot.positions.len() as u32;
                        let w = mul(side, 0.25 * size);
                        let tip = add(site.pos, mul(length_axis, size));
                        for (pos, uv) in [
                            (sub(site.pos, w), [0.0, 1.0]),
                            (add(site.pos, w), [1.0, 1.0]),
                            (add(tip, w), [1.0, 0.0]),
                            (sub(tip, w), [0.0, 0.0]),
                        ] {
                            slot.positions.push(pos.map(|x| x as f32));
                            slot.normals.push(face.map(|x| x as f32));
                            slot.uvs.push(uv);
                            slot.wind.push(wind);
                            slot.wind_pivot.push(pivot);
                        }
                        slot.indices
                            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                    }
                }
                placed.push(center.map(|x| x as f32));
            }
        }
        ctx.count("leaves", placed.len() as u64);
        mesh.leaf_sites.extend(placed);
        out.0.push(item.with_payload(Payload::PlantMesh(Arc::new(mesh))));
    }
    Ok(vec![out])
}

// ---------------------------------------------------------------------------------------------------
// plant.export

fn floats(v: Option<&SettingValue>) -> Option<Vec<f32>> {
    v?.as_items()?
        .iter()
        .map(|x| x.as_f64().map(|f| f as f32))
        .collect()
}

fn check_export(s: &Settings) -> Vec<(String, String)> {
    let mut e = Vec::new();
    match floats(s.get("lods")) {
        Some(l)
            if !l.is_empty()
                && l.iter().all(|r| *r > 0.0 && *r <= 1.0)
                && l.windows(2).all(|w| w[1] < w[0]) => {}
        _ => e.push((
            "setting_range".into(),
            "lods is a non-empty list of decreasing ratios in (0, 1]".into(),
        )),
    }
    if floats(s.get("lod_screen_size")).is_none() {
        e.push((
            "setting_type".into(),
            "lod_screen_size is a list of numbers".into(),
        ));
    }
    e
}

fn eval_export(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let name = s.text("name").to_string();
    let out_path = match s.text("out") {
        "" => format!("{name}.glb"),
        p => p.to_string(),
    };
    let settings = ExportSettings {
        lods: floats(s.get("lods")).unwrap_or_else(|| vec![1.0]),
        billboard: s.b("billboard"),
        lod_screen_size: floats(s.get("lod_screen_size")).unwrap_or_default(),
        shadow_lod: u32::try_from(s.i("shadow_lod")).unwrap_or(0),
        collision: if s.text("collision") == "capsules" {
            ExportCollision::Capsules
        } else {
            ExportCollision::None
        },
        name,
    };
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let Payload::PlantMesh(m) = &item.payload else {
            return Err(NodeError::new(
                "plant_not_meshed",
                "plant.export needs a PlantMesh (add plant.mesh before it)",
            ));
        };
        if m.triangles() == 0 {
            return Err(NodeError::new(
                "plant_not_meshed",
                "the plant mesh has no triangles",
            ));
        }
        let job = ExportJob {
            node: ctx.node_id,
            mesh: m.clone(),
            settings: settings.clone(),
            out: out_path.clone(),
        };
        ctx.output().exports.push(job);
        out.0.push(DataItem::clone(item));
    }
    Ok(vec![out])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plant::{BudState, PlantNode};

    fn chain(n: usize, radius: f64) -> PlantSkeleton {
        let mut nodes = vec![PlantNode::root([0.0; 3], [0.0, 1.0, 0.0], radius, 0.0)];
        for i in 1..n {
            nodes.push(PlantNode {
                parent: Some(i as u32 - 1),
                bud: BudState::Dead,
                ..PlantNode::root([0.0, i as f64 * 0.5, 0.0], [0.0, 1.0, 0.0], radius, 0.0)
            });
        }
        PlantSkeleton {
            plants: vec![Plant {
                seed: 3,
                nodes,
                ..Plant::default()
            }],
        }
    }

    fn settings() -> MeshSettings {
        MeshSettings {
            side_len_m: 0.04,
            profile: Vec::new(),
            noise_m: 0.0,
            bark_tile_m: 1.0,
            materials: Materials::ByOrder(vec!["bark".into()]),
            min_radius_m: 0.003,
        }
    }

    #[test]
    fn sides_follow_the_radius_rule() {
        assert_eq!(ring_sides(0.001, 0.04), 3);
        assert_eq!(ring_sides(0.1, 0.04), 16);
        assert_eq!(ring_sides(1.0, 0.04), 24);
    }

    #[test]
    fn straight_tube_has_closed_rings_and_the_expected_triangles() {
        let m = mesh_skeleton(&chain(5, 0.1), &settings(), 9);
        assert_eq!(m.slots.len(), 1);
        let slot = &m.slots[0];
        let sides = ring_sides(0.1, 0.04) as usize;
        assert_eq!(slot.triangles(), 2 * sides * 4, "2 sides (rings - 1) triangles");
        let ring = sides + 1;
        for r in 0..5 {
            let first = slot.positions[r * ring];
            let last = slot.positions[r * ring + sides];
            assert_eq!(first, last, "ring {r} closes in position");
            assert_ne!(
                slot.uvs[r * ring],
                slot.uvs[r * ring + sides],
                "seam differs in UV"
            );
        }
        for n in &slot.normals {
            let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((l - 1.0).abs() < 1e-5);
        }
        assert!(slot.positions.iter().flatten().all(|v| v.is_finite()));
        assert_eq!(slot.wind.len(), slot.positions.len());
        assert_eq!(slot.wind_pivot.len(), slot.positions.len());
    }

    #[test]
    fn thin_branches_are_not_meshed() {
        let m = mesh_skeleton(&chain(3, 0.001), &settings(), 9);
        assert_eq!(m.triangles(), 0);
    }

    #[test]
    fn faces_point_outward() {
        let m = mesh_skeleton(&chain(2, 0.1), &settings(), 1);
        let s = &m.slots[0];
        for t in s.indices.as_chunks::<3>().0 {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| s.positions[i as usize]);
            let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let n = [
                ab[1] * ac[2] - ab[2] * ac[1],
                ab[2] * ac[0] - ab[0] * ac[2],
                ab[0] * ac[1] - ab[1] * ac[0],
            ];
            let mid = [(a[0] + b[0] + c[0]) / 3.0, 0.0, (a[2] + b[2] + c[2]) / 3.0];
            assert!(
                n[0] * mid[0] + n[2] * mid[2] > 0.0,
                "triangle faces away from the axis"
            );
        }
    }
}
