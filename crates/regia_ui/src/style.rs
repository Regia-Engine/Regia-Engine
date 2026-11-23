// SPDX-License-Identifier: MPL-2.0
//! Node styling: an authoring-friendly subset of taffy's style model (`specs/18` § 1), convertible
//! to [`taffy::Style`]. Styles are authored in UI logical pixels; the layout system scales them to
//! physical pixels according to the root's [`crate::ScaleMode`].
//!
//! RON ergonomics: every length field accepts a bare number (points), a percentage
//! (`Percent(50.0)`) or `Auto`; edge fields (`margin`, `padding`, `border`, `inset`) accept a bare
//! number (all four sides) or `(x, y)` / `(left, top, right, bottom)` tuples.

use serde::{Deserialize, Serialize};

use crate::color::UiColor;

/// A length: automatic (the layout engine decides), points, or a percentage of the available size.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Default)]
pub enum Dimension {
    /// Layout decides (content-based or flex share).
    #[default]
    Auto,
    /// Fixed size in UI pixels.
    Points(f32),
    /// Percentage of the parent's content box.
    Percent(f32),
}

impl From<Dimension> for taffy::style::Dimension {
    fn from(d: Dimension) -> Self {
        match d {
            Dimension::Auto => Self::auto(),
            Dimension::Points(p) => Self::length(p),
            Dimension::Percent(p) => Self::percent(p / 100.0),
        }
    }
}

impl From<Dimension> for taffy::style::LengthPercentage {
    fn from(d: Dimension) -> Self {
        match d {
            // Padding/border/gap have no Auto; treat as zero.
            Dimension::Auto => Self::length(0.0),
            Dimension::Points(p) => Self::length(p),
            Dimension::Percent(p) => Self::percent(p / 100.0),
        }
    }
}

impl From<Dimension> for taffy::style::LengthPercentageAuto {
    fn from(d: Dimension) -> Self {
        match d {
            Dimension::Auto => Self::auto(),
            Dimension::Points(p) => Self::length(p),
            Dimension::Percent(p) => Self::percent(p / 100.0),
        }
    }
}

impl Dimension {
    /// Scales a points dimension by `s` (percent and auto pass through).
    pub fn scaled(self, s: f32) -> Self {
        match self {
            Self::Points(p) => Self::Points(p * s),
            other => other,
        }
    }
}

impl<'de> Deserialize<'de> for Dimension {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Number(f32),
            Named(Named),
        }
        #[derive(Deserialize)]
        enum Named {
            Auto,
            Points(f32),
            Percent(f32),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Number(n) => Dimension::Points(n),
            Repr::Named(Named::Auto) => Dimension::Auto,
            Repr::Named(Named::Points(p)) => Dimension::Points(p),
            Repr::Named(Named::Percent(p)) => Dimension::Percent(p),
        })
    }
}

/// Box display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Display {
    /// Flexbox row/column layout.
    #[default]
    Flex,
    /// CSS grid layout.
    Grid,
    /// Block layout (stack vertically).
    Block,
    /// Not laid out and not rendered (children included).
    None,
}

impl From<Display> for taffy::style::Display {
    fn from(d: Display) -> Self {
        match d {
            Display::Flex => Self::Flex,
            Display::Grid => Self::Grid,
            Display::Block => Self::Block,
            Display::None => Self::None,
        }
    }
}

/// How a node is positioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PositionType {
    /// In flow.
    #[default]
    Relative,
    /// Out of flow, positioned by `inset` within the parent.
    Absolute,
}

impl From<PositionType> for taffy::style::Position {
    fn from(p: PositionType) -> Self {
        match p {
            PositionType::Relative => Self::Relative,
            PositionType::Absolute => Self::Absolute,
        }
    }
}

/// Flex direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FlexDirection {
    /// Along the inline direction: left to right (right to left in a right-to-left layout).
    #[default]
    Row,
    /// Against the inline direction: right to left (left to right in a right-to-left layout).
    RowReverse,
    /// Top to bottom.
    Column,
    /// Bottom to top.
    ColumnReverse,
}

impl From<FlexDirection> for taffy::style::FlexDirection {
    fn from(d: FlexDirection) -> Self {
        match d {
            FlexDirection::Row => Self::Row,
            FlexDirection::RowReverse => Self::RowReverse,
            FlexDirection::Column => Self::Column,
            FlexDirection::ColumnReverse => Self::ColumnReverse,
        }
    }
}

