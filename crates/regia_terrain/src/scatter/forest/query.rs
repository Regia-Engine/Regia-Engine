// SPDX-License-Identifier: MPL-2.0
//! Scatter instances in a box or along a segment, straight from the deterministic placement
//! (ADR-1503): no tile has to be resident, so a path compiler or the corridor tool can ask about
//! a flight kilometres ahead of the camera.
//!
//! Each query places only the cells its region touches ([`place_class_cell_with_origin`] is a pure
//! function of the set, the terrain, the cell and the origin), seats every instance the way the
//! scatter field does (the class `sink` plus the tilt residual, along the instance's up axis) and
//! measures it with its mesh's [`ObstacleShape`]. The answer is identical to what the field streams
//! for the same set and terrain, on every run and in any order, provided the query is given the
//! field's [`PlacementOrigin`] ([`ScatterQuery::with_placement_origin`]; the default is the map
//! edge, as the field's).
//!
//! The shape follows the continuous-flight lane's scatter query brief: instances with positions
//! and heights in a box or along a segment. When that lane's query lands on the integration
//! branch, the two meet at [`ScatterHit`] (positions, heights, radii) and the lead keeps one.

use std::collections::HashMap;

use super::super::ScatterError;
use super::super::field::{ScatterLibrary, ScatterSource};
use super::super::placement::{
    DensityMap, PlacedInstance, PlacementGrid, PlacementOrigin, place_class_cell_with_origin,
};
use super::super::set::{ScatterClass, ScatterSet};
use super::shape::{ObstaclePart, ObstacleShape, WorldObstacle, dot, point_segment, scale3, sub};

/// One placed scatter instance, measured for clearance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScatterHit {
    /// Index of the class in the set.
    pub class: u16,
    /// Index of the mesh in the class.
    pub mesh: u16,
    /// Stable id of the instance: the class name, its placement cell and its ordinal in the
    /// cell, hashed (the same instance has the same id in every query and on every run; it is
    /// not the renderer's object key).
    pub id: u64,
    /// Placement cell of the instance (class grid).
    pub cell: [i32; 2],
    /// Ground contact point, world metres.
    pub position: [f32; 3],
    /// Height of the instance's top above its ground contact, metres.
    pub height: f32,
    /// Uniform scale.
    pub scale: f32,
    /// Trunk and crown in world space.
    pub obstacle: WorldObstacle,
}

impl ScatterHit {
    /// Horizontal reach of the instance from its axis, metres (crown or trunk radius).
    #[must_use]
    pub fn radius(&self) -> f32 {
        self.obstacle.horizontal_reach()
    }
}

/// A [`ScatterHit`] near a segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentHit {
    /// The instance.
    pub hit: ScatterHit,
    /// Clearance from the segment to the instance's surface, metres (negative = the segment
    /// passes through it). A lower bound for the crown (see `shape.rs`).
    pub distance: f32,
    /// The part the clearance is measured to.
    pub part: ObstaclePart,
    /// Segment parameter (0..1) of the nearest approach.
    pub t: f32,
}

/// Per class: how far an instance can reach from its cell (largest horizontal reach × scale) and
/// how high it can stand.
#[derive(Clone, Copy, Debug, Default)]
struct ClassReach {
    horizontal: f32,
    top: f32,
}

/// Placement-backed scatter queries over one set and terrain.
#[derive(Clone)]
pub struct ScatterQuery {
    set: ScatterSet,
    source: ScatterSource,
    /// `shapes[class][mesh]`.
    shapes: Vec<Vec<ObstacleShape>>,
    reach: Vec<ClassReach>,
    name_hashes: Vec<u64>,
    /// Where the placement cell grids are anchored (Q-08, ADR-8496).
    origin: PlacementOrigin,
}

/// One class's placement cells over the map under a [`PlacementOrigin`] ([`PlacementGrid`]),
/// with the inclusive last index per axis.
struct CellSpan {
    grid: PlacementGrid,
    last: [i32; 2],
}

