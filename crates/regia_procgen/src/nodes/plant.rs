// SPDX-License-Identifier: MPL-2.0
//! `plant.seed`, `plant.grow` and `plant.grow_envelope` (spec 68 §§ 3-4): starting plants and
//! growing their skeletons. Pipeline builds only.
//!
//! `plant.grow` is a bud-based, light-driven growth model after Palubicki et al., "Self-organizing
//! tree models for image synthesis" (SIGGRAPH 2009): shadow propagation in a voxel grid, light
//! gathered from the tips to the root, Borchert-Honda resource allocation with apical control, and
//! bud fate by resource. `plant.grow_envelope` is space colonization after Runions, Lane and
//! Prusinkiewicz (Eurographics Workshop on Natural Phenomena 2007). Original code; the stage order
//! inside a step, settings and defaults are Regia's (spec 68 § 13).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use regia_core::math::hash::mix;

use crate::data::{Bundle, DataItem, DataKind, KindSet, Payload};
use crate::detmath;
use crate::exec::NodeCtx;
use crate::nodes::common::{Combine, Region};
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, input, kind, num, pin_in, pin_multi,
    pin_out, pin_req, set,
};
use crate::plant::envelope::MeshEnvelope;
use crate::plant::{BudState, Plant, PlantBud, PlantNode, PlantSkeleton};
use crate::value::SettingValue;

/// Most nodes one plant may have (spec 68 § 4 validation).
pub(crate) const MAX_PLANT_NODES: usize = 200_000;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "plant.seed",
        "plant",
        "One plant per input point (default: one at the origin); the point seed becomes the plant seed",
        &[
            num("facing_deg", T::Float, "0.0", -360.0, 360.0, "Initial yaw of the first lateral pattern"),
            set("up", T::Vec3, "(0.0, 1.0, 0.0)", "Initial growth direction"),
            set("tilt_deg", T::Range2, "[0.0, 0.0]", "Random initial lean range, degrees"),
            num("start_radius_m", T::Float, "0.01", 0.0001, 10.0, "Root radius"),
        ],
        |_| PinSpec::new(vec![pin_in("in", POINTS)], vec![pin_out("out", kind(DataKind::PlantSkeleton))]),
        eval_seed,
    )
    .plant(),
    BuiltinNode::new(
        "plant.grow",
        "plant",
        "Grows skeletons in light-driven growth steps (buds, shadow voxels, Borchert-Honda allocation, tropisms, shedding)",
        GROW_SETTINGS,
        |_| {
            PinSpec::new(
                vec![
                    pin_req("in", kind(DataKind::PlantSkeleton)),
                    pin_multi("obstacles", kind(DataKind::Shape)),
                    pin_multi("envelope", KindSet::of(&[DataKind::Shape, DataKind::Mesh])),
                ],
                vec![pin_out("out", kind(DataKind::PlantSkeleton))],
            )
        },
        eval_grow,
    )
    .plant()
    .checked(check_grow),
    BuiltinNode::new(
        "plant.grow_envelope",
        "plant",
        "Space colonization: nodes grow toward attraction points sampled in the envelope",
        &[
            num("growth_steps", T::Int, "40", 1.0, 512.0, "Most iterations"),
            num("points", T::Int, "1500", 1.0, 100_000.0, "Attraction points"),
            num("kill_m", T::Float, "0.3", 0.001, 100.0, "Points this close to a node are removed"),
            num("influence_m", T::Float, "2.0", 0.01, 1000.0, "Points influence nodes within this distance"),
            num("step_m", T::Float, "0.2", 0.001, 100.0, "Length of each new segment"),
            num("max_order", T::Int, "4", 0.0, 16.0, "Highest branch order"),
        ],
        |_| {
            PinSpec::new(
                vec![
                    pin_req("in", kind(DataKind::PlantSkeleton)),
                    pin_multi("envelope", KindSet::of(&[DataKind::Shape, DataKind::Mesh])),
                ],
                vec![pin_out("out", kind(DataKind::PlantSkeleton))],
            )
        },
        eval_grow_envelope,
    )
    .plant(),
    BuiltinNode::new(
        "plant.envelope_mesh",
        "plant",
        "One Mesh item for an `envelope` pin: a closed mesh asset from the host (ADR-2731)",
        &[set("asset", T::Asset, "\"\"", "Closed mesh asset, object space of the plant")],
        |_| PinSpec::new(vec![], vec![pin_out("out", kind(DataKind::Mesh))]),
        eval_envelope_mesh,
    )
    .plant()
    .reads_world_data(),
];

