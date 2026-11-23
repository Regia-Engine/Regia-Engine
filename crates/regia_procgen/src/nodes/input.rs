// SPDX-License-Identifier: MPL-2.0
//! `in.*`: reading the captured world (spec 63 § 3). These nodes never touch the world directly;
//! they read the run's [`crate::exec::WorldSnapshot`].

use std::sync::Arc;

use crate::data::{AttrColumn, AttrValue, Bundle, DataItem, DataKind, GroundData, Payload, Point, PointSet};
use crate::exec::NodeCtx;
use crate::nodes::{BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, kind, pin_out, set};
use crate::value::SettingValue;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "in.self",
        "in",
        "The generator: its bounds as a Shape item tagged `self` (item scope `guid`, `name`), plus its painted points as a Points item tagged `painted` (attribute `layer`)",
        &[],
        |_| PinSpec::new(vec![], vec![pin_out("out", crate::nodes::SPATIAL)]),
        eval_self,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "in.ground",
        "in",
        "One Ground item over the run bounds (`rect: self`) or an explicit `[min_x, min_z, max_x, max_z]`",
        &[
            set("rect", T::Any, "self", "`self` or [min_x, min_z, max_x, max_z]"),
            set(
                "layers",
                T::NameList,
                "[]",
                "Terrain layers the item carries (read with `density.ground_layer`)",
            ),
        ],
        |_| PinSpec::new(vec![], vec![pin_out("out", kind(DataKind::Ground))]),
        eval_ground,
    )
    .uncached()
    .world()
    .checked(check_rect),
    BuiltinNode::new(
        "in.entities",
        "in",
        "Level entities by tag, one point each (attribute `entity` = guid; bounds = the entity's mesh bounds, else the unit box); order by guid",
        &[
            set("tags", T::NameList, "[]", "All required"),
            set(
                "any_tags",
                T::NameList,
                "[]",
                "At least one required (when not empty)",
            ),
            set(
                "within",
                T::Enum(&["self", "world"]),
                "self",
                "Only entities inside the generator bounds",
            ),
            set(
                "as",
                T::Enum(&["points", "splines", "shapes", "all"]),
                "points",
                "Output form: one point per entity, one item per Spline / Shape component, or all",
            ),
        ],
        |s| {
            let kinds = match s.text("as") {
                "splines" => kind(DataKind::Spline),
                "shapes" => kind(DataKind::Shape),
                "all" => crate::data::KindSet::of(&[DataKind::Points, DataKind::Spline, DataKind::Shape]),
                _ => POINTS,
            };
            PinSpec::new(vec![], vec![pin_out("out", kinds)])
        },
        eval_entities,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "in.generator_output",
        "in",
        "Another generator's exported table or points (`out.table` / `out.points`); it runs first",
        &[
            set(
                "generator",
                T::Name,
                "\"\"",
                "The other generator's guid or entity name",
            ),
            set("table", T::Name, "\"\"", "Export name"),
        ],
        |_| PinSpec::new(vec![], vec![pin_out("out", crate::nodes::ANY)]),
        eval_generator_output,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "in.rays",
        "in",
        "A world-aligned grid of downward physics rays over the generator bounds (cast by the host); one point per hit inside the region, attributes `normal`, `entity`, `phys_material`",
        &[
            crate::nodes::num("spacing_m", T::Float, "2.0", 0.01, 1.0e6, "Ray spacing"),
            crate::nodes::num("from_height_m", T::Float, "500.0", -1.0e6, 1.0e6, "Ray start height"),
            crate::nodes::num("max_dist_m", T::Float, "1000.0", 0.01, 1.0e7, "Ray length"),
            crate::nodes::num("layers", T::Int, "-1", -1.0, 4_294_967_295.0, "Physics layer mask (-1 = all)"),
            set("skip_generated", T::Bool, "true", "Ignore colliders of generated entities"),
        ],
        |_| PinSpec::new(vec![crate::nodes::pin_in("in", crate::nodes::SPATIAL)], vec![pin_out("out", POINTS)]),
        eval_rays,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "in.overlap",
        "in",
        "Points whose oriented world box touches a physics collider's shape go to `inside` (attribute `entity`), others to `outside`",
        &[
            crate::nodes::num("layers", T::Int, "-1", -1.0, 4_294_967_295.0, "Physics layer mask (-1 = all)"),
            set("skip_generated", T::Bool, "true", "Ignore colliders of generated entities"),
        ],
        |_| crate::nodes::pins_split(POINTS),
        eval_overlap,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "in.table",
        "in",
        "One Table item with a data table's rows (read untyped by the host at capture)",
        &[
            set("asset", T::Asset, "\"\"", "Data table path"),
            set(
                "row",
                T::Name,
                "\"\"",
                "Only the row with this key (empty = all rows)",
            ),
        ],
        |_| PinSpec::new(vec![], vec![pin_out("out", kind(DataKind::Table))]),
        eval_table,
    )
    .uncached(),
];

