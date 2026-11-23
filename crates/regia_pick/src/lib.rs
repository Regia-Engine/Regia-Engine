// SPDX-License-Identifier: MPL-2.0
//! Render-side ray picking over terrain and mesh layers (DO27 ask 12, Tincal F7).
//!
//! This is the runtime half of what the editor's picking did alone: ray–triangle and ray–mesh
//! casts with CPU skinning ([`mesh`]), a per-mesh triangle BVH ([`bvh`]), the heightfield march
//! that passes through holes ([`terrain`]), and [`world_pick`], which returns the nearest surface
//! with the [`MapLayer`] it belongs to, optionally restricted to a set of layers. The editor's
//! `picking` module re-exports the casts and keeps only the GPU id buffer and the editor's
//! owner mapping.
//!
//! ```rust,ignore
//! use regia_core::math::{Ray, Vec3};
//! use regia_pick::{PickFilter, world_pick};
//!
//! let ray = Ray::new(Vec3::new(0.0, 50.0, 0.0), Vec3::NEG_Y);
//! if let Some(hit) = world_pick(world, ray, &PickFilter::on_layers(&[1, 2])) {
//!     // hit.layer, hit.entity, hit.point, hit.normal, hit.distance
//! }
//! ```
//!
//! # Getting started
//!
//! The casts also work on bare geometry, with no world. This hits the floor triangle below a ray,
//! and the same ray over an empty world finds nothing:
//!
//! ```
//! use regia_core::math::{Ray, Vec3};
//! use regia_ecs::prelude::World;
//! use regia_pick::{FaceCull, PickFilter, ray_triangle, world_pick};
//!
//! let (a, b, c) = (Vec3::new(-5.0, 0.0, -5.0), Vec3::new(5.0, 0.0, -5.0), Vec3::new(0.0, 0.0, 5.0));
//! let hit = ray_triangle(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, a, b, c, FaceCull::None)
//!     .expect("the ray passes through the triangle");
//! assert!((hit.t - 10.0).abs() < 1e-4);
//!
//! // Nothing in the world: a pick is a miss, not an error.
//! let world = World::new();
//! let ray = Ray::new(Vec3::new(0.0, 50.0, 0.0), Vec3::NEG_Y);
//! assert!(world_pick(&world, ray, &PickFilter::default()).is_none());
//! ```

pub mod bvh;
pub mod mesh;
#[doc(hidden)]
pub mod stats;
pub mod terrain;
mod world;

pub use bvh::{BVH_MIN_TRIANGLES, MeshBvh, bvh_for};
pub use mesh::{FaceCull, MeshHit, RayTriangle, ray_triangle, raycast_mesh, raycast_mesh_bvh};
pub use regia_scene::prelude::MapLayer;
pub use stats::{PickCounts, counts as pick_counts, pick_cache_scope, reset_counts as reset_pick_counts};
pub use terrain::{TerrainHit, raycast_terrain, raycast_terrain_entity};
pub use world::{
    LayerSet, PickFilter, PickSurface, WorldPick, map_layer_of, ray_meets_box, raycast_mesh_entity,
    raycast_mesh_entity_within, raycast_meshes, track_pick_order, world_pick, world_pick_layers,
};
