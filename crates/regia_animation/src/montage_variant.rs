// SPDX-License-Identifier: MPL-2.0
//! Variant montages (`.montagevar.ron`, frozen v1).
//!
//! Specification: `docs/specs/34-montage-extensions.md` § 6 (M4). A [`MontageVariant`] derives a
//! montage from a parent by swapping selected clip segments (the same combo with different
//! weapon clips) without copying timing. Everything else (sections, notifies, warps, blends,
//! flags) is inherited and read-only: the file cannot name other fields. A variant's parent may
//! not itself be a variant.
//!
//! [`MontageLibrary`] holds montages and the variants resolved from them; replacing a parent
//! re-resolves its variants, so a development rebuild of the parent shows up on the next play.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::montage::{Montage, PastEndItem};

/// The `.montagevar.ron` format version (v1 is the first version).
pub const MONTAGE_VARIANT_FORMAT_VERSION: u32 = 1;

/// One replaced segment. An override names only these fields (spec 34 § 6, ADR-5633): any other
/// field is a load error, not silently ignored.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SegmentOverride {
    /// Track index in the parent.
    pub track: usize,
    /// Segment index on that track.
    pub segment: usize,
    /// The replacement clip.
    pub clip: String,
    /// Set the segment's rate so the replacement fills the original segment's duration.
    #[serde(default)]
    pub rescale: bool,
}

/// The `.montagevar.ron` document. Everything but the overrides is inherited from the parent and
/// read-only, so the file cannot name other fields (spec 34 § 6, ADR-5633).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MontageVariant {
    /// Format version of this file.
    #[serde(default)]
    pub format_version: u32,
    /// The resolved montage's name (the library key).
    pub name: String,
    /// The parent: a montage name, or the path of its `.montage.ron` (the file stem is used).
    pub parent: String,
    /// Replaced segments.
    pub overrides: Vec<SegmentOverride>,
}

/// Errors reading or resolving a variant.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum VariantError {
    /// RON text could not be read or written.
    #[error("montage variant RON: {0}")]
    Ron(String),
    /// The file is newer than this engine.
    #[error("montage variant format version {0} is unknown (current is {MONTAGE_VARIANT_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The document is malformed.
    #[error("invalid montage variant: {0}")]
    Invalid(String),
    /// The parent is itself a variant (derivation stops after one generation).
    #[error("variant {variant:?}: parent {parent:?} is itself a variant")]
    ParentIsVariant {
        /// The variant being resolved.
        variant: String,
        /// Its parent.
        parent: String,
    },
    /// The parent is not in the library.
    #[error("variant {variant:?}: parent {parent:?} is not loaded")]
    ParentMissing {
        /// The variant being resolved.
        variant: String,
        /// Its parent.
        parent: String,
    },
    /// An override names a track or segment the parent lacks.
    #[error("variant {variant:?}: track {track} segment {segment} does not exist in the parent")]
    OutOfRange {
        /// The variant being resolved.
        variant: String,
        /// The track index.
        track: usize,
        /// The segment index.
        segment: usize,
    },
    /// A rescale needs a clip whose duration is not known.
    #[error("variant {variant:?}: clip {clip:?} has no known duration")]
    UnknownClip {
        /// The variant being resolved.
        variant: String,
        /// The clip.
        clip: String,
    },
    /// A rescale found a zero or non-finite duration.
    #[error("variant {variant:?}: track {track} segment {segment} has no usable duration to rescale")]
    ZeroDuration {
        /// The variant being resolved.
        variant: String,
        /// The track index.
        track: usize,
        /// The segment index.
        segment: usize,
    },
}

/// `from` versions of the registered migrations (none: v1 is the first version).
#[must_use]
pub fn montage_variant_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of the format (for `cargo xtask check-formats`).
#[must_use]
pub fn montage_variant_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<MontageVariant>(),
        serde_field_names::<SegmentOverride>(),
    ])
}

/// The montage name a `parent` field refers to: the file stem of a path (`a/b/combo.montage.ron`
/// is `combo`), else the text itself.
#[must_use]
pub fn parent_name(parent: &str) -> &str {
    let file = parent.rsplit(['/', '\\']).next().unwrap_or(parent);
    file.strip_suffix(".montage.ron").unwrap_or(file)
}

