// SPDX-License-Identifier: MPL-2.0
//! Game-dylib hot reload (specs/20, ADR-015).
//!
//! The editor builds the game crate with `-C metadata=hotgen<N>`, copies it to
//! `.regia/hot/game-<N>-<pid>-<serial>.<ext>`, and moves reflected state across the boundary. The old mapping
//! is never unloaded.

mod build_game;
mod host;
mod load;
mod snapshot;
mod watch;

pub(crate) use build_game::engine_fingerprint;
pub use host::{
    CompileState, HotReloadConfig, HotReloadHost, HotReloaded, ReloadPhase, ReloadReport, toast_line,
};
pub use load::{
    GameExports, GameLibraryError, LoadedGameLibrary, dylib_extension, host_abi_hash, load_game_exports,
    load_game_library, process_links_dynamic_std,
};
pub(crate) use load::{open_library, read_abi_hash};
pub use snapshot::{reflected_count, reflected_number, with_game_owner};