fn check_rect(s: &Settings) -> Vec<(String, String)> {
    match s.get("rect") {
        Some(SettingValue::Ident(i)) | Some(SettingValue::Str(i)) if i == "self" => Vec::new(),
        Some(v) => match v.as_items() {
            Some(items)
                if items.len() == 4 && items.iter().all(|x| x.as_f64().is_some_and(f64::is_finite)) =>
            {
                Vec::new()
            }
            _ => vec![(
                "setting_type".to_string(),
                "`rect` must be `self` or [min_x, min_z, max_x, max_z]".to_string(),
            )],
        },
        None => Vec::new(),
    }
}

fn eval_self(ctx: &mut NodeCtx<'_>, _: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let Some(info) = ctx.world().self_info().cloned() else {
        ctx.warn("no_self", "the run has no generator (in.self is empty)");
        return Ok(vec![Bundle::empty()]);
    };
    let mut item = DataItem::new(Payload::Shape(info.bounds));
    item.tags.insert("self".to_string());
    item.scope
        .insert("guid".to_string(), AttrValue::Name(info.guid.to_string()));
    item.scope.insert("name".to_string(), AttrValue::Name(info.name));
    let mut items = vec![item];
    items.extend(ctx.world().self_items());
    // Painted points (spec 67 § 5.2): a second item tagged `painted`, with the `layer` attribute.
    if let Some(painted) = ctx.world().painted() {
        let mut points = DataItem::new(Payload::Points(painted.clone()));
        points.tags.insert("painted".to_string());
        items.push(points);
    }
    Ok(vec![Bundle(items)])
}

fn eval_ground(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let Some(ground) = ctx.world().ground() else {
        ctx.warn("no_terrain", "the level has no terrain");
        return Ok(vec![Bundle::empty()]);
    };
    let ground = match s.get("rect").and_then(SettingValue::as_items) {
        Some(r) if r.len() == 4 => {
            let v: Vec<f32> = r.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect();
            let rect = [
                v[0].max(ground.rect[0]),
                v[1].max(ground.rect[1]),
                v[2].min(ground.rect[2]),
                v[3].min(ground.rect[3]),
            ];
            Arc::new(GroundData {
                rect,
                ..(*ground).clone()
            })
        }
        _ => ground,
    };
    // The snapshot holds what any `in.ground` of the graph named: this item carries its own list.
    let names = s.names("layers");
    for name in &names {
        if !ground.layers.iter().any(|l| &l.name == name) {
            ctx.warn("layer_missing", format!("the terrain has no layer `{name}`"));
        }
    }
    let ground = if ground.layers.iter().all(|l| names.contains(&l.name)) {
        ground
    } else {
        let mut kept = (*ground).clone();
        kept.layers.retain(|l| names.contains(&l.name));
        Arc::new(kept)
    };
    Ok(vec![Bundle::one(DataItem::new(Payload::Ground(ground)))])
}

