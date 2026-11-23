// SPDX-License-Identifier: MIT-0
//! A match-like scene for the chase bench (`do27_chase --match`): DO27's units, parked vehicles,
//! buildings and props from the content mirror placed along the Switchbacks route at a live
//! match's counts, with local lights, VFX emitters and decals (DO27 tester item gpu-cost-midhigh).
//!
//! The census chase (`do27_chase --census`) draws the map, its terrain, the scatter rocks and the
//! set pieces: no units, no local lights, no glass. A tester's live match drew 2,600-4,100
//! objects out of 3,700-5,400 submitted mesh instances, with 24 local lights (up to 32) and
//! 730-1,020 shadow casters, and its GPU time hardly moved when the draws fell tenfold: the
//! cost is per pixel. This module lays out such a frame:
//!
//! - [`catalog`] picks each asset's LOD0 parts from the mirror (`units/`, `vehicles/`,
//!   `buildings/`, `props/`), one deterministic file list per asset ([`pick_files`]);
//! - [`place`] scatters objects of each [`MatchKind`] along the route around the camera path,
//!   seeded ([`MatchPlan::seed`]), until the mesh-part instances reach
//!   [`MatchPlan::part_instances`]; units drive small circles, the rest stand still;
//! - it adds [`MatchPlan::lights`] local lights (points and spots alternating, every other one
//!   shadowed by default: [`MatchPlan::shadowed_lights`]), VFX emitter sites and a decal grid;
//! - [`MatchLayout::instance_hash`] hashes the instance list of a frame, so two runs can be
//!   checked to draw the same scene.
//!
//! Shadow casters: DO27 casts from its near objects only (730-1,020 of 3,700-5,400). The bench
//! does the same: an instance whose origin lies within [`MatchPlan::caster_range_m`] of the eye
//! casts, the rest do not ([`MatchLayout::casts`]).
//!
//! Everything here is CPU-only and needs no content: the chase loads the assets and turns the
//! layout into frame meshes.

use std::path::{Path, PathBuf};

use regia_core::glam::{Mat4, Vec3};
use regia_render::{LabLocalLight, LabPointLight, LabSpotLight};

/// What an asset is in a match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MatchKind {
    /// A base building (`buildings/`): placed 20-110 m off the road.
    Building,
    /// A ground unit (`units/`): placed 7-70 m off the road, drives a small circle.
    Unit,
    /// A parked vehicle (`vehicles/`, with its glass canopy): 7-45 m off the road.
    Vehicle,
    /// A prop (`props/`, the `v1` variant): 6-80 m off the road.
    Prop,
}

impl MatchKind {
    /// Every kind, in placement order.
    pub const ALL: [Self; 4] = [Self::Building, Self::Unit, Self::Vehicle, Self::Prop];

    /// Share of [`MatchPlan::part_instances`] this kind takes (renormalised over the kinds
    /// that have assets).
    #[must_use]
    pub const fn share(self) -> f32 {
        match self {
            Self::Building => 0.12,
            Self::Unit => 0.55,
            Self::Vehicle => 0.08,
            Self::Prop => 0.25,
        }
    }

    /// Lateral offset from the road centreline, metres (`[min, max]`, before the asset's
    /// own radius is added to keep the road clear).
    #[must_use]
    pub const fn lateral_m(self) -> [f32; 2] {
        match self {
            Self::Building => [20.0, 110.0],
            Self::Unit => [7.0, 70.0],
            Self::Vehicle => [7.0, 45.0],
            Self::Prop => [6.0, 80.0],
        }
    }

    /// Short lowercase name for reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Building => "buildings",
            Self::Unit => "units",
            Self::Vehicle => "vehicles",
            Self::Prop => "props",
        }
    }
}

/// One asset of the match: its parts' glTF files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchAsset {
    /// Directory name under its kind (`sentry`, `foundry`, `bore-light-tower`, ...).
    pub name: String,
    /// Its kind.
    pub kind: MatchKind,
    /// Its parts, sorted.
    pub files: Vec<PathBuf>,
    /// Its lower LODs ([`LOD_FROM_M`]): per level 1 and 2, every part's `_LOD1` / `_LOD2`
    /// sibling (the part itself where it has none). Empty when no part has a sibling.
    pub lods: Vec<Vec<PathBuf>>,
}

/// An object farther than these lateral distances from the road (beyond its own radius) draws
/// its LOD1 / LOD2 parts, where the asset has them: the camera drives the road, so the lateral
/// distance is about the viewing distance. DO27 switches LODs by screen size; these distances
/// keep near objects at LOD0, as a chase camera sees them.
pub const LOD_FROM_M: [f32; 2] = [35.0, 80.0];

/// The `_LOD1` / `_LOD2` siblings of `files` (see [`MatchAsset::lods`]).
#[must_use]
pub fn lod_files(files: &[PathBuf]) -> Vec<Vec<PathBuf>> {
    let mut any = false;
    let levels: Vec<Vec<PathBuf>> = ["_LOD1", "_LOD2"]
        .iter()
        .map(|suffix| {
            files
                .iter()
                .map(|f| {
                    let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                    let Some(base) = stem.strip_suffix("_LOD0") else {
                        return f.clone();
                    };
                    let sibling = f.with_file_name(format!("{base}{suffix}.gltf"));
                    if sibling.is_file() {
                        any = true;
                        sibling
                    } else {
                        f.clone()
                    }
                })
                .collect()
        })
        .collect();
    if any { levels } else { Vec::new() }
}

