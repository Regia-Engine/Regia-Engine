// SPDX-License-Identifier: MPL-2.0
//! Unit-budget bench (batch v020, DO27's frozen per-class triangle table; `--ignored`, prints numbers).
//!
//! DO27 froze LOD0 triangles per unit class and the LOD1/LOD2/LOD3 ratios (light skimmer 150k,
//! 25 % / 6 % but under 15k / 1.5 % but under 4k; battle, heavy and walker 300k, 20 % / 5 % /
//! 1.2 %; structure exterior 150k; walk-in interior 100k; cockpit 160k at LOD0 only), with three
//! materials per hull (body, glass as glTF `alphaMode: BLEND`, emissive lights) and 2K BC7/BC5
//! maps shared per class. This file measures what that table costs on a real GPU, without DO27
//! content:
//!
//! - Hulls are procedural: closed, displaced ellipsoid pieces (exact triangle counts, checked
//!   within 2 %), written as a glTF and cooked through `regia_asset_pipeline::import_gltf_path`
//!   with `.meta` `generate_lods`, so every LOD chain is the engine's own meshopt simplification
//!   and screen-size thresholds (`mesh_cook.rs`). The achieved ratios are printed next to the
//!   table's.
//! - Frames run the runtime path: an ECS world with `MeshRenderer` + `MeshLodChain` entities,
//!   `extract_frame`, `select_mesh_lods`, `extract_glass`, then `render_extracted_into` with the
//!   Desktop High profile. The camera sways a few centimetres per frame so no held-view cache
//!   skips a pass, and unit hulls hover and turn slightly every frame so their shadows are
//!   redrawn as in a match (structures stand still; `UNIT_BUDGET_STATIC=1` holds every hull).
//! - Scenes: N = 1, 5, 10, 20, 40 hulls of one class; (a) every hull forced to LOD0
//!   (`DisableMeshLod`) and (b) the engine's selection on a by-distance layout built from the
//!   cooked thresholds (nearest 2 in the LOD0 band, about a fifth in LOD1, the rest LOD2/LOD3),
//!   and the same places with DO27's intended mix pinned (each hull draws the cooked level of
//!   its band, whatever the thresholds select: the thresholds come from the simplification
//!   error, so they depend on the content);
//!   (c) for the walker: rigid through the visibility buffer, and skinned (vertex and compute
//!   skinning) through the forward path; (d) with the glass part in its BLEND material and with
//!   the same triangles in the opaque body material; (e) the seated cockpit (interior 160k,
//!   viewer-inside canopy glass, own exterior hidden on a camera-masked layer, near plane
//!   0.05 m), with 0 and 10 other skimmers outside.
//! - Per case: median GPU ms per frame from the frame's timestamp nodes ([`GpuTimings`]), the
//!   top nodes, visible (submitted, LOD-selected) triangles, the LOD histogram, mesh draws
//!   ([`regia_render::DrawStats`]), VRAM from the allocator report, and CPU ms. Then a
//!   least-squares fit of GPU ms against visible triangles per path and resolution (cost per
//!   100k visible triangles).
//!
//! Tier LOD scales (v0.1.20, ADR-260, lane v0119d): the layouts above pin
//! `mesh_lod_distance_scale` 1 and `mesh_lod_shadow_scale` 0.5 (the selection before the tier
//! scales), so their rows compare with v020t's. Two more layouts at the same places:
//! (f) `close20`, colour LOD 0 held to 20 m (the distance scale that puts the body's LOD0→1
//! colour switch at 20 m at the case's resolution) with shadows at the table mix (the shadow
//! scale 1 / distance scale, so shadows select what the scale-1 colour selects at those places);
//! (g) `ultra`, Desktop Ultra's tier scales as shipped. Before the cases, the bench prints each
//! class's body switch distances (colour and shadow, LOD0→1→2→3) for every preset at every
//! resolution.
//!
//! `unit_budget_depth_precision` renders an unlit panel a small gap in front of a wall at 0.5 m
//! (and further) with the near plane at 0.05 m and counts the wall pixels that win inside the
//! panel (z-fighting).
//!
//! The non-ignored smoke test runs the same path on lavapipe: 5 hulls of 10k triangles, cooked
//! LOD chain, by-distance layout, and asserts the frame is not black.
//!
//! ```text
//! cargo test -p regia_render --test unit_budget --release -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Environment knobs for the bench: `UNIT_BUDGET_CLASSES` (comma list of `skimmer`, `battle`,
//! `walker`, `structure`, `interior`, `cockpit`), `UNIT_BUDGET_RES` (comma list of `720`, `1080`,
//! `1440`), `UNIT_BUDGET_N` (comma list of hull counts), `UNIT_BUDGET_WARM` and
//! `UNIT_BUDGET_FRAMES` (frames per case), `UNIT_BUDGET_TEXTURE` (map size, default 2048),
//! `UNIT_BUDGET_OUT` (also write the markdown table to this path), `UNIT_BUDGET_LAYOUTS` (comma
//! list of `lod0`, `mix`, `table`, `close20`, `ultra`; default all).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use regia_app::prelude::*;
use regia_asset::{AssetId, CookedMesh, TextureCookUsage};
use regia_asset_pipeline::{
    Forward, MeshletAttributes, MeshletSettings, TextureCookQuality, build_meshlet_mesh_with_attributes,
    cook_texture_rgba8, import_gltf_path,
};
use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{
    ALPHA_BLEND, AntiAliasing, Camera, CompressedMaps, CompressedTexture, CpuMaterial, DirectionalLight,
    DirectionalLightUniform, DisableMeshLod, Environment, ExtractedFrame, FrameQuality, FrameRenderer,
    FrameTarget, GlassView, GpuTimings, JointMatrix, LabMaterial, LabMesh, LabScene, LabVertex, LodViewport,
    MESH_LOD_DISTANCE_SCALE_RANGE, MESH_LOD_SHADOW_SCALE_RANGE, MemoryCategory, MeshLodChain, MeshLodScales,
    MeshRenderer, MeshletBuilder, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    RenderSettings, SHADOW_LOD_MESH_TAG, SkinJoints, SsaoMode, extract_frame, extract_glass, memory_report,
    render_extracted_into, select_mesh_lods,
};
use regia_scene::{GlobalTransform, Transform};

type RenderMesh = regia_render::CpuMesh;

/// Vertical field of view of every bench camera (radians, about 57°).
const FOV_Y: f32 = 1.0;
/// Primitive order inside every cooked hull.
const BODY: usize = 0;
const GLASS: usize = 1;
const LIGHTS: usize = 2;
/// Layer the cockpit's own exterior draws on; the seated camera's mask leaves it out.
const OWN_EXTERIOR_LAYER: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Procedural hulls
// ---------------------------------------------------------------------------------------------

/// One closed ellipsoid piece of a part.
#[derive(Clone, Copy, Debug)]
struct Piece {
    center: [f32; 3],
    radii: [f32; 3],
    /// Share of the part's triangles.
    share: f32,
    /// Normals face the centre (a room or cockpit shell seen from inside).
    inward: bool,
    /// Relative radial displacement amplitude (panel detail the simplifier has to keep).
    detail: f32,
    /// Keep only the triangles above `center.y + cap × radii.y` (an open canopy cap).
    cap: Option<f32>,
}

impl Piece {
    const fn solid(center: [f32; 3], radii: [f32; 3], share: f32) -> Self {
        Self {
            center,
            radii,
            share,
            inward: false,
            detail: 0.004,
            cap: None,
        }
    }

    const fn smooth(center: [f32; 3], radii: [f32; 3], share: f32) -> Self {
        Self {
            detail: 0.0,
            ..Self::solid(center, radii, share)
        }
    }

    const fn inward(center: [f32; 3], radii: [f32; 3], share: f32) -> Self {
        Self {
            inward: true,
            ..Self::solid(center, radii, share)
        }
    }
}

/// The pieces of the three parts and the parts' triangle shares (body, glass, lights).
#[derive(Clone, Debug)]
struct Shape {
    parts: [Vec<Piece>; 3],
    shares: [f32; 3],
}

#[derive(Clone, Debug, Default)]
struct Part {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

impl Part {
    fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Panel detail on the unit sphere: a few incommensurate ridges (0 at the poles).
fn displacement(theta: f32, phi: f32) -> f32 {
    0.5 * (37.0 * phi).sin() * (23.0 * theta).sin()
        + 0.3 * (91.0 * phi + 1.3).sin() * (61.0 * theta).sin()
        + 0.2 * (173.0 * phi + 0.4).sin() * (149.0 * theta).sin()
}

/// A latitude/longitude ellipsoid of about `triangles` triangles, appended to `part`.
///
/// `k` latitude rings of `s` vertices plus two poles give `2·s·k` triangles; no seam column is
/// duplicated, so the piece is closed and has no border for the simplifier to lock.
fn ellipsoid(part: &mut Part, piece: &Piece, triangles: f32) {
    // A cap keeps the rings above the sill: generate enough for the kept fraction.
    let kept = piece
        .cap
        .map_or(1.0, |c| c.clamp(-1.0, 1.0).acos() / std::f32::consts::PI);
    let target = (triangles / kept.max(0.05)).max(8.0);
    let k = ((target / 4.0).sqrt().round() as u32).max(2);
    let s = ((target / (2.0 * k as f32)).round() as u32).max(3);
    let base = part.positions.len() as u32;
    let point = |theta: f32, phi: f32| -> [f32; 3] {
        let r = 1.0 + piece.detail * displacement(theta, phi);
        [
            piece.center[0] + piece.radii[0] * r * theta.sin() * phi.cos(),
            piece.center[1] + piece.radii[1] * r * theta.cos(),
            piece.center[2] + piece.radii[2] * r * theta.sin() * phi.sin(),
        ]
    };
    let mut positions = vec![point(0.0, 0.0)];
    let mut uvs = vec![[0.5, 0.0]];
    for i in 1..=k {
        let theta = std::f32::consts::PI * i as f32 / (k + 1) as f32;
        for j in 0..s {
            let phi = std::f32::consts::TAU * j as f32 / s as f32;
            positions.push(point(theta, phi));
            uvs.push([j as f32 / s as f32 * 4.0, i as f32 / (k + 1) as f32 * 2.0]);
        }
    }
    positions.push(point(std::f32::consts::PI, 0.0));
    uvs.push([0.5, 2.0]);
    let south = positions.len() as u32 - 1;
    let ring = |i: u32, j: u32| 1 + (i - 1) * s + (j % s);
    let mut tris: Vec<[u32; 3]> = Vec::with_capacity((2 * s * k) as usize);
    for j in 0..s {
        tris.push([0, ring(1, j), ring(1, j + 1)]);
        tris.push([south, ring(k, j + 1), ring(k, j)]);
    }
    for i in 1..k {
        for j in 0..s {
            let (a, b, c, d) = (ring(i, j), ring(i, j + 1), ring(i + 1, j), ring(i + 1, j + 1));
            tris.push([a, c, b]);
            tris.push([b, c, d]);
        }
    }
    let sill = piece.cap.map(|c| piece.center[1] + c * piece.radii[1]);
    let mut normals = vec![[0.0f32; 3]; positions.len()];
    let mut kept_tris = Vec::with_capacity(tris.len());
    for mut t in tris {
        let [a, b, c] = t.map(|i| positions[i as usize]);
        if let Some(sill) = sill
            && (a[1] + b[1] + c[1]) / 3.0 <= sill
        {
            continue;
        }
        let mut n = cross(sub(b, a), sub(c, a));
        let centroid = [
            (a[0] + b[0] + c[0]) / 3.0,
            (a[1] + b[1] + c[1]) / 3.0,
            (a[2] + b[2] + c[2]) / 3.0,
        ];
        let outward = dot(n, sub(centroid, piece.center)) >= 0.0;
        if outward == piece.inward {
            t.swap(1, 2);
            n = [-n[0], -n[1], -n[2]];
        }
        for i in t {
            let slot = &mut normals[i as usize];
            slot[0] += n[0];
            slot[1] += n[1];
            slot[2] += n[2];
        }
        kept_tris.push(t);
    }
    for n in &mut normals {
        let len = dot(*n, *n).sqrt();
        *n = if len > 0.0 {
            [n[0] / len, n[1] / len, n[2] / len]
        } else {
            [0.0, 1.0, 0.0]
        };
    }
    // A cap leaves the vertices below the sill unreferenced; the cook drops them.
    part.positions.extend(positions);
    part.normals.extend(normals);
    part.uvs.extend(uvs);
    for t in kept_tris {
        part.indices.extend(t.map(|i| base + i));
    }
}

/// The three parts of `shape` at `lod0` triangles in total.
fn build_parts(shape: &Shape, lod0: u32) -> [Part; 3] {
    let mut parts: [Part; 3] = Default::default();
    for (index, pieces) in shape.parts.iter().enumerate() {
        let budget = lod0 as f32 * shape.shares[index];
        let total: f32 = pieces.iter().map(|p| p.share).sum();
        for piece in pieces {
            ellipsoid(&mut parts[index], piece, budget * piece.share / total.max(1e-6));
        }
    }
    parts
}

// ---------------------------------------------------------------------------------------------
// Classes (DO27's frozen table)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ClassKey {
    Skimmer,
    Battle,
    Walker,
    Structure,
    Interior,
    Cockpit,
}

impl ClassKey {
    fn name(self) -> &'static str {
        match self {
            Self::Skimmer => "skimmer",
            Self::Battle => "battle",
            Self::Walker => "walker",
            Self::Structure => "structure",
            Self::Interior => "interior",
            Self::Cockpit => "cockpit",
        }
    }