const GROW_SETTINGS: &[crate::nodes::SettingDesc] = &[
    num("growth_steps", T::Int, "12", 0.0, 64.0, "Growth steps"),
    num(
        "internode_m",
        T::Float,
        "0.35",
        0.001,
        100.0,
        "Internode length of the trunk",
    ),
    num(
        "internode_decay",
        T::Float,
        "0.92",
        0.01,
        2.0,
        "Internode length factor per order",
    ),
    num(
        "buds_per_node",
        T::Int,
        "1",
        1.0,
        12.0,
        "Lateral buds per node: 1 spiral, 2 opposite, n whorled",
    ),
    num(
        "divergence_deg",
        T::Float,
        "137.5",
        -360.0,
        360.0,
        "Yaw between successive nodes' buds",
    ),
    num(
        "branch_angle_deg",
        T::Float,
        "40.0",
        0.0,
        180.0,
        "Lateral bud angle from the axis",
    ),
    num(
        "branch_angle_decay",
        T::Float,
        "0.9",
        0.01,
        2.0,
        "Branch angle factor per order",
    ),
    num("max_order", T::Int, "4", 0.0, 16.0, "Highest branch order"),
    num(
        "vigor",
        T::Float,
        "1.0",
        0.0,
        1000.0,
        "Resource per unit of gathered light",
    ),
    num(
        "apical",
        T::Float,
        "0.55",
        0.0,
        1.0,
        "Share to the main axis at each fork (Borchert-Honda lambda)",
    ),
    num(
        "min_vigor",
        T::Float,
        "0.15",
        0.0001,
        1000.0,
        "Buds with less resource stay dormant",
    ),
    num("voxel_m", T::Float, "0.25", 0.01, 100.0, "Light voxel size"),
    num(
        "shadow_depth",
        T::Int,
        "6",
        0.0,
        64.0,
        "Shadow pyramid depth, voxels",
    ),
    num(
        "shadow_strength",
        T::Float,
        "0.6",
        0.0,
        1.0,
        "Shadow factor per voxel of depth",
    ),
    num(
        "shadow_spread",
        T::Float,
        "1.0",
        0.0,
        8.0,
        "Pyramid widening per voxel",
    ),
    num("full_light", T::Float, "1.0", 0.0, 1000.0, "Unshadowed light"),
    num(
        "up_pull",
        T::Float,
        "0.08",
        -10.0,
        10.0,
        "Bend toward up per internode",
    ),
    num(
        "light_pull",
        T::Float,
        "0.15",
        0.0,
        10.0,
        "Bend toward the lighter neighbor voxel",
    ),
    num(
        "wander_deg",
        T::Float,
        "6.0",
        0.0,
        180.0,
        "Random turn per internode",
    ),
    num(
        "optimal_dir_weight",
        T::Float,
        "0.3",
        0.0,
        10.0,
        "Weight of the light direction",
    ),
    num(
        "shed_below_light",
        T::Float,
        "0.12",
        0.0,
        1000.0,
        "Branches whose mean light stays below this are shed",
    ),
    num(
        "shed_after_steps",
        T::Int,
        "3",
        1.0,
        64.0,
        "Steps below the threshold (and dormant steps) before removal",
    ),
    set(
        "bloom_after_steps",
        T::Any,
        "None",
        "None, or the step after which terminal buds may bloom",
    ),
    num(
        "bloom_chance",
        T::Float,
        "0.0",
        0.0,
        1.0,
        "Chance per step a terminal bud blooms",
    ),
];

fn check_grow(s: &Settings) -> Vec<(String, String)> {
    match s.get("bloom_after_steps") {
        None => Vec::new(),
        Some(v) if bloom_after(v).is_some() || matches!(v, SettingValue::Ident(i) if i == "None") => {
            Vec::new()
        }
        Some(_) => vec![(
            "setting_type".to_string(),
            "`bloom_after_steps` is None or a step count".to_string(),
        )],
    }
}

fn bloom_after(v: &SettingValue) -> Option<u32> {
    match v {
        SettingValue::Call(n, a) if n == "Some" => a.first().and_then(SettingValue::as_i64),
        other => other.as_i64(),
    }
    .and_then(|x| u32::try_from(x).ok())
}

// ---------------------------------------------------------------------------------------------------
// Vector helpers (f64, `+ - * / sqrt` and `detmath` only).

/// A 3-vector.
pub(crate) type V3 = [f64; 3];

pub(crate) fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
pub(crate) fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
pub(crate) fn mul(a: V3, s: f64) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
pub(crate) fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
pub(crate) fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
pub(crate) fn len(a: V3) -> f64 {
    dot(a, a).sqrt()
}
/// `a / |a|`, or `fallback` for a near-zero vector.
pub(crate) fn norm_or(a: V3, fallback: V3) -> V3 {
    let l = len(a);
    if l > 1e-12 { mul(a, 1.0 / l) } else { fallback }
}
/// Rotates `v` about the unit `axis` by `deg` degrees (Rodrigues).
pub(crate) fn rotate(v: V3, axis: V3, deg: f64) -> V3 {
    let (s, c) = detmath::sin_cos_deg(deg);
    add(
        add(mul(v, c), mul(cross(axis, v), s)),
        mul(axis, dot(axis, v) * (1.0 - c)),
    )
}
/// Two unit vectors perpendicular to the unit `d` (a fixed choice per direction).
pub(crate) fn perp_basis(d: V3) -> (V3, V3) {
    let r = if d[1].abs() < 0.99 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let u = norm_or(cross(r, d), [0.0, 0.0, 1.0]);
    let v = cross(d, u);
    (u, v)
}
/// The direction `angle_deg` away from `d` toward its perpendicular at `yaw_deg`.
pub(crate) fn tilt_toward(d: V3, yaw_deg: f64, angle_deg: f64) -> V3 {
    let (u, v) = perp_basis(d);
    let (sy, cy) = detmath::sin_cos_deg(yaw_deg);
    let p = add(mul(u, cy), mul(v, sy));
    let (sa, ca) = detmath::sin_cos_deg(angle_deg);
    norm_or(add(mul(d, ca), mul(p, sa)), d)
}

