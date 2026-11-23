//! Spike stand-in for the engine: today's `regia` umbrella, the full skeleton dependency set
//! ([`heavy`]) and a prototype of the hot-reload runtime ([`hot`]) that specs/20 puts in the engine
//! (ownership-tagged registration, snapshot/teardown/restore, `export_game!`).
//!
//! In the engine this would be split: `GamePlugin`/`export_game!` in `regia_app` (or `regia`),
//! the snapshot/restore code in `regia_editor` (desktop only).

pub mod heavy;
pub mod hot;
pub mod loader;

pub use bevy_ecs;
pub use bevy_reflect;
pub use regia;
pub use regia_app;

/// Everything game code imports.
pub mod prelude {
    pub use crate::hot::{GamePlugin, GameRegistrar, PersistentStore};
    pub use bevy_reflect::prelude::*;
    pub use regia::prelude::*;
}
