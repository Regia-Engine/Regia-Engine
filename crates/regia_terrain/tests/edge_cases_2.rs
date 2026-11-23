// SPDX-License-Identifier: MPL-2.0
//! Edge cases, second batch: deformation, far-layer picking, debug views, bounce light.
//! No GPU, no goldens, no wall-clock.

use regia_terrain::bounce::{bounce_and_horizon, bounce_factor};
use regia_terrain::debug_view::{TERRAIN_DEBUG_VIEWS, TerrainDebugView, TerrainDebugViewHandle};
use regia_terrain::far_layers::{hard_top_two, soft_top_two};
use regia_terrain::{
    Crater, HeightMips, MAX_SCORCH_MARKS, ScorchList, ScorchMark, TerrainMap, apply_crater, crater_dirty_rect,
};

fn flat(n: u32, world: f32, h: f32) -> TerrainMap {
    TerrainMap::from_heights(n, n, vec![h; (n * n) as usize], world, 1.0, 0.0).expect("map")
}

// ---- craters -----------------------------------------------------------------------------

#[test]
fn crater_validation_rejects_every_non_finite_field() {
    let ok = Crater {
        center: [1.0, 2.0],
        radius: 3.0,
        depth: 1.0,
        rim_height: 0.2,
    };
    assert!(ok.validate().is_ok());
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            Crater {
                center: [bad, 0.0],
                ..ok
            }
            .validate()
            .is_err()
        );
        assert!(
            Crater {
                center: [0.0, bad],
                ..ok
            }
            .validate()
            .is_err()
        );
        assert!(Crater { radius: bad, ..ok }.validate().is_err());
        assert!(Crater { depth: bad, ..ok }.validate().is_err());
        assert!(
            Crater {
                rim_height: bad,
                ..ok
            }
            .validate()
            .is_err()
        );
    }
    for radius in [0.0, -1.0] {
        assert!(Crater { radius, ..ok }.validate().is_err());
    }
    // A negative depth (a mound) and a zero rim are allowed.
    assert!(
        Crater {
            depth: -2.0,
            rim_height: 0.0,
            ..ok
        }
        .validate()
        .is_ok()
    );
}

#[test]
fn a_rejected_crater_leaves_the_map_untouched() {
    let mut map = flat(17, 32.0, 4.0);
    let before = map.heights().to_vec();
    let bad = Crater {
        center: [0.0, 0.0],
        radius: f32::NAN,
        depth: 1.0,
        rim_height: 0.0,
    };
    assert!(apply_crater(&mut map, &bad).is_err());
    assert_eq!(map.heights(), before.as_slice());
}

#[test]
fn crater_displacement_is_zero_at_and_beyond_the_radius_and_continuous() {
    let c = Crater {
        center: [0.0, 0.0],
        radius: 10.0,
        depth: 3.0,
        rim_height: 0.5,
    };
    assert_eq!(c.displacement(10.0), 0.0);
    assert_eq!(c.displacement(1.0e9), 0.0);
    assert_eq!(c.displacement(f32::INFINITY), 0.0);
    assert!(
        (c.displacement(0.0) + 3.0).abs() < 1.0e-6,
        "centre is the full depth"
    );
    assert!(c.displacement(9.999).abs() < 1.0e-2, "no step at the rim edge");
    // A negative r reads as the centre (t clamps), never NaN.
    assert!(c.displacement(-4.0).is_finite());
}

