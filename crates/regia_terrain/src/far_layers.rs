// SPDX-License-Identifier: MPL-2.0
//! Far-field layer weight selection (ADR-1446). Past the terrain budget's `far_layers_m` the shader keeps only
//! the two strongest layers of a pixel and renormalises them. This is the CPU mirror of that step
//! (`terrain.wgsl`, "The two strongest band weights"), so the math can be tested without a GPU.
//!
//! The pick is continuous in the weights: the third strongest weight is subtracted from every layer first
//! (`max(w − w₃, 0)` keeps at most two), so a layer overtaken by the third fades to 0 instead of vanishing in
//! one step. A hard "two largest" pick (`hard_top_two`, the shader's earlier behaviour) jumps wherever two
//! weights cross, and the splat packs' texel-level noise turned those jumps into a stair-stepped outline around
//! every painted patch from far away.

/// The two strongest of `weights`, continuous in the weights and renormalised to sum to 1: the third strongest
/// weight is subtracted from all (negative results clamp to 0), so at most two entries are non-zero. All
/// weights equal down to the third leaves nothing; then the strongest layer takes everything. Empty input
/// returns an empty vector; all-zero input returns all zeros.
pub fn soft_top_two(weights: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0; weights.len()];
    let Some((i1, w1)) = strongest(weights, None) else {
        return out;
    };
    if w1 <= 0.0 {
        return out;
    }
    let second = strongest(weights, Some(i1));
    let third = second.and_then(|(i2, _)| strongest_excluding(weights, i1, i2));
    let floor = third.map_or(0.0, |(_, w)| w.max(0.0));
    let r1 = (w1 - floor).max(0.0);
    let r2 = second.map_or(0.0, |(_, w)| (w - floor).max(0.0));
    let sum = r1 + r2;
    if sum > 1e-5 {
        out[i1] = r1 / sum;
        if let Some((i2, _)) = second {
            out[i2] = r2 / sum;
        }
    } else {
        out[i1] = 1.0;
    }
    out
}

/// The two strongest of `weights` by a hard pick, renormalised: the shader's behaviour before ADR-1446, kept
/// as the reference the tests compare against.
pub fn hard_top_two(weights: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0; weights.len()];
    let Some((i1, w1)) = strongest(weights, None) else {
        return out;
    };
    let second = strongest(weights, Some(i1)).filter(|&(_, w)| w > 0.0);
    let sum = (w1 + second.map_or(0.0, |(_, w)| w)).max(1e-5);
    out[i1] = w1 / sum;
    if let Some((i2, w2)) = second {
        out[i2] = w2 / sum;
    }
    out
}

/// Index and value of the largest weight, skipping `skip`; the first wins a tie.
fn strongest(weights: &[f32], skip: Option<usize>) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (i, &w) in weights.iter().enumerate() {
        if Some(i) == skip {
            continue;
        }
        if best.is_none_or(|(_, b)| w > b) {
            best = Some((i, w));
        }
    }
    best
}

/// Index and value of the largest weight outside `a` and `b`.
fn strongest_excluding(weights: &[f32], a: usize, b: usize) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (i, &w) in weights.iter().enumerate() {
        if i == a || i == b {
            continue;
        }
        if best.is_none_or(|(_, v)| w > v) {
            best = Some((i, w));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(v: &[f32]) -> f32 {
        v.iter().sum()
    }

    #[test]
    fn at_most_two_layers_survive_and_they_sum_to_one() {
        let out = soft_top_two(&[0.5, 0.2, 0.15, 0.1, 0.05, 0.0, 0.0, 0.0]);
        assert_eq!(out.iter().filter(|&&w| w > 0.0).count(), 2, "{out:?}");
        assert!((sum(&out) - 1.0).abs() < 1e-6, "{out:?}");
        assert!(out[0] > out[1], "the strongest stays strongest: {out:?}");
    }

    #[test]
    fn a_dominant_layer_stays_alone() {
        let out = soft_top_two(&[1.0, 0.0, 0.0]);
        assert_eq!(out, vec![1.0, 0.0, 0.0]);
        let one = soft_top_two(&[0.7]);
        assert_eq!(one, vec![1.0]);
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(soft_top_two(&[]).is_empty());
        assert_eq!(soft_top_two(&[0.0, 0.0]), vec![0.0, 0.0]);
    }

    #[test]
    fn equal_weights_leave_the_strongest_layer_alone() {
        let out = soft_top_two(&[0.25, 0.25, 0.25, 0.25]);
        assert_eq!(out, vec![1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn two_layers_only_keep_their_ratio() {
        let out = soft_top_two(&[0.6, 0.4]);
        assert!(
            (out[0] - 0.6).abs() < 1e-6 && (out[1] - 0.4).abs() < 1e-6,
            "{out:?}"
        );
    }

    #[test]
    fn crossing_of_the_second_and_third_weights_is_continuous() {
        // Layer 1 sweeps from just under to just over layer 2 (both 0.2 at the crossing). The soft pick moves
        // by a small amount per step; the hard pick drops layer 2 and takes layer 1 in a single step.
        let mut worst_soft = 0.0_f32;
        let mut worst_hard = 0.0_f32;
        let mut prev_soft: Option<Vec<f32>> = None;
        let mut prev_hard: Option<Vec<f32>> = None;
        for step in 0..=100 {
            let x = 0.15 + 0.1 * step as f32 / 100.0;
            let w = [0.6, x, 0.2, 0.05];
            let soft = soft_top_two(&w);
            let hard = hard_top_two(&w);
            if let (Some(p), Some(h)) = (&prev_soft, &prev_hard) {
                worst_soft = worst_soft.max(max_delta(p, &soft));
                worst_hard = worst_hard.max(max_delta(h, &hard));
            }
            prev_soft = Some(soft);
            prev_hard = Some(hard);
        }
        assert!(worst_soft < 0.08, "soft pick moved {worst_soft} in one step");
        assert!(
            worst_hard > 0.15,
            "the hard pick is expected to jump here, moved {worst_hard}"
        );
    }

    fn max_delta(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
    }
}