fn eval_entities(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let tags = s.names("tags");
    let any = s.names("any_tags");
    let within_self = s.text("within") == "self";
    let bounds = ctx.world().self_info().map(|i| i.bounds);
    let form = s.text("as").to_string();
    let mut points = PointSet::new();
    let mut guids = Vec::new();
    let mut items = Vec::new();
    for e in ctx.world().entities() {
        if !tags.iter().all(|t| e.tags.contains(t)) {
            continue;
        }
        if !any.is_empty() && !any.iter().any(|t| e.tags.contains(t)) {
            continue;
        }
        if within_self
            && let Some(b) = &bounds
            && b.value(e.position) <= 0.0
        {
            continue;
        }
        let scoped = |payload: Payload| {
            let mut item = DataItem::new(payload);
            item.scope
                .insert("entity".to_string(), AttrValue::Name(e.guid.to_string()));
            item
        };
        if matches!(form.as_str(), "splines" | "all")
            && let Some(sp) = &e.spline
        {
            let mut item = scoped(Payload::Spline(sp.clone()));
            if let Some(p) = &e.road_profile {
                item.scope
                    .insert("road_profile".to_string(), AttrValue::Asset(p.clone()));
            }
            // An exclusion's width: the largest width factor along it (spec 63 § 3).
            if e.tags.iter().any(|t| t == crate::biome::EXCLUDE_TAG) {
                let width = sp
                    .spline
                    .points
                    .iter()
                    .map(|p| p.scale.x.abs())
                    .fold(0.0f32, f32::max);
                item.scope
                    .insert("exclude_width_m".to_string(), AttrValue::Float(f64::from(width)));
            }
            items.push(item);
        }
        if matches!(form.as_str(), "shapes" | "all")
            && let Some(sh) = e.shape
        {
            items.push(scoped(Payload::Shape(sh)));
        }
        let (bounds_min, bounds_max) = e.mesh_bounds.unwrap_or(([-0.5; 3], [0.5; 3]));
        points.push(Point {
            position: e.position,
            rotation: e.rotation,
            scale: e.scale,
            seed: crate::seeds::point_seed(e.position, ctx.tier_seed),
            bounds_min,
            bounds_max,
            ..Point::default()
        });
        guids.push(e.guid.to_string());
    }
    points
        .attrs
        .insert("entity".to_string(), AttrColumn::Name(guids.into()));
    if matches!(form.as_str(), "splines" | "shapes") {
        return Ok(vec![Bundle(items)]);
    }
    let mut out = vec![DataItem::points(points)];
    out.extend(items);
    Ok(vec![Bundle(out)])
}

fn eval_table(ctx: &mut NodeCtx<'_>, _: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let path = s.text("asset");
    let table =
        ctx.world().table(path).cloned().ok_or_else(|| {
            NodeError::new("table_missing", format!("data table `{path}` was not captured"))
        })?;
    if let Some((column, row)) = table.first_non_finite() {
        return Err(NodeError::new(
            "input_nan",
            format!("data table `{path}`: `{column}` is not finite in row {row}"),
        ));
    }
    let row = s.text("row");
    let table = if row.is_empty() {
        table
    } else {
        let keys = match table.column("key") {
            Some(AttrColumn::Name(k)) => k.clone(),
            _ => Default::default(),
        };
        let Some(r) = keys.iter().position(|k| k == row) else {
            return Err(NodeError::new(
                "table_row_missing",
                format!("`{path}` has no row `{row}`"),
            ));
        };
        table.select(&[r])
    };
    Ok(vec![Bundle::one(DataItem::new(Payload::Table(table)))])
}

fn eval_generator_output(
    ctx: &mut NodeCtx<'_>,
    _: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let (g, t) = (s.text("generator"), s.text("table"));
    match ctx.world().generator_export(g, t) {
        Some(item) => Ok(vec![Bundle::one(item)]),
        None => {
            ctx.warn("export_missing", format!("generator `{g}` exports no `{t}`"));
            Ok(vec![Bundle::empty()])
        }
    }
}

