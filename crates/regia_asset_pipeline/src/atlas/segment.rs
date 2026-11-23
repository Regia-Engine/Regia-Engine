// SPDX-License-Identifier: MIT
// Portions ported from xatlas (https://github.com/jpcy/xatlas, commit f700c77), source/xatlas/xatlas.cpp
// (segment::PlanarCharts and segment::ClusteredCharts: planar regions, seed placement, chart growth,
// seed relocation and the normal deviation / roundness / straightness / normal seam / texture seam
// cost metrics). Copyright (c) 2018-2020 Jonathan Young. xatlas is derived from thekla_atlas
// (https://github.com/Thekla/thekla_atlas), Copyright (c) 2013 Thekla, Inc and Copyright NVIDIA
// Corporation 2006 -- Ignacio Castano. MIT licence (see docs/ported-sources.toml):
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Chart segmentation (stage 1 of `generate`).
//!
//! Differences from the upstream code, all for speed on large meshes with the same metrics: a
//! chart's normal is its area-weighted average face normal (kept incrementally) instead of a
//! least-squares plane fit over all its vertices; candidate costs are refreshed lazily (a stale
//! candidate is re-scored when it reaches the top of its chart's queue) instead of re-scoring every
//! neighbour after every addition; growth does not re-project the chart after each face (the
//! parametrization stage validates and, when needed, splits the chart instead); and the optional
//! chart-merge pass is not ported.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use regia_core::glam::Vec3;

use super::AtlasOptions;
use super::mesh::{NONE, Prepared};

/// Normals whose dot product is within this of 1 are coplanar (xatlas `kEpsilon`).
const COPLANAR_EPSILON: f32 = 0.0001;
/// A face this far (1 − cos) from its chart's normal never joins (about 73°).
const MAX_NORMAL_DEVIATION: f32 = 0.707;
/// Faces considered when a seed moves to the most central well-fitting face.
const RELOCATE_CANDIDATES: usize = 10;

/// One chart out of segmentation.
pub(super) struct SegChart {
    /// Working face indices.
    pub faces: Vec<u32>,
    /// A coplanar region kept whole (no parametrization search needed).
    pub planar: bool,
}

/// Connected coplanar faces. `next` links a region's faces in a ring.
struct Regions {
    id: Vec<u32>,
    next: Vec<u32>,
    area: Vec<f32>,
    first: Vec<u32>,
}

impl Regions {
    fn new(m: &Prepared) -> Self {
        let n = m.face_count();
        let mut id = vec![NONE; n];
        let mut next: Vec<u32> = (0..n as u32).collect();
        let mut first = Vec::new();
        let mut stack = Vec::new();
        for f in 0..n as u32 {
            if id[f as usize] != NONE {
                continue;
            }
            let region = first.len() as u32;
            id[f as usize] = region;
            stack.clear();
            stack.push(f);
            while let Some(face) = stack.pop() {
                for i in 0..3 {
                    let o = m.opposite[face as usize * 3 + i];
                    if o == NONE {
                        continue;
                    }
                    let of = (o / 3) as usize;
                    if id[of] != NONE {
                        continue;
                    }
                    let d = m.face_normals[face as usize].dot(m.face_normals[of]);
                    if (d - 1.0).abs() > COPLANAR_EPSILON {
                        continue;
                    }
                    id[of] = region;
                    // Splice `of` into the ring after `face`.
                    next[of] = next[face as usize];
                    next[face as usize] = of as u32;
                    stack.push(of as u32);
                }
            }
            first.push(f);
        }
        let mut area = vec![0.0f32; first.len()];
        for (&region, &face_area) in id.iter().zip(&m.face_areas) {
            area[region as usize] += face_area;
        }
        Self {
            id,
            next,
            area,
            first,
        }
    }

    /// The faces of `face`'s region, starting at `face`.
    fn faces(&self, face: u32) -> RegionIter<'_> {
        RegionIter {
            regions: self,
            start: face,
            current: Some(face),
        }
    }
}

struct RegionIter<'a> {
    regions: &'a Regions,
    start: u32,
    current: Option<u32>,
}

impl Iterator for RegionIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let face = self.current?;
        let next = self.regions.next[face as usize];
        self.current = (next != self.start).then_some(next);
        Some(face)
    }
}