    fn all() -> [Self; 6] {
        [
            Self::Skimmer,
            Self::Battle,
            Self::Walker,
            Self::Structure,
            Self::Interior,
            Self::Cockpit,
        ]
    }
}

#[derive(Clone, Debug)]
struct ClassSpec {
    key: ClassKey,
    lod0: u32,
    /// LOD ratios from LOD0 (empty: LOD0 only).
    ratios: Vec<f32>,
    /// Upper bounds on a level's triangles (`None`: none), applied to the ratio.
    caps: [Option<u32>; 4],
    shape: Shape,
}

impl ClassSpec {
    /// The ratios the cook is asked for: DO27's, lowered where a cap is tighter.
    fn cook_ratios(&self) -> Vec<f32> {
        self.ratios
            .iter()
            .enumerate()
            .map(|(i, r)| match self.caps[i] {
                Some(cap) => r.min(cap as f32 / self.lod0 as f32),
                None => *r,
            })
            .collect()
    }
}

/// Four small running lights around a hull of half-length `x` and half-width `z` at height `y`.
fn running_lights(x: f32, y: f32, z: f32) -> Vec<Piece> {
    [[x, y, z], [x, y, -z], [-x, y, z], [-x, y, -z]]
        .into_iter()
        .map(|c| Piece::smooth(c, [0.14, 0.07, 0.14], 1.0))
        .collect()
}

fn skimmer_shape(scale: f32, turret: bool) -> Shape {
    let s = |v: [f32; 3]| v.map(|x| x * scale);
    let mut body = vec![
        Piece::solid(s([0.0, 1.2, 0.0]), s([4.0, 0.8, 1.9]), 0.6),
        Piece::solid(s([-1.5, 1.0, 1.9]), s([1.1, 0.45, 1.1]), 0.15),
        Piece::solid(s([-1.5, 1.0, -1.9]), s([1.1, 0.45, 1.1]), 0.15),
        Piece::solid(s([-3.2, 1.9, 0.0]), s([0.8, 0.6, 0.08]), 0.1),
    ];
    if turret {
        body.push(Piece::solid(s([0.0, 2.2, 0.0]), s([1.2, 0.5, 1.2]), 0.12));
    }
    Shape {
        parts: [
            body,
            vec![Piece::smooth(s([1.4, 1.85, 0.0]), s([1.2, 0.45, 0.75]), 1.0)],
            running_lights(3.6 * scale, 1.1 * scale, 1.1 * scale),
        ],
        shares: [0.88, 0.08, 0.04],
    }
}

fn walker_shape() -> Shape {
    let mut body = vec![Piece::solid([0.0, 3.2, 0.0], [2.4, 1.0, 1.8], 0.55)];
    for x in [-1.6, 0.0, 1.6] {
        for z in [-1.9, 1.9] {
            body.push(Piece::solid([x, 1.6, z], [0.3, 1.6, 0.3], 0.075));
        }
    }
    Shape {
        parts: [
            body,
            vec![Piece::smooth([1.6, 3.9, 0.0], [0.9, 0.4, 0.7], 1.0)],
            running_lights(2.2, 3.0, 1.4),
        ],
        shares: [0.88, 0.08, 0.04],
    }
}

fn structure_shape() -> Shape {
    Shape {
        parts: [
            vec![
                Piece::solid([0.0, 0.0, 0.0], [4.5, 3.2, 4.5], 0.7),
                Piece::solid([3.5, 1.0, 3.0], [1.3, 1.0, 1.3], 0.1),
                Piece::solid([-3.5, 1.0, -3.0], [1.3, 1.0, 1.3], 0.1),
                Piece::solid([0.0, 4.5, 0.0], [0.15, 2.0, 0.15], 0.1),
            ],
            vec![Piece::smooth([0.0, 1.6, 0.0], [3.95, 0.35, 3.95], 1.0)],
            running_lights(3.0, 0.3, 3.0),
        ],
        shares: [0.88, 0.08, 0.04],
    }
}

fn interior_shape() -> Shape {
    let mut body = vec![Piece::inward([0.0, 2.0, 0.0], [8.0, 3.0, 8.0], 0.6)];
    for (x, z) in [(3.0, 3.0), (3.0, -3.0), (-3.0, 3.0), (-3.0, -3.0)] {
        body.push(Piece::solid([x, 2.0, z], [0.4, 2.2, 0.4], 0.05));
    }
    body.push(Piece::solid([0.0, 0.8, 4.0], [1.5, 0.6, 0.5], 0.1));
    body.push(Piece::solid([0.0, 0.8, -4.0], [1.5, 0.6, 0.5], 0.1));
    let lights = [(2.0, 2.0), (2.0, -2.0), (-2.0, 2.0), (-2.0, -2.0)]
        .into_iter()
        .map(|(x, z)| Piece::smooth([x, 4.6, z], [0.6, 0.05, 0.6], 1.0))
        .collect();
    Shape {
        parts: [
            body,
            vec![Piece::smooth([0.0, 2.0, -6.5], [1.8, 0.9, 0.05], 1.0)],
            lights,
        ],
        shares: [0.9, 0.05, 0.05],
    }
}

/// The cockpit eye, in cockpit space (the cockpit faces −Z).
const COCKPIT_EYE: [f32; 3] = [0.0, 1.2, 0.0];

fn cockpit_shape() -> Shape {
    let body = vec![
        // The tub below the eye, the dashboard 0.5 m ahead, side consoles, the canopy frame
        // arch and the seat back.
        Piece::solid([0.0, 0.55, 0.0], [0.75, 0.4, 1.1], 0.4),
        Piece::solid([0.0, 0.95, -0.62], [0.55, 0.18, 0.12], 0.2),
        Piece::solid([0.55, 0.95, -0.1], [0.12, 0.12, 0.4], 0.1),
        Piece::solid([-0.55, 0.95, -0.1], [0.12, 0.12, 0.4], 0.1),
        Piece::solid([0.0, 1.35, -0.85], [0.75, 0.05, 0.05], 0.1),
        Piece::solid([0.0, 1.1, 0.45], [0.35, 0.5, 0.12], 0.1),
    ];
    let canopy = Piece {
        cap: Some(-0.25),
        ..Piece::smooth(COCKPIT_EYE, [1.0, 0.9, 1.3], 1.0)
    };
    let lights = [-0.25, 0.0, 0.25]
        .into_iter()
        .map(|x| Piece::smooth([x, 1.12, -0.51], [0.1, 0.06, 0.015], 1.0))
        .collect();
    Shape {
        parts: [body, vec![canopy], lights],
        shares: [0.94, 0.035, 0.025],
    }
}

fn class_spec(key: ClassKey, lod0: u32) -> ClassSpec {
    let skimmer_ratios = vec![1.0, 0.25, 0.06, 0.015];
    let heavy_ratios = vec![1.0, 0.2, 0.05, 0.012];
    let none = [None; 4];
    match key {
        ClassKey::Skimmer => ClassSpec {
            key,
            lod0,
            ratios: skimmer_ratios,
            caps: [None, None, Some(15_000), Some(4_000)],
            shape: skimmer_shape(1.0, false),
        },
        ClassKey::Battle => ClassSpec {
            key,
            lod0,
            ratios: heavy_ratios,
            caps: none,
            shape: skimmer_shape(1.25, true),
        },
        ClassKey::Walker => ClassSpec {
            key,
            lod0,
            ratios: heavy_ratios,
            caps: none,
            shape: walker_shape(),
        },
        // The table gives no ratios for structures: the skimmer's (assumption, see the report).
        ClassKey::Structure => ClassSpec {
            key,
            lod0,
            ratios: skimmer_ratios,
            caps: none,
            shape: structure_shape(),
        },
        ClassKey::Interior => ClassSpec {
            key,
            lod0,
            ratios: Vec::new(),
            caps: none,
            shape: interior_shape(),
        },
        ClassKey::Cockpit => ClassSpec {
            key,
            lod0,
            ratios: Vec::new(),
            caps: none,
            shape: cockpit_shape(),
        },
    }
}

fn table_lod0(key: ClassKey) -> u32 {
    match key {
        ClassKey::Skimmer | ClassKey::Structure => 150_000,
        ClassKey::Battle | ClassKey::Walker => 300_000,
        ClassKey::Interior => 100_000,
        ClassKey::Cockpit => 160_000,
    }
}

// ---------------------------------------------------------------------------------------------
// Cooking: glTF → `import_gltf_path` → `CookedMesh` + `MeshLodChain`
// ---------------------------------------------------------------------------------------------

