// SPDX-License-Identifier: MPL-2.0
//! Bake sizing (ADR-1099): a synthetic stand-in for a large DO27-shaped map, and the sizing
//! model of a probe layout.
//!
//! [`SyntheticLevel`] generates a level with no DO27 content in it: a terrain grid mesh (the
//! shape DO27 hands the bake, ADR-280), 20 interiors as closed two-sided shells (one of them a
//! shaft open to the sky that reaches down through a hole in the terrain), towers, shipping
//! containers, instanced scatter rocks marked `approximate` and a backdrop ring of cliffs. It
//! writes a `.bake.ron` v1 description of that level ([`SyntheticLevel::bake_ron`]) and serves
//! its meshes from memory ([`SyntheticMeshLoader`]), so [`SyntheticLevel::job`] runs the same
//! `GiBakeDescription::build_with` path as `regia bake-gi` (only the glTF import is replaced).
//!
//! [`size_job`] measures a built job without tracing: probes per layer and brick kind, probes
//! under the terrain (stored but never traced), the `.fgiv` payload and an upper bound of the
//! file, the renderer's one storage buffer against the portable 128 MiB binding, the rays a bake
//! starts, the bake's GPU buffers and an estimate of its host memory peak. The `bake_sizing`
//! example prints these for presets and a sweep, and runs a timed GPU bake of the same job.
//!
//! The terrain is a sum of sines with flattened pads under the buildings; placements come from a
//! SplitMix64 stream (Steele, Lea and Flood 2014) mapped to integer grids, so every coordinate is
//! exact in `f32` and the layout is the same on every platform.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use regia_asset::baked_gi::{
    GI_COMPONENTS, META_RECORD_BYTES, MOMENT_TEXEL_BYTES, ProbeData, SH_RECORD_BYTES, VolumeLayout,
};
use regia_render::baked_gi::memory::{self, BakedGiMemoryReport};
use serde::Serialize;

use crate::PtError;
use crate::bake_desc::{BakeDescError, BrickSource, GiBakeDescription, GiBakeJob};
use crate::bake_meshes::{BakeMeshLoader, BakeMeshPart, BakeMeshSource};
use crate::probe::ProbeTrace;
use crate::scene::PtMesh;
use crate::scenes::{merge, quad, uv_sphere};

/// Names of the synthetic level presets ([`SyntheticLevel::preset`]).
pub const SYNTHETIC_LEVELS: [&str; 3] = ["bore12", "small", "tall"];

/// Names of the layout presets ([`SizingLayout::preset`]).
pub const SIZING_LAYOUTS: [&str; 3] = ["defaults", "doc-example", "recommended"];

/// The largest storage binding every supported device offers (wgpu's default limit and the
/// mobile tier, `regia_render::minspec`): a volume buffer above it does not install there.
pub const PORTABLE_BINDING_BYTES: u64 = 128 << 20;

/// Shell wall thickness, metres.
const WALL: f32 = 0.5;
/// Ground forced flat to a building's pad this far around its footprint, metres.
const PAD_FLAT: f32 = 8.0;
/// Beyond the flat band the ground blends back to the open terrain over this distance, metres.
const PAD_BLEND: f32 = 16.0;
/// Bytes of one brick in the renderer's volume buffer (`regia_render::baked_gi`'s 12-word
/// brick table entry).
const RUNTIME_BRICK_BYTES: u64 = 48;
/// vec4s per probe before the moment texels in the probe trace's accumulator (`pt.wgsl`).
const PROBE_HEADER_VEC4S: u64 = 15;
/// vec4s per path slot in the wavefront tracer's path buffer (`tracer.rs`).
const PATH_VEC4S: u64 = 12;
/// Bytes per path slot in the hit buffer (`tracer.rs`).
const HIT_BYTES: u64 = 32;
/// Upper bound of the postcard header bytes that do not depend on the bricks or the levels:
/// counts, the terrain layer's fixed fields, lights, bake info, encodings, sections, hash.
const HEADER_FIXED_MAX: u64 = 512;
/// Upper bound of one brick in the postcard header (6 `f32`, up to 5 varint `u32`).
const HEADER_BRICK_MAX: u64 = 6 * 4 + 5 * 5;

/// One synthetic enclosed volume: a closed two-sided shell, or a shaft open to the sky.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SyntheticInterior {
    /// Name of the interior and of its shell's mesh entry.
    pub id: String,
    /// Shell class: interiors of one class share a mesh source.
    pub class: String,
    /// Footprint centre, world X and Z, metres.
    pub at: [f32; 2],
    /// Inner size along X, Y and Z, metres.
    pub size: [f32; 3],
    /// How far the floor sits below the ground pad, metres (a shaft reaches down through the
    /// terrain).
    pub depth: f32,
    /// Open to the sky: the shell has no roof and the terrain has a hole over it.
    pub sky_open: bool,
    /// Has openings (a vehicle bay). Only the description flag: the synthetic shell stays
    /// closed.
    pub openings: bool,
}

impl SyntheticInterior {
    fn new(id: &str, class: &str, at: [f32; 2], size: [f32; 3]) -> Self {
        Self {
            id: id.to_string(),
            class: class.to_string(),
            at,
            size,
            depth: 0.0,
            sky_open: false,
            openings: false,
        }
    }

    /// Outer footprint (X and Z), shell walls included: `(min, max)`.
    #[must_use]
    pub fn footprint(&self) -> ([f32; 2], [f32; 2]) {
        let hx = 0.5 * self.size[0] + WALL;
        let hz = 0.5 * self.size[2] + WALL;
        (
            [self.at[0] - hx, self.at[1] - hz],
            [self.at[0] + hx, self.at[1] + hz],
        )
    }
}

/// A synthetic level: the size of a large DO27 map, made of stand-in geometry only.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SyntheticLevel {
    /// Preset name.
    pub name: String,
    /// Terrain grid vertices per side.
    pub grid: u32,
    /// Terrain grid spacing, metres (the bake rasterizes the grid at this spacing).
    pub grid_spacing: f32,
    /// Terrain relief amplitude, metres.
    pub relief: f32,
    /// The interiors.
    pub interiors: Vec<SyntheticInterior>,
    /// Closed towers on a ring (fewer when a slot is too close to an interior).
    pub towers: u32,
    /// Ring radius of the towers, metres.
    pub tower_ring: f32,
    /// Shipping containers beside the interiors.
    pub containers: u32,
    /// Scatter rocks: one instanced entry marked `approximate` (DO27's large-rock hints).
    pub rocks: u32,
    /// A ring of cliff segments near the map edge, as one mesh entry (a backdrop).
    pub backdrop: bool,
    /// Seed of the rock placements.
    pub seed: u64,
}

/// Where the generator put the outdoor placements (column-major 4×4 transforms).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyntheticPlacements {
    /// Towers.
    pub towers: Vec<[f32; 16]>,
    /// Containers.
    pub containers: Vec<[f32; 16]>,
    /// Rocks.
    pub rocks: Vec<[f32; 16]>,
}

/// SplitMix64 (Steele, Lea and Flood 2014): a small seeded stream, the same on every platform.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (`n > 0`).
    fn pick(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn translation(x: f32, y: f32, z: f32) -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, x, y, z, 1.0,
    ]
}

/// Uniform scale `s`, a quarter turn about +Y when `quarter`, then a translation.
fn placement(x: f32, y: f32, z: f32, s: f32, quarter: bool) -> [f32; 16] {
    if quarter {
        [
            0.0, 0.0, -s, 0.0, 0.0, s, 0.0, 0.0, s, 0.0, 0.0, 0.0, x, y, z, 1.0,
        ]
    } else {
        [s, 0.0, 0.0, 0.0, 0.0, s, 0.0, 0.0, 0.0, 0.0, s, 0.0, x, y, z, 1.0]
    }
}

/// `v` rounded to the nearest quarter metre (exact in `f32` across the map).
fn quarter_metre(v: f32) -> f32 {
    (v * 4.0).round() / 4.0
}