/// A queued face with the chart version its cost was computed against. The heap pops the lowest
/// cost first, then the lowest face index (deterministic ties).
#[derive(Clone, Copy, Debug)]
struct Candidate {
    cost: f32,
    face: u32,
    version: u32,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .total_cmp(&self.cost)
            .then_with(|| other.face.cmp(&self.face))
            .then_with(|| other.version.cmp(&self.version))
    }
}

/// A chart's best candidate in the global growth queue: lowest cost first, then lowest chart index.
#[derive(Clone, Copy, Debug)]
struct ChartTop {
    cost: f32,
    chart: u32,
    face: u32,
    version: u32,
}

impl PartialEq for ChartTop {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ChartTop {}

impl PartialOrd for ChartTop {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ChartTop {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .total_cmp(&self.cost)
            .then_with(|| other.chart.cmp(&self.chart))
            .then_with(|| other.face.cmp(&self.face))
            .then_with(|| other.version.cmp(&self.version))
    }
}

struct Chart {
    seed: u32,
    faces: Vec<u32>,
    normal_sum: Vec3,
    normal: Vec3,
    area: f32,
    boundary_length: f32,
    centroid_sum: Vec3,
    candidates: BinaryHeap<Candidate>,
    version: u32,
}

impl Chart {
    fn new(seed: u32) -> Self {
        Self {
            seed,
            faces: Vec::new(),
            normal_sum: Vec3::ZERO,
            normal: Vec3::ZERO,
            area: 0.0,
            boundary_length: 0.0,
            centroid_sum: Vec3::ZERO,
            candidates: BinaryHeap::new(),
            version: 0,
        }
    }

    fn clear(&mut self) {
        self.faces.clear();
        self.normal_sum = Vec3::ZERO;
        self.normal = Vec3::ZERO;
        self.area = 0.0;
        self.boundary_length = 0.0;
        self.centroid_sum = Vec3::ZERO;
        self.candidates.clear();
        self.version = self.version.wrapping_add(1);
    }
}

struct Clusterer<'a> {
    m: &'a Prepared,
    o: &'a AtlasOptions,
    regions: Regions,
    /// Taken by a planar chart or a clustered chart.
    taken: Vec<bool>,
    /// Clustered chart of each face, or [`NONE`].
    face_chart: Vec<u32>,
    charts: Vec<Chart>,
    faces_left: usize,
    /// Faces by descending region area, then ascending index: the seed order.
    seed_order: Vec<u32>,
    seed_cursor: usize,
}

impl<'a> Clusterer<'a> {
    fn new(m: &'a Prepared, o: &'a AtlasOptions, regions: Regions, taken: Vec<bool>) -> Self {
        let mut seed_order: Vec<u32> = (0..m.face_count() as u32)
            .filter(|&f| !taken[f as usize])
            .collect();
        seed_order.sort_by(|&a, &b| {
            let ra = regions.area[regions.id[a as usize] as usize];
            let rb = regions.area[regions.id[b as usize] as usize];
            rb.total_cmp(&ra).then(a.cmp(&b))
        });
        let faces_left = taken.iter().filter(|t| !**t).count();
        Self {
            m,
            o,
            regions,
            face_chart: vec![NONE; m.face_count()],
            taken,
            charts: Vec::new(),
            faces_left,
            seed_order,
            seed_cursor: 0,
        }
    }

    fn run(&mut self) {
        if self.faces_left == 0 {
            return;
        }
        let max_cost = self.o.max_cost;
        while self.faces_left > 0 {
            self.create_chart(max_cost * 0.5);
        }
        if self.o.max_iterations == 0 {
            return;
        }
        self.relocate_seeds();
        self.reset_charts();
        let mut iteration = 0;
        loop {
            self.grow_charts(max_cost);
            while self.faces_left > 0 {
                self.create_chart(max_cost * 0.5);
            }
            iteration += 1;
            if iteration >= self.o.max_iterations || !self.relocate_seeds() {
                break;
            }
            self.reset_charts();
        }
    }

    fn next_seed(&mut self) -> Option<u32> {
        while self.seed_cursor < self.seed_order.len() {
            let f = self.seed_order[self.seed_cursor];
            if !self.taken[f as usize] {
                return Some(f);
            }
            self.seed_cursor += 1;
        }
        None
    }