fn f32v(v: V3) -> [f32; 3] {
    v.map(|x| x as f32)
}

/// The plant skeletons of a pin (cloned out of their `Arc`s), keeping item tags.
pub(crate) fn skeleton_items(b: &Bundle) -> Vec<(DataItem, PlantSkeleton)> {
    b.0.iter()
        .filter_map(|item| match &item.payload {
            Payload::PlantSkeleton(s) => Some((item.clone(), (**s).clone())),
            Payload::PlantMesh(m) => Some((item.clone(), (*m.skeleton).clone())),
            _ => None,
        })
        .collect()
}

/// A skeleton item with the same tags as `like`.
pub(crate) fn skeleton_item(like: &DataItem, s: PlantSkeleton) -> DataItem {
    like.with_payload(Payload::PlantSkeleton(Arc::new(s)))
}

fn limit_nodes(n: usize) -> Result<(), NodeError> {
    if n > MAX_PLANT_NODES {
        Err(NodeError::new(
            "limit_exceeded",
            format!("plant nodes: {n} > {MAX_PLANT_NODES}"),
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------
// plant.seed

fn eval_seed(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let up = norm_or(s.v3("up"), [0.0, 1.0, 0.0]);
    let [tilt_lo, tilt_hi] = s.range2("tilt_deg");
    let facing = s.f("facing_deg");
    let radius = s.f("start_radius_m");
    let mut points: Vec<([f32; 3], u32)> = Vec::new();
    if ctx.connected.first().copied().unwrap_or(false) {
        for item in &input(inputs, 0).0 {
            if let Some(p) = item.as_points() {
                points.extend(p.position.iter().copied().zip(p.seed.iter().copied()));
            }
        }
    } else {
        points.push(([0.0; 3], ctx.point_seed([0.0; 3])));
    }
    let mut sk = PlantSkeleton::default();
    for (pos, seed) in points {
        let tilt = tilt_lo + (tilt_hi - tilt_lo) * ctx.draw(seed, 0);
        let axis_yaw = 360.0 * ctx.draw(seed, 1);
        let dir = if tilt.abs() > 0.0 {
            tilt_toward(up, axis_yaw, tilt)
        } else {
            up
        };
        sk.plants.push(Plant {
            seed,
            nodes: vec![PlantNode::root(pos.map(f64::from), dir, radius, facing)],
            ..Plant::default()
        });
    }
    Ok(vec![Bundle::one(DataItem::new(Payload::PlantSkeleton(
        Arc::new(sk),
    )))])
}

// ---------------------------------------------------------------------------------------------------
// plant.grow

/// Resolved `plant.grow` settings.
#[derive(Clone, Debug)]
pub(crate) struct Grow {
    steps: u32,
    internode_m: f64,
    internode_decay: f64,
    buds_per_node: u32,
    divergence_deg: f64,
    branch_angle_deg: f64,
    branch_angle_decay: f64,
    max_order: u32,
    vigor: f64,
    apical: f64,
    min_vigor: f64,
    voxel_m: f64,
    shadow_depth: i32,
    shadow_strength: f64,
    shadow_spread: f64,
    full_light: f64,
    up_pull: f64,
    light_pull: f64,
    wander_deg: f64,
    optimal_dir_weight: f64,
    shed_below_light: f64,
    shed_after_steps: u32,
    bloom_after: Option<u32>,
    bloom_chance: f64,
}

impl Grow {
    fn from(s: &Settings) -> Self {
        let u = |n: &str| u32::try_from(s.i(n)).unwrap_or(0);
        Self {
            steps: u("growth_steps"),
            internode_m: s.f("internode_m"),
            internode_decay: s.f("internode_decay"),
            buds_per_node: u("buds_per_node").max(1),
            divergence_deg: s.f("divergence_deg"),
            branch_angle_deg: s.f("branch_angle_deg"),
            branch_angle_decay: s.f("branch_angle_decay"),
            max_order: u("max_order"),
            vigor: s.f("vigor"),
            apical: s.f("apical").clamp(0.0, 1.0),
            min_vigor: s.f("min_vigor").max(1e-6),
            voxel_m: s.f("voxel_m").max(1e-3),
            shadow_depth: i32::try_from(s.i("shadow_depth")).unwrap_or(0),
            shadow_strength: s.f("shadow_strength"),
            shadow_spread: s.f("shadow_spread"),
            full_light: s.f("full_light"),
            up_pull: s.f("up_pull"),
            light_pull: s.f("light_pull"),
            wander_deg: s.f("wander_deg"),
            optimal_dir_weight: s.f("optimal_dir_weight"),
            shed_below_light: s.f("shed_below_light"),
            shed_after_steps: u("shed_after_steps").max(1),
            bloom_after: s.get("bloom_after_steps").and_then(bloom_after),
            bloom_chance: s.f("bloom_chance"),
        }
    }
}

type Voxel = (i32, i32, i32);

/// The light field of one growth step: shadow per voxel from every node, obstacles dark.
struct LightField<'a> {
    voxel_m: f64,
    full_light: f64,
    shadow: HashMap<Voxel, f64>,
    obstacles: Option<&'a Region<'a>>,
}

impl LightField<'_> {
    fn voxel(&self, p: V3) -> Voxel {
        let v = |x: f64| (x / self.voxel_m).floor() as i32;
        (v(p[0]), v(p[1]), v(p[2]))
    }

    fn center(&self, v: Voxel) -> V3 {
        [
            (f64::from(v.0) + 0.5) * self.voxel_m,
            (f64::from(v.1) + 0.5) * self.voxel_m,
            (f64::from(v.2) + 0.5) * self.voxel_m,
        ]
    }

    fn blocked(&self, p: V3) -> bool {
        self.obstacles.is_some_and(|r| r.value(f32v(p)) >= 0.5)
    }

    fn light_at_voxel(&self, v: Voxel) -> f64 {
        if self.blocked(self.center(v)) {
            return 0.0;
        }
        (self.full_light - self.shadow.get(&v).copied().unwrap_or(0.0)).max(0.0)
    }

    fn light(&self, p: V3) -> f64 {
        self.light_at_voxel(self.voxel(p))
    }

    /// Unit direction to the lightest of the 26 neighbor voxels when it is lighter than `p`'s own
    /// (zero otherwise; ties keep the first in z, y, x loop order).
    fn light_dir(&self, p: V3) -> V3 {
        let c = self.voxel(p);
        let mut best = self.light_at_voxel(c);
        let mut dir = [0.0; 3];
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if (dx, dy, dz) == (0, 0, 0) {
                        continue;
                    }
                    let l = self.light_at_voxel((c.0 + dx, c.1 + dy, c.2 + dz));
                    if l > best {
                        best = l;
                        dir = [f64::from(dx), f64::from(dy), f64::from(dz)];
                    }
                }
            }
        }
        norm_or(dir, [0.0; 3])
    }
}