/// The three primitives with joint weights (LOD0 vertices) and the leg joints' hips.
type SkinnedHull = (Vec<Arc<RenderMesh>>, Vec<[f32; 3]>);

/// One cooked primitive.
struct Prim {
    mesh: Arc<RenderMesh>,
    chain: Option<MeshLodChain>,
    /// Triangles per LOD level.
    lod_tris: Vec<usize>,
    /// Bounding-sphere radius of the cooked mesh.
    radius: f32,
}

/// A cooked class: its three primitives, materials and the skinned body variant (walker).
struct ClassAssets {
    spec: ClassSpec,
    prims: Vec<Prim>,
    /// Generated LOD0 triangles (before the cook), per part.
    generated: [usize; 3],
    body: Arc<CpuMaterial>,
    glass: Arc<CpuMaterial>,
    lights: Arc<CpuMaterial>,
    /// The three primitives with joint weights (LOD0 vertices), and the palette's hips.
    skinned: Option<SkinnedHull>,
    /// Per primitive, one mesh per cooked level (the `Table` layout pins these).
    level_meshes: Vec<Vec<Arc<RenderMesh>>>,
    cook_s: f32,
    texture_s: f32,
}

fn gltf_json_and_bin(parts: &[Part; 3]) -> (String, Vec<u8>) {
    let mut bin: Vec<u8> = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut primitives = Vec::new();
    let mut view = |bin: &mut Vec<u8>, bytes: &[u8]| -> usize {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        views.push(format!(
            r#"{{"buffer":0,"byteOffset":{},"byteLength":{}}}"#,
            bin.len(),
            bytes.len()
        ));
        bin.extend_from_slice(bytes);
        views.len() - 1
    };
    for (material, part) in parts.iter().enumerate() {
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &part.positions {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let count = part.positions.len();
        let pv = view(&mut bin, bytemuck::cast_slice(&part.positions));
        accessors.push(format!(
            r#"{{"bufferView":{pv},"componentType":5126,"count":{count},"type":"VEC3","min":[{},{},{}],"max":[{},{},{}]}}"#,
            lo[0], lo[1], lo[2], hi[0], hi[1], hi[2]
        ));
        let pa = accessors.len() - 1;
        let nv = view(&mut bin, bytemuck::cast_slice(&part.normals));
        accessors.push(format!(
            r#"{{"bufferView":{nv},"componentType":5126,"count":{count},"type":"VEC3"}}"#
        ));
        let na = accessors.len() - 1;
        let uv = view(&mut bin, bytemuck::cast_slice(&part.uvs));
        accessors.push(format!(
            r#"{{"bufferView":{uv},"componentType":5126,"count":{count},"type":"VEC2"}}"#
        ));
        let ua = accessors.len() - 1;
        let iv = view(&mut bin, bytemuck::cast_slice(&part.indices));
        accessors.push(format!(
            r#"{{"bufferView":{iv},"componentType":5125,"count":{},"type":"SCALAR"}}"#,
            part.indices.len()
        ));
        let ia = accessors.len() - 1;
        primitives.push(format!(
            r#"{{"attributes":{{"POSITION":{pa},"NORMAL":{na},"TEXCOORD_0":{ua}}},"indices":{ia},"material":{material}}}"#
        ));
    }
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
        "nodes":[{{"name":"Hull","mesh":0}}],
        "meshes":[{{"name":"Hull","primitives":[{}]}}],
        "materials":[
          {{"name":"body","pbrMetallicRoughness":{{"baseColorFactor":[0.55,0.57,0.6,1.0],"metallicFactor":0.7,"roughnessFactor":0.5}}}},
          {{"name":"glass","alphaMode":"BLEND","pbrMetallicRoughness":{{"baseColorFactor":[0.75,0.85,0.95,0.3],"metallicFactor":0.0,"roughnessFactor":0.05}}}},
          {{"name":"lights","emissiveFactor":[1.0,0.6,0.3],"pbrMetallicRoughness":{{"baseColorFactor":[0.1,0.1,0.1,1.0]}}}}],
        "buffers":[{{"byteLength":{},"uri":"hull.bin"}}],
        "bufferViews":[{}],"accessors":[{}]}}"#,
        primitives.join(","),
        bin.len(),
        views.join(","),
        accessors.join(",")
    );
    (json, bin)
}

/// Writes the class as a glTF and cooks it with the asset pipeline's importer and LOD generator.
fn cook_class(spec: &ClassSpec, dir: &Path) -> Result<(Vec<Prim>, [usize; 3]), String> {
    let parts = build_parts(&spec.shape, spec.lod0);
    let generated = [parts[0].triangles(), parts[1].triangles(), parts[2].triangles()];
    let (json, bin) = gltf_json_and_bin(&parts);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(dir.join("hull.bin"), &bin).map_err(|e| e.to_string())?;
    let path = dir.join("hull.gltf");
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    let ratios = spec.cook_ratios();
    // `Some(..)` is required: the importer parses this string without RON's `IMPLICIT_SOME`, and a
    // bare `generate_lods: (..)` is dropped without a warning (reported, v020t).
    let meta = (ratios.len() > 1).then(|| {
        let list: Vec<String> = ratios.iter().map(|r| format!("{r:.6}")).collect();
        format!(
            "(generate_lods: Some((count: {}, ratios: [{}])))",
            ratios.len(),
            list.join(", ")
        )
    });
    let result = import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        meta.as_deref(),
        TextureCookQuality::High,
    )
    .map_err(|e| format!("import {}: {e}", path.display()))?;
    let mut prims = Vec::new();
    for index in 0..3 {
        let suffix = format!("/primitive_{index}");
        let sub = result
            .sub_assets
            .iter()
            .find(|s| s.type_path == "regia_asset::CookedMesh" && s.path.ends_with(&suffix))
            .ok_or_else(|| format!("no mesh …{suffix} in the import"))?;
        let sub_path = &sub.path;
        let cooked = CookedMesh::from_postcard(&sub.payload).map_err(|e| format!("{sub_path}: {e}"))?;
        let cpu = cooked.to_cpu_mesh().map_err(|e| format!("{sub_path}: {e}"))?;
        let mesh = Arc::new(RenderMesh::from_cooked_rigid(&cpu));
        let chain = if cooked.lods.len() > 1 {
            Some(MeshLodChain::from_cooked(&cooked).map_err(|e| format!("{sub_path}: {e}"))?)
        } else {
            None
        };
        let lod_tris = match &chain {
            Some(chain) => chain.levels.iter().map(|l| l.indices.len() / 3).collect(),
            None => vec![mesh.indices.len() / 3],
        };
        prims.push(Prim {
            mesh,
            chain,
            lod_tris,
            radius: cooked.bounding_sphere[3],
        });
    }
    Ok((prims, generated))
}

/// A procedural 2K panel set: albedo (BC7 sRGB), normal (BC5), packed ORM (BC7 linear).
fn class_maps(size: u32, seed: u32) -> Result<CompressedMaps, String> {
    let n = size as usize;
    let cell = (size / 16).max(4) as usize;
    let hash = |x: usize, y: usize| -> f32 {
        let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA6B) ^ seed;
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        (h & 0xFFFF) as f32 / 65535.0
    };
    let mut albedo = vec![0u8; n * n * 4];
    let mut normal = vec![0u8; n * n * 4];
    let mut orm = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (cx, cy) = (x / cell, y / cell);
            let (fx, fy) = (x % cell, y % cell);
            let groove_x = fx < 2 || fx + 2 >= cell;
            let groove_y = fy < 2 || fy + 2 >= cell;
            let tint = 0.85 + 0.15 * hash(cx, cy);
            let grime = 0.9 + 0.1 * hash(x / 3, y / 3);
            let o = (y * n + x) * 4;
            let v = |c: f32| (c * tint * grime * 255.0).clamp(0.0, 255.0) as u8;
            let groove = if groove_x || groove_y { 0.55 } else { 1.0 };
            albedo[o..o + 4].copy_from_slice(&[v(0.55 * groove), v(0.57 * groove), v(0.6 * groove), 255]);
            let nx = if groove_x {
                if fx < 2 { 0.5 } else { -0.5 }
            } else {
                0.0
            };
            let ny = if groove_y {
                if fy < 2 { 0.5 } else { -0.5 }
            } else {
                0.0
            };
            let nz = (1.0f32 - nx * nx - ny * ny).max(0.0).sqrt();
            normal[o..o + 4].copy_from_slice(&[
                ((nx * 0.5 + 0.5) * 255.0) as u8,
                ((ny * 0.5 + 0.5) * 255.0) as u8,
                ((nz * 0.5 + 0.5) * 255.0) as u8,
                255,
            ]);
            let rough = 0.35 + 0.35 * hash(cx + 7, cy + 3);
            orm[o..o + 4].copy_from_slice(&[(groove * 255.0) as u8, (rough * 255.0) as u8, 200, 255]);
        }
    }
    let cook = |rgba: &[u8], usage: TextureCookUsage| -> Result<CompressedTexture, String> {
        let cooked = cook_texture_rgba8(size, size, rgba, usage, "linux", TextureCookQuality::High)
            .map_err(|e| format!("cook {usage:?}: {e}"))?;
        CompressedTexture::from_cooked(&cooked).map_err(|e| format!("decode {usage:?}: {e}"))
    };
    Ok(CompressedMaps {
        albedo: Some(cook(&albedo, TextureCookUsage::ColorSrgb)?),
        normal: Some(cook(&normal, TextureCookUsage::NormalMap)?),
        occlusion: Some(cook(&orm, TextureCookUsage::ColorLinear)?),
        ..CompressedMaps::default()
    })
}

/// Walker skin: joint 0 is the body, joints 1–6 the legs (each vertex goes to the nearest piece).
fn skin_walker(prims: &[Prim], shape: &Shape) -> SkinnedHull {
    let legs: Vec<&Piece> = shape.parts[BODY].iter().skip(1).collect();
    let hips: Vec<[f32; 3]> = legs
        .iter()
        .map(|p| [p.center[0], p.center[1] + p.radii[1], p.center[2]])
        .collect();
    let meshes = prims
        .iter()
        .enumerate()
        .map(|(index, prim)| {
            let vertices = prim
                .mesh
                .vertices
                .iter()
                .map(|v| {
                    let mut joint = 0u32;
                    if index == BODY {
                        let mut best = f32::MAX;
                        for (i, piece) in shape.parts[BODY].iter().enumerate() {
                            let d = sub(v.position, piece.center);
                            let q = [
                                d[0] / piece.radii[0],
                                d[1] / piece.radii[1],
                                d[2] / piece.radii[2],
                            ];
                            let e = (dot(q, q).sqrt() - 1.0).abs();
                            if e < best {
                                best = e;
                                joint = i as u32;
                            }
                        }
                    }
                    LabVertex {
                        joints: [joint, 0, 0, 0],
                        weights: [1.0, 0.0, 0.0, 0.0],
                        ..*v
                    }
                })
                .collect();
            Arc::new(RenderMesh {
                vertices,
                indices: prim.mesh.indices.clone(),
            })
        })
        .collect();
    (meshes, hips)
}

