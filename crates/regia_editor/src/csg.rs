// SPDX-License-Identifier: MPL-2.0
//! Bounded polygon BSP evaluation for editor blockout (ADR-7809).

use glam::{DMat4, DVec3};
use regia_ecs::prelude::*;

/// A blockout brush's primitive solid.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrushShape {
    /// Rectangular solid.
    #[default]
    Box,
    /// Elliptic cylinder along Y.
    Cylinder,
    /// Ramp rising along Z.
    Wedge,
    /// Steps rising along Z.
    Stairs,
    /// Ellipsoid.
    Sphere,
}

/// Ordered solid operation.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrushOp {
    /// Union with the accumulated solid.
    #[default]
    Add,
    /// Remove the brush's interior.
    Subtract,
    /// Keep the common interior.
    Intersect,
}

/// Immutable worker input, independent of the ECS.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Input {
    pub shape: BrushShape,
    pub op: BrushOp,
    pub order: i32,
    pub size: DVec3,
    pub steps: u32,
    pub transform: DMat4,
    pub material: u32,
}

/// Triangle surface plus one material slot per triangle.
#[derive(Clone, Debug, Default)]
pub(crate) struct Mesh {
    pub positions: Vec<DVec3>,
    pub triangles: Vec<[u32; 3]>,
    pub materials: Vec<u32>,
}

const EPS: f64 = 1e-8;
const MAX_DEPTH: usize = 256;
const MAX_FACES: usize = 16_384;

#[derive(Clone, Copy, Debug)]
struct Plane {
    normal: DVec3,
    offset: f64,
}

#[derive(Clone, Debug)]
struct Polygon {
    points: Vec<DVec3>,
    plane: Plane,
    material: u32,
}

impl Polygon {
    fn new(mut points: Vec<DVec3>, material: u32) -> Option<Self> {
        points.dedup_by(|a, b| a.distance_squared(*b) <= EPS * EPS);
        if points.len() > 1 && points[0].distance_squared(*points.last()?) <= EPS * EPS {
            points.pop();
        }
        if points.len() < 3 {
            return None;
        }
        let origin = points[0];
        let area: DVec3 = (1..points.len() - 1)
            .map(|i| (points[i] - origin).cross(points[i + 1] - origin))
            .sum();
        let length = area.length();
        if !length.is_finite() || length < EPS * EPS {
            return None;
        }
        let normal = area / length;
        Some(Self {
            points,
            plane: Plane {
                normal,
                offset: normal.dot(origin),
            },
            material,
        })
    }

    fn reverse(&mut self) {
        self.points.reverse();
        self.plane.normal = -self.plane.normal;
        self.plane.offset = -self.plane.offset;
    }
}

struct Budget<'a> {
    remaining: usize,
    cancelled: &'a dyn Fn() -> bool,
}
impl Budget<'_> {
    fn spend(&mut self, depth: usize) -> Result<(), String> {
        if (self.cancelled)() {
            return Err("blockout_cancelled".into());
        }
        if depth > MAX_DEPTH || self.remaining == 0 {
            return Err("blockout_complexity_limit".into());
        }
        self.remaining -= 1;
        Ok(())
    }
}

/// Splits by signed distance. Coplanar faces are returned separately so building and clipping
/// can choose their own boundary ownership. Intersections are the affine root of distance.
fn split(polygon: Polygon, plane: Plane) -> (Option<Polygon>, Option<Polygon>, Option<Polygon>) {
    let distances: Vec<_> = polygon
        .points
        .iter()
        .map(|p| plane.normal.dot(*p) - plane.offset)
        .collect();
    let positive = distances.iter().any(|d| *d > EPS);
    let negative = distances.iter().any(|d| *d < -EPS);
    match (positive, negative) {
        (false, false) => return (None, None, Some(polygon)),
        (true, false) => return (Some(polygon), None, None),
        (false, true) => return (None, Some(polygon), None),
        (true, true) => {}
    }
    let mut front = Vec::new();
    let mut back = Vec::new();
    for i in 0..polygon.points.len() {
        let j = (i + 1) % polygon.points.len();
        let (a, b, da, db) = (polygon.points[i], polygon.points[j], distances[i], distances[j]);
        if da >= -EPS {
            front.push(a);
        }
        if da <= EPS {
            back.push(a);
        }
        if (da > EPS && db < -EPS) || (da < -EPS && db > EPS) {
            // Use the same endpoint order on both sides of a shared edge for stable rounding.
            let p = if da < db {
                a + (b - a) * (da / (da - db))
            } else {
                b + (a - b) * (db / (db - da))
            };
            front.push(p);
            back.push(p);
        }
    }
    (
        Polygon::new(front, polygon.material),
        Polygon::new(back, polygon.material),
        None,
    )
}

