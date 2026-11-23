// SPDX-License-Identifier: MPL-2.0
//! Helpers shared by the node families: selector reads and writes, region values, rotations.

use regia_core::math::Vec3;

use crate::rot::Rot as Quat;

use crate::data::{
    AttrColumn, AttrValue, Bundle, DataItem, Payload, PointSet, Selector, is_reserved_name, quat,
};
use crate::detmath;
use crate::nodes::NodeError;

/// Per-element numeric values of a selector: `width` components per element, flattened.
pub(crate) struct NumColumn {
    pub width: usize,
    pub values: Vec<f64>,
}

impl NumColumn {
    pub(crate) fn get(&self, i: usize) -> &[f64] {
        &self.values[i * self.width..(i + 1) * self.width]
    }
    pub(crate) fn scalar(&self, i: usize) -> f64 {
        self.values[i * self.width]
    }
    pub(crate) fn len(&self) -> usize {
        self.values.len().checked_div(self.width).unwrap_or(0)
    }
}

fn flat3(v: &[[f32; 3]]) -> Vec<f64> {
    v.iter().flatten().map(|x| f64::from(*x)).collect()
}
fn flat4(v: &[[f32; 4]]) -> Vec<f64> {
    v.iter().flatten().map(|x| f64::from(*x)).collect()
}

fn column_num(col: &AttrColumn, name: &str) -> Result<NumColumn, NodeError> {
    let width = col.ty().width().ok_or_else(|| {
        NodeError::new(
            "attr_type_mismatch",
            format!("`{name}` is a {} (not numeric)", col.ty().name()),
        )
    })?;
    let mut values = Vec::with_capacity(col.len() * width);
    for i in 0..col.len() {
        values.extend(col.components(i).unwrap_or_default());
    }
    Ok(NumColumn { width, values })
}

/// Reads a selector on an item as numbers (spec 62 § 3.5 conversions: bool → int → float widen,
/// a `:xyz` suffix picks parts, item scope broadcasts to every element).
pub(crate) fn read_num(item: &DataItem, sel: &Selector) -> Result<NumColumn, NodeError> {
    let sel = sel.resolve(item).map_err(|m| NodeError::new("attr_missing", m))?;
    let len = item.payload.len();
    let base = if sel.item_scope {
        let v = item
            .scope
            .get(&sel.name)
            .ok_or_else(|| NodeError::new("attr_missing", format!("item has no `{}`", sel.name)))?;
        let comps = v.components().ok_or_else(|| {
            NodeError::new(
                "attr_type_mismatch",
                format!("`item.{}` is not numeric", sel.name),
            )
        })?;
        let width = comps.len();
        NumColumn {
            width,
            values: (0..len).flat_map(|_| comps.iter().copied()).collect(),
        }
    } else {
        match &item.payload {
            Payload::Points(p) => points_num(p, &sel.name)?,
            Payload::Table(t) => {
                if sel.name == "index" {
                    NumColumn {
                        width: 1,
                        values: (0..t.rows).map(|i| i as f64).collect(),
                    }
                } else {
                    let col = t.column(&sel.name).ok_or_else(|| {
                        NodeError::new("attr_missing", format!("table has no column `{}`", sel.name))
                    })?;
                    column_num(col, &sel.name)?
                }
            }
            _ => return Err(NodeError::new("not_spatial", "this item has no elements to read")),
        }
    };
    if sel.parts.is_empty() {
        return Ok(base);
    }
    if base.width == 1 {
        return Err(NodeError::new(
            "attr_type_mismatch",
            format!("`{}` is a scalar; a component suffix needs a vector", sel.name),
        ));
    }
    if let Some(bad) = sel.parts.iter().find(|p| **p >= base.width) {
        return Err(NodeError::new(
            "attr_type_mismatch",
            format!(
                "`{}` has {} components; part {bad} does not exist",
                sel.name, base.width
            ),
        ));
    }
    let n = base.len();
    let mut values = Vec::with_capacity(n * sel.parts.len());
    for i in 0..n {
        let e = base.get(i);
        values.extend(sel.parts.iter().map(|p| e[*p]));
    }
    Ok(NumColumn {
        width: sel.parts.len(),
        values,
    })
}