/// Main-axis alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum JustifyContent {
    /// Items packed to the main-start edge (where the first item goes: the right edge of a
    /// `RowReverse` or right-to-left row).
    #[default]
    Start,
    /// Items packed to the main-end edge.
    End,
    /// Centred along the main axis.
    Center,
    /// Space between items, none at the edges.
    SpaceBetween,
    /// Space around items.
    SpaceAround,
    /// Space evenly distributed.
    SpaceEvenly,
}

// `Start` / `End` are the flex-relative `flex-start` / `flex-end`: a `RowReverse` row packs to
// the right like its first item, and a right-to-left row packs to the right too (ADR-518).
// Taffy's `START` / `END` are writing-mode relative and would pack a reversed row to the left.
impl From<JustifyContent> for taffy::style::JustifyContent {
    fn from(j: JustifyContent) -> Self {
        match j {
            JustifyContent::Start => Self::FLEX_START,
            JustifyContent::End => Self::FLEX_END,
            JustifyContent::Center => Self::CENTER,
            JustifyContent::SpaceBetween => Self::SPACE_BETWEEN,
            JustifyContent::SpaceAround => Self::SPACE_AROUND,
            JustifyContent::SpaceEvenly => Self::SPACE_EVENLY,
        }
    }
}

/// Cross-axis alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    /// Parent's `align_items` decides.
    #[default]
    Auto,
    /// Start edge.
    Start,
    /// End edge.
    End,
    /// Centred.
    Center,
    /// Stretch to fill the cross axis.
    Stretch,
    /// Baseline-aligned.
    Baseline,
}

impl From<Align> for taffy::style::AlignItems {
    fn from(a: Align) -> Self {
        match a {
            Align::Auto | Align::Stretch => Self::STRETCH,
            Align::Start => Self::START,
            Align::End => Self::END,
            Align::Center => Self::CENTER,
            Align::Baseline => Self::BASELINE,
        }
    }
}

impl From<Align> for Option<taffy::style::AlignSelf> {
    fn from(a: Align) -> Self {
        match a {
            Align::Auto => None,
            Align::Stretch => Some(taffy::style::AlignSelf::STRETCH),
            Align::Start => Some(taffy::style::AlignSelf::START),
            Align::End => Some(taffy::style::AlignSelf::END),
            Align::Center => Some(taffy::style::AlignSelf::CENTER),
            Align::Baseline => Some(taffy::style::AlignSelf::BASELINE),
        }
    }
}

/// What happens to content larger than the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Overflow {
    /// Content may draw outside the box.
    #[default]
    Visible,
    /// Content outside the box is clipped.
    Clip,
}

impl From<Overflow> for taffy::style::Overflow {
    fn from(o: Overflow) -> Self {
        match o {
            Overflow::Visible => Self::Visible,
            Overflow::Clip => Self::Clip,
        }
    }
}

/// One grid track: `Auto`, `Px(n)`, `Fr(n)` (fraction of the free space) or `Percent(p)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackSizing {
    /// Sized to content.
    Auto,
    /// Fixed size in UI pixels.
    Px(f32),
    /// Fraction of the remaining space.
    Fr(f32),
    /// Percentage of the grid container.
    Percent(f32),
}

/// Edge widths (margin, padding, border, inset): one value per side, in order
/// left, top, right, bottom.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Default)]
pub struct Edges {
    /// Left.
    pub left: Dimension,
    /// Top.
    pub top: Dimension,
    /// Right.
    pub right: Dimension,
    /// Bottom.
    pub bottom: Dimension,
}

impl Edges {
    /// All four sides set to `v`.
    pub const fn all(v: Dimension) -> Self {
        Self {
            left: v,
            top: v,
            right: v,
            bottom: v,
        }
    }

    /// Uniform points.
    pub const fn px(v: f32) -> Self {
        Self::all(Dimension::Points(v))
    }

    /// Scales point values by `s`.
    pub fn scaled(self, s: f32) -> Self {
        Self {
            left: self.left.scaled(s),
            top: self.top.scaled(s),
            right: self.right.scaled(s),
            bottom: self.bottom.scaled(s),
        }
    }
}

