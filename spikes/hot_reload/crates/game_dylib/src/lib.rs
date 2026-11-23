//! specs/20 § 1 `game_dylib`: re-exports `game` (its `regia_game_entry` / `REGIA_ABI_HASH` come
//! along). Game types keep `game`'s crate metadata, so their `TypeId`s are identical in every build
//! — the hazard `host --mode wrapper` demonstrates.

#[allow(unused_imports)]
pub use game::*;
