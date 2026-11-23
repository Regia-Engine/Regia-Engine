# 87 — Water finish: shore bake, shoaling, floating bodies, surface queries on every water kind

Status: proposed with ADR-1828. Extends `docs/specs/water.md` (ADR-1399, ADR-1620); the items it
closes are in that spec's § 5 ("Not yet"). Crates: `regia_terrain::water_body` (`shore`, `float`,
`query`) and `regia_framework::buoyancy`. Agent page: `book/agents/water.md` (sections "Shore" and
"Floating bodies"). Sources: public papers only (§ 11); no other engine's documents or code.

## 1. Purpose

The runtime water draws streams, lakes, the sea and each terrain's shoreline plane, with a
trochoidal swell and an exact CPU surface (ADR-1620). Three things stop gameplay from trusting it:

1. **The shore.** A plane knows nothing about the bed under it. A swell runs at full height up to
   the waterline and through the land; there is no shoreline foam on planes; a terrain's plane
   spans the whole map, so a surface query on a hilltop answers "water".
2. **Floating.** Nothing floats. Boats, crates and debris need a buoyant force from the same
   surface the frame draws.
3. **One question for every kind.** `water.surface` takes the first body in entity order, not the
   water that is actually on top, and it does not know whether the bed is above the level.

This spec adds a **shore bake** (a distance and depth field per plane body over a bed), **shoaling
and breaking** of the swell from that field, **shore foam**, **floating bodies** driven by the
water surface, and a **world surface query** that answers for every body kind (level plane, swell
plane, ribbon, terrain plane) with the bed taken into account.

## 2. Shore bake (`regia_terrain::water_body::shore`)

```rust
pub const SHORE_FORMAT_VERSION: u32 = 1;
pub const MAX_SHORE_SIZE: u32 = 1024;          // texels per side, at most
pub const MIN_SHORE_CELL: f32 = 0.25;          // m

pub struct ShoreField {                          // serde; a cache, not content (§ 2.3)
    pub format_version: u32,
    pub origin: [f32; 2],                        // world x, z of texel (0, 0)'s center
    pub cell: f32,                               // m per texel
    pub size: [u32; 2],                          // texels along x, z
    pub level: f32,                              // the water level it was baked for
    pub distance: Vec<f32>,                      // signed horizontal distance to the shoreline (m): + in water, − on land
    pub depth: Vec<f32>,                         // level − bed (m): + in water, − on land
}
pub struct ShoreSample { pub distance: f32, pub depth: f32 }
pub struct ShoreBakeDesc { pub level: f32, pub min: [f32; 2], pub max: [f32; 2], pub cell: f32 }

impl ShoreField {
    pub fn bake(desc: &ShoreBakeDesc, bed: impl Fn(f32, f32) -> f32) -> Result<Self, ShoreError>;
    pub fn bake_terrain(map: &TerrainMap, level: f32, min: [f32; 2], max: [f32; 2]) -> Result<Self, ShoreError>;
    pub fn sample(&self, x: f32, z: f32) -> Option<ShoreSample>;   // bilinear; None outside
    pub fn to_bytes(&self) -> Vec<u8>;  pub fn from_bytes(bytes: &[u8]) -> Result<Self, ShoreError>;
    pub fn stats(&self) -> ShoreStats;  // wet fraction, max depth, shoreline texels
}

#[derive(Component)] pub struct WaterShore { pub field: Arc<ShoreField> }   // on the body's entity
```

### 2.1 Algorithm

1. Sample the bed at every texel center (`origin + (i, j) · cell`); `depth = level − bed`.
2. Wet texels are `depth > 0`. Compute the exact squared Euclidean distance transform of the wet
   set and of the dry set by the separable lower-envelope-of-parabolas method (Felzenszwalb and
   Huttenlocher 2012): one 1D pass per row, then one per column, O(n) each.
3. Signed distance: a wet texel stores `+(√d_dry − cell/2)`, a dry texel `−(√d_wet − cell/2)`, so
   the zero crossing sits between the two texels that straddle the shoreline. A field with no
   shoreline stores `±(max(size[0], size[1]) · cell)` everywhere (all water or all land).
4. The texel count is `⌈(max − min) / cell⌉ + 1` per axis, at least 2; `cell` is raised (never
   lowered) until both sides fit [`MAX_SHORE_SIZE`]: first to `longest / (MAX_SHORE_SIZE − 1)`, then
   by single `f32` ulps while rounding still leaves a count over the cap, so the far edge always lies
   inside the field (ADR-5817); it is at least [`MIN_SHORE_CELL`]. `bake_terrain` uses
   `max(map spacing, MIN_SHORE_CELL)`.

