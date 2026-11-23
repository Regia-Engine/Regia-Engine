// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): popup placement and the radial wheel's sector math on boundary
//! and hostile numbers, plus seeded properties. Pure functions; no goldens, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::f32::consts::{PI, TAU};

use regia_test_utils::fuzz::FuzzRng;
use regia_ui::popup::{PopupPlacement, place_popup};
use regia_ui::radial::{MAX_SECTORS, MIN_SECTORS, RadialGeometry, angle_delta, angle_from_up};

const SCREEN: [f32; 4] = [0.0, 0.0, 800.0, 600.0];

#[test]
fn popups_sit_on_the_preferred_side_when_there_is_room() {
    let anchor = [100.0, 100.0, 80.0, 30.0];
    let size = [120.0, 90.0];
    let below = place_popup(anchor, size, PopupPlacement::Below, SCREEN, 4.0);
    assert_eq!(below.rect, [100.0, 134.0, 120.0, 90.0]);
    assert!(!below.flipped && !below.shifted);
    let above = place_popup(
        [100.0, 300.0, 80.0, 30.0],
        size,
        PopupPlacement::Above,
        SCREEN,
        4.0,
    );
    assert_eq!(above.rect, [100.0, 206.0, 120.0, 90.0]);
    let right = place_popup(anchor, size, PopupPlacement::Right, SCREEN, 4.0);
    assert_eq!(right.rect, [184.0, 100.0, 120.0, 90.0]);
    let left = place_popup(
        [300.0, 100.0, 80.0, 30.0],
        size,
        PopupPlacement::Left,
        SCREEN,
        4.0,
    );
    assert_eq!(left.rect, [176.0, 100.0, 120.0, 90.0]);
    for placed in [below, above, right, left] {
        assert!(!placed.flipped && !placed.shifted, "{placed:?}");
    }
}

#[test]
fn popups_flip_only_when_the_other_side_has_more_room() {
    let size = [100.0, 200.0];
    // Near the bottom: below has 100 px, above has 450: flips.
    let anchor = [50.0, 470.0, 60.0, 30.0];
    let placed = place_popup(anchor, size, PopupPlacement::Below, SCREEN, 0.0);
    assert!(
        placed.flipped && placed.placement == PopupPlacement::Above,
        "{placed:?}"
    );
    assert_eq!(placed.rect[1], 470.0 - 200.0);
    // Both sides too small: the one with more room wins, and the popup shifts to stay inside.
    let tall = place_popup(
        [50.0, 280.0, 60.0, 30.0],
        [100.0, 500.0],
        PopupPlacement::Below,
        SCREEN,
        0.0,
    );
    assert!(
        tall.rect[1] >= 0.0 && tall.rect[1] + tall.rect[3] <= 600.0,
        "{tall:?}"
    );
    // The preferred side is kept when it is as small as the other.
    let tie = place_popup(
        [50.0, 285.0, 60.0, 30.0],
        [100.0, 500.0],
        PopupPlacement::Below,
        SCREEN,
        0.0,
    );
    assert!(!tie.flipped, "{tie:?}");
}

#[test]
fn popups_shift_inside_the_bounds_and_report_it() {
    let placed = place_popup(
        [760.0, 100.0, 30.0, 30.0],
        [120.0, 50.0],
        PopupPlacement::Below,
        SCREEN,
        0.0,
    );
    assert_eq!(placed.rect[0], 680.0, "right edge at the bounds");
    assert!(placed.shifted && !placed.flipped);
    let placed = place_popup(
        [-40.0, 100.0, 30.0, 30.0],
        [120.0, 50.0],
        PopupPlacement::Below,
        SCREEN,
        0.0,
    );
    assert_eq!(placed.rect[0], 0.0, "left edge at the bounds");
    assert!(placed.shifted);
    // Bounds that do not start at the origin.
    let inset = [20.0, 30.0, 400.0, 300.0];
    let placed = place_popup(
        [0.0, 0.0, 10.0, 10.0],
        [50.0, 50.0],
        PopupPlacement::Below,
        inset,
        0.0,
    );
    assert!(placed.rect[0] >= 20.0 && placed.rect[1] >= 30.0, "{placed:?}");
}

#[test]
fn a_popup_larger_than_the_bounds_keeps_the_top_left_corner() {
    let placed = place_popup(
        [300.0, 300.0, 10.0, 10.0],
        [2000.0, 1500.0],
        PopupPlacement::Below,
        SCREEN,
        0.0,
    );
    assert_eq!(&placed.rect[..2], &[0.0, 0.0]);
    assert_eq!(&placed.rect[2..], &[2000.0, 1500.0]);
    // Zero-size and negative-size bounds do not panic.
    for bounds in [[0.0, 0.0, 0.0, 0.0], [10.0, 10.0, -5.0, -5.0]] {
        let placed = place_popup(
            [5.0, 5.0, 1.0, 1.0],
            [10.0, 10.0],
            PopupPlacement::Right,
            bounds,
            2.0,
        );
        assert!(placed.rect.iter().all(|v| v.is_finite()), "{placed:?}");
    }
    // A negative popup size counts as zero.
    let placed = place_popup(
        [100.0, 100.0, 10.0, 10.0],
        [-30.0, -30.0],
        PopupPlacement::Below,
        SCREEN,
        0.0,
    );
    assert_eq!(&placed.rect[2..], &[0.0, 0.0]);
}