/// Most rays one `in.rays` node casts per run.
pub(crate) const MAX_RAYS: u64 = 4 << 20;

/// The world-aligned ray grid over `rect` (`[min_x, min_z, max_x, max_z]`) at `spacing`: the
/// column range, the row range and the ray count. The host's capture and the node use the same
/// grid, so the node knows when the capture skipped an over-cap grid.
pub(crate) fn ray_grid(rect: [f32; 4], spacing: f64) -> ((i64, i64), (i64, i64), u64) {
    let cells = |lo: f32, hi: f32| {
        (
            (f64::from(lo) / spacing).floor() as i64,
            (f64::from(hi) / spacing).ceil() as i64 - 1,
        )
    };
    let ((x0, x1), (z0, z1)) = (cells(rect[0], rect[2]), cells(rect[1], rect[3]));
    let n = |a: i64, b: i64| u64::try_from(b.saturating_sub(a).saturating_add(1)).unwrap_or(0);
    ((x0, x1), (z0, z1), n(x0, x1).saturating_mul(n(z0, z1)))
}

fn eval_rays(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    use regia_core::math::Vec3;
    // The capture casts no rays for an over-cap grid; say so instead of an empty, "successful" run.
    if let Some(info) = ctx.world().self_info() {
        let (lo, hi) = info.bounds.world_aabb();
        let (_, _, count) = ray_grid([lo[0], lo[2], hi[0], hi[2]], s.f("spacing_m"));
        if count > MAX_RAYS {
            return Err(NodeError::new(
                "limit_exceeded",
                format!("in.rays: {count} rays > {MAX_RAYS}; raise `spacing_m` or shrink the bounds"),
            ));
        }
    }
    let region = crate::nodes::input(inputs, 0);
    let bounds: Vec<[f32; 4]> = region
        .0
        .iter()
        .filter_map(crate::nodes::common::xz_bounds)
        .collect();
    let hits = ctx.world().rays(ctx.node_id).to_vec();
    let mut points = PointSet::new();
    let (mut normals, mut entities, mut materials) = (Vec::new(), Vec::new(), Vec::new());
    for h in hits {
        let [x, _, z] = h.point;
        if ctx.connected.first().copied().unwrap_or(false)
            && !bounds
                .iter()
                .any(|b| x >= b[0] && x <= b[2] && z >= b[1] && z <= b[3])
        {
            continue;
        }
        points.push(Point {
            position: h.point,
            rotation: crate::nodes::common::rotation_between(Vec3::Y, Vec3::from(h.normal)).to_array(),
            seed: ctx.point_seed(h.point),
            ..Point::default()
        });
        normals.push(h.normal.map(f64::from));
        entities.push(h.entity);
        materials.push(h.phys_material);
    }
    ctx.check_points(points.len())?;
    points
        .attrs
        .insert("normal".to_string(), AttrColumn::Vec3(normals.into()));
    points
        .attrs
        .insert("entity".to_string(), AttrColumn::Name(entities.into()));
    points
        .attrs
        .insert("phys_material".to_string(), AttrColumn::Name(materials.into()));
    Ok(vec![Bundle::one(DataItem::points(points))])
}