fn points_num(p: &PointSet, name: &str) -> Result<NumColumn, NodeError> {
    let one = |v: Vec<f64>| NumColumn { width: 1, values: v };
    Ok(match name {
        "position" => NumColumn {
            width: 3,
            values: flat3(&p.position),
        },
        "rotation" => NumColumn {
            width: 4,
            values: flat4(&p.rotation),
        },
        "scale" => NumColumn {
            width: 3,
            values: flat3(&p.scale),
        },
        "color" => NumColumn {
            width: 4,
            values: flat4(&p.color),
        },
        "bounds_min" => NumColumn {
            width: 3,
            values: flat3(&p.bounds_min),
        },
        "bounds_max" => NumColumn {
            width: 3,
            values: flat3(&p.bounds_max),
        },
        "density" => one(p.density.iter().map(|x| f64::from(*x)).collect()),
        "hardness" => one(p.hardness.iter().map(|x| f64::from(*x)).collect()),
        "seed" => one(p.seed.iter().map(|x| f64::from(*x)).collect()),
        "index" => one((0..p.len()).map(|i| i as f64).collect()),
        other => {
            let col = p.attrs.get(other).ok_or_else(|| {
                NodeError::new("attr_missing", format!("points have no attribute `{other}`"))
            })?;
            column_num(col, other)?
        }
    })
}

fn column_of_width(width: usize, values: &[f64]) -> AttrColumn {
    match width {
        1 => AttrColumn::Float(values.to_vec().into()),
        2 => AttrColumn::Vec2(
            (0..values.len() / 2)
                .map(|i| [values[2 * i], values[2 * i + 1]])
                .collect(),
        ),
        3 => AttrColumn::Vec3(
            (0..values.len() / 3)
                .map(|i| [values[3 * i], values[3 * i + 1], values[3 * i + 2]])
                .collect(),
        ),
        _ => AttrColumn::Vec4(
            (0..values.len() / 4)
                .map(|i| {
                    [
                        values[4 * i],
                        values[4 * i + 1],
                        values[4 * i + 2],
                        values[4 * i + 3],
                    ]
                })
                .collect(),
        ),
    }
}

/// Writes numbers to a built-in property or custom attribute of a point set or table (creating it),
/// and records it as the item's `$prev`.
pub(crate) fn write_num(
    item: &mut DataItem,
    target: &str,
    width: usize,
    values: &[f64],
) -> Result<(), NodeError> {
    if values.iter().any(|v| !v.is_finite()) {
        let i = values.iter().position(|v| !v.is_finite()).unwrap_or(0) / width.max(1);
        return Err(NodeError::new(
            "attr_nan",
            format!("`{target}` would be non-finite at element {i}"),
        ));
    }
    let name = target.strip_prefix("item.");
    if let Some(name) = name {
        if is_reserved_name(name) {
            return Err(NodeError::new(
                "attr_reserved_name",
                format!("`{name}` is reserved"),
            ));
        }
        // An empty element set can arrive with a wide column (`position` of no points is width
        // 3 with no values): missing components are zero rather than an index past the end.
        let at = |k: usize| values.get(k).copied().unwrap_or(0.0);
        let v = match width {
            1 => AttrValue::Float(at(0)),
            2 => AttrValue::Vec2([at(0), at(1)]),
            3 => AttrValue::Vec3([at(0), at(1), at(2)]),
            _ => AttrValue::Vec4([at(0), at(1), at(2), at(3)]),
        };
        item.scope.insert(name.to_string(), v);
        item.last_written = Some(target.to_string());
        return Ok(());
    }
    match &mut item.payload {
        Payload::Points(p) => write_points(p, target, width, values)?,
        Payload::Table(t) => {
            if t.rows * width != values.len() {
                return Err(NodeError::new("attr_type_mismatch", "row count mismatch"));
            }
            t.set_column(target, column_of_width(width, values));
        }
        _ => {
            return Err(NodeError::new(
                "not_spatial",
                "this item has no elements to write",
            ));
        }
    }
    item.last_written = Some(target.to_string());
    Ok(())
}