/// A boundary BSP: absent positive children are exterior; absent negative children interior.
#[derive(Default)]
struct Node {
    plane: Option<Plane>,
    faces: Vec<Polygon>,
    front: Option<Box<Node>>,
    back: Option<Box<Node>>,
}
impl Node {
    fn insert(&mut self, faces: Vec<Polygon>, depth: usize, budget: &mut Budget<'_>) -> Result<(), String> {
        if faces.is_empty() {
            return Ok(());
        }
        budget.spend(depth)?;
        let plane = *self.plane.get_or_insert(faces[0].plane);
        let mut front = Vec::new();
        let mut back = Vec::new();
        for face in faces {
            budget.spend(depth)?;
            let (f, b, c) = split(face, plane);
            front.extend(f);
            back.extend(b);
            self.faces.extend(c);
        }
        if !front.is_empty() {
            self.front
                .get_or_insert_with(Default::default)
                .insert(front, depth + 1, budget)?;
        }
        if !back.is_empty() {
            self.back
                .get_or_insert_with(Default::default)
                .insert(back, depth + 1, budget)?;
        }
        Ok(())
    }

    fn outside(
        &self,
        faces: Vec<Polygon>,
        depth: usize,
        budget: &mut Budget<'_>,
    ) -> Result<Vec<Polygon>, String> {
        let Some(plane) = self.plane else {
            return Ok(faces);
        };
        let mut front = Vec::new();
        let mut back = Vec::new();
        for face in faces {
            budget.spend(depth)?;
            let (f, b, c) = split(face, plane);
            front.extend(f);
            back.extend(b);
            if let Some(c) = c {
                if c.plane.normal.dot(plane.normal) > 0.0 {
                    front.push(c);
                } else {
                    back.push(c);
                }
            }
        }
        if let Some(node) = &self.front {
            front = node.outside(front, depth + 1, budget)?;
        }
        if let Some(node) = &self.back {
            front.extend(node.outside(back, depth + 1, budget)?);
        }
        if front.len() > MAX_FACES {
            return Err("blockout_complexity_limit".into());
        }
        Ok(front)
    }

    fn clip(&mut self, other: &Self, budget: &mut Budget<'_>) -> Result<(), String> {
        self.faces = other.outside(std::mem::take(&mut self.faces), 0, budget)?;
        if let Some(node) = &mut self.front {
            node.clip(other, budget)?;
        }
        if let Some(node) = &mut self.back {
            node.clip(other, budget)?;
        }
        Ok(())
    }

    fn complement(&mut self) {
        for face in &mut self.faces {
            face.reverse();
        }
        if let Some(plane) = &mut self.plane {
            plane.normal = -plane.normal;
            plane.offset = -plane.offset;
        }
        if let Some(node) = &mut self.front {
            node.complement();
        }
        if let Some(node) = &mut self.back {
            node.complement();
        }
        std::mem::swap(&mut self.front, &mut self.back);
    }

    fn collect(self, out: &mut Vec<Polygon>) {
        out.extend(self.faces);
        if let Some(node) = self.front {
            node.collect(out);
        }
        if let Some(node) = self.back {
            node.collect(out);
        }
    }
}

