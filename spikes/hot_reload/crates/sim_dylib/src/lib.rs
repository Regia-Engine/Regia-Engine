//! `sim` as a dylib: game library copies link it dynamically, so the loader keeps the first copy and
//! the sim thread (and its code) survive game reloads.
//!
//! It links the engine dylib so that crates both use (serde, postcard) exist once in the process.

#[allow(unused_imports)]
pub use sim::*;

#[allow(unused_imports)]
use spike_engine_dylib as _;
