# Spec 55: Cloth, skinned cloth and soft bodies

Status: proposed (ADR-1579). Target rung: v0.4.x. Crate `regia_physics`, module `cloth` (today one file,
`cloth.rs`; it becomes `cloth/mod.rs` with submodules `skin`, `asset`, `wind`, `selfcol`, `soft`, `snapshot`,
`lod`, `maps`). Extends `specs/15-physics.md`; agent surface per `specs/23-agent-native.md`.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_physics::cloth` (XPBD cloth and ropes, `ClothSet`, `ClothPlugin`), `regia_agent::cloth`
(`physics.cloth_stats`), `book/agents/cloth.md`, `regia_framework::skinning` (§ 1.1).

Source area: runtime-physics/runtime-physics-cloth-deformables, runtime-characters/runtime-characters-cloth-assets.

## 1. Purpose and scope

Capes, skirts, coats, flags and soft props need to follow a skinned character, collide with the body and the
rigid world, and be authored as per-vertex data rather than code. The shipped CPU solver already simulates
free-standing cloth and ropes; this spec adds cloth bound to a skinned mesh, authored and cooked cloth assets,
self-collision, wind, collision against the rigid world, volume-preserving soft bodies, LOD and agent methods.

Not in scope: rigid bodies and ragdolls (`specs/15`), hair and strands (spring bones, `specs/17`), fluids and
fracture (tearing is an optional per-edge cut only), learned or baked cloth (§ 11). CPU only in this rung.

### 1.1 Baseline (implemented, spec records gaps)

`regia_physics::cloth` (`cloth.rs`): `ClothSolver` with XPBD particles, fixed step `ClothConfig.time_step`,
substeps (<= `MAX_SUBSTEPS` = 256), Gauss-Seidel iterations (<= `MAX_ITERATIONS` = 64), loops in insertion order;
`ClothConfig { gravity, time_step, substeps, iterations, damping, thickness, max_steps_per_call }` (excess steps are
dropped and counted in `ClothStats::dropped_steps`); builders `add_particle`, `add_distance`, `add_bend`, `add_grid`
(`ClothGrid`), `add_rope` (`ClothRope`); `pin`, `set_inv_mass`, `set_position`, `set_velocity`; colliders
`ClothCollider { shape: ClothShape, friction }` with `ClothShape::{Sphere, Capsule, Plane}`, treated as static within
a step; `stats() -> ClothStats`; `state_digest()` (64-bit FNV-1a of positions, velocities, inverse masses, step
count); `ClothError` with `code()` strings `cloth_non_finite`, `cloth_negative`, `cloth_particle_out_of_range`,
`cloth_collider_out_of_range`, `cloth_repeated_particle`, `cloth_degenerate`, `cloth_invalid_config`; `ClothSet`
(named solvers, insertion order, `step_all`); `ClothPlugin` stepping in `FixedUpdate`. Agent:
`physics.cloth_stats { name? }` (`regia_agent::cloth::install_cloth_methods`, `F0003` for an unknown name, `F0002`
for a bad param, `{ plugin: false }` without the plugin). Skinning: `regia_framework::skinning`
(`SkinnedMeshPlugin`) publishes palettes from `regia_animation::{SkinnedMesh, SkinPalette}` or from joint world
transforms (`SkinnedMesh::palette_from_joint_worlds`). Rigid queries: `regia_physics` `overlap(shape, pose,
QueryFilter)` with `from_layer` and `exclude_sensors`; layers from `config/physics.ron` (`PhysicsLayers`);
`ExternalImpulse { impulse, torque_impulse }`.

| Capability | Baseline | New here |
|---|---|---|
| Constraints | distance (stretch, shear, rope skip-one), dihedral bend | pose constraints (reach, standoff, pose pull), tethers, strain caps, tetra volume, pressure, shape-match clusters |
| Colliders | sphere, capsule, plane; static within a step | moving colliders (previous pose); boxes, hulls and triangles through world snapshots (§ 2.4) |
| Self-collision | none | particle spheres or vertex-triangle (§ 2.6) |
| Forces | gravity, damping | wind fields with drag and lift (§ 2.7) |
| Character link | caller moves pinned particles | skin binding, blend, teleport reset, body proxies (§ 2.2) |
| Assets | none | `.cloth.ron`, `.soft.ron`, cooked `.cloth.bin` / `.soft.bin` (§ 4) |
| Agent | `physics.cloth_stats` | `physics.cloth_*` methods (§ 5); `physics.cloth_stats` unchanged |

**Compatibility.** `ClothConfig`, `ClothCollider` and `ClothShape` keep their shape (new data lives in new types,
so no struct literal or exhaustive match breaks). `ClothError` gains variants: hosts that match it exhaustively
must add an arm (an Upgrade note for the lead's release fragment).

## 2. Model

Terms: **sim mesh** (the coarse triangle mesh that is simulated), **render mesh** (the visible mesh, bound to the
sim mesh), **pose target** (where a particle would be if it were only skinned), **vertex map** (a per-vertex float
array on the sim mesh), **body proxy** (a sphere or capsule collider that follows a joint), **world snapshot** (a
read-only copy of nearby rigid shapes taken once per fixed step), **soft body** (a closed-surface or volumetric
deformable).

### 2.1 Components and resources

| Item | Kind | Role |
|---|---|---|
| `ClothAsset` | asset (`.cloth.ron` -> `.cloth.bin`) | Sim mesh, vertex maps, constraint sets, parameters, LODs, render binding |
| `ClothSim` | component | Instance: asset handle, owner `SkinnedMesh` entity (optional), parameter overrides, collision layer name, solver name in `ClothSet` |
| `ClothBodyProxy` | component on joint entities | Sphere or capsule that follows the joint |
| `WindField` | component (with `Transform`) | Directional, radial or vortex wind (§ 2.7) |
| `SoftAsset` / `SoftSim` | asset (`.soft.ron`) / component | Soft-body counterparts (§ 2.8) |
| `ClothSet` | resource (baseline) | Named solvers, stepped in insertion order |
| `ClothOutput` | component (written by the plugin) | Output positions and normals, previous and current, for interpolation |

`ClothSim` has a state (§ 2.9) and a blend `b` in `[0, 1]` (0 = pure pose, 1 = pure simulation) the game may drive.

### 2.2 Skinned cloth

A cloth bound to a skinned mesh keeps one `PoseTarget { position, normal }` per sim vertex: the skinned
world-space position and normal for the current pose. `regia_physics` cannot depend on `regia_animation` (both are
Systems-layer crates with no allowed edge), so the solver takes targets as plain data
(`ClothSolver::set_pose_targets`). The glue system `sync_cloth_pose` lives in `regia_framework` (Gameplay layer):
it reads the palette `SkinnedMeshPlugin` already publishes, skins the sim mesh with its joint weights (<= 4
influences, the same joint-matrix maths as `regia_framework::skinning`), and calls the solver.

Vertex maps that bind cloth to the pose (all per sim vertex):

| Map | Range | Meaning |
|---|---|---|
| `reach` | meters, >= 0 | Radius of the sphere around the pose target the particle must stay inside; 0 makes the vertex follow the pose exactly (inverse mass 0). |
| `standoff` | meters, signed | The particle must satisfy `dot(x - target, n) >= -standoff` (`n` = pose normal): a half-space that keeps cloth outside the body. Ignored where `reach` is 0. |
| `pose_pull` | `[0, 1]` | Strength of a soft XPBD attachment to the pose target: compliance `pull_compliance / max(pose_pull, 1e-6)`; 0 means no pull. Multiplied at runtime by `ClothSim.pull_scale`. |
| `sim_mix` | `[0, 1]` | Per-vertex mix of simulated and pose position in the output (0 = skinned). |
| `mass_scale` | `> 0` | Multiplies particle mass (default 1). |

Decision (ADR-1585): maps store meters directly; there is no percentage paint scale. Editors show meters.

Per substep (extending the baseline loop):

1. Pose targets are linearly interpolated between the previous and current fixed step for the substep fraction,
   so fast animation is not one teleport per step.
2. Free particles integrate gravity, wind and damping as in the baseline. **Root inertia:** the displacement of
   the cloth's root joint (translation, and rotation about it) over the substep is applied to free particles
   scaled by `1 - inertia_scale` (`inertia_scale` in `[0, 1]`, default 1 = full world-space inertia; 0 = the
   cloth moves rigidly with the root, so a sprint does not stretch a cape behind the character).
3. Constraint order: distance, bend, tethers, then pose constraints (pull as a soft attachment, then the hard
   `reach` sphere projection, then the hard `standoff` half-space), then self-collision (§ 2.6), then colliders
   (last, as in the baseline), then strain caps (§ 2.3).
4. Velocities and contacts as in the baseline.

Output: `p_out = lerp(pose_target, p_sim, b * sim_mix)`; normals recomputed from `p_out` (area-weighted, fixed
triangle order). Vertices with `reach == 0` equal the pose target exactly.

**Teleport.** If the root moves more than `teleport_distance` (default 2.0 m) in one fixed step, or the game calls
`reset`, all particles snap to their targets, velocities are zeroed, self-collision history is cleared and the
state becomes `Settling`.

**Render binding.** Each render vertex of the cloth section stores `(sim triangle, u, v, d)`: the barycentric
point of the sim triangle plus `d` along its interpolated normal (cooked, § 4.3). The cooker splits the cloth
section out of the render mesh into its own mesh, drawn by a separate, unskinned `MeshRenderer` entity owned by the
`ClothSim`; the rest of the character stays on the normal skinned path. Decision (ADR-1585): v1 updates that mesh by
publishing a new `Arc<CpuMesh>` for the section each rendered frame (the renderer uploads by `Arc` identity into its
mesh arena today); an in-place dynamic vertex buffer is a renderer follow-up (§ 11) and the budget in § 10 counts
the upload.

### 2.3 Parameters

`ClothParams` (asset defaults, overridable per `ClothSim`, editable through `physics.cloth_set_params`). Fields
with baseline meaning are copied into the solver's `ClothConfig`; the rest live in a new `ClothTuning` held by the
solver.

| Field | Default | Range | Meaning |
|---|---|---|---|
| `gravity_scale`, `gravity_override` | 1.0, none | -4..4 | Scale or replace the config gravity |
| `damping` | 0.1 /s | 0..20 | Baseline |
| `stretch_compliance`, `shear_compliance`, `bend_compliance` | 0, 1e-4, 1e-2 | >= 0 | Baseline; cook time only today: a `ClothSim` override does not change a built solver (ADR-2250) |
| `substeps`, `iterations` | 8, 1 | baseline limits | Baseline; LOD scales them |
| `pull_compliance` | 1e-3 | >= 0 | Pose-pull softness |
| `inertia_scale` | 1.0 | 0..1 | § 2.2 |
| `strain_cap` | none | `(max >= 1, min in 0..1)` | Optional hard cap on edge length ratios, projected after the last iteration |
| `tether` | none | `scale >= 1` | Each free particle stays within `scale` times its rest path length from its nearest zero-reach vertex (long-range attachments, § 12) |
| `collision_thickness` | 0.01 m | >= 0 | Baseline `thickness` |
| `friction` | 0.3 | 0..2 | Reserved: validated, not yet read; contacts use the collider's friction (ADR-2250) |
| `self_collision` | `Off` | | § 2.6 |
| `wind_model` | `Aerodynamic { drag: 1.0, lift: 0.3 }` | | § 2.7 |
| `air_density` | 1.2 kg/m^3 | > 0 | For aerodynamic forces |
| `teleport_distance` | 2.0 m | > 0 | § 2.2 |
| `tear` | none | | § 2.5 |

Named starting points in the editor and agent (`light_fabric`, `heavy_fabric`, `stiff_fabric`, `elastic`, `chain`)
are plain data shipped as example assets; their numbers are authoring choices, not engine behavior.

### 2.4 Body and world collision

**Body proxies.** `ClothBodyProxy` entities give sphere or capsule colliders in world space. The glue system
`sync_cloth_colliders` (`regia_framework`) updates them each fixed step through `ClothSolver::set_collider` and
records the previous shape with the new `ClothSolver::set_collider_previous(index, ClothShape)`. A cloth asset lists
the proxy names it collides with; by default it uses every proxy under the same skinned root.

**Moving colliders.** Contact friction and push direction use the collider's velocity at the contact point,
derived from previous to current shape. A collider whose per-step travel exceeds `collision_thickness` is tested
swept (inflated by its travel). This replaces the baseline "colliders are static within a step". Built (ADR-2425):
when a collider's surface at a particle moves more than the thickness within one substep and the end test misses,
the particle's path relative to the collider is sampled (one sample per thickness, at most 16); at the first sample
that enters from outside the particle is pushed out and carried with the collider. Triangle lists are not swept.

**World snapshots.** Once per fixed step, after the rigid step and before cloth, the plugin builds a
`WorldSnapshot` per solver:

1. `overlap` query with a box around the solver's particle bounds grown by `snapshot_margin` (0.5 m), filtered by
   the cloth's layer (`ClothSim.layer`, default `"default"`, resolved through `PhysicsLayers`) and
   `exclude_sensors()`.
2. Each hit becomes `SnapshotShape { key: SpawnKey, kind, prev, cur, friction }`, `key` = the collider's
   spawn-arrival key (`regia_ecs::spawn_order::SpawnKey`, ADR-1347; never entity bits — they are reserved by
   parallel `Commands` in thread-timing order — and never an `EntityGuid`, which a runtime-spawned prefab draws
   at random).
   Kinds: sphere, capsule, box, convex hull (at most 64
   vertices and at most 64 unique outward planes `n · x <= d`; a larger cloud, a flat cloud, or more planes use the
   bounding box), plane, static triangle list (heightfield or trimesh, at most
   `snapshot_triangles_max` = 4,096, nearest to the cloth first).
3. Sorted by `key`, then frozen. The solver reads only the snapshot and never touches the rigid sets, so solvers can
   step in parallel. Boxes, hulls and triangles exist only in snapshots; `ClothShape` is unchanged.

Response is one-way by default. With `reaction: true` per solver, contact impulses against dynamic bodies are summed,
capped per body per step (`max_reaction_impulse`, 5 N*s) and added as `ExternalImpulse` for the next rigid step, with
the contacts' torque about the body's center of mass scaled by the same cap factor (ADR-2425,
`ClothSolver::take_reaction_wrench`); off
by default because coupling at different substep rates can be unstable. Hull and box contacts project to the closest
point with the baseline "push out to thickness, no launch speed" rule; triangle contacts use the face normal on the
front side; particles that start behind a thin triangle are not rescued in v1.

### 2.5 Tearing (optional)

Off by default. With `tear: Some(TearParams { strain, max_per_step })` a distance constraint whose length ratio
exceeds `strain` (default 2.0) at the end of a fixed step is switched off (infinite compliance). At most
`max_per_step` are cut per step, in ascending constraint index. Sim triangles using a cut edge are hidden and their
render triangles dropped from the section's index list; vertices are not split and nothing is re-meshed, so tears
are ragged holes. Writes `ClothTorn { solver, constraint, step }`. Edges touching a zero-reach vertex never tear.

### 2.6 Self-collision

`SelfCollision::{Off, Spheres { radius }, VertexTriangle { radius }}`:

- `Spheres`: particle pairs closer than `2r` are pushed apart along their separation, split by inverse mass.
- `VertexTriangle` (built, ADR-2425; before it ran as `Spheres`, ADR-1672, ADR-2250): a particle within `r` of a triangle's plane and inside its prism is pushed to the side it was on
  at the start of the step (signed distance sign kept), which handles folds and layered garments. About 3x the
  cost of `Spheres`. Triangles are hashed into every grid cell their box covers; the separating normal velocity of
  touched pairs is removed after each substep (inelastic contact); `self_crossings` counts pairs left crossed.

Shared rules: pairs within `exclude_rings` edge hops (default 2) and pairs closer than the contact distance at rest
are skipped. Broad phase: a uniform grid with cell size `max(2r, longest sim edge)`, counting sort by (cell,
particle), pairs visited in ascending `(i, j)`; no hash-map order is observable; rebuilt once per substep. As built
(ADR-5640): the grid is hashed into a power-of-two table (counting-sorted, still no observable map order), and one
long edge no longer sets the cell: `Spheres` uses `max(1.5 × reach, median rest edge)` and `VertexTriangle`
`max(2 × 1.5 × reach, typical edge / 3)`, where `reach` is the contact distance plus the layer gap. Garment
layers: a per-vertex `layer: u8` (default 0); pairs on different layers keep an extra `layer_gap` (default `2r`) on
the side given by layer order. As built (ADR-2425): the side is the step-start side for every pair, and a pair across
layers moves only its outer side (the inner side gets zero inverse mass), so inner layers push outer layers and never
the reverse. At most `self_pairs_max` (8 per particle on average) contacts per substep; the rest
count in `dropped_self_pairs`. Self friction `self_friction` (0.2).

### 2.7 Wind

`WindField` variants: `Directional { speed }` (along the entity's forward, inside its volume or everywhere when
`volume: None`); `Radial { strength, falloff }` (away from the center, or toward it when negative, magnitude
`strength * (1 - d/R)^falloff`); `Vortex { strength, axis, falloff }` (tangential about the local axis). Optional
`gust: Some(Gust { amplitude, period_steps, seed })` modulates speed with a pure function of `(seed, fixed step
counter, quantized position)`: an integer hash and smoothstep blend, no trigonometry, no clock. Fields sum in
ascending entity key order (key as in § 2.4); `ClothSet::set_global_wind(Vec3)` adds a global term. Wind speed is clamped to 60 m/s.

`WindModel` per cloth, for a sim triangle with area `A`, normal `n`, triangle velocity `v_t`, wind `w`, relative
velocity `v_r = w - v_t`, `v_n = dot(v_r, n)`:

- `Aerodynamic { drag, lift }`: drag `0.5 * rho * A * drag * |v_n| * v_n * n` plus lift `0.5 * rho * A * lift * |v_n|
  * t` (`t` = unit tangential part of `v_r`), split equally among the three vertices (decision: equal thirds;
  angle-weighted splitting is a follow-up). Both faces respond.
- `Push { gain }`: acceleration `w * gain` on every free particle, independent of orientation; one cheap pass, used
  by far LODs.

### 2.8 Soft bodies

`cloth::soft` reuses the particle store, substeps, colliders, snapshots, LOD and digest. `SoftSim` model per asset:

| Model | Input | Constraints | Use |
|---|---|---|---|
| `Pressure` | closed triangle mesh | edges + one XPBD volume constraint `C = V - k * V0` (`k` = `pressure`, default 1; `V` = sum of signed tetrahedra to the centroid) | balls, inflated props |
| `Tetra` | tetrahedral mesh | per-tet volume `C = V_t - V_t0` plus edge distances, separate compliances | rubber, flesh-like props |
| `ShapeMatch` | point set + clusters | per cluster, goals from the best-fit rotation (polar decomposition of the covariance), pulled by `stiffness` in `[0, 1]`; optional linear (affine) blend `beta` and plasticity | jelly, stylized squish |

- **Volume constraints** use XPBD with `volume_compliance` (0 = incompressible); the gradient for vertex `i` is the
  sum over its faces of `(x_j x x_k) / 6`. `V0` comes from the rest mesh; `pressure` is live and clamped to
  `[0.05, 8]`.
- **Shape matching** is applied after integration (`x_i += stiffness * (g_i - x_i)`), then velocities are derived.
  Plasticity: when `||A - R||` exceeds `yield`, the rest shape moves toward the current shape at `creep`, capped by
  `max_plastic`. The polar decomposition uses a fixed iteration count (deterministic instruction sequence).
  As built (ADR-9070): `SoftSolver::set_plasticity(Some(Plasticity { yield_threshold, creep, max }))`; per substep
  `S = sym(Rᵀ A)`, past `‖S - I‖_F > yield` the plastic map `S_p <- (I + min(h·creep, 1)(S - I)) S_p` (`creep` per
  second), unit determinant through the deterministic cube root, `‖S_p - I‖_F <= max`, rest offsets `S_p q0`. Runtime
  only (API and `physics.cloth_set_params`); the `.soft.ron` shape is unchanged.
- **Pose attachment** (flesh on a skeleton): the `reach` and `pose_pull` maps apply with the skinned rest vertex as
  target. Render vertices bind to tets by barycentric coordinates; surface-only models bind like cloth.
- **Tet generation** is a desktop authoring tool in `regia_asset_pipeline`: voxelize the closed surface at a chosen
  cell size, keep interior cells, split each cube into 5 tets with alternating parity so faces conform, bind each
  surface vertex to its containing tet. It reports tet count and the minimum volume ratio and refuses open meshes
  (`soft_not_closed`). Better meshing is a follow-up.
- Inverted tets at runtime are allowed (XPBD recovers) and counted (`inverted_tets`); an inverted rest tet fails
  load (`soft_inverted_rest`). Soft bodies collide with snapshots, proxies and optionally themselves (`Spheres`).

### 2.9 States and LOD

| State | Entered when | Behavior | Leaves to |
|---|---|---|---|
| `Inactive` | asset not built, or LOD off and fade done | no simulation; output = pose | `Settling` on activate |
| `Settling` | created, teleport, reset, wake from LOD off | particles at pose, zero velocity; `settle_steps` (8) at double damping; blend ramps in over `blend_in_steps` (30) | `Running` |
| `Running` | settling done | normal | `Fading`, `Paused`, `Settling` |
| `Fading` | LOD off requested | blend ramps to 0 over `blend_out_steps` (20) while simulating | `Inactive` |
| `Paused` | not shown, or paused by an agent. Shown reads the sim entity's `InheritedVisibility` when that component is present (a visible sim does not consult its section); otherwise the section's `InheritedVisibility`. Missing visibility counts as shown. `ViewVisibility` is not written (ADR-2083) | no stepping, state kept | `Running`, or `Settling` after `resume_reset_steps` (600) |

**Distance LOD.** `lod_distances: [d1, d2, d_off]` per `ClothSim`, with 10 % hysteresis (leave a level only past
`d * 1.1` outward or `d * 0.9` inward). The viewer is the camera with the lowest `Camera::order`; an `order` tie
resolves to the earliest-spawned camera (`regia_ecs::spawn_order`, ADR-1347), never the lowest entity id:

| Level | Substeps | Iterations | Self-collision | Wind |
|---|---|---|---|---|
| 0 | authored | authored | authored | authored |
| 1 | half (min 2) | 1 | one step down (`VertexTriangle` -> `Spheres` -> `Off`) | `Push` |
| 2 | quarter (min 2) | 1 | `Off` | `Push`; coarser sim mesh if the asset has one |
| off | none | | | fade to pose, then `Inactive` |

**Sim-mesh LODs** (optional) are separate sim meshes with their own render binding. Decision (ADR-1585): LOD 0 is the
only authored source: every other LOD takes LOD 0's parameters unless it overrides a field, and its maps are
resampled from LOD 0 by nearest rest vertex (ties by lower index). Switching sim-mesh LOD maps particle positions
and velocities by the same nearest-vertex rule. `ClothBudget { max_particles_per_step }` (8,192 on every target; the authoritative step must not depend on
the target OS, ADR-4764; `BUDGET_ANDROID` = 4,096 stays as a cap a host may set explicitly) ranks solvers by
(distance, spawn-arrival key) and moves the farthest down one level until the sum fits. As built (ADR-5640): a
solver counts its particles at LOD 0, half at level 1 and a quarter at level 2 (their substeps are halved and
quartered, `lod::budget_cost`), so the budget bounds particle-substeps, not raw particle counts.

**Edge cases.** Zero particles or all at zero reach: the step is a no-op but the step count advances. NaN or
infinity in any input (pose targets, fields, params) is a typed error and changes nothing. A frame over
`max_steps_per_call` drops the excess (baseline); pose targets use the latest pair. A hot-reloaded asset rebuilds the
solver and enters `Settling`. A mirrored (negative-scale) root flips winding and normals in the glue. A step without a
fresh animation pose reuses the current pose.

## 3. Rust API

### 3.1 Placement and layering

`regia_physics` (Systems) holds solvers, assets, snapshot types and vertex-map operations. `regia_framework`
(Gameplay) holds the glue systems `sync_cloth_pose`, `sync_cloth_colliders`, `sync_cloth_visibility` and
`publish_cloth_mesh` (render section). `regia_agent` holds the methods. `regia_asset_pipeline` (desktop only) holds
the cooker and tet generator. No new crate edge; no new dependency.

### 3.2 Types (sketch)

```rust
pub struct PoseTarget { pub position: Vec3, pub normal: Vec3 }
pub struct PoseMaps { pub reach: Vec<f32>, pub standoff: Vec<f32>, pub pose_pull: Vec<f32>,
                      pub sim_mix: Vec<f32>, pub mass_scale: Vec<f32> }