fn cook_assets(spec: ClassSpec, texture: u32, dir: &Path) -> Result<ClassAssets, String> {
    let t0 = Instant::now();
    let (prims, generated) = cook_class(&spec, &dir.join(spec.key.name()))?;
    let cook_s = t0.elapsed().as_secs_f32();
    let t1 = Instant::now();
    let maps = Arc::new(class_maps(texture, spec.key as u32 + 1)?);
    let texture_s = t1.elapsed().as_secs_f32();
    let body = Arc::new(LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        metallic: 0.7,
        roughness: 1.0,
        orm_packed: true,
        compressed: Some(Arc::clone(&maps)),
        ..LabMaterial::default()
    });
    let glass = Arc::new(LabMaterial {
        base_color: [0.75, 0.85, 0.95, 0.3],
        metallic: 0.0,
        roughness: 0.05,
        alpha_mode: ALPHA_BLEND,
        ..LabMaterial::default()
    });
    let lights = Arc::new(LabMaterial {
        base_color: [0.1, 0.1, 0.1, 1.0],
        emissive: [1.0, 0.6, 0.3],
        emissive_intensity: 2_000.0,
        roughness: 0.6,
        compressed: Some(maps),
        ..LabMaterial::default()
    });
    let skinned = (spec.key == ClassKey::Walker).then(|| skin_walker(&prims, &spec.shape));
    let level_meshes = prims
        .iter()
        .map(|prim| match &prim.chain {
            Some(chain) => chain
                .levels
                .iter()
                .map(|level| {
                    Arc::new(RenderMesh {
                        vertices: prim.mesh.vertices.clone(),
                        indices: level.indices.as_ref().clone(),
                    })
                })
                .collect(),
            None => vec![Arc::clone(&prim.mesh)],
        })
        .collect();
    Ok(ClassAssets {
        spec,
        prims,
        generated,
        body,
        glass,
        lights,
        skinned,
        level_meshes,
        cook_s,
        texture_s,
    })
}

impl ClassAssets {
    fn lod0_tris(&self) -> usize {
        self.prims.iter().map(|p| p.lod_tris[0]).sum()
    }

    /// Triangles per level summed over the primitives (a primitive with fewer levels repeats its last).
    fn level_tris(&self) -> Vec<usize> {
        let levels = self.prims.iter().map(|p| p.lod_tris.len()).max().unwrap_or(1);
        (0..levels)
            .map(|l| {
                self.prims
                    .iter()
                    .map(|p| p.lod_tris[l.min(p.lod_tris.len() - 1)])
                    .sum()
            })
            .collect()
    }

    /// Distances (metres) where the body switches LOD0→1, 1→2, 2→3 at a viewport height, with
    /// Desktop High's bias and the tier scales at 1 (the places every layout uses).
    fn switch_distances(&self, height: u32) -> Vec<f32> {
        self.switch_distances_with(height, &legacy_scales(), false)
    }

    /// The body's colour (`shadow` false) or shadow switch distances with `scales`: the
    /// selector compares `projected size × colour factor (× shadow scale)` with
    /// `threshold × 1080 / height`.
    fn switch_distances_with(&self, height: u32, scales: &MeshLodScales, shadow: bool) -> Vec<f32> {
        let Some(chain) = self.prims[BODY].chain.as_ref() else {
            return Vec::new();
        };
        chain
            .levels
            .iter()
            .take(chain.levels.len().saturating_sub(1))
            .map(|l| {
                scales
                    .switch_distance(chain.bounds_radius, l.screen_size, height, FOV_Y, 1.0, shadow)
                    .unwrap_or(f32::INFINITY)
            })
            .collect()
    }

    /// `close20`'s tier scales at `height`: the distance scale that puts the body's colour
    /// LOD0→1 switch at 20 m, and the shadow scale that undoes it (shadows select what the
    /// scale-1 colour selects). `None` without a chain.
    fn close20_scales(&self, height: u32) -> Option<(f32, f32)> {
        let d0 = *self.switch_distances(height).first()?;
        let bias = legacy_scales().bias;
        // The product bias × distance clamps to 0.25..16, so the distance scale tops out there.
        let distance = (CLOSE_LOD0_M / d0).clamp(
            MESH_LOD_DISTANCE_SCALE_RANGE.0 / bias,
            MESH_LOD_DISTANCE_SCALE_RANGE.1 / bias,
        );
        let shadow = (1.0 / distance).clamp(MESH_LOD_SHADOW_SCALE_RANGE.0, MESH_LOD_SHADOW_SCALE_RANGE.1);
        Some((distance, shadow))
    }

