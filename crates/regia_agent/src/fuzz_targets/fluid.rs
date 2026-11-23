// SPDX-License-Identifier: MPL-2.0
//! The `.fluidcache` target (ADR-2441): a small baked run with whitewater; the loader reads and fully
//! validates it, replays every frame and re-encodes it (`regia_fluid::cache`).

use regia_core::math::Vec3;
use regia_fluid::cache::{FLUIDCACHE_FORMAT_VERSION, FluidCache};
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};

use super::{FuzzInput, FuzzTarget};

/// Four frames of a falling block in a 0.3 m tank (a few hundred particles), whitewater on.
fn fixture() -> Vec<u8> {
    let params = FluidParams {
        bounds_max: Vec3::new(0.3, 0.3, 0.1),
        cell_size: 0.05,
        ..FluidParams::default()
    };
    let Ok(mut sim) = FluidSim::new(params) else {
        return Vec::new();
    };
    let block = Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.1, 0.2, 0.05),
            half_extents: Vec3::new(0.1, 0.1, 0.05),
        },
        velocity: Vec3::new(1.0, 0.0, 0.0),
        kind: EmitterKind::Fill,
        done: false,
    };
    if sim.add_emitter(block).is_err() {
        return Vec::new();
    }
    let mut cache = FluidCache::for_sim(&sim, true);
    for _ in 0..4 {
        sim.step();
        if cache.record(&sim).is_err() {
            return Vec::new();
        }
    }
    cache.to_bytes()
}

fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let cache = FluidCache::from_bytes(bytes).map_err(|e| format!("{} ({})", e, e.code()))?;
    for frame in 0..cache.frame_count() {
        cache
            .play(frame)
            .map_err(|e| format!("{}{e}", super::BUG_PREFIX))?;
    }
    Ok(cache.to_bytes())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "fluidcache",
        format: Some("fluidcache"),
        format_version: FLUIDCACHE_FORMAT_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}