    fn create_chart(&mut self, threshold: f32) {
        let Some(seed) = self.next_seed() else {
            // Unreachable while faces are left; stop rather than spin.
            self.faces_left = 0;
            return;
        };
        let c = self.charts.len();
        self.charts.push(Chart::new(seed));
        self.add_face(c, seed);
        while let Some((cost, face)) = self.peek(c) {
            if cost > threshold {
                break;
            }
            self.charts[c].candidates.pop();
            self.add_face(c, face);
        }
    }

    /// The best fresh candidate of chart `c` (claimed faces dropped, stale costs re-scored).
    fn peek(&mut self, c: usize) -> Option<(f32, u32)> {
        loop {
            let top = *self.charts[c].candidates.peek()?;
            if self.taken[top.face as usize] {
                self.charts[c].candidates.pop();
                continue;
            }
            if top.version != self.charts[c].version {
                self.charts[c].candidates.pop();
                let cost = self.cost(c, top.face);
                if cost.is_finite() {
                    let version = self.charts[c].version;
                    self.charts[c].candidates.push(Candidate {
                        cost,
                        face: top.face,
                        version,
                    });
                }
                continue;
            }
            return Some((top.cost, top.face));
        }
    }

    /// Grows all charts together, globally cheapest candidate first. `tops` holds at most one entry
    /// per chart (its fresh best candidate when pushed); an entry whose chart has changed since, or
    /// whose face was taken, is refreshed when it surfaces.
    fn grow_charts(&mut self, threshold: f32) {
        let mut tops = BinaryHeap::with_capacity(self.charts.len());
        for c in 0..self.charts.len() {
            self.push_top(&mut tops, c);
        }
        while self.faces_left > 0 {
            let Some(top) = tops.pop() else {
                break;
            };
            let c = top.chart as usize;
            if top.version != self.charts[c].version || self.taken[top.face as usize] {
                self.push_top(&mut tops, c);
                continue;
            }
            if top.cost > threshold {
                break;
            }
            // Still the chart's fresh best: it is on top of the chart's own queue.
            self.charts[c].candidates.pop();
            self.add_face(c, top.face);
            self.push_top(&mut tops, c);
        }
    }

    fn push_top(&mut self, tops: &mut BinaryHeap<ChartTop>, c: usize) {
        if let Some((cost, face)) = self.peek(c) {
            tops.push(ChartTop {
                cost,
                chart: c as u32,
                face,
                version: self.charts[c].version,
            });
        }
    }

    fn reset_charts(&mut self) {
        for (chart, taken) in self.face_chart.iter_mut().zip(self.taken.iter_mut()) {
            if *chart != NONE {
                *taken = false;
                *chart = NONE;
            }
        }
        self.faces_left = self.taken.iter().filter(|t| !**t).count();
        self.seed_cursor = 0;
        for c in 0..self.charts.len() {
            self.charts[c].clear();
            let seed = self.charts[c].seed;
            self.add_face(c, seed);
        }
    }

    fn relocate_seeds(&mut self) -> bool {
        let seeds: Vec<u32> = self
            .charts
            .iter()
            .map(|chart| {
                let mut best: Vec<(f32, u32)> = chart
                    .faces
                    .iter()
                    .map(|&f| (self.normal_deviation(chart.normal, f), f))
                    .collect();
                best.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                best.truncate(RELOCATE_CANDIDATES);
                let centroid = chart.centroid_sum / chart.faces.len().max(1) as f32;
                let mut most_central = chart.seed;
                let mut min_distance = f32::MAX;
                for &(_, f) in &best {
                    let d = (centroid - self.m.face_center(f)).length();
                    if d < min_distance {
                        min_distance = d;
                        most_central = f;
                    }
                }
                most_central
            })
            .collect();
        let mut changed = false;
        for (chart, seed) in self.charts.iter_mut().zip(seeds) {
            if chart.seed != seed {
                chart.seed = seed;
                changed = true;
            }
        }
        changed
    }

