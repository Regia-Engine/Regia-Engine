// SPDX-License-Identifier: MPL-2.0
//! World geometry for lockstep simulations (spec 121, ADR-8019; this crate: ADR-8050).
//!
//! GPU-free and ECS-free, so a game's sim and a headless server link it alone. It answers, from one set
//! of triangles, where the floor is under a body, where the ceiling is over it, how far a disc is pushed
//! out of a wall, and whether a segment hits rock.
//!
//! Today the crate holds **Stage A** (spec 121 § 4): [`compat`], Direct Order 27's tunnel code copied
//! with its arithmetic unchanged (ADR-8012 A-1), so DO27 can swap its own modules for this crate with no
//! change to its lockstep checksums. The native Stage B core is built at the crate root, lane by lane
//! (spec 121 § 17). Today it has the formats and the load half (lane W-06, ADR-8112):
//!
//! - [`units`]: the two pinned conversions `mm_to_m` and `m_to_mm` (spec 121 § 3).
//! - [`settings`]: `GeomSettings`, DO27's caps as settings.
//! - [`meshlayers`]: the source `<map>.meshlayers.ron` v1 (`terrain-mesh-layers`).
//! - [`passages`]: `<map>.passages.ron` and the `do27-tunnels/1` import and export.
//! - [`cooked`]: the cooked `<map>.meshlayers` v1 (`terrain-mesh-layers-cooked`), its canonical
//!   section and `content_hash`.
//! - [`cook`]: the canonical half of the cook (the `Passages` source, the weld and trim steps).
//! - [`geom`]: the load half, `WorldGeom::from_cooked`.
//! - [`ground`]: the sim ground lattice: integer-millimetre vertices, heights, point location and
//!   nested tiles (spec 121 § 3; lane W-10, ADR-8214).
//! - [`cut`]: mouths as named holes cut on lattice edges, the rings, the ring pins and the collar.
//! - [`pitch`]: the `Nested` ground mode: levels by error, the seam rule, hanging vertices, the
//!   memory budget and the `ground_pitch_seam` check.
//! - [`render`]: the render half of the cook (lane W-07, ADR-8175): the `Passages` render mesh, subdivided from
//!   the canonical faces, and the mouth cut quads, in the presentation section.
//! - [`sections`]: the section half of the cook (lane W-08, ADR-8210): each `Passages` layer cut into sections, with
//!   their portals, zones and blocker plugs, and skylight shafts cooked as open cuts.
//! - [`tunnel_bake`]: the one-description bake of DO27's `do27-tunnels-v2/1` descriptions (render, collision,
//!   mouth cut, centreline nav and the consistency checks), rewritten from DO27's offline baker.
//! - [`checks`]: the mesh-layers cook's consistency checks (spec 121 § 10, ADR-8359). Each check is named
//!   in `terrain.bake_report`. `regia terrain check-layers` runs them on a cooked file's source.
//! - [`viscells`]: `<map>.viscells.ron` emitted from the cooked sections (lane W-21, ADR-8371).
//! - `portal_sound` (feature `stage_b`): sound paths through the cooked portals, rock occlusion through the segment
//!   test, and the reverb preset each section arms (lane W-25, ADR-8416).
//! - `zipper`: the cook-time strip from a mouth ring to the authored boundary (W-14, ADR-8291).
//!   A hole whose centroid falls through is capped; a side lip is not written (ADR-8334). The
//!   filtered collar plus a 1 mm wedge still seals each ring sample the collar does not cover.
//! - [`zones`]: one exposure zone, one reverb zone and one interior volume per cooked section, and the
//!   frame-clock exposure ease (lane W-24, ADR-8384).
//!
//! - [`compat::layout`]: spline passage descriptions, their sampling and the mesher.
//! - [`compat::mesh`]: the triangle collision mesh with its XZ index, `segment_triangle`, the ceiling
//!   clamp, the disc wall push and the nav overlay.
//! - [`compat::surface`]: the T3 column query: the free spans at one XZ and the span a point is in.
//! - [`compat::ground`]: the floor query over the heightfield and the mesh (the column's `span_at`).
//! - [`compat::state`]: the tunnel arm a lockstep sim keeps (every body's floor, walls, lid and rock).
//! - [`compat::hover_ground`]: the hover-hull wall and ledge rules and the ground tilt.
//! - [`compat::sense`]: who hears, sees and takes what underground.
//! - [`compat::onfoot`]: the on-foot (walker) grid, its glTF loader, the walker slide and floor.
//! - [`compat::sampler`] and [`compat::terrain_patch`]: the heightfield the query falls back to.
//! - [`compat::pack`]: the baked tunnel file, its loader and the mouth checks.
//!
//! Determinism: runtime queries use `f32` `+ - * /` in a fixed order, `regia_detsim::math` (libm 0.2.16) for
//! `sqrt` and the hole test's trig, and `regia_detsim::float::det_*` for every float `min`, `max` and `clamp`
//! (ADR-8166); no SIMD types and no hash-map iteration. [`QUERY_BITS_VERSION`] names the query bits. The crate is on the `cargo xtask det-scan` lockstep closure
//! (`docs/ci/lockstep-closure.toml`). The public API is semver-versioned and DO27 signs off on changes
//! (spec 121 § 16).
//!
//! ```
//! use regia_world_geom::compat::layout::TunnelLayout;
//! use regia_world_geom::compat::mesh::{TunnelNavOverlay, separate_disc_from_walls};
//!
//! // A host registers its own layouts; an empty one meshes to nothing.
//! let layout = TunnelLayout {
//!     map_id: "demo".into(),
//!     version: regia_world_geom::compat::layout::LAYOUT_VERSION,
//!     sample_step_m: regia_world_geom::compat::layout::SAMPLE_STEP_M,
//!     passages: Vec::new(),
//!     openings: Vec::new(),
//!     junctions: Vec::new(),
//!     crawlways: Vec::new(),
//! };
//! let mesh = layout.mesh(None);
//! assert!(mesh.is_empty());
//! assert!(TunnelNavOverlay::bake(&mesh).portal_indices.is_empty());
//! assert_eq!(separate_disc_from_walls(&mesh, 1.0, 2.0, 2.5), [1.0, 2.0]);
//! ```