fn combine(
    a: Vec<Polygon>,
    b: Vec<Polygon>,
    op: BrushOp,
    budget: &mut Budget<'_>,
) -> Result<Vec<Polygon>, String> {
    if a.is_empty() {
        return Ok(if op == BrushOp::Add { b } else { a });
    }
    if b.is_empty() {
        return Ok(if op == BrushOp::Intersect { b } else { a });
    }
    let mut left = Node::default();
    let mut right = Node::default();
    left.insert(a, 0, budget)?;
    right.insert(b, 0, budget)?;
    // A difference is the complement of (complement(A) union B); intersection uses De Morgan.
    // Boundary ownership: keep one copy of coincident aligned faces; remove opposing pairs.
    if op != BrushOp::Add {
        left.complement();
    }
    if op == BrushOp::Intersect {
        right.complement();
    }
    left.clip(&right, budget)?;
    right.clip(&left, budget)?;
    right.complement();
    right.clip(&left, budget)?;
    right.complement();
    let mut result = Vec::new();
    left.collect(&mut result);
    right.collect(&mut result);
    if op != BrushOp::Add {
        for face in &mut result {
            face.reverse();
        }
    }
    if result.len() > MAX_FACES {
        return Err("blockout_complexity_limit".into());
    }
    Ok(result)
}

fn face(points: Vec<DVec3>, inside: DVec3, material: u32, out: &mut Vec<Polygon>) {
    if let Some(mut p) = Polygon::new(points, material) {
        if p.plane.normal.dot(inside) > p.plane.offset {
            p.reverse();
        }
        out.push(p);
    }
}

fn box_faces(lo: DVec3, hi: DVec3, material: u32) -> Vec<Polygon> {
    let mut out = Vec::new();
    for axis in 0..3 {
        let u = (axis + 1) % 3;
        let v = (axis + 2) % 3;
        for side in [lo[axis], hi[axis]] {
            let mut points = Vec::new();
            for (a, b) in [(lo[u], lo[v]), (hi[u], lo[v]), (hi[u], hi[v]), (lo[u], hi[v])] {
                let mut p = DVec3::ZERO;
                p[axis] = side;
                p[u] = a;
                p[v] = b;
                points.push(p);
            }
            face(points, (lo + hi) * 0.5, material, &mut out);
        }
    }
    out
}

fn primitive(input: &Input, budget: &mut Budget<'_>) -> Result<Vec<Polygon>, String> {
    if !input.size.is_finite()
        || input.size.min_element() <= 1e-5
        || input.size.max_element() > 10_000.0
        || !input.transform.is_finite()
        || input.transform.determinant().abs() < 1e-12
        || !(1..=64).contains(&input.steps)
        || input.material > 255
    {
        return Err("blockout_invalid_brush".into());
    }
    let h = input.size * 0.5;
    let m = input.material;
    let mut out = Vec::new();
    match input.shape {
        BrushShape::Box => out = box_faces(-h, h, m),
        BrushShape::Cylinder => {
            const SEGMENTS: usize = 64;
            let ring = |y| {
                (0..SEGMENTS)
                    .map(|i| {
                        let a = i as f64 * std::f64::consts::TAU / SEGMENTS as f64;
                        DVec3::new(h.x * a.cos(), y, h.z * a.sin())
                    })
                    .collect::<Vec<_>>()
            };
            let low = ring(-h.y);
            let high = ring(h.y);
            face(low.clone(), DVec3::ZERO, m, &mut out);
            face(high.clone(), DVec3::ZERO, m, &mut out);
            for i in 0..SEGMENTS {
                let j = (i + 1) % SEGMENTS;
                face(vec![low[i], low[j], high[j], high[i]], DVec3::ZERO, m, &mut out);
            }
        }
        BrushShape::Wedge => {
            let left = vec![
                DVec3::new(-h.x, -h.y, -h.z),
                DVec3::new(-h.x, -h.y, h.z),
                DVec3::new(-h.x, h.y, h.z),
            ];
            let right: Vec<_> = left.iter().map(|p| DVec3::new(h.x, p.y, p.z)).collect();
            let inside = DVec3::new(0.0, -h.y / 3.0, h.z / 3.0);
            face(left.clone(), inside, m, &mut out);
            face(right.clone(), inside, m, &mut out);
            for i in 0..3 {
                let j = (i + 1) % 3;
                face(vec![left[i], left[j], right[j], right[i]], inside, m, &mut out);
            }
        }
        BrushShape::Stairs => {
            for i in 0..input.steps {
                let z = -h.z + input.size.z * i as f64 / input.steps as f64;
                let next_z = -h.z + input.size.z * (i + 1) as f64 / input.steps as f64;
                let y = -h.y + input.size.y * (i + 1) as f64 / input.steps as f64;
                let step = box_faces(DVec3::new(-h.x, -h.y, z), DVec3::new(h.x, y, next_z), m);
                out = combine(out, step, BrushOp::Add, budget)?;
            }
        }
        BrushShape::Sphere => {
            const RINGS: usize = 8;
            const SEGMENTS: usize = 16;
            let point = |r: usize, s: usize| {
                let p = r as f64 * std::f64::consts::PI / RINGS as f64;
                let a = s as f64 * std::f64::consts::TAU / SEGMENTS as f64;
                h * DVec3::new(p.sin() * a.cos(), p.cos(), p.sin() * a.sin())
            };
            for r in 0..RINGS {
                for s in 0..SEGMENTS {
                    face(
                        vec![point(r, s), point(r, s + 1), point(r + 1, s + 1), point(r + 1, s)],
                        DVec3::ZERO,
                        m,
                        &mut out,
                    );
                }
            }
        }
    }
    let mirrored = input.transform.determinant() < 0.0;
    let mut transformed = Vec::with_capacity(out.len());
    for polygon in out {
        let points: Vec<_> = polygon
            .points
            .into_iter()
            .map(|p| input.transform.transform_point3(p))
            .collect();
        if points
            .iter()
            .any(|p| !p.is_finite() || p.abs().max_element() > 1e6)
        {
            return Err("blockout_invalid_brush".into());
        }
        if let Some(mut polygon) = Polygon::new(points, m) {
            if mirrored {
                polygon.reverse();
            }
            transformed.push(polygon);
        }
    }
    Ok(transformed)
}