impl<'de> Deserialize<'de> for Edges {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            All(Dimension),
            XY((Dimension, Dimension)),
            Ltrb([Dimension; 4]),
            Named(EdgesRepr),
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct EdgesRepr {
            left: Dimension,
            top: Dimension,
            right: Dimension,
            bottom: Dimension,
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::All(v) => Edges::all(v),
            Repr::XY((x, y)) => Edges {
                left: x,
                right: x,
                top: y,
                bottom: y,
            },
            Repr::Ltrb([l, t, r, b]) => Edges {
                left: l,
                top: t,
                right: r,
                bottom: b,
            },
            Repr::Named(e) => Edges {
                left: e.left,
                top: e.top,
                right: e.right,
                bottom: e.bottom,
            },
        })
    }
}

/// Text direction preference for the node's flex layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TextDir {
    /// Follow the content's bidi direction.
    #[default]
    Auto,
    /// Force left-to-right.
    Ltr,
    /// Force right-to-left.
    Rtl,
}

impl TextDir {
    /// Whether a node with this direction lays out right to left: `Rtl` always, `Ltr` never, and
    /// `Auto` inherits `inherited_rtl`: the parent's resolved direction, or at a root whether the
    /// active locale is a right-to-left one (see [`locale_is_rtl`]), as CSS `direction` inherits.
    pub fn is_rtl(self, inherited_rtl: bool) -> bool {
        match self {
            Self::Rtl => true,
            Self::Ltr => false,
            Self::Auto => inherited_rtl,
        }
    }
}

/// Languages written right to left (ISO 639 primary subtags): Arabic, Hebrew, Persian, Urdu,
/// Pashto, Sindhi, Yiddish, Dhivehi, Uyghur and Kurdish (Sorani, `ckb`).
const RTL_LANGUAGES: [&str; 10] = ["ar", "he", "fa", "ur", "ps", "sd", "yi", "dv", "ug", "ckb"];

/// Whether the BCP-47 `locale` tag (`ar`, `he-IL`, `fa_IR`) names a right-to-left language.
pub fn locale_is_rtl(locale: &str) -> bool {
    let primary = locale.split(['-', '_']).next().unwrap_or("");
    RTL_LANGUAGES.iter().any(|l| primary.eq_ignore_ascii_case(l))
}

impl Edges {
    /// The edges with left and right swapped (start/end mirroring for right-to-left layout).
    pub const fn mirrored(self) -> Self {
        Self {
            left: self.right,
            top: self.top,
            right: self.left,
            bottom: self.bottom,
        }
    }
}

/// The full node style. Field names mirror CSS; lengths are in UI pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Style {
    /// Box display mode (default flex).
    pub display: Display,
    /// Relative or absolute positioning.
    pub position: PositionType,
    /// Direction of the main axis.
    pub direction: FlexDirection,
    /// Main-axis alignment of children.
    pub justify_content: JustifyContent,
    /// Default cross-axis alignment of children.
    pub align_items: Align,
    /// This node's cross-axis alignment override.
    pub align_self: Align,
    /// Outer spacing.
    pub margin: Edges,
    /// Inner spacing.
    pub padding: Edges,
    /// Border widths (also rendered by [`crate::Border`]; layout reserves the space).
    pub border: Edges,
    /// Offsets for absolutely positioned nodes.
    pub inset: Edges,
    /// Width and height.
    pub size: [Dimension; 2],
    /// Minimum width and height.
    pub min_size: [Dimension; 2],
    /// Maximum width and height.
    pub max_size: [Dimension; 2],
    /// Preferred aspect ratio (width / height).
    pub aspect_ratio: Option<f32>,
    /// Child overflow behaviour (clip implies [`crate::ClipChildren`] rendering).
    pub overflow: [Overflow; 2],
    /// Whether flex wraps.
    pub flex_wrap: bool,
    /// Flex growth factor.
    pub flex_grow: f32,
    /// Flex shrink factor.
    pub flex_shrink: f32,
    /// Flex basis.
    pub flex_basis: Dimension,
    /// Grid rows (`grid` display only).
    pub grid_rows: SmallTracks,
    /// Grid columns (`grid` display only).
    pub grid_columns: SmallTracks,
    /// Gaps between flex items and grid tracks: `[horizontal, vertical]` (CSS `column-gap`,
    /// `row-gap`).
    pub gap: [Dimension; 2],
    /// Text direction of the layout.
    pub text_dir: TextDir,
}