fn write_points(p: &mut PointSet, target: &str, width: usize, values: &[f64]) -> Result<(), NodeError> {
    let n = p.len();
    let get = |i: usize, k: usize| -> f32 {
        // Broadcast a scalar to every component.
        if width == 1 {
            values[i] as f32
        } else {
            values[i * width + k.min(width - 1)] as f32
        }
    };
    if values.len() != n * width {
        return Err(NodeError::new(
            "attr_type_mismatch",
            format!("`{target}`: element count mismatch"),
        ));
    }
    let need = |w: usize| -> Result<(), NodeError> {
        if width == 1 || width == w {
            Ok(())
        } else {
            Err(NodeError::new(
                "attr_type_mismatch",
                format!("`{target}` takes {w} components, got {width}"),
            ))
        }
    };
    match target {
        "position" | "scale" | "bounds_min" | "bounds_max" => {
            need(3)?;
            let col = match target {
                "position" => &mut p.position,
                "scale" => &mut p.scale,
                "bounds_min" => &mut p.bounds_min,
                _ => &mut p.bounds_max,
            };
            for (i, v) in col.iter_mut().enumerate() {
                *v = [get(i, 0), get(i, 1), get(i, 2)];
            }
        }
        "rotation" | "color" => {
            need(4)?;
            let col = if target == "rotation" {
                &mut p.rotation
            } else {
                &mut p.color
            };
            for (i, v) in col.iter_mut().enumerate() {
                *v = [get(i, 0), get(i, 1), get(i, 2), get(i, 3)];
            }
        }
        "density" | "hardness" => {
            need(1)?;
            let col = if target == "density" {
                &mut p.density
            } else {
                &mut p.hardness
            };
            for (i, v) in col.iter_mut().enumerate() {
                *v = get(i, 0).clamp(0.0, 1.0);
            }
        }
        "seed" => {
            need(1)?;
            for (i, v) in p.seed.iter_mut().enumerate() {
                *v = values[i] as i64 as u32;
            }
        }
        "index" | "count" => {
            return Err(NodeError::new(
                "attr_reserved_name",
                format!("`{target}` is read-only"),
            ));
        }
        name if is_reserved_name(name) => {
            return Err(NodeError::new(
                "attr_reserved_name",
                format!("`{name}` is reserved"),
            ));
        }
        name => {
            p.attrs.insert(name.to_string(), column_of_width(width, values));
        }
    }
    Ok(())
}

/// Whether a spline length can be sampled: finite and positive. A NaN length (a NaN control
/// point) fails `<= 0.0` as false and would reach `f64::clamp`, which aborts on a NaN bound.
pub(crate) fn has_length(len: f64) -> bool {
    len.is_finite() && len > 0.0
}

/// The XZ bounds `[min_x, min_z, max_x, max_z]` of a spatial item (`None` for tables and empty sets).
pub(crate) fn xz_bounds(item: &DataItem) -> Option<[f32; 4]> {
    let (lo, hi) = world_bounds(item)?;
    Some([lo[0], lo[2], hi[0], hi[2]])
}