/// Distance from `(x, z)` to the rectangle `min..max` (0 inside).
fn rect_distance(x: f32, z: f32, min: [f32; 2], max: [f32; 2]) -> f32 {
    let dx = (min[0] - x).max(x - max[0]).max(0.0);
    let dz = (min[1] - z).max(z - max[1]).max(0.0);
    (dx * dx + dz * dz).sqrt()
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// An axis-aligned box's faces as quads facing out (`outward`) or in, without the top and/or
/// bottom face when asked.
fn box_faces(min: [f32; 3], max: [f32; 3], outward: bool, top: bool, bottom: bool) -> PtMesh {
    let mut m = PtMesh::default();
    for axis in 0..3 {
        let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
        for high in [false, true] {
            if axis == 1 && ((high && !top) || (!high && !bottom)) {
                continue;
            }
            let at = if high { max[axis] } else { min[axis] };
            let corner = |a: f32, b: f32| {
                let mut c = [0.0f32; 3];
                c[axis] = at;
                c[u] = a;
                c[v] = b;
                c
            };
            let c = [
                corner(min[u], min[v]),
                corner(max[u], min[v]),
                corner(max[u], max[v]),
                corner(min[u], max[v]),
            ];
            let face = quad(c);
            let want = if high == outward { 1.0 } else { -1.0 };
            let face = if face.normals[0][axis] * want > 0.0 {
                face
            } else {
                quad([c[0], c[3], c[2], c[1]])
            };
            merge(&mut m, &face);
        }
    }
    m
}

/// The quad over the corners `c` (in order around it), wound to face up.
fn up_quad(c: [[f32; 3]; 4]) -> PtMesh {
    let q = quad(c);
    if q.normals[0][1] > 0.0 {
        q
    } else {
        quad([c[0], c[3], c[2], c[1]])
    }
}

/// A two-sided shell around the inner box `(-sx/2, 0, -sz/2)..(sx/2, sy, sz/2)`: the inner box
/// facing in, the outer box (walls `WALL` thick) facing out. An open shell has no roof; four
/// mitred quads close the wall tops, so every edge still joins exactly two triangles.
#[must_use]
pub fn shell_mesh(size: [f32; 3], open_top: bool) -> PtMesh {
    let (ix, iz, h) = (0.5 * size[0], 0.5 * size[2], size[1]);
    let (ox, oz) = (ix + WALL, iz + WALL);
    let mut m = box_faces([-ix, 0.0, -iz], [ix, h, iz], false, !open_top, true);
    let top = if open_top { h } else { h + WALL };
    merge(
        &mut m,
        &box_faces([-ox, -WALL, -oz], [ox, top, oz], true, !open_top, true),
    );
    if open_top {
        let inner = [[-ix, h, -iz], [ix, h, -iz], [ix, h, iz], [-ix, h, iz]];
        let outer = [[-ox, h, -oz], [ox, h, -oz], [ox, h, oz], [-ox, h, oz]];
        for k in 0..4 {
            let next = (k + 1) % 4;
            merge(&mut m, &up_quad([inner[k], inner[next], outer[next], outer[k]]));
        }
    }
    m
}

impl SyntheticLevel {
    /// A preset by name ([`SYNTHETIC_LEVELS`]).
    #[must_use]
    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "bore12" => Some(Self::bore_twelve_scale()),
            "small" => Some(Self::small()),
            "tall" => Some(Self::tall()),
            _ => None,
        }
    }

    /// The scale of DO27's Bore Twelve (a 1025 × 1025 terrain grid at 1.25 m, a borehole shaft
    /// about 45 × 100 × 45 m; ADR-279, ADR-280), with 20 interiors of our own sizes: the shaft,
    /// two hangars with vehicle bays, six habs, four workshops, three bunkers, two corridors, a
    /// control room and a store. 12 towers, 40 containers, 600 rocks and a backdrop ring.
    #[must_use]
    pub fn bore_twelve_scale() -> Self {
        let mut interiors = Vec::with_capacity(20);
        interiors.push(SyntheticInterior {
            depth: 90.0,
            sky_open: true,
            ..SyntheticInterior::new("shaft", "shaft", [0.0, 0.0], [45.0, 100.0, 45.0])
        });
        for (id, at) in [("hangar_a", [-220.0, 140.0]), ("hangar_b", [240.0, -160.0])] {
            interiors.push(SyntheticInterior {
                openings: true,
                ..SyntheticInterior::new(id, "hangar", at, [60.0, 16.0, 40.0])
            });
        }
        for k in 0..6u8 {
            let at = [-180.0 + 32.0 * f32::from(k % 3), -150.0 + 30.0 * f32::from(k / 3)];
            interiors.push(SyntheticInterior::new(
                &format!("hab_{}", k + 1),
                "hab",
                at,
                [24.0, 6.0, 16.0],
            ));
        }
        for k in 0..4u8 {
            interiors.push(SyntheticInterior::new(
                &format!("workshop_{}", k + 1),
                "workshop",
                [160.0 + 30.0 * f32::from(k), 200.0],
                [20.0, 8.0, 20.0],
            ));
        }
        for (k, at) in [[-350.0, -300.0], [330.0, 330.0], [-60.0, 380.0]]
            .into_iter()
            .enumerate()
        {
            interiors.push(SyntheticInterior::new(
                &format!("bunker_{}", k + 1),
                "bunker",
                at,
                [16.0, 4.0, 12.0],
            ));
        }
        interiors.push(SyntheticInterior::new(
            "corridor_1",
            "corridor_x",
            [-60.0, -150.0],
            [48.0, 4.0, 6.0],
        ));
        interiors.push(SyntheticInterior::new(
            "corridor_2",
            "corridor_z",
            [100.0, 120.0],
            [6.0, 4.0, 48.0],
        ));
        interiors.push(SyntheticInterior::new(
            "control",
            "control",
            [90.0, 60.0],
            [30.0, 10.0, 24.0],
        ));
        interiors.push(SyntheticInterior::new(
            "storage",
            "storage",
            [-260.0, 280.0],
            [32.0, 10.0, 32.0],
        ));
        Self {
            name: "bore12".into(),
            grid: 1025,
            grid_spacing: 1.25,
            relief: 30.0,
            interiors,
            towers: 12,
            tower_ring: 330.0,
            containers: 40,
            rocks: 600,
            backdrop: true,
            seed: 0x0b0e_1200,
        }
    }

    /// A 160 m level with four interiors (one shaft): the same generator, small enough for unit
    /// tests.
    #[must_use]
    pub fn small() -> Self {
        let interiors = vec![
            SyntheticInterior {
                depth: 15.0,
                sky_open: true,
                ..SyntheticInterior::new("shaft", "shaft", [0.0, 0.0], [10.0, 20.0, 10.0])
            },
            SyntheticInterior::new("hab", "hab", [-30.0, -25.0], [12.0, 4.0, 8.0]),
            SyntheticInterior {
                openings: true,
                ..SyntheticInterior::new("hangar", "hangar", [30.0, 20.0], [20.0, 8.0, 14.0])
            },
            SyntheticInterior::new("bunker", "bunker", [-30.0, 30.0], [8.0, 3.0, 6.0]),
        ];
        Self {
            name: "small".into(),
            grid: 129,
            grid_spacing: 1.25,
            relief: 4.0,
            interiors,
            towers: 2,
            tower_ring: 50.0,
            containers: 4,
            rocks: 20,
            backdrop: true,
            seed: 7,
        }
    }

    /// A tall map (ADR-3006): most of its volume is stacked on a small footprint. On a 320 m terrain:
    /// a 300 m shaft open to the sky, a 200 m closed tower, a block of 12 floors stacked at one spot
    /// (separate bricks, one over the other) and six levels of crossing tunnels under the surface.
    #[must_use]
    pub fn tall() -> Self {
        let mut interiors = vec![
            SyntheticInterior {
                depth: 290.0,
                sky_open: true,
                ..SyntheticInterior::new("shaft", "shaft", [-90.0, -90.0], [45.0, 300.0, 45.0])
            },
            SyntheticInterior::new("tower", "tower", [90.0, -90.0], [30.0, 200.0, 30.0]),
        ];
        for floor in 0..12u8 {
            interiors.push(SyntheticInterior {
                // Each floor is 6 m tall with a 2 m slab between; the stack starts at the pad.
                depth: -8.0 * f32::from(floor),
                ..SyntheticInterior::new(
                    &format!("floor_{floor}"),
                    "floor",
                    [90.0, 90.0],
                    [40.0, 6.0, 40.0],
                )
            });
        }
        for level in 0..6u8 {
            let depth = 20.0 + 24.0 * f32::from(level);
            interiors.push(SyntheticInterior {
                depth,
                ..SyntheticInterior::new(
                    &format!("tunnel_{level}_x"),
                    "tunnel_x",
                    [0.0, 40.0],
                    [260.0, 5.0, 6.0],
                )
            });
            interiors.push(SyntheticInterior {
                depth,
                ..SyntheticInterior::new(
                    &format!("tunnel_{level}_z"),
                    "tunnel_z",
                    [-30.0, 0.0],
                    [6.0, 5.0, 260.0],
                )
            });
        }
        Self {
            name: "tall".into(),
            grid: 257,
            grid_spacing: 1.25,
            relief: 2.0,
            interiors,
            towers: 0,
            tower_ring: 0.0,
            containers: 0,
            rocks: 0,
            backdrop: false,
            seed: 11,
        }
    }

    /// Half the terrain's extent, metres.
    #[must_use]
    pub fn half_extent(&self) -> f32 {
        0.5 * (self.grid.saturating_sub(1)) as f32 * self.grid_spacing
    }

    /// Open terrain height (no pads) at world `(x, z)`.
    fn base_height(&self, x: f32, z: f32) -> f32 {
        self.relief
            * (0.6 * (x * 0.0061).sin() * (z * 0.0047).cos()
                + 0.25 * (x * 0.019 + z * 0.013).sin()
                + 0.15 * (x * 0.031 - z * 0.027).cos())
    }

    /// Ground pad height of an interior: the open terrain at its centre, to a quarter metre.
    #[must_use]
    pub fn pad_height(&self, interior: &SyntheticInterior) -> f32 {
        quarter_metre(self.base_height(interior.at[0], interior.at[1]))
    }

    /// Terrain height at world `(x, z)`: exactly an interior's pad within 2 m of its footprint,
    /// else the open terrain blended to each pad near it (flat within `PAD_FLAT` metres, back
    /// to the open terrain over the next `PAD_BLEND`).
    #[must_use]
    pub fn height_at(&self, x: f32, z: f32) -> f32 {
        let distance = |i: &SyntheticInterior| {
            let (min, max) = i.footprint();
            rect_distance(x, z, min, max)
        };
        if let Some(i) = self.interiors.iter().find(|i| distance(i) <= 2.0) {
            return self.pad_height(i);
        }
        let mut h = self.base_height(x, z);
        for i in &self.interiors {
            let w = 1.0 - smoothstep((distance(i) - PAD_FLAT) / PAD_BLEND);
            if w >= 1.0 {
                h = self.pad_height(i);
            } else if w > 0.0 {
                h += (self.pad_height(i) - h) * w;
            }
        }
        h
    }

    /// The world box of an interior's inner volume: `(min, max)`.
    #[must_use]
    pub fn interior_box(&self, interior: &SyntheticInterior) -> ([f32; 3], [f32; 3]) {
        let base = self.pad_height(interior) - interior.depth;
        let (hx, hz) = (0.5 * interior.size[0], 0.5 * interior.size[2]);
        (
            [interior.at[0] - hx, base, interior.at[1] - hz],
            [interior.at[0] + hx, base + interior.size[1], interior.at[1] + hz],
        )
    }

    fn near_interior(&self, x: f32, z: f32, clearance: f32) -> bool {
        self.interiors.iter().any(|i| {
            let (min, max) = i.footprint();
            rect_distance(x, z, min, max) < clearance
        })
    }

    /// The outdoor placements: towers on a ring (a slot within 10 m of an interior is left
    /// empty), containers beside the interiors (on their pads), rocks on a half-metre grid away
    /// from interiors and towers.
    #[must_use]
    pub fn placements(&self) -> SyntheticPlacements {
        let mut out = SyntheticPlacements::default();
        let mut tower_spots: Vec<[f32; 2]> = Vec::new();
        for k in 0..self.towers {
            let angle = k as f32 / self.towers.max(1) as f32 * std::f32::consts::TAU;
            let x = (self.tower_ring * angle.cos() * 2.0).round() / 2.0;
            let z = (self.tower_ring * angle.sin() * 2.0).round() / 2.0;
            if self.near_interior(x, z, 13.0) {
                continue;
            }
            let y = quarter_metre(self.height_at(x, z)) - 2.0;
            out.towers.push(translation(x, y, z));
            tower_spots.push([x, z]);
        }
        let anchors: Vec<&SyntheticInterior> = self.interiors.iter().filter(|i| !i.sky_open).collect();
        if !anchors.is_empty() {
            for k in 0..self.containers as usize {
                let a = anchors[k % anchors.len()];
                let side = (k / anchors.len()) % 4;
                let gap = WALL + 2.0 + 1.25;
                let (x, z, quarter) = match side {
                    0 => (a.at[0] + 0.5 * a.size[0] + gap, a.at[1], true),
                    1 => (a.at[0] - 0.5 * a.size[0] - gap, a.at[1], true),
                    2 => (a.at[0], a.at[1] + 0.5 * a.size[2] + gap, false),
                    _ => (a.at[0], a.at[1] - 0.5 * a.size[2] - gap, false),
                };
                out.containers
                    .push(placement(x, self.pad_height(a), z, 1.0, quarter));
            }
        }
        // Rocks within 7/8 of the half extent (inside the backdrop ring).
        let lim = (self.half_extent() * 0.875).floor() as u64;
        let mut rng = SplitMix(self.seed);
        let scales = [1.0f32, 1.5, 2.0, 3.0];
        let mut tries = 0u32;
        while out.rocks.len() < self.rocks as usize && tries < self.rocks.saturating_mul(50) {
            tries += 1;
            let x = rng.pick(4 * lim + 1) as f32 * 0.5 - lim as f32;
            let z = rng.pick(4 * lim + 1) as f32 * 0.5 - lim as f32;
            let s = scales[rng.pick(scales.len() as u64) as usize];
            if self.near_interior(x, z, 12.0)
                || tower_spots
                    .iter()
                    .any(|t| (t[0] - x).abs() < 10.0 && (t[1] - z).abs() < 10.0)
            {
                continue;
            }
            let y = quarter_metre(self.height_at(x, z)) - 0.5;
            out.rocks.push(placement(x, y, z, s, false));
        }
        out
    }

    /// The terrain grid mesh, facing up, without the cells over a sky-open interior (the
    /// shaft's hole: the bake's rasterized heightfield is NaN there).
    #[must_use]
    pub fn terrain_mesh(&self) -> PtMesh {
        let n = self.grid.max(2);
        let half = self.half_extent();
        let s = self.grid_spacing;
        let mut m = PtMesh::default();
        m.positions.reserve((n * n) as usize);
        for j in 0..n {
            for i in 0..n {
                let (x, z) = (i as f32 * s - half, j as f32 * s - half);
                m.positions.push([x, self.height_at(x, z), z]);
            }
        }
        m.normals = vec![[0.0; 3]; m.positions.len()];
        let holes: Vec<([f32; 2], [f32; 2])> = self
            .interiors
            .iter()
            .filter(|i| i.sky_open)
            .map(SyntheticInterior::footprint)
            .collect();
        for j in 0..n - 1 {
            for i in 0..n - 1 {
                let (cx, cz) = ((i as f32 + 0.5) * s - half, (j as f32 + 0.5) * s - half);
                if holes
                    .iter()
                    .any(|(lo, hi)| cx > lo[0] && cx < hi[0] && cz > lo[1] && cz < hi[1])
                {
                    continue;
                }
                let a = j * n + i;
                let (b, c) = (a + 1, a + n);
                m.indices.extend_from_slice(&[a, c, b, b, c, c + 1]);
            }
        }
        m
    }

    /// The backdrop: 36 cliff slabs (60 m wide, 12 m thick, 140 m tall) on a ring at 15/16 of
    /// the half extent (600 m on the 1280 m map), as one mesh in world space.
    #[must_use]
    pub fn backdrop_mesh(&self) -> PtMesh {
        let r = self.half_extent() * 0.9375;
        let mut m = PtMesh::default();
        for k in 0..36 {
            let angle = k as f32 / 36.0 * std::f32::consts::TAU;
            let slab = box_faces([-30.0, -40.0, -6.0], [30.0, 100.0, 6.0], true, true, true);
            // A rotation about +Y (determinant +1, so the winding stays front-facing): local X
            // along the ring's tangent, local Z along its radius, the long side facing the centre.
            let (c, s) = (angle.cos(), angle.sin());
            let (px, pz) = (r * c, r * s);
            for p in &slab.positions {
                let (x, z) = (p[0], p[2]);
                m.positions.push([px + x * s + z * c, p[1], pz - x * c + z * s]);
            }
            for nrm in &slab.normals {
                let (x, z) = (nrm[0], nrm[2]);
                m.normals.push([x * s + z * c, nrm[1], -x * c + z * s]);
            }
            let base = (m.positions.len() - slab.positions.len()) as u32;
            m.indices.extend(slab.indices.iter().map(|i| i + base));
        }
        m
    }

    /// The mesh of a synthetic source by file stem (`terrain`, `shell_<class>`, `tower`,
    /// `container`, `rock`, `backdrop`).
    #[must_use]
    pub fn mesh(&self, stem: &str) -> Option<PtMesh> {
        match stem {
            "terrain" => Some(self.terrain_mesh()),
            "tower" => Some(box_faces([-3.0, 0.0, -3.0], [3.0, 30.0, 3.0], true, true, true)),
            "container" => Some(box_faces([-6.0, 0.0, -1.25], [6.0, 2.5, 1.25], true, true, true)),
            "rock" => Some(uv_sphere(1.0, 8, 6, false)),
            "backdrop" => Some(self.backdrop_mesh()),
            _ => {
                let class = stem.strip_prefix("shell_")?;
                let i = self.interiors.iter().find(|i| i.class == class)?;
                Some(shell_mesh(i.size, i.sky_open))
            }
        }
    }

    /// The in-memory loader of this level's sources.
    #[must_use]
    pub fn loader(&self) -> SyntheticMeshLoader {
        SyntheticMeshLoader { level: self.clone() }
    }

    /// The `.bake.ron` v1 description of this level with `layout` (sources under `synthetic/`,
    /// served by [`Self::loader`]). The same level and layout give the same text.
    #[must_use]
    pub fn bake_ron(&self, layout: &SizingLayout) -> String {
        let p = self.placements();
        let mut t = String::new();
        let _ = writeln!(
            t,
            "// Synthetic stand-in `{}` (regia_pt::bake_sizing, ADR-1099).",
            self.name
        );
        let _ = writeln!(t, "// No DO27 content: every mesh is generated.");
        t.push_str("(\n    format_version: 1,\n");
        let _ = writeln!(
            t,
            "    terrain_mesh: Some((source: \"synthetic/terrain.mesh\", spacing: {}, albedo: (0.3, 0.28, 0.26))),",
            ron_f32(self.grid_spacing)
        );
        t.push_str(
            "    sun: Some((toward: (0.35, 0.8, 0.25), illuminance: 100000.0, color: (1.0, 0.96, 0.9), angular_radius: 0.00465)),\n",
        );
        t.push_str("    sky: Some((2500.0, 3500.0, 5000.0)),\n");
        t.push_str("    meshes: [\n");
        for i in &self.interiors {
            let base = self.pad_height(i) - i.depth;
            let _ = writeln!(
                t,
                "        (id: Some(\"{}\"), source: \"synthetic/shell_{}.mesh\", transform: {}, layer: Some(\"interior_shell\")),",
                i.id,
                i.class,
                ron_mat(&translation(i.at[0], base, i.at[1]))
            );
        }
        let mut instanced = |id: &str, layer: &str, list: &[[f32; 16]], approximate: bool| {
            if list.is_empty() {
                return;
            }
            let _ = writeln!(
                t,
                "        (id: Some(\"{id}\"), source: \"synthetic/{id}.mesh\", layer: Some(\"{layer}\"), approximate: {approximate}, instances: Some(["
            );
            for m in list {
                let _ = writeln!(t, "            {},", ron_mat(m));
            }
            t.push_str("        ])),\n");
        };
        instanced("tower", "setpiece", p.towers.as_slice(), false);
        instanced("container", "setpiece", p.containers.as_slice(), false);
        instanced("rock", "scatter", p.rocks.as_slice(), true);
        if self.backdrop {
            t.push_str(
                "        (id: Some(\"backdrop\"), source: \"synthetic/backdrop.mesh\", layer: Some(\"backdrop\")),\n",
            );
        }
        t.push_str("    ],\n    interiors: [\n");
        for i in &self.interiors {
            let (min, max) = self.interior_box(i);
            let spacing = if i.sky_open {
                layout.shaft_spacing.or(layout.interior_spacing)
            } else {
                layout.interior_spacing
            };
            let _ = writeln!(
                t,
                "        (id: \"{}\", min: {}, max: {}, sky_open: {}, openings: {}, spacing: {}),",
                i.id,
                ron_vec3(min),
                ron_vec3(max),
                i.sky_open,
                i.openings,
                ron_opt(spacing)
            );
        }
        t.push_str("    ],\n    layout: (\n");
        let _ = writeln!(
            t,
            "        terrain_spacing: {},",
            match layout.terrain_spacing {
                Some(s) => format!("Some(({}, {}))", ron_f32(s), ron_f32(s)),
                None => "None".into(),
            }
        );
        let _ = writeln!(t, "        levels: [{}],", ron_list(&layout.levels));
        let layers: Vec<String> = layout.layers.iter().map(|l| format!("\"{l}\"")).collect();
        let _ = writeln!(
            t,
            "        auto_bricks: Some((spacing: {}, margin: {}, max_probes: {}, layers: [{}], interiors: true)),",
            ron_f32(layout.auto_spacing),
            ron_f32(layout.margin),
            layout.max_probes,
            layers.join(", ")
        );
        let _ = writeln!(t, "        moment_resolution: {},", layout.moment_resolution);
        t.push_str("    ),\n");
        let _ = writeln!(
            t,
            "    bake: (rays_per_probe: {}, samples: {}, probes_per_batch: {}),",
            layout.rays_per_probe, layout.samples, layout.probes_per_batch
        );
        t.push_str(")\n");
        t
    }

    /// Builds the bake job of this level with `layout` through the description path
    /// (`GiBakeDescription::parse_versioned` + `build_with` with [`Self::loader`]).
    ///
    /// # Errors
    ///
    /// The description does not parse or build (a [`BakeDescError`] with its agent code), for
    /// example when the automatic bricks cannot fit `max_probes`.
    pub fn job(&self, layout: &SizingLayout) -> Result<GiBakeJob, BakeDescError> {
        let _span = tracing::info_span!("pt_bake_sizing_job", level = %self.name).entered();
        let (desc, _) = GiBakeDescription::parse_versioned(&self.bake_ron(layout))?;
        desc.build_with(Path::new(""), &mut self.loader())
    }
}