impl MontageVariant {
    /// Parses `.montagevar.ron` text: peeks the version (newer is rejected), validates.
    ///
    /// # Errors
    ///
    /// Syntax errors, unknown versions and failed [`MontageVariant::validate`].
    pub fn from_ron_str(text: &str) -> Result<Self, VariantError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| VariantError::Invalid(format!("cannot read format_version: {e}")))?;
        if version > MONTAGE_VARIANT_FORMAT_VERSION {
            return Err(VariantError::UnknownVersion(version));
        }
        let mut variant: MontageVariant =
            ron::from_str(text).map_err(|e| VariantError::Ron(e.to_string()))?;
        variant.format_version = MONTAGE_VARIANT_FORMAT_VERSION;
        variant.validate()?;
        Ok(variant)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, VariantError> {
        let mut variant = self.clone();
        variant.format_version = MONTAGE_VARIANT_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        ron::ser::to_string_pretty(&variant, pretty).map_err(|e| VariantError::Ron(e.to_string()))
    }

    /// Checks names and that two overrides do not replace the same segment.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), VariantError> {
        if self.name.is_empty() {
            return Err(VariantError::Invalid("the variant has no name".into()));
        }
        if self.parent.is_empty() {
            return Err(VariantError::Invalid("the variant has no parent".into()));
        }
        for (i, o) in self.overrides.iter().enumerate() {
            if o.clip.is_empty() {
                return Err(VariantError::Invalid(format!("override {i} has no clip")));
            }
            if self.overrides[..i]
                .iter()
                .any(|p| (p.track, p.segment) == (o.track, o.segment))
            {
                return Err(VariantError::Invalid(format!(
                    "two overrides replace track {} segment {}",
                    o.track, o.segment
                )));
            }
        }
        Ok(())
    }

    /// The parent with every override applied, named after this variant.
    ///
    /// `clip_span(clip)` gives a clip's duration in seconds; it is needed only for overrides
    /// with `rescale` (and for old segments that play to their clip's end).
    ///
    /// # Errors
    ///
    /// [`VariantError::OutOfRange`] for a missing track or segment,
    /// [`VariantError::UnknownClip`] / [`VariantError::ZeroDuration`] when a rescale cannot be
    /// computed.
    pub fn resolve(
        &self,
        parent: &Montage,
        clip_span: &dyn Fn(&str) -> Option<f32>,
    ) -> Result<Montage, VariantError> {
        let mut out = parent.clone();
        out.name.clone_from(&self.name);
        for o in &self.overrides {
            let out_of_range = || VariantError::OutOfRange {
                variant: self.name.clone(),
                track: o.track,
                segment: o.segment,
            };
            let segment = out
                .tracks
                .get_mut(o.track)
                .and_then(|t| t.segments.get_mut(o.segment))
                .ok_or_else(out_of_range)?;
            if o.rescale {
                let unknown = |clip: &str| VariantError::UnknownClip {
                    variant: self.name.clone(),
                    clip: clip.to_owned(),
                };
                let old_end = match segment.end {
                    Some(end) => end,
                    None => clip_span(&segment.clip).ok_or_else(|| unknown(&segment.clip))?,
                };
                let new_end = match segment.end {
                    Some(end) => end,
                    None => clip_span(&o.clip).ok_or_else(|| unknown(&o.clip))?,
                };
                let loops = segment.loops as f32;
                let old_duration = (old_end - segment.start) / segment.rate * loops;
                let new_span = (new_end - segment.start) * loops;
                let rate = new_span / old_duration;
                if !(old_duration.is_finite() && old_duration > 0.0 && rate.is_finite() && rate > 0.0) {
                    return Err(VariantError::ZeroDuration {
                        variant: self.name.clone(),
                        track: o.track,
                        segment: o.segment,
                    });
                }
                segment.rate = rate;
            }
            segment.clip.clone_from(&o.clip);
        }
        Ok(out)
    }
}

/// Montages by name plus the variants resolved from them.
#[derive(Clone, Debug, Default)]
pub struct MontageLibrary {
    montages: Vec<Arc<Montage>>,
    variants: Vec<MontageVariant>,
    /// Validation warnings of resolved variants by variant name (spec 34 M4).
    past_end: Vec<(String, Vec<PastEndItem>)>,
}

impl MontageLibrary {
    /// An empty library.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace by name) a plain montage. Variants of a replaced montage re-resolve with
    /// `clip_span`; a variant that no longer resolves is dropped from the library and returned
    /// in the error list.
    pub fn add_montage(
        &mut self,
        montage: Montage,
        clip_span: &dyn Fn(&str) -> Option<f32>,
    ) -> Vec<VariantError> {
        let name = montage.name.clone();
        self.montages.retain(|m| m.name != name);
        self.montages.push(Arc::new(montage));
        let mut errors = Vec::new();
        let dependents: Vec<MontageVariant> = self
            .variants
            .iter()
            .filter(|v| parent_name(&v.parent) == name)
            .cloned()
            .collect();
        for variant in dependents {
            if let Err(error) = self.add_variant(variant.clone(), clip_span) {
                self.montages.retain(|m| m.name != variant.name);
                self.variants.retain(|v| v.name != variant.name);
                self.past_end.retain(|(name, _)| *name != variant.name);
                errors.push(error);
            }
        }
        errors
    }

