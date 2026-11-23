# Spec 122 — Integer voxel core for deformable terrain (`regia_voxel_core`)

Status: proposed (ADR-8116, design lane do27-voxel-core-design, 2026-11-16). Nothing below is built. Line: later (v0.6
or after; DO27 does not need it for its next alpha). The public API is semver-versioned and **DO27 signs off on it and
on every change** (ADR-8012 A-5, spec 25 § 7.10). Sign-off state: § 18 (not signed yet). Build lanes wait for the
sign-off (§ 19).

Citations of Regia code and docs are `file:line` at `75e59c830` (the world-geometry-design-ff head). Spec 121 is the
world geometry contract this design extends (signed with exceptions E1-E7, amendment 1 pending DO27's re-sign, ADR-8084).

Related: spec 25 § 7.10 (the proposed boundary), `book/user-guide/do27-interface-contract.md` § 9 (the 2026-10-17
agreement), spec 121 (`regia_world_geom`: one source of query, collision and bake), spec 26 § 2a (terrain mesh layers:
seam, streaming, LOD, lighting, picks), spec 12 § 4a (frozen content formats).

## 1. Need and scope

DO27's deformable terrain stage D2 ("dig through, carve overhangs") needs a field that the lockstep sim owns and edits,
that is bit-identical on every machine and thread count, and that the engine meshes and draws. D1 (craters and berms on
the ground lattice) is spec 121 § 6 and stays there. Tunnels, overhangs and stacked levels that never change are
spec 121's canonical triangles. D2 is what neither can do: changing the topology of rock at run time.

The 2026-10-17 agreement (`book/user-guide/do27-interface-contract.md:635-677`) fixed the boundary: a pure-Rust crate,
no GPU, an integer-only API, a fixed-point CSG op log that replays bit-identically, a frozen file with a migrator, DO27
signing every API change, and a 1.5 GB go/no-go memory gate. Since then, ADR-8019 (spec 121) put sim collision in the
engine: the voxel core's answers must come through the same query DO27's sim already calls (`support`, `segment`,
`push_disc`), not beside it. This spec does both:

1. **`regia_voxel_core`** (new crate, § 3 to § 10, § 13): the field, the ops, the op log, replay, the hashes, the integer
   queries and the file. No dependency on any other Regia crate.
2. **The world geometry plug-in** (§ 11): `regia_world_geom` gains voxel volumes behind its query, its state hash and
   its cooked canonical section, the way spec 121 § 1 (`docs/specs/121-world-geometry.md:44`) and § 5.7 (`:868-870`)
   already promise ("D2 volumes plug into the same blocker and edit calls later").
3. **The presentation** (§ 12): `regia_terrain` meshes the same field with its dual contouring mesher through an
   integer-to-`SdfField` adapter, and each volume is a terrain mesh layer (spec 26 § 2a).

Not in scope: engine-physics colliders for hosts that want them (later, § 17); voxel-only worlds without a heightfield
(the crate supports them, the integration does not, § 17); smooth or blended CSG; GPU editing.

## 2. What exists (at `75e59c830`)

| Item | Where | What it gives this design |
|---|---|---|
| No voxel core crate | `crates/` has no `regia_voxel_core` | the crate is new |
| Dual contouring mesher | `crates/regia_terrain/src/volume_mesh.rs`: `SdfField` (`:114`, `sample` and an overridable `gradient`), `mesh_chunk` (`:1149`), `MeshParams` (`:207`), `NeighborLods` (`:267`), `ChunkMesh` (`:454`, positions, normals, indices; no material) | render meshing, chunk welding (`:26-38`), crack-free 2x LOD steps without skirts (`:40-77`), deterministic output (`:87-100`) |
| Welding precondition | `volume_mesh.rs:79-81`: exact welding needs chunk origins and cell sizes whose sums are exact in `f32`/`f64` | voxel edges and origins are dyadic in metres (§ 4) |
| Heightfield as a field | `TerrainMapField` (`volume_mesh.rs:155`) | the model of the adapter (§ 12.1) |
| Render-only volumes | `book/user-guide/volume-mesh.md` | no sim field, no edit log, no collision |
| World geometry contract | spec 121: the mm conversions (`:70-83`), the lattice and ground tiles (`:85-154`), `GeomState` copy-on-write blocks (`:377-384`), the O(1) state hash (`:613-636`), the canonical section rule (`:1080-1109`), the query rules (`:677-724`), the handshake versions (`:581-587`, `:1273-1282`) | the query, the hash fold, the handshake and the cook the voxel core joins (§ 11) |
| D2 hooks | spec 121 § 1 (`:44`), § 5.7 (`:868-870`); spec 26 § 2a.1 (`docs/specs/26-terrain-vfx-highend.md:97-99`), § 2a.9 Volume source (`:400-402`), § 2a.13 (`:456-458`) | the voxel core must reuse the blocker, edit and layer contracts |
| Mesh layer contract | spec 26 § 2a.3 (`TerrainMeshLayers`, `sync_state`, `:139-224`), § 2a.4 (lattice cut, ring, band, skirt, ring attach, `:226-296`), § 2a.5 (chunk LOD by the terrain's ranges, residency, `:298-318`) | a voxel volume is a mesh layer with run-time chunks (§ 12.2) |
| Sim-to-presentation edits | spec 25 § 7.4 `TerrainHeightEdits` (`docs/specs/25-sim-bridge-and-do27.md:200-226`) | the shape of the voxel hand-off (§ 12.3) |
| The name `.fvox` | taken: the cooked voxel far proxy, magic `FVOX`, v4 (`docs/formats/voxel-far.md:1-30`, spec 12 § 4a row `docs/specs/12-assets.md:292`, `crates/regia_render/src/visbuffer/voxel_far.rs:1347`) | the voxel core's file is renamed `.voxfield` (§ 13, ADR-8116) |
| Proposed-format register | `docs/formats/proposed-formats.ron`, checked by `xtask/src/proposed_formats.rs` | `voxel-field` is registered there by this design (§ 13.6) |
| BLAKE3 | workspace dependency (`Cargo.toml:99`) | `field_hash` (§ 8.1); no new dependency |

## 3. Principles and determinism rules

1. **Integer only.** No `f32` or `f64` anywhere in `regia_voxel_core`, not in its API and not inside it. A test reads
   the crate's own sources and fails on any float token (`crate_has_no_float_tokens`), and the crate joins `det-scan`'s
   lockstep-closure list with `regia_world_geom` (ADR-8012's planned scan; not in `xtask` at `75e59c830`). Floats
   appear only on the other side of the boundary: in `regia_world_geom`'s pinned conversions (spec 121 § 3, `m_to_mm`
   and `mm_to_m`) and in the render adapter (§ 12), which the sim never reads.
2. **Explicit widths.** Inputs are `i32` millimetres, `i16` distances and `u8` materials. Intermediates are the `i64`,
   `i128` or `u128` the formulas below name. No `usize` reaches a hashed, encoded or reported value. Every input is
   range-checked before arithmetic (§ 6.4), so no formula can overflow (the bounds are in § 6.4, and a test drives
   each formula at its bounds).
3. **One rounding rule.** Integer division rounds toward negative infinity (`div_euclid`, divisor positive). Square
   roots are the exact floor square root (`u128::isqrt`), which has one correct answer, so no algorithm is pinned.
4. **Fixed order.** Volumes in ascending id; bricks in ascending linear index (x fastest, then y, then z); voxels in a
   brick in ascending local index (x fastest). Ops are pointwise (§ 6.3), so the order never changes a value; it fixes
   the report, the encoding and the hash.
5. **No hash-map iteration**, no time, no randomness, no thread id in any result.
6. **Thread-count independence.** § 9.
7. **Canonical form.** A brick is stored dense if and only if it is not uniform, and a voxel's material is 0 (air) if
   and only if its distance is ≥ 0. So two fields with equal voxel values have equal bytes and equal `field_hash`,
   whatever history produced them.
8. **State, not history** (spec 121 E4). The field never keeps its ops. The op log is a separate value for tests,
   tools and replays (§ 7).
9. **Fully resident** on the sim side (spec 121 principle 5): the field never streams. Render meshes stream (§ 12).
10. **Bits version.** `VOXEL_BITS_VERSION` is bumped by every change that moves any bit `apply` or a query produces
    for the same inputs. The lockstep handshake compares it with `QUERY_BITS_VERSION` (spec 121 § 5.2) and
    `content_hash` (§ 11.4).

## 4. Model

- **Volume.** An axis-aligned box of whole bricks with its own voxel lattice. A map has 0 to 64 volumes. Volumes never
  overlap and never touch (the cook refuses it, `voxel_volumes_touch`); DO27 authors one larger volume instead of two
  adjacent ones.
- **Voxel edge** `voxel_mm` is one of 125, 250, 500 or 1000 mm, per volume. These are dyadic in metres (1/8, 1/4,
  1/2, 1), so the mesher's chunk welding is exact (`volume_mesh.rs:79-81`).
- **Lattice point** `(i, j, k)` of a volume is at world millimetres `origin_mm + (i, j, k) · voxel_mm`, with
  `0 ≤ i < 8 · bricks_x` (and so on). `origin_mm` is a multiple of `voxel_mm` on each axis. The volume's box is
  `[origin_mm, origin_mm + (8 · bricks − 1) · voxel_mm]`. Voxel values live on lattice points; between them the
  field is the trilinear interpolant of the eight surrounding values (§ 10.1).
- **Distance.** `i16` in field units of 1/4096 voxel (`DIST_ONE` = 4096), clamped to ±4 voxels (`DIST_CLAMP` =
  16 384). Negative is solid, zero or positive is air (the mesher's sign convention, `volume_mesh.rs:20-24`). At
  250 mm a unit is 0.061 mm.
- **Material.** `u8`. 0 is air; solid voxels carry 1 to 255. The host owns the meaning (DO27: rock types and dig
  hardness); the engine maps ids to render materials (§ 12.4).
- **Brick.** 8 × 8 × 8 lattice points (`BRICK_EDGE` = 8; 512 voxels; 1536 bytes dense). A brick slot is `Air` (every
  voxel `+DIST_CLAMP`, material 0), `Solid(m)` (every voxel `−DIST_CLAMP`, material `m`) or `Dense`. Bricks own their
  points once: there is no apron (the 2026-10-17 starting point had a one-voxel apron, which would duplicate voxels
  in the hash and the file and make one voxel write up to eight bricks; readers read across bricks instead).
  `BRICK_EDGE` is 8 and not the 32 of spec 25 § 7.10 because of the memory measurement in § 14 (ADR-8116; DO27
  question Q2).
- **Volume size.** 1 to 128 bricks per axis (up to 1024 lattice points: 256 m at 250 mm, 512 m at 500 mm). A voxel's
  linear index `i + nx · (j + ny · k)` is below 2^30.
- **The frozen rim.** The `rim` lattice points next to each of the six box faces are never written by an op (§ 6.1).
  `rim ≥ 4` (the clamp band) and is set by the cook so that the volume's mouth ring lies inside it (§ 11.2). The rim
  is what keeps every seam with the rest of the world at its cooked value forever: digging cannot reach a box face.
- **Outside the box.** A volume records `below` and `above`, the material of the space under and over its box inside
  its footprint (`below` ≥ 1: solid rock under the box; `above` = 0: air over it). The world geometry uses them to
  join the volume's column to the static column (§ 11.3). The core's own queries answer inside the box only.

## 5. API (`regia_voxel_core`)

```rust
//! Integer voxel fields for deformable terrain (spec 122). Pure Rust, no GPU, no floats, no other Regia crate.
#![forbid(unsafe_code)]

/// Field units per voxel edge: distances are stored in 1/4096 of a voxel.
pub const DIST_ONE: i32 = 4096;
/// Stored distances are clamped to ±4 voxels.
pub const DIST_CLAMP: i16 = 16_384;
/// Lattice points per brick edge.
pub const BRICK_EDGE: u32 = 8;
/// The voxel edges a volume may use, millimetres (dyadic in metres).
pub const VOXEL_MM_ALLOWED: [u32; 4] = [125, 250, 500, 1000];
/// Most bricks along one axis of a volume.
pub const MAX_BRICKS_PER_AXIS: u16 = 128;
/// Most volumes in one field.
pub const MAX_VOLUMES: usize = 64;
/// Largest sphere or capsule radius, millimetres.
pub const MAX_RADIUS_MM: i32 = 16_000;
/// Largest capsule length or box side, millimetres.
pub const MAX_EXTENT_MM: i32 = 32_000;
/// Most lattice points one op may evaluate after clipping (2^22).
pub const MAX_OP_VOXELS: u64 = 1 << 22;
/// The version of the bits `apply` and the integer queries produce for the same inputs (§ 3 rule 10).
pub const VOXEL_BITS_VERSION: u32 = 1;

/// A volume id, unique and ascending in a field (below 1024; at most `MAX_VOLUMES` volumes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VolumeId(pub u16);

/// A material id: 0 is air, 1..=255 are solid materials whose meaning the host owns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Material(pub u8);

/// One volume: a box of whole bricks on its own lattice (§ 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeSpec {
    /// Its id.
    pub id: VolumeId,
    /// Voxel edge, millimetres: one of `VOXEL_MM_ALLOWED`.
    pub voxel_mm: u32,
    /// World position of lattice point (0, 0, 0), millimetres; each component a multiple of `voxel_mm`.
    pub origin_mm: [i32; 3],
    /// Bricks per axis, each 1..=`MAX_BRICKS_PER_AXIS`.
    pub bricks: [u16; 3],
    /// Lattice points next to each box face that no op writes; 4 or more, and less than half of each axis.
    pub rim: u8,
    /// Material of the space under the box inside its footprint (0 = air).
    pub below: Material,
    /// Material of the space over the box inside its footprint (0 = air).
    pub above: Material,
}

/// A lattice point of one volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VoxelCoord { pub volume: VolumeId, pub x: u16, pub y: u16, pub z: u16 }

/// A brick of one volume (each component below the volume's brick count).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrickCoord { pub volume: VolumeId, pub x: u8, pub y: u8, pub z: u8 }

/// One stored voxel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Voxel {
    /// Signed distance in 1/4096 voxel, in `-DIST_CLAMP..=DIST_CLAMP`; negative is solid.
    pub dist: i16,
    /// 0 exactly when `dist >= 0` (§ 3 rule 7).
    pub material: Material,
}

/// A read view of one brick slot.
#[derive(Clone, Copy, Debug)]
pub enum BrickView<'a> {
    /// Every voxel `+DIST_CLAMP`, material 0.
    Air,
    /// Every voxel `-DIST_CLAMP` with this material.
    Solid(Material),
    /// 512 stored voxels, local index `x + 8 * (y + 8 * z)`.
    Dense(&'a [Voxel; 512]),
}

/// A sparse integer signed-distance field over one or more volumes. `Clone` is O(brick slots) and shares every dense
/// brick: the first write to a shared brick copies that brick only (copy-on-write, as spec 121's `GeomState`
/// override blocks), so a per-tick clone for prediction or for the presentation costs one pointer per dense brick.
#[derive(Clone)]
pub struct VoxelField { /* private: volume specs, per volume a slot table (u32 per slot), an Arc'd dense-brick
                          pool, the state term */ }

impl VoxelField {
    /// A field whose every brick is uniform: `Solid(fill)`, or `Air` when `fill` is 0. Validates the volumes
    /// (`voxel_bad_volume`, `voxel_volumes_touch`). State term 0.
    pub fn new(volumes: &[VolumeSpec], fill: Material) -> Result<VoxelField, FieldError>;
    /// The volumes, ascending by id.
    pub fn volumes(&self) -> &[VolumeSpec];
    /// One brick.
    pub fn brick(&self, at: BrickCoord) -> Result<BrickView<'_>, FieldError>;
    /// One voxel.
    pub fn voxel(&self, at: VoxelCoord) -> Result<Voxel, FieldError>;
    /// Replaces one brick (the cook and the editor; the sim uses `apply`). Refuses values that break § 3 rule 7
    /// (`voxel_bad_value`), stores the brick canonically and updates the state term.
    pub fn write_brick(&mut self, at: BrickCoord, voxels: &[Voxel; 512]) -> Result<(), FieldError>;

    /// Applies one op on the calling thread (§ 6). Writes the dirty bricks and counts into `report` (cleared
    /// first; reuses its buffer). An op that reaches no volume's editable points is `Ok` with an empty report.
    pub fn apply(&mut self, op: &Op, report: &mut OpReport) -> Result<(), OpError>;
    /// `apply` with up to `threads` scoped worker threads (1 = the calling thread only). The field, the report and
    /// the state term are identical for every thread count (§ 9).
    pub fn apply_threads(&mut self, op: &Op, threads: core::num::NonZeroUsize, report: &mut OpReport)
        -> Result<(), OpError>;

    /// BLAKE3 of the canonical encoding: the `.voxfield` body (§ 13.2). O(field). Equal voxel values give equal
    /// hashes (§ 3 rule 7).
    pub fn field_hash(&self) -> [u8; 32];
    /// The running state term, O(1): the XOR over every voxel of `term(now) ^ term(at creation or load)` (§ 8.2).
    /// 0 for a fresh or freshly loaded field; a function of the current voxels only.
    pub fn state_term(&self) -> u64;
    /// Resident bytes: slot tables, the dense-brick pool and the bricks (the § 14 gate figure).
    pub fn resident_bytes(&self) -> u64;
    /// Air, solid and dense brick counts per volume.
    pub fn brick_counts(&self) -> Vec<(VolumeId, BrickCounts)>;

    /// Appends the `.voxfield` v1 bytes (§ 13). Allocates only by growing `out`.
    pub fn encode(&self, out: &mut Vec<u8>);
    /// Reads a `.voxfield` file of any supported version (older versions through the migrator, § 13.4).
    pub fn decode(bytes: &[u8]) -> Result<VoxelField, VoxfieldError>;

    /// The trilinear sign at a point inside a volume's box (§ 10.1).
    pub fn solid_at(&self, volume: VolumeId, p_mm: [i32; 3]) -> Result<bool, FieldError>;
    /// Every surface crossing of the vertical line through `(x_mm, z_mm)` inside the box, ascending (§ 10.2).
    /// No allocation: `out` has a fixed capacity and sets `overflowed` past it.
    pub fn column(&self, volume: VolumeId, x_mm: i32, z_mm: i32, out: &mut CrossingBuf) -> Result<(), FieldError>;
    /// The first point of `from -> to` where the field is solid (§ 10.3), searched only inside the box and, when
    /// given, inside the half-open XZ rectangle `xz_clip_mm` (`[[x0, z0], [x1, z1]]`; the world geometry passes its
    /// domain rectangle). `t` is measured along the unclipped segment.
    pub fn segment(&self, volume: VolumeId, from_mm: [i32; 3], to_mm: [i32; 3], xz_clip_mm: Option<[[i32; 2]; 2]>)
        -> Result<Option<SegmentCrossing>, FieldError>;
    /// Pushes a vertical disc out of solid by radial probes (§ 10.4), each probe searched as `segment` with the same
    /// optional XZ clip.
    pub fn probe_disc(&self, volume: VolumeId, center_mm: [i32; 3], radius_mm: i32, band_mm: [i32; 2],
        xz_clip_mm: Option<[[i32; 2]; 2]>) -> Result<DiscPush, FieldError>;
}

/// Air, solid and dense brick counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BrickCounts { pub air: u32, pub solid: u32, pub dense: u32 }

/// One CSG edit in world integer millimetres (§ 6). It applies to every volume its extent reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op { pub mode: CsgMode, pub shape: Shape }

/// What an op does to the voxels its shape covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsgMode {
    /// Subtract: rock inside the shape becomes air ("dig", "carve").
    Carve,
    /// Union: air inside the shape becomes rock of this material (1..=255).
    Fill(Material),
    /// Change the material of rock inside the shape (1..=255); no distance changes.
    Paint(Material),
}

/// The op shapes (integer millimetres; § 6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// A ball: `radius_mm` in 1..=`MAX_RADIUS_MM`.
    Sphere { center_mm: [i32; 3], radius_mm: i32 },
    /// A swept ball between `a_mm` and `b_mm` (a bore, a drill path): `|b - a| <= MAX_EXTENT_MM`.
    Capsule { a_mm: [i32; 3], b_mm: [i32; 3], radius_mm: i32 },
    /// An axis-aligned box: `min_mm < max_mm` on each axis, each side `<= MAX_EXTENT_MM`.
    Box { min_mm: [i32; 3], max_mm: [i32; 3] },
}

/// What one `apply` changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpReport {
    /// The bricks whose voxels changed, ascending (volume, linear index).
    pub dirty: Vec<BrickCoord>,
    /// Voxels whose value changed.
    pub changed_voxels: u32,
    /// True when the frozen rim kept the op from writing part of its extent.
    pub clipped_by_rim: bool,
}

/// An ordered list of ops (§ 7): tests, tools, replays. Not part of the field's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpLog { /* Vec<Op> */ }

impl OpLog {
    /// Appends one op.
    pub fn push(&mut self, op: Op);
    /// The ops in order.
    pub fn ops(&self) -> &[Op];
    /// Appends the byte-stable encoding (§ 7.1).
    pub fn encode(&self, out: &mut Vec<u8>);
    /// Reads an encoding; refuses a newer version, a bad record or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<OpLog, OpLogError>;
}

/// Applies `log` to a clone of `base` in order and returns the result. Stops at the first op that fails and names
/// its index. The result is identical for every `threads` value (§ 9).
pub fn replay(base: &VoxelField, log: &OpLog, threads: core::num::NonZeroUsize) -> Result<VoxelField, ReplayError>;

/// A surface crossing of a vertical line (§ 10.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crossing {
    /// Height, millimetres (floor rounding).
    pub y_mm: i32,
    /// Solid below and air above (a floor), or air below and solid above (a roof).
    pub kind: CrossingKind,
    /// The trilinear gradient at the crossing, as integers with a common positive scale (the direction only).
    pub gradient: [i64; 3],
    /// The material of the solid side (the nearest solid lattice point of the crossing's cell edge).
    pub material: Material,
}

/// Floor or roof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossingKind { Floor, Roof }

/// Fixed-capacity crossing list: 32 crossings plus `len` and `overflowed`.
#[derive(Clone, Debug, Default)]
pub struct CrossingBuf { /* [Crossing; 32] + len + overflowed */ }

/// The first solid point along a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentCrossing {
    /// Fraction along `from -> to` in units of 2^-32 (0..=2^32).
    pub t_q32: u64,
    /// The point, millimetres (floor rounding).
    pub point_mm: [i32; 3],
    /// The trilinear gradient there (direction only).
    pub gradient: [i64; 3],
}

/// The result of `probe_disc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscPush {
    /// The pushed centre XZ, millimetres.
    pub xz_mm: [i32; 2],
    /// Probes that moved it (0: `xz_mm` equals the input).
    pub contacts: u16,
}

/// Field errors (agent codes in parentheses).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FieldError {
    /// (`voxel_bad_volume`) a spec breaks § 4: voxel edge, origin, brick counts, rim, ids, materials.
    BadVolume { id: u16, reason: &'static str },
    /// (`voxel_volumes_touch`) two volumes overlap or share a face, edge or corner.
    VolumesTouch { a: u16, b: u16 },
    /// (`voxel_too_many_volumes`) more than `MAX_VOLUMES`.
    TooManyVolumes,
    /// (`voxel_unknown_volume`)
    UnknownVolume(VolumeId),
    /// (`voxel_out_of_volume`) a coordinate or point outside the volume.
    OutOfVolume,
    /// (`voxel_bad_value`) a voxel breaks § 3 rule 7, or a distance outside the clamp.
    BadValue,
}

/// Op errors (agent codes in parentheses). An error leaves the field unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    /// (`voxel_op_bad_shape`) a radius, extent or box outside § 6.4's bounds.
    BadShape,
    /// (`voxel_op_bad_material`) `Fill(0)` or `Paint(0)`.
    BadMaterial,
    /// (`voxel_op_too_large`) more than `MAX_OP_VOXELS` lattice points after clipping.
    TooLarge,
}
```

`OpLogError`, `ReplayError` and `VoxfieldError` carry the codes of § 7 and § 13.5. `CrossingBuf` holds 32 crossings
because spec 121's `SpanBuf` holds 16 spans (`docs/specs/121-world-geometry.md:481-485`) and each span has a floor and
a roof.

## 6. Ops in fixed point

### 6.1 Which lattice points an op evaluates

The op's **extent** is the shape's axis-aligned bounding box grown by `4 · voxel_mm` (the clamp band) on every side,
in millimetres. Outside it the shape's distance is at least `DIST_CLAMP`, so `Carve`, `Fill` and `Paint` cannot change
a voxel there (§ 6.3): skipping it is exact. For each volume in ascending id, the evaluated points are the lattice
points inside the extent and inside the **editable range** `[rim, n − rim)` on every axis. When the extent reaches
into the rim, `clipped_by_rim` is set. The count of evaluated points is checked against `MAX_OP_VOXELS` for all
volumes together before any write (`TooLarge`).

### 6.2 Shape distance in field units

For a lattice point `p` (world millimetres, `i64`) of a volume with edge `v = voxel_mm`, each shape gives `d(p)` in
field units, clamped to `[−DIST_CLAMP, DIST_CLAMP]`. `isqrt` is `u128::isqrt`, and `S = 2^24` (so
`isqrt(X · S) = floor(√X · 4096)`).

- **Sphere** `(c, r)`: `X = |p − c|²` (`i64`, then `u128`). `d = (isqrt(X · S) − r · 4096).div_euclid(v)`.
- **Capsule** `(a, b, r)`: `e = b − a`, `w = p − a`, `num = w·e`, `den = e·e` (`i128`).
  If `den == 0` or `num ≤ 0`, `X = |w|²`; if `num ≥ den`, `X = |p − b|²`; both as for the sphere.
  Otherwise the squared distance to the segment is `(|w|² · den − num²) / den`, and
  `d = (isqrt(((|w|² · den − num²) · S).div_euclid(den)) − r · 4096).div_euclid(v)`. The inner floor division is
  exact for the floor square root: `⌊√⌊y⌋⌋ = ⌊√y⌋` for real `y ≥ 0`.
- **Box** `(min, max)`, in half millimetres so the centre is an integer: per axis
  `q_i = |2 p_i − (min_i + max_i)| − (max_i − min_i)`. If any `q_i > 0`:
  `d = isqrt((Σ max(q_i, 0)²) · S).div_euclid(2 v)` (outside, Euclidean); else `d = (max_i q_i · 4096).div_euclid(2 v)`
  (inside, the nearest face).

Each formula is the exact signed distance to the shape (Euclidean outside; for the box, the nearest face inside),
rounded down to a field unit. Standard vector geometry (point to segment and point to box: Ericson, *Real-Time
Collision Detection*, 2005, ch. 5); truncated signed distances after Curless and Levoy, "A Volumetric Method for
Building Complex Models from Range Images", SIGGRAPH 1996.

### 6.3 The CSG rules (per evaluated point; `q` and `m` are the stored distance and material, `d` the shape's)

| Mode | New distance | New material |
|---|---|---|
| `Carve` | `q' = max(q, −d)` | `m' = 0` if `q' ≥ 0`, else `m` |
| `Fill(f)` | `q' = min(q, d)` | `m' = 0` if `q' ≥ 0`; else `f` if `d < q`; else `m` |
| `Paint(f)` | `q' = q` | `m' = f` if `d < 0` and `q < 0`, else `m` |

Each rule reads only the point's own value and the shape, so the result does not depend on the order points are
visited (§ 9). The material rules keep § 3 rule 7. A `Fill` whose shape distance ties the stored one keeps the old
material (`d < q` is strict). Min and max of signed distances are the standard CSG union and subtraction of implicit
solids (Ricci, "A Constructive Geometry for Computer Graphics", The Computer Journal, 1973).

### 6.4 Bounds (each checked before any write)

| Input | Bound | Error |
|---|---|---|
| radius | 1..=16 000 mm | `BadShape` |
| capsule length, box side | ≤ 32 000 mm; box `min < max` per axis | `BadShape` |
| any coordinate | within ±2^24 mm (spec 121's `geom_extent_too_large` range) | `BadShape` |
| `Fill`, `Paint` material | 1..=255 | `BadMaterial` |
| evaluated points | ≤ 2^22 | `TooLarge` |

With these, the largest intermediate is the capsule's `(|w|² · den − num²) · S`: `|w|` and `|e|` are below 2^17 mm
within an extent (32 m + 16 m + 4 voxels), so `|w|² · den < 2^70` and the product is below 2^94 in `u128`. Every
other formula is smaller. A test evaluates each formula at its bounds and at the volume corners.

### 6.5 Canonicalization after an op

After the writes, every brick the op touched is checked on the calling thread in ascending order: a dense brick whose
voxels are all `+DIST_CLAMP` (material 0) becomes `Air`, all `−DIST_CLAMP` with one material becomes `Solid(m)`. Every
uniform brick the extent reaches is made dense before the writes, also on the calling thread in ascending order (§ 9),
and one the op left unchanged collapses back here. `OpReport::dirty` lists the bricks with a changed voxel, ascending.

## 7. Op log and replay

### 7.1 Encoding (a wire and test payload, not a content file)

Little-endian. `VXOL` (4 bytes), `version: u32` (1), `count: u32`, then `count` records of 32 bytes: `mode: u8`
(0 carve, 1 fill, 2 paint), `material: u8` (0 for carve), `shape: u8` (0 sphere, 1 capsule, 2 box), `reserved: u8`
(0), then seven `i32`: sphere `cx, cy, cz, r, 0, 0, 0`; capsule `ax, ay, az, bx, by, bz, r`; box
`min x, y, z, max x, y, z, 0`. `decode` refuses a newer version (`oplog_newer_version`), an unknown mode or shape, a
non-zero reserved or padding field, a record that fails § 6.4 (`oplog_bad_op`, with its index) and trailing bytes
(`oplog_corrupt`). Like spec 121's `GeomState` snapshot (`docs/specs/121-world-geometry.md:629-633`), a lane that
stores it as a file registers it as a format first. DO27's replays record inputs and re-run the sim, so they do not
need it.

### 7.2 Replay

`replay(base, log, threads)` clones `base` and applies each op in order with `apply_threads`. It is the reference
for "the same ops give the same field": the first-slice acceptance (§ 16, V-01) replays a 10 000-op log and compares
`field_hash` and `state_term` with committed constants at 1 and 16 threads, on x86-64 and aarch64.

### 7.3 The test log generator

The 10 000-op log is generated, not stored: SplitMix64 (Steele, Lea and Flood, "Fast Splittable Pseudorandom Number
Generators", OOPSLA 2014) from the seed `0x5EED_D2D2_0000_0001`, drawing for each op a mode (60 % carve, 30 % fill,
10 % paint), a shape (50 % sphere, 35 % capsule, 15 % box), integer centres inside the fixture volumes and radii
250-6000 mm, by integer arithmetic only (modulo and shifts). The generator is part of the test, so its stream is
pinned with the hash. A 100-op log is also committed as bytes (`tests/fixtures/ops_100.vxol`) to pin `OpLog::decode`.

## 8. Hashes

### 8.1 `field_hash`

BLAKE3 of the `.voxfield` body (§ 13.2): the volume table, the slot tables and the dense bricks in canonical order.
`field_hash()` equals the body hash stored in the file's header. O(field); for tests, tools, the cook and the
handshake's content hash (§ 11.4), never per tick.

### 8.2 The state term (per tick, O(1))

Zobrist hashing (Zobrist, "A New Hashing Method with Application for Game Playing", 1970), as spec 121's state hash
(`docs/specs/121-world-geometry.md:613-636`), per voxel:

- `key(volume, index, dist, material) = index | volume << 30 | (dist as u16 as u64) << 40 | material << 56`, where
  `index` is the voxel's linear index in its volume (below 2^30) and `volume` its id (below 1024). The packing is
  one-to-one.
- `term(key) = mix(key + 0x9E37_79B9_7F4A_7C15)` (wrapping add), with `mix` the SplitMix64 finalizer:
  `z = (z ^ (z >> 30)) · 0xBF58_476D_1CE4_E5B9; z = (z ^ (z >> 27)) · 0x94D0_49BB_1331_11EB; z ^ (z >> 31)` (wrapping
  multiplies). `mix` is a bijection, so distinct keys give distinct terms.
- Every voxel change from `(q, m)` to `(q', m')` does `acc ^= term(old) ^ term(new)`. So `acc` is the XOR over all
  voxels of `term(now) ^ term(at creation or load)`: a function of the current voxels and the base only (two op
  sequences that reach the same field give the same `acc`), 0 when nothing differs from the base, and O(changed
  voxels) per op. XOR is commutative, so worker threads keep partial `acc` values that the calling thread XORs
  together (§ 9).
- `state_term()` returns `acc`.

In the world geometry, `GeomState` XORs `state_term` into its own running accumulator (§ 11.4), so a map without
volumes keeps spec 121's fresh-state constant and every Stage B pin.

## 9. Threads

`apply_threads(op, threads, report)`:

1. On the calling thread: validate (§ 6.4), list the touched bricks of every volume in ascending (volume, linear
   index), make the uniform ones dense (copying shared ones, copy-on-write), in that order.
2. Split the list into `threads` contiguous runs of nearly equal length (the first `len % threads` runs get one more).
   Each worker writes only the bricks of its run and returns its changed-voxel count, its partial `acc` and whether it
   met the rim. Workers are `std::thread::scope` threads (no new dependency); `threads = 1` runs on the calling thread.
3. On the calling thread: XOR the partial accumulators, add the counts, canonicalize the touched bricks in ascending
   order (§ 6.5) and fill the report.

Every voxel's new value depends only on its old value and the op (§ 6.3), each brick is written by exactly one worker,
the merge is XOR and addition, and the allocation and collapse steps run on one thread in a fixed order. So the field,
the report and the state term are identical for every thread count. Where a dense brick sits in the pool never
reaches any output. Pins: § 16 V-01 (1, 2, 3 and 16 threads).

## 10. Integer queries (inside one volume's box)

All inputs are world millimetres (`i32`); the world geometry converts the sim's `f32` metres with `m_to_mm` (spec 121
§ 3) and reads the answers back with `mm_to_m`. A point outside the box is `OutOfVolume`.

### 10.1 Trilinear value

For a point `p` in cell `(i, j, k)` with fractional offsets `f = p − lattice(i, j, k)` (`0 ≤ f_a < v`), the value
scaled by `v³` is `T(p) = Σ_corners q_c · Π_a (f_a or v − f_a)` in `i64` (`|q| ≤ 2^14`, `v³ ≤ 2^30`, eight terms:
below 2^48). `solid_at(p)` is `T(p) < 0`. On a lattice point it is the stored sign. This interpolant is the voxel
solid the sim stands on; the renderer's surface is projected onto it (§ 12.1).

### 10.2 Column

At fixed `(x, z)` the trilinear interpolant is linear in `y` inside each cell, so the column is exact: with
`L_k = Σ_4 q · (f_x or v − f_x) · (f_z or v − f_z)` at lattice level `k` (scale `v²`), each pair of levels with
`L_k < 0 ≤ L_{k+1}` is a **floor** at `y_k + (−L_k · v).div_euclid(L_{k+1} − L_k)`, and each pair with
`L_k ≥ 0 > L_{k+1}` is a **roof** at `y_k + (L_k · v).div_euclid(L_k − L_{k+1})` (millimetres, floor rounding).
Uniform bricks are skipped as blocks (an air brick over a solid brick still yields the crossing between them). The
gradient is the trilinear gradient at `(x, y_k, z)` and `(x, y_{k+1}, z)` summed (integers, direction only).
Crossings are ascending; `out.overflowed` is set past 32.

### 10.3 Segment

`from -> to` is clipped to the box (and to `xz_clip_mm` when given; the clip is exact rational arithmetic on the
parameter, and `t` stays measured along the unclipped segment) and walked cell by cell with a 3D DDA in exact rational arithmetic (Amanatides and
Woo, "A Fast Voxel Traversal Algorithm for Ray Tracing", Eurographics 1987): the parameter is `u` in units of 2^-32,
the next cell boundary on each axis is compared by cross-multiplication in `i128`, and an exact tie steps x, then y,
then z. In each cell:

- all eight corners `≥ 0`: no solid in the cell (the interpolant is a convex combination), skip;
- all eight `< 0`: the cell is solid, the hit is the entry point;
- otherwise: evaluate `T` at 9 evenly spaced parameters across the cell (positions in 1/256 mm, `i128`), take the
  first sign change, then bisect that interval 10 times on the integer parameter; the hit is the first solid end.

A solid start point is a hit at `t = 0`. A sliver thinner than a ninth of the cell chord can be missed; that is
deterministic and documented, and the clamped field cannot hold features under a voxel anyway.

### 10.4 Disc probe (wall push)

The clamped field cannot see a wall farther than 4 voxels, and hull radii reach 4.5 m (spec 121 § 10), so the push
does not read distances. `probe_disc(c, r, band)` instead casts `PROBE_DIRS = 16` horizontal rays of length `r` from
the centre (a committed table of 16 integer unit vectors, `(cos, sin)` of `k · 22.5°` times 2^14, rounded to nearest)
at `PROBE_HEIGHTS = 3` heights (`band.lo + (band.hi − band.lo) · {1, 2, 3} / 4`, floor), in order (height ascending,
then direction index). A ray from centre `c` ends at `c + (dir · r) >> 14` (arithmetic shift, floor). Each ray uses § 10.3 from the **current** centre (each probe sees the position the previous
one left, as T3's disc push sees each wall, spec 121 § 5.4); a hit at distance `s < r` moves the centre by `r − s`
against the ray. One pass, no iteration. The two constants are a performance and fit choice that V-03 measures; a
change before DO27's sign-off is free, after it is a § 18 change with a bits-version bump.

## 11. The world geometry plug-in (`regia_world_geom`, D2 amendment, proposed)

Spec 121 is signed with exceptions and amended (ADR-8084, re-sign pending). This section is a **proposed D2
amendment** to it. It changes nothing for a map without volumes: no bit, no hash, no pin (§ 11.6). It lands behind a
`d2_voxels` cargo feature of `regia_world_geom`, additive only, under the same rules as `stage_b`
(`docs/specs/121-world-geometry.md:1262-1271`).

### 11.1 Source

`MeshLayersDocument` (spec 121 § 9.1) gains one optional field, defaulted to empty, so the source version stays 1
(spec 25 § 7.1's additive rule):

```rust
/// Voxel volumes (spec 122): the boxes where D2 edits may change the rock. Omitted = none.
#[serde(default)]
pub voxel_volumes: Vec<VoxelVolumeSource>,

/// One voxel volume in the source.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoxelVolumeSource {
    /// 1..=64 ASCII letters, digits, `_`, `-`, `.` (the named-hole id rule).
    pub id: String,
    /// The box's minimum corner, world millimetres (snapped outward to whole bricks by the cook).
    pub min_mm: [i32; 3],
    /// The box's maximum corner, world millimetres (snapped outward the same way).
    pub max_mm: [i32; 3],
    /// Voxel edge, millimetres (125, 250, 500 or 1000; default 250).
    pub voxel_mm: u32,
    /// Material ids of the rock: by depth under the cooked ground, millimetre bands, top first; the last band
    /// continues down. Default: one band of material 1.
    pub strata: Vec<(i32, u8)>,
    /// Material of the layer surfaces voxelized into the volume (default 1).
    pub layer_material: u8,
    /// Ops applied once at cook after voxelization (authored caves, pre-dug pits): a list in the § 7.1 encoding.
    pub ops: Option<String>,
    /// Named hole id the volume's footprint opens in the terrain (default: the volume id).
    pub hole: Option<String>,
    /// Render: the material per voxel material id (§ 12.4), the pick layer and shadows, as for a mesh layer.
    #[serde(default)]
    pub look: VoxelLook,
}

/// How a voxel volume draws (presentation only; never in the canonical section).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VoxelLook {
    /// Render material per voxel material id; an unmapped id draws `default_material`.
    pub materials: Vec<(u8, String)>,
    /// The material of unmapped ids (empty: the engine's default rock material).
    pub default_material: String,
    /// Pick layer of the chunk entities (`MapLayer`); default 0.
    pub map_layer: u8,
    /// Chunks cast shadows (default true).
    pub cast_shadows: bool,
    /// Skirt under the LOD-0 clip loop at the ring, metres (default 0.25, as the band's).
    pub skirt_m: f32,
}
```

### 11.2 Cook

1. **Snap and validate.** The box is snapped outward to whole bricks on the lattice of `voxel_mm` (origin a multiple
   of `voxel_mm`). The cook fails if volumes touch (`voxel_volumes_touch`), if a volume is over 128 bricks on an axis
   (`voxel_volume_too_large`), or if the LOD-0 ground inside the footprint is not at least `rim + 1` lattice points
   above the box bottom and below its top (`voxel_ground_outside_box`).
2. **Layers.** A canonical triangle (spec 121 § 9.2) is either wholly inside a volume's box, wholly outside it, or
   the cook fails (`layer_crosses_voxel_volume`). DO27 authors a volume around a whole tunnel system, or not at all.
   Triangles inside a volume move into its field (item 4) and leave the canonical set. The sections, portals and
   zones cooked from them stay (exposure, reverb and VisCells of the authored passages; digs do not update them).
   A blocker plug inside a volume is refused (`blocker_in_voxel_volume`; Q6: a collapse inside a volume is a `Fill`
   op).
3. **The domain rectangle and the render cut.** The footprint rectangle inset by
   `inset_mm = ⌈lattice diagonal⌉ + voxel_mm` on each side is the volume's **domain rectangle** `R` (integer
   millimetres, half-open `[x0, x1) × [z0, z1)`). It is the exact sim partition (§ 11.3). For the renderer, `R` becomes
   a `HoleShape::Rect` named hole (`holes.rs:188`) that the volume fills; its lattice cut, ring and mouth band are
   cooked exactly as a mesh layer's (spec 26 § 2a.4 items 2 and 3). A cut triangle's centroid is inside `R`, so the
   render cut differs from `R` by at most one lattice diagonal and lies inside the box by at least `voxel_mm`. In that
   band both the ground and the field are the cooked ground (the rim is frozen and D1 edits are refused over the
   footprint, § 11.4), equal within `voxel_seam_mm` (item 6). The sim does not use the cut bitmap for volumes: its
   partition is `R`, an integer test with no lattice snapping. `rim` is set to
   `max(4, ⌈(2 · ⌈lattice diagonal⌉ + 2 · voxel_mm) / voxel_mm⌉)` lattice points, so `R`'s boundary, the ring and
   every cell they touch lie inside the frozen rim (Switchbacks, 0.442 m diagonal at 250 mm: 6).
4. **Voxelization.** Each lattice point's distance is the clamped signed distance to the static solid: solid when it
   is under the cooked ground and in no free span of the static column (`floors_at` of the canonical set before the
   triangles left it; spec 121 § 5.3), magnitude the distance to the nearest ground lattice triangle or canonical
   triangle within 4 voxels. This step may use IEEE `f64` `+ − × ÷` and `sqrt` in a fixed order (no fused
   multiply-add, no other function), then `round_ties_even` to field units, as spec 121's runtime query uses IEEE
   `f32` (`docs/specs/121-world-geometry.md:52-55`); the output is integer and byte-stable, and V-05 pins
   `cook_twice_gives_identical_bytes` and the cooked hash on x86-64 and aarch64. Materials: canonical layer surfaces'
   solid side gets `layer_material`; other solid points take `strata` by depth under the ground at their XZ.
5. **Authored ops** (`ops`) are applied with `apply` at cook, rim included (the rim is frozen for run-time ops only).
6. **Seam check `voxel_ring_step`.** At every point where a lattice edge crosses `R`'s boundary, at every ring vertex
   and at every ring-edge midpoint, the height of the ground lattice and the floor of the field's column (§ 10.2)
   differ by at most `voxel_seam_mm` (default 10 mm, a `GeomSettings` field).
   The step is fixed forever by the frozen rim. It is a cook error, reported with the worst point and value. 10 mm is
   well inside DO27's 50 mm per-tick hop gate (spec 121 E3); voxelization of the planar lattice triangles is exact to
   a field unit away from their creases, and the measured value goes in `terrain.bake_report`.
7. **Outputs.** `<map>.voxfield` (§ 13), written once per cook. The cooked `<map>.meshlayers` canonical section (spec
   121 § 9.2) gains, after the blocker plugs: `u32` volume count, then per volume its `VolumeSpec` fields, its hole id,
   and the 32-byte `field_hash` of the `.voxfield`. So `content_hash` covers the voxel base (spec 121's rule that every
   byte the run time reads is hashed, ADR-8084 W1).

### 11.3 Queries

`WorldGeom::from_cooked_with_voxels(geom: &[u8], voxfield: &[u8])` loads both and refuses a `.voxfield` whose body
hash is not the one in the canonical section (`geom_voxels_mismatch`). `GeomState` holds the `VoxelField` (it is
`Clone` by copy-on-write bricks, so spec 121 § 5.1's cheap clone holds).

The query rule is a **partition** by the domain rectangle `R` (§ 11.2 item 3), tested on `m_to_mm` of the query
point's x and z: inside `R` and between the box bottom and top the field is the only solid (the ground lattice is
skipped, and no canonical triangle is there); everywhere else the static world (ground lattice and canonical
triangles) is, unchanged. In a column inside `R` the field's `below` material fills the space under the box and
`above` the space over it, and static triangles wholly above or below the box keep counting. Every field query is
clipped to `R` × the box's height range, so a field surface outside `R` (the cooked ground in the rim) never answers.

- `support`, `surface_at`, `floors_at`, `ceiling_above`, `covered_at`, `clamp_under_ceiling`: the column kernel (spec
  121 § 5.3 rule 4) takes the field's crossings (§ 10.2) as extra floors and lids after the static candidates, in
  ascending height, converted with `mm_to_m`. A new `SurfaceKind::Voxel` marks them; `SurfaceHit::tri` is `None`, and
  `SurfaceHit::material` is the voxel material. At equal height the static candidate stands (it comes first in
  candidate order). The span rules, `same_height_m` and `lid_gap_m` are unchanged.
- `segment`, `rock_between`, `line_of_sight`: the static DDA (spec 121 § 5.5), with ground hits inside `R` dropped,
  plus the field's segment (§ 10.3) on the part of the segment inside each `R` × height range (exact rational
  clipping on the Q32 parameter); the smallest `t` wins (the field's `t_q32` converted as
  `(t_q32 as f64 / 2^32) as f32`), ties to the static hit. A voxel hit reports `SurfaceKind::Voxel`.
- `push_disc`, `push_capsule`: the static wall pass first (spec 121 § 5.4), then `probe_disc` (§ 10.4) from the
  pushed position when the disc reaches `R`, each probe ray clipped to `R`. `Push::contacts` adds the probe
  contacts.
- `support_for_placement`: unchanged rule; voxel floors are candidates like layer floors.

Pinned like spec 121's: a **voxel query table** over the fixtures (support, surface_at, segment, push at fixed points,
`f32` bits) hashed and equal on every § 13.2 runner of spec 121, and `query_table_is_the_same_without_volumes` (the
Stage B table on a map with no volumes is unchanged by the `d2_voxels` feature).

### 11.4 State, edits and the handshake

```rust
/// A D2 edit: one voxel op in world integer millimetres (the sim converts with `m_to_mm`).
pub type VoxelOp = regia_voxel_core::Op;

/// What a voxel edit changed, for the presentation (§ 12.3), nav tiles and sound.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoxelDirty {
    /// Dirty bricks, ascending (volume, linear index).
    pub bricks: Vec<regia_voxel_core::BrickCoord>,
    /// Their world-millimetre bounding box (inclusive min, exclusive max).
    pub bounds_mm: [[i32; 3]; 2],
}

impl GeomState {
    /// Applies one voxel op (§ 6) to every volume it reaches and bumps the generation once when anything changed.
    /// Allocates only when a brick becomes dense or a shared brick is first written (copy-on-write).
    pub fn apply_voxel_op(&mut self, geom: &WorldGeom, op: &VoxelOp, out: &mut VoxelDirty)
        -> Result<Generation, GeomEditError>;
    /// The voxel field (read-only; the presentation meshes it).
    pub fn voxels(&self) -> Option<&regia_voxel_core::VoxelField>;
}
```

- `GeomEditError` gains `VoxelOp(OpError)` with the § 5 codes. A D1 `GroundEdit` that would write a lattice vertex
  inside a volume's box footprint is refused (`edit_in_voxel_volume`): the ground there is the field's, and the
  cooked ground between `R` and the box faces must stay equal to the field's frozen rim.
- **State hash.** Spec 121's accumulator `acc` (`docs/specs/121-world-geometry.md:619-624`) is XORed with the field's
  `state_term` (it is kept in step on every voxel op). A fresh state still has `acc = 0`, so the fresh-state constant
  and every pin of a map without volumes are unchanged. The fold, the tags and the field order of spec 121 stay as
  they are. This is a § 14 rule 5 bit change only for maps with volumes, which have no earlier bits.
- **Snapshot.** `GeomState::encode` (spec 121 § 5.2 item 5) appends, after the vertex overrides: `u32` count of dense
  or uniform bricks that differ from the base, then each as (volume id `u16`, linear index `u32`, slot tag `u8`,
  material `u8`, and for a dense brick its 1536 bytes), ascending. The snapshot version goes to 2 (the snapshot is a
  wire payload; version 1 snapshots are those of maps without volumes and still decode).
- **Handshake.** The lockstep handshake (N224) compares `(content_hash, QUERY_BITS_VERSION,
  regia_detsim::MATH_BITS_VERSION, regia_voxel_core::VOXEL_BITS_VERSION)`. `content_hash` covers the voxel base
  (§ 11.2 item 7).

### 11.5 Cook checks added to spec 121 § 10

| Code | Rule |
|---|---|
| `voxel_volumes_touch` | volumes overlap or touch |
| `voxel_volume_too_large` | more than 128 bricks on an axis, or more than 64 volumes |
| `voxel_ground_outside_box` | the ground inside the footprint is within `rim` points of the box bottom or top |
| `layer_crosses_voxel_volume` | a canonical triangle is partly inside a volume's box |
| `blocker_in_voxel_volume` | a section with a blocker plug is inside a volume |
| `voxel_ring_step` | ground-to-field step at the ring over `voxel_seam_mm` (default 10 mm) |
| `voxel_over_budget` | the map's `resident_bytes` over the voxel budget (§ 14; default 1.5 GiB) |

### 11.6 What does not change

A map without `voxel_volumes`: the same canonical section bytes (the volume count is written only when the list is
non-empty, so `content_hash` is unchanged), the same queries, the same state hash, the same snapshot (version 1).
Pinned by `no_volumes_is_stage_b_bit_for_bit` (the Stage B query table and a 1000-edit state hash run with and without
`d2_voxels`).

## 12. The engine side (`regia_terrain`)

### 12.1 The adapter

```rust
/// [`SdfField`] over one volume of a [`regia_voxel_core::VoxelField`] (spec 122 § 12.1). Render only: the sim never
/// reads it. A chunk LOD's samples fall on lattice points by construction (cell size `voxel_m · 2^l`), so no
/// downsampled copy exists.
#[derive(Clone, Copy)]
pub struct VoxelSdf<'a> {
    /// The field (the presentation's copy-on-write clone of the sim's).
    pub field: &'a regia_voxel_core::VoxelField,
    /// The volume.
    pub volume: regia_voxel_core::VolumeId,
}

impl SdfField for VoxelSdf<'_> {
    /// The trilinear interpolant of § 10.1 in `f32` metres: `q / 4096 · voxel_m`; outside the box the nearest box
    /// point's value (clamp to edge), so halo cells at a box face see the surface run straight across it (the render
    /// cut clips it there anyway, § 12.2).
    fn sample(&self, p: [f32; 3]) -> f32;
    /// The analytic trilinear gradient of the same interpolant (no finite difference).
    fn gradient(&self, p: [f32; 3]) -> [f32; 3];
}
```

Chunks are meshed with `mesh_chunk` (`volume_mesh.rs:1149`) on the volume's lattice: chunk = 4 × 4 × 4 bricks
(32 cells; 8 m at 250 mm), origin `origin_mm + chunk · 32 · voxel_mm` (dyadic: exact welds, `volume_mesh.rs:79-81`).
LOD `l` (0, 1, 2) uses cell size `voxel_m · 2^l` and `32 / 2^l` cells, so its samples are lattice points: a coarse
LOD reads the stored field at stride `2^l`, with no downsampled copy. Neighbor LODs differ by at most one step
(`NeighborLods`, `volume_mesh.rs:267`), the mesher's own crack-free rule. `MeshParams::project_to_surface` (default
on, `volume_mesh.rs:217-222`) pulls each dual vertex onto the trilinear zero set, which is the sim's surface (§ 10.1):
render vertices lie on the sim's solid to `f32` rounding, and the render-to-sim distance is the chord error between
them, which V-04 measures (`dc_vertices_lie_on_the_sim_surface`, `render_off_sim_le_5cm` on the fixture volumes).