    /// Adds `face` and its coplanar region to chart `c` and queues the new neighbours.
    fn add_face(&mut self, c: usize, face: u32) {
        let new_area = self.region_area_with(c, face);
        let new_boundary = self.boundary_length_with(c, face);
        let first = self.charts[c].faces.len();
        let region: Vec<u32> = self.regions.faces(face).collect();
        {
            let chart = &mut self.charts[c];
            for &f in &region {
                chart.faces.push(f);
                chart.normal_sum += self.m.face_normals[f as usize] * self.m.face_areas[f as usize];
                chart.centroid_sum += self.m.face_center(f);
            }
            chart.normal = if first == 0 {
                self.m.face_normals[face as usize]
            } else {
                chart.normal_sum.try_normalize().unwrap_or(chart.normal)
            };
            chart.area = new_area;
            chart.boundary_length = new_boundary;
            chart.version = chart.version.wrapping_add(1);
        }
        for &f in &region {
            self.face_chart[f as usize] = c as u32;
            self.taken[f as usize] = true;
            self.faces_left -= 1;
        }
        for &f in &region {
            for i in 0..3 {
                let o = self.m.opposite[f as usize * 3 + i];
                if o == NONE {
                    continue;
                }
                let of = o / 3;
                if self.taken[of as usize] {
                    continue;
                }
                let cost = self.cost(c, of);
                if cost.is_finite() {
                    let version = self.charts[c].version;
                    self.charts[c].candidates.push(Candidate {
                        cost,
                        face: of,
                        version,
                    });
                }
            }
        }
    }

    fn region_area_with(&self, c: usize, face: u32) -> f32 {
        self.charts[c].area + self.regions.area[self.regions.id[face as usize] as usize]
    }

    /// Chart `c`'s boundary length after adding `face`'s region.
    fn boundary_length_with(&self, c: usize, face: u32) -> f32 {
        let region = self.regions.id[face as usize];
        let mut length = self.charts[c].boundary_length;
        for f in self.regions.faces(face) {
            for i in 0..3 {
                let e = f as usize * 3 + i;
                let l = self.m.edge_lengths[e];
                let o = self.m.opposite[e];
                if o == NONE {
                    length += l;
                } else if self.regions.id[(o / 3) as usize] != region {
                    if self.face_chart[(o / 3) as usize] == c as u32 {
                        length -= l;
                    } else {
                        length += l;
                    }
                }
            }
        }
        length.max(0.0)
    }

    fn normal_deviation(&self, chart_normal: Vec3, face: u32) -> f32 {
        (1.0 - self.m.face_normals[face as usize].dot(chart_normal)).min(1.0)
    }

    /// Weighted cost of adding `face`'s region to chart `c` (`f32::INFINITY` = never).
    fn cost(&self, c: usize, face: u32) -> f32 {
        let o = self.o;
        let chart = &self.charts[c];
        let new_area = self.region_area_with(c, face);
        let new_boundary = self.boundary_length_with(c, face);
        if o.max_chart_area > 0.0 && new_area > o.max_chart_area {
            return f32::INFINITY;
        }
        if o.max_boundary_length > 0.0 && new_boundary > o.max_boundary_length {
            return f32::INFINITY;
        }
        let deviation = self.normal_deviation(chart.normal, face);
        if deviation >= MAX_NORMAL_DEVIATION {
            return f32::INFINITY;
        }
        let normal_seam = self.normal_seam(c, face);
        if o.normal_seam_weight >= 1000.0 && normal_seam > 0.0 {
            return f32::INFINITY;
        }
        let mut cost = o.normal_deviation_weight * deviation + o.normal_seam_weight * normal_seam;
        cost += o.roundness_weight * roundness(chart, new_boundary, new_area);
        cost += o.straightness_weight * self.straightness(c, face);
        if o.texture_seam_weight > 0.0 {
            cost += o.texture_seam_weight * self.texture_seam(c, face);
        }
        if cost.is_finite() { cost } else { f32::INFINITY }
    }

    /// In `[-1, 0]`: negative when the region's edges mostly border the chart (closing a gap).
    fn straightness(&self, c: usize, face: u32) -> f32 {
        let region = self.regions.id[face as usize];
        let (mut l_out, mut l_in) = (0.0f32, 0.0f32);
        for f in self.regions.faces(face) {
            for i in 0..3 {
                let e = f as usize * 3 + i;
                let l = self.m.edge_lengths[e];
                let o = self.m.opposite[e];
                if o == NONE {
                    l_out += l;
                } else if self.regions.id[(o / 3) as usize] != region {
                    if self.face_chart[(o / 3) as usize] == c as u32 {
                        l_in += l;
                    } else {
                        l_out += l;
                    }
                }
            }
        }
        let total = l_out + l_in;
        if total <= 0.0 {
            return 0.0;
        }
        ((l_out - l_in) / total).min(0.0)
    }