/// Up to 16 grid tracks (keeps [`Style`] `Copy`).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct SmallTracks(pub Vec<TrackSizing>);

impl<'de> Deserialize<'de> for SmallTracks {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = Vec::<TrackSizing>::deserialize(deserializer)?;
        Ok(Self(v))
    }
}

impl SmallTracks {
    /// No tracks.
    pub fn empty() -> Self {
        Self(Vec::new())
    }
}

impl Style {
    /// The start/end swap of this style for right-to-left layout: the left and right edges of
    /// `margin`, `padding`, `border` and `inset` trade places (they are logical start and end
    /// edges). Everything else is kept: the flex direction stays as authored because the taffy
    /// style's `direction` flips the inline axis (ADR-518).
    #[must_use]
    pub fn mirrored(&self) -> Style {
        let mut s = self.clone();
        s.margin = s.margin.mirrored();
        s.padding = s.padding.mirrored();
        s.border = s.border.mirrored();
        s.inset = s.inset.mirrored();
        s
    }

    /// [`Self::to_taffy`] for a node laid out right to left when `rtl` is set: the start/end
    /// edges swap ([`Self::mirrored`]) and taffy's CSS `direction` is `Rtl`, so rows run from the
    /// right edge, `justify_content: Start` packs to the right and grid columns are placed right
    /// to left. Mirroring happens here, in the style-to-taffy step; draw emission is untouched.
    pub fn to_taffy_dir(&self, scale: f32, rtl: bool) -> taffy::style::Style {
        if rtl {
            let mut style = self.mirrored().to_taffy(scale);
            style.direction = taffy::style::Direction::Rtl;
            style
        } else {
            self.to_taffy(scale)
        }
    }

    /// Converts to a taffy style, scaling point lengths by `scale` (UI px → physical px).
    ///
    /// Values the layout engine cannot use are replaced, never passed through (user content must
    /// not produce NaN rects or overflow): a non-finite length becomes `Auto` (each property's
    /// initial value), a negative size, padding, border or gap becomes `Auto` too, lengths are
    /// clamped to ±`MAX_LAYOUT_PX` physical px and percentages to ±`MAX_PERCENT`, flex factors
    /// must be finite and non-negative (else 0), and an aspect ratio must be finite and positive
    /// (else none). A non-finite or non-positive `scale` counts as 1.
    pub fn to_taffy(&self, scale: f32) -> taffy::style::Style {
        use taffy::geometry::{Point, Rect, Size};
        use taffy::style as t;
        use taffy::style::GridTemplateComponent;
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let dim_size = |d: [Dimension; 2]| Size::<taffy::style::Dimension> {
            width: d[0].scaled(scale).sanitized(true).into(),
            height: d[1].scaled(scale).sanitized(true).into(),
        };
        let dim_minmax = |d: [Dimension; 2]| Size::<taffy::style::LengthPercentageAuto> {
            width: d[0].scaled(scale).sanitized(true).into(),
            height: d[1].scaled(scale).sanitized(true).into(),
        };
        let edges_auto = |e: Edges| Rect {
            left: e.left.scaled(scale).sanitized(false).into(),
            top: e.top.scaled(scale).sanitized(false).into(),
            right: e.right.scaled(scale).sanitized(false).into(),
            bottom: e.bottom.scaled(scale).sanitized(false).into(),
        };
        // Margins must convert `Auto` to zero, not to a CSS `margin: auto`: auto margins absorb
        // free space and centre the item, silently defeating `align_items: Stretch` and
        // `justify_content: Start` for every node that leaves its (default-Auto) margins unset.
        // Taffy's own default margin is zero; `Dimension::Auto` on a margin means "no margin".
        let edges_margin = |e: Edges| Rect {
            left: margin_zero(e.left, scale),
            top: margin_zero(e.top, scale),
            right: margin_zero(e.right, scale),
            bottom: margin_zero(e.bottom, scale),
        };
        let edges_lp = |e: Edges| Rect {
            left: e.left.scaled(scale).sanitized(true).into(),
            top: e.top.scaled(scale).sanitized(true).into(),
            right: e.right.scaled(scale).sanitized(true).into(),
            bottom: e.bottom.scaled(scale).sanitized(true).into(),
        };
        t::Style {
            display: self.display.into(),
            position: self.position.into(),
            flex_direction: self.direction.into(),
            justify_content: Some(self.justify_content.into()),
            align_items: Some(self.align_items.into()),
            align_self: self.align_self.into(),
            margin: edges_margin(self.margin),
            padding: edges_lp(self.padding),
            border: edges_lp(self.border),
            inset: edges_auto(self.inset),
            size: dim_size(self.size),
            min_size: dim_minmax(self.min_size),
            max_size: dim_minmax(self.max_size),
            aspect_ratio: self
                .aspect_ratio
                .filter(|r| r.is_finite() && *r > 0.0)
                .map(|r| r.clamp(1.0 / MAX_FACTOR, MAX_FACTOR)),
            overflow: Point {
                x: self.overflow[0].into(),
                y: self.overflow[1].into(),
            },
            flex_wrap: if self.flex_wrap {
                t::FlexWrap::Wrap
            } else {
                t::FlexWrap::NoWrap
            },
            flex_grow: factor(self.flex_grow),
            flex_shrink: factor(self.flex_shrink),
            flex_basis: self.flex_basis.scaled(scale).sanitized(true).into(),
            grid_template_rows: self
                .grid_rows
                .0
                .iter()
                .map(|s| GridTemplateComponent::Single(track(*s, scale)))
                .collect(),
            grid_template_columns: self
                .grid_columns
                .0
                .iter()
                .map(|s| GridTemplateComponent::Single(track(*s, scale)))
                .collect(),
            gap: Size {
                width: self.gap[0].scaled(scale).sanitized(true).into(),
                height: self.gap[1].scaled(scale).sanitized(true).into(),
            },
            ..Default::default()
        }
    }
}