/// Step 1: every node adds shadow to the voxels below it, in a pyramid `shadow_depth` deep that
/// widens by `shadow_spread` per voxel, `shadow_strength^q` at depth `q`.
fn build_shadow(plants: &[Plant], g: &Grow) -> HashMap<Voxel, f64> {
    let v = |x: f64| (x / g.voxel_m).floor() as i32;
    let mut occupancy: BTreeMap<Voxel, u32> = BTreeMap::new();
    for n in plants.iter().flat_map(|p| &p.nodes) {
        *occupancy
            .entry((v(n.position[0]), v(n.position[1]), v(n.position[2])))
            .or_default() += 1;
    }
    let falloff: Vec<f64> = (0..=g.shadow_depth.max(0))
        .map(|q| detmath::pow(g.shadow_strength, f64::from(q)))
        .collect();
    let mut shadow: HashMap<Voxel, f64> = HashMap::new();
    for (&(i, j, k), &count) in &occupancy {
        for q in 1..=g.shadow_depth {
            let r = (f64::from(q) * g.shadow_spread).floor() as i32;
            let add = f64::from(count) * falloff[q as usize];
            for dk in -r..=r {
                for di in -r..=r {
                    *shadow.entry((i + di, j - q, k + dk)).or_default() += add;
                }
            }
        }
    }
    shadow
}

/// One growth consumer at a node: the main continuation or a lateral (child subtree or bud).
#[derive(Clone, Copy)]
enum Consumer {
    Child(u32),
    Terminal(u32),
    Lateral(usize),
}

