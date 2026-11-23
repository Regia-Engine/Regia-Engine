// SPDX-License-Identifier: MPL-2.0
//! The terrain's caster proxy for the ray-traced sun (ADR-2310).
//!
//! The traced sun's scene holds triangles, and the terrain draws a GPU-selected clipmap that
//! has none on the CPU. The proxy is a coarse grid over the whole map, cut into square tiles
//! (one bottom-level structure each, so an edit rebuilds only the tiles it touches), built from
//! the CPU height mirror:
//!
//! - **Spacing.** One proxy vertex every `stride` samples, `stride` from
//!   [`TraceProxySettings::spacing_m`], raised until the whole map fits in
//!   [`TraceProxySettings::max_triangles`].
//! - **Conservative low.** Each vertex takes the lowest sample within half a stride around it, so
//!   the proxy never rises above the drawn surface: the ground never shadows itself through the
//!   proxy (no acne on slopes the coarse grid cuts across), at the cost of casters a few
//!   decimetres lower, which shortens a ridge's shadow by that much over a long throw.
//! - **Limits.** Holes cut through the terrain still cast as ground, relief displacement and
//!   grass are not in the proxy, and far scatter impostors are not either (ADR-2310 "limits").
//!
//! Vertices are in the map's world-anchored space; the one instance per tile has the identity
//! transform (the renderer applies the floating-origin offset).

use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;
use regia_render::hwrt::{SharedTraceCasters, TraceCasterInstance, TraceCasterMesh};

use crate::deform::DirtyRect;
use crate::sampler::TerrainMap;

/// The proxy's resolution and budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceProxySettings {
    /// Wanted distance between proxy vertices, metres (rounded to whole samples, at least one).
    pub spacing_m: f32,
    /// Largest number of triangles over the whole map; the spacing grows to fit.
    pub max_triangles: u32,
    /// Quads along each edge of a tile (at least 1).
    pub tile_quads: u32,
}

impl Default for TraceProxySettings {
    fn default() -> Self {
        Self {
            spacing_m: 2.0,
            max_triangles: 2_000_000,
            tile_quads: 64,
        }
    }
}

impl TraceProxySettings {
    /// Largest [`Self::spacing_m`] accepted, metres.
    pub const MAX_SPACING_M: f32 = 1_000.0;
    /// Largest [`Self::max_triangles`] accepted.
    pub const MAX_TRIANGLES: u32 = 1 << 28;
    /// Largest [`Self::tile_quads`] accepted.
    pub const MAX_TILE_QUADS: u32 = 1_024;

    /// Why these settings are refused, if they are: a spacing that is not finite and in
    /// `(0, MAX_SPACING_M]`, a budget outside `2..=MAX_TRIANGLES`, tiles outside
    /// `1..=MAX_TILE_QUADS`.
    #[must_use]
    pub fn invalid(&self) -> Option<&'static str> {
        if !(self.spacing_m.is_finite() && self.spacing_m > 0.0 && self.spacing_m <= Self::MAX_SPACING_M) {
            return Some("spacing_m must be finite, above 0 and at most 1000");
        }
        if !(2..=Self::MAX_TRIANGLES).contains(&self.max_triangles) {
            return Some("max_triangles must be in 2..=268435456");
        }
        if !(1..=Self::MAX_TILE_QUADS).contains(&self.tile_quads) {
            return Some("tile_quads must be in 1..=1024");
        }
        None
    }
}

/// The proxy's settings and what it holds, shared between a terrain draw and whoever reads or
/// changes them while it runs (`terrain.trace_proxy`, `terrain.set_trace_proxy`).
#[derive(Debug, Clone, Default)]
pub struct TerrainTraceProxyHandle(Arc<Mutex<(TraceProxySettings, Option<TraceProxyStats>)>>);