impl CellSpan {
    /// `None` when no cell touches the map.
    fn of(map: &crate::sampler::TerrainMap, cell_size: f32, origin: &PlacementOrigin) -> Option<Self> {
        let grid = PlacementGrid::new(map, cell_size, origin);
        if grid.end[0] <= grid.first[0] || grid.end[1] <= grid.first[1] {
            return None;
        }
        Some(Self {
            grid,
            last: [grid.end[0] - 1, grid.end[1] - 1],
        })
    }

    /// The cell holding world coordinate `v` on `axis` (0 = X, 1 = Z), clamped to the map's
    /// cells. Under [`PlacementOrigin::MapEdge`] the corner is `-half`, and `v - (-half)` is
    /// `v + half` exactly, so the edge-anchored cells are those of every earlier release.
    fn index(&self, v: f32, axis: usize) -> i32 {
        (((v - self.grid.corner[axis]) / self.grid.cell_size).floor() as i32)
            .clamp(self.grid.first[axis], self.last[axis])
    }
}

/// FNV-1a of a class name.
fn fnv(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn instance_id(name_hash: u64, cx: i32, cz: i32, ordinal: usize) -> u64 {
    let mut h = name_hash ^ 0x6a09_e667_f3bc_c908;
    for v in [u64::from(cx as u32), u64::from(cz as u32), ordinal as u64] {
        h ^= v;
        h = h.wrapping_mul(0x0100_0000_01b3).rotate_left(27);
        h ^= h >> 29;
    }
    h
}

impl ScatterQuery {
    /// Measures every mesh the set names from `library` and keeps the set and terrain.
    ///
    /// # Errors
    ///
    /// [`ScatterError::Asset`] when a mesh or a density map the set names was not supplied.
    pub fn new(
        set: &ScatterSet,
        source: ScatterSource,
        library: &ScatterLibrary,
    ) -> Result<Self, ScatterError> {
        let mut cache: HashMap<&str, ObstacleShape> = HashMap::new();
        let mut shapes = Vec::with_capacity(set.classes.len());
        let mut reach = Vec::with_capacity(set.classes.len());
        for class in &set.classes {
            let mut row = Vec::with_capacity(class.meshes.len());
            let mut r = ClassReach::default();
            for mesh in &class.meshes {
                let shape = match cache.get(mesh.mesh.as_str()) {
                    Some(s) => *s,
                    None => {
                        let asset = library.get(&mesh.mesh).ok_or_else(|| ScatterError::Asset {
                            path: mesh.mesh.clone(),
                            reason: format!("mesh of class '{}' was not supplied", class.name),
                        })?;
                        let s = ObstacleShape::of_asset(asset);
                        cache.insert(mesh.mesh.as_str(), s);
                        s
                    }
                };
                let scale = mesh.max_scale.max(mesh.min_scale);
                r.horizontal = r.horizontal.max(shape.reach * scale);
                r.top = r.top.max(shape.top * scale);
                row.push(shape);
            }
            if let Some(path) = &class.density_map
                && !source.density_maps.contains_key(path)
            {
                return Err(ScatterError::Asset {
                    path: path.clone(),
                    reason: format!("density map of class '{}' was not supplied", class.name),
                });
            }
            shapes.push(row);
            reach.push(r);
        }
        Ok(Self {
            name_hashes: set.classes.iter().map(|c| fnv(&c.name)).collect(),
            set: set.clone(),
            source,
            shapes,
            reach,
            origin: PlacementOrigin::MapEdge,
        })
    }

    /// [`Self::set_placement_origin`] as a builder step after [`Self::new`].
    #[must_use]
    pub fn with_placement_origin(mut self, origin: PlacementOrigin) -> Self {
        self.set_placement_origin(origin);
        self
    }

    /// Anchors every class's placement cell grid at `origin`, as
    /// [`ScatterField::set_placement_origin`](super::super::field::ScatterField::set_placement_origin)
    /// does (Q-08, ADR-8496): a query answers with what a field of the same set, terrain and
    /// origin streams. The default, [`PlacementOrigin::MapEdge`], is every earlier release's
    /// placement. Under [`PlacementOrigin::World`] cell indices ([`ScatterHit::cell`],
    /// [`Self::cell_range`], [`Self::cell_hits`]) count from the anchor and may be negative.
    pub fn set_placement_origin(&mut self, origin: PlacementOrigin) {
        self.origin = origin;
    }

    /// Where the query's placement cell grids are anchored.
    #[must_use]
    pub fn placement_origin(&self) -> PlacementOrigin {
        self.origin
    }

    /// The set the query places.
    #[must_use]
    pub fn set(&self) -> &ScatterSet {
        &self.set
    }

    /// The terrain and dressing the query places on.
    #[must_use]
    pub fn source(&self) -> &ScatterSource {
        &self.source
    }

    /// The measured shape of mesh `mesh` of class `class`.
    #[must_use]
    pub fn shape(&self, class: usize, mesh: usize) -> Option<&ObstacleShape> {
        self.shapes.get(class).and_then(|row| row.get(mesh))
    }

    /// Name of class `class`.
    #[must_use]
    pub fn class_name(&self, class: u16) -> &str {
        self.set
            .classes
            .get(usize::from(class))
            .map_or("", |c| c.name.as_str())
    }

    /// Index of the class named `name`.
    #[must_use]
    pub fn class_index(&self, name: &str) -> Option<u16> {
        self.set
            .classes
            .iter()
            .position(|c| c.name == name)
            .map(|i| i as u16)
    }

    /// Largest horizontal reach (metres) and height of any instance of class `class`.
    #[must_use]
    pub fn class_extent(&self, class: u16) -> (f32, f32) {
        self.reach
            .get(usize::from(class))
            .map_or((0.0, 0.0), |r| (r.horizontal, r.top))
    }

    /// Replaces the set (after a corridor carve), keeping the terrain and the measured shapes.
    /// The new set must name the same classes and meshes in the same order (a carve only adds
    /// exclusions).
    ///
    /// # Errors
    ///
    /// [`ScatterError::Class`] when the classes or their meshes differ.
    pub fn replace_set(&mut self, set: ScatterSet) -> Result<(), ScatterError> {
        let same = set.classes.len() == self.set.classes.len()
            && set
                .classes
                .iter()
                .zip(&self.set.classes)
                .all(|(a, b)| a.name == b.name && a.meshes == b.meshes);
        if !same {
            return Err(ScatterError::Class {
                class: set.classes.first().map_or_else(String::new, |c| c.name.clone()),
                reason: "a replacement set must keep the classes and meshes (only exclusions change)".into(),
            });
        }
        self.set = set;
        Ok(())
    }

    fn density(&self, class: &ScatterClass) -> Option<&DensityMap> {
        class
            .density_map
            .as_ref()
            .and_then(|p| self.source.density_maps.get(p))
    }

    /// Calls `visit` for every instance of the enabled classes (`classes`: indices, `None` =
    /// every class) whose placement cell lies within `margin` metres plus the class's reach of
    /// the XZ rectangle `min..max`. Cells are visited class by class, row-major; instances in
    /// cell order. The caller filters exactly.
    pub fn visit_rect(
        &self,
        min: [f32; 2],
        max: [f32; 2],
        margin: f32,
        classes: Option<&[u16]>,
        mut visit: impl FnMut(&ScatterHit),
    ) {
        let map = &*self.source.map;
        let half = map.world_width() * 0.5;
        let inputs = self.source_inputs();
        let mut placed: Vec<PlacedInstance> = Vec::new();
        for (ci, class) in self.set.classes.iter().enumerate() {
            if classes.is_some_and(|c| !c.contains(&(ci as u16))) || class.meshes.is_empty() {
                continue;
            }
            let pad = margin.max(0.0) + self.reach[ci].horizontal;
            if max[0] + pad < -half || max[1] + pad < -half || min[0] - pad > half || min[1] - pad > half {
                continue;
            }
            let Some(cells) = CellSpan::of(map, class.cell_size, &self.origin) else {
                continue;
            };
            let density = self.density(class);
            for cz in cells.index(min[1] - pad, 1)..=cells.index(max[1] + pad, 1) {
                for cx in cells.index(min[0] - pad, 0)..=cells.index(max[0] + pad, 0) {
                    placed.clear();
                    place_class_cell_with_origin(
                        self.set.seed,
                        ci,
                        class,
                        density,
                        inputs,
                        &self.origin,
                        cx,
                        cz,
                        &mut placed,
                    );
                    for (ordinal, p) in placed.iter().enumerate() {
                        if let Some(hit) = self.hit_of(ci, class, p, cx, cz, ordinal) {
                            visit(&hit);
                        }
                    }
                }
            }
        }
    }

    /// The placement cells of class `class` within `margin` metres plus the class's reach of the
    /// XZ rectangle `min..max`, as an inclusive `(lo, hi)` cell range (`None` when the rectangle
    /// misses the terrain or the class does not exist).
    #[must_use]
    pub fn cell_range(
        &self,
        class: u16,
        min: [f32; 2],
        max: [f32; 2],
        margin: f32,
    ) -> Option<([i32; 2], [i32; 2])> {
        let ci = usize::from(class);
        let c = self.set.classes.get(ci)?;
        let half = self.source.map.world_width() * 0.5;
        let pad = margin.max(0.0) + self.reach[ci].horizontal;
        if max[0] + pad < -half || max[1] + pad < -half || min[0] - pad > half || min[1] - pad > half {
            return None;
        }
        let cells = CellSpan::of(&self.source.map, c.cell_size, &self.origin)?;
        Some((
            [cells.index(min[0] - pad, 0), cells.index(min[1] - pad, 1)],
            [cells.index(max[0] + pad, 0), cells.index(max[1] + pad, 1)],
        ))
    }

    /// The instances of cell `(cx, cz)` of class `class`, appended to `out` in placement order.
    pub fn cell_hits(&self, class: u16, cx: i32, cz: i32, out: &mut Vec<ScatterHit>) {
        let ci = usize::from(class);
        let Some(c) = self.set.classes.get(ci) else { return };
        if c.meshes.is_empty() {
            return;
        }
        let mut placed = Vec::new();
        place_class_cell_with_origin(
            self.set.seed,
            ci,
            c,
            self.density(c),
            self.source_inputs(),
            &self.origin,
            cx,
            cz,
            &mut placed,
        );
        for (ordinal, p) in placed.iter().enumerate() {
            if let Some(hit) = self.hit_of(ci, c, p, cx, cz, ordinal) {
                out.push(hit);
            }
        }
    }

    fn source_inputs(&self) -> super::super::placement::PlacementInputs<'_> {
        super::super::placement::PlacementInputs {
            map: &self.source.map,
            holes: self.source.holes.as_ref(),
            layers: self.source.layers.as_ref(),
            density_maps: Some(&self.source.density_maps),
        }
    }

    /// Seats and measures one placed instance like the scatter field's `build_instance`.
    fn hit_of(
        &self,
        ci: usize,
        class: &ScatterClass,
        p: &PlacedInstance,
        cx: i32,
        cz: i32,
        ordinal: usize,
    ) -> Option<ScatterHit> {
        let shape = self.shapes.get(ci)?.get(usize::from(p.mesh))?;
        let up = normalized(p.up);
        let normal = normalized(p.normal);
        let cos = dot(up, normal).clamp(0.2, 1.0);
        let residual_tan = (1.0 - cos * cos).sqrt() / cos;
        let height = (shape.top - shape.bottom).max(1e-3) * p.scale;
        let sink = (class.sink * height + shape.footprint * p.scale * residual_tan).min(0.6 * height);
        let pivot = sub(p.position, scale3(up, sink));
        let obstacle = WorldObstacle::place(shape, pivot, up, p.scale);
        Some(ScatterHit {
            class: ci as u16,
            mesh: p.mesh,
            id: instance_id(self.name_hashes[ci], cx, cz, ordinal),
            cell: [cx, cz],
            position: p.position,
            height: (obstacle.top_y - p.position[1]).max(0.0),
            scale: p.scale,
            obstacle,
        })
    }

    /// Every instance whose ground contact lies in the XZ extent of the box `min..max` and whose
    /// vertical span (base to top) overlaps the box's, in visit order.
    #[must_use]
    pub fn in_box(&self, min: [f32; 3], max: [f32; 3], classes: Option<&[u16]>) -> Vec<ScatterHit> {
        let _span = tracing::info_span!(target: "regia_terrain", "forest_query_box").entered();
        let (lo, hi) = ordered(min, max);
        let mut out = Vec::new();
        self.visit_rect([lo[0], lo[2]], [hi[0], hi[2]], 0.0, classes, |h| {
            let (x, y, z) = (h.position[0], h.position[1], h.position[2]);
            if x >= lo[0] && x <= hi[0] && z >= lo[2] && z <= hi[2] && y <= hi[1] && h.obstacle.top_y >= lo[1]
            {
                out.push(*h);
            }
        });
        out
    }

    /// Every instance whose surface comes within `radius` metres of the segment `a..b`, with its
    /// clearance, nearest part and the segment parameter of the nearest approach, in visit
    /// order.
    #[must_use]
    pub fn along_segment(
        &self,
        a: [f32; 3],
        b: [f32; 3],
        radius: f32,
        classes: Option<&[u16]>,
    ) -> Vec<SegmentHit> {
        let _span = tracing::info_span!(target: "regia_terrain", "forest_query_segment").entered();
        let min = [a[0].min(b[0]), a[2].min(b[2])];
        let max = [a[0].max(b[0]), a[2].max(b[2])];
        let mut out = Vec::new();
        let low = a[1].min(b[1]) - radius;
        let high = a[1].max(b[1]) + radius;
        self.visit_rect(min, max, radius, classes, |h| {
            if h.obstacle.top_y < low || h.obstacle.trunk_start[1] - h.obstacle.horizontal_reach() > high {
                return;
            }
            let near = segment_clearance(&h.obstacle, a, b);
            if near.0 <= radius {
                out.push(SegmentHit {
                    hit: *h,
                    distance: near.0,
                    part: near.1,
                    t: near.2,
                });
            }
        });
        out
    }
}