fn eval_grow(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let g = Grow::from(s);
    let obstacles_b = input(inputs, 1);
    let envelope_b = input(inputs, 2);
    let obstacles = if obstacles_b.0.is_empty() {
        None
    } else {
        Some(Region::new(&obstacles_b, Combine::Max)?)
    };
    let envelope = if envelope_b.0.is_empty() {
        None
    } else {
        Some(Envelope::new(&envelope_b)?)
    };
    let mut out = Bundle::empty();
    for (item, mut sk) in skeleton_items(&input(inputs, 0)) {
        let mut field = LightField {
            voxel_m: g.voxel_m,
            full_light: g.full_light,
            shadow: HashMap::new(),
            obstacles: obstacles.as_ref(),
        };
        for step in 1..=g.steps {
            ctx.check_cancel()?;
            field.shadow = build_shadow(&sk.plants, &g);
            let mut shed_total = 0u32;
            let mut active = 0u64;
            for plant in &mut sk.plants {
                let gc = GrowCtx {
                    ctx,
                    g: &g,
                    field: &field,
                    envelope: envelope.as_ref(),
                    step,
                };
                grow_step(&gc, plant)?;
                let shed = shed_step(plant, &g);
                plant.shed_branches += shed;
                shed_total += shed;
                active += plant.nodes.iter().filter(|n| n.bud == BudState::Active).count() as u64
                    + plant.buds.iter().filter(|b| b.state == BudState::Active).count() as u64;
            }
            ctx.count(&format!("step_{step:02}_nodes"), sk.node_count() as u64);
            ctx.count(&format!("step_{step:02}_active_buds"), active);
            ctx.count(&format!("step_{step:02}_shed"), u64::from(shed_total));
        }
        out.0.push(skeleton_item(&item, sk));
    }
    Ok(vec![out])
}

/// What one growth step of one plant reads.
struct GrowCtx<'a, 'c> {
    ctx: &'a NodeCtx<'c>,
    g: &'a Grow,
    field: &'a LightField<'a>,
    envelope: Option<&'a Envelope<'a>>,
    step: u32,
}

/// What an `envelope` pin holds: shape items and closed meshes. A point is inside when any of them
/// says so (shapes: a region value of at least 0.5; meshes: [`MeshEnvelope::contains`]).
struct Envelope<'a> {
    region: Region<'a>,
    meshes: Vec<MeshEnvelope>,
}

impl<'a> Envelope<'a> {
    /// Prepares the items of an `envelope` pin.
    ///
    /// # Errors
    ///
    /// `not_spatial`, and the codes of [`MeshEnvelope::new`] for a mesh that has no inside.
    fn new(bundle: &'a Bundle) -> Result<Self, NodeError> {
        let mut meshes = Vec::new();
        for item in &bundle.0 {
            if let Payload::Mesh(m) = &item.payload {
                meshes.push(MeshEnvelope::new(m).map_err(|e| NodeError::new(e.code, e.message))?);
            }
        }
        Ok(Self {
            region: Region::new(bundle, Combine::Max)?,
            meshes,
        })
    }

    fn contains(&self, p: V3) -> bool {
        self.region.value(f32v(p)) >= 0.5 || self.meshes.iter().any(|m| m.contains(p))
    }
}

/// `plant.envelope_mesh`: the host's mesh. A name it does not know has no inside, so it fails the run
/// rather than growing a plant with no envelope.
fn eval_envelope_mesh(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let path = s.text("asset");
    match ctx.world().mesh(path) {
        Some(m) => Ok(vec![Bundle::one(DataItem::new(Payload::Mesh(m)))]),
        None => Err(NodeError::new(
            "envelope_empty",
            format!("envelope `{path}`: the host has no such mesh"),
        )),
    }
}

