//! Prototype of the `regia` facade crate. Engine change implied (ADR-015 draft): today `regia`
//! is the umbrella and `regia_dylib` depends on it, so `regia` cannot depend on `regia_dylib`.
//! The umbrella moves to `regia_internal`; `regia` becomes this facade (bevy / bevy_internal /
//! bevy_dylib pattern).

pub use spike_engine::*;

#[cfg(feature = "dynamic")]
#[allow(unused_imports)]
use spike_engine_dylib as _;