Per-vertex material: `ChunkMesh` has none (`volume_mesh.rs:454-462`). The adapter assigns each output vertex the
material of the solid lattice point of its cell nearest to it (ties: lowest local index), and the chunk is split into
one submesh per material (a triangle takes the material of its first vertex). Blended materials are a follow-up.

### 12.2 A volume is a mesh layer with run-time chunks

Each volume is one `TerrainMeshLayer` (spec 26 § 2a.3, `docs/specs/26-terrain-vfx-highend.md:167-185`). It fills its
footprint hole; its mouth (cut, ring, band, skirt, collar) is cooked once (§ 11.2 item 3) and drawn by the terrain
draw with the ring attach, so the terrain side of the seam is exact as for any layer (`:266-281`). What differs:

- **Chunks are the volume's 4 × 4 × 4-brick chunks, not CDLOD nodes**: the voxel lattice is in integer millimetres
  and the CDLOD node grid is in the lattice pitch (312.576 mm on Switchbacks), so they cannot align. LOD uses the same
  rule as layer chunks: chunk LOD `l` while the chunk's distance is in the terrain's `[range(l−1), range(l))`
  (`:304-307`), at most 3 LODs (strides 1, 2, 4).
- **Run-time meshes.** A chunk's meshes are built at run time from the field, not loaded. The coarsest LOD of every
  non-empty chunk stays resident (spec 26 § 2a.5's rule, `:314-316`).
- **The band meets LOD 0.** The band's inner edge is zippered (cook) to the LOD-0 clip loop of the field's surface
  (the surface clipped in XZ to the lattice cut, § 11.2 item 3; deterministic because the rim is frozen). A coarser
  rim chunk meets it within its LOD error under a `skirt_m` skirt (Ulrich 2002, as the band's own skirt). Bar: rim
  gap ≤ 2 cm on the fixtures at every LOD pair (the 2026-10-17 gate G2), measured by `terrain.seam_report`.
- **Everything else is the layer path**: shadows, `MapLayer` picks, `FrameExport`, the bake export and DDGI see the
  resident chunk entities as meshes (spec 26 § 2a.6).

### 12.3 Edits reach the presentation

The sim owns `GeomState` and the presentation mirrors it with `TerrainMeshLayers::sync_state` (spec 26 § 2a.3,
`:217-223`). With volumes, `sync_state` clones the voxel field (copy-on-write: one pointer per dense brick; the sim's
next write to a shared brick copies that brick) and takes the dirty bricks since the last sync from the generation's
`VoxelDirty` lists (the host passes them, or the presentation diffs brick pointers when it missed a generation). A
dirty brick dirties every chunk whose cells or one-cell halo read it. Dirty chunks are re-meshed on the compute task
pool, nearest first, at most `voxel_remesh_chunks_per_frame` (default 8) a frame, each under a `tracing` span; all
chunks of one generation swap in the same frame, so a dig never shows half done. An engine-native game that owns its
`GeomState` in `TerrainMeshLayers` gets the same path with no host code.

### 12.4 Materials

`VoxelLook::materials: Vec<(u8, String)>` maps voxel material ids to render materials (any `.material.ron`; a
world-space triplanar setup is the author's). An unmapped id draws the layer's default material.

## 13. The `.voxfield` v1 format (registry name `voxel-field`)

### 13.1 Name

Spec 25 § 7.10 and the contract page called it `.fvox`. That name and its magic `FVOX` are the cooked voxel far proxy
(v4; `docs/formats/voxel-far.md`, `docs/specs/12-assets.md:292`). Two formats with one extension would break
`regia upgrade-project`'s dispatch by extension and confuse every tool. The voxel core's file is **`<map>.voxfield`**,
magic `VXFD` (ADR-8116).

### 13.2 Layout (little-endian; hand-written, no serde, like the voxel far proxy's)

| Bytes | Content |
|---|---|
| 4 | magic `VXFD` |
| 4 | `format_version` (u32, 1) |
| 4 | `flags` (u32, 0; any other value is refused) |
| 4 | `brick_edge` (u32, 8: checked) |
| 4 | `dist_one` (u32, 4096: checked) |
| 4 | `dist_clamp` (u32, 16384: checked) |
| 4 | `volume_count` (u32, 0..=64) |
| 32 | `body_hash`: BLAKE3 of every byte after the header (= `field_hash`) |
| 4 | reserved (0) |
| *body:* 32 per volume | `id` u16, `rim` u8, `below` u8, `above` u8, reserved u8 (0), reserved u16 (0), `voxel_mm` u32, `origin_mm` 3 × i32, `bricks` 3 × u16, reserved u16 (0) |
| per volume, in id order: 2 per brick slot | `tag` u8 (0 air, 1 solid, 2 dense), `material` u8 (the solid material; 0 for air and dense), slots in ascending linear index (x fastest, then y, then z) |
| per volume, then per dense slot in slot order: 1536 | 512 × `dist` i16, then 512 × `material` u8, local index `x + 8 · (y + 8 · z)` |

A 256 m volume at 250 mm with 40 522 dense bricks of 524 288 slots (§ 14) is 1 MiB of slots plus 59 MiB of bricks.

### 13.3 Validation (decode)

Every rule is checked, never trusted: the magic; the version (a newer one is `voxfield_newer_version`); the header
constants; the volume table (§ 4 rules, ascending unique ids, no touching boxes); each tag and material (a solid slot
with material 0 is corrupt); each dense voxel (`|dist| ≤ 16384`, material 0 exactly when `dist ≥ 0`); **canonical
form** (a dense brick that is uniform is corrupt, so one field has exactly one encoding and one hash); the exact
length (no trailing bytes); and the body hash. Errors: `voxfield_bad_magic`, `voxfield_newer_version`,
`voxfield_corrupt` (with the byte offset), `voxfield_hash_mismatch`. Decoding never panics and allocates at most the
size the header and the volume table imply, bounded by the input length (a fuzz target, § 13.5).

### 13.4 Versions, migrator, `regia upgrade-project`

- v1 is the first version. The decoder dispatches on `format_version` through a migrator table (`v → v + 1` steps over
  the decoded value), so a v2 adds one step and v1 files keep loading; a newer file is refused, never guessed at
  (spec 25 § 7.1).
- `regia upgrade-project` finds `*.voxfield`, decodes each, and rewrites any file older than the current version
  (atomic: temp file, then rename). At v1 it validates and leaves v1 files untouched, and reports a corrupt file
  without changing it. Its test is modeled on the voxel far one (`upgrade_project_rewrites_v1_voxel_far_proxies`,
  `crates/regia_cli/src/commands/upgrade_project.rs:5049`): `upgrade_project_knows_voxfield_v1`.
- **Frozen, not a cache.** The cook writes it from the source (§ 11.2), but it is also what an editor sculpt saves
  and what a host may store as a dug world, so it is migrated, never silently recooked. The cooked `<map>.meshlayers`
  stays a cache (spec 121 § 9.2) and holds the `.voxfield`'s hash.

### 13.5 Registration (the build lane V-02 does all of it in one commit)

- `docs/formats/voxel-field.md` (this layout, the rules, the example sizes).
- A `format-hashes.ron` row `voxel-field` through a layout description for `check-formats`, as the voxel far proxy
  has (`voxel_far.rs:1347`); the lead runs `cargo xtask check-formats --update`.
- A fuzz target `voxfield_decode` (and `oplog_decode`) in `regia_agent --test fuzz_targets`, with corpus fixtures: an
  empty field, one volume with one dense brick, and the 100-op log.
- The spec 12 § 4a row moves from proposed to live, and the `proposed-formats.ron` row is removed (§ 13.6).
- `cargo test -p regia_cli --test book_cli_coverage` and `--bin regia upgrade_project`, `cargo test -p regia_agent
  --test fuzz_targets`, `cargo xtask check-formats` (the cross-cutting coverage tests).

### 13.6 Proposed until built

This design adds the row `voxel-field` (`<map>.voxfield`, kind `source`: frozen and migrated, never recooked; v1, ADR-8116) to
`docs/formats/proposed-formats.ron` and a matching spec 12 § 4a row marked proposed, so `check-formats` holds the
design to its spec (`xtask/src/proposed_formats.rs`).

## 14. Memory and the 1.5 GB gate

### 14.1 Bytes

| Item | Bytes |
|---|---|
| dense brick | 1536 (512 × `i16` + 512 × `u8`) + 16 (its `Arc` header) + 8 (pool entry) |
| brick slot | 4 (a `u32`: tag in the top bits, pool index or material below) |
| uniform brick | its slot only |
| per volume | 64 (spec) |
| per op in flight | 0 (pointwise, in place) |
| a sim clone (prediction) | slots + pool entries, bricks shared; each later write to a shared brick copies 1536 bytes |
| the presentation | the same clone (§ 12.3): no second field |

### 14.2 Measured brick counts (synthetic volumes)

The design lane counted dense bricks on synthetic volumes after DO27's volume list (2026-10-17: The Cinder Steps 12
to 14 volumes of 128 to 256 m tubes, 64 to 128 m decks, 64 m collapse sites; Shackleton 3 to 6 of 128 to 256 m; the
Switchbacks retrofit 8 to 10 of 256 m). The model: a ground with a 10 % slope and ±1.5 m undulation, 64 m tall boxes,
10 m tubes 14 to 22 m under the ground, two stacked 60 × 6 × 60 m decks; a brick is dense when any of its points is
within the clamp band (the script is the lane report's; V-08 re-measures with real ops).

| Volume | Voxel | Dense bricks 8³ | MiB 8³ | Dense bricks 32³ | MiB 32³ |
|---|---|---|---|---|---|
| ground only, 256 × 256 m | 250 mm | 32 502 | 47.6 | 1 366 | 128.1 |
| two tubes, 256 × 256 m | 250 mm | 40 522 | 59.4 | 1 682 | 157.7 |
| two tubes, 256 × 256 m | 500 mm | 10 232 | 15.0 | 422 | 39.6 |
| one tube, 128 × 128 m | 250 mm | 10 235 | 15.0 | 380 | 35.6 |
| two decks, 128 × 128 m | 250 mm | 13 233 | 19.4 | 492 | 46.1 |
| collapse site, 64 × 64 m | 250 mm | 2 959 | 4.3 | 110 | 10.3 |

Slot tables at 8³ add 2 MiB per 256 m volume at 250 mm (524 288 slots × 4 bytes); the per-brick `Arc` header and
pool entry add 1.6 % to the brick column. 16³ bricks fall in between (96.8 MiB
for the two-tube volume).

### 14.3 Worst-case maps at 250 mm, before any dig

| Map (DO27's list) | 8³ bricks | 32³ bricks |
|---|---|---|
| Switchbacks retrofit, 10 × 256 m tube volumes | 594 + 20 = 614 MiB | 1 577 MiB: **over 1.5 GB** |
| The Cinder Steps, 6 × 256 m tubes + 4 × 128 m decks + 4 × 64 m sites | 356 + 78 + 17 + 14 = 465 MiB | 1 172 MiB |
| Shackleton, 6 × 256 m | 356 + 12 = 368 MiB | 946 MiB |

**Digging grows the field.** A 3 m carve in solid rock makes about 113 m² of new surface: at 250 mm and 8³ bricks
about 75 new dense bricks (112 KiB, counted on the same model); with 32³ bricks 8 new bricks (768 KiB). 1000 disjoint
digs add about 110 MiB with 8³ bricks and up to 750 MiB with 32³. So 8³ bricks pass the gate on every listed map with room for a match's
digging, and 32³ bricks fail it on the Switchbacks retrofit before the first dig. At 500 mm both pass (32³: 396 MiB on
the Switchbacks retrofit).

### 14.4 The gate

- **It is per map, not per preset.** Every lockstep peer holds the same field, so a Deck or LOW client cannot hold a
  smaller one. The 2026-10-17 "lower for Deck and LOW" applies to render meshes only (they stream and LOD, § 12.2).
  The cook enforces the per-map field cap (`voxel_over_budget`, default 1.5 GiB of `resident_bytes` at cook) and the
  sim reports `resident_bytes` live; DO27 caps digging per match (its rules) with the growth figure above.
- **Recommended cap: 1 GiB per map** (DO27 and owner question Q9): the worst listed map is 614 MiB at 250 mm before
  digging, and 1 GiB leaves about 3600 disjoint 3 m digs of headroom (about 1400 at 768 MiB), well inside the Deck's
  16 GB.
- **Go or no-go measurement** (lane V-08, right after V-01): build the three maps' volumes from ops on synthetic
  ground with the real crate, report `resident_bytes`, brick counts, `apply` cost per op size and the growth over a
  10 000-op dig log, on the build server and on the Deck. Go when the worst map is under the cap with 1000 digs of headroom and
  a 3 m carve applies in ≤ 1 ms on the Deck; otherwise no-go with the numbers, and the fallback is 500 mm voxels for
  the large volumes (a per-volume source setting, no code change).

### 14.5 Render memory (presentation; per preset)

At 250 mm the dual contour of a surface has about 16 vertices and 32 triangles per m² at LOD 0. A 256 m two-tube
volume (about 82 000 m² of surface) is about 1.3 M vertices and 2.6 M triangles at LOD 0, about 75 MiB of vertex and
index data if all of it were LOD 0; LOD 1 and 2 divide by 4 and 16. Chunk residency follows the layer stream's
`budget_bytes` (spec 26 § 2a.5); V-07 measures the resident figure on the fixtures per preset.

## 15. Agent surface (rule 8; book page `book/agents/voxel-core.md`)

| Method | Request → reply | Errors |
|---|---|---|
| `terrain.voxel_volumes` | `{}` → per volume `{ id, box_mm, voxel_mm, rim, bricks: { air, solid, dense }, resident_bytes, field_hash, state_term }`, `generation`, `voxel_bits_version` | `F0003` |
| `terrain.voxel_op` | `{ mode: carve\|fill\|paint, material?, shape: { sphere\|capsule\|box } }` (millimetres) → `{ dirty_bricks, changed_voxels, clipped_by_rim, generation }`; dev builds and the editor only | `voxel_op_bad_shape`, `voxel_op_bad_material`, `voxel_op_too_large`, `voxel_sim_owned` |
| `terrain.voxel_column` | `{ volume, x_mm, z_mm }` → crossings `{ y_mm, kind, material }` | `voxel_unknown_volume`, `voxel_out_of_volume` |
| `terrain.surface_at`, `terrain.segment` | spec 121 § 12; results gain `kind: "voxel"` | as spec 121 |
| `terrain.bake_report` | gains the § 11.5 checks, `voxel_ring_step` values and the § 14 figures | as spec 121 |

CLI (V-02): `regia voxel hash <file.voxfield>` prints `field_hash`, brick counts and `resident_bytes`; `regia voxel
replay <base.voxfield> <log.vxol> [--threads N]` prints the replayed `field_hash` and `state_term`, so DO27 can run
the equal-hash check on its own machines (the 2026-10-17 note).

## 16. Acceptance (each fails on the base: the crate and the methods do not exist)

| Lane | Tests |
|---|---|
| V-01 field | `replay_10k_ops_same_hash_at_1_2_3_16_threads` (committed `field_hash` and `state_term` constants); `field_hash_is_the_committed_constant` (the same constants, run on aarch64: the Mac and the Android test device); `state_term_depends_on_state_not_history` (two op orders reaching one field: equal `field_hash` and `state_term`); `carve_then_fill_back_is_the_fresh_field` (canonical form); `ops_never_write_the_rim`; `op_outside_the_extent_changes_nothing` (property over 1000 generated ops); `shape_distances_match_the_integer_table` (committed table of `d` per shape at 64 points incl. bounds); `formulas_do_not_overflow_at_their_bounds`; `crate_has_no_float_tokens` |
| V-02 format | `voxfield_round_trip_is_byte_identical`; `voxfield_hash_is_field_hash`; `non_canonical_brick_is_refused`; `newer_version_is_refused`; `every_truncation_is_an_error_not_a_panic`; `upgrade_project_knows_voxfield_v1`; fuzz targets `voxfield_decode`, `oplog_decode`; `regia voxel hash` and `replay` CLI tests |
| V-03 queries | `column_crossings_match_the_vector_table`, `segment_hits_match_the_vector_table`, `probe_disc_matches_the_vector_table` (committed integer tables, equal on x86-64 and aarch64); `column_of_a_flat_floor_is_exact`; `segment_never_skips_a_solid_cell`; `queries_do_not_allocate` (counting allocator) |
| V-04 mesh adapter | `dc_vertices_lie_on_the_sim_surface` (each LOD-0 vertex within 1e-4 m of the § 10.1 zero set); `render_off_sim_le_5cm`; `voxel_chunks_weld_bit_identically` (every LOD pair); `lod_samples_are_lattice_points` |
| V-05 cook | `cook_twice_gives_identical_bytes`; `voxelized_ground_matches_the_lattice` (`voxel_ring_step` ≤ 10 mm on the fixtures); each § 11.5 check fails on its fixture; `no_volumes_is_stage_b_bit_for_bit` (canonical bytes) |
| V-06 query | the voxel query table on x86-64 and aarch64; `support_crosses_a_volume_ring_with_step_le_10mm`; `dig_through_a_tube_wall_opens_the_column` (support sees the tube floor through the dug hole); `carve_overhang_has_a_roof`; `state_hash_without_volumes_is_unchanged`; `snapshot_restore_keeps_voxels_and_state_hash`; `d1_edit_in_a_volume_is_refused` |
| V-07 layer runtime | `a_dig_remeshes_only_dirty_chunks`; `one_generation_swaps_together`; `rim_gap_le_2cm_at_every_lod_pair` (`terrain.seam_report`); `sync_state_shares_bricks` (no copy without a write); agent e2e for the § 15 methods |
| V-08 memory gate | the § 14.4 measurement report: the three maps, digs, apply cost (the build server and the Deck) |

## 17. Not in this design

Engine-physics colliders built from voxel chunks for hosts that do not lockstep (a later lane: trimesh pieces from the
LOD-0 chunk meshes, as spec 26 § 2a.8 does for layer chunks); voxel-only worlds with no heightfield in the world
geometry (the crate supports them; the cook does not); smooth CSG and blended materials; oriented boxes, cylinders and
cones (Q1); GPU editing; volumes that touch; layers crossing a volume; lightmaps on voxel chunks; nav over voxel
volumes (W-20's layered nav reads `walkable_spans`, which sees voxel floors through the column; the dirty-tile hookup
is a follow-up lane once W-20 exists).

## 18. DO27 sign-off

**Sign-off record** (the build lanes read this line before they start): not signed. DO27 signs a spec revision with
its answers to the questions below (the lane report carries them with recommended answers); every later API change
adds a dated line here and bumps the crate's minor version (0.x until DO27 adopts it, then semver).

Questions for DO27:

1. Q1 op set: carve, fill and paint with sphere, capsule and axis-aligned box. Anything else D2 needs at launch?
2. Q2 brick and voxel size: 8³ bricks; 250 mm by default, 500 mm for volumes over 256 m.
3. Q3 the partition: inside a volume the field is the only rock; layers are wholly inside or outside; ops stop at a
   frozen rim (4+ voxels) at the box faces.
4. Q4 the sim surface: the trilinear zero set (§ 10); the push by 16 × 3 radial probes.
5. Q5 the ring step: ≤ 10 mm between the ground lattice and the field at a volume ring.
6. Q6 collapses inside a volume are `Fill` ops (no cooked blocker plugs there).
7. Q7 the sign-off process (this section).
8. Q8 persistence of dug fields (`.voxfield` as the save; op logs are not state).
9. Q9 the per-map field cap (1 GiB recommended) and render budgets per preset.

## 19. Lanes

| Lane | Builds | Sections | After |
|---|---|---|---|
| V-01 | the crate: model, ops, apply and threads, op log, replay, hashes | § 3 to § 9 | DO27's sign-off |
| V-02 | `.voxfield` v1, migrator table, `upgrade-project`, fuzz targets, CLI, registration | § 13, § 15 CLI | V-01 |
| V-03 | integer column, segment and disc probe, vector tables | § 10 | V-01 |
| V-04 | `VoxelSdf`, chunk meshing by stride, materials, render-to-sim bound | § 12.1 | V-01 |
| V-05 | world geometry cook: source field, voxelization, footprint hole, rim, checks, canonical section | § 11.1, § 11.2, § 11.5 | V-02, spec 121 W-06 and W-10 |
| V-06 | world geometry queries, state, snapshot, handshake term | § 11.3, § 11.4 | V-03, V-05, spec 121 W-11 and W-12 |
| V-07 | the volume as a mesh layer: run-time chunks, sync, band at LOD 0, agent methods, book | § 12.2 to § 12.4, § 15 | V-04, V-06, spec 26 lanes 15-02 and 15-04 |
| V-08 | the memory and cost gate | § 14.4 | V-01 |

The briefs are staged as `queue/staged/do27-adopt/followups/16-0N-<lane>.job` (V-01 = 16-01
`do27-voxel-core-field` … V-08 = 16-08 `do27-voxel-memory-gate`). Critical path after DO27's sign-off: V-01, then V-02
and V-03 (V-04 and V-08 in parallel), then V-05 once spec 121's W-06 and W-10 have merged, V-06 once W-11 and W-12 have
merged, and V-07 once the mesh-layer lanes 15-02 and 15-04 have merged: about 29 serial lane hours.