/// Steps 1 (exposure), 2 (resource), 3-5 (bud fate, direction, new buds) for one plant.
fn grow_step(gc: &GrowCtx<'_, '_>, plant: &mut Plant) -> Result<(), NodeError> {
    let (ctx, g, field, step) = (gc.ctx, gc.g, gc.field, gc.step);
    let n = plant.nodes.len();
    if n == 0 {
        return Ok(());
    }
    // 1. Exposure of every node and bud.
    for node in &mut plant.nodes {
        node.light = field.light(node.position);
    }
    let terminal_light: Vec<f64> = plant
        .nodes
        .iter()
        .map(|nd| {
            if matches!(nd.bud, BudState::Active | BudState::Dormant) {
                field.light(add(nd.position, mul(nd.dir, 0.5 * g.voxel_m)))
            } else {
                0.0
            }
        })
        .collect();
    let bud_light: Vec<f64> = plant
        .buds
        .iter()
        .map(|b| {
            if b.state == BudState::Active || b.state == BudState::Dormant {
                let at = plant
                    .nodes
                    .get(b.node as usize)
                    .map_or([0.0; 3], |nd| nd.position);
                field.light(add(at, mul(b.dir, 0.5 * g.voxel_m)))
            } else {
                0.0
            }
        })
        .collect();

    // 2. Light gathered tip to root, then Borchert-Honda allocation root to tip.
    let children = plant.children();
    let mut buds_at: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (k, b) in plant.buds.iter().enumerate() {
        if let Some(slot) = buds_at.get_mut(b.node as usize) {
            slot.push(k);
        }
    }
    let mut q = vec![0.0f64; n];
    for i in (0..n).rev() {
        let mut v = terminal_light[i] + buds_at[i].iter().map(|&k| bud_light[k]).sum::<f64>();
        v += children[i].iter().map(|&c| q[c as usize]).sum::<f64>();
        q[i] = v;
    }
    let mut node_res = vec![0.0f64; n];
    let mut terminal_res = vec![0.0f64; n];
    let mut bud_res = vec![0.0f64; plant.buds.len()];
    node_res[0] = g.vigor * q[0];
    for i in 0..n {
        let v = node_res[i];
        if v <= 0.0 {
            continue;
        }
        let order = plant.nodes[i].order;
        let mut main: Option<(Consumer, f64)> = None;
        let mut laterals: Vec<(Consumer, f64)> = Vec::new();
        for &c in &children[i] {
            let entry = (Consumer::Child(c), q[c as usize]);
            if main.is_none() && plant.nodes[c as usize].order == order {
                main = Some(entry);
            } else {
                laterals.push(entry);
            }
        }
        if main.is_none() && terminal_light[i] > 0.0 {
            main = Some((Consumer::Terminal(i as u32), terminal_light[i]));
        }
        for &k in &buds_at[i] {
            laterals.push((Consumer::Lateral(k), bud_light[k]));
        }
        let qm = main.map_or(0.0, |m| m.1);
        let ql: f64 = laterals.iter().map(|l| l.1).sum();
        let denom = g.apical * qm + (1.0 - g.apical) * ql;
        if denom <= 0.0 {
            continue;
        }
        let v_main = v * g.apical * qm / denom;
        let v_lat = v - v_main;
        let mut give = |c: Consumer, r: f64| match c {
            Consumer::Child(c) => node_res[c as usize] += r,
            Consumer::Terminal(t) => terminal_res[t as usize] += r,
            Consumer::Lateral(k) => bud_res[k] += r,
        };
        if let Some((c, _)) = main {
            give(c, v_main);
        }
        if ql > 0.0 {
            for (c, w) in laterals {
                give(c, v_lat * w / ql);
            }
        }
    }

    // 3-5. Bud fate: terminal buds in node order, then lateral buds in list order.
    let mut new_buds: Vec<PlantBud> = Vec::new();
    for (i, &r) in terminal_res.iter().enumerate() {
        if !matches!(plant.nodes[i].bud, BudState::Active | BudState::Dormant) {
            continue;
        }
        if let Some(after) = g.bloom_after
            && step > after
            && g.bloom_chance > 0.0
            && ctx.draw(mix(plant.seed, i as u32), step.wrapping_mul(16).wrapping_add(15)) < g.bloom_chance
        {
            plant.nodes[i].bud = BudState::Bloom;
            plant.nodes[i].bloom = true;
            continue;
        }
        let start = Shoot {
            from: i as u32,
            dir: plant.nodes[i].dir,
            order: plant.nodes[i].order,
            yaw_deg: plant.nodes[i].yaw_deg + g.divergence_deg,
        };
        let grown = if r >= g.min_vigor {
            extend(gc, plant, start, r, &mut new_buds)?
        } else {
            0
        };
        let nd = &mut plant.nodes[i];
        if grown > 0 {
            nd.bud = BudState::Dead;
            nd.dormant_steps = 0;
        } else {
            nd.bud = BudState::Dormant;
            nd.dormant_steps += 1;
            if nd.dormant_steps > g.shed_after_steps {
                nd.bud = BudState::Dead;
            }
        }
    }
    let buds = std::mem::take(&mut plant.buds);
    let mut kept = Vec::with_capacity(buds.len());
    for (k, mut b) in buds.into_iter().enumerate() {
        let r = bud_res.get(k).copied().unwrap_or(0.0);
        let start = Shoot {
            from: b.node,
            dir: b.dir,
            order: b.order,
            yaw_deg: b.yaw_deg,
        };
        let grown = if r >= g.min_vigor {
            extend(gc, plant, start, r, &mut new_buds)?
        } else {
            0
        };
        if grown > 0 {
            continue;
        }
        b.state = BudState::Dormant;
        b.dormant_steps += 1;
        if b.dormant_steps <= g.shed_after_steps {
            kept.push(b);
        }
    }
    kept.append(&mut new_buds);
    plant.buds = kept;
    Ok(())
}

/// Where a shoot starts.
#[derive(Clone, Copy)]
struct Shoot {
    from: u32,
    dir: V3,
    order: u32,
    yaw_deg: f64,
}