fn weld(points: &mut Vec<DVec3>, point: DVec3) -> u32 {
    if let Some(i) = points.iter().position(|p| p.distance_squared(point) <= EPS * EPS) {
        return i as u32;
    }
    let index = points.len() as u32;
    points.push(point);
    index
}

fn triangulate(polygons: Vec<Polygon>, budget: &mut Budget<'_>) -> Result<Mesh, String> {
    let mut mesh = Mesh::default();
    let mut faces = Vec::new();
    for p in polygons {
        budget.spend(0)?;
        let face: Vec<_> = p
            .points
            .into_iter()
            .map(|v| weld(&mut mesh.positions, v))
            .collect();
        if mesh.positions.len() > 8192 {
            return Err("blockout_complexity_limit".into());
        }
        faces.push((face, p.material));
    }
    let corner_count = mesh.positions.len();
    // BSP splits introduce T junctions: every face edge must share the same subdivision.
    // A centroid fan preserves collinear boundary corners without zero-area triangles.
    for (face, material) in faces {
        let mut boundary = Vec::new();
        for i in 0..face.len() {
            budget.spend(0)?;
            let (a, b) = (face[i], face[(i + 1) % face.len()]);
            let (pa, pb) = (mesh.positions[a as usize], mesh.positions[b as usize]);
            let d = pb - pa;
            let length = d.length_squared();
            if length <= EPS * EPS {
                continue;
            }
            let mut edge = vec![(0.0, a)];
            for (j, p) in mesh.positions[..corner_count].iter().enumerate() {
                let t = (*p - pa).dot(d) / length;
                if t > EPS && t < 1.0 - EPS && p.distance_squared(pa + d * t) <= EPS * EPS {
                    edge.push((t, j as u32));
                }
            }
            edge.sort_by(|a, b| a.0.total_cmp(&b.0));
            edge.dedup_by_key(|p| p.1);
            boundary.extend(edge.into_iter().map(|p| p.1));
        }
        if boundary.len() < 3 {
            continue;
        }
        let center: DVec3 = boundary
            .iter()
            .map(|i| mesh.positions[*i as usize])
            .sum::<DVec3>()
            / boundary.len() as f64;
        let c = weld(&mut mesh.positions, center);
        for i in 0..boundary.len() {
            let (a, b) = (boundary[i], boundary[(i + 1) % boundary.len()]);
            if (mesh.positions[a as usize] - center)
                .cross(mesh.positions[b as usize] - center)
                .length_squared()
                <= EPS.powi(4)
            {
                continue;
            }
            mesh.triangles.push([c, a, b]);
            mesh.materials.push(material);
        }
    }
    Ok(mesh)
}