/// The world AABB of a spatial item.
pub(crate) fn world_bounds(item: &DataItem) -> Option<([f32; 3], [f32; 3])> {
    match &item.payload {
        Payload::Points(p) => {
            if p.is_empty() {
                return None;
            }
            let mut lo = [f32::INFINITY; 3];
            let mut hi = [f32::NEG_INFINITY; 3];
            for i in 0..p.len() {
                let (a, b) = p.world_aabb(i);
                lo = std::array::from_fn(|k| lo[k].min(a[k]));
                hi = std::array::from_fn(|k| hi[k].max(b[k]));
            }
            Some((lo, hi))
        }
        Payload::Shape(s) => Some(s.world_aabb()),
        Payload::Ground(g) => {
            let (min_h, max_h) = g
                .heights
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), h| {
                    (a.min(*h), b.max(*h))
                });
            Some(([g.rect[0], min_h, g.rect[1]], [g.rect[2], max_h, g.rect[3]]))
        }
        Payload::Mask(m) => Some(([m.rect[0], 0.0, m.rect[1]], [m.rect[2], 0.0, m.rect[3]])),
        Payload::Mesh(m) => Some(m.bounds()),
        Payload::Spline(sp) => {
            if sp.spline.points.is_empty() {
                return None;
            }
            let (lo, hi) = sp.curve().bounds();
            Some((lo.to_array(), hi.to_array()))
        }
        Payload::Polygon(p) => {
            let b = p.bounds()?;
            let y = p.origin[1] as f32;
            Some(([b[0] as f32, y, b[1] as f32], [b[2] as f32, y, b[3] as f32]))
        }
        Payload::PlantSkeleton(s) => Some(s.bounds()),
        Payload::PlantMesh(m) => Some(m.bounds()),
        Payload::Table(_) => None,
    }
}

/// How several region items combine (spec 63 § 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Combine {
    Max,
    Add,
    Any,
}

impl Combine {
    pub(crate) fn from_name(n: &str) -> Self {
        match n {
            "add" => Self::Add,
            "any" => Self::Any,
            _ => Self::Max,
        }
    }
}

/// A uniform grid over point boxes for region queries.
struct PointGrid {
    cell: f32,
    origin: [f32; 2],
    dims: [usize; 2],
    cells: Vec<Vec<u32>>,
}

impl PointGrid {
    fn build(p: &PointSet) -> Self {
        let n = p.len();
        let boxes: Vec<_> = (0..n).map(|i| p.world_aabb(i)).collect();
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        let mut size_sum = 0.0f64;
        for (a, b) in &boxes {
            lo = [lo[0].min(a[0]), lo[1].min(a[2])];
            hi = [hi[0].max(b[0]), hi[1].max(b[2])];
            size_sum += f64::from((b[0] - a[0]).max(b[2] - a[2]));
        }
        let avg = if n > 0 { (size_sum / n as f64) as f32 } else { 1.0 };
        let extent = (hi[0] - lo[0]).max(hi[1] - lo[1]).max(1e-3);
        // Cells about two boxes wide, at most 512 per side.
        let cell = (avg * 2.0).max(extent / 512.0).max(1e-3);
        let dims = [
            (((hi[0] - lo[0]) / cell).floor() as usize + 1).min(513),
            (((hi[1] - lo[1]) / cell).floor() as usize + 1).min(513),
        ];
        let mut cells = vec![Vec::new(); dims[0] * dims[1]];
        let cl = |v: f32, o: f32, d: usize| (((v - o) / cell).floor().max(0.0) as usize).min(d - 1);
        for (i, (a, b)) in boxes.iter().enumerate() {
            for z in cl(a[2], lo[1], dims[1])..=cl(b[2], lo[1], dims[1]) {
                for x in cl(a[0], lo[0], dims[0])..=cl(b[0], lo[0], dims[0]) {
                    cells[z * dims[0] + x].push(i as u32);
                }
            }
        }
        Self {
            cell,
            origin: lo,
            dims,
            cells,
        }
    }