/// Grows a shoot of `floor(resource / min_vigor)` internodes (at most 4); returns how many grew.
fn extend(
    gc: &GrowCtx<'_, '_>,
    plant: &mut Plant,
    start: Shoot,
    resource: f64,
    new_buds: &mut Vec<PlantBud>,
) -> Result<u32, NodeError> {
    let (ctx, g, field, envelope, step) = (gc.ctx, gc.g, gc.field, gc.envelope, gc.step);
    let count = ((resource / g.min_vigor).floor() as u32).min(4);
    let length = g.internode_m * detmath::pow(g.internode_decay, f64::from(start.order));
    let mut cur = start.from;
    let mut dir = start.dir;
    let mut yaw = start.yaw_deg;
    let mut grown = 0u32;
    for _ in 0..count {
        let Some(base) = plant.nodes.get(cur as usize).map(|nd| nd.position) else {
            break;
        };
        let index = plant.nodes.len() as u32;
        let elem = mix(plant.seed, index);
        // 4. Direction: previous + light + up, then a random turn up to `wander_deg`.
        let light = field.light_dir(base);
        let mut d = add(
            add(dir, mul(light, g.optimal_dir_weight * g.light_pull)),
            mul([0.0, 1.0, 0.0], g.up_pull),
        );
        d = norm_or(d, dir);
        if g.wander_deg > 0.0 {
            let slot = step.wrapping_mul(16);
            let around = 360.0 * ctx.draw(elem, slot);
            let amount = g.wander_deg * ctx.draw(elem, slot + 1);
            d = tilt_toward(d, around, amount);
        }
        let pos = add(base, mul(d, length));
        if envelope.is_some_and(|e| !e.contains(pos)) || field.blocked(pos) {
            break;
        }
        limit_nodes(plant.nodes.len() + 1)?;
        plant.nodes.push(PlantNode {
            position: pos,
            parent: Some(cur),
            radius: plant.nodes.get(cur as usize).map_or(0.01, |nd| nd.radius),
            order: start.order,
            age: step,
            light: field.light(pos),
            dir: d,
            bud: BudState::Active,
            yaw_deg: yaw,
            bloom: false,
            dormant_steps: 0,
            dim_steps: 0,
        });
        for col in plant.attrs.values_mut() {
            col.push(0.0);
        }
        if grown > 0
            && let Some(prev) = plant.nodes.get_mut(cur as usize)
        {
            prev.bud = BudState::Dead;
        }
        // 5. Lateral buds around the new node.
        if start.order < g.max_order {
            let angle = g.branch_angle_deg * detmath::pow(g.branch_angle_decay, f64::from(start.order));
            for b in 0..g.buds_per_node {
                let byaw = yaw + 360.0 * f64::from(b) / f64::from(g.buds_per_node);
                new_buds.push(PlantBud {
                    node: index,
                    dir: tilt_toward(d, byaw, angle),
                    order: start.order + 1,
                    state: BudState::Active,
                    dormant_steps: 0,
                    yaw_deg: byaw,
                });
            }
        }
        cur = index;
        dir = d;
        yaw += g.divergence_deg;
        grown += 1;
    }
    Ok(grown)
}

/// Step 6: removes branches whose mean light stayed below `shed_below_light` for
/// `shed_after_steps` steps (with their subtrees); returns how many were shed.
fn shed_step(plant: &mut Plant, g: &Grow) -> u32 {
    let n = plant.nodes.len();
    let mut sum = vec![0.0f64; n];
    let mut cnt = vec![0u32; n];
    for i in (0..n).rev() {
        sum[i] += plant.nodes[i].light;
        cnt[i] += 1;
        if let Some(p) = plant.nodes[i].parent {
            let p = p as usize;
            sum[p] += sum[i];
            cnt[p] += cnt[i];
        }
    }
    let mut remove = vec![false; n];
    let mut shed = 0u32;
    for i in 0..n {
        let Some(p) = plant.nodes[i].parent else {
            continue;
        };
        if remove[p as usize] {
            remove[i] = true;
            continue;
        }
        if plant.nodes[i].order <= plant.nodes[p as usize].order {
            continue;
        }
        let mean = sum[i] / f64::from(cnt[i].max(1));
        let nd = &mut plant.nodes[i];
        if mean < g.shed_below_light {
            nd.dim_steps += 1;
        } else {
            nd.dim_steps = 0;
        }
        if nd.dim_steps >= g.shed_after_steps {
            remove[i] = true;
            shed += 1;
        }
    }
    if shed > 0 {
        let keep: Vec<bool> = remove.iter().map(|r| !r).collect();
        plant.retain_nodes(&keep);
    }
    shed
}

// ---------------------------------------------------------------------------------------------------
// plant.grow_envelope