#[cfg(test)]
pub(crate) fn evaluate(inputs: &[Input]) -> Result<Mesh, String> {
    evaluate_cancellable(inputs, &|| false)
}

pub(crate) fn evaluate_cancellable(inputs: &[Input], cancelled: &dyn Fn() -> bool) -> Result<Mesh, String> {
    let _span = tracing::info_span!("blockout_csg", brushes = inputs.len()).entered();
    if inputs.len() > 128 {
        return Err("blockout_complexity_limit".into());
    }
    let mut budget = Budget {
        remaining: 2_000_000,
        cancelled,
    };
    let mut ordered: Vec<_> = inputs.iter().collect();
    ordered.sort_by_key(|b| b.order);
    let mut solid = Vec::new();
    for input in ordered {
        let brush = primitive(input, &mut budget)?;
        solid = combine(solid, brush, input.op, &mut budget)?;
    }
    triangulate(solid, &mut budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn brush(shape: BrushShape, size: DVec3, op: BrushOp) -> Input {
        Input {
            shape,
            op,
            order: 0,
            size,
            steps: 6,
            transform: DMat4::IDENTITY,
            material: 0,
        }
    }

    pub(super) fn volume(mesh: &Mesh) -> f64 {
        mesh.triangles
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| mesh.positions[i as usize]);
                a.dot(b.cross(c)) / 6.0
            })
            .sum()
    }

    fn closed(mesh: &Mesh) {
        let mut edges = BTreeMap::<(u32, u32), (usize, i32)>::new();
        for [a, b, c] in &mesh.triangles {
            let [pa, pb, pc] = [*a, *b, *c].map(|i| mesh.positions[i as usize]);
            assert!((pb - pa).cross(pc - pa).length() > 1e-12, "degenerate triangle");
            for (x, y) in [(*a, *b), (*b, *c), (*c, *a)] {
                let entry = edges.entry((x.min(y), x.max(y))).or_default();
                entry.0 += 1;
                entry.1 += if x < y { 1 } else { -1 };
            }
        }
        for (edge, uses) in edges {
            assert_eq!(uses, (2, 0), "edge {edge:?}");
        }
        assert_eq!(mesh.materials.len(), mesh.triangles.len());
        assert!(mesh.positions.iter().all(|p| p.is_finite()));
    }

    #[test]
    fn csg_box_minus_cylinder_is_closed_and_has_expected_volume() {
        let a = brush(BrushShape::Box, DVec3::splat(2.0), BrushOp::Add);
        let mut b = brush(BrushShape::Cylinder, DVec3::new(1.0, 3.0, 1.0), BrushOp::Subtract);
        b.material = 7;
        let mesh = evaluate(&[a, b]).expect("difference");
        closed(&mesh);
        let expected = 8.0 - std::f64::consts::PI * 0.5;
        println!(
            "box minus cylinder: {} triangles, volume {:.9}, expected {:.9}, relative error {:.6}%",
            mesh.triangles.len(),
            volume(&mesh),
            expected,
            (volume(&mesh) - expected).abs() / expected * 100.0
        );
        assert!((volume(&mesh) - expected).abs() / expected < 0.005);
        assert!(
            (256..=4096).contains(&mesh.triangles.len()),
            "{} triangles",
            mesh.triangles.len()
        );
        assert!(
            mesh.materials.contains(&7),
            "cut faces retain the cutter's material"
        );
    }

    #[test]
    fn csg_every_brush_is_closed() {
        for shape in [
            BrushShape::Box,
            BrushShape::Cylinder,
            BrushShape::Wedge,
            BrushShape::Stairs,
            BrushShape::Sphere,
        ] {
            let mesh = evaluate(&[brush(shape, DVec3::splat(2.0), BrushOp::Add)]).expect("solid");
            closed(&mesh);
            assert!(volume(&mesh) > 0.0, "{shape:?}");
        }
    }

    #[test]
    fn csg_operations_obey_volume_and_empty_set_laws() {
        let a = brush(BrushShape::Box, DVec3::splat(2.0), BrushOp::Add);
        for (op, expected) in [
            (BrushOp::Add, 12.0),
            (BrushOp::Subtract, 4.0),
            (BrushOp::Intersect, 4.0),
        ] {
            let mut b = a.clone();
            b.op = op;
            b.transform = DMat4::from_translation(DVec3::X);
            let mesh = evaluate(&[a.clone(), b]).expect("overlap");
            closed(&mesh);
            assert!((volume(&mesh) - expected).abs() < 1e-7, "{op:?}");
        }
        for op in [BrushOp::Subtract, BrushOp::Intersect] {
            let mut b = a.clone();
            b.op = op;
            assert!(evaluate(&[b]).expect("empty").triangles.is_empty());
        }
        let mut b = a.clone();
        b.op = BrushOp::Subtract;
        assert!(evaluate(&[a, b]).expect("self subtraction").triangles.is_empty());
    }

    #[test]
    fn csg_random_boxes_never_panic_or_emit_non_finite_geometry() {
        // Deterministic property corpus, including degenerate and invalid extents; no dependency.
        let mut seed = 0x7809_u64;
        for index in 0_usize..256 {
            let mut next = || {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed >> 32) as u32 as f64 / u32::MAX as f64
            };
            let size = DVec3::new(next() * 4.0, next() * 4.0, next() * 4.0);
            let mut a = brush(BrushShape::Box, size, BrushOp::Add);
            let mut b = brush(BrushShape::Box, DVec3::splat(next() * 3.0), BrushOp::Subtract);
            b.transform = DMat4::from_translation(DVec3::new(next(), next(), next()));
            if index.is_multiple_of(7) {
                a.size.x = 0.0;
            }
            if index.is_multiple_of(11) {
                a.size.y = f64::NAN;
            }
            let invalid = !a.size.is_finite() || a.size.min_element() <= 1e-5;
            let result = evaluate(&[a, b]);
            if invalid {
                assert!(result.is_err());
            } else {
                closed(&result.expect("valid random boxes"));
            }
        }
    }
    #[test]
    fn csg_disjoint_touching_contained_and_mirrored_boxes() {
        for (offset, size, volumes) in [
            (4.0, 2.0, [16.0, 8.0, 0.0]),
            (2.0, 2.0, [16.0, 8.0, 0.0]),
            (0.0, 1.0, [8.0, 7.0, 1.0]),
            (0.0, 2.0, [8.0, 0.0, 8.0]),
        ] {
            for (op, expected) in [BrushOp::Add, BrushOp::Subtract, BrushOp::Intersect]
                .into_iter()
                .zip(volumes)
            {
                let a = brush(BrushShape::Box, DVec3::splat(2.0), BrushOp::Add);
                let mut b = brush(BrushShape::Box, DVec3::splat(size), op);
                b.transform = DMat4::from_translation(DVec3::new(offset, 0.0, 0.0));
                let mesh = evaluate(&[a, b]).expect("box boolean");
                closed(&mesh);
                assert!(
                    (volume(&mesh) - expected).abs() < 1e-7,
                    "{op:?} {offset} {size}: {}",
                    volume(&mesh)
                );
            }
        }
        let mut mirrored = brush(BrushShape::Box, DVec3::splat(2.0), BrushOp::Add);
        mirrored.transform = DMat4::from_scale(DVec3::new(-2.0, 1.0, 1.0));
        let mesh = evaluate(&[mirrored]).expect("mirror");
        closed(&mesh);
        assert!((volume(&mesh) - 16.0).abs() < 1e-7);
    }

    #[test]
    fn csg_order_and_work_budget_are_explicit() {
        let a = brush(BrushShape::Box, DVec3::splat(2.0), BrushOp::Add);
        let mut b = brush(BrushShape::Box, DVec3::ONE, BrushOp::Subtract);
        b.order = 1;
        let mesh = evaluate(&[b, a.clone()]).expect("ordered");
        closed(&mesh);
        assert!((volume(&mesh) - 7.0).abs() < 1e-7);
        assert_eq!(
            evaluate(&vec![a.clone(); 129]).expect_err("bounded").as_str(),
            "blockout_complexity_limit"
        );
        assert_eq!(
            evaluate_cancellable(&[a], &|| true)
                .expect_err("cancelled")
                .as_str(),
            "blockout_cancelled"
        );
    }
}