fn eval_overlap(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let boxes = ctx.world().colliders(ctx.node_id).to_vec();
    let (mut inside, mut outside) = (Bundle::empty(), Bundle::empty());
    for item in crate::nodes::input_items(inputs, 0) {
        let Payload::Points(p) = &item.payload else {
            continue;
        };
        let mut keep = Vec::with_capacity(p.len());
        let mut hit_entity = Vec::new();
        for i in 0..p.len() {
            if i.is_multiple_of(crate::exec::CHUNK) {
                ctx.check_cancel()?;
            }
            let (lo, hi) = p.world_aabb(i);
            let touches = |a: &[f32; 3], b: &[f32; 3]| (0..3).all(|k| lo[k] <= b[k] && a[k] <= hi[k]);
            let obb = PointBox::of(p, i);
            // The AABBs first, then the collider's convex parts against the point's oriented box.
            let hit = boxes.iter().find(|b| {
                touches(&b.min, &b.max)
                    && (b.parts.is_empty()
                        || b.parts
                            .iter()
                            .any(|part| touches(&part.min, &part.max) && gjk_intersects(&obb, part)))
            });
            keep.push(hit.is_some());
            if let Some(b) = hit {
                hit_entity.push(b.entity.clone());
            }
        }
        let (mut a, b) = p.partition(&keep);
        a.attrs
            .insert("entity".to_string(), AttrColumn::Name(hit_entity.into()));
        inside.0.push(item.with_payload(Payload::Points(a)));
        outside.0.push(item.with_payload(Payload::Points(b)));
    }
    Ok(vec![inside, outside])
}

/// A point's oriented world box for the exact `in.overlap` test.
struct PointBox {
    center: [f64; 3],
    /// Half axes (unit axis times half extent), world space.
    axes: [[f64; 3]; 3],
}

impl PointBox {
    fn of(p: &PointSet, i: usize) -> Self {
        use regia_core::math::Vec3;
        let q = crate::data::quat(p.rotation[i]);
        let s = Vec3::from(p.scale[i]);
        let lo = Vec3::from(p.bounds_min[i]) * s;
        let hi = Vec3::from(p.bounds_max[i]) * s;
        let center = Vec3::from(p.position[i]) + q * ((lo + hi) * 0.5);
        let half = (hi - lo).abs() * 0.5;
        let axis = |v: Vec3| (q * v).to_array().map(f64::from);
        Self {
            center: center.to_array().map(f64::from),
            axes: [
                axis(Vec3::X * half.x),
                axis(Vec3::Y * half.y),
                axis(Vec3::Z * half.z),
            ],
        }
    }