fn ron_f32(v: f32) -> String {
    format!("{v:?}")
}

fn ron_vec3(v: [f32; 3]) -> String {
    format!("({}, {}, {})", ron_f32(v[0]), ron_f32(v[1]), ron_f32(v[2]))
}

fn ron_list(v: &[f32]) -> String {
    v.iter().map(|x| ron_f32(*x)).collect::<Vec<_>>().join(", ")
}

fn ron_mat(m: &[f32; 16]) -> String {
    format!("({})", ron_list(m))
}

fn ron_opt(v: Option<f32>) -> String {
    match v {
        Some(x) => format!("Some({})", ron_f32(x)),
        None => "None".into(),
    }
}

/// Serves a [`SyntheticLevel`]'s meshes by file stem (`synthetic/<stem>.mesh`): one plain grey
/// part each (0.45 albedo; the terrain's albedo comes from the description).
#[derive(Clone, Debug)]
pub struct SyntheticMeshLoader {
    level: SyntheticLevel,
}

impl BakeMeshLoader for SyntheticMeshLoader {
    fn load(&mut self, path: &Path) -> Result<BakeMeshSource, String> {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let mesh = self
            .level
            .mesh(stem)
            .ok_or_else(|| format!("{}: not a synthetic source", path.display()))?;
        Ok(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(mesh, [0.45; 3])],
        })
    }

    fn companions(&self, _path: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// The layout and bake settings a sizing run writes into the description.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SizingLayout {
    /// Terrain layer column spacing, metres (`None` = no terrain layer).
    pub terrain_spacing: Option<f32>,
    /// Terrain layer heights above the ground, metres.
    pub levels: Vec<f32>,
    /// `auto_bricks.spacing`, metres.
    pub auto_spacing: f32,
    /// Spacing of the interiors' bricks (`None` = the auto spacing).
    pub interior_spacing: Option<f32>,
    /// Spacing of sky-open interiors' bricks (`None` = as the other interiors).
    pub shaft_spacing: Option<f32>,
    /// `auto_bricks.margin`, metres.
    pub margin: f32,
    /// `auto_bricks.max_probes`.
    pub max_probes: u32,
    /// `auto_bricks.layers` (empty = every layer but `terrain`).
    pub layers: Vec<String>,
    /// Octahedral moment texels per edge.
    pub moment_resolution: u32,
    /// Rays per probe per sample.
    pub rays_per_probe: u32,
    /// Samples per probe.
    pub samples: u32,
    /// Probes per GPU batch.
    pub probes_per_batch: u32,
}