/// A margin edge: `Auto` (unset) becomes zero — margins are spacing, never auto-centring
/// (see [`Style::to_taffy`]). Points scale with the UI scale; Percent passes through.
fn margin_zero(d: Dimension, scale: f32) -> taffy::style::LengthPercentageAuto {
    match d.scaled(scale).sanitized(false) {
        Dimension::Auto => taffy::style_helpers::zero(),
        other => other.into(),
    }
}

/// One grid track → taffy's min/max track sizing pair. A `Px` or `Percent` track is fixed at that
/// size (CSS `100px` / `50%`: content larger than the track overflows it rather than growing it);
/// an `Fr` track is `minmax(auto, Nfr)`. A track value layout cannot use (non-finite or
/// negative) becomes an `Auto` track, like an invalid CSS track.
fn track(s: TrackSizing, scale: f32) -> taffy::style::TrackSizingFunction {
    use taffy::geometry::MinMax;
    use taffy::style::{MaxTrackSizingFunction, MinTrackSizingFunction};
    let valid = |v: f32| v.is_finite() && v >= 0.0;
    match s {
        TrackSizing::Px(p) if valid(p * scale) => {
            let px = (p * scale).min(MAX_LAYOUT_PX);
            MinMax {
                min: MinTrackSizingFunction::length(px),
                max: MaxTrackSizingFunction::length(px),
            }
        }
        TrackSizing::Fr(f) if valid(f) => MinMax {
            min: MinTrackSizingFunction::auto(),
            max: MaxTrackSizingFunction::fr(f.min(MAX_FACTOR)),
        },
        TrackSizing::Percent(p) if valid(p) => {
            let fraction = p.min(MAX_PERCENT) / 100.0;
            MinMax {
                min: MinTrackSizingFunction::percent(fraction),
                max: MaxTrackSizingFunction::percent(fraction),
            }
        }
        _ => MinMax {
            min: MinTrackSizingFunction::auto(),
            max: MaxTrackSizingFunction::auto(),
        },
    }
}

/// Largest length magnitude layout accepts, in physical px: far beyond any screen, small enough
/// that sums of many such lengths stay finite in `f32`.
const MAX_LAYOUT_PX: f32 = 1.0e6;
/// Largest percentage magnitude layout accepts (100 times the reference size).
const MAX_PERCENT: f32 = 1.0e4;
/// Largest flex / `fr` factor and aspect ratio (the smallest aspect ratio is its reciprocal).
const MAX_FACTOR: f32 = 1.0e6;