    fn support(&self, d: [f64; 3]) -> [f64; 3] {
        let mut out = self.center;
        for a in &self.axes {
            let sign = if dot(*a, d) >= 0.0 { 1.0 } else { -1.0 };
            out = add(out, scale(*a, sign));
        }
        out
    }
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: [f64; 3], k: f64) -> [f64; 3] {
    [a[0] * k, a[1] * k, a[2] * k]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn neg(a: [f64; 3]) -> [f64; 3] {
    [-a[0], -a[1], -a[2]]
}

/// `(a × b) × a`: the direction perpendicular to `a` toward `b`.
fn triple(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    cross(cross(a, b), a)
}

/// The support point of a convex part in direction `d`.
fn part_support(part: &crate::exec::ConvexPart, d: [f64; 3]) -> [f64; 3] {
    let f = |v: [f32; 3]| v.map(f64::from);
    let mut best: Option<([f64; 3], f64)> = None;
    let mut consider = |p: [f64; 3]| {
        let v = dot(p, d);
        if best.is_none_or(|(_, b)| v > b) {
            best = Some((p, v));
        }
    };
    for p in &part.points {
        consider(f(*p));
    }
    for (c, n, r) in &part.disks {
        let (c, n) = (f(*c), f(*n));
        let along = sub(d, scale(n, dot(d, n)));
        let len = dot(along, along).sqrt();
        consider(if len > 1e-12 {
            add(c, scale(along, f64::from(*r) / len))
        } else {
            c
        });
    }
    let core = best.map_or([0.0; 3], |(p, _)| p);
    let len = dot(d, d).sqrt();
    if part.radius > 0.0 && len > 0.0 {
        add(core, scale(d, f64::from(part.radius) / len))
    } else {
        core
    }
}

/// Does the point box touch the convex part? Boolean GJK on the Minkowski difference (Gilbert,
/// Johnson and Keerthi 1988; the simplex cases follow the usual line/triangle/tetrahedron
/// regions). Touching counts as an overlap; a search that does not settle in 64 steps counts as one
/// (it only happens for contacts within rounding of each other).
fn gjk_intersects(a: &PointBox, b: &crate::exec::ConvexPart) -> bool {
    const STEPS: usize = 64;
    let support = |d: [f64; 3]| sub(a.support(d), part_support(b, neg(d)));
    let mut d = sub(a.center, part_support(b, [0.0, 0.0, 0.0]));
    if dot(d, d) < 1e-24 {
        d = [1.0, 0.0, 0.0];
    }
    let mut s: Vec<[f64; 3]> = vec![support(d)];
    d = neg(s[0]);
    for _ in 0..STEPS {
        if dot(d, d) < 1e-24 {
            return true;
        }
        let p = support(d);
        if dot(p, d) < 0.0 {
            return false;
        }
        s.insert(0, p);
        if gjk_simplex(&mut s, &mut d) {
            return true;
        }
    }
    true
}

/// The line case: `s = [a, b]`, `a` newest.
fn gjk_line(s: &mut Vec<[f64; 3]>, d: &mut [f64; 3], a: [f64; 3], b: [f64; 3]) {
    let (ab, ao) = (sub(b, a), neg(a));
    if dot(ab, ao) > 0.0 {
        *s = vec![a, b];
        *d = triple(ab, ao);
    } else {
        *s = vec![a];
        *d = ao;
    }
}

/// The triangle case: `s = [a, b, c]`, `a` newest. Leaves a stored triangle wound so that its
/// normal `(b - a) × (c - a)` is the new search direction.
fn gjk_triangle(s: &mut Vec<[f64; 3]>, d: &mut [f64; 3], a: [f64; 3], b: [f64; 3], c: [f64; 3]) {
    let (ab, ac, ao) = (sub(b, a), sub(c, a), neg(a));
    let abc = cross(ab, ac);
    if dot(cross(abc, ac), ao) > 0.0 {
        if dot(ac, ao) > 0.0 {
            *s = vec![a, c];
            *d = triple(ac, ao);
        } else {
            gjk_line(s, d, a, b);
        }
    } else if dot(cross(ab, abc), ao) > 0.0 {
        gjk_line(s, d, a, b);
    } else if dot(abc, ao) > 0.0 {
        *s = vec![a, b, c];
        *d = abc;
    } else {
        *s = vec![a, c, b];
        *d = neg(abc);
    }
}

/// Updates the simplex `s` (newest first) and the search direction; true when it holds the origin.
fn gjk_simplex(s: &mut Vec<[f64; 3]>, d: &mut [f64; 3]) -> bool {
    match *s.as_slice() {
        [a, b] => gjk_line(s, d, a, b),
        [a, b, c] => gjk_triangle(s, d, a, b, c),
        [a, b, c, e] => {
            let (ab, ac, ae, ao) = (sub(b, a), sub(c, a), sub(e, a), neg(a));
            if dot(cross(ab, ac), ao) > 0.0 {
                gjk_triangle(s, d, a, b, c);
            } else if dot(cross(ac, ae), ao) > 0.0 {
                gjk_triangle(s, d, a, c, e);
            } else if dot(cross(ae, ab), ao) > 0.0 {
                gjk_triangle(s, d, a, e, b);
            } else {
                return true;
            }
        }
        _ => {}
    }
    false
}

#[cfg(test)]
mod gjk_tests {
    use super::*;
    use crate::exec::ConvexPart;

    fn unit_box_at(x: f32, z: f32, yaw_deg: f32) -> PointBox {
        let mut p = PointSet::new();
        let half = yaw_deg.to_radians() * 0.5;
        p.push(Point {
            position: [x, 0.0, z],
            rotation: [0.0, half.sin(), 0.0, half.cos()],
            ..Point::default()
        });
        PointBox::of(&p, 0)
    }

    fn sphere(c: [f32; 3], r: f32) -> ConvexPart {
        ConvexPart {
            points: vec![c],
            radius: r,
            min: [c[0] - r, c[1] - r, c[2] - r],
            max: [c[0] + r, c[1] + r, c[2] + r],
            ..ConvexPart::default()
        }
    }

    #[test]
    fn a_sphere_touches_a_box_only_within_its_radius_of_the_faces_and_corners() {
        // Unit box at the origin (half 0.5); sphere of radius 1.
        assert!(gjk_intersects(
            &unit_box_at(0.0, 0.0, 0.0),
            &sphere([1.4, 0.0, 0.0], 1.0)
        ));
        assert!(!gjk_intersects(
            &unit_box_at(0.0, 0.0, 0.0),
            &sphere([1.6, 0.0, 0.0], 1.0)
        ));
        // Toward a corner: the corner (0.5, 0.5, 0.5) is sqrt(3)/2 from the center; the AABBs of the
        // two overlap at 1.6 along the diagonal axis, the shapes do not.
        let k = 1.6 / 3f32.sqrt() + 0.5;
        assert!(!gjk_intersects(
            &unit_box_at(0.0, 0.0, 0.0),
            &sphere([k, k, k], 1.0)
        ));
        let k = 0.9 / 3f32.sqrt() + 0.5;
        assert!(gjk_intersects(
            &unit_box_at(0.0, 0.0, 0.0),
            &sphere([k, k, k], 1.0)
        ));
    }

    #[test]
    fn a_turned_box_misses_a_diagonal_neighbor_its_aabb_would_hit() {
        // Two unit boxes 1.2 apart along x; the second turned 45 degrees reaches 0.707 along x.
        let tilted = ConvexPart {
            points: [-0.5f32, 0.5]
                .iter()
                .flat_map(|&x| [-0.5f32, 0.5].map(move |y| [-0.5f32, 0.5].map(move |z| [x + 1.2, y, z])))
                .flatten()
                .collect(),
            min: [0.7, -0.5, -0.5],
            max: [1.7, 0.5, 0.5],
            ..ConvexPart::default()
        };
        assert!(
            !gjk_intersects(&unit_box_at(0.0, 0.0, 0.0), &tilted),
            "0.5 + 0.5 < 1.2"
        );
        assert!(
            gjk_intersects(&unit_box_at(0.0, 0.0, 45.0), &tilted),
            "0.707 + 0.5 > 1.2"
        );
    }

    #[test]
    fn a_cylinder_and_a_triangle_follow_their_shapes() {
        // Upright cylinder radius 1, half height 1, at x = 2: its rim reaches x = 1.
        let cyl = ConvexPart {
            disks: vec![
                ([2.0, -1.0, 0.0], [0.0, 1.0, 0.0], 1.0),
                ([2.0, 1.0, 0.0], [0.0, 1.0, 0.0], 1.0),
            ],
            min: [1.0, -1.0, -1.0],
            max: [3.0, 1.0, 1.0],
            ..ConvexPart::default()
        };
        assert!(gjk_intersects(&unit_box_at(0.6, 0.0, 0.0), &cyl));
        assert!(!gjk_intersects(&unit_box_at(0.4, 0.0, 0.0), &cyl));
        // Off the rim's diagonal: the AABBs overlap, the box corner (1.25, 0.75) is 1.06 from the axis.
        assert!(!gjk_intersects(&unit_box_at(0.75, 1.25, 0.0), &cyl));
        assert!(
            gjk_intersects(&unit_box_at(0.85, 1.15, 0.0), &cyl),
            "corner 0.92 from the axis"
        );
        let tri = ConvexPart {
            points: vec![[0.0, 2.0, 0.0], [0.0, 0.4, 0.0], [1.0, 2.0, 0.0]],
            min: [0.0, 0.4, 0.0],
            max: [1.0, 2.0, 0.0],
            ..ConvexPart::default()
        };
        assert!(
            gjk_intersects(&unit_box_at(0.0, 0.0, 0.0), &tri),
            "its tip dips to y 0.4"
        );
        assert!(!gjk_intersects(&unit_box_at(-0.6, 0.0, 0.0), &tri));
    }
}