    /// Resolve `variant` against its parent and add it (replacing a montage of the same name).
    ///
    /// # Errors
    ///
    /// [`VariantError::ParentMissing`], [`VariantError::ParentIsVariant`] and the errors of
    /// [`MontageVariant::resolve`]. The library is unchanged on error.
    pub fn add_variant(
        &mut self,
        variant: MontageVariant,
        clip_span: &dyn Fn(&str) -> Option<f32>,
    ) -> Result<Arc<Montage>, VariantError> {
        let parent = parent_name(&variant.parent).to_owned();
        if self.variants.iter().any(|v| v.name == parent) {
            return Err(VariantError::ParentIsVariant {
                variant: variant.name,
                parent,
            });
        }
        let Some(base) = self.montages.iter().find(|m| m.name == parent).cloned() else {
            return Err(VariantError::ParentMissing {
                variant: variant.name,
                parent,
            });
        };
        let resolved = Arc::new(variant.resolve(&base, clip_span)?);
        // A `rescale: false` override can shorten the timeline under inherited items: they
        // keep their times, and validation reports them (spec 34 M4). A warning: the variant
        // still loads, and playback ends at the new end.
        let past = resolved
            .timeline_length(clip_span)
            .map(|end| resolved.items_past_end(end))
            .unwrap_or_default();
        for item in &past {
            tracing::warn!(
                target: "regia_animation",
                code = item.code,
                "variant {:?}: {} {} {:?} at {} s lies past the timeline end {} s",
                variant.name,
                item.kind,
                item.index,
                item.name,
                item.time,
                item.end
            );
        }
        self.past_end.retain(|(name, _)| *name != variant.name);
        if !past.is_empty() {
            self.past_end.push((variant.name.clone(), past));
        }
        self.montages.retain(|m| m.name != variant.name);
        self.montages.push(resolved.clone());
        self.variants.retain(|v| v.name != variant.name);
        self.variants.push(variant);
        Ok(resolved)
    }

