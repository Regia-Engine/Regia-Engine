// SPDX-License-Identifier: MPL-2.0
//! Tags, spans and search filters for motion-matching databases
//! (`docs/specs/52-motion-matching.md` § 2.5).
//!
//! A database has up to 32 named tags, each one bit of a `u32` mask. A clip entry carries a tag
//! mask and a list of [`Span`]s over clip time; baking them gives every database frame three
//! values: its tag mask, a landing bias added to its cost when it is a jump target, and flags
//! (`pass_through`, `unindexed`). A [`SearchFilter`] then restricts a search to frames that
//! have all of the `require` bits and none of the `forbid` bits.

use serde::{Deserialize, Serialize};

/// Most tags a table holds (one bit each of a `u32`).
pub const MAX_TAGS: usize = 32;

/// Landing biases are clamped to this magnitude.
pub const MAX_BIAS: f32 = 100.0;

/// Frame flag: the frame may be played through but is never chosen as a jump target.
pub const FLAG_PASS_THROUGH: u8 = 1;
/// Frame flag: the frame is not indexed at all (never a target), though neighbors still use it.
pub const FLAG_UNINDEXED: u8 = 2;
/// Frame flag: the frame belongs to a mirrored range (it may still be a target).
pub const FLAG_MIRRORED: u8 = 4;

/// Why tags or spans were refused.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum TagError {
    /// More than [`MAX_TAGS`] tags.
    #[error("a tag table holds at most {MAX_TAGS} tags, got {0}")]
    TooManyTags(usize),
    /// A tag name was empty or used twice.
    #[error("tag name {0:?} is empty or repeated")]
    BadTagName(String),
    /// A filter or mask named a tag the table lacks.
    #[error("no tag named {0:?}")]
    UnknownTag(String),
    /// A span's range is empty, reversed or not finite.
    #[error("span {index} needs finite 0 <= start < end, got {start}..{end}")]
    BadSpan {
        /// Index in the clip's span list.
        index: usize,
        /// The rejected start.
        start: f32,
        /// The rejected end.
        end: f32,
    },
    /// A landing bias was not finite or outside `[-100, 100]`.
    #[error("span {index} landing bias {value} must be finite and within +-{MAX_BIAS}")]
    BadBias {
        /// Index in the clip's span list.
        index: usize,
        /// The rejected bias.
        value: f32,
    },
    /// The annotation list does not have one entry per clip.
    #[error("got annotations for {got} clips, the database has {expected}")]
    ClipCount {
        /// Annotations given.
        got: usize,
        /// Clips in the database.
        expected: usize,
    },
}

/// The named tags of a database.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagTable {
    names: Vec<String>,
}

impl TagTable {
    /// A table of `names`; bit `i` of a mask is the `i`-th name.
    ///
    /// # Errors
    ///
    /// [`TagError::TooManyTags`] or [`TagError::BadTagName`].
    pub fn new(names: Vec<String>) -> Result<Self, TagError> {
        if names.len() > MAX_TAGS {
            return Err(TagError::TooManyTags(names.len()));
        }
        for (i, name) in names.iter().enumerate() {
            if name.is_empty() || names[..i].contains(name) {
                return Err(TagError::BadTagName(name.clone()));
            }
        }
        Ok(TagTable { names })
    }

    /// The tag names, in bit order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The mask with the bits of `names` set.
    ///
    /// # Errors
    ///
    /// [`TagError::UnknownTag`] for the first name the table lacks.
    pub fn mask(&self, names: &[&str]) -> Result<u32, TagError> {
        let mut mask = 0u32;
        for name in names {
            let bit = self
                .names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| TagError::UnknownTag((*name).to_owned()))?;
            mask |= 1 << bit;
        }
        Ok(mask)
    }

    /// The names of the bits set in `mask`, in bit order.
    #[must_use]
    pub fn names_of(&self, mask: u32) -> Vec<&str> {
        self.names
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, n)| n.as_str())
            .collect()
    }
}