impl SizingLayout {
    /// The `.bake.ron` defaults with an 8 m terrain layer: automatic bricks at 1 m on every layer
    /// but `terrain`, 262 144 probes at most, 4 × 4 moments, 256 rays × 16 samples.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            terrain_spacing: Some(8.0),
            levels: vec![1.0, 2.0, 4.0, 8.0, 16.0],
            auto_spacing: 1.0,
            interior_spacing: None,
            shaft_spacing: None,
            margin: 2.0,
            max_probes: 1 << 18,
            layers: Vec::new(),
            moment_resolution: 4,
            rays_per_probe: 256,
            samples: 16,
            probes_per_batch: 4096,
        }
    }

    /// The example of `docs/formats/bake-ron.md`: the defaults with automatic bricks on the
    /// `setpiece` layer only.
    #[must_use]
    pub fn doc_example() -> Self {
        Self {
            layers: vec!["setpiece".into()],
            ..Self::defaults()
        }
    }

    /// The layout `docs/research/bake-sizing-bore-twelve.md` recommends for a Bore-Twelve-sized
    /// map: set pieces at 1 m, interiors at 1 m, the sky-open shaft at 2 m, an 8 m terrain layer,
    /// room in `max_probes` for all of it, and the largest power-of-two probe batch whose path
    /// buffer fits a 128 MiB binding at 256 rays (2048 × 256 × 192 bytes = 96 MiB).
    #[must_use]
    pub fn recommended() -> Self {
        Self {
            shaft_spacing: Some(2.0),
            max_probes: 1 << 19,
            layers: vec!["setpiece".into()],
            probes_per_batch: 2048,
            ..Self::defaults()
        }
    }

    /// A preset by name ([`SIZING_LAYOUTS`]).
    #[must_use]
    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "defaults" => Some(Self::defaults()),
            "doc-example" => Some(Self::doc_example()),
            "recommended" => Some(Self::recommended()),
            _ => None,
        }
    }
}

/// The layouts `bake_sizing sweep` compares: the three presets, then the recommendation with one
/// knob changed each.
#[must_use]
pub fn sweep_layouts() -> Vec<(String, SizingLayout)> {
    fn with(name: &str, f: impl Fn(&mut SizingLayout)) -> (String, SizingLayout) {
        let mut l = SizingLayout::recommended();
        f(&mut l);
        (name.to_string(), l)
    }
    vec![
        ("defaults".into(), SizingLayout::defaults()),
        ("doc-example".into(), SizingLayout::doc_example()),
        ("recommended".into(), SizingLayout::recommended()),
        with("terrain-4m", |l| l.terrain_spacing = Some(4.0)),
        with("terrain-16m", |l| l.terrain_spacing = Some(16.0)),
        with("interiors-0.5m", |l| l.interior_spacing = Some(0.5)),
        with("shaft-1m", |l| l.shaft_spacing = None),
        with("bricks-2m", |l| l.auto_spacing = 2.0),
        with("plus-scatter", |l| l.layers.push("scatter".into())),
        with("plus-shells", |l| l.layers.push("interior_shell".into())),
        with("moments-8", |l| l.moment_resolution = 8),
    ]
}

/// Bytes of a volume with a given layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct VolumeBytes {
    /// Meta, SH and moment bytes per probe.
    pub per_probe: u64,
    /// The four sections of the file, each padded to 16 bytes.
    pub payload: u64,
    /// Upper bound of the file (payload, header, magic and padding).
    pub file_max: u64,
    /// The renderer's one storage buffer: the sections, the ground heights and the brick table.
    pub runtime: u64,
}

/// The volume bytes of `layout` (`.fgiv` v2 encoding: `regia_asset::baked_gi`).
#[must_use]
pub fn volume_bytes(layout: &VolumeLayout) -> VolumeBytes {
    let n = layout.probe_count() as u64;
    let texels = u64::from(layout.moment_resolution) * u64::from(layout.moment_resolution);
    let meta = n * META_RECORD_BYTES as u64;
    let sh = n * (GI_COMPONENTS * SH_RECORD_BYTES) as u64;
    let moments = n * texels * MOMENT_TEXEL_BYTES as u64;
    let ground = layout.terrain.as_ref().map_or(0, |t| t.ground.len() as u64 * 4);
    let levels = layout.terrain.as_ref().map_or(0, |t| t.levels.len() as u64);
    let bricks = layout.bricks.len() as u64;
    let payload: u64 = [meta, sh, moments, ground]
        .iter()
        .map(|b| b.next_multiple_of(16))
        .sum();
    let header = HEADER_FIXED_MAX + 4 * levels + HEADER_BRICK_MAX * bricks;
    VolumeBytes {
        per_probe: META_RECORD_BYTES as u64
            + (GI_COMPONENTS * SH_RECORD_BYTES) as u64
            + texels * MOMENT_TEXEL_BYTES as u64,
        payload,
        file_max: (12 + header).next_multiple_of(16) + payload,
        runtime: (meta + sh + moments + ground + RUNTIME_BRICK_BYTES * bricks).max(16),
    }
}

/// The rays a bake starts (primary rays; each path then takes up to `max_bounces` vertices).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BakeRays {
    /// Probes traced: every probe not under the terrain.
    pub live_probes: u64,
    /// Rays of the main trace: live probes × rays × samples.
    pub main: u64,
    /// Rays of the relocation rounds when every round runs (they stop early once nothing moves).
    pub relocation_max: u64,
}

/// GPU buffer bytes of one probe trace (`PathTracer::trace_probes` as of v0.1.27).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BakeGpuBytes {
    /// Probes per batch actually used (the setting, capped by the live probes).
    pub batch_probes: u64,
    /// Path slots per batch (batch × rays).
    pub path_slots: u64,
    /// The packed scene: BVH nodes, triangles, instances, materials, textures, lights,
    /// environment and the terrain pyramid.
    pub scene: u64,
    /// The path buffer.
    pub paths: u64,
    /// The hit buffer.
    pub hits: u64,
    /// The probe accumulator (and its readback copy, the same size again).
    pub accum: u64,
    /// The largest single storage binding (must fit the device's binding limit).
    pub largest_binding: u64,
    /// Everything together.
    pub total: u64,
}

/// One brick in a sizing report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BrickSize {
    /// `interior:<id>`, `cluster:<first mesh id>` (`+N` more meshes), `brick:<index>` or
    /// `builtin`.
    pub label: String,
    /// Probes.
    pub probes: usize,
    /// Spacing, metres (the smallest axis).
    pub spacing: f32,
    /// Probes per axis.
    pub counts: [u32; 3],
}

/// What a layout costs ([`size_job`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SizingReport {
    /// Probes in the volume.
    pub probes: usize,
    /// Probes in the terrain layer.
    pub terrain_probes: usize,
    /// Probes in the bricks over interiors.
    pub interior_probes: usize,
    /// Probes in the bricks over mesh clusters.
    pub cluster_probes: usize,
    /// Probes in explicit or built-in bricks.
    pub other_brick_probes: usize,
    /// Detail bricks.
    pub bricks: usize,
    /// Bricks over interiors.
    pub interior_bricks: usize,
    /// Bricks the plan flags INTERIOR.
    pub interior_flagged: usize,
    /// Bricks over mesh clusters.
    pub cluster_bricks: usize,
    /// Probes whose nominal position is under the traced heightfield: stored, never traced.
    pub under_terrain: usize,
    /// Mesh-cluster spacing multiplier from [`crate::bake_desc::AutoBrickReport::scale`]
    /// (1 = as requested). Interior bricks use [`crate::bake_desc::AutoBrickReport::interior_scale`],
    /// which this report does not copy. That factor stays 1 unless the interiors alone leave
    /// fewer than 8 probes per cluster.
    pub auto_scale: Option<f32>,
    /// Mesh placements in the scene.
    pub placements: usize,
    /// Triangles over every placement.
    pub triangles: u64,
    /// The five bricks with the most probes.
    pub largest_bricks: Vec<BrickSize>,
    /// Volume bytes.
    pub volume: VolumeBytes,
    /// [`PORTABLE_BINDING_BYTES`].
    pub portable_limit: u64,
    /// The unsplit `volume.runtime` fits [`PORTABLE_BINDING_BYTES`]. A `false` value is not a
    /// refusal: [`crate::bake_desc::GiBakeDescription`] splits a larger layout into at most four
    /// parts, each at most that limit, when a split fits (ADR-2618). `bake_volume_too_large`
    /// is only a layout no split fits.
    pub fits_portable: bool,
    /// Rays the bake starts.
    pub rays: BakeRays,
    /// GPU buffers of the bake.
    pub gpu: BakeGpuBytes,
    /// The bake's GPU path buffer fits [`PORTABLE_BINDING_BYTES`].
    pub gpu_fits_portable: bool,
    /// Estimated host memory peak of the bake's probe data (`bake_host_bytes_estimate`).
    pub host_peak_estimate: u64,
    /// The bytes against every tier's budget, what is over and what fixes it ([`job_memory`]).
    pub memory: BakedGiMemoryReport,
    /// What cutting the layout along each axis would do ([`split_axis_study`]).
    pub split_study: SplitAxisStudy,
}