    fn summary(&self) -> String {
        let generated: usize = self.generated.iter().sum();
        let table = self.spec.lod0 as f32;
        let levels = self.level_tris();
        let ratios: Vec<String> = levels
            .iter()
            .map(|t| format!("{t} ({:.2} %)", 100.0 * *t as f32 / levels[0].max(1) as f32))
            .collect();
        let asked: Vec<String> = self
            .spec
            .cook_ratios()
            .iter()
            .map(|r| format!("{:.2} %", r * 100.0))
            .collect();
        format!(
            "{}: table LOD0 {} | generated {} ({:+.2} %) | cooked LOD0 {} | levels {} | asked {} | per part LOD0 body/glass/lights {}/{}/{} | cook {:.1} s, maps {:.1} s",
            self.spec.key.name(),
            self.spec.lod0,
            generated,
            100.0 * (generated as f32 - table) / table,
            self.lod0_tris(),
            ratios.join(" / "),
            if asked.is_empty() {
                "LOD0 only".to_string()
            } else {
                asked.join(" / ")
            },
            self.prims[BODY].lod_tris[0],
            self.prims[GLASS].lod_tris[0],
            self.prims[LIGHTS].lod_tris[0],
            self.cook_s,
            self.texture_s,
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Every hull forced to LOD0 (`DisableMeshLod`), same places as `Mix`.
    Lod0,
    /// By-distance places from the cooked thresholds; the engine selects.
    Mix,
    /// DO27's intended mix pinned (same places as `Mix`): each hull draws the cooked level of its
    /// band (2 at LOD0, about a fifth at LOD1, the rest LOD2/LOD3) whatever the thresholds say.
    Table,
    /// Same places; the engine selects with colour LOD0 held to 20 m and shadows at the table
    /// mix ([`ClassAssets::close20_scales`]).
    Close20,
    /// Same places; the engine selects with Desktop Ultra's tier scales.
    Ultra,
}

impl Layout {
    const ALL: [Self; 5] = [Self::Lod0, Self::Mix, Self::Table, Self::Close20, Self::Ultra];

    fn key(self) -> &'static str {
        match self {
            Self::Lod0 => "lod0",
            Self::Mix => "mix",
            Self::Table => "table",
            Self::Close20 => "close20",
            Self::Ultra => "ultra",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Lod0 => "all LOD0",
            Self::Mix => "LOD mix",
            Self::Table => "table mix",
            Self::Close20 => "colour LOD0 to 20 m, shadows at the table mix",
            Self::Ultra => "Desktop Ultra tier scales",
        }
    }
}

/// Where `close20` holds the body's colour LOD 0 to (metres).
const CLOSE_LOD0_M: f32 = 20.0;

/// The selection before the tier scales: Desktop High's bias, distance scale 1, shadow 0.5.
fn legacy_scales() -> MeshLodScales {
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.mesh_lod_distance_scale = 1.0;
    profile.mesh_lod_shadow_scale = 0.5;
    MeshLodScales::of_profile(&profile)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RenderPath {
    /// Rigid meshes, forward (Desktop High's default path).
    Forward,
    /// Rigid opaque meshes through the visibility buffer (clusters from the pipeline's cooker).
    VisBuffer,
    /// Skinned, vertex-shader skinning (forward).
    SkinnedVertex,
    /// Skinned, compute skinning (forward).
    SkinnedCompute,
}

impl RenderPath {
    fn name(self) -> &'static str {
        match self {
            Self::Forward => "forward rigid",
            Self::VisBuffer => "visbuffer rigid",
            Self::SkinnedVertex => "forward skinned (vertex)",
            Self::SkinnedCompute => "forward skinned (compute)",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Case {
    n: usize,
    layout: Layout,
    glass: bool,
    path: RenderPath,
    /// Cockpit: the seated set plus `n` skimmers outside.
    cockpit: bool,
}

/// Places for `n` hulls in front of the camera: (distance band LOD, world position, yaw).
fn fleet_places(assets: &ClassAssets, n: usize, height: u32, aspect: f32) -> Vec<(usize, Vec3, f32)> {
    let d = assets.switch_distances(height);
    let radius = assets.prims[BODY].radius.max(1.0);
    // Distances for the LOD bands; a chain-less class stands at 3 radii.
    let band = |lod: usize| -> f32 {
        match (lod, d.len()) {
            (_, 0) => 3.0 * radius,
            (0, _) => (0.7 * d[0]).max(2.5 * radius),
            (1, 1) => 1.6 * d[0],
            (1, _) => (d[0] * d[1]).sqrt(),
            (2, 2) => 1.6 * d[1],
            (2, _) => (d[1] * d[2]).sqrt(),
            _ => 1.6 * d[d.len() - 1],
        }
    };
    let mut lods = Vec::with_capacity(n);
    let lod1 = (n.saturating_sub(2) as f32 * 0.2).ceil() as usize;
    for i in 0..n {
        let lod = if i < 2 {
            0
        } else if i < 2 + lod1 {
            1
        } else if (i - 2 - lod1) % 5 < 3 {
            2
        } else {
            3
        };
        lods.push(lod.min(d.len()));
    }
    let half_x = ((FOV_Y * 0.5).tan() * aspect).atan() * 0.7;
    let mut places = Vec::with_capacity(n);
    for lod in 0..4 {
        let members: Vec<usize> = (0..n).filter(|&i| lods[i] == lod).collect();
        let m = members.len();
        for (slot, &i) in members.iter().enumerate() {
            let t = if m == 1 {
                0.0
            } else {
                (slot as f32 + 0.5) / m as f32 * 2.0 - 1.0
            };
            let angle = t * half_x;
            // Alternate depth inside the band so neighbours do not stand in one line.
            let distance = band(lod) * if slot % 2 == 0 { 0.94 } else { 1.06 };
            let pos = Vec3::new(distance * angle.sin(), 0.0, -distance * angle.cos());
            let yaw = std::f32::consts::FRAC_PI_2 + 0.6 * i as f32;
            places.push((lod, pos, yaw));
        }
    }
    places
}

fn place(transform: Transform) -> (Transform, GlobalTransform) {
    (transform, GlobalTransform(transform.compute_affine()))
}

fn look_at(eye: Vec3, target: Vec3) -> Transform {
    Transform::from_affine(
        regia_core::glam::camera::rh::view::look_at_affine3a(eye, target, Vec3::Y).inverse(),
    )
}

/// Desktop High as shipped, exposure fixed, with the mesh LOD tier scales at the values that
/// select as before them (1 and 0.5; `build_world` sets `close20`'s and `ultra`'s); `light`
/// trims the screen-space and GI passes (the lavapipe smoke).
fn settings(path: RenderPath, light: bool) -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.profile.mesh_lod_distance_scale = 1.0;
    settings.profile.mesh_lod_shadow_scale = 0.5;
    settings.profile.visibility_buffer = path == RenderPath::VisBuffer;
    settings.post.auto_exposure = false;
    settings.post.exposure_ev = 14.0;
    if light {
        settings.profile.gi_probes = false;
        settings.profile.screen_space_gi = false;
        settings.profile.ssr = false;
        settings.profile.volumetric_fog = false;
        settings.profile.volumetric_clouds = false;
        settings.profile.physical_atmosphere = false;
        settings.profile.reflection_probes = false;
        settings.profile.bloom = false;
        settings.profile.anti_aliasing = AntiAliasing::None;
        settings.profile.ssao = SsaoMode::Off;
        settings.profile.contact_shadows = false;
        settings.profile.motion_blur = false;
        settings.profile.depth_of_field = false;
        settings.post.bloom = false;
        settings.post.taa = false;
        settings.post.fxaa = false;
    }
    settings
}

/// What a frame drew: entity → (hull, primitive) for the LOD histogram.
type Tags = BTreeMap<Entity, (usize, usize)>;

/// What spawning recorded for the frame loop and the statistics.
#[derive(Default)]
struct Spawned {
    tags: Tags,
    /// Skinned entities (their palettes animate).
    skinned: Vec<Entity>,
    /// Units that move every frame: entity, resting transform, phase.
    movers: Vec<(Entity, Transform, f32)>,
    /// `Table` layout: the level each entity was pinned to.
    pinned: BTreeMap<Entity, usize>,
}

struct Bench {
    app: App,
    camera: Entity,
    eye: Vec3,
    target: Vec3,
    tags: Tags,
    /// Skinned entities with their palette hips.
    skinned: Vec<Entity>,
    hips: Vec<[f32; 3]>,
    /// The camera's layer mask (the cockpit hides its own exterior).
    layer_mask: u32,
    /// Units that move every frame: entity, resting transform, phase.
    movers: Vec<(Entity, Transform, f32)>,
    /// `Table` layout: the level each entity was pinned to.
    pinned: BTreeMap<Entity, usize>,
}

fn spawn_prim(
    world: &mut World,
    transform: Transform,
    mesh: Arc<RenderMesh>,
    material: Arc<CpuMaterial>,
    chain: Option<&MeshLodChain>,
    force_lod0: bool,
) -> Entity {
    let mut entity = world.spawn((place(transform), MeshRenderer::new(mesh, material)));
    if let Some(chain) = chain {
        entity.insert(chain.clone());
        if force_lod0 {
            entity.insert(DisableMeshLod);
        }
    }
    entity.id()
}

fn new_app(settings: RenderSettings, width: u32, height: u32) -> App {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(settings);
    app.insert_resource(LodViewport { width, height });
    app.insert_resource(Environment::default());
    app.add_systems(Render, (extract_frame, select_mesh_lods, extract_glass).chain());
    app.world_mut().spawn((
        place(Transform::IDENTITY),
        DirectionalLight {
            direction: [0.35, -0.8, -0.45],
            color: [1.0, 0.97, 0.92],
            illuminance: 90_000.0,
            cast_shadows: true,
            static_shadow: false,
        },
    ));
    app
}

fn ground() -> (Arc<RenderMesh>, Arc<CpuMaterial>) {
    let (vertices, indices) = regia_render::box_mesh([3_000.0, 0.05, 3_000.0]);
    (
        Arc::new(RenderMesh { vertices, indices }),
        Arc::new(LabMaterial {
            base_color: [0.32, 0.3, 0.27, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }),
    )
}

/// Spawns `n` hulls of `assets` at the fleet places; returns their entities' tags.
#[allow(clippy::too_many_arguments)]
fn spawn_fleet(
    app: &mut App,
    assets: &ClassAssets,
    n: usize,
    layout: Layout,
    glass: bool,
    path: RenderPath,
    width: u32,
    height: u32,
    out: &mut Spawned,
) {
    let skinned_meshes = matches!(path, RenderPath::SkinnedVertex | RenderPath::SkinnedCompute)
        .then(|| assets.skinned.as_ref())
        .flatten();
    for (hull, (band, pos, yaw)) in fleet_places(assets, n, height, width as f32 / height as f32)
        .into_iter()
        .enumerate()
    {
        let transform = Transform {
            translation: pos,
            rotation: Quat::from_rotation_y(yaw),
            scale: Vec3::ONE,
        };
        for (index, prim) in assets.prims.iter().enumerate() {
            let material = match index {
                GLASS if glass => Arc::clone(&assets.glass),
                LIGHTS => Arc::clone(&assets.lights),
                _ => Arc::clone(&assets.body),
            };
            let entity = match skinned_meshes {
                Some((meshes, _)) => {
                    let id = spawn_prim(
                        app.world_mut(),
                        transform,
                        Arc::clone(&meshes[index]),
                        material,
                        None,
                        true,
                    );
                    out.skinned.push(id);
                    id
                }
                None if layout == Layout::Table => {
                    let levels = &assets.level_meshes[index];
                    let lod = band.min(levels.len() - 1);
                    let id = spawn_prim(
                        app.world_mut(),
                        transform,
                        Arc::clone(&levels[lod]),
                        material,
                        None,
                        true,
                    );
                    out.pinned.insert(id, lod);
                    id
                }
                None => spawn_prim(
                    app.world_mut(),
                    transform,
                    Arc::clone(&prim.mesh),
                    material,
                    prim.chain.as_ref(),
                    layout == Layout::Lod0,
                ),
            };
            out.tags.insert(entity, (hull, index));
            out.movers.push((entity, transform, hull as f32 * 0.9));
        }
    }
}

fn build_world(
    assets: &ClassAssets,
    others: Option<&ClassAssets>,
    case: Case,
    width: u32,
    height: u32,
    light: bool,
) -> Bench {
    let mut render_settings = settings(case.path, light);
    match case.layout {
        Layout::Close20 => {
            if let Some((distance, shadow)) = assets.close20_scales(height) {
                render_settings.profile.mesh_lod_distance_scale = distance;
                render_settings.profile.mesh_lod_shadow_scale = shadow;
            }
        }
        Layout::Ultra => {
            let ultra = RenderProfile::from_preset(RenderProfilePreset::DesktopUltra);
            render_settings.profile.mesh_lod_bias = ultra.mesh_lod_bias;
            render_settings.profile.mesh_lod_distance_scale = ultra.mesh_lod_distance_scale;
            render_settings.profile.mesh_lod_shadow_scale = ultra.mesh_lod_shadow_scale;
        }
        Layout::Lod0 | Layout::Mix | Layout::Table => {}
    }
    let mut app = new_app(render_settings, width, height);
    let (ground_mesh, ground_material) = ground();
    app.world_mut().spawn((
        place(Transform::from_translation(Vec3::new(0.0, -0.05, 0.0))),
        MeshRenderer::new(ground_mesh, ground_material),
    ));
    let mut spawned = Spawned::default();
    let mut layer_mask = u32::MAX;
    let (eye, target, near) = match assets.spec.key {
        ClassKey::Interior => {
            for (index, prim) in assets.prims.iter().enumerate() {
                let material = match index {
                    GLASS if case.glass => Arc::clone(&assets.glass),
                    LIGHTS => Arc::clone(&assets.lights),
                    _ => Arc::clone(&assets.body),
                };
                let id = spawn_prim(
                    app.world_mut(),
                    Transform::IDENTITY,
                    Arc::clone(&prim.mesh),
                    material,
                    None,
                    true,
                );
                spawned.tags.insert(id, (0, index));
            }
            (Vec3::new(0.0, 1.7, 3.0), Vec3::new(0.0, 1.5, -6.0), 0.1)
        }
        ClassKey::Cockpit => {
            // Seated: the cockpit around the eye, the canopy viewer-inside, the own exterior
            // (a skimmer, LOD0) on a layer the camera does not draw.
            let eye = Vec3::from_array(COCKPIT_EYE) + Vec3::new(0.0, 1.0, 0.0);
            let cockpit = Transform::from_translation(Vec3::new(0.0, 1.0, 0.0));
            for (index, prim) in assets.prims.iter().enumerate() {
                let material = match index {
                    GLASS => Arc::clone(&assets.glass),
                    LIGHTS => Arc::clone(&assets.lights),
                    _ => Arc::clone(&assets.body),
                };
                if index == GLASS && !case.glass {
                    continue;
                }
                let id = spawn_prim(
                    app.world_mut(),
                    cockpit,
                    Arc::clone(&prim.mesh),
                    material,
                    None,
                    true,
                );
                if index == GLASS {
                    app.world_mut().entity_mut(id).insert(GlassView::viewer_inside());
                }
                spawned.tags.insert(id, (0, index));
            }
            if let Some(exterior) = others {
                // The skimmer's canopy centre sits at the eye; the hull faces −Z.
                let canopy = Vec3::new(1.4, 1.85, 0.0);
                let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
                let transform = Transform {
                    translation: eye - rotation * canopy,
                    rotation,
                    scale: Vec3::ONE,
                };
                for (index, prim) in exterior.prims.iter().enumerate() {
                    let material = match index {
                        GLASS => Arc::clone(&exterior.glass),
                        LIGHTS => Arc::clone(&exterior.lights),
                        _ => Arc::clone(&exterior.body),
                    };
                    let id = spawn_prim(
                        app.world_mut(),
                        transform,
                        Arc::clone(&prim.mesh),
                        material,
                        None,
                        true,
                    );
                    if let Some(mut renderer) = app.world_mut().get_mut::<MeshRenderer>(id) {
                        renderer.layer = OWN_EXTERIOR_LAYER;
                    }
                }
                layer_mask = !(1 << OWN_EXTERIOR_LAYER);
                if case.n > 0 {
                    let mut outside = Spawned::default();
                    spawn_fleet(
                        &mut app,
                        exterior,
                        case.n,
                        Layout::Mix,
                        true,
                        RenderPath::Forward,
                        width,
                        height,
                        &mut outside,
                    );
                    for (entity, (hull, prim)) in outside.tags {
                        spawned.tags.insert(entity, (hull + 1, prim + 3));
                    }
                    spawned.movers.extend(outside.movers);
                }
            }
            (eye, eye + Vec3::new(0.0, -0.15, -1.0), 0.05)
        }
        _ => {
            spawn_fleet(
                &mut app,
                assets,
                case.n,
                case.layout,
                case.glass,
                case.path,
                width,
                height,
                &mut spawned,
            );
            let lift = if assets.spec.key == ClassKey::Walker {
                3.5
            } else {
                2.5
            };
            (Vec3::new(0.0, lift, 0.0), Vec3::new(0.0, lift - 1.2, -60.0), 0.1)
        }
    };
    let far = std::iter::once(assets)
        .chain(others)
        .filter_map(|a| a.switch_distances(height).last().copied())
        .fold(2_000.0f32, |far, d| far.max(d * 3.0));
    let camera = app
        .world_mut()
        .spawn((
            place(look_at(eye, target)),
            Camera {
                fov_y: FOV_Y,
                near,
                far,
                exposure_ev: Some(14.0),
                layer_mask,
                ..Camera::default()
            },
        ))
        .id();
    if case.path == RenderPath::SkinnedVertex || case.path == RenderPath::SkinnedCompute {
        let hips = assets
            .skinned
            .as_ref()
            .map(|(_, h)| h.clone())
            .unwrap_or_default();
        for &id in &spawned.skinned {
            app.world_mut().entity_mut(id).insert(SkinJoints {
                palette: palette(&hips, 0.0),
            });
        }
    }
    let hips = assets
        .skinned
        .as_ref()
        .map(|(_, h)| h.clone())
        .unwrap_or_default();
    let still = assets.spec.key == ClassKey::Structure
        || regia_core::brand::env_var_os("UNIT_BUDGET_STATIC").is_some();
    if still {
        spawned.movers.clear();
    }
    Bench {
        app,
        camera,
        eye,
        target,
        tags: spawned.tags,
        skinned: spawned.skinned,
        hips,
        layer_mask,
        movers: spawned.movers,
        pinned: spawned.pinned,
    }
}

/// The walker's palette at time `t`: the body still, each leg swinging about its hip.
fn palette(hips: &[[f32; 3]], t: f32) -> Vec<JointMatrix> {
    let mut joints = vec![JointMatrix::identity()];
    for (i, hip) in hips.iter().enumerate() {
        let hip = Vec3::from_array(*hip);
        let swing = Mat4::from_translation(hip)
            * Mat4::from_rotation_z(0.25 * (t * 2.0 + i as f32 * 1.1).sin())
            * Mat4::from_translation(-hip);
        joints.push(JointMatrix::from_mat4(swing));
    }
    joints
}

impl Bench {
    /// Advances one frame: camera sway, leg swing, extraction and LOD selection.
    fn step(&mut self, frame: usize) {
        let t = frame as f32 / 60.0;
        let sway = Vec3::new(0.06 * (t * 1.7).sin(), 0.02 * (t * 1.3).sin(), 0.0);
        let transform = look_at(self.eye + sway, self.target + sway);
        self.app
            .world_mut()
            .entity_mut(self.camera)
            .insert(place(transform));
        // Units hover and turn a little every frame, so their shadows are redrawn as in a match
        // (structures stand still and keep their cached shadow pages).
        for &(id, rest, phase) in &self.movers {
            let moved = Transform {
                translation: rest.translation + Vec3::new(0.0, 0.03 * (t * 2.0 + phase).sin(), 0.0),
                rotation: rest.rotation * Quat::from_rotation_y(0.02 * (t * 0.7 + phase).sin()),
                scale: rest.scale,
            };
            self.app.world_mut().entity_mut(id).insert(place(moved));
        }
        if !self.skinned.is_empty() {
            let joints = palette(&self.hips, t);
            for &id in &self.skinned {
                if let Some(mut skin) = self.app.world_mut().get_mut::<SkinJoints>(id) {
                    skin.palette.clone_from(&joints);
                }
            }
        }
        self.app.update();
    }

    fn extracted(&self) -> &ExtractedFrame {
        self.app.world().resource::<ExtractedFrame>()
    }

    /// Triangles drawn in the colour view and the body LOD histogram of the last extraction.
    fn visible(&self) -> (usize, [usize; 4]) {
        let mut tris = 0usize;
        let mut hist = [0usize; 4];
        for mesh in &self.extracted().meshes {
            if mesh.lod_key & SHADOW_LOD_MESH_TAG == SHADOW_LOD_MESH_TAG {
                continue;
            }
            if self.layer_mask & (1 << mesh.layer.min(31)) == 0 {
                continue;
            }
            let indices = mesh
                .draw_indices
                .as_deref()
                .map_or(mesh.mesh.indices.len(), Vec::len);
            tris += indices / 3;
            // The outgoing half of a cross-fade counts in the triangles, not the histogram.
            if mesh.lod_dither < 0.0 {
                continue;
            }
            if let Some(&(_, prim)) = self.tags.get(&mesh.entity)
                && prim % 3 == BODY
            {
                let lod = match self.pinned.get(&mesh.entity) {
                    Some(&lod) => lod,
                    None => ((mesh.lod_key >> 1) & 3) as usize,
                };
                hist[lod.min(3)] += 1;
            }
        }
        (tris, hist)
    }

    /// Body LOD histogram of the shadow casters: a shadow-only draw's level, else the level of
    /// the colour draw that casts.
    fn shadow_hist(&self) -> [usize; 4] {
        let mut by_entity: BTreeMap<Entity, usize> = BTreeMap::new();
        for mesh in &self.extracted().meshes {
            let Some(&(_, prim)) = self.tags.get(&mesh.entity) else {
                continue;
            };
            if prim % 3 != BODY || mesh.shadow == regia_render::ShadowCast::Off {
                continue;
            }
            let lod = match self.pinned.get(&mesh.entity) {
                Some(&lod) => lod,
                None => ((mesh.lod_key >> 1) & 3) as usize,
            };
            by_entity.insert(mesh.entity, lod.min(3));
        }
        let mut hist = [0usize; 4];
        for lod in by_entity.into_values() {
            hist[lod] += 1;
        }
        hist
    }
}

// ---------------------------------------------------------------------------------------------
// Measuring
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Row {
    res: (u32, u32),
    class: ClassKey,
    case: Case,
    frame_ms: f32,
    /// Fastest measured frame: the cost without another process's work on a shared GPU.
    frame_min: f32,
    frame_p90: f32,
    geometry_ms: f32,
    glass_ms: f32,
    top: Vec<(&'static str, f32)>,
    tris: usize,
    hist: [usize; 4],
    shadow_hist: [usize; 4],
    draws: regia_render::DrawStats,
    vis: Option<(u32, u32)>,
    vram_mb: f32,
    meshes_mb: f32,
    cpu_ms: f32,
}

fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

fn p90(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    values[((values.len() - 1) as f32 * 0.9).round() as usize]
}

/// Shadows, prepass, cull (with the visibility pass) and forward: what hull triangles drive.
fn geometry(g: &GpuTimings) -> f32 {
    g.shadows_ms + g.prepass_ms + g.cull_ms + g.forward_ms
}

struct Target {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn target(device: &RenderDevice, width: u32, height: u32) -> Target {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("unit budget target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Target {
        _texture: texture,
        view,
        width,
        height,
    }
}

fn vis_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        });
        build_meshlet_mesh_with_attributes(&positions, attributes, indices, &MeshletSettings::default()).ok()
    })
}

/// Renders one case (warm-up, then `frames` measured frames) and returns its row.
#[allow(clippy::too_many_arguments)]
fn measure(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    out: &Target,
    assets: &ClassAssets,
    others: Option<&ClassAssets>,
    case: Case,
    warm: usize,
    frames: usize,
) -> Result<Row, String> {
    renderer.set_meshlet_builder((case.path == RenderPath::VisBuffer).then(vis_builder));
    renderer.set_compute_skinning(case.path == RenderPath::SkinnedCompute);
    let mut world = build_world(assets, others, case, out.width, out.height, false);
    let mut gpu = Vec::with_capacity(frames);
    let mut cpu = Vec::with_capacity(frames);
    for frame in 0..warm + frames {
        world.step(frame);
        render_extracted_into(
            renderer,
            device,
            world.extracted(),
            FrameTarget::full(&out.view, TARGET_FORMAT, out.width, out.height),
        )
        .map_err(|e| format!("frame {frame}: {e}"))?;
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
        if frame >= warm {
            gpu.push(renderer.gpu_timings());
            cpu.push(renderer.cpu_timings().total_ms);
        }
    }
    let (tris, hist) = world.visible();
    let shadow_hist = world.shadow_hist();
    let draws = renderer.draw_stats();
    let vis = if case.path == RenderPath::VisBuffer {
        let counters = renderer
            .visbuffer_counters(device)
            .map_err(|e| format!("visbuffer counters: {e}"))?;
        let stats = renderer.visbuffer_stats();
        Some((
            stats.instances,
            counters.map_or(0, |c| c.phase1_clusters + c.phase2_clusters),
        ))
    } else {
        None
    };
    let report = memory_report(device);
    let mut frame_ms: Vec<f32> = gpu.iter().map(|g| g.frame_ms).collect();
    let mut geometry_ms: Vec<f32> = gpu.iter().map(geometry).collect();
    let mut glass_ms: Vec<f32> = gpu.iter().map(|g| g.forward_glass_ms).collect();
    let mut top: Vec<(&'static str, f32)> = GpuTimings::default()
        .passes()
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let mut v: Vec<f32> = gpu.iter().map(|g| g.passes()[i].1).collect();
            (*name, median(&mut v))
        })
        .collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    top.truncate(5);
    let mut frame_p90 = frame_ms.clone();
    let frame_min = frame_ms.iter().copied().fold(f32::INFINITY, f32::min);
    Ok(Row {
        res: (out.width, out.height),
        class: assets.spec.key,
        case,
        frame_ms: median(&mut frame_ms),
        frame_min: if frame_min.is_finite() { frame_min } else { 0.0 },
        frame_p90: p90(&mut frame_p90),
        geometry_ms: median(&mut geometry_ms),
        glass_ms: median(&mut glass_ms),
        top,
        tris,
        hist,
        shadow_hist,
        draws,
        vis,
        vram_mb: report.allocated_bytes as f32 / (1024.0 * 1024.0),
        meshes_mb: report.bytes(MemoryCategory::Meshes) as f32 / (1024.0 * 1024.0),
        cpu_ms: median(&mut cpu),
    })
}

