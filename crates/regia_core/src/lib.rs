// SPDX-License-Identifier: MPL-2.0
//! Foundation: math (glam re-export), ids, errors, logging/profiling, task pools.
//!
//! Specification: `docs/specs/10-core.md` § 1.
//!
//! - [`math`]: glam re-exports, bounding volumes, frustum culling, colour, curves, easing, PCG32 random.
//! - [`id`]: [`id::Guid`], [`id::Name`], [`id::TypePathHash`].
//! - [`log`]: logging setup; [`profiling`]: `scope!` and the in-process [`profiling::FrameProfiler`].
//! - [`tasks`]: the compute / async-compute / IO thread pools.
//! - [`residency`]: [`residency::ResidencyKeeper`] keeps the process's code paged in.
//! - [`memory`]: CPU heap accounting by subsystem ([`memory::TaggingAllocator`], [`memory::enter`]).
//! - [`mesh_query`]: triangle-mesh and point-set spatial queries (BVH, winding numbers, distance grids).
//! - [`project_path`]: confining caller-supplied relative paths to a project root.
//! - [`flash`]: photosensitivity check (WCAG 2.2 success criterion 2.3.1) over sRGB frames.
//! - [`ron_fmt`]: [`ron_pretty()`], the one pretty RON config (`\n` line ending on every host).
//! - [`load_switch`]: [`load_switch::LoadSwitch`], one map loaded at a time with cancel (a level switch).
//!
//! # Getting started
//!
//! The math types are glam's, re-exported so every crate agrees on one version, and ids round-trip
//! through their text form:
//!
//! ```
//! use regia_core::id::Guid;
//! use regia_core::math::Vec3;
//!
//! let v = Vec3::new(3.0, 4.0, 0.0);
//! assert_eq!(v.length(), 5.0);
//!
//! let id = Guid::from_u128(0x1234_5678);
//! assert_eq!(id.to_string().parse::<Guid>().unwrap(), id);
//! ```

pub use glam;
pub use ron_fmt::ron_pretty;

pub mod brand;
pub mod editor_formats;
pub mod engine_version;
pub mod flash;
pub mod fuzz;
pub mod id;
pub mod load_switch;
pub mod log;
pub mod math;
pub mod memory;
pub mod mesh_query;
pub mod privacy;
pub mod profiling;
pub mod project_path;
pub mod residency;
pub mod ron_fmt;
pub mod schema;
pub mod tasks;
pub mod telemetry;
pub mod version;

/// Speed of sound in dry air at 20 °C, m/s: the blast-front speed the crash effects and the
/// ground-shock airblast share (ADR-1497's one front speed, one constant). Defined here so both
/// the client build (`regia_vfx::crash::SHOCK_FRONT_SPEED` re-exports it) and the server build
/// (`regia_framework::ground_shock::AIR_SPEED_M_S` reads it) see the same value.
pub const SPEED_OF_SOUND_M_S: f32 = 343.0;