/// The memory answer for `job` (ADR-3006, `regia_render::baked_gi::memory`): the bytes it would bake
/// to against every tier's budget, what is over and what fixes it, with the plan's names for its
/// bricks.
#[must_use]
pub fn job_memory(job: &GiBakeJob) -> BakedGiMemoryReport {
    let label = |index: usize| {
        let source = job.plan.bricks.get(index).map(|p| &p.source);
        brick_label(source, index)
    };
    memory::layout_report(&job.layout, &label)
}

/// What cutting a layout along each axis would do (ADR-3006): the numbers behind keeping splits in X
/// and Z. Each `largest` is the largest part's runtime bytes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SplitAxisStudy {
    /// The whole layout's runtime bytes.
    pub whole: u64,
    /// The largest brick's bytes: no cut that keeps bricks whole gets below it.
    pub largest_brick: u64,
    /// Parts of the split the bake writes (`split_layout` at the portable binding), 0 when none fits
    /// (or none is needed: `whole` fits one binding).
    pub xz_parts: usize,
    /// The largest part of a 2 × 2 cut in X and Z at the byte quantiles, bricks whole (as the bake
    /// cuts them: a brick across a cut goes into both parts).
    pub xz_whole_bricks: u64,
    /// The largest slab of four cut in Y at the byte quantiles, bricks whole, the terrain layer in
    /// the slabs its probes reach. What a Y split would give without cutting a brick.
    pub y_whole_bricks: u64,
    /// The largest part of the same 2 × 2 cut with each brick cropped to its part plus a margin
    /// of two of its cells: what X and Z give once a brick may be cut.
    pub xz_cropped_bricks: u64,
    /// The largest slab of the four in Y with each brick cropped the same way.
    pub y_cropped_bricks: u64,
    /// Each part's runtime bytes in the split the bake writes (`split_layout` at the portable
    /// binding; empty when none fits or none is needed). Past every whole-brick cut these are the
    /// cropped parts (ADR-3237).
    pub split_parts: Vec<u64>,
    /// Per part of that split, the bricks it holds cropped at a cut (ADR-3237; all 0 for a split
    /// that keeps bricks whole).
    pub split_cropped_bricks: Vec<usize>,
}

/// Probes of `counts` along `axis` between world `lo..hi` (a brick cropped to a box, by whole cells
/// plus `margin` of them), at least one.
fn cropped_count(origin: f32, spacing: f32, count: u32, lo: f32, hi: f32, margin: f32) -> u64 {
    let first = (((lo - origin) / spacing).floor() - margin).max(0.0);
    let last = (((hi - origin) / spacing).ceil() + margin).min(count.saturating_sub(1) as f32);
    if last < first {
        0
    } else {
        (last - first) as u64 + 1
    }
}

/// The values at the `tiles` byte quantiles of `weights` (position, bytes), as cut positions.
fn weight_cuts(weights: &mut [(f32, u64)], tiles: u64) -> Vec<f32> {
    weights.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: u64 = weights.iter().map(|w| w.1).sum();
    let mut cuts = Vec::new();
    let mut acc = 0u64;
    let mut k = 1u64;
    for (at, bytes) in weights.iter() {
        acc += bytes;
        while k < tiles && acc * tiles >= total * k {
            cuts.push(*at);
            k += 1;
        }
    }
    cuts
}

/// Measures the cuts of [`SplitAxisStudy`] on `layout`, without building any of them.
#[must_use]
pub fn split_axis_study(layout: &VolumeLayout, terrain_heights: Option<(f32, f32)>) -> SplitAxisStudy {
    let per_probe = volume_bytes(layout).per_probe;
    let whole = layout.runtime_buffer_bytes();
    let brick_bytes = |b: &regia_asset::baked_gi::DetailBrick| b.probe_count() as u64 * per_probe;
    let largest_brick = layout.bricks.iter().map(brick_bytes).max().unwrap_or(0);
    let terrain_bytes = layout.terrain.as_ref().map_or(0, |t| {
        t.probe_count() as u64 * per_probe + t.ground.len() as u64 * 4
    });
    let span = |b: &regia_asset::baked_gi::DetailBrick, a: usize| {
        (
            b.origin[a],
            b.origin[a] + b.counts[a].saturating_sub(1) as f32 * b.spacing[a],
        )
    };
    let centre = |b: &regia_asset::baked_gi::DetailBrick, a: usize| {
        let (lo, hi) = span(b, a);
        0.5 * (lo + hi)
    };
    // Cuts at the byte quantiles of the probes along an axis, the bricks' bytes spread over their
    // span (a tall brick weighs along its height).
    let cuts_along = |a: usize, tiles: u64, terrain: Option<(f32, u64)>| {
        let mut weights: Vec<(f32, u64)> = Vec::new();
        for b in &layout.bricks {
            let n = u64::from(b.counts[a].max(1));
            let (lo, hi) = span(b, a);
            for k in 0..n {
                let t = (k as f32 + 0.5) / n as f32;
                weights.push((lo + t * (hi - lo), brick_bytes(b) / n));
            }
        }
        if let Some(t) = terrain {
            weights.push(t);
        }
        weight_cuts(&mut weights, tiles)
    };
    let tile_bounds = |cuts: &[f32], k: usize| {
        (
            if k == 0 { f32::NEG_INFINITY } else { cuts[k - 1] },
            cuts.get(k).copied().unwrap_or(f32::INFINITY),
        )
    };
    let terrain_mid = terrain_heights.map_or(0.0, |(lo, hi)| 0.5 * (lo + hi));
    let cuts_x = cuts_along(0, 2, None);
    let cuts_z = cuts_along(2, 2, None);
    let cuts_y = cuts_along(1, 4, Some((terrain_mid, terrain_bytes)));
    let (mut xz_whole, mut xz_cropped) = (0u64, 0u64);
    for i in 0..=cuts_x.len() {
        for j in 0..=cuts_z.len() {
            let (x0, x1) = tile_bounds(&cuts_x, i);
            let (z0, z1) = tile_bounds(&cuts_z, j);
            let (mut whole_bytes, mut cropped) = (terrain_bytes / 4, terrain_bytes / 4);
            for b in &layout.bricks {
                let (bx0, bx1) = span(b, 0);
                let (bz0, bz1) = span(b, 2);
                let cx = centre(b, 0);
                let cz = centre(b, 2);
                let reaches = bx1 >= x0 && bx0 <= x1 && bz1 >= z0 && bz0 <= z1;
                let owned = cx >= x0 && cx < x1 && cz >= z0 && cz < z1;
                if reaches || owned {
                    whole_bytes += brick_bytes(b);
                }
                if reaches {
                    let nx = cropped_count(b.origin[0], b.spacing[0], b.counts[0], x0, x1, 2.0);
                    let nz = cropped_count(b.origin[2], b.spacing[2], b.counts[2], z0, z1, 2.0);
                    cropped += nx * u64::from(b.counts[1]) * nz * per_probe;
                }
            }
            xz_whole = xz_whole.max(whole_bytes);
            xz_cropped = xz_cropped.max(cropped);
        }
    }
    let (mut y_whole, mut y_cropped) = (0u64, 0u64);
    for k in 0..=cuts_y.len() {
        let (y0, y1) = tile_bounds(&cuts_y, k);
        let holds_terrain = terrain_heights.is_some_and(|(lo, hi)| hi >= y0 && lo <= y1);
        let terrain = if holds_terrain { terrain_bytes } else { 0 };
        let (mut whole_bytes, mut cropped) = (terrain, terrain);
        for b in &layout.bricks {
            let (b0, b1) = span(b, 1);
            if b1 >= y0 && b0 <= y1 {
                whole_bytes += brick_bytes(b);
                let ny = cropped_count(b.origin[1], b.spacing[1], b.counts[1], y0, y1, 2.0);
                cropped += u64::from(b.counts[0]) * ny * u64::from(b.counts[2]) * per_probe;
            }
        }
        y_whole = y_whole.max(whole_bytes);
        y_cropped = y_cropped.max(cropped);
    }
    let split = if whole > PORTABLE_BINDING_BYTES {
        regia_asset::baked_gi_split::split_layout(layout, PORTABLE_BINDING_BYTES).unwrap_or_default()
    } else {
        Vec::new()
    };
    let xz_parts = split.len();
    SplitAxisStudy {
        whole,
        largest_brick,
        xz_parts,
        xz_whole_bricks: xz_whole,
        y_whole_bricks: y_whole,
        xz_cropped_bricks: xz_cropped,
        y_cropped_bricks: y_cropped,
        split_parts: split.iter().map(|p| p.runtime_buffer_bytes()).collect(),
        split_cropped_bricks: split.iter().map(|p| p.split.cropped_bricks(&p.layout)).collect(),
    }
}

/// Estimated host bytes the bake holds at its peak (the filter: the main traces, the raw and
/// the filtered probes, the encoded sections, positions and flags), for `probes` probes. The
/// type sizes come from the code, so the estimate follows it; `bake_sizing bake` prints it
/// beside the measured peak resident set.
#[must_use]
pub fn bake_host_bytes_estimate(probes: usize, moment_resolution: u32) -> u64 {
    let texels = u64::from(moment_resolution) * u64::from(moment_resolution);
    let moments_heap = texels * 8;
    let trace = std::mem::size_of::<ProbeTrace>() as u64 + moments_heap;
    let data = std::mem::size_of::<ProbeData>() as u64 + moments_heap;
    let encoded = META_RECORD_BYTES as u64
        + (GI_COMPONENTS * SH_RECORD_BYTES) as u64
        + texels * MOMENT_TEXEL_BYTES as u64;
    // Nominal positions, half cells, offsets, final positions, traced positions (3 × f32 each),
    // the flags (u16) and the live index (usize).
    let small = 5 * 12 + 2 + 8;
    probes as u64 * (trace + 2 * data + encoded + small)
}

/// GPU buffer bytes of a probe trace of `job` over `live` probes.
///
/// # Errors
///
/// The scene does not pack (a malformed mesh or terrain).
pub fn bake_gpu_bytes(job: &GiBakeJob, live: usize) -> Result<BakeGpuBytes, PtError> {
    let packed = crate::pack::pack_with(&job.scene, None)?;
    let t = &job.settings.trace;
    let rays = u64::from(t.rays_per_probe.clamp(1, 4096));
    let batch = u64::from(t.probes_per_batch.max(1)).min(live.max(1) as u64);
    let texels = u64::from(job.layout.moment_resolution) * u64::from(job.layout.moment_resolution);
    let slots = batch * rays;
    let sizes = [
        std::mem::size_of_val(packed.nodes.as_slice()) as u64,
        std::mem::size_of_val(packed.tri_pos.as_slice()) as u64,
        std::mem::size_of_val(packed.tri_attr.as_slice()) as u64,
        std::mem::size_of_val(packed.instances.as_slice()) as u64,
        std::mem::size_of_val(packed.materials.as_slice()) as u64,
        std::mem::size_of_val(packed.textures.as_slice()) as u64,
        std::mem::size_of_val(packed.lights.as_slice()) as u64,
        // The probe records and the coherent order table (ADR-862).
        std::mem::size_of_val(packed.env.as_slice()) as u64
            + batch * 16
            + if t.coherent_rays && rays > 1 { rays * 4 } else { 0 },
        std::mem::size_of_val(packed.terrain.as_slice()) as u64,
    ];
    let scene: u64 = sizes.iter().sum();
    let paths = slots * PATH_VEC4S * 16;
    let hits = slots * HIT_BYTES;
    let accum = batch * (PROBE_HEADER_VEC4S + texels) * 16;
    // Two queues of path slots when the bounces are compacted (ADR-862).
    let queues = if t.compact_paths && t.max_bounces > 1 {
        slots * 8
    } else {
        0
    };
    let largest_binding = sizes
        .iter()
        .copied()
        .chain([paths, hits, accum, queues])
        .max()
        .unwrap_or(0);
    Ok(BakeGpuBytes {
        batch_probes: batch,
        path_slots: slots,
        scene,
        paths,
        hits,
        accum,
        largest_binding,
        total: scene + paths + hits + 2 * accum + queues,
    })
}