The bake is a pure function of its inputs (no threads, no hashing order): the same bed and
description give the same bytes on every platform (IEEE `f32` arithmetic in a fixed order, `sqrt`
is correctly rounded).

### 2.2 Runtime

- `WaterPlugin` bakes a `WaterShore` for every terrain water body (`TerrainWater`) when the
  plane changes or the field is missing, and at most every `SHORE_REBAKE_FRAMES` (32) frames
  after the terrain was deformed (`water_shore_bake` span); with the switch off it bakes nothing. A host bakes one for
  its own plane body with `ShoreField::bake_terrain` (or `bake` with any bed function) and inserts
  `WaterShore` on the body's entity. Ribbons never use a field (their banks are their width).
- `WaterSettings::shore: bool` (default `true`, agent `water.set { shore }`) is the runtime
  switch: off, no field is used by queries, shoaling or the frame, and the terrain bakes are kept.

### 2.3 Format and migration

`ShoreField` serializes with postcard behind an 8-byte header (`b"RGSH"`, `format_version` as
`u32` little endian). It is a **derived cache**, not authored content: it is rebuilt from the
terrain and the level, never hand-edited, and it is not registered in `docs/specs/12-assets.md`
§ 4a. Migration: a reader that finds another `format_version` returns
`ShoreError::Version { found, expected }` and the caller re-bakes; no migrator is needed and
`regia upgrade-project` has nothing to do. If a later version makes the field authored content, it
gets registered and frozen then.

## 3. Shoaling, breaking and shore foam

With a field and the switch on, each swell wave of a plane body is scaled by the local depth `h`
at the rest point (bilinear from the field):

- **Shoaling** (linear theory, energy-flux conservation): `Ks = 1 / √(tanh(kh) · (1 + 2kh / sinh(2kh)))`
  with the wave's deep-water `k`, `kh` clamped to at least `1e-4`; at `kh ≥ 2π` (a wavelength of
  depth, where the factor is within 5e-5 of 1) it is taken as 1: deep water.
- **Breaking** (McCowan 1894 breaking index `γ = 0.78`): the shoaled amplitude is capped at
  `γ h / 2`; at `h ≤ 0` the wave is gone. Scale `s = clamp(min(Ks, γ h / (2 A)), 0, SHOAL_MAX)`
  with `SHOAL_MAX = 2`.
- The horizontal and vertical amplitudes take the same scale (the steepness is kept). The scale is
  treated as locally constant in the derivatives (the slowly varying bed of ray/WKB theory), so
  normals, velocity and the Jacobian keep their ADR-1620 form with scaled amplitudes; the Newton
  inverse is unchanged.
- **Breaker foam:** `breaking = smoothstep(0.7, 1.0, max over waves of (Ks A) / (γ h / 2))`.
- **Shore foam:** `shore = smoothstep(wet_width, 0, distance)` for `0 ≤ distance` (the look's
  `wet_width` is the band). A surface query reports both; the frame adds them to the whitewater
  term on High and Ultra. Low binds no field (§ 9).
- **Dry land:** a query at a point whose field depth is `≤ 0` misses the body (the bed is above the
  water there).

CPU (`swell.rs`, `shore.rs`) and GPU (`water_body.wgsl`) evaluate the same terms in the same order;
the constants are pinned by text, as for the swell.

## 4. World surface query (`regia_terrain::water_body::query`)

```rust
pub fn surface_at<'a>(bodies: impl IntoIterator<Item = BodyRef<'a>>, x: f32, z: f32, clock: &WaterClock, shore: bool)
    -> Option<(Entity, WaterSurface)>;
pub fn raycast_all<'a>(bodies: impl IntoIterator<Item = BodyRef<'a>>, origin, direction, max_distance, clock, shore)
    -> Option<(Entity, WaterHit)>;
#[derive(SystemParam)] pub struct WaterQuery<'w, 's> { … }   // .surface(x, z), .raycast(o, d, max)
```

- Every kind answers through `WaterBody::surface_on(x, z, clock, shore)`: a level plane its level,
  a swell plane the displaced surface (shoaled when a field is given), a ribbon its sloped surface,
  a terrain plane like any plane (with its baked field).
- Overlapping bodies: the **highest** wet surface wins (ties: the lower entity, in `Entity` order, which is the
  order of `to_bits()`, the id agents see; not the order of the index, ADR-5817). A hilltop under a
  terrain plane is dry (field depth ≤ 0) and answers nothing; a lake above the sea answers the lake.