    /// Share of the region's chart-facing edge length that crosses a normal seam, weighted by how
    /// much the normals differ.
    fn normal_seam(&self, c: usize, face: u32) -> f32 {
        let region = self.regions.id[face as usize];
        let (mut seam, mut total) = (0.0f32, 0.0f32);
        for f in self.regions.faces(face) {
            for i in 0..3 {
                let e = f * 3 + i as u32;
                let o = self.m.opposite[e as usize];
                if o == NONE || self.face_chart[(o / 3) as usize] != c as u32 {
                    continue;
                }
                let mut l = self.m.edge_lengths[e as usize];
                total += l;
                if !self.m.is_seam(e) {
                    continue;
                }
                let same_region = self.regions.id[(o / 3) as usize] == region;
                if self.m.is_normal_seam(e, same_region) {
                    let d = if let Some(normals) = &self.m.normals {
                        let d0 = normals[self.m.edge_v0(e) as usize]
                            .dot(normals[self.m.edge_v1(o) as usize])
                            .clamp(0.0, 1.0);
                        let d1 = normals[self.m.edge_v1(e) as usize]
                            .dot(normals[self.m.edge_v0(o) as usize])
                            .clamp(0.0, 1.0);
                        (d0 + d1) * 0.5
                    } else {
                        self.m.face_normals[f as usize]
                            .dot(self.m.face_normals[(o / 3) as usize])
                            .clamp(0.0, 1.0)
                    };
                    l *= 1.0 - d;
                    seam += l;
                }
            }
        }
        if seam <= 0.0 || total <= 0.0 {
            return 0.0;
        }
        seam / total
    }

    /// Share of the region's chart-facing edge length that crosses a texture seam.
    fn texture_seam(&self, c: usize, face: u32) -> f32 {
        let (mut seam, mut total) = (0.0f32, 0.0f32);
        for f in self.regions.faces(face) {
            for i in 0..3 {
                let e = f * 3 + i as u32;
                let o = self.m.opposite[e as usize];
                if o == NONE || self.face_chart[(o / 3) as usize] != c as u32 {
                    continue;
                }
                let l = self.m.edge_lengths[e as usize];
                total += l;
                if self.m.is_texture_seam(e) {
                    seam += l;
                }
            }
        }
        if seam <= 0.0 || total <= 0.0 {
            return 0.0;
        }
        seam / total
    }
}

/// `1 - old / new` roundness (boundary length² / area); positive when the chart gets less round.
fn roundness(chart: &Chart, new_boundary: f32, new_area: f32) -> f32 {
    if chart.area <= 0.0 || new_area <= 0.0 {
        return 0.0;
    }
    let old = chart.boundary_length * chart.boundary_length / chart.area;
    let new = new_boundary * new_boundary / new_area;
    if new <= 0.0 {
        return 0.0;
    }
    1.0 - old / new
}

/// Splits the working mesh into charts: planar regions first (in region order), then the
/// clustered charts (in creation order).
pub(super) fn compute_charts(m: &Prepared, o: &AtlasOptions) -> Vec<SegChart> {
    let _span = tracing::debug_span!("atlas.segment", faces = m.face_count()).entered();
    let regions = Regions::new(m);
    let mut taken = vec![false; m.face_count()];
    let mut out = Vec::new();
    for (region, &first) in regions.first.iter().enumerate() {
        // A region becomes a chart as is when every edge to another region bends by >= 90°.
        let closed_off = regions.faces(first).all(|f| {
            (0..3).all(|i| {
                let o = m.opposite[f as usize * 3 + i];
                if o == NONE || regions.id[(o / 3) as usize] == region as u32 {
                    return true;
                }
                m.face_normals[f as usize].dot(m.face_normals[(o / 3) as usize]) <= 0.0
            })
        });
        if closed_off {
            let faces: Vec<u32> = regions.faces(first).collect();
            for &f in &faces {
                taken[f as usize] = true;
            }
            out.push(SegChart { faces, planar: true });
        }
    }
    let mut clusterer = Clusterer::new(m, o, regions, taken);
    clusterer.run();
    out.extend(clusterer.charts.into_iter().map(|c| SegChart {
        faces: c.faces,
        planar: false,
    }));
    out
}