/// What a span does to the frames it covers.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum SpanKind {
    /// Frames may play through but are never chosen as a jump target.
    PassThrough,
    /// Frames are not indexed (never targets), though still sampled for neighbors' features.
    Unindexed,
    /// Adds the bias to the frame's cost when it is a jump target; negative favors it.
    LandingBias(f32),
    /// ORs the mask into the frames' tag mask.
    Tag(u32),
}

/// A half-open range `[start, end)` of clip seconds with an effect.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// Start, clip seconds.
    pub start: f32,
    /// End (exclusive), clip seconds.
    pub end: f32,
    /// The effect.
    pub kind: SpanKind,
}

/// Tags and spans of one database clip.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipAnnotation {
    /// Tags of every frame of the clip.
    pub tags: u32,
    /// Spans over clip time.
    pub spans: Vec<Span>,
}

/// The baked per-frame result of tags and spans.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameMeta {
    /// Tag mask.
    pub tags: u32,
    /// Landing bias added to the frame's cost as a jump target.
    pub bias: f32,
    /// [`FLAG_PASS_THROUGH`] and [`FLAG_UNINDEXED`].
    pub flags: u8,
}

impl FrameMeta {
    /// True when a search may choose this frame as a jump target (before the filter).
    #[must_use]
    pub fn is_target(&self) -> bool {
        self.flags & (FLAG_PASS_THROUGH | FLAG_UNINDEXED) == 0
    }
}

/// Which frames a search may choose: all `require` bits and none of the `forbid` bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchFilter {
    /// Tag bits a frame must all have.
    pub require: u32,
    /// Tag bits a frame must have none of.
    pub forbid: u32,
}

impl SearchFilter {
    /// A filter that accepts everything.
    pub const NONE: SearchFilter = SearchFilter {
        require: 0,
        forbid: 0,
    };

    /// True when the filter accepts every frame.
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.require == 0 && self.forbid == 0
    }

    /// True when a frame with tag mask `tags` passes.
    #[must_use]
    pub fn accepts(&self, tags: u32) -> bool {
        tags & self.require == self.require && tags & self.forbid == 0
    }
    /// The filter a frame passes only when it passes both `self` and `other` (requirements and
    /// exclusions are ORed).
    #[must_use]
    pub fn and(self, other: SearchFilter) -> SearchFilter {
        SearchFilter {
            require: self.require | other.require,
            forbid: self.forbid | other.forbid,
        }
    }
}

/// Checks one clip's spans.
///
/// # Errors
///
/// [`TagError::BadSpan`] or [`TagError::BadBias`] for the first bad span.
pub fn validate_spans(spans: &[Span]) -> Result<(), TagError> {
    for (index, span) in spans.iter().enumerate() {
        if !(span.start.is_finite() && span.end.is_finite() && span.start >= 0.0 && span.end > span.start) {
            return Err(TagError::BadSpan {
                index,
                start: span.start,
                end: span.end,
            });
        }
        if let SpanKind::LandingBias(value) = span.kind
            && !(value.is_finite() && value.abs() <= MAX_BIAS)
        {
            return Err(TagError::BadBias { index, value });
        }
    }
    Ok(())
}