    fn candidates(&self, p: [f32; 3]) -> &[u32] {
        let x = (p[0] - self.origin[0]) / self.cell;
        let z = (p[2] - self.origin[1]) / self.cell;
        if x.is_nan() || z.is_nan() || x < 0.0 || z < 0.0 {
            return &[];
        }
        let (x, z) = (x.floor() as usize, z.floor() as usize);
        if x >= self.dims[0] || z >= self.dims[1] {
            return &[];
        }
        &self.cells[z * self.dims[0] + x]
    }
}

enum RegionPart<'a> {
    Points(&'a PointSet, PointGrid),
    /// Boxed: the curve's tables make it far larger than the other parts.
    Spline(Box<SplineRegion>),
    Item(&'a DataItem),
}

/// A spline prepared as a region (spec 63 § 2): a closed spline is its XZ polygon, an open one a
/// band of half its interpolated width (`width` value, else 1 m × `scale.x`).
pub(crate) struct SplineRegion {
    curve: regia_scene::spline::SplineCurve,
    width_attr: Option<usize>,
    polygon: Option<crate::polygon::PolygonData>,
}

impl SplineRegion {
    pub(crate) fn new(sp: &crate::data::SplineData) -> Self {
        let curve = sp.curve();
        let polygon = if sp.spline.closed {
            let ring: Vec<[f64; 2]> = curve
                .to_polyline(0.05)
                .iter()
                .map(|p| [f64::from(p.x), f64::from(p.z)])
                .collect();
            crate::polygon::PolygonData::new([0.0, 0.0, 0.0], &[ring]).ok()
        } else {
            None
        };
        Self {
            width_attr: sp.spline.attr_index("width"),
            curve,
            polygon,
        }
    }

    pub(crate) fn value(&self, p: [f32; 3]) -> f32 {
        if let Some(poly) = &self.polygon {
            return if poly.contains(f64::from(p[0]), f64::from(p[2])) {
                1.0
            } else {
                0.0
            };
        }
        let hit = self.curve.closest(Vec3::from(p));
        let s = self.curve.at_key(hit.key);
        let width = match self.width_attr.and_then(|i| s.values.get(i)) {
            Some(regia_scene::spline::SplineValue::F32(w)) => *w,
            _ => s.scale.x,
        };
        if hit.distance_m <= width * 0.5 { 1.0 } else { 0.0 }
    }
}

/// A prepared region bundle: `value(p)` combines its items by [`Combine`].
pub(crate) struct Region<'a> {
    parts: Vec<RegionPart<'a>>,
    combine: Combine,
}

impl<'a> Region<'a> {
    /// Prepares the items of a region pin.
    ///
    /// # Errors
    ///
    /// `not_spatial` for table items.
    pub(crate) fn new(bundle: &'a Bundle, combine: Combine) -> Result<Self, NodeError> {
        let mut parts = Vec::with_capacity(bundle.0.len());
        for item in &bundle.0 {
            match &item.payload {
                Payload::Points(p) => parts.push(RegionPart::Points(p, PointGrid::build(p))),
                Payload::Spline(sp) => parts.push(RegionPart::Spline(Box::new(SplineRegion::new(sp)))),
                Payload::Table(_) => return Err(NodeError::new("not_spatial", "a table is not a region")),
                _ => parts.push(RegionPart::Item(item)),
            }
        }
        Ok(Self { parts, combine })
    }

    /// The combined region value at `p`, in [0, 1].
    pub(crate) fn value(&self, p: [f32; 3]) -> f32 {
        let mut acc = 0.0f32;
        for part in &self.parts {
            let v = match part {
                RegionPart::Points(set, grid) => grid
                    .candidates(p)
                    .iter()
                    .map(|&j| set.contains(j as usize, p))
                    .fold(0.0, f32::max),
                RegionPart::Spline(r) => r.value(p),
                RegionPart::Item(item) => item_value(item, p),
            };
            acc = match self.combine {
                Combine::Max => acc.max(v),
                Combine::Add => (acc + v).min(1.0),
                Combine::Any => {
                    if v > 0.0 {
                        1.0
                    } else {
                        acc
                    }
                }
            };
        }
        acc
    }
}