mod approach;
pub mod checks;
pub mod compat;
pub mod cook;
pub mod cooked;
pub mod cut;
pub mod geom;
pub mod ground;
pub mod interior;
pub mod meshlayers;
pub mod passages;
pub mod pitch;
pub mod render;
pub mod sections;
pub mod settings;
pub mod tunnel_bake;
pub mod units;
pub mod viscells;
mod zipper;
pub mod zones;

// The deterministic AI's `LineOfSight` for the terrain sampler and the tunnel mesh (plan row S-10, ADR-8395).
mod los;

pub use cooked::{CookedMeshLayers, GeomLoadError, WorldGeomCooked};
pub use geom::{TriRef, WorldGeom};
pub use meshlayers::{GroundMode, MeshLayersDocument};
pub use settings::GeomSettings;
pub use units::{m_to_mm, mm_to_m};

/// The version of the query code's bits (spec 121 § 5.2 and § 14 rule 2, ADR-8084 row 10, ADR-8166). Bumped by
/// every change that moves any answer's bits for the same input: the mesher, the hole list, the floor, ceiling,
/// wall and segment queries and `hole_at`. A lockstep handshake compares it (with the map's content and
/// `regia_detsim`'s math version) so a peer on an older Regia patch with the same map is refused instead of
/// desyncing.
///
/// - `1`: Stage A after review rev-b L1/L2 (ADR-8166): the hole test's trig through `regia_detsim::math`, every
///   float `min`/`max`/`clamp` through `det_*`. The regia-absorb-1 copy before it (platform trig) had no
///   constant and counts as `0`.
/// - `2`: the Stage A T3 re-sync (ADR-8167): `compat::{mesh, ground, sampler}` move from regia-absorb-1 to
///   regia-absorb-2 (DO27's 1.58 query): `query_ground` is the column's `span_at`, support floors are trimmed and
///   welded. The mesh and hole bits did not move.
///
/// Pinned by `tests/mesh_bits.rs`: the committed hashes record the version they were made with, and a hash change
/// without a bump fails review there.
// Version 3 introduced the provisional native kernel; 4 fixes its raw-lid threshold
// and pins the C2 segment arithmetic (ADR-8246). Stage A arithmetic is unchanged.
// Version 5 includes pilot-only decks in geometric support, as T3 does; navigation filters them.
// Version 6 (ADR-8246 R4): near-equal floors and equal-height lids tie by the canonical
// integer-millimetre triangle key, so a source permutation cannot move an answer. Stage A
// arithmetic and hashes stay at the version-5 bytes.
// Version 7 (lane W-13, spec 121 § 13.3 "Blocker generation"): an active blocker plug's triangles are
// candidates of support, floors_at, surface_at, ceiling_above, segment, rock_between and push_disc
// (before, plug triangles were never candidates). A state with every plug clear answers bit for bit as
// version 6; the query table and every Stage A hash are unchanged.
// Version 8 (ADR-8323, lane W-30; numbered 8 at integ-v06r-det, ADR-8531, because W-13 took 7): `push_capsule`
// and its vertical rule join the query table; no earlier answer moved. Stage A arithmetic and hashes stay at the
// version-5 bytes.
pub const QUERY_BITS_VERSION: u32 = 8;