/// Bakes `annotation` for frames at the given clip times: biases add, masks OR, the two flags
/// are sticky. A frame is covered by a span when `start <= time < end`.
#[must_use]
pub fn bake(annotation: &ClipAnnotation, times: impl Iterator<Item = f32>) -> Vec<FrameMeta> {
    times
        .map(|time| {
            let mut meta = FrameMeta {
                tags: annotation.tags,
                ..FrameMeta::default()
            };
            for span in &annotation.spans {
                if time < span.start || time >= span.end {
                    continue;
                }
                match span.kind {
                    SpanKind::PassThrough => meta.flags |= FLAG_PASS_THROUGH,
                    SpanKind::Unindexed => meta.flags |= FLAG_UNINDEXED,
                    SpanKind::LandingBias(b) => meta.bias += b,
                    SpanKind::Tag(mask) => meta.tags |= mask,
                }
            }
            meta.bias = meta.bias.clamp(-MAX_BIAS, MAX_BIAS);
            meta
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TagTable {
        TagTable::new(vec!["walk".into(), "stop".into(), "injured".into()]).expect("valid table")
    }

    #[test]
    fn masks_and_names_round_trip() {
        let t = table();
        let mask = t.mask(&["walk", "injured"]).expect("known tags");
        assert_eq!(mask, 0b101);
        assert_eq!(t.names_of(mask), vec!["walk", "injured"]);
        assert_eq!(t.mask(&[]), Ok(0));
        assert_eq!(t.mask(&["fly"]), Err(TagError::UnknownTag("fly".into())));
    }

    #[test]
    fn table_limits_are_enforced() {
        let many: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
        assert_eq!(TagTable::new(many), Err(TagError::TooManyTags(33)));
        assert!(TagTable::new((0..32).map(|i| format!("t{i}")).collect()).is_ok());
        assert!(matches!(
            TagTable::new(vec!["a".into(), "a".into()]),
            Err(TagError::BadTagName(_))
        ));
        assert!(matches!(
            TagTable::new(vec![String::new()]),
            Err(TagError::BadTagName(_))
        ));
    }

    #[test]
    fn filters_require_all_and_forbid_any() {
        let f = SearchFilter {
            require: 0b011,
            forbid: 0b100,
        };
        assert!(f.accepts(0b011));
        assert!(f.accepts(0b1011), "unrelated bits do not matter");
        assert!(!f.accepts(0b001), "missing a required bit");
        assert!(!f.accepts(0b111), "has a forbidden bit");
        assert!(SearchFilter::NONE.accepts(0));
        assert!(SearchFilter::NONE.is_none() && !f.is_none());
    }

    #[test]
    fn overlapping_spans_combine() {
        let annotation = ClipAnnotation {
            tags: 0b001,
            spans: vec![
                Span {
                    start: 0.0,
                    end: 1.0,
                    kind: SpanKind::LandingBias(-0.5),
                },
                Span {
                    start: 0.5,
                    end: 1.5,
                    kind: SpanKind::LandingBias(0.25),
                },
                Span {
                    start: 0.5,
                    end: 1.0,
                    kind: SpanKind::Tag(0b100),
                },
                Span {
                    start: 1.0,
                    end: 2.0,
                    kind: SpanKind::PassThrough,
                },
                Span {
                    start: 1.25,
                    end: 1.5,
                    kind: SpanKind::Unindexed,
                },
            ],
        };
        let metas = bake(&annotation, [0.0, 0.5, 1.0, 1.25, 2.0].into_iter());
        assert_eq!(
            metas[0],
            FrameMeta {
                tags: 0b001,
                bias: -0.5,
                flags: 0
            }
        );
        assert_eq!(metas[1].tags, 0b101, "tag span ORs in");
        assert!(
            (metas[1].bias - (-0.25)).abs() < 1e-6,
            "biases add: {}",
            metas[1].bias
        );
        assert_eq!(
            metas[2].flags, FLAG_PASS_THROUGH,
            "end is exclusive for the tag span"
        );
        assert_eq!(metas[2].tags, 0b001);
        assert_eq!(metas[3].flags, FLAG_PASS_THROUGH | FLAG_UNINDEXED);
        assert!(!metas[3].is_target() && metas[0].is_target());
        assert_eq!(
            metas[4],
            FrameMeta {
                tags: 0b001,
                bias: 0.0,
                flags: 0
            },
            "past every span"
        );
    }

    #[test]
    fn bad_spans_are_named() {
        let span = |start, end, kind| Span { start, end, kind };
        assert!(matches!(
            validate_spans(&[span(1.0, 1.0, SpanKind::PassThrough)]),
            Err(TagError::BadSpan { index: 0, .. })
        ));
        assert!(matches!(
            validate_spans(&[
                span(0.0, 1.0, SpanKind::PassThrough),
                span(f32::NAN, 2.0, SpanKind::Unindexed)
            ]),
            Err(TagError::BadSpan { index: 1, .. })
        ));
        assert!(matches!(
            validate_spans(&[span(0.0, 1.0, SpanKind::LandingBias(500.0))]),
            Err(TagError::BadBias { index: 0, .. })
        ));
        assert!(validate_spans(&[span(0.0, 1.0, SpanKind::LandingBias(-100.0))]).is_ok());
    }
}