- `raycast_all` returns the nearest hit over all bodies.
- `WaterSurface` gains `depth: Option<f32>` (field depth, a ribbon's authored depth), `shore_distance:
  Option<f32>`, `breaking: f32` and `shore_foam: f32`.

## 5. Floating bodies

### 5.1 Math (`regia_terrain::water_body::float`)

```rust
pub struct FloatPoint { pub offset: [f32; 3], pub radius: f32 }            // a sphere in the body's frame
#[derive(Component)] pub struct Floater {
    pub points: Vec<FloatPoint>,   // ≤ MAX_FLOAT_POINTS = 64
    pub fluid_density: f32,        // kg/m³ (1000 fresh, 1025 sea)
    pub linear_drag: f32,          // N·s/m at full submersion, relative to the water's velocity
    pub angular_drag: f32,         // N·m·s at full submersion
}
impl Floater { pub fn sphere(r) ; pub fn box_points(half_extents, counts: [u32; 3]) ; pub fn validate() }
pub fn float_forces(f: &Floater, pose: &FloatPose, water: impl Fn(f32, f32) -> Option<WaterSurface>, gravity: f32)
    -> FloatForces;   // force, torque about the center of mass, submerged volume and fraction, wet points
#[derive(Component)] pub struct FloaterState { … }   // the last step's FloatForces, for the agent
```

- Each point is a sphere of radius `r` whose center is at world `p`. With the surface height `y_s`
  over `(p.x, p.z)`, the submerged height is `s = clamp(y_s − (p.y − r), 0, 2r)` and the submerged
  volume the spherical cap `V = π s² (3r − s) / 3`. Buoyancy `ρ g V` acts straight up at the
  point; the torque about the center of mass is `(p − c) × F` (a tilted hull rights itself).
  A sample whose `miss` is NaN, or greater than `0.5 * r`, contributes no force that step
  (the choppy-wave inverse missed the center). `float_forces` applies at most the first
  `MAX_FLOAT_POINTS` (64) spheres. `validate` rejects any other count. `box_points` may build
  more than 64, up to 4096, so `validate` can refuse the grid; the step ignores the extras
  (ADR-5083).
- Drag per point: `−linear_drag · (V / V_total) · (v_point − v_water)`, `v_point = v + ω × (p − c)`,
  `v_water` from the surface query (orbital velocity plus drift or flow, so a stream carries a
  floater downstream); angular drag `−angular_drag · (V_sub / V_total) · ω`.
- `box_points` fills a box with `counts[0] · counts[1] · counts[2]` spheres (`n³` for a cube grid)
  whose total volume equals the box's (radius `(3 · cell volume / 4π)^(1/3)`, which is
  `(3 / 4π)^(1/3) · edge / n` for equal counts), so Archimedes' draft holds on average.
- Pure: same inputs, same bits. No allocation per call.

### 5.2 Physics (`regia_framework::buoyancy`)

`regia_framework::buoyancy::BuoyancyPlugin` (layer 3: layer-2 systems do not depend on each
other, `docs/02-architecture.md` § 1) adds `apply_buoyancy` to `FixedUpdate`, after `advance_water_clock` and before
`PhysicsStep`: for every dynamic body with a `Floater`, it reads the body's pose, velocities and
center of mass from `PhysicsWorld`, evaluates `float_forces` against the world surface query (§ 4,
with the shore switch), writes the result as the body's `ExternalForce` and a `FloaterState`, and
removes the `ExternalForce` it wrote the step the body leaves the water (no sphere touches it). A
host that sets its own `ExternalForce` on a floater in the water has it replaced; it adds a separate
force body or joint instead (spec note). A body that stays dry gets no `ExternalForce` from the
bridge and keeps one the host gave it. The system is client-only (`regia_framework` feature
`client`, ADR-2014). Span `water_buoyancy`.

## 6. Agent protocol (`book/agents/water.md`)

| Method | Change |
|---|---|
| `water.state` | each plane body reports `shore`: `null` or `{ size, cell_m, level, wet_fraction, max_depth_m, shoreline_texels }`; the switch is the top-level `shore` field, beside the other settings (ADR-5817) |
| `water.set { shore: bool }` | the runtime switch (§ 2.2) |
| `water.surface { at }` | answers the highest wet surface over each point (§ 4); each answer gains `depth_m`, `shore_m`, `breaking`, `shore_foam` |
| `water.raycast` | nearest hit over all bodies (§ 4); the hit gains `depth_m`, `breaking` and `shore_foam` (not `shore_m`, which ADR-1828's text listed: read it from `water.surface` at the hit point; ADR-5817) |
| `water.floaters { entity? }` | new: each floater's entity, `points` (the sphere count), submerged fraction and volume, force, torque, wet points (from `FloaterState`) |

No new error codes: `F0002` (bad values), `F0003` (no plugin, no entity) as before.

## 7. Determinism

The bake is a fixed-order pure function (§ 2.1). Shoaling is a pure function of the depth and the
wave. `float_forces` is pure; `apply_buoyancy` computes each body from its own state only (the
iteration order cannot change a result, ADR-5817) and runs at a fixed point in `FixedUpdate`, so two runs of the same steps give bit-identical poses (test § 8). The
water clock is the tick, as before.

## 8. Tests and acceptance

- Shore: the EDT matches a brute-force distance on random masks (exact); a sloped beach puts the
  zero crossing at the analytic shoreline within half a texel; a bowl lake is wet inside, dry
  outside; bytes round-trip; a wrong version is refused; the cell is raised to fit the cap.
- Shoaling: `Ks → 1` in deep water and continuous at the deep cut (within 1e-4); `Ks` is positive; the breaking cap holds at
  every depth; the scale is 0 on land; a shoaled swell's CPU surface stays continuous across the
  shoreline; constants pinned between Rust and WGSL.
- World query: highest body wins; a terrain plane over a hilltop answers nothing with the field and
  answers the level without it (switch); ribbons and level planes answer as before.
- Floating: a sphere of half the water's density floats with its center on the level; a box of
  density ρ_b settles at the Archimedes draft `ρ_b / ρ · height` within a fifth of a sphere layer (5 cm on a 1 m cube of 4³ spheres); a tilted box gets a
  righting torque; a floater in a stream drifts downstream; with rapier (regia_physics) a dynamic
  sphere settles at the level and two identical runs are bit-identical.
- Frame (lavapipe): a swell plane with a shore field renders, is hash-stable per tick, and shows
  no displacement over dry texels; the GPU vertex probe with a field equals the CPU mirror within
  1 mm. Frame cost: the water pass with and without the field, measured in the example
  `water_shore_cost` on lavapipe (correctness only); the RTX numbers are the lead's.

## 9. Platforms and budget

- Linux, Windows, Android aarch64: plain Rust and WGSL (`textureLoad` of an `Rg32Float` array in the vertex and fragment stages,
  no storage textures, no new feature); macOS builds where it builds today.
- Budget: the bake is off the frame (on change; a 1024² field took 64-71 ms on one the build server core under
  load, `water_shore_cost`); per frame, four `Rg32Float` `textureLoad`s per vertex and up to twelve per
  fragment (three bilinear samples: the swell's rest depth, the shore foam band and the caustics
  mask, ADR-5817) of a plane with a field (≤ 4 fields per frame, `MAX_SHORE_FIELDS`, one 2D array layer
  each, sized to the largest field: 8 MB for one 1024² field). Exact `f32` texels read with
  `textureLoad` (no filtering) keep the GPU bit-close to the CPU sample and need no filterable-float
  feature on any platform. Buoyancy: ≤ 64 points × one surface query (8 Newton steps on a
  swell) per floater per fixed step. Measured (`water_shore_cost`, 100 floaters × 8 spheres on a
  20-wave sea, one the build server core under load 110): 4.9 ms per step without a field, of which nearly
  all is the existing swell inverse (ADR-1620, about 6 µs a query); a field adds 0.5 ms in deep
  water and 2.8 ms across a beach (the shoaling coefficient per wave per Newton step). The first
  draft's "under 1 ms" target does not hold for that inverse; hosts with many floaters use fewer
  spheres per body. Cheaper queries (a shared `SwellInstant` per floater, a wave cap for buoyancy)
  are a follow-up. Steam Deck and mobile: the Low tier binds no field (no texture memory, no fetches) and draws the
  swell unshoaled, as it already draws fewer waves than the queries use; the CPU queries still shoal.

## 10. Out of scope

The FFT ocean band (N19) was built after this spec: `docs/specs/water-ocean.md` (ADR-1969). Not here:
wave refraction
(direction change over the bed), wave reflection, a run-up/swash simulation, two-way coupling
(floaters do not disturb the water), weather-driven sea states, the underwater view.

## 11. References

Felzenszwalb, Huttenlocher, "Distance Transforms of Sampled Functions", Theory of Computing 8
(2012); Dean, Dalrymple, *Water Wave Mechanics for Engineers and Scientists* (1991), ch. 4
(shoaling coefficient from energy flux); McCowan, "On the highest wave of permanent type" (1894),
breaking index 0.78; Archimedes, *On Floating Bodies*; Tessendorf, "Simulating Ocean Water" (2001)
for the FFT note.