/// A flex grow or shrink factor made safe for layout: finite and in `0..=MAX_FACTOR`, else 0.
fn factor(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, MAX_FACTOR)
    } else {
        0.0
    }
}

impl Dimension {
    /// This (already scaled) dimension made safe for layout: a non-finite value becomes `Auto`;
    /// with `non_negative` (sizes, padding, border, gap, basis) a negative one does too, as an
    /// invalid CSS value falls back to the property's initial value; magnitudes are clamped to
    /// `MAX_LAYOUT_PX` physical px and `MAX_PERCENT` percent.
    fn sanitized(self, non_negative: bool) -> Self {
        let keep =
            |v: f32, max: f32| (v.is_finite() && (v >= 0.0 || !non_negative)).then(|| v.clamp(-max, max));
        match self {
            Self::Auto => Self::Auto,
            Self::Points(p) => keep(p, MAX_LAYOUT_PX).map_or(Self::Auto, Self::Points),
            Self::Percent(p) => keep(p, MAX_PERCENT).map_or(Self::Auto, Self::Percent),
        }
    }
}

/// A resolved (physical-pixel) border: per-side widths and per-corner radii (tl, tr, br, bl).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderSpec {
    /// Border colour.
    pub color: UiColor,
    /// Widths: left, top, right, bottom (physical px).
    pub widths: [f32; 4],
    /// Corner radii: top-left, top-right, bottom-right, bottom-left (physical px).
    pub radius: [f32; 4],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtl_locales_are_detected_by_primary_subtag() {
        for tag in ["ar", "ar-EG", "he-IL", "fa_IR", "UR", "ckb-IQ"] {
            assert!(locale_is_rtl(tag), "{tag}");
        }
        for tag in ["en-US", "ja", "de", "", "arn", "heb"] {
            assert!(!locale_is_rtl(tag), "{tag}");
        }
    }

    #[test]
    fn text_dir_resolves_against_the_locale() {
        assert!(TextDir::Rtl.is_rtl(false));
        assert!(!TextDir::Ltr.is_rtl(true));
        assert!(TextDir::Auto.is_rtl(true));
        assert!(!TextDir::Auto.is_rtl(false));
    }

    #[test]
    fn mirrored_swaps_horizontal_edges_and_keeps_the_direction() {
        let style = Style {
            direction: FlexDirection::Row,
            margin: Edges {
                left: Dimension::Points(4.0),
                right: Dimension::Points(8.0),
                ..Edges::default()
            },
            padding: Edges {
                left: Dimension::Points(1.0),
                top: Dimension::Points(2.0),
                right: Dimension::Points(3.0),
                bottom: Dimension::Points(5.0),
            },
            ..Style::default()
        };
        let m = style.mirrored();
        // The flex direction is kept: taffy's `direction: Rtl` flips the inline axis (ADR-518).
        assert_eq!(m.direction, FlexDirection::Row);
        assert_eq!(m.margin.left, Dimension::Points(8.0));
        assert_eq!(m.margin.right, Dimension::Points(4.0));
        assert_eq!(m.padding.left, Dimension::Points(3.0));
        assert_eq!(m.padding.right, Dimension::Points(1.0));
        assert_eq!(m.padding.top, Dimension::Points(2.0));
        assert_eq!(m.padding.bottom, Dimension::Points(5.0));
        // Mirroring twice restores the style; columns keep their direction.
        assert_eq!(m.mirrored(), style);
        let col = Style {
            direction: FlexDirection::Column,
            ..Style::default()
        };
        assert_eq!(col.mirrored().direction, FlexDirection::Column);
    }

    /// Taffy's `START` is the left edge for `Row` and `RowReverse` alike, so a mirrored row swaps
    /// `Start`/`End` to keep its first child at the (now right) start edge. Columns and the
    /// symmetric values are untouched.
    #[test]
    fn mirrored_row_keeps_justify_and_the_taffy_direction_flips() {
        // ADR-518 replaces the justify swap of b09fa565: the mirror only trades the left and right
        // edges, and taffy's `direction: Rtl` with FLEX_START/FLEX_END packs a row from the right.
        let row = |direction, justify_content| Style {
            direction,
            justify_content,
            ..Style::default()
        };
        for direction in [
            FlexDirection::Row,
            FlexDirection::RowReverse,
            FlexDirection::Column,
            FlexDirection::ColumnReverse,
        ] {
            for kept in [
                JustifyContent::Start,
                JustifyContent::End,
                JustifyContent::Center,
                JustifyContent::SpaceBetween,
                JustifyContent::SpaceAround,
                JustifyContent::SpaceEvenly,
            ] {
                let mirrored = row(direction, kept).mirrored();
                assert_eq!(mirrored.justify_content, kept);
                assert_eq!(mirrored.direction, direction);
            }
        }
        let rtl = row(FlexDirection::Row, JustifyContent::Start).to_taffy_dir(1.0, true);
        assert_eq!(rtl.direction, taffy::style::Direction::Rtl);
        assert_eq!(
            rtl.justify_content,
            Some(taffy::style::JustifyContent::FLEX_START)
        );
        let end = row(FlexDirection::Row, JustifyContent::End).to_taffy_dir(1.0, true);
        assert_eq!(end.justify_content, Some(taffy::style::JustifyContent::FLEX_END));
    }

    #[test]
    fn to_taffy_dir_only_changes_rtl_output() {
        let style = Style {
            direction: FlexDirection::Row,
            padding: Edges {
                left: Dimension::Points(10.0),
                ..Edges::default()
            },
            ..Style::default()
        };
        assert_eq!(style.to_taffy_dir(1.0, false), style.to_taffy(1.0));
        assert_eq!(style.to_taffy(1.0).direction, taffy::style::Direction::Ltr);
        let rtl = style.to_taffy_dir(2.0, true);
        assert_eq!(rtl.flex_direction, taffy::style::FlexDirection::Row);
        assert_eq!(rtl.direction, taffy::style::Direction::Rtl);
        assert_eq!(rtl.padding.right, taffy::style::LengthPercentage::length(20.0));
        assert_eq!(rtl.padding.left, taffy::style::LengthPercentage::length(0.0));
    }

    #[test]
    fn justify_start_and_end_are_flex_relative() {
        let t = |j: JustifyContent| taffy::style::JustifyContent::from(j);
        assert_eq!(t(JustifyContent::Start), taffy::style::JustifyContent::FLEX_START);
        assert_eq!(t(JustifyContent::End), taffy::style::JustifyContent::FLEX_END);
    }

    #[test]
    fn unusable_values_become_initial_values() {
        use taffy::style::{Dimension as TD, LengthPercentage as LP, LengthPercentageAuto as LPA};
        let style = Style {
            size: [Dimension::Points(f32::NAN), Dimension::Points(-20.0)],
            min_size: [Dimension::Percent(f32::INFINITY), Dimension::Points(1.0e30)],
            max_size: [Dimension::Points(f32::NEG_INFINITY), Dimension::Percent(-5.0)],
            padding: Edges {
                left: Dimension::Points(-4.0),
                top: Dimension::Points(f32::NAN),
                right: Dimension::Points(3.0),
                bottom: Dimension::Percent(1.0e9),
            },
            margin: Edges {
                left: Dimension::Points(-4.0),
                top: Dimension::Points(f32::NAN),
                ..Edges::default()
            },
            inset: Edges::all(Dimension::Points(f32::INFINITY)),
            gap: [Dimension::Points(-1.0), Dimension::Points(f32::NAN)],
            aspect_ratio: Some(0.0),
            flex_grow: f32::NAN,
            flex_shrink: -2.0,
            flex_basis: Dimension::Points(-10.0),
            ..Style::default()
        };
        let t = style.to_taffy(1.0);
        assert_eq!(t.size.width, TD::auto());
        assert_eq!(t.size.height, TD::auto(), "negative size is invalid");
        assert_eq!(t.min_size.width, LPA::auto());
        assert_eq!(t.min_size.height, LPA::length(MAX_LAYOUT_PX), "clamped");
        assert_eq!(t.max_size.width, LPA::auto());
        assert_eq!(t.max_size.height, LPA::auto());
        assert_eq!(t.padding.left, LP::length(0.0));
        assert_eq!(t.padding.top, LP::length(0.0));
        assert_eq!(t.padding.right, LP::length(3.0));
        assert_eq!(t.padding.bottom, LP::percent(MAX_PERCENT / 100.0));
        // Negative margins are legal; a NaN margin is no margin.
        assert_eq!(t.margin.left, LPA::length(-4.0));
        assert_eq!(t.margin.top, LPA::length(0.0));
        assert_eq!(t.inset.left, LPA::auto());
        assert_eq!(t.gap.width, LP::length(0.0));
        assert_eq!(t.gap.height, LP::length(0.0));
        assert_eq!(t.aspect_ratio, None);
        assert_eq!(t.flex_grow, 0.0);
        assert_eq!(t.flex_shrink, 0.0);
        assert_eq!(t.flex_basis, TD::auto());
        // A broken scale counts as 1.
        let scaled = Style {
            size: [Dimension::Points(10.0), Dimension::Auto],
            ..Style::default()
        };
        assert_eq!(scaled.to_taffy(f32::NAN).size.width, TD::length(10.0));
        assert_eq!(scaled.to_taffy(-2.0).size.width, TD::length(10.0));
        // Extreme but valid aspect ratios are clamped, not dropped.
        let wide = Style {
            aspect_ratio: Some(1.0e12),
            ..Style::default()
        };
        assert_eq!(wide.to_taffy(1.0).aspect_ratio, Some(MAX_FACTOR));
    }

    #[test]
    fn percent_and_px_tracks_are_fixed_and_bad_tracks_are_auto() {
        use taffy::style::{MaxTrackSizingFunction as Max, MinTrackSizingFunction as Min};
        let pct = track(TrackSizing::Percent(50.0), 1.0);
        assert_eq!(pct.min, Min::percent(0.5));
        assert_eq!(pct.max, Max::percent(0.5));
        let px = track(TrackSizing::Px(40.0), 2.0);
        assert_eq!(px.min, Min::length(80.0));
        assert_eq!(px.max, Max::length(80.0));
        for bad in [
            TrackSizing::Px(-1.0),
            TrackSizing::Px(f32::NAN),
            TrackSizing::Fr(-1.0),
            TrackSizing::Fr(f32::INFINITY),
            TrackSizing::Percent(f32::NAN),
        ] {
            let t = track(bad, 1.0);
            assert_eq!(t.min, Min::auto(), "{bad:?}");
            assert_eq!(t.max, Max::auto(), "{bad:?}");
        }
    }

    #[test]
    fn dimension_ron_forms() {
        let pts: Dimension = ron::from_str("120").unwrap();
        assert_eq!(pts, Dimension::Points(120.0));
        let auto: Dimension = ron::from_str("Auto").unwrap();
        assert_eq!(auto, Dimension::Auto);
        let pct: Dimension = ron::from_str("Percent(50)").unwrap();
        assert_eq!(pct, Dimension::Percent(50.0));
    }

    #[test]
    fn edges_ron_forms() {
        let all: Edges = ron::from_str("8").unwrap();
        assert_eq!(all, Edges::px(8.0));
        let xy: Edges = ron::from_str("(4, 8)").unwrap();
        assert_eq!(xy.left, Dimension::Points(4.0));
        assert_eq!(xy.top, Dimension::Points(8.0));
        let ltrb: Edges = ron::from_str("(1, 2, 3, 4)").unwrap();
        assert_eq!(ltrb.bottom, Dimension::Points(4.0));
    }

    #[test]
    fn style_ron_roundtrip_and_taffy() {
        let src = r#"(
            display: flex,
            direction: column,
            justify_content: center,
            align_items: center,
            padding: 12,
            size: (Points(300.0), Auto),
            flex_grow: 1.0,
            grid_columns: [px(100), fr(1.0)],
        )"#;
        let style: Style = ron::from_str(src).unwrap();
        assert_eq!(style.direction, FlexDirection::Column);
        assert_eq!(style.padding, Edges::px(12.0));
        let t = style.to_taffy(2.0);
        assert_eq!(t.padding.top, taffy::style::LengthPercentage::length(24.0));
        assert_eq!(t.grid_template_columns.len(), 2);
    }

    #[test]
    fn unknown_fields_rejected() {
        let bad = ron::from_str::<Style>("(colur: 1)").unwrap_err();
        assert!(bad.to_string().contains("colur"));
    }
}
