// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): the pure scroll math (thumb placement, rubber band, inertia,
//! chaining) on hostile inputs and as seeded properties. No ECS, no goldens, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_test_utils::fuzz::FuzzRng;
use regia_ui::scroll_motion::{
    ChainLink, DragSamples, MAX_FLING_SPEED, Overscroll, ScrollAxes, ScrollChaining, ScrollInput, coast,
    distribute, fling_distance, overscroll_amount, rubber_band, rubber_band_inverse, rubber_offset,
    spring_step,
};
use regia_ui::scrollbar::{
    ScrollAxis, Scrollbars, bar_rects, drag_to_scroll, thumb_span, track_click_to_offset,
};

const HOSTILE: [f32; 8] = [
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    -1.0,
    0.0,
    -0.0,
    1e-30,
    3.0e38,
];

fn unit(rng: &mut FuzzRng) -> f32 {
    rng.below(1_000_001) as f32 / 1_000_000.0
}

// ---- scrollbar thumb ----------------------------------------------------------------------

#[test]
fn thumb_span_is_none_or_finite_and_inside_the_track_for_hostile_inputs() {
    for track in HOSTILE.into_iter().chain([200.0]) {
        for viewport in HOSTILE.into_iter().chain([100.0]) {
            for content in HOSTILE.into_iter().chain([1000.0]) {
                for offset in HOSTILE.into_iter().chain([50.0]) {
                    for min_thumb in [0.0, 24.0, f32::NAN, 1e9] {
                        if let Some((start, len)) = thumb_span(track, viewport, content, offset, min_thumb) {
                            assert!(
                                start.is_finite() && len.is_finite() && start >= 0.0 && len >= 0.0,
                                "({track}, {viewport}, {content}, {offset}, {min_thumb}) -> ({start}, {len})"
                            );
                            assert!(
                                start + len <= track * (1.0 + 1e-6) + 1e-3,
                                "thumb leaves the track: ({track}, {viewport}, {content}, {offset}, {min_thumb}) -> ({start}, {len})"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn thumb_span_boundaries() {
    // Content exactly one pixel larger than the view still counts as fitting.
    assert_eq!(thumb_span(100.0, 100.0, 101.0, 0.0, 10.0), None);
    assert!(thumb_span(100.0, 100.0, 101.1, 0.0, 10.0).is_some());
    assert_eq!(thumb_span(0.0, 100.0, 500.0, 0.0, 10.0), None, "no track");
    assert_eq!(thumb_span(-5.0, 100.0, 500.0, 0.0, 10.0), None);
    // At the ends the thumb touches the ends of the track.
    let (start, len) = thumb_span(200.0, 100.0, 400.0, 0.0, 10.0).unwrap();
    assert_eq!((start, len), (0.0, 50.0));
    let (start, len) = thumb_span(200.0, 100.0, 400.0, 300.0, 10.0).unwrap();
    assert_eq!(start + len, 200.0);
    // Past either end the thumb stays on the track.
    assert_eq!(thumb_span(200.0, 100.0, 400.0, -50.0, 10.0).unwrap().0, 0.0);
    assert_eq!(thumb_span(200.0, 100.0, 400.0, 9999.0, 10.0).unwrap().0, 150.0);
    // A minimum thumb longer than the track is capped at the track.
    assert_eq!(thumb_span(30.0, 100.0, 400.0, 10.0, 90.0).unwrap(), (0.0, 30.0));
    // A NaN scroll offset counts as the start.
    assert_eq!(thumb_span(200.0, 100.0, 400.0, f32::NAN, 10.0).unwrap().0, 0.0);
}

#[test]
fn thumb_drag_and_track_click_agree_with_the_thumb_position() {
    let mut rng = FuzzRng::new(0x7B_4D);
    for _ in 0..500 {
        let track = 50.0 + unit(&mut rng) * 900.0;
        let viewport = 50.0 + unit(&mut rng) * 500.0;
        let content = viewport + 10.0 + unit(&mut rng) * 5000.0;
        let min_thumb = unit(&mut rng) * 40.0;
        let max_scroll = content - viewport;
        let Some((_, len)) = thumb_span(track, viewport, content, 0.0, min_thumb) else {
            continue;
        };
        if track - len < 1.0 {
            continue;
        }
        // Clicking the track puts the thumb's center on the click (clamped to the track).
        let pos = unit(&mut rng) * track;
        let offset = track_click_to_offset(pos, track, len, max_scroll);
        assert!((0.0..=max_scroll + 1e-2).contains(&offset), "offset {offset}");
        let (start, len2) = thumb_span(track, viewport, content, offset, min_thumb).unwrap();
        assert_eq!(len, len2);
        let want_center = pos.clamp(len / 2.0, track - len / 2.0);
        assert!(
            (start + len / 2.0 - want_center).abs() < 0.05,
            "track {track} view {viewport} content {content} pos {pos}: center {} want {want_center}",
            start + len / 2.0
        );
        // Dragging the thumb by its full travel scrolls the full range.
        let full = drag_to_scroll(track - len, track, len, max_scroll);
        assert!(
            (full - max_scroll).abs() < 1e-2 * max_scroll.max(1.0),
            "{full} vs {max_scroll}"
        );
    }
}

#[test]
fn drag_and_click_with_no_travel_or_bad_numbers_move_nothing() {
    assert_eq!(
        drag_to_scroll(10.0, 50.0, 50.0, 500.0),
        0.0,
        "thumb fills the track"
    );
    assert_eq!(
        drag_to_scroll(10.0, 50.0, 80.0, 500.0),
        0.0,
        "thumb longer than the track"
    );
    assert_eq!(drag_to_scroll(f32::NAN, 100.0, 20.0, 500.0), 0.0);
    assert_eq!(drag_to_scroll(f32::INFINITY, 100.0, 20.0, 500.0), 0.0);
    assert_eq!(drag_to_scroll(10.0, f32::NAN, 20.0, 500.0), 0.0);
    assert_eq!(track_click_to_offset(10.0, 50.0, 50.0, 500.0), 0.0);
    assert_eq!(track_click_to_offset(f32::NAN, 100.0, 20.0, 500.0), 0.0);
    assert_eq!(
        track_click_to_offset(-1e9, 100.0, 20.0, 500.0),
        0.0,
        "before the track"
    );
    assert_eq!(
        track_click_to_offset(1e9, 100.0, 20.0, 500.0),
        500.0,
        "after the track"
    );
}

#[test]
fn bar_rects_edge_cases() {
    let config = Scrollbars::default();
    let view = [10.0, 20.0, 300.0, 200.0];
    // Content that fits: no bar. An axis turned off: no bar.
    assert_eq!(
        bar_rects(
            &config,
            ScrollAxis::Vertical,
            view,
            [300.0, 150.0],
            [0.0; 2],
            1.0,
            false
        ),
        None
    );
    let off = Scrollbars {
        vertical: false,
        ..Scrollbars::default()
    };
    assert_eq!(
        bar_rects(
            &off,
            ScrollAxis::Vertical,
            view,
            [300.0, 900.0],
            [0.0; 2],
            1.0,
            false
        ),
        None
    );
    // A view smaller than the other bar's inset has no track.
    let tiny = [0.0, 0.0, 5.0, 5.0];
    assert_eq!(
        bar_rects(
            &config,
            ScrollAxis::Vertical,
            tiny,
            [5.0, 900.0],
            [0.0; 2],
            1.0,
            true
        ),
        None
    );
    // The thumb lies inside the track, and the other bar's inset shortens the track.
    let full = bar_rects(
        &config,
        ScrollAxis::Vertical,
        view,
        [300.0, 900.0],
        [0.0, 300.0],
        1.0,
        false,
    )
    .unwrap();
    let inset = bar_rects(
        &config,
        ScrollAxis::Vertical,
        view,
        [300.0, 900.0],
        [0.0, 300.0],
        1.0,
        true,
    )
    .unwrap();
    assert!(inset.track[3] < full.track[3]);
    for r in [full, inset] {
        assert!(r.thumb[1] >= r.track[1] - 1e-3);
        assert!(r.thumb[1] + r.thumb[3] <= r.track[1] + r.track[3] + 1e-3);
        assert_eq!(r.thumb[0], r.track[0]);
    }
    // A thickness of zero or less still draws a bar (at least 1 px).
    let thin = Scrollbars {
        horizontal: true,
        thickness: -4.0,
        ..Scrollbars::default()
    };
    let r = bar_rects(
        &thin,
        ScrollAxis::Horizontal,
        view,
        [900.0, 200.0],
        [0.0; 2],
        1.0,
        false,
    )
    .unwrap();
    assert_eq!(r.track[3], 1.0);
    // Output is in UI px: doubling the scale halves every number.
    let one = bar_rects(
        &config,
        ScrollAxis::Vertical,
        view,
        [300.0, 900.0],
        [0.0, 100.0],
        1.0,
        false,
    )
    .unwrap();
    let two = bar_rects(
        &config,
        ScrollAxis::Vertical,
        [20.0, 40.0, 600.0, 400.0],
        [600.0, 1800.0],
        [0.0, 200.0],
        2.0,
        false,
    )
    .unwrap();
    for i in 0..4 {
        assert!((one.track[i] - two.track[i]).abs() < 1e-3, "track {i}");
    }
}

// ---- rubber band, inertia, spring ---------------------------------------------------------

#[test]
fn rubber_band_is_monotone_bounded_and_safe_on_hostile_input() {
    let dim = 400.0;
    let mut last = 0.0;
    for step in 0..200 {
        let shown = rubber_band(step as f32 * 25.0, dim);
        assert!(shown >= last && shown < dim, "step {step}: {shown}");
        last = shown;
    }
    // 3e38 is left out as a view length: `dim / 0.55` is not representable in f32.
    for bad in HOSTILE {
        for d in HOSTILE.into_iter().filter(|d| *d != 3.0e38) {
            let a = rubber_band(bad, d);
            let b = rubber_band_inverse(bad, d);
            assert!(
                !a.is_nan() && !b.is_nan(),
                "rubber({bad}, {d}) = {a}, inverse = {b}"
            );
        }
    }
    assert_eq!(rubber_band(100.0, 0.0), 0.0);
    assert_eq!(rubber_band(-100.0, 100.0), 0.0);
    // The inverse never asks for infinity, even for a request at the limit.
    assert!(rubber_band_inverse(400.0, 400.0).is_finite());
    assert!(rubber_band_inverse(1e9, 400.0).is_finite());
}

#[test]
fn rubber_offset_is_continuous_at_the_edges_and_monotone_in_the_drag() {
    let (max, dim) = (1000.0, 400.0);
    // Inside the range a drag is 1:1; at the edge it is continuous.
    assert_eq!(rubber_offset(500.0, max, 100.0, dim), 600.0);
    assert!((rubber_offset(max, max, 0.001, dim) - max).abs() < 0.01);
    assert!((rubber_offset(0.0, max, -0.001, dim)).abs() < 0.01);
    let mut rng = FuzzRng::new(99);
    for _ in 0..500 {
        let offset = -dim * unit(&mut rng) + (max + dim) * unit(&mut rng);
        let (d1, d2) = (unit(&mut rng) * 300.0 - 150.0, unit(&mut rng) * 300.0 - 150.0);
        let (lo, hi) = if d1 <= d2 { (d1, d2) } else { (d2, d1) };
        let a = rubber_offset(offset, max, lo, dim);
        let b = rubber_offset(offset, max, hi, dim);
        assert!(
            a <= b + 1e-3,
            "monotone: offset {offset}, {lo} -> {a}, {hi} -> {b}"
        );
        assert!(a > -dim && a < max + dim, "never past the band: {a}");
    }
    // Content shorter than the view (max 0) with a negative max still behaves.
    assert!(rubber_offset(0.0, -50.0, 10.0, dim) >= 0.0);
    assert_eq!(overscroll_amount(5.0, -3.0), 5.0, "negative max counts as 0");
    assert_eq!(overscroll_amount(-5.0, 100.0), -5.0);
    assert_eq!(
        overscroll_amount(100.0, 100.0),
        0.0,
        "the edge itself is in range"
    );
}

#[test]
fn coast_and_fling_distance_agree_and_bad_inputs_stop() {
    let (v, tau) = (900.0, 0.4);
    let (far, left) = coast(v, 1e6, tau);
    assert!((far - fling_distance(v, tau)).abs() < 1e-2);
    assert!(left.abs() < 1e-3);
    assert_eq!(coast(v, 0.0, tau), (0.0, v), "no time passes");
    assert_eq!(coast(v, -1.0, tau), (0.0, v), "negative time counts as none");
    assert_eq!(
        coast(v, 0.1, 0.0),
        (0.0, 0.0),
        "no friction time constant stops at once"
    );
    assert_eq!(coast(v, 0.1, -3.0), (0.0, 0.0));
    assert_eq!(fling_distance(v, -3.0), 0.0);
    // Direction is kept.
    let (d, nv) = coast(-v, 0.1, tau);
    assert!(d < 0.0 && nv < 0.0);
}

#[test]
fn spring_step_identity_and_settling() {
    assert_eq!(spring_step(12.0, 3.0, 0.0, 14.0), (12.0, 3.0));
    assert_eq!(
        spring_step(12.0, 3.0, -1.0, 14.0),
        (12.0, 3.0),
        "negative dt counts as 0"
    );
    let (mut x, mut v) = (80.0f32, 0.0f32);
    let mut previous = x;
    for _ in 0..120 {
        (x, v) = spring_step(x, v, 1.0 / 60.0, 14.0);
        assert!(
            x >= 0.0 && x <= previous,
            "critically damped: no overshoot ({x} after {previous})"
        );
        previous = x;
    }
    assert!(x < 0.01 && v.abs() < 0.1, "settled: x {x}, v {v}");
    // One big step equals many small ones.
    let big = spring_step(80.0, 0.0, 1.0, 14.0);
    let (mut sx, mut sv) = (80.0f32, 0.0f32);
    for _ in 0..100 {
        (sx, sv) = spring_step(sx, sv, 0.01, 14.0);
    }
    assert!(
        (big.0 - sx).abs() < 1e-3 && (big.1 - sv).abs() < 1e-2,
        "{big:?} vs ({sx}, {sv})"
    );
}

#[test]
fn drag_samples_survive_non_finite_deltas_and_reset_on_begin() {
    let mut s = DragSamples::default();
    assert!(s.is_empty());
    assert_eq!(s.velocity(1.0, 0.1), [0.0; 2], "no samples");
    s.begin(0.0);
    s.push(0.05, [f32::NAN, 10.0]);
    s.push(0.10, [f32::INFINITY, f32::NEG_INFINITY]);
    s.push(0.15, [30.0, 0.0]);
    let v = s.velocity(0.2, 0.2);
    assert!(
        v.iter().all(|c| c.is_finite() && c.abs() <= MAX_FLING_SPEED),
        "{v:?}"
    );
    assert!(v[0] > 0.0, "the finite part still counts");
    assert_eq!(s.velocity(0.2, 0.0), [0.0; 2], "an empty window has no velocity");
    assert_eq!(
        s.velocity(0.2, -1.0),
        [0.0; 2],
        "a negative window has none either"
    );
    s.begin(5.0);
    assert!(s.is_empty());
    assert_eq!(s.velocity(5.0, 0.1), [0.0; 2], "no time since begin");
    for i in 0..40 {
        s.push(5.0 + i as f32 * 0.01, [1.0, 0.0]);
    }
    assert_eq!(s.len(), 16, "a ring of 16");
}

// ---- nested scrolling ---------------------------------------------------------------------

fn link(offset: [f32; 2], max: [f32; 2], chaining: ScrollChaining) -> ChainLink {
    ChainLink {
        offset,
        max,
        viewport: [300.0, 300.0],
        axes: ScrollAxes::Both,
        overscroll: Overscroll::Clamp,
        chaining,
    }
}

#[test]
fn distribute_ignores_a_nan_delta_and_empty_chains() {
    assert!(distribute(&[], [10.0, 10.0], ScrollInput::Wheel).is_empty());
    let links = [link([5.0, 5.0], [100.0, 100.0], ScrollChaining::Chain)];
    for input in [ScrollInput::Wheel, ScrollInput::Drag] {
        let out = distribute(&links, [f32::NAN, f32::NAN], input);
        assert_eq!(out, [[5.0, 5.0]], "{input:?}");
        let out = distribute(&links, [f32::NAN, 10.0], input);
        assert_eq!(out, [[5.0, 15.0]], "{input:?}: the good axis still moves");
    }
    let out = distribute(&links, [f32::INFINITY, f32::NEG_INFINITY], ScrollInput::Wheel);
    assert_eq!(out, [[100.0, 0.0]], "an infinite wheel delta stops at the edges");
}

#[test]
fn wheel_chains_never_leave_the_range_and_never_move_more_than_asked() {
    let mut rng = FuzzRng::new(0xC4A1);
    for case in 0..400 {
        let n = 1 + rng.below(4);
        let links: Vec<ChainLink> = (0..n)
            .map(|_| {
                let max = [unit(&mut rng) * 500.0, unit(&mut rng) * 500.0];
                let mut l = link(
                    [unit(&mut rng) * max[0], unit(&mut rng) * max[1]],
                    max,
                    if rng.chance(3) {
                        ScrollChaining::Contain
                    } else {
                        ScrollChaining::Chain
                    },
                );
                l.axes = match rng.below(3) {
                    0 => ScrollAxes::Both,
                    1 => ScrollAxes::Vertical,
                    _ => ScrollAxes::Horizontal,
                };
                l
            })
            .collect();
        let delta = [unit(&mut rng) * 800.0 - 400.0, unit(&mut rng) * 800.0 - 400.0];
        let out = distribute(&links, delta, ScrollInput::Wheel);
        assert_eq!(out.len(), links.len());
        for axis in 0..2 {
            let mut moved = 0.0;
            for (l, o) in links.iter().zip(&out) {
                assert!(
                    o[axis] >= -1e-3 && o[axis] <= l.max[axis] + 1e-3,
                    "case {case}: {o:?} vs {:?}",
                    l.max
                );
                if !l.axes.allows(axis) {
                    assert_eq!(
                        o[axis], l.offset[axis],
                        "case {case}: a view that does not scroll the axis stays put"
                    );
                }
                let step = o[axis] - l.offset[axis];
                assert!(
                    step * delta[axis].signum() >= -1e-3,
                    "case {case}: moved against the delta"
                );
                moved += step.abs();
            }
            assert!(
                moved <= delta[axis].abs() + 1e-2,
                "case {case}: moved {moved} for {}",
                delta[axis]
            );
        }
        // Deterministic.
        assert_eq!(out, distribute(&links, delta, ScrollInput::Wheel));
    }
}

#[test]
fn contain_keeps_the_leftover_inside_and_the_inner_view_takes_first() {
    let inner = link([0.0, 90.0], [0.0, 100.0], ScrollChaining::Contain);
    let outer = link([0.0, 0.0], [0.0, 500.0], ScrollChaining::Chain);
    let out = distribute(&[inner, outer], [0.0, 50.0], ScrollInput::Wheel);
    assert_eq!(
        out,
        [[0.0, 100.0], [0.0, 0.0]],
        "contained: nothing passes outward"
    );
    let inner = link([0.0, 90.0], [0.0, 100.0], ScrollChaining::Chain);
    let out = distribute(&[inner, outer], [0.0, 50.0], ScrollInput::Wheel);
    assert_eq!(
        out,
        [[0.0, 100.0], [0.0, 40.0]],
        "chained: the leftover 40 passes out"
    );
}