#[test]
fn nan_bounds_or_size_do_not_panic() {
    let nan = f32::NAN;
    for bounds in [
        [nan, 0.0, 800.0, 600.0],
        [0.0, nan, 800.0, 600.0],
        [0.0, 0.0, nan, nan],
        [nan; 4],
    ] {
        for size in [[10.0, 10.0], [nan, 10.0], [10.0, nan]] {
            for side in [
                PopupPlacement::Below,
                PopupPlacement::Above,
                PopupPlacement::Left,
                PopupPlacement::Right,
            ] {
                let _ = place_popup([100.0, 100.0, 10.0, 10.0], size, side, bounds, 4.0);
            }
        }
    }
    // A NaN anchor position still places the popup inside the bounds.
    let placed = place_popup(
        [nan, nan, 10.0, 10.0],
        [50.0, 50.0],
        PopupPlacement::Below,
        SCREEN,
        0.0,
    );
    assert!(placed.rect.iter().all(|v| v.is_finite()), "{placed:?}");
}

#[test]
fn placement_names_and_opposites() {
    for side in [
        PopupPlacement::Below,
        PopupPlacement::Above,
        PopupPlacement::Left,
        PopupPlacement::Right,
    ] {
        assert_eq!(PopupPlacement::parse(side.name()), Some(side));
        assert_eq!(side.opposite().opposite(), side);
        assert_ne!(side.opposite(), side);
    }
    for bad in ["", "Below", "top", "below "] {
        assert_eq!(PopupPlacement::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn seeded_placements_stay_inside_the_bounds_when_the_popup_fits() {
    let mut rng = FuzzRng::new(0x90_9A);
    let u = |rng: &mut FuzzRng| rng.below(10_001) as f32 / 10_000.0;
    for case in 0..2000 {
        let bounds = [
            u(&mut rng) * 100.0,
            u(&mut rng) * 100.0,
            300.0 + u(&mut rng) * 700.0,
            300.0 + u(&mut rng) * 700.0,
        ];
        let size = [u(&mut rng) * 280.0, u(&mut rng) * 280.0];
        let anchor = [
            bounds[0] + u(&mut rng) * bounds[2],
            bounds[1] + u(&mut rng) * bounds[3],
            u(&mut rng) * 60.0,
            u(&mut rng) * 60.0,
        ];
        let side = [
            PopupPlacement::Below,
            PopupPlacement::Above,
            PopupPlacement::Left,
            PopupPlacement::Right,
        ][rng.below(4)];
        let gap = u(&mut rng) * 8.0;
        let placed = place_popup(anchor, size, side, bounds, gap);
        let [x, y, w, h] = placed.rect;
        assert_eq!([w, h], size, "case {case}");
        let eps = 1e-3;
        assert!(
            x >= bounds[0] - eps
                && y >= bounds[1] - eps
                && x + w <= bounds[0] + bounds[2] + eps
                && y + h <= bounds[1] + bounds[3] + eps,
            "case {case}: {placed:?} outside {bounds:?}"
        );
        assert_eq!(placed.flipped, placed.placement != side, "case {case}");
        if !placed.shifted {
            // Unshifted popups touch the anchor's side at exactly the gap.
            let touch = match placed.placement {
                PopupPlacement::Below => (y - (anchor[1] + anchor[3])).abs(),
                PopupPlacement::Above => ((anchor[1] - y - h) - 0.0).abs() - 0.0,
                PopupPlacement::Right => (x - (anchor[0] + anchor[2])).abs(),
                PopupPlacement::Left => (anchor[0] - x - w).abs(),
            };
            assert!(
                (touch - gap).abs() < eps,
                "case {case}: {placed:?} gap {touch} vs {gap}"
            );
        }
        // Deterministic.
        assert_eq!(placed, place_popup(anchor, size, side, bounds, gap));
    }
}

// ---- radial wheel -------------------------------------------------------------------------

#[test]
fn angles_are_clockwise_from_up_and_never_reach_a_full_turn() {
    for (x, y, want) in [
        (0.0, 1.0, 0.0),
        (1.0, 0.0, PI / 2.0),
        (0.0, -1.0, PI),
        (-1.0, 0.0, 3.0 * PI / 2.0),
    ] {
        assert!((angle_from_up(x, y) - want).abs() < 1e-6, "({x}, {y})");
    }
    assert_eq!(angle_from_up(0.0, 0.0), 0.0);
    // A hair left of straight up is nearly a full turn, but not TAU.
    for x in [-1e-9, -1e-12, -f32::MIN_POSITIVE, -0.0] {
        let a = angle_from_up(x, 1.0);
        assert!((0.0..TAU).contains(&a), "x {x}: {a}");
    }
    let mut rng = FuzzRng::new(8);
    for _ in 0..1000 {
        let (x, y) = (rng.below(2001) as f32 - 1000.0, rng.below(2001) as f32 - 1000.0);
        assert!((0.0..TAU).contains(&angle_from_up(x, y)), "({x}, {y})");
    }
}

#[test]
fn angle_delta_is_the_short_way_round() {
    assert!(angle_delta(0.1, TAU - 0.1) > 0.0 && (angle_delta(0.1, TAU - 0.1) - 0.2).abs() < 1e-5);
    assert!((angle_delta(TAU - 0.1, 0.1) + 0.2).abs() < 1e-5);
    assert!(angle_delta(1.0, 1.0).abs() < 1e-7);
    assert!(
        angle_delta(0.0, TAU).abs() < 1e-5,
        "a full turn apart is no difference"
    );
    assert!(
        angle_delta(100.0 * TAU + 0.5, 0.25).abs() - 0.25 < 1e-2,
        "many turns"
    );
    for i in 0..360 {
        for j in (0..360).step_by(7) {
            let d = angle_delta((i as f32).to_radians(), (j as f32).to_radians());
            assert!((-PI - 1e-5..=PI + 1e-5).contains(&d), "{i} {j}: {d}");
        }
    }
}

#[test]
fn sector_counts_clamp_and_every_centre_maps_to_its_own_sector() {
    for sectors in [0, 1, 2, 3, 5, 8, 12, 13, 99] {
        let g = RadialGeometry {
            sectors,
            ..RadialGeometry::default()
        };
        let n = g.count();
        assert!((MIN_SECTORS..=MAX_SECTORS).contains(&n), "{sectors} -> {n}");
        for start in [0.0, 0.3, -1.0, 7.0] {
            let g = RadialGeometry {
                sectors,
                start,
                ..RadialGeometry::default()
            };
            for i in 0..n {
                assert_eq!(
                    g.sector_at(g.centre_angle(i)),
                    i,
                    "{sectors} sectors, start {start}, sector {i}"
                );
            }
            // Every angle lands in a valid sector.
            for deg in 0..720 {
                assert!(g.sector_at((deg as f32).to_radians()) < n);
            }
        }
    }
    let g = RadialGeometry {
        sectors: 4,
        ..RadialGeometry::default()
    };
    assert_eq!(g.sector_at(PI / 4.0 - 1e-4), 0);
    assert_eq!(g.sector_at(PI / 4.0 + 1e-4), 1);
    assert_eq!(
        g.sector_at(f32::NAN),
        g.sector_at(0.0),
        "NaN counts as straight up"
    );
}

#[test]
fn selection_has_a_dead_zone_hysteresis_and_ignores_bad_numbers() {
    let g = RadialGeometry::default();
    assert_eq!(g.select(None, [0.0, 0.0]), None);
    assert_eq!(g.select(None, [0.0, 0.34]), None, "inside the dead zone");
    assert_eq!(g.select(None, [0.0, 0.36]), Some(0));
    assert_eq!(
        g.select(Some(0), [0.0, 0.30]),
        Some(0),
        "a selected sector survives a dip to 80 % of the dead zone"
    );
    assert_eq!(g.select(Some(0), [0.0, 0.27]), None, "below 80 % it clears");
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(g.select(None, [bad, 0.9]), None, "{bad}");
        assert_eq!(g.select(Some(1), [0.9, bad]), None, "{bad}");
    }
    let nan_dz = RadialGeometry {
        dead_zone: f32::NAN,
        ..g
    };
    assert_eq!(
        nan_dz.select(None, [0.0, 0.0001]),
        None,
        "a NaN dead zone does not switch the dead zone off"
    );
    // Hysteresis: just past the boundary of sector 0 the selection stays, far past it changes.
    let boundary = g.width() * 0.5;
    let at = |angle: f32| [angle.sin(), angle.cos()];
    assert_eq!(g.select(Some(0), at(boundary + 0.02)), Some(0));
    assert_eq!(g.select(None, at(boundary + 0.02)), Some(1));
    assert_eq!(g.select(Some(0), at(boundary + 0.5 * g.width())), Some(1));
    // A current sector out of range is ignored.
    assert_eq!(g.select(Some(99), at(0.0)), Some(0));
    // Two sectors with maximum hysteresis can still change sector.
    let two = RadialGeometry {
        sectors: 2,
        hysteresis: 9.0,
        ..g
    };
    assert_eq!(two.select(Some(0), at(PI)), Some(1));
}

#[test]
fn selecting_the_current_sector_is_stable_and_matches_sector_at_when_cold() {
    let mut rng = FuzzRng::new(0x5EC7);
    for _ in 0..1000 {
        let g = RadialGeometry {
            sectors: 2 + rng.below(11),
            start: rng.below(628) as f32 / 100.0,
            ..RadialGeometry::default()
        };
        let angle = rng.below(6283) as f32 / 1000.0;
        let v = [angle.sin() * 0.9, angle.cos() * 0.9];
        let cold = g.select(None, v).expect("outside the dead zone");
        assert_eq!(cold, g.sector_at(angle_from_up(v[0], v[1])));
        assert_eq!(
            g.select(Some(cold), v),
            Some(cold),
            "feeding the answer back changes nothing"
        );
    }
}