fn row_line(row: &Row) -> String {
    let top: Vec<String> = row.top.iter().map(|(n, ms)| format!("{n} {ms:.2}")).collect();
    let vis = row.vis.map_or(String::new(), |(instances, clusters)| {
        format!(", vis {instances} inst / {clusters} clusters")
    });
    format!(
        "| {}x{} | {} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {} | {}/{}/{}/{} | {}/{}/{}/{} | {} drawn, {} instanced, {} per-object, {} culled{} | {:.0} / {:.0} | {:.2} |",
        row.res.0,
        row.res.1,
        row.class.name(),
        row.case.path.name(),
        if row.case.cockpit {
            "seated"
        } else {
            row.case.layout.label()
        },
        if row.case.glass { "BLEND" } else { "opaque" },
        row.case.n,
        row.frame_ms,
        row.frame_min,
        row.frame_p90,
        row.geometry_ms,
        row.glass_ms,
        top.join(", "),
        row.tris,
        row.hist[0],
        row.hist[1],
        row.hist[2],
        row.hist[3],
        row.shadow_hist[0],
        row.shadow_hist[1],
        row.shadow_hist[2],
        row.shadow_hist[3],
        row.draws.drawn,
        row.draws.instanced,
        row.draws.per_object,
        row.draws.culled,
        vis,
        row.vram_mb,
        row.meshes_mb,
        row.cpu_ms,
    )
}