/// The vehicles a match parks: the two the hero catalog loads as the game ships them
/// ([`crate::hero::catalog`]: the X-11 with its canopy and cockpit, the Remora).
pub const VEHICLES: [&str; 2] = ["x11", "remora"];

/// `true` for a glTF stem that is a lower LOD (`_LOD1`..`_LOD9`), a damage stage (`_D1`..),
/// a second export (`_00b`) or a fastener sheet.
fn skip_stem(stem: &str) -> bool {
    let lower = stem.to_ascii_lowercase();
    if lower.ends_with("_00b") || lower.contains("zfasteners") {
        return true;
    }
    lower.split('_').any(|seg| {
        let lod = seg
            .strip_prefix("lod")
            .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
        let dmg = seg
            .strip_prefix('d')
            .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
        lod.is_some_and(|d| d != "0") || dmg.is_some()
    })
}

/// The parts of one asset directory a match draws, sorted:
///
/// - `*.gltf` in the directory, or in its `deployed/` (buildings and units that deploy) or
///   `v1/` (props) subdirectory when that exists; never `damage/`, `wreck/`, `mobile/`, `tex/`;
/// - no lower LOD, damage stage, second export or fastener sheet (`skip_stem`);
/// - `X.gltf` dropped when `X_LOD0.gltf` is there too (the same mesh twice);
/// - in a Rem directory (`foundry`, not `foundry_a52`), no `*_A52*` file (the other faction's
///   copy that some directories carry).
#[must_use]
pub fn pick_files(dir: &Path) -> Vec<PathBuf> {
    let deployed = dir.join("deployed");
    let v1 = dir.join("v1");
    let root = if deployed.is_dir() {
        deployed
    } else if v1.is_dir() {
        v1
    } else {
        dir.to_path_buf()
    };
    let rem = !dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with("_a52"));
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let stems: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "gltf"))
        .filter_map(|p| Some((p.file_stem()?.to_str()?.to_owned(), p.clone())))
        .collect();
    let mut out: Vec<PathBuf> = stems
        .iter()
        .filter(|(stem, _)| !skip_stem(stem))
        .filter(|(stem, _)| !(rem && stem.to_ascii_lowercase().contains("_a52")))
        .filter(|(stem, _)| {
            let lod0 = format!("{stem}_LOD0");
            !stems.iter().any(|(other, _)| other.eq_ignore_ascii_case(&lod0))
        })
        .map(|(_, p)| p.clone())
        .collect();
    out.sort();
    out
}

/// The match's assets in a DO27 content drop, sorted by kind then name: Rem units and buildings
/// (the `_a52` directories and `arsenal_pods` are left out: pods are a unit's loadout, and the
/// A52 directories reuse the Rem parts), [`VEHICLES`] as [`crate::hero::catalog`] lists them,
/// and every prop's `v1`. Assets without a part are dropped.
#[must_use]
pub fn catalog(content: &Path) -> Vec<MatchAsset> {
    let mut out = Vec::new();
    for (kind, sub) in [
        (MatchKind::Building, "buildings"),
        (MatchKind::Unit, "units"),
        (MatchKind::Prop, "props"),
    ] {
        let Ok(entries) = std::fs::read_dir(content.join(sub)) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if name.to_ascii_lowercase().ends_with("_a52") || name == "arsenal_pods" {
                continue;
            }
            let files = pick_files(&dir);
            if !files.is_empty() {
                let lods = lod_files(&files);
                out.push(MatchAsset {
                    name,
                    kind,
                    files,
                    lods,
                });
            }
        }
    }
    let heroes = crate::hero::catalog(content);
    for name in VEHICLES {
        if let Some(asset) = heroes.iter().find(|a| a.name == name) {
            out.push(MatchAsset {
                name: name.to_owned(),
                kind: MatchKind::Vehicle,
                files: asset.files.clone(),
                lods: Vec::new(),
            });
        }
    }
    out.sort_by(|a, b| (kind_rank(a.kind), &a.name).cmp(&(kind_rank(b.kind), &b.name)));
    out
}

fn kind_rank(kind: MatchKind) -> usize {
    MatchKind::ALL
        .iter()
        .position(|&k| k == kind)
        .unwrap_or(usize::MAX)
}

/// How many of what, and where (`do27_chase --match`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatchPlan {
    /// Placement seed.
    pub seed: u64,
    /// Mesh-part instances the match objects add (the rocks and set pieces come on top).
    pub part_instances: u32,
    /// Local lights.
    pub lights: u32,
    /// Of [`Self::lights`], how many render into the shadow atlas (spread evenly; the profile's
    /// `local_shadow_max_lights` caps it in the frame: 32 at Desktop High).
    pub shadowed_lights: u32,
    /// VFX emitter sites (each spawns a showcase effect and a scorch decal every
    /// [`Self::emit_every`] frames).
    pub emitters: u32,
    /// Frames between two spawns of one emitter.
    pub emit_every: u32,
    /// The permanent decal grid at the base centre, `n` x `n` (0 = none).
    pub decal_grid: u32,
    /// Instances whose origin lies within this distance of the eye cast sun and local shadows.
    pub caster_range_m: f32,
    /// The objects stand along the route from `start_m + from_m` to `start_m + to_m`.
    pub from_m: f32,
    /// See [`Self::from_m`].
    pub to_m: f32,
}

