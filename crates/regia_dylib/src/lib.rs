// SPDX-License-Identifier: MPL-2.0
//! One shared copy of the engine for hot reload (`docs/specs/20-hot-reload.md` § 1, ADR-015).
//!
//! The editor and every reloaded game dylib link this crate, so statics, `TypeId`s and the
//! allocator are not duplicated. Re-exporting [`regia`] is the `bevy_dylib` pattern (the facade
//! split that moves the umbrella to `regia_internal` is still ahead; this crate depends on
//! `regia`, and `regia` does not depend on it). Until that split lands, a host that also links
//! the engine rlibs maps a second copy of the engine statics; the editor's `hot-dylib` build only
//! swaps part of them. A game opts in with its own `hot-reload` feature (see the project
//! templates), which the editor passes to `cargo rustc`.
//!
//! Every artifact that links this dylib (the host and each game or sim dylib) must be built with
//! `-C prefer-dynamic` so all of them load the toolchain's `libstd` and share one allocator. On
//! Linux the editor refuses a game load when it has a static `std` (`GameLibraryError::StaticStd`);
//! elsewhere this is not detected, and a static `std` next to a `prefer-dynamic` game dylib can
//! corrupt the heap. A `prefer-dynamic` process also needs the toolchain's
//! `lib/rustlib/<target>/lib` directory on `LD_LIBRARY_PATH` (Linux) or `PATH` (Windows), or the
//! system loader fails before any engine code runs. Enable the same `regia` features as the
//! launcher through this crate's features of the same names; the `[features]` table in
//! `Cargo.toml` is the full list (including `client` and `server`, and a `default` that mirrors
//! `regia`'s), so the shared copy matches the launcher.

pub use regia::*;