fn item_value(item: &DataItem, p: [f32; 3]) -> f32 {
    match &item.payload {
        Payload::Shape(s) => s.value(p),
        Payload::Ground(g) => {
            let r = g.rect;
            if p[0] >= r[0] && p[0] <= r[2] && p[2] >= r[1] && p[2] <= r[3] {
                1.0
            } else {
                0.0
            }
        }
        Payload::Points(set) => (0..set.len()).map(|j| set.contains(j, p)).fold(0.0, f32::max),
        Payload::Mask(m) => m.sample(p[0], p[2]),
        Payload::Polygon(poly) => {
            if poly.contains(f64::from(p[0]), f64::from(p[2])) {
                1.0
            } else {
                0.0
            }
        }
        Payload::Spline(sp) => SplineRegion::new(sp).value(p),
        Payload::Table(_) | Payload::Mesh(_) | Payload::PlantSkeleton(_) | Payload::PlantMesh(_) => 0.0,
    }
}

/// A quaternion from an axis and an angle in radians, through `detmath`.
pub(crate) fn axis_angle(axis: Vec3, radians: f64) -> Quat {
    let (s, c) = detmath::sin_cos(radians * 0.5);
    let s = s as f32;
    Quat::from_xyzw(axis.x * s, axis.y * s, axis.z * s, c as f32)
}

/// Rotation from Euler degrees `(pitch X, yaw Y, roll Z)`, applied yaw, then pitch, then roll
/// (`q = yaw · pitch · roll`), through `detmath`.
pub(crate) fn euler_deg(deg: [f64; 3]) -> Quat {
    let r = std::f64::consts::PI / 180.0;
    axis_angle(Vec3::Y, deg[1] * r) * axis_angle(Vec3::X, deg[0] * r) * axis_angle(Vec3::Z, deg[2] * r)
}

/// The shortest rotation taking `from` to `to` (both unit), normalized through
/// [`crate::rot::Rot::normalize`] — scalar f32 in one fixed order, with its degenerate and
/// non-finite guard. `audit-g-procgen2-determinism` finding 2 was a non-bug: `Quat` here is
/// `crate::rot::Rot`, whose `length`/`normalize` already were exactly this scalar code, so glam's
/// SIMD `Quat::normalize` was never on this path; the normalize stays the single `Rot`
/// implementation instead of an inline copy that would drop the guard.
pub(crate) fn rotation_between(from: Vec3, to: Vec3) -> Quat {
    let d = from.dot(to);
    if d < -0.999_999 {
        // Opposite: half-turn about any perpendicular axis.
        let axis = if from.x.abs() < 0.9 {
            Vec3::X.cross(from)
        } else {
            Vec3::Z.cross(from)
        };
        let axis = axis / axis.length().max(1e-12);
        return Quat::from_xyzw(axis.x, axis.y, axis.z, 0.0);
    }
    let c = from.cross(to);
    Quat::from_xyzw(c.x, c.y, c.z, 1.0 + d).normalize()
}

/// A point set's rotation as a quaternion.
pub(crate) fn rot(p: &PointSet, i: usize) -> Quat {
    quat(p.rotation[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rotation_between` must keep `Rot::normalize`'s guard: a degenerate input whose squares
    /// overflow f32 yields `IDENTITY`, not a zero quaternion. `audit-g-procgen2-determinism`
    /// finding 2 was a non-bug (`Quat` here is `crate::rot::Rot`, whose `normalize` already was
    /// this scalar code), but the `is_finite` guard the inline form dropped must stay.
    #[test]
    fn rotation_between_keeps_the_normalize_guard_for_overflowing_inputs() {
        let q = rotation_between(Vec3::X, Vec3::new(1.0e20, 0.0, 0.0));
        assert_eq!(q, Quat::IDENTITY);
    }
}