fn eval_grow_envelope(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let env_b = input(inputs, 1);
    if env_b.0.is_empty() {
        return Err(NodeError::new(
            "pin_unconnected",
            "plant.grow_envelope needs an `envelope` shape",
        ));
    }
    let env = Envelope::new(&env_b)?;
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for item in &env_b.0 {
        if let Some((a, b)) = crate::nodes::common::world_bounds(item) {
            lo = std::array::from_fn(|k| lo[k].min(a[k]));
            hi = std::array::from_fn(|k| hi[k].max(b[k]));
        }
    }
    let steps = u32::try_from(s.i("growth_steps")).unwrap_or(0);
    let want = usize::try_from(s.i("points")).unwrap_or(0);
    let kill = s.f("kill_m");
    let influence = s.f("influence_m");
    let step_m = s.f("step_m");
    let max_order = u32::try_from(s.i("max_order")).unwrap_or(0);
    let mut out = Bundle::empty();
    for (item, mut sk) in skeleton_items(&input(inputs, 0)) {
        for plant in &mut sk.plants {
            // Attraction points: rejection sampling in the envelope's AABB, draws keyed by the plant
            // seed and the attempt index.
            let mut pts: Vec<V3> = Vec::with_capacity(want);
            let mut attempt = 0u32;
            while pts.len() < want && attempt < (want as u32).saturating_mul(64) {
                let e = mix(plant.seed, attempt);
                let p: V3 = std::array::from_fn(|k| {
                    f64::from(lo[k]) + f64::from(hi[k] - lo[k]) * ctx.draw(e, k as u32)
                });
                if env.contains(p) {
                    pts.push(p);
                }
                attempt += 1;
            }
            let col = Colonize {
                steps,
                kill,
                influence,
                step_m,
                max_order,
            };
            colonize(ctx, plant, &mut pts, &col)?;
        }
        out.0.push(skeleton_item(&item, sk));
    }
    Ok(vec![out])
}

/// `plant.grow_envelope` settings.
#[derive(Clone, Copy)]
struct Colonize {
    steps: u32,
    kill: f64,
    influence: f64,
    step_m: f64,
    max_order: u32,
}

/// Space colonization iterations over attraction points.
fn colonize(ctx: &NodeCtx<'_>, plant: &mut Plant, pts: &mut Vec<V3>, c: &Colonize) -> Result<(), NodeError> {
    let Colonize {
        steps,
        kill,
        influence,
        step_m,
        max_order,
    } = *c;
    let cell = influence.max(1e-3);
    let key = |p: V3| -> Voxel {
        let v = |x: f64| (x / cell).floor() as i32;
        (v(p[0]), v(p[1]), v(p[2]))
    };
    for step in 1..=steps {
        ctx.check_cancel()?;
        if pts.is_empty() || plant.nodes.is_empty() {
            break;
        }
        let mut grid: HashMap<Voxel, Vec<u32>> = HashMap::new();
        for (i, nd) in plant.nodes.iter().enumerate() {
            grid.entry(key(nd.position)).or_default().push(i as u32);
        }
        // Each point pulls its nearest node within `influence` (ties: lower index).
        let mut pull: BTreeMap<u32, V3> = BTreeMap::new();
        for p in pts.iter() {
            let c = key(*p);
            let mut best: Option<(f64, u32)> = None;
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let Some(list) = grid.get(&(c.0 + dx, c.1 + dy, c.2 + dz)) else {
                            continue;
                        };
                        for &i in list {
                            let d = len(sub(*p, plant.nodes[i as usize].position));
                            if d <= influence && best.is_none_or(|(bd, bi)| d < bd || (d == bd && i < bi)) {
                                best = Some((d, i));
                            }
                        }
                    }
                }
            }
            if let Some((_, i)) = best {
                let dir = norm_or(sub(*p, plant.nodes[i as usize].position), [0.0; 3]);
                let e = pull.entry(i).or_insert([0.0; 3]);
                *e = add(*e, dir);
            }
        }
        if pull.is_empty() {
            break;
        }
        let children = plant.children();
        for (i, sum) in pull {
            let base = &plant.nodes[i as usize];
            let d = norm_or(sum, base.dir);
            let continuing = children[i as usize]
                .iter()
                .any(|&c| plant.nodes[c as usize].order == base.order);
            let order = if continuing { base.order + 1 } else { base.order };
            if order > max_order {
                continue;
            }
            limit_nodes(plant.nodes.len() + 1)?;
            let node = PlantNode {
                position: add(base.position, mul(d, step_m)),
                parent: Some(i),
                radius: base.radius,
                order,
                age: step,
                light: 1.0,
                dir: d,
                bud: BudState::Active,
                yaw_deg: base.yaw_deg,
                bloom: false,
                dormant_steps: 0,
                dim_steps: 0,
            };
            if !continuing && let Some(b) = plant.nodes.get_mut(i as usize) {
                b.bud = BudState::Dead;
            }
            plant.nodes.push(node);
            for col in plant.attrs.values_mut() {
                col.push(0.0);
            }
        }
        // Points within the kill distance of any node are removed.
        let mut grid: HashMap<Voxel, Vec<u32>> = HashMap::new();
        for (i, nd) in plant.nodes.iter().enumerate() {
            grid.entry(key(nd.position)).or_default().push(i as u32);
        }
        pts.retain(|p| {
            let c = key(*p);
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if let Some(list) = grid.get(&(c.0 + dx, c.1 + dy, c.2 + dz))
                            && list
                                .iter()
                                .any(|&i| len(sub(*p, plant.nodes[i as usize].position)) <= kill)
                        {
                            return false;
                        }
                    }
                }
            }
            true
        });
    }
    Ok(())
}