fn normalized(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len > 1e-12 {
        scale3(v, 1.0 / len)
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn ordered(a: [f32; 3], b: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    (
        [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])],
        [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])],
    )
}

/// Clearance from segment `a..b` to `o`'s surface, the part, and the segment parameter of the
/// nearest approach.
#[must_use]
pub fn segment_clearance(o: &WorldObstacle, a: [f32; 3], b: [f32; 3]) -> (f32, ObstaclePart, f32) {
    let (trunk, t_trunk) = segment_segment(a, b, o.trunk_start, o.trunk_end);
    let trunk = trunk - o.trunk_radius;
    let (crown, t_crown) = segment_spheroid(o, a, b);
    if crown < trunk {
        (crown, ObstaclePart::Crown, t_crown)
    } else {
        (trunk, ObstaclePart::Trunk, t_trunk)
    }
}

/// Lower bound of the distance from segment `a..b` to the crown spheroid of `o` (negative when
/// the segment enters it): the segment mapped into the spheroid's unit-sphere space stays a
/// segment, and its nearest approach to the origin bounds every point's distance.
fn segment_spheroid(o: &WorldObstacle, a: [f32; 3], b: [f32; 3]) -> (f32, f32) {
    if o.crown_radius <= 0.0 || o.crown_half_height <= 0.0 {
        return (f32::INFINITY, 0.0);
    }
    let (e1, e2) = perpendiculars(o.up);
    let to_q = |p: [f32; 3]| {
        let d = sub(p, o.crown_centre);
        [
            dot(d, e1) / o.crown_radius,
            dot(d, o.up) / o.crown_half_height,
            dot(d, e2) / o.crown_radius,
        ]
    };
    let (k, t) = point_segment([0.0; 3], to_q(a), to_q(b));
    ((k - 1.0) * o.crown_radius.min(o.crown_half_height), t)
}

