// SPDX-License-Identifier: MPL-2.0
//! The size-intent check for placed scatter (DO27's `scatter.json`).
//!
//! DO27's scatter defines `size_m` as the intent: the longest side of the source mesh × the
//! instance's uniform scale equals `size_m` before the per-axis jitter (`size_rule`). `scale`
//! stays the transform, and nothing here changes which field a loader reads. A mesh swap that
//! leaves stale scales (DO27's tunnel rocks came out ~4 m) shows up as a disagreement; a
//! loader calls [`check_size_intent`] per instance (or [`check_size_intents`] for a set) and
//! gets a `tracing` warning (target `regia_terrain::content`) naming the class and both
//! values.

/// Relative disagreement above which [`check_size_intent`] warns (20 %).
pub const SIZE_INTENT_TOLERANCE: f32 = 0.2;

/// One instance whose transform does not give its intended size.
#[derive(Clone, Debug, PartialEq)]
pub struct SizeIntentMismatch {
    /// Scatter class (DO27's `kind`, or the mesh id).
    pub class: String,
    /// Longest side of the source mesh × the uniform scale, metres.
    pub transformed_m: f32,
    /// The intended size (`size_m`), metres.
    pub intended_m: f32,
}

/// The uniform part of a jittered `scale` (glTF axes, Y up): DO27 jitters the two horizontal
/// axes around the uniform scale and flattens Y, so the horizontal geometric mean recovers
/// the uniform factor.
#[must_use]
pub fn uniform_scale(scale: [f32; 3]) -> f32 {
    (scale[0].abs() * scale[2].abs()).sqrt()
}

/// Compares `longest_side_m × uniform(scale)` with `size_m`. Returns (and logs through
/// `tracing::warn!`) the mismatch when they differ by more than [`SIZE_INTENT_TOLERANCE`] of
/// `size_m`; `None` when they agree or the inputs are not finite and positive.
#[must_use]
pub fn check_size_intent(
    class: &str,
    longest_side_m: f32,
    scale: [f32; 3],
    size_m: f32,
) -> Option<SizeIntentMismatch> {
    let transformed = longest_side_m * uniform_scale(scale);
    if !(transformed.is_finite() && size_m.is_finite() && size_m > 0.0 && longest_side_m > 0.0) {
        return None;
    }
    if ((transformed - size_m) / size_m).abs() <= SIZE_INTENT_TOLERANCE {
        return None;
    }
    tracing::warn!(
        target: "regia_terrain::content",
        class,
        transformed_m = transformed,
        intended_m = size_m,
        "scatter class {class}: longest side × uniform scale is {transformed:.2} m but size_m is {size_m:.2} m \
         (stale scale after a mesh swap?)"
    );
    Some(SizeIntentMismatch {
        class: class.to_owned(),
        transformed_m: transformed,
        intended_m: size_m,
    })
}

/// [`check_size_intent`] over many instances `(class, longest_side_m, scale, size_m)`; one
/// warning per class (its first mismatch) so a whole stale class does not flood the log. The
/// returned list holds every mismatching instance.
pub fn check_size_intents<'a>(
    instances: impl IntoIterator<Item = (&'a str, f32, [f32; 3], f32)>,
) -> Vec<SizeIntentMismatch> {
    let mut warned = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (class, longest, scale, size) in instances {
        let transformed = longest * uniform_scale(scale);
        let off = size.is_finite()
            && size > 0.0
            && longest > 0.0
            && transformed.is_finite()
            && ((transformed - size) / size).abs() > SIZE_INTENT_TOLERANCE;
        if !off {
            continue;
        }
        if warned.insert(class.to_owned())
            && let Some(m) = check_size_intent(class, longest, scale, size)
        {
            out.push(m);
            continue;
        }
        out.push(SizeIntentMismatch {
            class: class.to_owned(),
            transformed_m: transformed,
            intended_m: size,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_that_matches_its_size_passes_and_a_stale_scale_is_named() {
        // DO27's first bore_twelve chunk: a 0.23 m mesh at horizontal scale ~0.92 → 0.21 m.
        assert_eq!(
            check_size_intent("chunk", 0.23, [0.88262, 0.8141, 0.96566], 0.206),
            None
        );
        // Jitter inside 20 % passes.
        assert_eq!(check_size_intent("chunk", 1.0, [1.15, 0.7, 1.1], 1.0), None);
        // A tunnel rock meant to be 0.8 m whose scale was left from a 4× smaller mesh.
        let m = check_size_intent("tunnel_rock", 1.0, [4.0, 3.6, 4.1], 0.8).expect("mismatch");
        assert_eq!(m.class, "tunnel_rock");
        assert!((m.transformed_m - (4.0f32 * 4.1).sqrt()).abs() < 1e-4);
        assert!((m.intended_m - 0.8).abs() < 1e-6);
        // Bad inputs are not judged.
        assert_eq!(check_size_intent("x", 0.0, [1.0; 3], 1.0), None);
        assert_eq!(check_size_intent("x", 1.0, [1.0; 3], f32::NAN), None);
    }

    #[test]
    fn a_set_reports_every_mismatch_and_warns_once_per_class() {
        let found = check_size_intents([
            ("chunk", 0.23, [0.9, 0.8, 0.95], 0.206),
            ("tunnel_rock", 1.0, [4.0, 4.0, 4.0], 0.8),
            ("tunnel_rock", 1.0, [3.9, 4.0, 4.1], 0.9),
        ]);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|m| m.class == "tunnel_rock"));
    }
}
