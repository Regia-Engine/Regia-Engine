//! Prototype of `regia_dylib` (specs/20 § 1): re-exports the engine from a `dylib`, so the editor
//! host and every game library copy share one engine (statics, task pools, type ids, allocator).
//! Pattern from `bevy_dylib` (MIT/Apache-2.0), see docs/provenance.md.

#[allow(unused_imports)]
pub use spike_engine::*;
