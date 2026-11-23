// SPDX-License-Identifier: MPL-2.0
//! The per-frame contact read and the shared foot lock allocate nothing once they are warm
//! (ADR-8308). Cook-time labeling builds the curves; playback only samples them.

use regia_animation::foot_ik::contact::contact_weight;
use regia_animation::foot_ik::lock::{PlantLock, PlantRelease};
use regia_animation::params::ParameterSet;
use regia_core::math::Vec3;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// One hundred and twenty steady frames of a planted foot, then a release: `PlantLock::update`
/// and `contact_weight` ask the allocator for nothing.
#[test]
fn plant_lock_and_contact_weight_allocate_nothing_in_steady_state() {
    let mut lock = PlantLock::default();
    let mut params = ParameterSet::default();
    params.set_float("contact.foot_l", 1.0);
    for i in 0..8 {
        let foot = Vec3::new(i as f32 * 0.01, 0.1, 0.0);
        let repin = lock.drifted(foot.distance(lock.pin()), 0.3);
        lock.update(true, repin, foot, 1.0 / 60.0, 0.2, PlantRelease::Follow);
        let _ = contact_weight(&params, "foot_l");
    }
    let (bits, stats) = alloc_counter::measure(|| {
        let mut bits = 0u32;
        for i in 0..120 {
            let foot = Vec3::new(0.2 + i as f32 * 0.002, 0.1, 0.05);
            let down = i < 60;
            let repin = lock.drifted(foot.distance(lock.pin()), 0.3);
            lock.update(down, repin, foot, 1.0 / 60.0, 0.2, PlantRelease::Follow);
            bits ^= lock.weight().to_bits();
            bits ^= lock.pin().x.to_bits();
            if let Some(weight) = contact_weight(&params, "foot_l") {
                bits ^= weight.to_bits();
            }
        }
        bits
    });
    assert_ne!(bits, 0, "the measured loop did not run");
    assert_eq!(
        stats.allocs, 0,
        "steady state allocated {} times ({} bytes)",
        stats.allocs, stats.bytes
    );
}