impl MatchPlan {
    /// Tester A's live match (gpu-cost-midhigh, A2 frame summaries): about 3,500 drawn of
    /// 5,000 submitted (the rocks and set pieces add about 1,200 to the parts placed here),
    /// 24 local lights with every other one shadowed, about 900 casters, a few emitters and a
    /// 6 x 6 decal grid.
    pub const TESTER_A: Self = Self {
        seed: 0x0D27_5EED,
        part_instances: 3_800,
        lights: 24,
        shadowed_lights: 12,
        emitters: 4,
        emit_every: 90,
        decal_grid: 6,
        caster_range_m: 45.0,
        from_m: -40.0,
        to_m: 300.0,
    };
}

impl Default for MatchPlan {
    fn default() -> Self {
        Self::TESTER_A
    }
}

/// One loaded asset as the placement sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Proto {
    /// Its kind.
    pub kind: MatchKind,
    /// Mesh parts (frame meshes per instance).
    pub parts: u32,
    /// Footprint radius on the ground, metres.
    pub radius: f32,
    /// The protos of its LOD1 and LOD2 ([`LOD_FROM_M`]; `None` draws this one).
    pub lods: [Option<u32>; 2],
    /// A lower LOD of another proto: never picked by [`place`] itself.
    pub lod_only: bool,
    /// Triangles over its parts (0 when unknown).
    pub triangles: u64,
}

impl Proto {
    /// A proto without lower LODs.
    #[must_use]
    pub const fn new(kind: MatchKind, parts: u32, radius: f32) -> Self {
        Self {
            kind,
            parts,
            radius,
            lods: [None, None],
            lod_only: false,
            triangles: 0,
        }
    }
}

/// One placed object.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatchObject {
    /// Index into the protos given to [`place`].
    pub proto: u32,
    /// Ground position (y on the terrain).
    pub at: [f32; 3],
    /// Heading, radians about +Y.
    pub yaw: f32,
    /// Units drive a circle of [`UNIT_CIRCLE_M`] around `at`; the rest stand still.
    pub moving: bool,
}

/// Radius of a unit's circle, metres.
pub const UNIT_CIRCLE_M: f32 = 4.0;
/// A unit's angular speed on its circle, radians per frame (about 12.6 s a lap at 60 Hz).
pub const UNIT_RAD_PER_FRAME: f32 = 0.5 / 60.0;

/// A placed match: objects, lights, emitter sites and the decal grid centre.
#[derive(Clone, Debug)]
pub struct MatchLayout {
    /// The objects, in placement order.
    pub objects: Vec<MatchObject>,
    /// The local lights.
    pub lights: Vec<LabLocalLight>,
    /// VFX emitter sites (XZ).
    pub emitters: Vec<[f32; 2]>,
    /// Centre of the decal grid (XZ).
    pub decal_centre: [f32; 2],
    /// [`MatchPlan::caster_range_m`].
    pub caster_range_m: f32,
}

/// SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number generators",
/// OOPSLA 2014): a seeded, platform-independent stream.
#[derive(Clone, Copy, Debug)]
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)` from the top 24 bits.
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Lays out a match along `route` (XZ polyline, the chase's road) from `start_m`, with
/// `height(x, z)` the ground. `protos` are the loaded assets ([`Proto`]); kinds without one
/// give their share to the others. Deterministic: the same inputs give the same layout.
#[must_use]
pub fn place(
    plan: &MatchPlan,
    protos: &[Proto],
    route: &[[f32; 2]],
    start_m: f32,
    height: &dyn Fn(f32, f32) -> f32,
) -> MatchLayout {
    let mut rng = SplitMix(plan.seed);
    let present: Vec<MatchKind> = MatchKind::ALL
        .into_iter()
        .filter(|&k| protos.iter().any(|p| p.kind == k && p.parts > 0 && !p.lod_only))
        .collect();
    let share_sum: f32 = present.iter().map(|k| k.share()).sum();
    let (from, to) = (start_m + plan.from_m, start_m + plan.to_m);
    let mut objects: Vec<MatchObject> = Vec::new();
    // Footprints placed so far (x, z, radius), for a few retries against overlaps.
    let mut taken: Vec<(f32, f32, f32)> = Vec::new();
    for kind in present.iter().copied() {
        let of_kind: Vec<u32> = protos
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == kind && p.parts > 0 && !p.lod_only)
            .map(|(i, _)| i as u32)
            .collect();
        let budget = (plan.part_instances as f32 * kind.share() / share_sum.max(1e-6)).round() as u32;
        let mut used = 0u32;
        while used < budget {
            let proto_ix = of_kind[rng.below(of_kind.len())];
            let proto = protos[proto_ix as usize];
            let [lo, hi] = kind.lateral_m();
            let mut spot = (0.0, 0.0);
            let mut lateral = 0.0f32;
            for _attempt in 0..8 {
                let s = rng.range(from, to);
                let side = if rng.next() & 1 == 0 { 1.0 } else { -1.0 };
                lateral = rng.range(lo, hi);
                let off = side * (lateral + proto.radius);
                let (p, dir) = crate::soak::along(route, s);
                spot = (p[0] - dir[1] * off, p[1] + dir[0] * off);
                let clear = taken
                    .iter()
                    .all(|&(x, z, r)| (x - spot.0).hypot(z - spot.1) > (r + proto.radius) * 0.9);
                if clear {
                    break;
                }
            }
            let yaw = rng.range(0.0, std::f32::consts::TAU);
            taken.push((spot.0, spot.1, proto.radius));
            let drawn_ix = match LOD_FROM_M.iter().rposition(|&m| lateral > m) {
                Some(level) => proto.lods[level].unwrap_or(proto_ix),
                None => proto_ix,
            };
            objects.push(MatchObject {
                proto: drawn_ix,
                at: [spot.0, height(spot.0, spot.1), spot.1],
                yaw,
                moving: kind == MatchKind::Unit,
            });
            used += protos.get(drawn_ix as usize).map_or(proto.parts, |p| p.parts);
        }
    }
    let lights = (0..plan.lights)
        .map(|k| {
            let s = from + 30.0 + (k as f32 + 0.5) / plan.lights.max(1) as f32 * (to - from - 30.0);
            let side = if k % 2 == 0 { 1.0 } else { -1.0 };
            let off = side * rng.range(10.0, 30.0);
            let (p, dir) = crate::soak::along(route, s);
            let (x, z) = (p[0] - dir[1] * off, p[1] + dir[0] * off);
            let position = [x, height(x, z) + 6.0, z];
            // Spread the shadowed ones evenly over the list.
            let cast_shadows = (u64::from(k) * u64::from(plan.shadowed_lights))
                % u64::from(plan.lights.max(1))
                < u64::from(plan.shadowed_lights);
            let color = [1.0, 0.86, 0.68];
            if k % 2 == 0 {
                LabLocalLight::Point(LabPointLight {
                    position,
                    color,
                    intensity: 6_000.0,
                    radius: 20.0,
                    cast_shadows,
                    source_radius: None,
                })
            } else {
                // A floodlight aimed at the road centre below.
                let road = Vec3::new(p[0], height(p[0], p[1]), p[1]);
                let direction = (road - Vec3::from_array(position))
                    .normalize_or(Vec3::NEG_Y)
                    .to_array();
                LabLocalLight::Spot(LabSpotLight {
                    position,
                    direction,
                    color,
                    intensity: 9_000.0,
                    range: 35.0,
                    inner_angle: 0.35,
                    outer_angle: 0.6,
                    cast_shadows,
                    source_radius: None,
                })
            }
        })
        .collect();
    let emitters = (0..plan.emitters)
        .map(|k| {
            let s = start_m + 20.0 + k as f32 * 60.0;
            let (p, dir) = crate::soak::along(route, s);
            let off = if k % 2 == 0 { 12.0 } else { -12.0 };
            [p[0] - dir[1] * off, p[1] + dir[0] * off]
        })
        .collect();
    let (centre, _) = crate::soak::along(route, start_m + 30.0);
    MatchLayout {
        objects,
        lights,
        emitters,
        decal_centre: centre,
        caster_range_m: plan.caster_range_m,
    }
}

impl MatchLayout {
    /// Mesh-part instances one frame submits (`sum of parts over objects`).
    #[must_use]
    pub fn part_instances(&self, protos: &[Proto]) -> u32 {
        self.objects
            .iter()
            .map(|o| protos.get(o.proto as usize).map_or(0, |p| p.parts))
            .sum()
    }

    /// Objects per kind, in [`MatchKind::ALL`] order.
    #[must_use]
    pub fn objects_per_kind(&self, protos: &[Proto]) -> [u32; 4] {
        let mut out = [0u32; 4];
        for o in &self.objects {
            if let Some(p) = protos.get(o.proto as usize) {
                out[kind_rank(p.kind)] += 1;
            }
        }
        out
    }

    /// Lights that render into the shadow atlas.
    #[must_use]
    pub fn shadowed_lights(&self) -> u32 {
        self.lights
            .iter()
            .filter(|l| match l {
                LabLocalLight::Point(p) => p.cast_shadows,
                LabLocalLight::Spot(s) => s.cast_shadows,
            })
            .count() as u32
    }

    /// The model matrix of `object` at frame `frame` (units on their circle).
    #[must_use]
    pub fn model(&self, object: &MatchObject, frame: u32) -> Mat4 {
        let base = Vec3::from_array(object.at);
        if object.moving {
            let a = object.yaw + frame as f32 * UNIT_RAD_PER_FRAME;
            let at = base + Vec3::new(a.cos(), 0.0, a.sin()) * UNIT_CIRCLE_M;
            // Facing along the circle's tangent.
            Mat4::from_translation(at) * Mat4::from_rotation_y(-a - std::f32::consts::FRAC_PI_2)
        } else {
            Mat4::from_translation(base) * Mat4::from_rotation_y(object.yaw)
        }
    }

    /// `true` when an instance at `at` casts shadows for an eye at `eye`
    /// ([`MatchPlan::caster_range_m`]).
    #[must_use]
    pub fn casts(&self, at: Vec3, eye: Vec3) -> bool {
        at.distance_squared(eye) <= self.caster_range_m * self.caster_range_m
    }

    /// A stable non-zero object key for part `part` of object `index` (TAA motion vectors and the
    /// shadow caches follow it).
    #[must_use]
    pub fn object_key(index: usize, part: u32) -> u64 {
        let mut h = Fnv::new();
        h.u64(0x4D41_5443_4800_0000);
        h.u64(index as u64);
        h.u64(u64::from(part));
        h.0 | 1
    }

    /// FNV-1a 64 over frame `frame`'s instance list: per object its proto, part count and model
    /// matrix bits, then every light. Two runs of one plan on one route give one hash.
    #[must_use]
    pub fn instance_hash(&self, protos: &[Proto], frame: u32) -> u64 {
        let mut h = Fnv::new();
        h.u64(self.objects.len() as u64);
        for o in &self.objects {
            h.u64(u64::from(o.proto));
            h.u64(u64::from(protos.get(o.proto as usize).map_or(0, |p| p.parts)));
            for v in self.model(o, frame).to_cols_array() {
                h.u64(u64::from(v.to_bits()));
            }
        }
        h.u64(self.lights.len() as u64);
        for light in &self.lights {
            let (pos, cast) = match light {
                LabLocalLight::Point(p) => (p.position, p.cast_shadows),
                LabLocalLight::Spot(s) => (s.position, s.cast_shadows),
            };
            for v in pos {
                h.u64(u64::from(v.to_bits()));
            }
            h.u64(u64::from(cast));
        }
        h.0
    }
}

/// FNV-1a 64 (Fowler, Noll, Vo; public domain).
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xCBF2_9CE4_8422_2325)
    }

    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

/// `do27_chase --match`'s switches ([`crate::chase::ChaseOptions::match_scene`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatchOptions {
    /// Counts and placement.
    pub plan: MatchPlan,
    /// Texture edge cap when the assets load (texels a side).
    pub max_texture: u32,
    /// Draw the match's assets with [`flat_material`]s (`--match-flat`).
    pub flat: bool,
    /// Draw each part as its own bounding box (`--match-proxy`, [`proxy_part`]): the same caster
    /// bounds, poses and counts, so the same shadow-cache decisions, at a geometry cost a
    /// software rasteriser can carry (ADR-7986).
    pub proxy: bool,
}

/// `mesh` replaced by the box of its vertices' bounds (12 triangles), keeping its material and
/// model. The renderer's mesh bounds are the vertices' box centre and half-diagonal, so the box
/// has the same bounds as the part, and every shadow-cache decision (which pages and tiles a
/// caster covers, when it moved) is the part's.
#[must_use]
pub fn proxy_part(mesh: &regia_render::LabMesh) -> regia_render::LabMesh {
    let Some(first) = mesh.vertices.first() else {
        return mesh.clone();
    };
    let (mut lo, mut hi) = (Vec3::from_array(first.position), Vec3::from_array(first.position));
    for v in &mesh.vertices {
        lo = lo.min(Vec3::from_array(v.position));
        hi = hi.max(Vec3::from_array(v.position));
    }
    let centre = (lo + hi) * 0.5;
    let (mut vertices, indices) = regia_render::box_mesh(((hi - lo) * 0.5).to_array());
    for v in &mut vertices {
        v.position = (Vec3::from_array(v.position) + centre).to_array();
    }
    regia_render::LabMesh {
        vertices,
        indices,
        ..mesh.clone()
    }
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            plan: MatchPlan::TESTER_A,
            max_texture: 1024,
            flat: false,
            proxy: false,
        }
    }
}

/// The match's assets loaded: per proto its meshes (centred on the ground at the origin,
/// material indices into [`Self::materials`]).
pub struct LoadedMatch {
    /// One [`Proto`] per asset, in [`catalog`] order.
    pub protos: Vec<Proto>,
    /// Per proto, its parts.
    pub meshes: Vec<Vec<regia_render::LabMesh>>,
    /// Every proto's materials, one list.
    pub materials: Vec<regia_render::LabMaterial>,
    /// Asset names, per proto.
    pub names: Vec<String>,
}

/// The last [`load`] of this process: content directory, texture cap, result.
type LoadCache = Option<(PathBuf, u32, std::sync::Arc<LoadedMatch>)>;
static LOAD_CACHE: std::sync::Mutex<LoadCache> = std::sync::Mutex::new(None);

/// Loads [`catalog`]'s assets (LOD0 parts and their [`MatchAsset::lods`], textures at most
/// `options.max_texture` a side), each centred on the origin with its lowest point on the
/// ground (a lower LOD takes its LOD0's offset). Files load on up to eight threads; the result
/// keeps [`catalog`] order, each asset's LOD0 proto followed by its LOD protos. An asset whose
/// LOD0 fails to load is left out with a warning. The result is kept for the next call with the
/// same content and texture cap (a lever bench runs the match many times in one process).
///
/// # Errors
///
/// The content has no match asset at all.
pub fn load(
    content: &Path,
    options: &MatchOptions,
) -> Result<std::sync::Arc<LoadedMatch>, crate::LookdevError> {
    if !options.proxy
        && let Ok(cache) = LOAD_CACHE.lock()
        && let Some((dir, cap, loaded)) = cache.as_ref()
        && dir == content
        && *cap == options.max_texture
    {
        return Ok(std::sync::Arc::clone(loaded));
    }
    let assets = catalog(content);
    if assets.is_empty() {
        return Err(crate::LookdevError::Config(format!(
            "--match: no units, vehicles, buildings or props under {}",
            content.display()
        )));
    }
    let _span = tracing::info_span!("match_load", assets = assets.len()).entered();
    // (asset, level) jobs: level 0 is the asset's files, 1 and 2 its LODs.
    let jobs: Vec<(usize, usize)> = assets
        .iter()
        .enumerate()
        .flat_map(|(i, a)| (0..=a.lods.len()).map(move |level| (i, level)))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.len().min(8) {
            scope.spawn(|| {
                loop {
                    let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(i, level)) = jobs.get(j) else {
                        break;
                    };
                    let asset = &assets[i];
                    let files = if level == 0 {
                        &asset.files
                    } else {
                        &asset.lods[level - 1]
                    };
                    let model = if level == 0 {
                        crate::hero::load_files(&asset.name, files, options.max_texture, true)
                    } else {
                        crate::hero::load_lod_files(&asset.name, files, options.max_texture, true)
                    };
                    if let Ok(mut out) = results.lock() {
                        out.push(((i, level), model));
                    }
                }
            });
        }
    });
    let mut models: std::collections::HashMap<(usize, usize), crate::hero::HeroModel> =
        std::collections::HashMap::new();
    for (key, model) in results.into_inner().unwrap_or_default() {
        match model {
            Ok(model) => {
                models.insert(key, model);
            }
            Err(err) => eprintln!(
                "  --match: {} (LOD {}) not loaded: {err}",
                assets[key.0].name, key.1
            ),
        }
    }
    let mut out = LoadedMatch {
        protos: Vec::new(),
        meshes: Vec::new(),
        materials: Vec::new(),
        names: Vec::new(),
    };
    for (i, asset) in assets.iter().enumerate() {
        let Some(lod0) = models.get(&(i, 0)).filter(|m| !m.meshes.is_empty()) else {
            continue;
        };
        let centre = (lod0.min + lod0.max) * 0.5;
        let offset = Vec3::new(-centre.x, -lod0.min.y, -centre.z);
        let half = (lod0.max - lod0.min) * 0.5;
        let radius = half.x.hypot(half.z);
        let first = out.protos.len() as u32;
        for level in 0..=asset.lods.len() {
            let Some(mut model) = models.remove(&(i, level)).filter(|m| !m.meshes.is_empty()) else {
                continue;
            };
            crate::hero::place(&mut model, 0.0, offset);
            let base = out.materials.len();
            out.materials.extend(model.materials);
            for mesh in &mut model.meshes {
                mesh.material += base;
            }
            let ix = out.protos.len() as u32;
            let mut proto = Proto::new(asset.kind, model.meshes.len() as u32, radius);
            proto.lod_only = level > 0;
            proto.triangles = model.meshes.iter().map(|m| m.indices.len() as u64 / 3).sum();
            out.protos.push(proto);
            if level > 0 {
                // Level 2 falls back to level 1 when it did not load, and the other way round.
                for slot in (level - 1)..2 {
                    if slot == level - 1 || out.protos[first as usize].lods[slot].is_none() {
                        out.protos[first as usize].lods[slot] = Some(ix);
                    }
                }
            }
            if options.proxy {
                out.meshes.push(model.meshes.iter().map(proxy_part).collect());
            } else {
                out.meshes.push(model.meshes);
            }
            out.names.push(format!(
                "{}{}",
                asset.name,
                if level > 0 {
                    format!("_lod{level}")
                } else {
                    String::new()
                }
            ));
        }
    }
    let out = std::sync::Arc::new(out);
    if !options.proxy
        && let Ok(mut cache) = LOAD_CACHE.lock()
    {
        *cache = Some((
            content.to_path_buf(),
            options.max_texture,
            std::sync::Arc::clone(&out),
        ));
    }
    Ok(out)
}

/// What a `--match` run placed, for its report line.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchReport {
    /// Assets loaded.
    pub assets: u32,
    /// Objects per kind ([`MatchKind::ALL`] order).
    pub objects_per_kind: [u32; 4],
    /// Mesh-part instances the objects submit per frame.
    pub part_instances: u32,
    /// Local lights.
    pub lights: u32,
    /// Of which shadowed.
    pub shadowed_lights: u32,
    /// VFX emitter sites.
    pub emitters: u32,
    /// Decals in the permanent grid.
    pub decals: u32,
    /// [`MatchLayout::instance_hash`] of the first frame.
    pub first_frame_hash: u64,
    /// Triangles the objects submit per frame (before any culling or LOD in the renderer).
    pub triangles: u64,
    /// Asset load time, ms.
    pub load_ms: f32,
}

impl MatchReport {
    /// The report of `layout` over `protos`.
    #[must_use]
    pub fn new(plan: &MatchPlan, layout: &MatchLayout, protos: &[Proto], load_ms: f32) -> Self {
        Self {
            assets: protos.iter().filter(|p| !p.lod_only).count() as u32,
            objects_per_kind: layout.objects_per_kind(protos),
            part_instances: layout.part_instances(protos),
            lights: layout.lights.len() as u32,
            shadowed_lights: layout.shadowed_lights(),
            emitters: layout.emitters.len() as u32,
            decals: plan.decal_grid * plan.decal_grid,
            first_frame_hash: layout.instance_hash(protos, 0),
            triangles: layout
                .objects
                .iter()
                .map(|o| protos.get(o.proto as usize).map_or(0, |p| p.triangles))
                .sum(),
            load_ms,
        }
    }

    /// One line: `match: 412 objects (...), 3811 part instances, ...`.
    #[must_use]
    pub fn line(&self) -> String {
        let k = self.objects_per_kind;
        format!(
            "match: {} assets, {} objects ({} {}, {} {}, {} {}, {} {}), {} part instances, {} local lights ({} shadowed), \
             {} emitters, {} decals, {:.1} M triangles, first-frame instance hash {:016x}, loaded in {:.1} s",
            self.assets,
            k.iter().sum::<u32>(),
            k[0],
            MatchKind::ALL[0].name(),
            k[1],
            MatchKind::ALL[1].name(),
            k[2],
            MatchKind::ALL[2].name(),
            k[3],
            MatchKind::ALL[3].name(),
            self.part_instances,
            self.lights,
            self.shadowed_lights,
            self.emitters,
            self.decals,
            self.triangles as f64 / 1e6,
            self.first_frame_hash,
            self.load_ms / 1000.0,
        )
    }
}

/// `material` with every texture and hard-surface term dropped, the factors kept (glass stays
/// glass): the `--match-flat` toggle that prices the match's texture and material work.
#[must_use]
pub fn flat_material(material: &regia_render::LabMaterial) -> regia_render::LabMaterial {
    regia_render::LabMaterial {
        weather: material.weather,
        base_color: material.base_color,
        metallic: material.metallic,
        roughness: material.roughness,
        emissive: material.emissive,
        emissive_intensity: material.emissive_intensity,
        alpha_mode: material.alpha_mode,
        alpha_cutoff: material.alpha_cutoff,
        unlit: material.unlit,
        transmission: material.transmission,
        ior: material.ior,
        transmission_tint: material.transmission_tint,
        transmission_thickness: material.transmission_thickness,
        transmission_roughness: material.transmission_roughness,
        double_sided: material.double_sided,
        ..regia_render::LabMaterial::EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic catalog: two of each kind with fixed part counts and radii.
    fn protos() -> Vec<Proto> {
        vec![
            Proto::new(MatchKind::Building, 31, 12.0),
            Proto::new(MatchKind::Building, 9, 7.0),
            Proto::new(MatchKind::Unit, 22, 3.0),
            Proto::new(MatchKind::Unit, 14, 2.5),
            Proto::new(MatchKind::Vehicle, 17, 3.5),
            Proto::new(MatchKind::Vehicle, 12, 3.0),
            Proto::new(MatchKind::Prop, 2, 1.5),
            Proto::new(MatchKind::Prop, 1, 4.0),
        ]
    }

    fn route() -> Vec<[f32; 2]> {
        // A bent road, 600 m.
        vec![[0.0, 0.0], [200.0, 0.0], [350.0, 120.0], [500.0, 160.0]]
    }

    fn ground(x: f32, z: f32) -> f32 {
        0.01 * x + 0.02 * z
    }

    #[test]
    fn a_proxy_part_keeps_the_parts_bounds() {
        let (vertices, indices) = regia_render::box_mesh([0.4, 1.0, 0.2]);
        let mut part = regia_render::LabMesh {
            vertices,
            indices,
            material: 3,
            ..regia_render::LabMesh::unskinned()
        };
        // A lopsided part: one vertex far out on +x and one below.
        part.vertices[0].position = [2.5, -0.3, 0.1];
        part.vertices[1].position = [-0.4, 1.7, -0.2];
        let bounds = |m: &regia_render::LabMesh| {
            m.vertices
                .iter()
                .fold(([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]), |(lo, hi), v| {
                    (
                        std::array::from_fn(|k| lo[k].min(v.position[k])),
                        std::array::from_fn(|k| hi[k].max(v.position[k])),
                    )
                })
        };
        let proxy = proxy_part(&part);
        assert_eq!(proxy.indices.len(), 36, "a box is 12 triangles");
        assert_eq!(proxy.material, 3);
        let ((a_lo, a_hi), (b_lo, b_hi)) = (bounds(&part), bounds(&proxy));
        for k in 0..3 {
            assert!(
                (a_lo[k] - b_lo[k]).abs() < 1e-5 && (a_hi[k] - b_hi[k]).abs() < 1e-5,
                "axis {k}"
            );
        }
    }

    #[test]
    fn match_placement_is_deterministic() {
        let plan = MatchPlan::TESTER_A;
        let protos = protos();
        let a = place(&plan, &protos, &route(), 40.0, &ground);
        let b = place(&plan, &protos, &route(), 40.0, &ground);
        assert_eq!(a.objects, b.objects, "two placements of one plan differ");
        assert_eq!(a.emitters, b.emitters);
        assert_eq!(a.decal_centre, b.decal_centre);
        assert_eq!(a.part_instances(&protos), b.part_instances(&protos));
        assert_eq!(a.lights.len(), b.lights.len());
        assert_eq!(
            a.instance_hash(&protos, 0),
            b.instance_hash(&protos, 0),
            "first-frame instance hash differs between two runs"
        );
        // A later frame moves the units, so its list hashes differently, and identically on both.
        assert_eq!(a.instance_hash(&protos, 300), b.instance_hash(&protos, 300));
        assert_ne!(a.instance_hash(&protos, 0), a.instance_hash(&protos, 300));
        let other = place(
            &MatchPlan {
                seed: plan.seed + 1,
                ..plan
            },
            &protos,
            &route(),
            40.0,
            &ground,
        );
        assert_ne!(a.instance_hash(&protos, 0), other.instance_hash(&protos, 0));
    }

    #[test]
    fn match_counts_follow_the_plan() {
        let plan = MatchPlan::TESTER_A;
        let protos = protos();
        let layout = place(&plan, &protos, &route(), 40.0, &ground);
        let parts = layout.part_instances(&protos);
        // Each kind stops at the first object past its share, so at most one object over.
        assert!(
            (plan.part_instances..plan.part_instances + 4 * 31).contains(&parts),
            "{parts} part instances for a plan of {}",
            plan.part_instances
        );
        assert_eq!(layout.lights.len(), 24);
        assert_eq!(layout.shadowed_lights(), 12);
        assert_eq!(layout.emitters.len(), 4);
        let kinds = layout.objects_per_kind(&protos);
        assert!(kinds.iter().all(|&n| n > 0), "every kind placed: {kinds:?}");
        // Units move, the rest stand.
        for o in &layout.objects {
            assert_eq!(o.moving, protos[o.proto as usize].kind == MatchKind::Unit);
            assert!((o.at[1] - ground(o.at[0], o.at[2])).abs() < 1e-4, "on the ground");
        }
        let none = place(
            &MatchPlan {
                lights: 24,
                shadowed_lights: 0,
                ..plan
            },
            &protos,
            &route(),
            40.0,
            &ground,
        );
        assert_eq!(none.shadowed_lights(), 0);
    }

    #[test]
    fn a_kind_without_assets_gives_its_share_away() {
        let plan = MatchPlan::TESTER_A;
        let protos: Vec<Proto> = protos()
            .into_iter()
            .filter(|p| p.kind != MatchKind::Vehicle)
            .collect();
        let layout = place(&plan, &protos, &route(), 0.0, &ground);
        assert!(layout.part_instances(&protos) >= plan.part_instances);
        assert_eq!(layout.objects_per_kind(&protos)[2], 0);
    }

    #[test]
    fn far_objects_draw_their_lower_lods() {
        // One building with LOD1 (proto 1) and LOD2 (proto 2), lighter each step.
        let mut lod0 = Proto::new(MatchKind::Building, 30, 10.0);
        lod0.lods = [Some(1), Some(2)];
        let mut lod1 = Proto::new(MatchKind::Building, 20, 10.0);
        lod1.lod_only = true;
        let mut lod2 = Proto::new(MatchKind::Building, 10, 10.0);
        lod2.lod_only = true;
        let protos = vec![lod0, lod1, lod2, Proto::new(MatchKind::Unit, 5, 2.0)];
        let layout = place(&MatchPlan::TESTER_A, &protos, &route(), 40.0, &ground);
        let drawn: std::collections::HashSet<u32> = layout
            .objects
            .iter()
            .filter(|o| protos[o.proto as usize].kind == MatchKind::Building)
            .map(|o| o.proto)
            .collect();
        // Buildings stand 20-110 m off the road: some near (LOD0), some past each switch.
        assert_eq!(drawn, [0, 1, 2].into_iter().collect(), "LODs drawn: {drawn:?}");
        // The lower LODs are never picked as assets of their own: every proto-1/2 object stands
        // past its switch distance.
        let again = place(&MatchPlan::TESTER_A, &protos, &route(), 40.0, &ground);
        assert_eq!(layout.objects, again.objects);
    }

    #[test]
    fn lod_files_find_siblings_or_keep_the_part() {
        let root = regia_test_utils::unique_temp_dir("match-lods");
        for rel in [
            "SM_A_LOD0.gltf",
            "SM_A_LOD1.gltf",
            "SM_A_LOD2.gltf",
            "SM_B_LOD0.gltf",
            "SM_C.gltf",
        ] {
            std::fs::write(root.join(rel), b"{}").expect("test file");
        }
        let files: Vec<PathBuf> = ["SM_A_LOD0.gltf", "SM_B_LOD0.gltf", "SM_C.gltf"]
            .iter()
            .map(|f| root.join(f))
            .collect();
        let lods = lod_files(&files);
        assert_eq!(lods.len(), 2);
        assert_eq!(
            lods[0],
            [root.join("SM_A_LOD1.gltf"), files[1].clone(), files[2].clone()]
        );
        assert_eq!(
            lods[1],
            [root.join("SM_A_LOD2.gltf"), files[1].clone(), files[2].clone()]
        );
        assert!(lod_files(&files[1..]).is_empty(), "no sibling: no LOD levels");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn object_keys_are_nonzero_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for index in 0..200 {
            for part in 0..40 {
                let key = MatchLayout::object_key(index, part);
                assert_ne!(key, 0);
                assert!(seen.insert(key), "object key collision at {index}/{part}");
            }
        }
    }

    #[test]
    fn stems_skip_lower_lods_damage_and_second_exports() {
        for skipped in [
            "SM_Pod_Ammo_A52_LOD1",
            "SM_Marv_D1",
            "SM_Marv_D2_Fan0",
            "SM_X12_Hull_00b",
            "SM_Remora_ZFasteners",
        ] {
            assert!(skip_stem(skipped), "{skipped} kept");
        }
        for kept in [
            "SM_Turret",
            "SM_Silo_Rem_LOD0",
            "SM_Scavenger_Fan0",
            "SM_FoundryDeployed_Gear",
            "SM_X11_Hull_00",
            "SM_Constructor_Door",
        ] {
            assert!(!skip_stem(kept), "{kept} skipped");
        }
    }

    #[test]
    fn pick_files_takes_lod0_deployed_and_v1() {
        let root = regia_test_utils::unique_temp_dir("match-pick");
        let touch = |rel: &str| {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("test dir");
            }
            std::fs::write(&p, b"{}").expect("test file");
        };
        // A building with a duplicate un-suffixed export and the other faction's copy.
        touch("reactor/SM_Reactor_Rem.gltf");
        touch("reactor/SM_Reactor_Rem_LOD0.gltf");
        touch("reactor/SM_Reactor_Rem_LOD1.gltf");
        touch("reactor/SM_Reactor_A52_LOD0.gltf");
        // A deploying unit.
        touch("reclaimer/deployed/SM_ReclaimerDeployed.gltf");
        touch("reclaimer/deployed/SM_ReclaimerDeployed_Glass.gltf");
        touch("reclaimer/mobile/SM_Reclaimer.gltf");
        touch("reclaimer/damage/SM_Reclaimer_D1.gltf");
        // A prop with variants.
        touch("cargo/v1/SM_Cargo.gltf");
        touch("cargo/v2/SM_Cargo.gltf");
        let names = |dir: &str| -> Vec<String> {
            pick_files(&root.join(dir))
                .iter()
                .filter_map(|p| p.strip_prefix(&root).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect()
        };
        assert_eq!(names("reactor"), ["reactor/SM_Reactor_Rem_LOD0.gltf"]);
        assert_eq!(
            names("reclaimer"),
            [
                "reclaimer/deployed/SM_ReclaimerDeployed.gltf",
                "reclaimer/deployed/SM_ReclaimerDeployed_Glass.gltf"
            ]
        );
        assert_eq!(names("cargo"), ["cargo/v1/SM_Cargo.gltf"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