fn brick_label(source: Option<&BrickSource>, index: usize) -> String {
    match source {
        Some(BrickSource::Interior { id }) => format!("interior:{id}"),
        Some(BrickSource::Cluster { meshes, placements }) => {
            let first = meshes.first().map_or("?", String::as_str);
            if meshes.len() > 1 {
                format!("cluster:{first}+{} ({placements} placements)", meshes.len() - 1)
            } else {
                format!("cluster:{first} ({placements} placements)")
            }
        }
        Some(BrickSource::Explicit { index }) => format!("brick:{index}"),
        Some(BrickSource::Builtin) => "builtin".into(),
        None => format!("brick:{index}"),
    }
}

/// The lowest and highest terrain-layer probe of `job`, world Y.
fn ground_range(job: &GiBakeJob) -> Option<(f32, f32)> {
    let t = job.layout.terrain.as_ref()?;
    let (top, bottom) = (
        t.levels.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        t.levels.iter().copied().fold(f32::INFINITY, f32::min),
    );
    let finite = || t.ground.iter().copied().filter(|g| g.is_finite());
    let lo = finite().fold(f32::INFINITY, f32::min);
    let hi = finite().fold(f32::NEG_INFINITY, f32::max);
    (lo <= hi).then_some((lo + bottom, hi + top))
}