#[cfg(feature = "stage_b")]
pub mod query;
#[cfg(feature = "stage_b")]
mod query_index;
#[cfg(all(feature = "stage_b", any(test, feature = "stats")))]
pub use query::QueryStats;
#[cfg(feature = "stage_b")]
pub use query::{
    Band, CeilingClamp, Push, SectionId, Span, SpanBuf, Support, SurfaceAt, SurfaceHit, SurfaceKind,
};

#[cfg(feature = "stage_b")]
pub mod query_segment;
#[cfg(feature = "stage_b")]
mod query_walls;
#[cfg(feature = "stage_b")]
pub use query_segment::{FaceMask, SegmentHit};

#[cfg(feature = "stage_b")]
mod query_pyramid;

// The segment and disc-push tables built at load (plan row W-11, ADR-8815).
#[cfg(feature = "stage_b")]
mod query_seg_index;

// Test support only: reuse the audited allocator without pulling in rendering or ECS.
// ADR-8246 permits this inclusion, not a new unsafe implementation. The file is a byte-identical
// copy of `regia_test_utils/src/alloc_counter.rs` kept inside this crate (a `#[path]` outside it
// fails check-packaging, ADR-1485) and outside `src/` (it is not lockstep code, so det-scan does not
// read it); `tests/allocation_counter_copy.rs` keeps the two identical (ADR-8421).
#[cfg(all(feature = "allocation-tests", not(doc)))]
#[doc(hidden)]
#[path = "../test_support/allocation_counter.rs"]
#[expect(
    clippy::disallowed_methods,
    reason = "test support: the audited allocator's REGIA_ALLOC_TRACE diagnostic reads the environment, never sim code (ADR-8367)"
)]
pub mod allocation_counter;

#[cfg(feature = "stage_b")]
mod query_ground;
// The fine column directory, its box table and the per-hit evaluation (lane W-11 D4, ADR-8817).
#[cfg(feature = "stage_b")]
mod query_column;

// The capsule wall push and its vertical rule, and the character mover (lane W-30, ADR-8323).
#[cfg(feature = "stage_b")]
pub mod mover;
#[cfg(feature = "stage_b")]
mod query_capsule;
#[cfg(feature = "stage_b")]
pub use query_capsule::HALF_RADIUS_RISE;

#[cfg(feature = "stage_b")]
pub mod edit;
#[cfg(feature = "stage_b")]
pub use edit::{
    DirtyRect, EditStats, Generation, GeomEditError, GeomSnapshotError, GeomState, GroundEdit, GroundEdits,
    HeightRectMm,
};

#[cfg(feature = "stage_b")]
pub mod blockers;
#[cfg(feature = "stage_b")]
pub use blockers::{BlockerId, BlockerStatus, CellStep, PortalId};

/// Covered overview bake and per-unit tunnel view flags (ADR-8333).
pub mod overview;

pub use checks::{CheckFinding, TerrainBakeReport};

// Sound over the cooked portal graph: paths through portals, rock occlusion, section reverb (W-25, ADR-8416).
#[cfg(feature = "stage_b")]
pub mod portal_sound;