const ROW_HEADER: &str = "| res | class | path | layout | glass | N | GPU ms (median) | min | p90 | geometry ms | glass ms | top nodes (ms) | visible tris | body LOD 0/1/2/3 | body shadow LOD 0/1/2/3 | mesh draws | VRAM MB total / meshes | CPU ms |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|";

/// Least squares `y = a + b·x`: (a, b, r²).
fn fit(points: &[(f32, f32)]) -> Option<(f32, f32, f32)> {
    if points.len() < 3 {
        return None;
    }
    let n = points.len() as f64;
    let (sx, sy) = points.iter().fold((0.0f64, 0.0f64), |(sx, sy), (x, y)| {
        (sx + f64::from(*x), sy + f64::from(*y))
    });
    let (mx, my) = (sx / n, sy / n);
    let (mut sxx, mut sxy, mut syy) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in points {
        let (dx, dy) = (f64::from(*x) - mx, f64::from(*y) - my);
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    if sxx <= 0.0 {
        return None;
    }
    let b = sxy / sxx;
    let a = my - b * mx;
    let r2 = if syy > 0.0 { (sxy * sxy) / (sxx * syy) } else { 1.0 };
    Some((a as f32, b as f32, r2 as f32))
}

fn fits(rows: &[Row]) -> String {
    let mut groups: BTreeMap<((u32, u32), RenderPath), Vec<&Row>> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.case.glass && !r.case.cockpit) {
        groups.entry((row.res, row.case.path)).or_default().push(row);
    }
    let mut out = String::from(
        "| res | path | GPU ms per 100k visible tris (frame) | intercept ms | r² | (geometry passes) ms per 100k | r² | cases |\n|---|---|---|---|---|---|---|---|\n",
    );
    for ((res, path), rows) in groups {
        let frame: Vec<(f32, f32)> = rows.iter().map(|r| (r.tris as f32, r.frame_ms)).collect();
        let geo: Vec<(f32, f32)> = rows.iter().map(|r| (r.tris as f32, r.geometry_ms)).collect();
        let (Some((a, b, r2)), Some((_, gb, gr2))) = (fit(&frame), fit(&geo)) else {
            continue;
        };
        let _ = writeln!(
            out,
            "| {}x{} | {} | {:.3} | {:.2} | {:.3} | {:.3} | {:.3} | {} |",
            res.0,
            res.1,
            path.name(),
            b * 100_000.0,
            a,
            r2,
            gb * 100_000.0,
            gr2,
            rows.len()
        );
    }
    out
}

/// DO27's quality tier for a preset (`shell-ferrite` settings; ULTRA LOW is not mapped yet).
fn do27_tier(preset: RenderProfilePreset) -> &'static str {
    match preset {
        RenderProfilePreset::MobileLow => "ULTRA LOW (candidate)",
        RenderProfilePreset::MobileHigh => "-",
        RenderProfilePreset::DesktopLow => "LOW",
        RenderProfilePreset::Deck => "MEDIUM",
        RenderProfilePreset::DesktopHigh => "HIGH",
        RenderProfilePreset::DesktopUltra => "ULTRA / MAX",
        RenderProfilePreset::Cinematic => "- (offline)",
    }
}