impl TerrainTraceProxyHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, (TraceProxySettings, Option<TraceProxyStats>)> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The settings the next build uses.
    #[must_use]
    pub fn settings(&self) -> TraceProxySettings {
        self.lock().0
    }

    /// Sets the settings: a proxy already built is rebuilt with them the next frame that traces.
    pub fn set_settings(&self, settings: TraceProxySettings) {
        self.lock().0 = settings;
    }

    /// What the built proxy holds; `None` while the frame does not trace.
    #[must_use]
    pub fn stats(&self) -> Option<TraceProxyStats> {
        self.lock().1
    }

    /// Records what the draw's proxy holds (`None`: released).
    pub fn set_stats(&self, stats: Option<TraceProxyStats>) {
        self.lock().1 = stats;
    }
}

/// The ECS resource a host inserts with its terrain draw's handle
/// (`TerrainSharedDraw::trace_proxy_handle`), so agent `terrain.trace_proxy` /
/// `terrain.set_trace_proxy` reach the running draw.
#[derive(Resource, Debug, Clone, Default)]
pub struct TerrainTraceProxyControl(pub TerrainTraceProxyHandle);

/// What the proxy holds (`terrain.trace_proxy`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TraceProxyStats {
    /// Samples between proxy vertices.
    pub stride: u32,
    /// Distance between proxy vertices, metres.
    pub spacing_m: f32,
    /// Tiles (bottom-level meshes, one instance each).
    pub tiles: u32,
    /// Triangles over every tile.
    pub triangles: u64,
    /// Tile builds since the proxy was created (a full build counts every tile).
    pub tile_builds: u64,
}

/// The coarse caster grid ([`TraceProxySettings`]).
#[derive(Debug, Clone)]
pub struct TraceProxy {
    settings: TraceProxySettings,
    stride: u32,
    spacing_m: f32,
    /// Proxy quads along X and Z.
    quads: [u32; 2],
    /// Tiles along X and Z.
    tiles: [u32; 2],
    meshes: Vec<TraceCasterMesh>,
    instances: Vec<TraceCasterInstance>,
    /// Shared index lists by tile size in quads.
    index_lists: Vec<([u32; 2], Arc<[u32]>)>,
    dirty: Vec<bool>,
    tile_builds: u64,
}

/// Tile generations, unique in the process: a rebuilt proxy (new settings) never reuses one, so
/// the renderer's upload cache, keyed by tile and generation, never keeps a stale tile.
static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Samples between proxy vertices for `map` under `settings`.
#[must_use]
pub fn proxy_stride(map: &TerrainMap, settings: &TraceProxySettings) -> u32 {
    let spacing = map.sample_spacing().max(f32::MIN_POSITIVE);
    let wanted = (settings.spacing_m / spacing).round();
    let mut stride = if wanted.is_finite() && wanted >= 1.0 {
        wanted as u32
    } else {
        1
    };
    let triangles = |stride: u32| {
        let qx = u64::from((map.width() - 1).div_ceil(stride));
        let qz = u64::from((map.height() - 1).div_ceil(stride));
        qx * qz * 2
    };
    while triangles(stride) > u64::from(settings.max_triangles.max(2))
        && stride < map.width().max(map.height())
    {
        stride += 1;
    }
    stride
}

impl TraceProxy {
    /// A proxy of `map`, every tile built.
    #[must_use]
    pub fn build(map: &TerrainMap, settings: TraceProxySettings) -> Self {
        let _span = tracing::info_span!("terrain_trace_proxy_build").entered();
        let stride = proxy_stride(map, &settings);
        let quads = [
            (map.width() - 1).div_ceil(stride),
            (map.height() - 1).div_ceil(stride),
        ];
        let tile = settings.tile_quads.max(1);
        let tiles = [quads[0].div_ceil(tile), quads[1].div_ceil(tile)];
        let count = (tiles[0] * tiles[1]) as usize;
        let mut proxy = Self {
            settings,
            stride,
            spacing_m: stride as f32 * map.sample_spacing(),
            quads,
            tiles,
            meshes: Vec::with_capacity(count),
            instances: Vec::with_capacity(count),
            index_lists: Vec::new(),
            dirty: vec![false; count],
            tile_builds: 0,
        };
        for tz in 0..tiles[1] {
            for tx in 0..tiles[0] {
                let mesh = proxy.tile_mesh(map, tx, tz);
                proxy.instances.push(TraceCasterInstance {
                    mesh: mesh.key,
                    transform: IDENTITY,
                });
                proxy.meshes.push(mesh);
            }
        }
        proxy
    }

