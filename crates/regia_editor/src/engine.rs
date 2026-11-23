// SPDX-License-Identifier: MPL-2.0
//! Which copy of the engine this build of the editor links.
//!
//! The default build links the engine rlibs. In-process tests use that: one copy, the process
//! allocator.
//!
//! `--features hot-dylib` links the engine from `regia_dylib` instead. A game library built
//! against that same artifact then shares `TypeId`s with the editor (specs/20 § 1). Ship that build
//! with `-C prefer-dynamic` so Rust `std` is shared too.

/// Application crate (`App`, plugins, schedules).
#[cfg(feature = "hot-dylib")]
pub use regia_dylib::app;
/// Logging and the other foundation modules.
#[cfg(feature = "hot-dylib")]
pub use regia_dylib::core;
/// ECS, reflection and reflected serialization.
#[cfg(feature = "hot-dylib")]
pub use regia_dylib::ecs;

/// Foundation modules, from the engine rlib.
#[cfg(not(feature = "hot-dylib"))]
pub use regia::core;
/// Application crate, from the engine rlib.
#[cfg(not(feature = "hot-dylib"))]
pub use regia_app as app;
/// ECS crate, from the engine rlib.
#[cfg(not(feature = "hot-dylib"))]
pub use regia_ecs as ecs;