/// Per class and resolution: the body's colour and shadow switch distances on every preset, and
/// with `close20`'s scales.
fn tier_table(cooked: &BTreeMap<ClassKey, ClassAssets>, resolutions: &[(u32, u32)]) -> String {
    let metres = |d: &[f32]| -> String {
        d.iter()
            .map(|d| {
                if d.is_finite() {
                    format!("{d:.1}")
                } else {
                    "-".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" / ")
    };
    let mut out = String::from(
        "| res | class | preset | DO27 tier | bias × distance | shadow scale | colour LOD0→1 / 1→2 / 2→3 (m) | shadow LOD0→1 / 1→2 / 2→3 (m) |\n|---|---|---|---|---|---|---|---|\n",
    );
    for &(_, height) in resolutions {
        for (key, assets) in cooked {
            if assets.prims[BODY].chain.is_none() {
                continue;
            }
            let mut rows: Vec<(String, &str, MeshLodScales)> = RenderProfilePreset::ALL
                .iter()
                .map(|p| {
                    (
                        p.name().to_string(),
                        do27_tier(*p),
                        MeshLodScales::of_profile(&RenderProfile::from_preset(*p)),
                    )
                })
                .collect();
            if let Some((distance, shadow)) = assets.close20_scales(height) {
                rows.push((
                    "close20".to_string(),
                    "-",
                    MeshLodScales {
                        distance,
                        shadow,
                        ..legacy_scales()
                    },
                ));
            }
            for (name, tier, scales) in rows {
                let _ = writeln!(
                    out,
                    "| {height}p | {} | {name} | {tier} | {:.2} | {:.2} | {} | {} |",
                    key.name(),
                    scales.color_factor(1.0),
                    scales.shadow,
                    metres(&assets.switch_distances_with(height, &scales, false)),
                    metres(&assets.switch_distances_with(height, &scales, true)),
                );
            }
        }
    }
    out
}

fn env_list<T: std::str::FromStr>(name: &str, default: Vec<T>) -> Vec<T> {
    regia_core::brand::env_var(name)
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect::<Vec<T>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

/// The lavapipe smoke: 5 hulls of 10k triangles through the cook, the LOD selection and the
/// runtime frame; the frame is lit (not black) and the selection used more than one level.
#[test]
fn unit_budget_smoke_five_hulls_render() {
    let device = device();
    let spec = class_spec(ClassKey::Skimmer, 10_000);
    let assets =
        cook_assets(spec, 256, &tmp_dir("unit_budget_smoke")).unwrap_or_else(|e| panic!("cook: {e}"));
    eprintln!("{}", assets.summary());
    let generated: usize = assets.generated.iter().sum();
    let cooked = assets.lod0_tris();
    for (what, count) in [("generated", generated), ("cooked LOD0", cooked)] {
        let error = (count as f32 - 10_000.0).abs() / 10_000.0;
        assert!(
            error <= 0.02,
            "{what} {count} triangles is {:.1} % off 10k",
            error * 100.0
        );
    }
    let levels = assets.prims[BODY].lod_tris.clone();
    assert_eq!(levels.len(), 4, "the body cooked {levels:?}, not four levels");
    assert!(
        levels.windows(2).all(|w| w[1] < w[0]),
        "the body LOD chain does not shrink: {levels:?}"
    );

    let (width, height) = (320, 180);
    let case = Case {
        n: 5,
        layout: Layout::Mix,
        glass: true,
        path: RenderPath::Forward,
        cockpit: false,
    };
    let mut world = build_world(&assets, None, case, width, height, true);
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    let out = target(&device, width, height);
    for frame in 0..2 {
        world.step(frame);
        render_extracted_into(
            &mut renderer,
            &device,
            world.extracted(),
            FrameTarget::full(&out.view, TARGET_FORMAT, width, height),
        )
        .unwrap_or_else(|e| panic!("frame {frame}: {e}"));
    }
    world.step(2);
    let scene = world.extracted().frame_scene(0, width, height);
    let image = renderer
        .render_offscreen(&device, &scene)
        .unwrap_or_else(|e| panic!("offscreen frame: {e}"));
    let (tris, hist) = world.visible();
    let draws = renderer.draw_stats();
    eprintln!(
        "smoke: body switch distances at {height}p {:?} m, body radius {:.2} m",
        assets.switch_distances(height),
        assets.prims[BODY].radius
    );
    let luma: Vec<f32> = image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect();
    let mean = luma.iter().sum::<f32>() / luma.len().max(1) as f32;
    let lit = luma.iter().filter(|&&l| l > 8.0).count() as f32 / luma.len().max(1) as f32;
    eprintln!(
        "smoke: {}x{} mean luma {mean:.1}, lit {:.0} %, visible tris {tris}, body LODs {hist:?}, draws {draws:?}, gpu {}",
        image.width,
        image.height,
        lit * 100.0,
        renderer.gpu_timings().report()
    );
    assert!(
        mean > 10.0 && lit > 0.5,
        "the frame is black: mean luma {mean:.1}, {:.0} % lit",
        lit * 100.0
    );
    assert!(
        draws.drawn >= 15,
        "the 5 hulls (15 primitives) were not drawn: {draws:?}"
    );
    assert!(
        hist.iter().filter(|&&c| c > 0).count() >= 2,
        "the by-distance layout selected one LOD only: {hist:?}"
    );
    assert!(
        tris > 0 && tris < 5 * cooked,
        "visible triangles {tris} for 5 hulls of {cooked}"
    );
}

/// Tier LOD scales on the cooked chain (selection only, no GPU): `close20` puts the body's
/// colour LOD0→1 switch at 20 m and its shadows select what the scale-1 colour selects; Desktop
/// Ultra's scales keep colour detail at least as long and never draw finer shadows than colour.
#[test]
fn unit_budget_tier_scales_hold_colour_lod0_and_shadows_at_the_mix() {
    let spec = class_spec(ClassKey::Skimmer, 10_000);
    let assets = cook_assets(spec, 64, &tmp_dir("unit_budget_tiers")).unwrap_or_else(|e| panic!("cook: {e}"));
    let (width, height) = (1280, 720);
    let (distance, shadow) = assets.close20_scales(height).expect("the body has a chain");
    let close = MeshLodScales {
        distance,
        shadow,
        ..legacy_scales()
    };
    let colour = assets.switch_distances_with(height, &close, false);
    let at_clamp = legacy_scales().bias * distance >= MESH_LOD_DISTANCE_SCALE_RANGE.1 - 1.0e-3;
    assert!(
        (colour[0] - CLOSE_LOD0_M).abs() < 0.05 || at_clamp,
        "close20 colour LOD0→1 at {colour:?} m, not {CLOSE_LOD0_M} m"
    );
    let scale1 = assets.switch_distances(height);
    let shadows = assets.switch_distances_with(height, &close, true);
    for (s, d) in shadows.iter().zip(&scale1) {
        assert!(
            (s / d - 1.0).abs() < 1.0e-4,
            "shadow switches {shadows:?} vs scale-1 colour {scale1:?}"
        );
    }
    let run = |layout| {
        let case = Case {
            n: 20,
            layout,
            glass: true,
            path: RenderPath::Forward,
            cockpit: false,
        };
        let mut world = build_world(&assets, None, case, width, height, true);
        world.step(0);
        (world.visible().1, world.shadow_hist())
    };
    let (mix, mix_shadow) = run(Layout::Mix);
    let (near, near_shadow) = run(Layout::Close20);
    let (ultra, ultra_shadow) = run(Layout::Ultra);
    eprintln!(
        "tiers at {height}p: scale-1 switches {scale1:?} m, close20 ×{distance:.2} / shadow {shadow:.3}; \
         colour mix {mix:?} close20 {near:?} ultra {ultra:?}; shadows mix {mix_shadow:?} close20 \
         {near_shadow:?} ultra {ultra_shadow:?}"
    );
    assert_eq!(near.iter().sum::<usize>(), 20);
    assert!(
        near[0] >= mix[0] && ultra[0] >= mix[0],
        "{mix:?} {near:?} {ultra:?}"
    );
    assert_eq!(
        near_shadow, mix,
        "close20 shadows select what the scale-1 colour selects"
    );
    let mean = |h: &[usize; 4]| h.iter().enumerate().map(|(l, c)| l * c).sum::<usize>();
    assert!(
        mean(&ultra_shadow) >= mean(&ultra),
        "Ultra's shadows are never finer than its colour"
    );
}

/// The bench (prints the tables; copy them into the report).
#[test]
#[ignore = "bench: prints numbers; run on the GPU being measured"]
fn unit_budget_bench() {
    let device = device();
    assert!(
        device.capabilities.timestamp_query,
        "timestamp queries are required ({})",
        device.capabilities.adapter_name
    );
    let classes: Vec<ClassKey> = {
        let names = env_list::<String>("UNIT_BUDGET_CLASSES", Vec::new());
        ClassKey::all()
            .into_iter()
            .filter(|c| names.is_empty() || names.iter().any(|n| n == c.name()))
            .collect()
    };
    let resolutions: Vec<(u32, u32)> = env_list::<u32>("UNIT_BUDGET_RES", vec![720, 1080, 1440])
        .into_iter()
        .map(|h| (h * 16 / 9, h))
        .collect();
    let counts = env_list::<usize>("UNIT_BUDGET_N", vec![1, 5, 10, 20, 40]);
    let layouts: Vec<Layout> = {
        let names = env_list::<String>("UNIT_BUDGET_LAYOUTS", Vec::new());
        Layout::ALL
            .into_iter()
            .filter(|l| names.is_empty() || names.iter().any(|n| n == l.key()))
            .collect()
    };
    let warm = env_usize("UNIT_BUDGET_WARM", 40);
    let frames = env_usize("UNIT_BUDGET_FRAMES", 60);
    let texture = env_usize("UNIT_BUDGET_TEXTURE", 2048) as u32;
    let dir = tmp_dir("unit_budget_bench");
    let adapter = device.adapter.get_info();
    let mut out = format!(
        "# Unit budget bench: {} ({:?}, driver {} {})\n\nwarm {warm} frames, measured {frames}, maps {texture}², fov_y {FOV_Y} rad, Desktop High (mesh LOD bias {}), units {}\n\n## Cooked classes\n\n",
        adapter.name,
        adapter.backend,
        adapter.driver,
        adapter.driver_info,
        RenderSettings::from_profile(RenderProfilePreset::DesktopHigh)
            .profile
            .mesh_lod_bias,
        if regia_core::brand::env_var_os("UNIT_BUDGET_STATIC").is_some() {
            "static"
        } else {
            "moving"
        }
    );
    print!("{out}");
    let mut cooked: BTreeMap<ClassKey, ClassAssets> = BTreeMap::new();
    let needs_skimmer = classes.contains(&ClassKey::Cockpit);
    for key in ClassKey::all() {
        if !classes.contains(&key) && !(needs_skimmer && key == ClassKey::Skimmer) {
            continue;
        }
        let assets =
            cook_assets(class_spec(key, table_lod0(key)), texture, &dir).unwrap_or_else(|e| panic!("{e}"));
        let line = assets.summary();
        println!("{line}");
        let _ = writeln!(out, "- {line}");
        for &(_, h) in &resolutions {
            let d: Vec<String> = assets
                .switch_distances(h)
                .iter()
                .map(|d| format!("{d:.0} m"))
                .collect();
            if !d.is_empty() {
                let line = format!("  - {h}p body switch distances LOD0→1→2→3: {}", d.join(", "));
                println!("{line}");
                let _ = writeln!(out, "{line}");
            }
        }
        cooked.insert(key, assets);
    }
    let tiers = tier_table(&cooked, &resolutions);
    println!("\n{tiers}");
    let _ = writeln!(out, "\n## Body switch distances per tier\n\n{tiers}");
    let mut rows: Vec<Row> = Vec::new();
    let _ = writeln!(out, "\n## Cases\n\n{ROW_HEADER}");
    println!("{ROW_HEADER}");
    for &(width, height) in &resolutions {
        let surface = target(&device, width, height);
        for &key in &classes {
            let Some(assets) = cooked.get(&key) else {
                continue;
            };
            let mut cases = Vec::new();
            match key {
                ClassKey::Interior => cases.push(Case {
                    n: 1,
                    layout: Layout::Lod0,
                    glass: true,
                    path: RenderPath::Forward,
                    cockpit: false,
                }),
                ClassKey::Cockpit => {
                    for n in [0, 10] {
                        for glass in [true, false] {
                            cases.push(Case {
                                n,
                                layout: Layout::Mix,
                                glass,
                                path: RenderPath::Forward,
                                cockpit: true,
                            });
                        }
                    }
                }
                _ => {
                    let mut paths = vec![RenderPath::Forward];
                    if key == ClassKey::Walker {
                        paths.extend([
                            RenderPath::VisBuffer,
                            RenderPath::SkinnedVertex,
                            RenderPath::SkinnedCompute,
                        ]);
                    }
                    for &path in &paths {
                        for &n in &counts {
                            for &layout in &layouts {
                                for glass in [true, false] {
                                    // The glass split is measured on the rigid forward path.
                                    if !glass && path != RenderPath::Forward {
                                        continue;
                                    }
                                    // A skinned mesh draws LOD0 whatever the layout; pinning
                                    // levels and tier scales are rigid-path questions.
                                    if matches!(layout, Layout::Table | Layout::Close20 | Layout::Ultra)
                                        && matches!(
                                            path,
                                            RenderPath::SkinnedVertex | RenderPath::SkinnedCompute
                                        )
                                    {
                                        continue;
                                    }
                                    cases.push(Case {
                                        n,
                                        layout,
                                        glass,
                                        path,
                                        cockpit: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            // One renderer per class and resolution: VRAM is this class's alone.
            let mut renderer = FrameRenderer::new();
            renderer.set_gpu_timing(&device, true);
            let others = (key == ClassKey::Cockpit)
                .then(|| cooked.get(&ClassKey::Skimmer))
                .flatten();
            for case in cases {
                match measure(
                    &device,
                    &mut renderer,
                    &surface,
                    assets,
                    others,
                    case,
                    warm,
                    frames,
                ) {
                    Ok(row) => {
                        let line = row_line(&row);
                        println!("{line}");
                        let _ = writeln!(out, "{line}");
                        rows.push(row);
                    }
                    Err(err) => {
                        println!("{} {case:?}: {err}", key.name());
                        let _ = writeln!(out, "| {width}x{height} | {} | error: {err} |", key.name());
                    }
                }
            }
        }
    }
    let fit_table = fits(&rows);
    println!("\n{fit_table}");
    let _ = writeln!(out, "\n## Fitted cost per path\n\n{fit_table}");
    let path = regia_core::brand::env_var("UNIT_BUDGET_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dir.join("unit_budget.md"));
    match std::fs::write(&path, &out) {
        Ok(()) => println!("wrote {}", path.display()),
        Err(err) => println!("could not write {}: {err}", path.display()),
    }
}

/// Depth precision at the cockpit's near plane (0.05 m): an unlit green panel `gap` in front of
/// an unlit red wall at `distance`, square to the view and at 70°. Prints the share of panel
/// pixels where the wall wins (z-fighting). Reports; does not assert.
#[test]
#[ignore = "bench: prints numbers"]
fn unit_budget_depth_precision() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    let size = 256u32;
    let quad = |half: f32, z: f32| -> (Vec<LabVertex>, Vec<u32>) {
        let n = [0.0, 0.0, 1.0];
        let v = |x: f32, y: f32| LabVertex::from_pos_normal_uv([x, y, z], n, [0.5 + x, 0.5 + y]);
        (
            vec![v(-half, -half), v(half, -half), v(half, half), v(-half, half)],
            vec![0, 1, 2, 0, 2, 3],
        )
    };
    let unlit = |rgb: [f32; 3]| LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    let post = PostSettings {
        exposure_ev: 0.0,
        ..PostSettings::golden_deterministic()
    };
    println!("| distance m | angle ° | gap mm | panel px | wall wins | share |\n|---|---|---|---|---|---|");
    for distance in [0.5f32, 2.0, 10.0, 50.0] {
        for angle in [0.0f32, 70.0] {
            for gap_mm in [0.05f32, 0.1, 0.25, 0.5, 1.0, 2.0] {
                let gap = gap_mm * 1e-3;
                let model = Mat4::from_translation(Vec3::new(0.0, 0.0, -distance))
                    * Mat4::from_rotation_y(angle.to_radians());
                let mesh = |(vertices, indices): (Vec<LabVertex>, Vec<u32>), material: usize| LabMesh {
                    vertices,
                    indices,
                    material,
                    model: model.to_cols_array_2d(),
                    ..LabMesh::unskinned()
                };
                let scene = |with_wall: bool| LabScene {
                    width: size,
                    height: size,
                    eye: [0.0, 0.0, 0.0],
                    target: [0.0, 0.0, -1.0],
                    fov_y: FOV_Y,
                    near: 0.05,
                    far: 20_000.0,
                    depth_prepass: true,
                    directional_shadows: false,
                    ibl: false,
                    exposure_ev: 0.0,
                    ambient_lux: 0.0,
                    double_sided: true,
                    post: Some(post.clone()),
                    directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
                    local_lights: Vec::new(),
                    materials: vec![unlit([1.0, 0.0, 0.0]), unlit([0.0, 1.0, 0.0])],
                    meshes: if with_wall {
                        vec![
                            mesh(quad(distance * 0.6, 0.0), 0),
                            mesh(quad(distance * 0.3, gap), 1),
                        ]
                    } else {
                        vec![mesh(quad(distance * 0.3, gap), 1)]
                    },
                };
                let render = |renderer: &mut FrameRenderer, lab: &LabScene| -> Vec<[u8; 4]> {
                    let mut frame = lab.as_frame_scene_with_quality(FrameQuality::LAB);
                    frame.post = Some(&post);
                    let image = renderer
                        .render_offscreen(&device, &frame)
                        .unwrap_or_else(|e| panic!("depth frame: {e}"));
                    image.pixels.as_chunks::<4>().0.to_vec()
                };
                let alone = render(&mut renderer, &scene(false));
                let both = render(&mut renderer, &scene(true));
                let green = |p: &[u8; 4]| i32::from(p[1]) > i32::from(p[0]) + 30;
                // Panel pixels away from its edges (two pixels in from any non-panel pixel).
                let s = size as usize;
                let mut panel = 0usize;
                let mut wall_wins = 0usize;
                for y in 2..s - 2 {
                    for x in 2..s - 2 {
                        let inside =
                            (y - 2..=y + 2).all(|yy| (x - 2..=x + 2).all(|xx| green(&alone[yy * s + xx])));
                        if !inside {
                            continue;
                        }
                        panel += 1;
                        let p = both[y * s + x];
                        if i32::from(p[0]) > i32::from(p[1]) + 30 {
                            wall_wins += 1;
                        }
                    }
                }
                println!(
                    "| {distance} | {angle} | {gap_mm} | {panel} | {wall_wins} | {:.3} |",
                    wall_wins as f32 / panel.max(1) as f32
                );
            }
        }
    }
}