    /// The items of variant `name` that lie past its timeline end (spec 34 M4, code
    /// `montage_past_end`): empty for a plain montage, a variant whose items all fit, or a
    /// variant whose clip durations were unknown when it resolved.
    #[must_use]
    pub fn past_end(&self, name: &str) -> &[PastEndItem] {
        self.past_end
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&[], |(_, items)| items.as_slice())
    }

    /// A montage or resolved variant by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<Montage>> {
        self.montages.iter().find(|m| m.name == name)
    }

    /// The parent montage name when `name` is a variant (what agents list as `variant_of`).
    #[must_use]
    pub fn variant_of(&self, name: &str) -> Option<&str> {
        self.variants
            .iter()
            .find(|v| v.name == name)
            .map(|v| parent_name(&v.parent))
    }

    /// Every montage and variant name, in insertion order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.montages.iter().map(|m| m.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::montage::{MontageSection, MontageSegment, MontageTrack};

    fn span(clip: &str) -> Option<f32> {
        match clip {
            "swing_sword" => Some(1.0),
            "swing_axe" => Some(1.5),
            "idle" => Some(2.0),
            _ => None,
        }
    }

    fn parent() -> Montage {
        let mut m = Montage::single("combo", "full", "swing_sword");
        m.tracks[0].segments.push(MontageSegment {
            clip: "idle".into(),
            start: 0.0,
            end: Some(1.0),
            rate: 2.0,
            loops: 2,
        });
        m.sections = vec![MontageSection {
            name: "all".into(),
            start: 0.0,
            next: crate::montage::SectionNext::Stop,
            follow: None,
        }];
        m
    }

    fn variant(rescale: bool) -> MontageVariant {
        MontageVariant {
            format_version: MONTAGE_VARIANT_FORMAT_VERSION,
            name: "combo_axe".into(),
            parent: "assets/anim/combo.montage.ron".into(),
            overrides: vec![SegmentOverride {
                track: 0,
                segment: 0,
                clip: "swing_axe".into(),
                rescale,
            }],
        }
    }

    fn duration(seg: &MontageSegment, clip_end: f32) -> f32 {
        (seg.end.unwrap_or(clip_end) - seg.start) / seg.rate * seg.loops as f32
    }

    #[test]
    fn the_file_round_trips_and_defaults_rescale_off() {
        let text = variant(true).to_ron_string().expect("write");
        assert_eq!(MontageVariant::from_ron_str(&text).expect("read"), variant(true));
        let short =
            r#"(format_version: 1, name: "v", parent: "p", overrides: [(track: 0, segment: 0, clip: "c")])"#;
        assert!(!MontageVariant::from_ron_str(short).expect("short").overrides[0].rescale);
        assert!(matches!(
            MontageVariant::from_ron_str("(format_version: 7, name: \"v\", parent: \"p\", overrides: [])"),
            Err(VariantError::UnknownVersion(7))
        ));
    }

    #[test]
    fn resolve_changes_only_the_overridden_segment() {
        let p = parent();
        let r = variant(false).resolve(&p, &span).expect("resolve");
        assert_eq!(r.name, "combo_axe");
        assert_eq!(r.tracks[0].segments[0].clip, "swing_axe");
        assert_eq!(r.tracks[0].segments[0].rate, p.tracks[0].segments[0].rate);
        assert_eq!(r.tracks[0].segments[1], p.tracks[0].segments[1]);
        assert_eq!(r.sections, p.sections);
        assert_eq!(r.notifies, p.notifies);
    }

    #[test]
    fn rescale_fills_the_original_duration() {
        let p = parent();
        let r = variant(true).resolve(&p, &span).expect("resolve");
        let before = duration(&p.tracks[0].segments[0], 1.0);
        let after = duration(&r.tracks[0].segments[0], 1.5);
        assert!((before - after).abs() < 1e-5, "{before} vs {after}");
    }

    #[test]
    fn bad_indices_and_unknown_clips_are_named() {
        let p = parent();
        let mut v = variant(false);
        v.overrides[0].segment = 9;
        assert!(matches!(
            v.resolve(&p, &span),
            Err(VariantError::OutOfRange {
                track: 0,
                segment: 9,
                ..
            })
        ));
        let mut v = variant(true);
        v.overrides[0].clip = "nope".into();
        assert!(matches!(
            v.resolve(&p, &span),
            Err(VariantError::UnknownClip { .. })
        ));
        let mut zero = parent();
        zero.tracks[0].segments[0].end = Some(0.0);
        assert!(matches!(
            variant(true).resolve(&zero, &span),
            Err(VariantError::ZeroDuration { .. })
        ));
    }

    #[test]
    fn a_variant_of_a_variant_fails() {
        let mut lib = MontageLibrary::new();
        assert!(lib.add_montage(parent(), &span).is_empty());
        lib.add_variant(variant(false), &span).expect("first generation");
        let mut second = variant(false);
        second.name = "combo_axe_2".into();
        second.parent = "combo_axe".into();
        assert!(matches!(
            lib.add_variant(second, &span),
            Err(VariantError::ParentIsVariant { .. })
        ));
        assert_eq!(lib.variant_of("combo_axe"), Some("combo"));
        assert_eq!(lib.variant_of("combo"), None);
        assert!(lib.get("combo_axe_2").is_none());
    }

    #[test]
    fn a_missing_parent_is_reported() {
        let mut lib = MontageLibrary::new();
        assert!(matches!(
            lib.add_variant(variant(false), &span),
            Err(VariantError::ParentMissing { .. })
        ));
    }

    #[test]
    fn replacing_the_parent_re_resolves_its_variants() {
        let mut lib = MontageLibrary::new();
        lib.add_montage(parent(), &span);
        lib.add_variant(variant(true), &span).expect("variant");
        let mut edited = parent();
        edited.tracks[0].segments[0].rate = 2.0;
        assert!(lib.add_montage(edited, &span).is_empty());
        let r = lib.get("combo_axe").expect("variant stays");
        assert_eq!(r.tracks[0].segments[0].clip, "swing_axe");
        // The parent's segment now lasts 0.5 s, so the 1.5 s axe clip is sped up to match.
        assert!((duration(&r.tracks[0].segments[0], 1.5) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn a_parent_edit_that_breaks_a_variant_drops_it() {
        let mut lib = MontageLibrary::new();
        lib.add_montage(parent(), &span);
        lib.add_variant(variant(false), &span).expect("variant");
        let mut edited = parent();
        edited.tracks[0].segments.clear();
        edited.tracks.push(MontageTrack {
            slot: "other".into(),
            segments: Vec::new(),
        });
        let errors = lib.add_montage(edited, &span);
        assert_eq!(errors.len(), 1);
        assert!(lib.get("combo_axe").is_none());
    }

    #[test]
    fn parent_names_come_from_paths() {
        assert_eq!(parent_name("assets/anim/combo.montage.ron"), "combo");
        assert_eq!(parent_name("combo"), "combo");
        assert_eq!(parent_name("a\\b\\x.montage.ron"), "x");
    }
}