pub enum SelfCollision { Off, Spheres { radius: f32 }, VertexTriangle { radius: f32 } }
pub enum WindModel { Push { gain: f32 }, Aerodynamic { drag: f32, lift: f32 } }
pub struct TearParams { pub strain: f32, pub max_per_step: u32 }
pub struct ClothTuning { /* pull_compliance, inertia_scale, strain_cap, tether, self_collision, wind_model, ... */ }
pub struct SnapshotShape { pub key: SpawnKey, pub kind: SnapshotKind, pub prev: Pose, pub cur: Pose, pub friction: f32 }
pub struct WorldSnapshot { /* sorted by key */ }
pub enum ClothState { Inactive, Settling, Running, Fading, Paused }

impl ClothSolver {                               // added; baseline methods unchanged
    pub fn set_pose_targets(&mut self, prev: &[PoseTarget], cur: &[PoseTarget]) -> Result<(), ClothError>;
    pub fn set_pose_maps(&mut self, maps: PoseMaps) -> Result<(), ClothError>;
    pub fn set_tuning(&mut self, tuning: ClothTuning) -> Result<(), ClothError>;
    pub fn set_blend(&mut self, blend: f32) -> Result<(), ClothError>;
    pub fn set_collider_previous(&mut self, index: usize, previous: ClothShape) -> Result<(), ClothError>;
    pub fn set_snapshot(&mut self, snap: WorldSnapshot);
    pub fn unpin(&mut self, index: usize, inv_mass: f32) -> Result<(), ClothError>;
    pub fn output(&self, out: &mut Vec<PoseTarget>);          // blended positions + normals
    pub fn reset_to_pose(&mut self);
    pub fn state(&self) -> ClothState;
}
pub struct SoftSolver { /* owns a ClothSolver particle store plus model constraints */ }
impl SoftSolver { pub fn volume(&self) -> f32; pub fn rest_volume(&self) -> f32;
                  pub fn set_pressure(&mut self, k: f32) -> Result<(), ClothError>; }