#[test]
fn crater_dirty_rect_covers_every_changed_sample_and_stays_in_the_map() {
    let n = 33;
    let world = 64.0;
    let craters = [
        Crater {
            center: [0.0, 0.0],
            radius: 9.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        // Straddling each edge and each corner of the map.
        Crater {
            center: [-32.0, 0.0],
            radius: 6.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        Crater {
            center: [32.0, 5.0],
            radius: 6.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        Crater {
            center: [3.0, -32.0],
            radius: 6.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        Crater {
            center: [3.0, 32.0],
            radius: 6.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        Crater {
            center: [32.0, 32.0],
            radius: 8.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        Crater {
            center: [-32.0, -32.0],
            radius: 8.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        // Bigger than the whole map.
        Crater {
            center: [0.0, 0.0],
            radius: 500.0,
            depth: 2.0,
            rim_height: 0.3,
        },
        // Smaller than a sample spacing (2 m): may touch zero or one sample.
        Crater {
            center: [0.9, 0.9],
            radius: 0.4,
            depth: 2.0,
            rim_height: 0.3,
        },
    ];
    for crater in craters {
        let mut map = flat(n, world, 10.0);
        let rect = crater_dirty_rect(&map, &crater);
        assert!(rect.col_end <= n && rect.row_end <= n, "{crater:?} -> {rect:?}");
        let applied = apply_crater(&mut map, &crater).expect("crater");
        assert_eq!(applied, rect);
        for row in 0..n {
            for col in 0..n {
                let changed = map.height_at_sample(col as i64, row as i64) != 10.0;
                let inside = col >= rect.col_start
                    && col < rect.col_end
                    && row >= rect.row_start
                    && row < rect.row_end;
                assert!(
                    !changed || inside,
                    "{crater:?}: sample ({col},{row}) changed outside {rect:?}"
                );
            }
        }
        assert!(map.heights().iter().all(|h| h.is_finite()));
    }
}

#[test]
fn a_crater_far_off_the_map_changes_nothing() {
    for centre in [[1000.0, 0.0], [0.0, -1000.0], [-1.0e6, 1.0e6], [1.0e30, 1.0e30]] {
        let mut map = flat(17, 32.0, 6.0);
        let crater = Crater {
            center: centre,
            radius: 20.0,
            depth: 4.0,
            rim_height: 1.0,
        };
        apply_crater(&mut map, &crater).expect("valid crater");
        assert!(map.heights().iter().all(|&h| h == 6.0), "{centre:?}");
    }
}

#[test]
fn applying_the_same_crater_twice_digs_twice_and_is_deterministic() {
    let crater = Crater {
        center: [1.0, -2.0],
        radius: 8.0,
        depth: 2.0,
        rim_height: 0.0,
    };
    let mut a = flat(33, 64.0, 10.0);
    let mut b = flat(33, 64.0, 10.0);
    apply_crater(&mut a, &crater).expect("a");
    apply_crater(&mut b, &crater).expect("b");
    assert_eq!(a.heights(), b.heights());
    let once = a.sample_bilinear(1.0, -2.0);
    apply_crater(&mut a, &crater).expect("a again");
    let twice = a.sample_bilinear(1.0, -2.0);
    assert!(
        twice < once - 1.0,
        "the second crater digs deeper: {once} then {twice}"
    );
}

// ---- scorch marks ------------------------------------------------------------------------

#[test]
fn scorch_list_keeps_the_newest_marks_in_order_and_packs_the_count() {
    let mark = |i: usize| ScorchMark {
        center: [i as f32, -(i as f32)],
        radius: 1.0 + i as f32,
        strength: 0.5,
    };
    let mut list = ScorchList::default();
    let empty = list.pack();
    assert_eq!(empty[MAX_SCORCH_MARKS], [0.0; 4]);
    assert!(empty[..MAX_SCORCH_MARKS].iter().all(|r| *r == [0.0; 4]));

    for i in 0..MAX_SCORCH_MARKS + 5 {
        list.push(mark(i));
        assert!(list.marks.len() <= MAX_SCORCH_MARKS);
    }
    assert_eq!(list.marks.len(), MAX_SCORCH_MARKS);
    assert_eq!(list.marks[0], mark(5), "the five oldest were dropped");
    assert_eq!(list.marks[MAX_SCORCH_MARKS - 1], mark(MAX_SCORCH_MARKS + 4));
    let rows = list.pack();
    assert_eq!(rows[0], [5.0, -5.0, 6.0, 0.5]);
    assert_eq!(rows[MAX_SCORCH_MARKS][0], MAX_SCORCH_MARKS as f32);

    // A hand-built over-full list still packs inside the array and clamps the count.
    let over = ScorchList {
        marks: (0..MAX_SCORCH_MARKS + 3).map(mark).collect(),
    };
    let rows = over.pack();
    assert_eq!(rows[MAX_SCORCH_MARKS - 1][0], (MAX_SCORCH_MARKS - 1) as f32);
    assert_eq!(rows[MAX_SCORCH_MARKS][0], MAX_SCORCH_MARKS as f32);
}

// ---- far layers --------------------------------------------------------------------------

fn sum(v: &[f32]) -> f32 {
    v.iter().sum()
}

#[test]
fn soft_top_two_edge_inputs() {
    assert!(soft_top_two(&[]).is_empty());
    assert_eq!(soft_top_two(&[0.0, 0.0, 0.0]), vec![0.0; 3]);
    assert_eq!(
        soft_top_two(&[-1.0, -2.0]),
        vec![0.0; 2],
        "no positive weight, nothing to keep"
    );
    assert_eq!(soft_top_two(&[0.7]), vec![1.0]);
    let pair = soft_top_two(&[0.3, 0.9]);
    assert!(
        (pair[0] - 0.25).abs() < 1e-6 && (pair[1] - 0.75).abs() < 1e-6,
        "{pair:?}"
    );
    // All equal: the third subtracts everything, the first strongest takes it all.
    assert_eq!(soft_top_two(&[0.25, 0.25, 0.25, 0.25]), vec![1.0, 0.0, 0.0, 0.0]);
    // Three-way tie above a clear leader.
    let w = soft_top_two(&[0.1, 0.1, 0.1, 0.7]);
    assert!(
        (w[3] - 1.0).abs() < 1e-6 && w[..3].iter().all(|&c| c == 0.0),
        "{w:?}"
    );
}

#[test]
fn soft_top_two_is_a_distribution_with_at_most_two_entries_and_is_continuous() {
    // A fixed pseudo-random sweep (LCG, fixed seed).
    let mut state = 0x1234_5678_u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / 16_777_216.0
    };
    for len in 1..=8 {
        for _ in 0..200 {
            let w: Vec<f32> = (0..len).map(|_| next()).collect();
            let out = soft_top_two(&w);
            assert_eq!(out.len(), len);
            assert!(out.iter().filter(|&&c| c > 0.0).count() <= 2, "{w:?} -> {out:?}");
            assert!(out.iter().all(|&c| (0.0..=1.0).contains(&c)));
            assert!((sum(&out) - 1.0).abs() < 1e-4, "{w:?} -> {out:?}");
            // Determinism.
            assert_eq!(out, soft_top_two(&w));
            // The hard pick agrees on which layer is strongest.
            let hard = hard_top_two(&w);
            let argmax = |v: &[f32]| {
                v.iter()
                    .enumerate()
                    .fold(0, |b, (i, &c)| if c > v[b] { i } else { b })
            };
            assert_eq!(argmax(&out), argmax(&w));
            assert_eq!(argmax(&hard), argmax(&w));
        }
    }
    // Continuity where the hard pick jumps: weights 0.4, 0.3, w3 with w3 crossing 0.3.
    let at = |w3: f32| soft_top_two(&[0.4, 0.3, w3]);
    let (below, above) = (at(0.3 - 1.0e-4), at(0.3 + 1.0e-4));
    for i in 0..3 {
        assert!(
            (below[i] - above[i]).abs() < 0.01,
            "layer {i}: {below:?} vs {above:?}"
        );
    }
    let (hb, ha) = (
        hard_top_two(&[0.4, 0.3, 0.3 - 1.0e-4]),
        hard_top_two(&[0.4, 0.3, 0.3 + 1.0e-4]),
    );
    assert!(
        (hb[1] - ha[1]).abs() > 0.3,
        "the hard pick does jump: {hb:?} vs {ha:?}"
    );
}

#[test]
fn hard_top_two_edge_inputs() {
    assert!(hard_top_two(&[]).is_empty());
    assert_eq!(hard_top_two(&[0.0, 0.0]), vec![0.0; 2]);
    assert_eq!(hard_top_two(&[0.5]), vec![1.0]);
    // A tie goes to the lower index; a zero second weight is dropped.
    assert_eq!(hard_top_two(&[0.5, 0.5, 0.5]), vec![0.5, 0.5, 0.0]);
    assert_eq!(hard_top_two(&[0.0, 0.8, 0.0]), vec![0.0, 1.0, 0.0]);
}

#[test]
fn soft_top_two_with_nan_weights_never_returns_nan() {
    for w in [
        [f32::NAN, 0.5, 0.2],
        [0.5, f32::NAN, 0.2],
        [0.5, 0.2, f32::NAN],
        [f32::NAN; 3],
    ] {
        let out = soft_top_two(&w);
        assert!(out.iter().all(|c| c.is_finite()), "{w:?} -> {out:?}");
    }
}

// ---- debug views -------------------------------------------------------------------------

#[test]
fn debug_view_table_is_consistent() {
    assert!(!TERRAIN_DEBUG_VIEWS.is_empty());
    for (i, v) in TERRAIN_DEBUG_VIEWS.iter().enumerate() {
        assert!(!v.name.is_empty() && !v.channels.is_empty());
        assert!(
            v.name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{}",
            v.name
        );
        for other in &TERRAIN_DEBUG_VIEWS[i + 1..] {
            assert_ne!(v.id, other.id, "duplicate id");
            assert_ne!(v.name, other.name, "duplicate name");
        }
        assert_eq!(TerrainDebugView::by_id(v.id), Some(*v));
        assert_eq!(TerrainDebugView::find(v.name), Some(*v));
        assert_eq!(TerrainDebugView::find(&v.id.to_string()), Some(*v));
        assert_eq!(
            TerrainDebugView::find(&format!("  {}\t", v.name.to_uppercase())),
            Some(*v)
        );
    }
    // Ids are dense from 0 ("off"), so the shader switch has no hole.
    let mut ids: Vec<u32> = TERRAIN_DEBUG_VIEWS.iter().map(|v| v.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, (0..ids.len() as u32).collect::<Vec<_>>());
}

#[test]
fn debug_view_lookup_misses() {
    for key in ["", " ", "-1", "1.5", "+", "nope", "9999999999999"] {
        let hit = TerrainDebugView::find(key);
        assert!(hit.is_none(), "{key:?} -> {hit:?}");
    }
    assert_eq!(TerrainDebugView::by_id(0).map(|v| v.name), Some("off"));
    assert_eq!(TerrainDebugView::find("OFF").map(|v| v.id), Some(0));
    assert!(TerrainDebugView::by_id(u32::MAX).is_none());
}

#[test]
fn debug_view_handle_is_shared_between_clones() {
    let a = TerrainDebugViewHandle::default();
    assert_eq!(a.get(), 0);
    let b = a.clone();
    b.set(7);
    assert_eq!(a.get(), 7);
    a.set(u32::MAX);
    assert_eq!(b.get(), u32::MAX);
}

// ---- bounce light ------------------------------------------------------------------------

#[test]
fn bounce_on_flat_ground_is_zero_and_the_horizon_is_clear() {
    let map = flat(65, 1280.0, 5.0);
    let mips = HeightMips::build(&map);
    let (factor, horizon) = bounce_and_horizon(&map, &mips, [0.0, 5.0, 0.0], [0.3, 0.8, 0.2], 1.0);
    assert_eq!(factor, 0.0);
    assert_eq!(horizon, [0.0; 4]);
}

#[test]
fn bounce_is_finite_and_non_negative_for_degenerate_arguments() {
    // A bowl so there is something to bounce off.
    let n = 65_u32;
    let mut heights = Vec::new();
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * 1280.0 - 640.0;
            let z = row as f32 / (n - 1) as f32 * 1280.0 - 640.0;
            heights.push(40.0 * ((x * x + z * z).sqrt() / 600.0).min(1.0));
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, 1280.0, 1.0, 0.0).expect("map");
    let mips = HeightMips::build(&map);
    let sun_cases = [
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0; 3],
        [f32::NAN, 1.0, 0.0],
    ];
    let world_cases = [
        [0.0, 0.0, 0.0],
        [700.0, 0.0, 700.0],
        [-1.0e6, 0.0, 1.0e6],
        [0.0, 1.0e6, 0.0],
        [0.0, -1.0e6, 0.0],
    ];
    for world in world_cases {
        for sun in sun_cases {
            for footprint in [0.0, 1.0, 1.0e6] {
                let f = bounce_factor(&map, &mips, world, sun, footprint);
                assert!(
                    f.is_finite() && f >= 0.0,
                    "world {world:?} sun {sun:?} footprint {footprint}: {f}"
                );
                // Same inputs, same answer.
                assert_eq!(f, bounce_factor(&map, &mips, world, sun, footprint));
            }
        }
    }
    // In the bowl's floor the walls rise on every side.
    let (_, horizon) = bounce_and_horizon(&map, &mips, [0.0, 0.0, 0.0], [0.0, 1.0, 0.0], 1.0);
    assert!(horizon.iter().all(|&t| t > 0.0), "{horizon:?}");
}