    /// The settings it was built with.
    #[must_use]
    pub fn settings(&self) -> TraceProxySettings {
        self.settings
    }

    /// Marks the tiles a height edit touched (sample rectangle); [`Self::refresh`] rebuilds them.
    pub fn mark_dirty(&mut self, rect: &DirtyRect) {
        if rect.is_empty() {
            return;
        }
        // A vertex reads samples half a stride around it: grow the rectangle by that, and by one
        // vertex, before mapping it to tiles.
        let reach = self.stride / 2 + self.stride;
        let to_tile = |sample: u32, axis: usize| {
            let vertex = sample / self.stride;
            (vertex / self.settings.tile_quads.max(1)).min(self.tiles[axis] - 1)
        };
        let c0 = to_tile(rect.col_start.saturating_sub(reach), 0);
        let c1 = to_tile(rect.col_end.saturating_add(reach), 0);
        let r0 = to_tile(rect.row_start.saturating_sub(reach), 1);
        let r1 = to_tile(rect.row_end.saturating_add(reach), 1);
        for tz in r0..=r1 {
            for tx in c0..=c1 {
                self.dirty[(tz * self.tiles[0] + tx) as usize] = true;
            }
        }
    }

    /// Whether some tile waits for [`Self::refresh`].
    #[must_use]
    pub fn has_dirty(&self) -> bool {
        self.dirty.iter().any(|d| *d)
    }

