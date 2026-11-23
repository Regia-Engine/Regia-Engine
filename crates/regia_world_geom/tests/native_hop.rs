// SPDX-License-Identifier: MPL-2.0
//! Native counterpart of Stage A's kinematic 17-sample / 10-halving hop meter.
//! Source requirements: ir-core/tests/tunnel_t3.rs 1-447, regia-absorb-3, ADR-8012.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]
pub mod native_mouths;
use regia_detsim::{float::det_max, math};

fn discontinuity(f: impl Fn(f32) -> f32) -> f32 {
    let mut worst = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut previous = f(0.0);
    for i in 1..=16 {
        let (a, b) = ((i - 1) as f32 / 16.0, i as f32 / 16.0);
        let current = f(b);
        if (current - previous).abs() > worst.0 {
            worst = ((current - previous).abs(), a, b, previous, current);
        }
        previous = current;
    }
    if worst.0 < 0.005 {
        return 0.0;
    }
    let (_, mut a, mut b, mut fa, mut fb) = worst;
    for _ in 0..10 {
        let middle = (a + b) * 0.5;
        let fm = f(middle);
        if (fm - fa).abs() >= (fb - fm).abs() {
            (b, fb) = (middle, fm);
        } else {
            (a, fa) = (middle, fm);
        }
    }
    (fb - fa).abs()
}
#[test]
fn native_mouth_hop_at_full_speed_le_5cm_per_tick() {
    let mut failures = Vec::new();
    for map in ["switchbacks", "cinder_steps", "atacama_rail"] {
        let (_, geom, layout, sampled) = native_mouths::world(map);
        let state = geom.new_state();
        for opening in &layout.openings {
            let ramp = &sampled
                .passages
                .iter()
                .find(|p| p.id == opening.ramp)
                .unwrap()
                .samples;
            let last = ramp[ramp.len() - 1].p;
            let previous = ramp[ramp.len() - 2].p;
            let (dx, dz) = (last[0] - previous[0], last[2] - previous[2]);
            let length = math::sqrt(dx * dx + dz * dz);
            let outside = [last[0] + dx / length * 10.0, last[2] + dz / length * 10.0];
            let mut route = vec![outside];
            route.extend(ramp.iter().rev().map(|s| [s.p[0], s.p[2]]));
            route.extend(ramp.iter().skip(1).map(|s| [s.p[0], s.p[2]]));
            route.push(outside);
            // The same bounding hull speeds and walker as the accepted Stage A meter.
            for speed in [50.0, 20.0, 6.0] {
                let ground_y = native_mouths::acceptance_ground_m(map);
                let mut feet = geom
                    .support(&state, outside[0], outside[1], ground_y + 0.9)
                    .floor
                    .y;
                let mut position = outside;
                let mut worst = 0.0;
                let mut at = (position, position, feet, feet);
                let mut ticks = 0;
                for leg in route.windows(2) {
                    let (a, b) = (leg[0], leg[1]);
                    let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
                    let distance = math::sqrt(dx * dx + dz * dz);
                    let step = speed / 60.0;
                    let count = (distance / step).ceil() as usize;
                    for tick in 1..=count.max(1) {
                        let t = if tick >= count {
                            1.0
                        } else {
                            tick as f32 * step / distance
                        };
                        let next = [a[0] + dx * t, a[1] + dz * t];
                        let next_feet = geom.support(&state, next[0], next[1], feet + 0.9).floor.y;
                        let jump = discontinuity(|u| {
                            geom.support(
                                &state,
                                position[0] + (next[0] - position[0]) * u,
                                position[1] + (next[1] - position[1]) * u,
                                feet + (next_feet - feet) * u + 0.9,
                            )
                            .floor
                            .y
                        });
                        if jump > worst {
                            at = (position, next, feet, next_feet);
                        }
                        worst = det_max(worst, jump);
                        ticks += 1;
                        position = next;
                        feet = next_feet;
                    }
                }
                eprintln!(
                    "NATIVE_HOP {map} {} speed={speed} ticks={ticks} worst_m={worst} from={:?} to={:?} feet={}..{}",
                    opening.id, at.0, at.1, at.2, at.3
                );
                assert!(ticks > 0);
                if worst > 0.05 {
                    failures.push((map.to_owned(), opening.id.clone(), speed, worst));
                }
            }
        }
    }
    assert!(failures.is_empty(), "native hops exceed 50 mm: {failures:?}");
}