/// Two unit vectors perpendicular to unit `n` and to each other.
fn perpendiculars(n: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let helper = if n[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let e1 = normalized(cross(n, helper));
    (e1, cross(n, e1))
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Distance between segments `p1..q1` and `p2..q2` and the parameter on the first (closest points
/// of two segments: Ericson, *Real-Time Collision Detection*, 2005, § 5.1.9).
fn segment_segment(p1: [f32; 3], q1: [f32; 3], p2: [f32; 3], q2: [f32; 3]) -> (f32, f32) {
    const EPS: f32 = 1e-9;
    let d1 = sub(q1, p1);
    let d2 = sub(q2, p2);
    let r = sub(p1, p2);
    let a = dot(d1, d1);
    let e = dot(d2, d2);
    let f = dot(d2, r);
    let (s, t) = if a <= EPS && e <= EPS {
        (0.0, 0.0)
    } else if a <= EPS {
        (0.0, (f / e).clamp(0.0, 1.0))
    } else {
        let c = dot(d1, r);
        if e <= EPS {
            ((-c / a).clamp(0.0, 1.0), 0.0)
        } else {
            let b = dot(d1, d2);
            let denom = a * e - b * b;
            let mut s = if denom > EPS {
                ((b * f - c * e) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mut t = (b * s + f) / e;
            if t < 0.0 {
                t = 0.0;
                s = (-c / a).clamp(0.0, 1.0);
            } else if t > 1.0 {
                t = 1.0;
                s = ((b - c) / a).clamp(0.0, 1.0);
            }
            (s, t)
        }
    };
    let c1 = [p1[0] + d1[0] * s, p1[1] + d1[1] * s, p1[2] + d1[2] * s];
    let c2 = [p2[0] + d2[0] * t, p2[1] + d2[1] * t, p2[2] + d2[2] * t];
    let d = sub(c1, c2);
    (dot(d, d).sqrt(), s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SC2 (ADR-8496): a query given the field's world anchor (Q-08) answers with the
    /// world-anchored placement, cells with negative indices included; the default query is the
    /// map-edge placement, unchanged.
    #[test]
    fn a_world_anchored_query_places_as_the_world_anchored_set() {
        use crate::sampler::TerrainMap;
        use crate::scatter::placement::{WorldAnchor, place_set, place_set_with_origin};
        use crate::scatter::rock::{RockStyle, procedural_rock};
        use crate::scatter::set::ScatterMeshRef;
        use std::sync::Arc;
        let n = 129u32;
        let heights = (0..n * n)
            .map(|i| ((i % n) * 3 + (i / n) * 7 % 23) as f32 * 0.25)
            .collect();
        let map = Arc::new(TerrainMap::from_heights(n, n, heights, 256.0, 1.0, 0.0).expect("map"));
        let set = ScatterSet {
            seed: 0x5c2,
            classes: vec![ScatterClass {
                name: "boulders".into(),
                meshes: vec![ScatterMeshRef {
                    mesh: "rock_a".into(),
                    weight: 1.0,
                    min_scale: 0.5,
                    max_scale: 1.5,
                    scale_bias: 1.0,
                }],
                cell_size: 4.0,
                density: 1.0,
                ..ScatterClass::default()
            }],
            ..ScatterSet::default()
        };
        let mut library = ScatterLibrary::new();
        library.insert(
            "rock_a".into(),
            procedural_rock(3, RockStyle::angular([0.5, 0.4, 0.35])),
        );
        let source = ScatterSource::new(Arc::clone(&map));
        // On a sample (2 m spacing), inside the map: cells west and south of it are negative.
        let origin = PlacementOrigin::World(WorldAnchor::at([6.0, -10.0]));
        fn edge_inputs(source: &ScatterSource) -> crate::scatter::placement::PlacementInputs<'_> {
            crate::scatter::placement::PlacementInputs {
                map: &source.map,
                holes: None,
                layers: None,
                density_maps: None,
            }
        }
        let bits = |p: [f32; 3]| p.map(f32::to_bits);
        let sorted = |mut v: Vec<[u32; 3]>| {
            v.sort_unstable();
            v
        };
        let everything = |q: &ScatterQuery| {
            sorted(
                q.in_box([-200.0, -1e4, -200.0], [200.0, 1e4, 200.0], None)
                    .iter()
                    .map(|h| bits(h.position))
                    .collect(),
            )
        };
        let whole = |origin: &PlacementOrigin| {
            sorted(
                place_set_with_origin(&set, edge_inputs(&source), origin)
                    .expect("place")
                    .iter()
                    .map(|p| bits(p.position))
                    .collect(),
            )
        };
        let edge = ScatterQuery::new(&set, source.clone(), &library).expect("query");
        assert_eq!(edge.placement_origin(), PlacementOrigin::MapEdge);
        let edge_placed = sorted(
            place_set(&set, edge_inputs(&source))
                .expect("place")
                .iter()
                .map(|p| bits(p.position))
                .collect(),
        );
        assert_eq!(
            everything(&edge),
            edge_placed,
            "the default query is the map-edge placement"
        );
        let anchored = edge.clone().with_placement_origin(origin);
        assert_eq!(anchored.placement_origin(), origin);
        let world = whole(&origin);
        assert!(world.len() > 1000, "{} instances", world.len());
        assert_ne!(world, edge_placed, "the anchor moves the rocks");
        assert_eq!(
            everything(&anchored),
            world,
            "the query places as the anchored set"
        );
        // The south-west corner: negative cells, and their hits are the anchored set's there.
        let (lo, hi) = anchored
            .cell_range(0, [-128.0, -128.0], [-100.0, -100.0], 0.0)
            .expect("cells");
        assert!(lo[0] < 0 && lo[1] < 0 && hi[0] < 0 && hi[1] < 0, "{lo:?} {hi:?}");
        let mut hits = Vec::new();
        for cz in lo[1]..=hi[1] {
            for cx in lo[0]..=hi[0] {
                anchored.cell_hits(0, cx, cz, &mut hits);
            }
        }
        assert!(!hits.is_empty());
        for h in &hits {
            assert!(world.binary_search(&bits(h.position)).is_ok(), "{h:?}");
            let cell = [
                ((h.position[0] - 6.0) / 4.0).floor() as i32,
                ((h.position[2] + 10.0) / 4.0).floor() as i32,
            ];
            assert_eq!(h.cell, cell);
        }
    }

    #[test]
    fn segment_distances_match_closed_forms() {
        // Parallel offset segments.
        let (d, _) = segment_segment(
            [0.0, 0.0, 0.0],
            [10.0, 0.0, 0.0],
            [0.0, 3.0, 0.0],
            [10.0, 3.0, 0.0],
        );
        assert!((d - 3.0).abs() < 1e-5);
        // Crossing at right angles, 2 m apart vertically.
        let (d, s) = segment_segment(
            [-5.0, 2.0, 0.0],
            [5.0, 2.0, 0.0],
            [0.0, 0.0, -5.0],
            [0.0, 0.0, 5.0],
        );
        assert!((d - 2.0).abs() < 1e-5 && (s - 0.5).abs() < 1e-5);
        // Disjoint ends: nearest points are the end points.
        let (d, s) = segment_segment([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [4.0, 0.0, 0.0], [4.0, 5.0, 0.0]);
        assert!((d - 3.0).abs() < 1e-5 && (s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_segment_through_a_crown_is_negative() {
        let o = WorldObstacle {
            trunk_start: [0.0, 0.0, 0.0],
            trunk_end: [0.0, 6.0, 0.0],
            trunk_radius: 0.3,
            crown_centre: [0.0, 9.0, 0.0],
            up: [0.0, 1.0, 0.0],
            crown_radius: 3.0,
            crown_half_height: 3.0,
            top_y: 12.0,
        };
        let (d, part, t) = segment_clearance(&o, [-10.0, 9.0, 0.0], [10.0, 9.0, 0.0]);
        assert!(d < 0.0 && part == ObstaclePart::Crown && (t - 0.5).abs() < 1e-4);
        // Passing under the crown, beside the trunk: the trunk is nearest.
        let (d, part, _) = segment_clearance(&o, [-10.0, 2.0, 1.3], [10.0, 2.0, 1.3]);
        assert!(
            (d - 1.0).abs() < 1e-4 && part == ObstaclePart::Trunk,
            "{d} {part:?}"
        );
        // Well above: the crown top is 3 m below.
        let (d, part, _) = segment_clearance(&o, [-10.0, 15.0, 0.0], [10.0, 15.0, 0.0]);
        assert!((d - 3.0).abs() < 1e-4 && part == ObstaclePart::Crown);
    }
}