    /// Rebuilds the dirty tiles from `map` (each gets a new generation). Returns how many.
    pub fn refresh(&mut self, map: &TerrainMap) -> usize {
        if !self.has_dirty() {
            return 0;
        }
        let _span = tracing::info_span!("terrain_trace_proxy_refresh").entered();
        let dirty: Vec<usize> = self
            .dirty
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.then_some(i))
            .collect();
        self.dirty.fill(false);
        let mut rebuilt = 0;
        for index in dirty {
            let (tx, tz) = (index as u32 % self.tiles[0], index as u32 / self.tiles[0]);
            let key = self.meshes[index].key;
            let mesh = self.tile_mesh(map, tx, tz);
            self.meshes[index] = TraceCasterMesh { key, ..mesh };
            rebuilt += 1;
        }
        rebuilt
    }

    /// The casters for [`regia_render::SharedPassDraw::trace_casters`].
    #[must_use]
    pub fn casters(&self) -> SharedTraceCasters<'_> {
        SharedTraceCasters {
            meshes: &self.meshes,
            instances: &self.instances,
        }
    }

    /// What it holds.
    #[must_use]
    pub fn stats(&self) -> TraceProxyStats {
        TraceProxyStats {
            stride: self.stride,
            spacing_m: self.spacing_m,
            tiles: self.meshes.len() as u32,
            triangles: self.meshes.iter().map(|m| u64::from(m.triangle_count())).sum(),
            tile_builds: self.tile_builds,
        }
    }

    /// The index list of a tile `size` quads across, shared by every tile of that size.
    fn indices(&mut self, size: [u32; 2]) -> Arc<[u32]> {
        if let Some((_, list)) = self.index_lists.iter().find(|(s, _)| *s == size) {
            return Arc::clone(list);
        }
        let row = size[0] + 1;
        let mut list = Vec::with_capacity((size[0] * size[1] * 6) as usize);
        for z in 0..size[1] {
            for x in 0..size[0] {
                let a = z * row + x;
                let (b, c, d) = (a + 1, a + row, a + row + 1);
                // Counter-clockwise seen from above (+Y), like the drawn terrain.
                list.extend_from_slice(&[a, c, b, b, c, d]);
            }
        }
        let list: Arc<[u32]> = Arc::from(list);
        self.index_lists.push((size, Arc::clone(&list)));
        list
    }

    fn tile_mesh(&mut self, map: &TerrainMap, tx: u32, tz: u32) -> TraceCasterMesh {
        let tile = self.settings.tile_quads.max(1);
        let q0 = [tx * tile, tz * tile];
        let q1 = [
            ((tx + 1) * tile).min(self.quads[0]),
            ((tz + 1) * tile).min(self.quads[1]),
        ];
        let size = [q1[0] - q0[0], q1[1] - q0[1]];
        let mut positions = Vec::with_capacity(((size[0] + 1) * (size[1] + 1)) as usize);
        let half = i64::from(self.stride / 2);
        for vz in q0[1]..=q1[1] {
            let row = (vz * self.stride).min(map.height() - 1);
            for vx in q0[0]..=q1[0] {
                let col = (vx * self.stride).min(map.width() - 1);
                let mut low = f32::INFINITY;
                for r in i64::from(row) - half..=i64::from(row) + half {
                    for c in i64::from(col) - half..=i64::from(col) + half {
                        low = low.min(map.height_at_sample(c, r));
                    }
                }
                let [x, z] = map.sample_to_world(col, row);
                positions.push([x, low, z]);
            }
        }
        let generation = NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.tile_builds += 1;
        TraceCasterMesh {
            key: (u64::from(tz) << 32) | u64::from(tx),
            generation,
            positions: Arc::from(positions),
            indices: self.indices(size),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(samples: u32, world: f32) -> TerrainMap {
        let heights: Vec<f32> = (0..samples * samples)
            .map(|i| {
                let (c, r) = (i % samples, i / samples);
                (c as f32 * 0.37).sin() * 3.0 + r as f32 * 0.25
            })
            .collect();
        TerrainMap::from_heights(samples, samples, heights, world, 100.0, -50.0).expect("map")
    }

    fn settings(spacing_m: f32, tile_quads: u32) -> TraceProxySettings {
        TraceProxySettings {
            spacing_m,
            tile_quads,
            ..TraceProxySettings::default()
        }
    }

    #[test]
    fn the_grid_covers_the_map_in_tiles_that_share_index_lists() {
        // 65 samples over 128 m: 2 m spacing, stride 2 at 4 m, 32 quads per axis.
        let map = ramp(65, 128.0);
        let proxy = TraceProxy::build(&map, settings(4.0, 10));
        let stats = proxy.stats();
        assert_eq!(stats.stride, 2);
        assert_eq!(stats.spacing_m, 4.0);
        assert_eq!(stats.tiles, 16, "32 quads in tiles of 10: 4 x 4");
        assert_eq!(stats.triangles, 32 * 32 * 2);
        assert_eq!(stats.tile_builds, 16);
        let casters = proxy.casters();
        assert_eq!(casters.instances.len(), casters.meshes.len());
        for (mesh, instance) in casters.meshes.iter().zip(casters.instances) {
            assert!(mesh.is_valid());
            assert_eq!(instance.mesh, mesh.key);
            assert_eq!(instance.transform, IDENTITY);
        }
        // Full, right-edge, bottom-edge and corner tiles: four index lists for sixteen tiles.
        let lists: std::collections::HashSet<usize> = casters
            .meshes
            .iter()
            .map(|m| m.indices.as_ptr() as usize)
            .collect();
        assert_eq!(lists.len(), 4);
        // The proxy reaches the map's edges.
        let xs = casters
            .meshes
            .iter()
            .flat_map(|m| m.positions.iter().map(|p| p[0]));
        let (lo, hi) = xs.fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(x), hi.max(x)));
        assert_eq!((lo, hi), (-64.0, 64.0));
    }

    /// Every proxy vertex is at or under every sample within half a stride: the proxy never
    /// rises above the drawn surface where it cuts across the heightfield.
    #[test]
    fn the_proxy_stays_at_or_under_the_heightfield() {
        let map = ramp(65, 128.0);
        let proxy = TraceProxy::build(&map, settings(8.0, 16));
        assert_eq!(proxy.stats().stride, 4);
        for mesh in proxy.casters().meshes {
            for p in mesh.positions.iter() {
                let [gc, gr] = map.world_to_grid(p[0], p[2]);
                let (c, r) = (gc.round() as i64, gr.round() as i64);
                for dr in -2..=2 {
                    for dc in -2..=2 {
                        assert!(p[1] <= map.height_at_sample(c + dc, r + dr) + 1e-5);
                    }
                }
                assert!(p[1] <= map.sample_bilinear(p[0], p[2]) + 1e-5);
            }
        }
        // Stride 1 is the heightfield itself.
        let exact = TraceProxy::build(&map, settings(0.1, 16));
        assert_eq!(exact.stats().stride, 1);
        for mesh in exact.casters().meshes {
            for p in mesh.positions.iter() {
                assert!((p[1] - map.sample_bilinear(p[0], p[2])).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn the_budget_raises_the_stride() {
        let map = ramp(129, 256.0);
        let small = TraceProxySettings {
            spacing_m: 2.0,
            max_triangles: 2_000,
            tile_quads: 16,
        };
        let proxy = TraceProxy::build(&map, small);
        let stats = proxy.stats();
        assert!(stats.triangles <= 2_000, "{stats:?}");
        assert!(stats.stride > 1);
        assert_eq!(proxy_stride(&map, &small), stats.stride);
    }

    /// An edit rebuilds only the tiles it reaches, each with a new generation, keeping its key.
    #[test]
    fn an_edit_rebuilds_the_tiles_it_touches() {
        let mut map_heights = ramp(65, 128.0).heights().to_vec();
        let map = TerrainMap::from_heights(65, 65, map_heights.clone(), 128.0, 100.0, -50.0).expect("map");
        let mut proxy = TraceProxy::build(&map, settings(2.0, 16));
        let before: Vec<(u64, u64)> = proxy
            .casters()
            .meshes
            .iter()
            .map(|m| (m.key, m.generation))
            .collect();
        assert_eq!(before.len(), 16);
        // Dig a pit near sample (10, 10): tile (0, 0) only.
        for r in 9..12 {
            for c in 9..12 {
                map_heights[r * 65 + c] -= 5.0;
            }
        }
        let edited = TerrainMap::from_heights(65, 65, map_heights, 128.0, 100.0, -50.0).expect("map");
        proxy.mark_dirty(&DirtyRect {
            col_start: 9,
            col_end: 12,
            row_start: 9,
            row_end: 12,
        });
        assert!(proxy.has_dirty());
        assert_eq!(proxy.refresh(&edited), 1);
        assert!(!proxy.has_dirty());
        let after: Vec<(u64, u64)> = proxy
            .casters()
            .meshes
            .iter()
            .map(|m| (m.key, m.generation))
            .collect();
        for (i, (b, a)) in before.iter().zip(&after).enumerate() {
            assert_eq!(b.0, a.0, "keys stay");
            assert_eq!(b.1 != a.1, i == 0, "tile {i}");
        }
        let pit = proxy.casters().meshes[0].positions[10 * 17 + 10];
        assert!(pit[1] < map.height_at_sample(10, 10) - 4.0);
        // An edit on a tile border reaches both neighbours.
        proxy.mark_dirty(&DirtyRect {
            col_start: 32,
            col_end: 33,
            row_start: 5,
            row_end: 6,
        });
        assert_eq!(proxy.refresh(&edited), 2);
        assert_eq!(proxy.stats().tile_builds, 16 + 1 + 2);
    }
}