/// Measures `job` without tracing ([`SizingReport`]).
///
/// # Errors
///
/// The scene does not pack ([`bake_gpu_bytes`]).
pub fn size_job(job: &GiBakeJob) -> Result<SizingReport, PtError> {
    let _span = tracing::info_span!("pt_bake_sizing", probes = job.layout.probe_count()).entered();
    let layout = &job.layout;
    let probes = layout.probe_count();
    let terrain_probes = layout.terrain.as_ref().map_or(0, |t| t.probe_count());
    let mut by_kind: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut sizes: Vec<BrickSize> = Vec::with_capacity(layout.bricks.len());
    for (index, b) in layout.bricks.iter().enumerate() {
        let source = job.plan.bricks.get(index).map(|p| &p.source);
        let kind = match source {
            Some(BrickSource::Interior { .. }) => "interior",
            Some(BrickSource::Cluster { .. }) => "cluster",
            _ => "other",
        };
        let e = by_kind.entry(kind).or_default();
        e.0 += 1;
        e.1 += b.probe_count();
        sizes.push(BrickSize {
            label: brick_label(source, index),
            probes: b.probe_count(),
            spacing: b.spacing.iter().copied().fold(f32::INFINITY, f32::min),
            counts: b.counts,
        });
    }
    sizes.sort_by(|a, b| b.probes.cmp(&a.probes).then_with(|| a.label.cmp(&b.label)));
    sizes.truncate(5);
    let kind = |k: &str| by_kind.get(k).copied().unwrap_or_default();
    let under_terrain = match &job.scene.terrain {
        Some(hf) => layout
            .nominal_positions()
            .iter()
            .filter(|p| hf.height_at(p[0], p[2]).is_some_and(|h| p[1] < h))
            .count(),
        None => 0,
    };
    let live = probes - under_terrain;
    let t = &job.settings.trace;
    let rays = u64::from(t.rays_per_probe.clamp(1, 4096));
    let rays = BakeRays {
        live_probes: live as u64,
        main: live as u64 * rays * u64::from(t.samples.max(1)),
        relocation_max: u64::from(job.settings.relocation_iterations)
            * live as u64
            * rays
            * u64::from(job.settings.relocation_samples.max(1)),
    };
    let volume = volume_bytes(layout);
    let gpu = bake_gpu_bytes(job, live)?;
    Ok(SizingReport {
        probes,
        terrain_probes,
        interior_probes: kind("interior").1,
        cluster_probes: kind("cluster").1,
        other_brick_probes: kind("other").1,
        bricks: layout.bricks.len(),
        interior_bricks: kind("interior").0,
        interior_flagged: job.plan.bricks.iter().filter(|b| b.interior_flag).count(),
        cluster_bricks: kind("cluster").0,
        under_terrain,
        auto_scale: job.plan.auto_bricks.as_ref().map(|a| a.scale),
        placements: job.plan.meshes.placements,
        triangles: job.plan.meshes.triangles,
        largest_bricks: sizes,
        volume,
        portable_limit: PORTABLE_BINDING_BYTES,
        fits_portable: volume.runtime <= PORTABLE_BINDING_BYTES,
        rays,
        gpu,
        gpu_fits_portable: gpu.largest_binding <= PORTABLE_BINDING_BYTES,
        host_peak_estimate: bake_host_bytes_estimate(probes, layout.moment_resolution),
        memory: job_memory(job),
        split_study: split_axis_study(layout, ground_range(job)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{detail_brick, terrain_layer};
    use crate::scene::PtHeightfield;
    use regia_asset::baked_gi::{BakeInfo, BakeLights, BakedGiVolume, ProbeFlags, ProbeMeta};

    fn overlaps(a: &SyntheticInterior, b: &SyntheticInterior, gap: f32) -> bool {
        let (amin, amax) = a.footprint();
        let (bmin, bmax) = b.footprint();
        (0..2).all(|k| amin[k] - gap < bmax[k] && bmin[k] - gap < amax[k])
    }

    /// The Bore Twelve stand-in: 20 interiors with unique ids, one shaft open to the sky, no two
    /// footprints within 4 m of each other, all well inside the terrain and the backdrop ring;
    /// every tower slot is used and the rock and container counts are met.
    #[test]
    fn the_bore_twelve_stand_in_has_twenty_separate_interiors() {
        let level = SyntheticLevel::bore_twelve_scale();
        assert_eq!(level.interiors.len(), 20);
        assert_eq!(level.interiors.iter().filter(|i| i.sky_open).count(), 1);
        assert_eq!(level.interiors.iter().filter(|i| i.openings).count(), 2);
        let mut ids: Vec<&str> = level.interiors.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 20);
        assert_eq!(level.half_extent(), 640.0);
        for (k, a) in level.interiors.iter().enumerate() {
            let (min, max) = a.footprint();
            assert!(
                min[0] > -420.0 && max[0] < 420.0 && min[1] > -420.0 && max[1] < 420.0,
                "{}",
                a.id
            );
            for b in &level.interiors[k + 1..] {
                assert!(!overlaps(a, b, 4.0), "{} and {}", a.id, b.id);
            }
        }
        let p = level.placements();
        assert_eq!(p.towers.len(), 12);
        assert_eq!(p.containers.len(), 40);
        assert_eq!(p.rocks.len(), 600);
        // The shaft reaches 90 m below its pad and 10 m above it.
        let shaft = &level.interiors[0];
        let (lo, hi) = level.interior_box(shaft);
        assert_eq!(hi[1] - lo[1], 100.0);
        assert_eq!(hi[1] - level.pad_height(shaft), 10.0);
    }

    /// Pads are flat where the buildings stand, the shaft's cells are cut out of the terrain
    /// mesh, and every terrain triangle faces up.
    #[test]
    fn the_terrain_is_flat_under_buildings_and_open_over_the_shaft() {
        let level = SyntheticLevel::small();
        for i in &level.interiors {
            let pad = level.pad_height(i);
            let (min, max) = i.footprint();
            for (x, z) in [(min[0], min[1]), (max[0], max[1]), (i.at[0], i.at[1])] {
                assert_eq!(level.height_at(x, z), pad, "{}", i.id);
            }
        }
        let m = level.terrain_mesh();
        let n = level.grid as usize;
        assert_eq!(m.positions.len(), n * n);
        // The shaft's 11 m outer footprint holds the centres of 8 × 8 cells (-4.375..=4.375 m).
        let shaft_cells = 8 * 8;
        assert_eq!(m.indices.len() / 6, (n - 1) * (n - 1) - shaft_cells);
        for tri in m.indices.as_chunks::<3>().0 {
            let [a, b, c] = tri.map(|i| regia_core::glam::Vec3::from_array(m.positions[i as usize]));
            assert!((b - a).cross(c - a).y > 0.0);
        }
    }

    /// Shells are closed: every edge is shared by exactly two triangles, also for the open
    /// shaft with its rim; inner faces point in and outer faces out.
    #[test]
    fn shells_are_watertight_and_face_the_right_way() {
        for open in [false, true] {
            let m = shell_mesh([6.0, 3.0, 4.0], open);
            let key = |p: [f32; 3]| p.map(f32::to_bits);
            let mut edges: BTreeMap<([u32; 3], [u32; 3]), i32> = BTreeMap::new();
            for tri in m.indices.as_chunks::<3>().0 {
                for k in 0..3 {
                    let (a, b) = (
                        key(m.positions[tri[k] as usize]),
                        key(m.positions[tri[(k + 1) % 3] as usize]),
                    );
                    *edges.entry(if a < b { (a, b) } else { (b, a) }).or_default() += 1;
                }
            }
            assert!(
                edges.values().all(|&c| c == 2),
                "open {open}: an edge not joining two triangles"
            );
            // The inner floor faces up; the outer floor faces down.
            let floor_up = m
                .normals
                .iter()
                .zip(&m.positions)
                .any(|(nrm, p)| p[1] == 0.0 && nrm[1] > 0.5);
            let base_down = m
                .normals
                .iter()
                .zip(&m.positions)
                .any(|(nrm, p)| p[1] == -WALL && nrm[1] < -0.5);
            assert!(floor_up && base_down);
        }
        assert_eq!(shell_mesh([6.0, 3.0, 4.0], false).triangle_count(), 24);
        assert_eq!(shell_mesh([6.0, 3.0, 4.0], true).triangle_count(), 28);
    }

    /// The description is the same text every time, parses as v1 and builds through the real
    /// description path: every interior known, every placement placed, the shaft skipped as a
    /// mesh of the terrain.
    #[test]
    fn the_description_is_deterministic_and_builds() {
        let level = SyntheticLevel::small();
        let layout = SizingLayout::doc_example();
        let text = level.bake_ron(&layout);
        assert_eq!(text, level.bake_ron(&layout));
        let (desc, version) = GiBakeDescription::parse_versioned(&text).expect("parses");
        assert_eq!(version, 1);
        assert_eq!(desc.interiors.len(), 4);
        let job = level.job(&layout).expect("builds");
        let p = level.placements();
        let expected = level.interiors.len() + p.towers.len() + p.containers.len() + p.rocks.len() + 1;
        assert_eq!(job.plan.meshes.placements, expected);
        assert!(job.plan.terrain_from_mesh);
        assert_eq!(job.plan.interiors, 4);
        let hf = job.scene.terrain.as_ref().expect("terrain");
        assert_eq!((hf.width, hf.depth), (level.grid, level.grid));
        // The shaft's hole is cut: no height at its centre, a height beside the shaft.
        assert!(hf.height_at(0.0, 0.0).is_none());
        assert!(hf.height_at(20.0, 0.0).is_some());
        assert_eq!(
            level
                .loader()
                .load(Path::new("synthetic/nope.mesh"))
                .expect_err("unknown"),
            "synthetic/nope.mesh: not a synthetic source"
        );
    }

    /// Interior bricks are `detail_brick`s over the interior boxes at the asked spacing (the
    /// shaft at its own), the plan flags every closed interior and not the shaft, and the
    /// terrain layer has `floor(extent / spacing)²` columns per level.
    #[test]
    fn interior_bricks_and_the_terrain_layer_have_the_expected_counts() {
        let level = SyntheticLevel::small();
        let layout = SizingLayout {
            interior_spacing: Some(1.0),
            shaft_spacing: Some(2.0),
            ..SizingLayout::doc_example()
        };
        let job = level.job(&layout).expect("builds");
        let report = size_job(&job).expect("sizes");
        assert_eq!(report.auto_scale, Some(1.0));
        assert_eq!(report.terrain_probes, 20 * 20 * 5);
        assert_eq!(report.interior_bricks, 4);
        let mut interior_probes = 0;
        for i in &level.interiors {
            let (min, max) = level.interior_box(i);
            let spacing = if i.sky_open { 2.0 } else { 1.0 };
            let want = detail_brick(min, max, spacing);
            let index = job
                .plan
                .bricks
                .iter()
                .position(|b| b.source == BrickSource::Interior { id: i.id.clone() })
                .expect("interior brick");
            assert_eq!(job.layout.bricks[index].counts, want.counts, "{}", i.id);
            assert_eq!(job.plan.bricks[index].interior_flag, !i.sky_open, "{}", i.id);
            interior_probes += want.probe_count();
        }
        assert_eq!(report.interior_probes, interior_probes);
        // Hand counts: hab 13·5·9, hangar 21·9·15, bunker 9·4·7, shaft at 2 m 6·11·6.
        assert_eq!(interior_probes, 585 + 2835 + 252 + 396);
        assert_eq!(
            report.probes,
            report.terrain_probes
                + report.interior_probes
                + report.cluster_probes
                + report.other_brick_probes
        );
        assert!(report.interior_flagged >= 3);
    }

    /// On a map with a backdrop ring, the defaults (bricks on every layer but `terrain`) put one
    /// brick over the whole ring: the budget coarsens every automatic brick, interiors
    /// included. Restricting the layers keeps the asked spacing.
    #[test]
    fn the_defaults_spend_the_budget_on_the_backdrop() {
        let level = SyntheticLevel::small();
        let defaults = size_job(&level.job(&SizingLayout::defaults()).expect("defaults")).expect("sizes");
        let scale = defaults.auto_scale.expect("auto bricks");
        assert!(scale > 1.5, "{scale}");
        assert!(
            defaults.largest_bricks[0].label.starts_with("cluster:backdrop"),
            "{:?}",
            defaults.largest_bricks
        );
        let doc = size_job(&level.job(&SizingLayout::doc_example()).expect("doc")).expect("sizes");
        assert_eq!(doc.auto_scale, Some(1.0));
        assert!(doc.largest_bricks.iter().all(|b| !b.label.contains("backdrop")));
        // Both keep every interior covered by its own brick.
        assert_eq!((defaults.interior_bricks, doc.interior_bricks), (4, 4));
    }

    /// The volume bytes are those of an encoded volume: the payload is exactly the sections, the
    /// file fits the bound, and the runtime buffer is the sections, the ground and 48 bytes per
    /// brick.
    #[test]
    fn volume_bytes_match_an_encoded_volume() {
        let hf = PtHeightfield {
            width: 17,
            depth: 13,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights: (0..17 * 13).map(|k| (k % 5) as f32 * 0.25).collect(),
            material: 0,
        };
        for res in [4u32, 8] {
            let mut layout = VolumeLayout {
                terrain: Some(terrain_layer(&hf, [2.0, 2.0], &[1.0, 3.0, 9.0]).expect("layer")),
                bricks: vec![
                    detail_brick([0.0; 3], [3.0, 2.0, 1.0], 1.0),
                    detail_brick([5.0, 0.0, 5.0], [7.5, 1.0, 6.0], 0.5),
                ],
                moment_resolution: res,
                max_distance: 8.0,
            };
            layout.assign_brick_offsets();
            let n = layout.probe_count();
            let probe = ProbeData {
                sh: [[[0.5, 0.1, -0.1, 0.2]; 3]; GI_COMPONENTS],
                moments: vec![[2.0, 4.5]; (res * res) as usize],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            };
            let volume = BakedGiVolume::from_probes(
                &layout,
                BakeLights::default(),
                BakeInfo::default(),
                &vec![probe; n],
            )
            .expect("volume");
            let file = volume.to_bytes().expect("bytes");
            let b = volume_bytes(&layout);
            let sections = [
                volume.meta_bytes().len(),
                volume.sh_bytes().len(),
                volume.moment_bytes().len(),
                layout.terrain.as_ref().map_or(0, |t| t.ground.len() * 4),
            ];
            assert_eq!(
                b.per_probe * n as u64,
                (sections[0] + sections[1] + sections[2]) as u64
            );
            assert_eq!(
                b.payload,
                sections
                    .iter()
                    .map(|s| (*s as u64).next_multiple_of(16))
                    .sum::<u64>()
            );
            assert!(
                file.len() as u64 > b.payload && file.len() as u64 <= b.file_max,
                "{} {b:?}",
                file.len()
            );
            assert_eq!(
                b.runtime,
                sections.iter().map(|s| *s as u64).sum::<u64>() + 48 * layout.bricks.len() as u64
            );
            assert_eq!(b.per_probe, if res == 4 { 136 } else { 328 });
        }
    }

    /// The bake's GPU buffers follow the batch and ray settings: the defaults' 4096 probes ×
    /// 256 rays need a 192 MiB path buffer (over a 128 MiB binding), 512 probes 24 MiB. Rays
    /// count live probes only.
    #[test]
    fn the_gpu_buffers_and_rays_follow_the_settings() {
        let level = SyntheticLevel::small();
        let mut layout = SizingLayout::doc_example();
        let job = level.job(&layout).expect("builds");
        let r = size_job(&job).expect("sizes");
        assert!(r.rays.live_probes > 4096);
        assert_eq!(r.gpu.batch_probes, 4096);
        assert_eq!(r.gpu.paths, 4096 * 256 * 192);
        assert_eq!(r.gpu.hits, 4096 * 256 * 32);
        assert_eq!(r.gpu.accum, 4096 * (15 + 16) * 16);
        assert_eq!(r.gpu.largest_binding, r.gpu.paths);
        assert!(!r.gpu_fits_portable);
        assert_eq!(r.rays.live_probes, (r.probes - r.under_terrain) as u64);
        assert_eq!(r.rays.main, r.rays.live_probes * 256 * 16);
        assert_eq!(r.rays.relocation_max, 2 * r.rays.live_probes * 256 * 2);
        assert!(r.gpu.scene > 0);
        layout.probes_per_batch = 512;
        let r = size_job(&level.job(&layout).expect("builds")).expect("sizes");
        assert_eq!(r.gpu.paths, 24 << 20);
        assert!(r.gpu_fits_portable);
        // The host estimate grows with the moments and the probes.
        assert!(bake_host_bytes_estimate(1000, 8) > bake_host_bytes_estimate(1000, 4));
        assert_eq!(
            bake_host_bytes_estimate(2000, 4),
            2 * bake_host_bytes_estimate(1000, 4)
        );
    }

    /// Every sweep layout builds on the small level and has a unique name; every preset name
    /// resolves.
    #[test]
    fn every_sweep_layout_builds() {
        let level = SyntheticLevel::small();
        let sweep = sweep_layouts();
        let mut names: Vec<&str> = sweep.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), sweep.len());
        for (name, layout) in &sweep {
            let job = level.job(layout).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(size_job(&job).is_ok(), "{name}");
        }
        for name in SIZING_LAYOUTS {
            assert!(SizingLayout::preset(name).is_some());
        }
        for name in SYNTHETIC_LEVELS {
            assert!(SyntheticLevel::preset(name).is_some());
        }
        assert!(SizingLayout::preset("nope").is_none() && SyntheticLevel::preset("nope").is_none());
    }

    /// The full-size stand-in with the recommended layout, against the numbers of
    /// `docs/research/bake-sizing-bore-twelve.md`: 128 000 terrain probes (160 × 160 columns × 5
    /// levels), 175 385 probes in the 20 interior bricks (exact: the boxes lie on a quarter-metre
    /// grid), about 394 000 probes in all with no coarsening, a volume that installs on every
    /// device and a path buffer (2048 probes × 256 rays) that fits a 128 MiB binding. The
    /// defaults on the same map merge every placement into the backdrop's brick and coarsen the
    /// interiors more than fivefold.
    /// The tall map at half a metre (the shaft at one): the sizes of ADR-3006's first case.
    /// `max_probes` is the study's, not a recommendation.
    fn dense_layout() -> SizingLayout {
        SizingLayout {
            interior_spacing: Some(0.5),
            shaft_spacing: Some(1.0),
            max_probes: 1 << 26,
            ..SizingLayout::recommended()
        }
    }

    /// At the recommended spacing the tall map needs no split, whatever the axis: ADR-3006's numbers.
    #[test]
    fn the_tall_map_needs_no_split_at_the_recommended_spacing() {
        let level = SyntheticLevel::tall();
        let job = level
            .job(&SizingLayout::recommended())
            .expect("recommended builds");
        let study = split_axis_study(&job.layout, ground_range(&job));
        println!("tall recommended: {study:?}");
        assert!(study.whole <= PORTABLE_BINDING_BYTES, "{study:?}");
        assert_eq!(study.xz_parts, 0, "no split is needed");
        let report = job_memory(&job);
        assert!(report.tiers.iter().all(|t| t.fits), "{report:?}");
    }

    /// At half a metre the description refuses the map: one tall brick is past the binding, and a
    /// cut in any axis that keeps bricks whole leaves it there. Cropping bricks is what helps.
    #[test]
    fn the_dense_tall_map_is_past_any_cut_that_keeps_bricks_whole() {
        let level = SyntheticLevel::tall();
        let Err(err) = level.job(&dense_layout()) else {
            panic!("one tall brick is past the binding");
        };
        assert_eq!(err.code, "bake_volume_too_large", "{err:?}");
        let layout = tall_stack_layout(&level, 0.5, 1.0);
        let study = split_axis_study(&layout, Some((0.0, 20.0)));
        println!("tall dense: {study:?}");
        assert!(study.largest_brick > PORTABLE_BINDING_BYTES, "{study:?}");
        assert_eq!(study.xz_parts, 0, "no split of whole bricks fits: {study:?}");
        assert!(study.xz_whole_bricks > PORTABLE_BINDING_BYTES, "{study:?}");
        assert!(study.y_whole_bricks > PORTABLE_BINDING_BYTES, "{study:?}");
        assert!(
            study.xz_cropped_bricks < study.xz_whole_bricks,
            "cropping is what helps: {study:?}"
        );
    }

    /// ADR-3237 acceptance: the whole tall map with every interior at half a metre (the shaft at the
    /// recommended 2 m) bakes. Its tower is one brick past the binding, which failed every split
    /// before ADR-3237 (`bake_volume_too_large`); now the description plans four parts under the
    /// binding with the tower cropped, and the memory answer and the split study report those parts.
    #[test]
    fn the_dense_tall_map_bakes_as_a_cropped_split() {
        let level = SyntheticLevel::tall();
        let layout = SizingLayout {
            shaft_spacing: Some(2.0),
            ..dense_layout()
        };
        let job = level
            .job(&layout)
            .expect("the tall map at half a metre bakes as a cropped split");
        let per_probe = volume_bytes(&job.layout).per_probe;
        assert!(
            job.layout
                .bricks
                .iter()
                .any(|b| b.probe_count() as u64 * per_probe > PORTABLE_BINDING_BYTES),
            "a brick is past one binding"
        );
        let parts = &job.plan.split_parts;
        println!(
            "tall dense, 2 m shaft: whole {} parts {parts:?} cropped {:?}",
            job.plan.volume_bytes, job.plan.split_cropped_bricks
        );
        assert!(
            job.plan.volume_bytes > 3 * PORTABLE_BINDING_BYTES,
            "{}",
            job.plan.volume_bytes
        );
        assert_eq!(parts.len(), 4, "{parts:?}");
        assert!(parts.iter().all(|b| *b <= PORTABLE_BINDING_BYTES), "{parts:?}");
        assert_eq!(job.plan.split_cropped_bricks.len(), 4);
        assert!(job.plan.split_cropped_bricks.iter().all(|c| *c > 0));
        // The memory answer before a bake and the sizing study report the cropped parts.
        let report = job_memory(&job);
        assert_eq!(&report.parts, parts);
        let study = split_axis_study(&job.layout, ground_range(&job));
        assert_eq!(&study.split_parts, parts);
        assert_eq!(study.split_cropped_bricks, job.plan.split_cropped_bricks);
        assert!(
            report
                .tiers
                .iter()
                .all(|t| t.over.iter().all(|o| o.code != memory::CODE_OVER_BINDING)),
            "{report:?}"
        );
    }

    /// The shaft at one metre (ADR-3006's row) is past four bindings in total (525.9 MiB against
    /// 512 MiB), so no split, cropped or not, fits it: the error is still `bake_volume_too_large`,
    /// and its message names the best split it found.
    #[test]
    fn a_tall_map_past_four_bindings_still_fails_with_the_best_split() {
        let level = SyntheticLevel::tall();
        let Err(err) = level.job(&dense_layout()) else {
            panic!("525.9 MiB is past four parts of 128 MiB");
        };
        assert_eq!(err.code, "bake_volume_too_large", "{err:?}");
        assert!(err.message.contains("the best split's largest part is"), "{err}");
    }

    /// A layout of the interiors' boxes as plain bricks at `spacing` (`shaft` for the sky-open one),
    /// plus an 8 m terrain layer: the description's bricks without its size check.
    fn tall_stack_layout(level: &SyntheticLevel, spacing: f32, shaft: f32) -> VolumeLayout {
        let half = level.half_extent();
        let columns = (2.0 * half / 8.0) as u32 + 1;
        let mut layout = VolumeLayout {
            terrain: Some(regia_asset::baked_gi::TerrainLayer {
                origin: [-half, -half],
                spacing: [8.0, 8.0],
                counts: [columns, columns],
                levels: vec![1.0, 2.0, 4.0, 8.0, 16.0],
                ground: vec![0.0; (columns * columns) as usize],
            }),
            bricks: level
                .interiors
                .iter()
                .map(|i| {
                    let (min, max) = level.interior_box(i);
                    let s = if i.sky_open { shaft } else { spacing };
                    let counts: [u32; 3] = std::array::from_fn(|a| ((max[a] - min[a]) / s).ceil() as u32 + 1);
                    regia_asset::baked_gi::DetailBrick {
                        origin: min,
                        spacing: [s; 3],
                        counts,
                        first_probe: 0,
                        flags: 0,
                    }
                })
                .collect(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        layout
    }

    /// The stack of separate floors alone: X and Z cannot separate them with whole bricks, a Y cut
    /// with whole bricks can, and cropping in X and Z does as well as that. So Y is not what a stack
    /// needs, and since ADR-3237 the bake crops them.
    #[test]
    fn a_stack_of_floors_splits_as_well_in_x_and_z_once_bricks_may_be_cut() {
        let mut level = SyntheticLevel::tall();
        level.interiors.retain(|i| i.class == "floor");
        // Twelve 40 × 6 × 40 floors at half a metre: each is 81 × 13 × 81 probes.
        let layout = tall_stack_layout(&level, 0.5, 0.5);
        let study = split_axis_study(&layout, Some((0.0, 20.0)));
        println!("floor stack: {study:?}");
        assert!(study.whole > PORTABLE_BINDING_BYTES, "{study:?}");
        assert!(study.largest_brick <= PORTABLE_BINDING_BYTES, "{study:?}");
        // ADR-3237: the bake now splits it, cropping the floors at the cut.
        assert!(study.xz_parts >= 2, "the bake splits the stack: {study:?}");
        assert!(
            study.split_parts.iter().all(|b| *b <= PORTABLE_BINDING_BYTES),
            "{study:?}"
        );
        assert!(study.split_cropped_bricks.iter().any(|c| *c > 0), "{study:?}");
        assert!(
            study.xz_whole_bricks >= study.whole - study.whole / 8,
            "{study:?}"
        );
        assert!(study.y_whole_bricks <= PORTABLE_BINDING_BYTES, "{study:?}");
        assert!(study.xz_cropped_bricks <= PORTABLE_BINDING_BYTES, "{study:?}");
    }

    /// The Bore Twelve stand-in with a 1.6 m terrain layer: within four parts of one binding, within
    /// the minimum tier's share and past the Deck's, which names the terrain as the fix. A real
    /// description, through the same path as `regia bake-gi --dry-run`.
    #[test]
    fn a_map_over_the_decks_share_is_told_what_to_coarsen() {
        let layout = SizingLayout {
            terrain_spacing: Some(1.6),
            ..SizingLayout::recommended()
        };
        let job = SyntheticLevel::bore_twelve_scale()
            .job(&layout)
            .expect("the map builds");
        let report = job_memory(&job);
        println!(
            "bore12 at 1.6 m terrain: {} parts, {} bytes",
            report.parts.len(),
            report.bytes
        );
        assert!(report.parts.len() >= 2, "{:?}", report.parts);
        assert_eq!(
            report.parts, job.plan.split_parts,
            "the parts the bake would write"
        );
        let deck = report.tier("deck").expect("deck");
        assert!(!deck.fits, "{deck:?}");
        assert_eq!(deck.over.len(), 1, "{deck:?}");
        assert_eq!(deck.over[0].code, memory::CODE_OVER_BUDGET);
        assert_eq!(deck.fixes[0].fix, "spacing");
        assert_eq!(deck.fixes[0].target, "terrain");
        assert!(deck.fixes_suffice, "{deck:?}");
        assert!(report.tier("minimum").is_some_and(|t| t.fits), "{report:?}");
        assert!(report.tier("desktop_high").is_some_and(|t| t.fits), "{report:?}");
    }

    /// `job_memory` is the budget answer with the plan's own names for its bricks.
    #[test]
    fn the_memory_answer_names_the_plans_bricks() {
        let level = SyntheticLevel::tall();
        let job = level.job(&SizingLayout::recommended()).expect("builds");
        let report = job_memory(&job);
        assert_eq!(report.bytes, job.layout.runtime_buffer_bytes());
        assert_eq!(report.parts, [job.layout.runtime_buffer_bytes()]);
        assert!(
            report
                .contributors
                .iter()
                .any(|c| c.target.starts_with("interior:")),
            "{:?}",
            report.contributors
        );
        let sized = size_job(&job).expect("sizes");
        assert_eq!(sized.memory, report);
    }

    #[test]
    fn the_recommended_layout_fits_the_bore_twelve_stand_in() {
        let level = SyntheticLevel::bore_twelve_scale();
        let job = level.job(&SizingLayout::recommended()).expect("builds");
        let r = size_job(&job).expect("sizes");
        assert_eq!(r.terrain_probes, 160 * 160 * 5);
        assert_eq!(r.interior_bricks, 20);
        assert_eq!(r.interior_probes, 175_385);
        assert_eq!(r.interior_flagged, 19);
        assert_eq!(r.auto_scale, Some(1.0));
        assert!(r.probes.abs_diff(393_985) < 4000, "{}", r.probes);
        assert_eq!(r.volume.per_probe, 136);
        assert!(r.fits_portable, "{:?}", r.volume);
        assert_eq!(r.gpu.paths, 96 << 20);
        assert!(r.gpu_fits_portable, "{:?}", r.gpu);
        assert_eq!(r.placements, 20 + 12 + 40 + 600 + 1);

        let defaults = size_job(&level.job(&SizingLayout::defaults()).expect("defaults")).expect("sizes");
        assert!(
            defaults.auto_scale.is_some_and(|s| s > 5.0),
            "{:?}",
            defaults.auto_scale
        );
        assert_eq!(defaults.cluster_bricks, 1);
        assert!(defaults.largest_bricks[0].label.starts_with("cluster:backdrop"));
    }
}
