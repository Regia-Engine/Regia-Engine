// SPDX-License-Identifier: MPL-2.0
//! CPU contract histories for HLOD cross-fades across repaired cuts and world reuse.

use std::collections::{BTreeMap, BTreeSet};

use regia_scene::stream_grid::CellCoord;
use regia_world::hlod_fade::{Drawable, FadeState};

fn cut(drawables: &[Drawable]) -> BTreeSet<Drawable> {
    drawables.iter().copied().collect()
}

fn cell(x: i64) -> Drawable {
    Drawable::Full(CellCoord { x, z: 0 })
}

fn proxy(id: u32) -> Drawable {
    Drawable::Proxy(id)
}

fn assert_signed_weights(fades: &BTreeMap<Drawable, f32>) {
    for (drawable, weight) in fades {
        assert!(weight.is_finite(), "{drawable:?} has non-finite weight {weight}");
        assert!(
            *weight > -1.0 && *weight < 1.0,
            "{drawable:?} has out-of-range weight {weight}"
        );
        assert_ne!(*weight, 0.0, "settled drawables must be retired");
    }
}

#[test]
fn parent_proxy_to_multiple_leaves_keeps_every_outgoing_draw_until_retired() {
    let parent = proxy(10);
    let leaves = cut(&[cell(1), cell(2), cell(3)]);
    let mut state = FadeState::default();
    assert!(state.step(&cut(&[parent]), 3).is_empty());

    for expected in [1.0 / 3.0, 2.0 / 3.0] {
        let fades = state.step(&leaves, 3);
        assert_signed_weights(&fades);
        assert_eq!(fades.len(), 4, "three new leaves and the outgoing parent draw");
        for leaf in &leaves {
            assert!((fades[leaf] - expected).abs() < 0.00001);
        }
        assert!((fades[&parent] + expected).abs() < 0.00001);
        // These leaves are different drawable regions. Their weights are checked individually;
        // they are not summed as if they replaced one drawable.
    }
    let retired = state.step(&leaves, 3);
    assert!(retired.is_empty(), "all four fades retire at their fixed length");

    // Two settled leaves collapse to a sibling proxy while the third leaf stays in the cut.
    let sibling_cut = cut(&[proxy(11), cell(3)]);
    for expected in [1.0 / 3.0, 2.0 / 3.0] {
        let fades = state.step(&sibling_cut, 3);
        assert_signed_weights(&fades);
        assert_eq!(
            fades.len(),
            3,
            "two outgoing leaves and the incoming sibling proxy"
        );
        assert!((fades[&cell(1)] + expected).abs() < 0.00001);
        assert!((fades[&cell(2)] + expected).abs() < 0.00001);
        assert!((fades[&proxy(11)] - expected).abs() < 0.00001);
        assert!(
            !fades.contains_key(&cell(3)),
            "the unchanged leaf does not restart"
        );
    }
    assert!(
        state.step(&sibling_cut, 3).is_empty(),
        "outgoing leaves retire after the sibling-proxy handoff"
    );
}

#[test]
fn rapid_reversals_preserve_each_drawables_signed_share() {
    let full = cell(4);
    let proxy = proxy(4);
    let mut state = FadeState::default();
    state.step(&cut(&[full]), 4);
    assert_eq!(
        state.step(&cut(&[proxy]), 4),
        [(full, -0.25), (proxy, 0.25)].into()
    );
    assert_eq!(
        state.step(&cut(&[full]), 4),
        [(full, 0.75), (proxy, -0.75)].into()
    );
    assert_eq!(state.step(&cut(&[proxy]), 4), [(full, -0.5), (proxy, 0.5)].into());
    assert_eq!(
        state.step(&cut(&[proxy]), 4),
        [(full, -0.75), (proxy, 0.75)].into()
    );
    let settled = state.step(&cut(&[proxy]), 4);
    assert!(
        settled.is_empty(),
        "the final outgoing full drawable retires on time"
    );
}

#[test]
fn starting_a_sibling_branch_does_not_restart_an_existing_fade() {
    let full_a = cell(5);
    let full_b = cell(6);
    let proxy_a = proxy(5);
    let proxy_b = proxy(6);
    let mut state = FadeState::default();
    state.step(&cut(&[full_a, full_b]), 4);
    state.step(&cut(&[proxy_a, full_b]), 4);

    let next_branch = state.step(&cut(&[proxy_a, proxy_b]), 4);
    assert_signed_weights(&next_branch);
    assert_eq!(next_branch[&proxy_a], 0.5, "branch A advances");
    assert_eq!(next_branch[&full_a], -0.5, "branch A outgoing draw advances");
    assert_eq!(next_branch[&proxy_b], 0.25, "branch B starts independently");
    assert_eq!(
        next_branch[&full_b], -0.25,
        "branch B outgoing draw starts independently"
    );
}

#[test]
fn an_empty_repaired_cut_keeps_only_outgoing_draws_until_they_retire() {
    let old = cut(&[proxy(20), cell(20)]);
    let empty = BTreeSet::new();
    let mut state = FadeState::default();
    state.step(&old, 2);

    let first = state.step(&empty, 2);
    assert_eq!(first, [(proxy(20), -0.5), (cell(20), -0.5)].into());
    assert_signed_weights(&first);
    assert!(
        state.step(&empty, 2).is_empty(),
        "both outgoing drawables retire on frame two"
    );
}

#[test]
fn clear_during_fade_makes_reused_proxy_ids_a_fresh_first_frame() {
    let reused = proxy(7);
    let mut state = FadeState::default();
    state.step(&cut(&[reused]), 3);
    let leaving = state.step(&BTreeSet::new(), 3);
    assert_eq!(leaving, [(reused, -1.0 / 3.0)].into());

    state.clear();
    assert!(
        state.step(&cut(&[reused]), 3).is_empty(),
        "the new world's first frame is fresh"
    );
    assert!(
        state.step(&cut(&[reused]), 3).is_empty(),
        "reused identity does not inherit old state"
    );
}

#[test]
fn fixed_lengths_zero_one_two_and_four_have_finite_bounded_outputs() {
    let old = proxy(30);
    let new = cell(30);
    for frames in [0, 1, 2, 4] {
        let mut state = FadeState::default();
        assert!(state.step(&cut(&[old]), frames).is_empty());
        let mut outputs = Vec::new();
        for _ in 0..=frames {
            let fades = state.step(&cut(&[new]), frames);
            assert_signed_weights(&fades);
            outputs.push(fades);
        }
        if frames <= 1 {
            // One frame has no representable weight strictly inside (0, 1), so it is instant too.
            assert!(outputs.iter().all(BTreeMap::is_empty));
        } else {
            assert!(outputs[0].contains_key(&old));
            assert!(outputs[0].contains_key(&new));
            assert!(outputs[usize::try_from(frames).expect("small fixed test length")].is_empty());
        }
    }
}