```

Messages: `ClothTorn`, `ClothStateChanged { entity, from, to }`, `ClothTeleported { entity }`. `ClothPlugin`
(baseline) adds the asset loaders, `SoftPlugin`, and registers `WindField`, `ClothSim`, `ClothBodyProxy`, `SoftSim`
for reflection and serde; runtime-only data (pose targets, grids, snapshots) is `#[reflect(ignore)]`. As built
(ADR-2083, ADR-5640): the glue writes `ClothTear { entity, constraint, step }` (the solver's `ClothTorn
{ constraint, step }` has no solver name; the entity identifies the instance); there is no separate `SoftPlugin`
(`ClothPlugin` steps the `SoftSet`, and `regia_framework`'s `ClothGluePlugin` adds the soft-body glue); the level
data `ClothSimSource` and `SoftSimSource` are the reflected components (they build `ClothSim` / `SoftSim`, which
hold an `Arc` of the cooked asset and are not reflected), with `ClothBodyProxy` and `WindField`.

Schedule in `FixedUpdate`, every edge explicit (`before`/`after`, no reliance on ambiguity, docs/04): rigid step ->
`sync_cloth_colliders` -> snapshot -> `sync_cloth_pose` -> wind gather -> cloth and soft step (solvers in insertion
order; `ClothPlugin` inserts its `ClothSet` with parallel stepping on, and leaves a set the app already inserted
alone) -> `ClothOutput` swap. `publish_cloth_mesh` runs in
`PostUpdate` and interpolates `ClothOutput` by the fixed-step overstep.

### 3.3 Errors

New `ClothError` variants with `code()`: `cloth_pose_size_mismatch`, `cloth_bad_blend`, `cloth_bad_param`,
`cloth_asset_invalid`, `cloth_map_size_mismatch`, `cloth_unsupported_version`, `cloth_snapshot_too_large`,
`soft_not_closed`, `soft_inverted_rest`, `soft_bad_cluster`. A rejected call changes nothing.

### 3.4 Vertex-map edits

Map editing is a data API in `cloth::maps`, shared by the editor's paint mode and the agent, so every edit is
reproducible without a GUI. `apply_map_edit(asset: &mut ClothAsset, lod: 0, map: MapName, edit: MapEdit)` with:

- `Set { vertices: Vec<(u32, f32)> }`: explicit values.
- `Sphere { center: Vec3, radius, value, falloff: Linear | Smooth | Hard, strength }`: blend toward `value` for
  vertices within `radius` of `center` on the rest pose (the editor's brush issues these along a stroke).
- `Relax { iterations, strength }`: each vertex moves toward the mean of its edge neighbors (Laplacian smoothing).
- `Ramp { from: u32, to: u32, a, b }`: values from `a` at vertex `from` to `b` at vertex `to`, by projection onto the
  rest-pose segment between them, clamped.
- `Flood { seed: u32, tolerance, value }`: the edge-connected region whose values are within `tolerance` of the seed's.
- `FromVertexColor { channel: R | G | B | A, low, high }`: map a render-mesh color channel (nearest sim vertex) to
  `[low, high]`.

`transfer_maps(from: &ClothAsset, to: &mut ClothAsset)` copies maps between sim meshes: by index when the topology
matches, else by nearest rest vertex. Edits mark the asset dirty and the source `.cloth.ron` is rewritten
canonically on save.

## 4. Files and formats

All formats start at `format_version: 1` and are registered with `cargo xtask check-formats` as `cloth`, `soft`,
`cooked-cloth`, `cooked-soft` (the lead runs `cargo xtask check-formats --update` and `cargo xtask manual`). Frozen
rule (spec 12 § 4a): any change of meaning or layout bumps the version, ships a migrator and `regia upgrade-project`
support for the source formats; the loaders keep reading every older version and return
`cloth_unsupported_version` only for newer ones. Cooked files are rebuilt from source, never migrated. The formats
freeze with the v0.4.x rung.

### 4.1 `.cloth.ron`

```ron
(
  format_version: 1,
  name: "cape",
  skeleton: Some("hero.glb"),
  sim_mesh: (source: "cape_sim.glb", node: "Cape_Sim"),
  render_mesh: Some((source: "hero.glb", node: "Hero", section: 2)),
  params: (substeps: 8, pull_compliance: 0.001, wind_model: Aerodynamic(drag: 1.0, lift: 0.3)),
  maps: {
    "reach": Sparse(default: 0.0, entries: [(12, 0.25), (13, 0.30)]),
    "standoff": Dense([/* one value per sim vertex */]),
  },
  constraint_sets: [(name: "stretch", kind: Stretch, compliance: 0.0),
                    (name: "bend", kind: Bend, compliance: 0.01)],
  proxies: ["spine_cap", "thigh_l_cap"],
  lods: [(sim_mesh: Some("cape_sim_lod1.glb"), overrides: ())],
)
```

Maps are `Dense(Vec<f32>)` (length = sim vertex count, else `cloth_map_size_mismatch`) or `Sparse(default,
entries)`. Map names are the five of § 2.2 plus `layer`; unknown names fail load. `constraint_sets` kinds: `Stretch`,
`Shear`, `Bend`, `Tether`, `Volume`, each with a compliance and an optional per-vertex scale map. `lods` lists LOD 1
and up (LOD 0 is the top-level mesh).

### 4.2 `.soft.ron`

```ron
(format_version: 1, name: "ball",
 model: Pressure(pressure: 1.0, volume_compliance: 0.0),   // or Tetra(tets: [[u32; 4]], ...) or ShapeMatch(clusters: [[u32]], stiffness, beta, yield)
 surface: Some((source: "ball.glb", node: "Ball")), params: (substeps: 8), maps: {})
```

### 4.3 Cooked `.cloth.bin` / `.soft.bin`

Wrapped in the standard cooked header (spec 12: magic `FCKD`, `format_version`, `type_tag`, `asset_id`). Payload v1,
little-endian, 4-byte aligned:

| Block | Layout |
|---|---|
| Header | `u32 particle_count, u32 triangle_count, u32 lod_count, u32 flags` (bit 0 pose maps, bit 1 render binding, bit 2 tearable) |
| Rest positions | `f32[3]` per particle (model space) |
| Triangles | `u32[3]` per triangle |
| Constraints | runs by kind, each sorted by `(a, b)`: `u8 kind, u8 pad, u16 reserved, u32 a, u32 b, f32 rest, f32 compliance` (bend adds `u32 c, d`) |
| Maps | `f32` arrays, or `u8` quantized with a flag: `reach, standoff, pose_pull, sim_mix, mass_scale, layer` |
| Joint weights | `u8[4]` joint indices + `u8[4]` weights per particle (normalized) |
| Render binding | `u32 triangle, f32 u, f32 v, f32 d` per render vertex of the section |
| Params | canonical RON of `ClothParams` |
| LOD table | repeated blocks for LOD 1+ |

As built (ADR-2083): the payload starts with its own magic and version inside the `FCKD` wrapper, the header's third
word is the constraint count (bit 3 of `flags` marks tethers), every constraint record carries four ids (links write
`c = d = 0`), maps are `f32` only (no `u8` quantization), and the render binding adds a per-vertex UV and the render
triangle list. `docs/formats/cloth-ron.md` is the byte layout of record.

The cooker checks finite values, indices in range, distinct constraint endpoints, at most 65,535 particles per cloth.
Output depends only on the source, so cooking is byte-reproducible.

## 5. Agent surface

Registered by `regia_agent::cloth::install_cloth_methods` on the `AgentExtension`, next to the baseline method, in the
existing `physics.cloth_*` namespace. `target` is `{ entity }` or `{ name }` of a `ClothSet` solver. Without the
plugin, read methods answer `{ plugin: false }` (baseline convention) and play methods answer `F0003` with reason
`cloth_plugin_missing`. Errors: `F0003` (not found) or `F0002` (bad params) with `data.reason` one of
`cloth_not_found`, `cloth_bad_param`, `cloth_particle_out_of_range`, `cloth_step_count_too_large`,
`cloth_plugin_missing`, or a § 3.3 code.

| Method | Class | Params | Result |
|---|---|---|---|
| `physics.cloth_stats` | read | `name?` | baseline, unchanged |
| `physics.cloth_inspect` | read | `target?`, `particles?: { from, count <= 256 }` | per solver: the `cloth_stats` fields plus `state`, `blend`, `lod`, `dropped_self_pairs`, `torn`, `wind`, `max_pose_error`, `volume` / `rest_volume` (soft), `inverted_tets`, and the particle slice |
| `physics.cloth_set_params` | play | `target`, `patch` (any `ClothParams` field, `blend`, `pressure`, `lod_distances`; `plasticity` for shape matching, ADR-9070) | `{ applied, params }`; as built (ADR-5640) `lod_distances`, `gravity_override`, the three compliances (cook time, ADR-2250), `friction` and `density` are not patchable yet (refused as an unknown parameter, `cloth_bad_param`); `lod` (0-3) forces a level |
| `physics.cloth_pin` | play | `target`, `particles: [u32]` (<= 1,024) | `{ pinned }` |
| `physics.cloth_unpin` | play | `target`, `particles`, `inv_mass?` | `{ unpinned }` |
| `physics.cloth_wind` | play | `target?`, `velocity: [x, y, z]`, `gust?`, `clear?` | `{ wind }` |
| `physics.cloth_step` | play | `target?`, `count: 1..=600` | `{ steps_run, digest }` (fixed steps for that solver only, for use while paused) |
| `physics.cloth_reset` | play | `target` | `{ state }` |
| `physics.cloth_edit_map` | edit_scene | `asset`, `map`, `edit` (a § 3.4 `MapEdit` as JSON) | `{ changed }`; marks the asset dirty |

Classes map to `Permission::{Read, Play, EditScene}`. Book: extend `book/agents/cloth.md` with every method, reason
and the checks below, and add `book/agents/soft-bodies.md`. `book/COVERAGE.md`: one § 2 row per new method
(`| physics.cloth_inspect | physics_cloth_inspect | [cloth](agents/cloth.md) |`, ...), § 4 rows for the four formats.

Checks an agent runs: settle (`physics.cloth_step 300`, `physics.cloth_inspect`, then `step 60` again: bounds and
`max_pose_error` stable within 1 cm); determinism (`physics.cloth_reset`, `step N`, record `digest`, repeat,
compare); wind (`physics.cloth_wind`, `step`, `play.expect` bounds shift downwind); screenshots at blend 0 and 1;
for a ball, `volume / rest_volume` near `pressure`; `play.wait_until` on `state == "running"`.

## 6. Editor surface

A cloth inspector (params, starting-point picker, LOD distances, self-collision mode, blend slider) and a paint mode
on the sim mesh that issues § 3.4 edits (map picker, color ramp in meters, simulate-preview toggle). Proxies and
snapshot shapes draw as gizmos (group `cloth`, off by default), with `reach` spheres and `standoff` planes for
selected vertices. The editor holds no data that is not in the `.cloth.ron`; `physics.cloth_edit_map` is the
headless equivalent.

Built (ADR-2884): `regia_editor::cloth_panel`, agent `editor.cloth_panel_state`, `editor.cloth_panel_edit` and
`editor.cloth_paint_stroke`; paint runs `regia_agent::cloth::edit_cloth_map`, the core of `physics.cloth_edit_map`.
Blend and LOD distances are instance fields (`ClothSimSource`), edited through the editor's undo stack, not the file.
`ClothParams::wind_scale` (default 1, optional) scales the scene wind (spec 99 § 3.3) per cloth.

## 7. Determinism

- Fixed steps and substeps only; the per-solver step counter and gust seeds are the only time inputs.
- Order: solvers in insertion order; constraints in cooked order; particles by index; snapshot shapes by `key`; wind
  fields by entity key; tears by constraint index; self pairs by `(i, j)`. Parallelism is across solvers only (each
  solver single-threaded), so results do not depend on thread count. Built as `ClothSet::set_parallel` (ADR-2425):
  one solver per task, step counts reduced in insertion order; bit-identical to serial stepping.
- Bit-exact for the same build, platform and inputs. Across platforms (x86-64 vs aarch64, different fused
  multiply-add contraction) tolerance-based: positions within `1e-3 * rest edge length` after 600 steps for the
  standard tests; digests are not compared across platforms.
- `state_digest` keeps the baseline fields and adds blend, state, pose targets, the torn-edge bitmap, a canonical
  encoding of the tuning, and (soft) `pressure`. A digest version is part of the seed; a known-answer test pins it.
- No `sin`/`cos` in stepping; the polar decomposition uses a fixed iteration count.

## 8. Tests and acceptance

Unit and integration tests without a GPU; fixed steps and counters only, no wall-clock asserts; cooked files and
fixtures in `regia_test_utils::unique_temp_dir`.

1. **Hanging cloth settles.** 16 x 16 grid, top row pinned, stretch compliance 0, 8 substeps, damping 0.5: after 600
   steps kinetic energy < `1e-6` of the potential energy released and the height within 2 % of `rows * spacing`;
   total mechanical energy never rises more than 1 % after step 10.
2. **Two pinned corners.** Pinned particles never move (bit-exact); the shape is mirror-symmetric within `1e-4` m;
   the lowest particle is at the bottom middle.
3. **Pressure ball.** Icosphere (subdivision 3), gravity 0, `volume_compliance 0`: after 600 steps `volume /
   rest_volume` within 2 % of 1.5 for `pressure 1.5`, 0.5 % of 1.0 for 1.0, 3 % of 0.5 for 0.5; resting on a plane
   under gravity within 5 % of `pressure`, with per-step center-of-mass motion < `1e-5`.
4. **Tet cube.** 4 x 4 x 4 tet block, `volume_compliance 0`, squeezed 30 % by a moving collider: total tet volume within
   5 % of rest; released, back within 2 % of its rest bounds.
5. **Shape matching.** A point set rotated by a known rotation, `stiffness 1`, no forces: the extracted rotation
   matches within `1e-4`; with no gravity the center of mass moves < `1e-5` m over 100 steps.
6. **Reach and standoff.** A pose-bound grid with `reach 0.05` under strong gravity stays within `0.05 + 1e-5` of
   its targets every step; `standoff 0` keeps every particle within `1e-4` in front of its target plane; `reach 0`
   equals the target bit-exactly.
7. **Blend.** Blend 0 equals the pose bit-for-bit; blend 1 with `sim_mix 1` equals the simulation; 0.5 gives the
   midpoint.
8. **Teleport.** A root jump over `teleport_distance` snaps to targets, zeroes velocity, enters `Settling`; the next 8
   steps keep every particle within `reach`.
9. **Self-collision.** A folded sheet: with `Spheres` the minimum non-excluded pair distance is `>= 2r - 1e-4` after
   settling; with `Off` some pair is closer than `r` (control); with `VertexTriangle` (built, ADR-2425,
   `tests/cloth_extras.rs`) no particle crosses to the
   other side of a triangle; `dropped_self_pairs == 0` on the fixture.
10. **Wind.** A free plate, gravity 0, `Aerodynamic { drag: 1, lift: 0 }`, 10 m/s along its normal: initial
    acceleration `0.5 * rho * A * 100 / m` within 1 %; parallel to the wind < 1 % of that; `Push` gives the same
    acceleration for any orientation; gusts for the same `(seed, step)` are identical across runs.
11. **Snapshots.** Cloth dropped on a rigid box ends outside it (`>= thickness - 1e-4`); permuted collider insertion
    gives the same digest; a moving sphere pushes the cloth and friction uses its velocity; terrain triangles cap at
    `snapshot_triangles_max`; a layer the cloth does not collide with is ignored.
12. **Tearing.** A 10 x 10 grid pulled with `strain 1.5` cuts the fixture's known edges in ascending order and honors
    `max_per_step`; without `tear` nothing is cut.
13. **LOD.** Levels change only past hysteresis; off fades over `blend_out_steps` then `Inactive`; active particles
    never exceed `ClothBudget`; LOD 1 maps equal LOD 0 maps resampled by nearest vertex.
14. **Map edits.** Each `MapEdit` on a fixture gives an expected array (stored in the test); `transfer_maps` by index
    and by nearest vertex; an edit through `physics.cloth_edit_map` and the same edit through the API give equal
    assets.
15. **Formats.** `.cloth.ron` -> cook -> load keeps particles, constraints and maps; a checked-in v1 `.cloth.bin`
    fixture loads; a newer version gives `cloth_unsupported_version`; cooking twice is byte-identical.
16. **Determinism.** Two runs with scripted inputs give equal digests every 50 steps; four solvers in parallel vs serial
    give equal digests; the known-answer digest for test 1 is pinned (re-pinned only with a stated reason).
17. **Agent.** In `regia_agent` tests: `physics.cloth_inspect` shape and bounds; `physics.cloth_step` count limits and
    the same digest as manual stepping; pin/unpin change `pinned`; wind moves bounds; unknown target gives
    `F0003`/`cloth_not_found`; no plugin gives `{ plugin: false }` / `cloth_plugin_missing`; `cargo xtask
    agent-coverage` passes.
18. **Zero allocation.** A counting allocator sees no heap allocation per step after warm-up (cloth and soft).
19. **Lead TODO (GPU or device):** a screenshot test of a caped character at blend 0 and 1 (golden images are blessed by
    the lead only); an Android run of test 1 and a character cape for 600 steps with no dropped steps at 60 Hz.

## 9. Platform notes

- **Linux, Windows:** CPU solver; identical results per build and platform; parallel across solvers on the task pool.
- **Android aarch64:** same code; `ClothBudget` 8,192 like every other target (ADR-4764) and LOD 0 default substeps 4 on this target; no heap allocation
  while stepping (per-solver arenas); no 64-bit atomics needed; velocities below `1e-12` are clamped to 0 (avoids
  denormals). Surface loss does not affect cloth; an app pause counts as `Paused`.
- **macOS** (testing only): same as Linux.
- **Dependencies:** none. The task pool is already pinned (`bevy_tasks`); the voxel tet method needs no crate.

## 10. Budgets

Throughput targets for one fixed step at 4 substeps, 1 iteration, grid topology, one collider, no self-collision,
measured by the lead with the tracing spans on the min-spec machines (`docs/06-testing-and-ci.md` § 8); they are
estimates until measured and are never asserted in unit tests:

| Case | Desktop min-spec | Android aarch64 (big core) |
|---|---|---|
| Cloth step | >= 2,000 particles/ms | >= 1,200 particles/ms |
| + `Spheres` self-collision | >= 800 particles/ms | >= 450 particles/ms |
| + `VertexTriangle` self-collision | >= 300 particles/ms | >= 180 particles/ms |
| Pose sync (CPU skin of the sim mesh, 4 influences) | >= 20,000 vertices/ms | >= 10,000 vertices/ms |
| `Tetra` soft body | >= 300 tets/ms | >= 180 tets/ms |
| `Pressure` / `ShapeMatch` | >= 1,500 / 1,000 particles/ms | half |

Frame budget (60 Hz, up to 2 fixed steps in a late frame): all cloth and soft bodies <= 1.0 ms mean and <= 2.0 ms
worst at the default `ClothBudget` with LOD on, plus the section mesh upload (§ 2.2) <= 0.1 ms per 5,000 render
vertices. Memory: <= 96 bytes per particle and <= 40 per constraint; <= 2 MB for a 5,000-particle cloth with snapshot
and grid. Tracing spans: `cloth_step`, `cloth_pose_sync`, `cloth_snapshot`, `cloth_wind`, `cloth_pose_constraints`,
`cloth_self_collide`, `cloth_collide`, `cloth_soft_step`, `cloth_lod`, `cloth_publish_mesh`.

## 11. Out of scope and follow-ups

GPU solving; learned or baked cloth; garment resizing across body shapes; hair strands; fluids, fracture and
re-meshed tearing; full two-way coupling; continuous collision against thin triangles; a node-graph cloth authoring
tool; garment pattern tailoring; garment import beyond glTF; automatic skin-weight transfer from a body to a sim mesh;
SIMD and multithreaded single-solver stepping; an in-place dynamic vertex buffer in the renderer (replaces the
per-frame `Arc<CpuMesh>` publish); angle-weighted wind splitting; better tet meshing.

## 12. Provenance

Algorithm sources (public sources only; the shared row is `docs/provenance.md`, "Cloth and soft body spec"):

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Solver core, compliance, volume constraints | XPBD with compliance; distance, tetra-volume constraints | Macklin, Müller, Chentanez, "XPBD: Position-Based Simulation of Compliant Constrained Dynamics", MIG 2016 |
| Constraint projection, pressure | Position based dynamics; distance, bend, closed-mesh volume constraints | Müller, Heidelberger, Hennix, Ratcliff, "Position Based Dynamics", JVCIR 18(2), 2007 |
| Shape matching | Best-fit rotation by polar decomposition, clusters, linear blend, plasticity | Müller, Heidelberger, Teschner, Gross, "Meshless Deformations Based on Shape Matching", SIGGRAPH 2005 |
| Substepping | Many substeps with one iteration | Macklin, Storey, Lu, Terdiman, Chentanez, Jeschke, Müller, "Small Steps in Physics Simulation", SCA 2019 |
| Tethers | Long-range attachments to fixed vertices limit stretch | Kim, Chentanez, Müller, "Long Range Attachments: A Method to Simulate Inextensible Clothing in Computer Games", SCA 2012 |
| Self-collision broad phase | Uniform grid of particles, counting sort | Teschner, Heidelberger, Müller, Pomeranets, Gross, "Optimized Spatial Hashing for Collision Detection of Deformable Objects", VMV 2003 |
| Vertex-triangle self-collision | Point-triangle proximity with thickness, barycentric repulsion, friction, inelastic contact | Bridson, Fedkiw, Anderson, "Robust Treatment of Collisions, Contact and Friction for Cloth Animation", SIGGRAPH 2002 |
| Wind | Flat-plate drag and lift, quadratic in relative speed | Anderson, *Fundamentals of Aerodynamics* (flat-plate model) |
| Map relax | Laplacian smoothing over mesh edges | Taubin, "A Signal Processing Approach to Fair Surface Design", SIGGRAPH 1995 |
| Tet generation | Cube split into 5 tetrahedra with alternating parity | Standard marching-tetrahedra cube decomposition (e.g. Treece, Prager, Gee, "Regularised marching tetrahedra", Computers & Graphics 1999) |

### Owner decisions

None.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
