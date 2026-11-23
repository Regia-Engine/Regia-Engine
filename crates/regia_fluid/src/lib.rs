// SPDX-License-Identifier: MPL-2.0
//! Particle liquid for Regia Engine (ADR-2441, `book/agents/fluid.md`).
//!
//! - [`sim`]: the deterministic CPU reference solver, PIC/FLIP on a MAC grid with a MIC(0)-PCG
//!   pressure projection, solid boundaries from a signed-distance field, emitters, sinks and a
//!   closed domain box. The same inputs give bit-identical particles ([`FluidSim::state_digest`]).
//! - [`whitewater`]: foam, spray and bubble particles from the liquid's trapped-air, wave-crest and
//!   kinetic potentials, capped per type.
//! - [`surface`]: a particle level set and its marching-cubes surface mesh.
//! - [`frame`]: [`FluidFrame`], points with a type tag and an age.
//! - [`cache`]: the `.fluidcache` v1 bake ([`FluidCache::record`], [`FluidCache::play`]).
//! - [`presets`]: `pour`, `dam_break`, `splash`, `stream`.
//! - [`plugin`]: [`FluidWorld`] and [`FluidPlugin`]; with the `draw` feature the surface and the
//!   whitewater are published as `MeshRenderer` entities.
//! - `gpu` (feature `gpu`): the wgpu compute path, checked against the CPU reference within a
//!   documented tolerance. The CPU reference is the contract.
//! - [`solid`]: static colliders and moving solids ([`MovingSolid`], kinematic and dynamic bodies
//!   rasterized every substep, ADR-2577); [`FluidSim::solid_impulses`] gives the pressure impulse
//!   on each.
//! - `coupling` (feature `physics`): `FluidPhysicsPlugin` couples `regia_physics` bodies both ways
//!   (moving solids in, pressure impulses back as `ExternalImpulse`).
//!
//! The crate is optional (the `fluid` feature of `regia`) and is never a dependency of `regia_core`.

pub mod cache;
#[cfg(feature = "physics")]
pub mod coupling;
#[cfg(feature = "draw")]
pub mod draw;
pub mod frame;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod grid;
pub(crate) mod math;
pub mod pcg;
pub mod plugin;
pub mod presets;
pub mod sim;
pub mod sleep;
pub mod solid;
pub mod surface;
pub(crate) mod tally;
pub mod whitewater;

pub use cache::{CacheError, FLUIDCACHE_FORMAT_VERSION, FluidCache};
pub use frame::{FluidFrame, PointKind};
pub use plugin::{FluidEntry, FluidPlugin, FluidWorld};
pub use sim::{Emitter, EmitterKind, FluidError, FluidParams, FluidSim, FluidStats, Shape};
pub use solid::{Collider, MovingSolid, SolidImpulse, SolidMotion, SolidShape};
pub use surface::SurfaceMesh;
pub use whitewater::{WhitewaterKind, WhitewaterParams};
