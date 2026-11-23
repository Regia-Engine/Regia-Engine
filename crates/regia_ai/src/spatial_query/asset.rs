// SPDX-License-Identifier: MPL-2.0
//! The `.spatialquery.ron` asset (`docs/specs/33-spatial-queries.md` § 3, format `spatialquery` v1):
//! one generator, an ordered list of tests, declared parameters and a default pick.
//!
//! [`SpatialQueryAsset::parse`] reads and validates the text. Every load error carries a 1-based line
//! and column: parse errors take RON's position, validation errors the position of the offending
//! test id (or of the `generator` key).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The current `.spatialquery.ron` format version. Version 2 adds `Generator::Custom` and
/// `Test::Custom` (ADR-2709); a version 1 file means the same thing at version 2.
pub const SPATIAL_QUERY_FORMAT_VERSION: u32 = 2;

/// The oldest `.spatialquery.ron` format version this build reads.
pub const SPATIAL_QUERY_MIN_FORMAT_VERSION: u32 = 1;

/// The default limit on tests per asset (`SpatialQueryConfig::max_tests`).
pub const DEFAULT_MAX_TESTS: usize = 32;

/// The ordered serde field list of every struct in the format and the variant names of every enum
/// (for `cargo xtask check-formats`, `docs/specs/12-assets.md` § 4a).
pub fn spatial_query_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<SpatialQueryAsset>(),
        serde_field_names::<ScoreSpec>(),
        serde_field_names::<ArcSpec>(),
        serde_variant_names::<ItemType>(),
        serde_variant_names::<Pick>(),
        serde_variant_names::<ParamValue>(),
        serde_variant_names::<Anchor>(),
        serde_variant_names::<Direction>(),
        serde_variant_names::<Generator>(),
        serde_variant_names::<Test>(),
        serde_variant_names::<Purpose>(),
        serde_variant_names::<Require>(),
        serde_variant_names::<Combine>(),
        serde_variant_names::<Shape>(),
        serde_variant_names::<Origin>(),
        serde_variant_names::<Clamp>(),
        serde_variant_names::<DistanceMode>(),
        serde_variant_names::<ZoneMode>(),
        serde_variant_names::<CastShape>(),
        serde_variant_names::<RouteDirection>(),
        serde_variant_names::<RouteMeasure>(),
        serde_variant_names::<SnapTarget>(),
        serde_variant_names::<CustomValue>(),
    ])
}

/// The `from` versions of the registered migration steps: one step leaves version 1 (to 2).
pub fn spatial_query_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Rewrites the root `format_version: 1` of a `.spatialquery.ron` to 2 and keeps every other byte
/// (comments included): version 2 only adds variants, so a version 1 file means the same thing at
/// version 2 (`regia upgrade-project`). A file already at version 2 is returned unchanged.
///
/// # Errors
///
/// The text is not RON with a root `format_version`, or names a version this build does not read.
pub fn migrate_ron_v1_to_v2(text: &str) -> Result<String, SpatialQueryLoadError> {
    let at_version = |message: String| {
        let (line, col) = locate(text, "format_version", 0);
        SpatialQueryLoadError::Unreadable { line, col, message }
    };
    let header: IgnoreRest = ron_options()
        .from_str(text)
        .map_err(|e: ron::error::SpannedError| SpatialQueryLoadError::Unreadable {
            line: e.span.start.line,
            col: e.span.start.col,
            message: e.code.to_string(),
        })?;
    match header.format_version {
        SPATIAL_QUERY_FORMAT_VERSION => return Ok(text.to_string()),
        1 => {}
        found => {
            return Err(at_version(format!(
                "format_version {found} is not supported (this build reads \
                 {SPATIAL_QUERY_MIN_FORMAT_VERSION}..={SPATIAL_QUERY_FORMAT_VERSION})"
            )));
        }
    }
    let (start, end) = crate::state_tree::version_number_span(text)
        .ok_or_else(|| at_version("no root `format_version` number".to_string()))?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&SPATIAL_QUERY_FORMAT_VERSION.to_string());
    out.push_str(&text[end..]);
    Ok(out)
}

/// Optional fields take a bare value (`tag: "player"`) as well as `Some(..)`.
fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
}

/// A typed parameter value (declared default or caller override).
#[derive(Reflect, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParamValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f32),
    /// A name (a string).
    Name(String),
}

impl ParamValue {
    /// The value as a number (`Int` and `Float`), or `None`.
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            Self::Int(v) => Some(*v as f32),
            Self::Float(v) => Some(*v),
            Self::Bool(_) | Self::Name(_) => None,
        }
    }
}

/// A numeric field: a literal or a reference to a declared parameter.
///
/// In RON a literal is a plain number (`2.0`, `8`) and a parameter reference is a one-string tuple
/// with any name, by convention `ParamF32("radius")`.
#[derive(Debug, Clone, PartialEq)]
pub enum Num {
    /// A literal value.
    Lit(f32),
    /// The value of the parameter with this name.
    Param(String),
}

impl Num {
    /// The parameter this field reads, if any.
    pub fn param(&self) -> Option<&str> {
        match self {
            Self::Lit(_) => None,
            Self::Param(name) => Some(name),
        }
    }
}

impl From<f32> for Num {
    fn from(v: f32) -> Self {
        Self::Lit(v)
    }
}

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Lit(v) => serializer.serialize_f32(*v),
            Self::Param(name) => serializer.serialize_newtype_struct("ParamF32", name),
        }
    }
}

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NumVisitor;
        impl<'de> serde::de::Visitor<'de> for NumVisitor {
            type Value = Num;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number or a parameter reference such as ParamF32(\"radius\")")
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Num, E> {
                Ok(Num::Lit(v as f32))
            }
            fn visit_f32<E: serde::de::Error>(self, v: f32) -> Result<Num, E> {
                Ok(Num::Lit(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Num, E> {
                Ok(Num::Lit(v as f32))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Num, E> {
                Ok(Num::Lit(v as f32))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Num, E> {
                Ok(Num::Param(v.to_string()))
            }
            fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<Num, D::Error> {
                Ok(Num::Param(String::deserialize(d)?))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Num, A::Error> {
                let name: String = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("a parameter reference needs a name"))?;
                if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom("a parameter reference holds one name"));
                }
                Ok(Num::Param(name))
            }
        }
        deserializer.deserialize_any(NumVisitor)
    }
}

/// Which kind of item a query produces.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ItemType {
    /// Positions.
    #[default]
    Point,
    /// Entities (with their positions).
    Entity,
}

impl ItemType {
    /// The agent string (`"point"` or `"entity"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::Entity => "entity",
        }
    }
}

/// How the result is chosen from the scored survivors (§ 3.6).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Pick {
    /// The single highest-scored item (ties to the lower generation index).
    #[default]
    Best,
    /// One item drawn uniformly from the items with `score >= best - band * (best - worst)`.
    Sample {
        /// The score band, 0..=1.
        #[serde(default = "default_band")]
        band: f32,
    },
    /// Every survivor, score descending, ties by generation index.
    All,
}

fn default_band() -> f32 {
    0.1
}

/// A frame of reference a generator or test points at (§ 3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Anchor {
    /// The entity running the query.
    Asker,
    /// The item under test (only valid inside tests). Write `Candidate()` for no offset.
    Candidate {
        /// Vertical offset.
        #[serde(default)]
        height: f32,
    },
    /// Every live entity with the scene tag (`regia_scene::Tags`) or the `Name`.
    Entity {
        /// The scene tag to match.
        #[serde(default)]
        tag: Option<String>,
        /// The entity name to match.
        #[serde(default)]
        name: Option<String>,
        /// Vertical offset.
        #[serde(default)]
        height: f32,
    },
    /// A fixed world position.
    Point(Vec3),
    /// A vec3 or entity value on the asker's `Blackboard`.
    Blackboard(String),
    /// A Rust [`super::AnchorProvider`] registered by name.
    Provider(String),
    /// Another anchor raised by `height` (for anchors without their own `height` field).
    Offset {
        /// The anchor.
        anchor: Box<Anchor>,
        /// Vertical offset.
        height: f32,
    },
}

impl Anchor {
    /// The candidate anchor with no offset.
    pub const CANDIDATE: Anchor = Anchor::Candidate { height: 0.0 };

    pub(crate) fn uses_candidate(&self) -> bool {
        match self {
            Self::Candidate { .. } => true,
            Self::Offset { anchor, .. } => anchor.uses_candidate(),
            _ => false,
        }
    }
}

/// A direction: an anchor's forward (-Z of its entity; world -Z for a plain position) or the
/// direction from one anchor to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Direction {
    /// The forward of each value of the anchor.
    Forward(Anchor),
    /// From the first anchor to the second (every pair of values).
    Between(Anchor, Anchor),
}

impl Direction {
    pub(crate) fn uses_candidate(&self) -> bool {
        match self {
            Self::Forward(a) => a.uses_candidate(),
            Self::Between(a, b) => a.uses_candidate() || b.uses_candidate(),
        }
    }
}

/// An arc limiting a ring or annulus: centered on `toward`, spanning `degrees`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArcSpec {
    /// The arc's middle direction (its first value; planar).
    pub toward: Direction,
    /// The arc's full angle in degrees.
    pub degrees: Num,
}

fn num_zero() -> Num {
    Num::Lit(0.0)
}
fn num_one() -> Num {
    Num::Lit(1.0)
}
fn num_reach_limit() -> Num {
    Num::Lit(1.5)
}
fn anchor_asker() -> Anchor {
    Anchor::Asker
}
fn anchor_candidate() -> Anchor {
    Anchor::CANDIDATE
}
fn yes() -> bool {
    true
}
fn default_extents() -> Vec3 {
    Vec3::new(1.0, 2.0, 1.0)
}

/// A generator: produces the candidate items (§ 3.3). Point generators can project their points onto
/// the navigation mesh (`project_to_navmesh`) and lower them to the ground (`drop_to_ground: h`: a
/// downward ray from `h` above to `h` below; points with no ground are dropped).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Generator {
    /// Entity items: every entity with the scene tag or name within `radius` of a center value.
    NearbyEntities {
        /// The scene tag to match.
        #[serde(default)]
        tag: Option<String>,
        /// The entity name to match.
        #[serde(default)]
        name: Option<String>,
        /// The center.
        #[serde(default = "anchor_asker")]
        center: Anchor,
        /// The inclusive search radius; `None` searches the whole world, `<= 0` finds nothing.
        #[serde(default)]
        radius: Option<Num>,
    },
    /// One point per value of an anchor.
    AtAnchor {
        /// The anchor.
        anchor: Anchor,
        /// Project onto the navigation mesh.
        #[serde(default)]
        project_to_navmesh: bool,
        /// Lower to the ground with a trace of this height.
        #[serde(default)]
        drop_to_ground: Option<Num>,
    },
    /// Points on a circle edge, angle ascending.
    Ring {
        /// The center.
        #[serde(default = "anchor_asker")]
        center: Anchor,
        /// The radius.
        radius: Num,
        /// Distance between points along the edge (or use `count`).
        #[serde(default)]
        gap: Option<Num>,
        /// Number of points (or use `gap`).
        #[serde(default)]
        count: Option<Num>,
        /// Optional arc.
        #[serde(default)]
        arc: Option<ArcSpec>,
        /// Vertical offset of the points.
        #[serde(default = "num_zero")]
        height: Num,
        /// Project onto the navigation mesh.
        #[serde(default)]
        project_to_navmesh: bool,
        /// Lower to the ground with a trace of this height.
        #[serde(default)]
        drop_to_ground: Option<Num>,
    },
    /// Points along the rays of a cone, angle ascending then distance ascending.
    Wedge {
        /// The apex.
        #[serde(default = "anchor_asker")]
        center: Anchor,
        /// The cone's middle direction.
        facing: Direction,
        /// The cone's full angle in degrees.
        degrees: Num,
        /// The ray length.
        range: Num,
        /// Degrees between rays (at least 1).
        #[serde(default = "num_one")]
        angle_step: Num,
        /// Distance between points along a ray.
        aligned_spacing: Num,
        /// Also emit the apex (once, first).
        #[serde(default)]
        include_center: bool,
        /// Project onto the navigation mesh.
        #[serde(default)]
        project_to_navmesh: bool,
        /// Lower to the ground with a trace of this height.
        #[serde(default)]
        drop_to_ground: Option<Num>,
    },
    /// Points on concentric rings, inner ring first.
    Annulus {
        /// The center.
        #[serde(default = "anchor_asker")]
        center: Anchor,
        /// Inner radius.
        inner_radius: Num,
        /// Outer radius (swapped with `inner_radius` when smaller).
        outer_radius: Num,
        /// Number of rings (1 uses the mid radius).
        rings: Num,
        /// Points per ring.
        points_per_ring: Num,
        /// Optional arc.
        #[serde(default)]
        arc: Option<ArcSpec>,
        /// Rotates ring `i` by `i * spiral` degrees.
        #[serde(default)]
        spiral: Option<Num>,
        /// Project onto the navigation mesh.
        #[serde(default)]
        project_to_navmesh: bool,
        /// Lower to the ground with a trace of this height.
        #[serde(default)]
        drop_to_ground: Option<Num>,
    },
    /// A square lattice in the XZ plane, row-major, rows along X (X varies fastest), then Z.
    Lattice {
        /// The center.
        #[serde(default = "anchor_asker")]
        center: Anchor,
        /// Half the side length.
        half_size: Num,
        /// Distance between points.
        spacing: Num,
        /// Project onto the navigation mesh.
        #[serde(default)]
        project_to_navmesh: bool,
        /// Lower to the ground with a trace of this height.
        #[serde(default)]
        drop_to_ground: Option<Num>,
    },
    /// A lattice projected onto the navigation mesh, keeping the points reachable from the center
    /// within `reach_limit * half_size` of path length.
    ReachLattice {
        /// The center.
        #[serde(default = "anchor_asker")]
        center: Anchor,
        /// Half the side length.
        half_size: Num,
        /// Distance between points.
        spacing: Num,
        /// The largest path length as a multiple of `half_size`.
        #[serde(default = "num_reach_limit")]
        reach_limit: Num,
        /// Lower to the ground with a trace of this height.
        #[serde(default)]
        drop_to_ground: Option<Num>,
    },
    /// Runs every child; duplicates (same position within 1e-4 and same entity) keep the first.
    Union {
        /// The child generators.
        children: Vec<Generator>,
        /// Allow entity children in a point query (their items become positions).
        #[serde(default)]
        allow_mixed: bool,
    },
    /// Items from a Rust [`super::GeneratorImpl`] registered by name (format version 2). An
    /// unregistered name ends the run with `sq_missing_system`; an item of the wrong type with
    /// `sq_item_type_mismatch`. The projection and ground trace apply to its point items.
    Custom {
        /// The registered name.
        name: String,
        /// The anchor whose values the generator receives.
        #[serde(default = "anchor_asker")]
        center: Anchor,
        /// Named numbers handed to the generator: literals or parameter references.
        #[serde(default)]
        args: BTreeMap<String, Num>,
        /// Project onto the navigation mesh.
        #[serde(default)]
        project_to_navmesh: bool,
        /// Lower to the ground with a trace of this height.
        #[serde(default)]
        drop_to_ground: Option<Num>,
    },
}

impl Generator {
    /// The item type this generator produces on its own (a union reports `declared`).
    fn output(&self, declared: ItemType) -> ItemType {
        match self {
            Self::NearbyEntities { .. } => ItemType::Entity,
            Self::Union { .. } | Self::Custom { .. } => declared,
            _ => ItemType::Point,
        }
    }

    /// The variant name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NearbyEntities { .. } => "NearbyEntities",
            Self::AtAnchor { .. } => "AtAnchor",
            Self::Ring { .. } => "Ring",
            Self::Wedge { .. } => "Wedge",
            Self::Annulus { .. } => "Annulus",
            Self::Lattice { .. } => "Lattice",
            Self::ReachLattice { .. } => "ReachLattice",
            Self::Union { .. } => "Union",
            Self::Custom { .. } => "Custom",
        }
    }

    fn anchors_use_candidate(&self) -> bool {
        match self {
            Self::NearbyEntities { center, .. }
            | Self::Lattice { center, .. }
            | Self::ReachLattice { center, .. }
            | Self::Custom { center, .. } => center.uses_candidate(),
            Self::AtAnchor { anchor, .. } => anchor.uses_candidate(),
            Self::Ring { center, arc, .. } | Self::Annulus { center, arc, .. } => {
                center.uses_candidate() || arc.as_ref().is_some_and(|a| a.toward.uses_candidate())
            }
            Self::Wedge { center, facing, .. } => center.uses_candidate() || facing.uses_candidate(),
            Self::Union { children, .. } => children.iter().any(Generator::anchors_use_candidate),
        }
    }

    /// Whether this generator is a `Union` (whose items are deduplicated).
    pub(crate) fn has_union(&self) -> bool {
        matches!(self, Self::Union { .. })
    }

    /// Whether this generator or a child of it is `Custom`.
    fn uses_custom(&self) -> bool {
        match self {
            Self::Custom { .. } => true,
            Self::Union { children, .. } => children.iter().any(Generator::uses_custom),
            _ => false,
        }
    }
}

/// What a test does with its result (§ 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Purpose {
    /// Drop failing items.
    Filter,
    /// Weight the survivors.
    Score,
    /// Both.
    Both,
}

impl Purpose {
    /// Filters (alone or with scoring).
    pub fn filters(self) -> bool {
        matches!(self, Self::Filter | Self::Both)
    }
    /// Scores (alone or with filtering).
    pub fn scores(self) -> bool {
        matches!(self, Self::Score | Self::Both)
    }
}

/// How a filter combines several anchor values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Require {
    /// Every value must pass.
    #[default]
    Every,
    /// At least one value must pass.
    Some,
}

/// How a score combines several anchor values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Combine {
    /// The arithmetic mean.
    #[default]
    Mean,
    /// The lowest.
    Lowest,
    /// The highest.
    Highest,
    /// The product.
    Product,
}

/// The score curve over the normalized value `n` (§ 3.5 step 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Shape {
    /// 1.
    Flat,
    /// `n`.
    #[default]
    Linear,
    /// `n^2`.
    Quadratic,
    /// `1 - n`.
    Falling,
    /// `sqrt(n)`.
    Root,
}

/// The normalization origin (§ 3.5 step 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Origin {
    /// `n = (v - lo) / (hi - lo)`.
    #[default]
    Floor,
    /// `n = v / hi`.
    Zero,
}

/// Where a score clamp bound comes from (§ 3.5 step 1).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum Clamp {
    /// A fixed value.
    Fixed(Num),
    /// The test's filter `min` / `max`.
    FilterBound,
    /// The smallest / largest raw value among the items alive when the score pass starts.
    #[default]
    Seen,
}

/// How a test scores (§ 3.5). Boolean tests read only `weight`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreSpec {
    /// The curve.
    #[serde(default)]
    pub shape: Shape,
    /// The weight (may be negative).
    #[serde(default = "num_one")]
    pub weight: Num,
    /// The lower clamp bound.
    #[serde(default)]
    pub clamp_min: Clamp,
    /// The upper clamp bound.
    #[serde(default)]
    pub clamp_max: Clamp,
    /// The normalization origin.
    #[serde(default)]
    pub origin: Origin,
    /// Score peaks at this raw value and falls off toward the bounds.
    #[serde(default)]
    pub peak_at: Option<Num>,
}

impl Default for ScoreSpec {
    fn default() -> Self {
        Self {
            shape: Shape::Linear,
            weight: num_one(),
            clamp_min: Clamp::Seen,
            clamp_max: Clamp::Seen,
            origin: Origin::Floor,
            peak_at: None,
        }
    }
}

/// How `Distance` measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DistanceMode {
    /// Euclidean.
    #[default]
    Full3d,
    /// Ignoring height.
    Planar,
    /// Signed height difference (item minus anchor).
    Vertical,
    /// Absolute height difference.
    AbsVertical,
}

/// How `InsideZone` tests containment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ZoneMode {
    /// Inside the zone's collider shape (box, sphere, capsule, cylinder; other shapes use the bounds).
    #[default]
    Exact,
    /// Inside the world-space bounding box of the zone's collider.
    Bounds,
}

/// The shape of a `Cast` or a `Clearance` volume.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum CastShape {
    /// A ray (`Cast` only).
    #[default]
    Line,
    /// A sphere of this radius.
    Sphere(f32),
    /// A box with these half-extents.
    Box(Vec3),
    /// A Y-axis capsule: radius, half-height of the cylinder part.
    Capsule(f32, f32),
}

/// Which way a `Route` path runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RouteDirection {
    /// From the item to the anchor.
    #[default]
    ToAnchor,
    /// From the anchor to the item.
    FromAnchor,
}

/// What a `Route` test reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RouteMeasure {
    /// A boolean: a path exists.
    #[default]
    Exists,
    /// A float: the path length (`regia_nav` answers lengths; its cost is the length).
    Length,
}

/// What `Snap` moves the item onto.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum SnapTarget {
    /// The navigation mesh.
    #[default]
    NavMesh,
    /// Physics geometry on these layers (empty: every layer).
    Geometry(Vec<String>),
}

/// What a `Test::Custom` row says its [`super::TestImpl`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CustomValue {
    /// A boolean, compared with `want`.
    #[default]
    Bool,
    /// A float, read by `min`, `max` and `score`.
    Float,
}

/// A test: filters and/or scores items (§ 3.4). Every test has an `id` unique in the asset and a
/// `purpose`; anchors with several values combine through `require` (filter) and `combine` (score).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Test {
    /// Float: distance from the item to each anchor value.
    Distance {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The anchor.
        to: Anchor,
        /// How to measure.
        #[serde(default)]
        mode: DistanceMode,
        /// Keep `v >= min`.
        #[serde(default)]
        min: Option<Num>,
        /// Keep `v <= max`.
        #[serde(default)]
        max: Option<Num>,
        /// Scoring.
        #[serde(default)]
        score: ScoreSpec,
        /// Multi-value filtering.
        #[serde(default)]
        require: Require,
        /// Multi-value scoring.
        #[serde(default)]
        combine: Combine,
    },
    /// Float: the dot product of two unit directions, -1..1.
    Facing {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The first direction.
        a: Direction,
        /// The second direction.
        b: Direction,
        /// Ignore height.
        #[serde(default)]
        planar: bool,
        /// Use the absolute value.
        #[serde(default)]
        abs: bool,
        /// Keep `v >= min`.
        #[serde(default)]
        min: Option<Num>,
        /// Keep `v <= max`.
        #[serde(default)]
        max: Option<Num>,
        /// Scoring.
        #[serde(default)]
        score: ScoreSpec,
        /// Multi-value filtering.
        #[serde(default)]
        require: Require,
        /// Multi-value scoring.
        #[serde(default)]
        combine: Combine,
    },
    /// Boolean: the item is inside a zone (an entity with the scene tag and a physics `Collider`).
    InsideZone {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The zones' scene tag.
        tag: String,
        /// Exact shape or bounding box.
        #[serde(default)]
        mode: ZoneMode,
        /// The wanted boolean.
        #[serde(default = "yes")]
        want: bool,
        /// Scoring (`weight` only).
        #[serde(default)]
        score: ScoreSpec,
    },
    /// Boolean: the item entity (or the entity nearest a point item) matches a tag query through
    /// the host's tag matcher.
    TagMatch {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The tag query handed to the matcher.
        query: String,
        /// The wanted boolean.
        #[serde(default = "yes")]
        want: bool,
        /// Scoring (`weight` only).
        #[serde(default)]
        score: ScoreSpec,
    },
    /// Boolean: a ray or swept shape between two anchors is blocked.
    Cast {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The start.
        #[serde(default = "anchor_candidate")]
        from: Anchor,
        /// The end.
        to: Anchor,
        /// Ray or swept shape.
        #[serde(default)]
        shape: CastShape,
        /// Physics layer names (empty: every layer).
        #[serde(default)]
        layers: Vec<String>,
        /// Ignore sensor colliders.
        #[serde(default)]
        blocking_only: bool,
        /// The wanted boolean (`false` keeps items with a clear line).
        want_hit: bool,
        /// Scoring (`weight` only).
        #[serde(default)]
        score: ScoreSpec,
        /// Multi-value filtering.
        #[serde(default)]
        require: Require,
        /// Multi-value scoring.
        #[serde(default)]
        combine: Combine,
    },
    /// Boolean: a volume at the item overlaps anything.
    Clearance {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The volume (not `Line`).
        shape: CastShape,
        /// Offset of the volume from the item.
        #[serde(default)]
        offset: Vec3,
        /// Physics layer names (empty: every layer).
        #[serde(default)]
        layers: Vec<String>,
        /// Ignore sensor colliders.
        #[serde(default)]
        blocking_only: bool,
        /// Ignore the asker's colliders.
        #[serde(default = "yes")]
        skip_asker: bool,
        /// The wanted boolean (`false` keeps items with free space).
        #[serde(default)]
        want_hit: bool,
        /// Scoring (`weight` only).
        #[serde(default)]
        score: ScoreSpec,
    },
    /// Boolean (a path exists) or float (path length) between the item and each anchor value.
    Route {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The anchor.
        to: Anchor,
        /// Path direction.
        #[serde(default)]
        direction: RouteDirection,
        /// Boolean or length.
        #[serde(default)]
        measure: RouteMeasure,
        /// Unreachable items fail instead of scoring worst.
        #[serde(default)]
        drop_unreachable: bool,
        /// Keep `v >= min` (length).
        #[serde(default)]
        min: Option<Num>,
        /// Keep `v <= max` (length).
        #[serde(default)]
        max: Option<Num>,
        /// Scoring.
        #[serde(default)]
        score: ScoreSpec,
        /// Multi-value filtering.
        #[serde(default)]
        require: Require,
        /// Multi-value scoring.
        #[serde(default)]
        combine: Combine,
    },
    /// `Route` for every anchor value, skipping pairs farther apart than `scan_limit` (straight line;
    /// default 1.5 times the largest item-to-anchor distance seen).
    RouteAll {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The anchor.
        to: Anchor,
        /// Path direction.
        #[serde(default)]
        direction: RouteDirection,
        /// Boolean or length.
        #[serde(default)]
        measure: RouteMeasure,
        /// Unreachable items fail instead of scoring worst.
        #[serde(default)]
        drop_unreachable: bool,
        /// The largest examined straight-line distance.
        #[serde(default)]
        scan_limit: Option<Num>,
        /// Keep `v >= min` (length).
        #[serde(default)]
        min: Option<Num>,
        /// Keep `v <= max` (length).
        #[serde(default)]
        max: Option<Num>,
        /// Scoring.
        #[serde(default)]
        score: ScoreSpec,
        /// Multi-value filtering.
        #[serde(default)]
        require: Require,
        /// Multi-value scoring.
        #[serde(default)]
        combine: Combine,
    },
    /// Rewrites the item: moves it onto the navigation mesh or geometry, searching `up` above and
    /// `down` below, then adds `offset`; items that cannot snap are dropped. Point queries only;
    /// always a filter; snaps run before every other test.
    Snap {
        /// Unique id.
        id: String,
        /// Must be `Filter`.
        purpose: Purpose,
        /// Navigation mesh or geometry.
        #[serde(default)]
        target: SnapTarget,
        /// Search distance above the item.
        #[serde(default = "num_one")]
        up: Num,
        /// Search distance below the item.
        #[serde(default = "num_one")]
        down: Num,
        /// Added after snapping.
        #[serde(default)]
        offset: Vec3,
    },
    /// A Rust [`super::TestImpl`] registered by name (format version 2). An unregistered name ends
    /// the run with `sq_missing_system`; a value of the other kind than `value` with
    /// `sq_bad_param`.
    Custom {
        /// Unique id.
        id: String,
        /// Filter / score / both.
        purpose: Purpose,
        /// The registered name.
        name: String,
        /// The kind of value the implementation returns.
        #[serde(default)]
        value: CustomValue,
        /// The anchor evaluated per value; `None` evaluates each item once with no anchor.
        #[serde(default)]
        to: Option<Anchor>,
        /// Named numbers handed to the test: literals or parameter references.
        #[serde(default)]
        args: BTreeMap<String, Num>,
        /// The wanted boolean (`value: Bool`).
        #[serde(default = "yes")]
        want: bool,
        /// Keep `v >= min` (`value: Float`).
        #[serde(default)]
        min: Option<Num>,
        /// Keep `v <= max` (`value: Float`).
        #[serde(default)]
        max: Option<Num>,
        /// Scoring.
        #[serde(default)]
        score: ScoreSpec,
        /// Multi-value filtering.
        #[serde(default)]
        require: Require,
        /// Multi-value scoring.
        #[serde(default)]
        combine: Combine,
    },
}

/// Borrowed view of the fields most tests share.
pub(crate) struct TestCommon<'a> {
    pub id: &'a str,
    pub purpose: Purpose,
    pub min: Option<&'a Num>,
    pub max: Option<&'a Num>,
    pub score: Option<&'a ScoreSpec>,
    pub require: Require,
    pub combine: Combine,
}

impl Test {
    /// The test id.
    pub fn id(&self) -> &str {
        self.common().id
    }

    /// The test purpose.
    pub fn purpose(&self) -> Purpose {
        self.common().purpose
    }

    /// The variant name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Distance { .. } => "Distance",
            Self::Facing { .. } => "Facing",
            Self::InsideZone { .. } => "InsideZone",
            Self::TagMatch { .. } => "TagMatch",
            Self::Cast { .. } => "Cast",
            Self::Clearance { .. } => "Clearance",
            Self::Route { .. } => "Route",
            Self::RouteAll { .. } => "RouteAll",
            Self::Snap { .. } => "Snap",
            Self::Custom { .. } => "Custom",
        }
    }

    pub(crate) fn common(&self) -> TestCommon<'_> {
        match self {
            Self::Distance {
                id,
                purpose,
                min,
                max,
                score,
                require,
                combine,
                ..
            }
            | Self::Facing {
                id,
                purpose,
                min,
                max,
                score,
                require,
                combine,
                ..
            }
            | Self::Route {
                id,
                purpose,
                min,
                max,
                score,
                require,
                combine,
                ..
            }
            | Self::RouteAll {
                id,
                purpose,
                min,
                max,
                score,
                require,
                combine,
                ..
            }
            | Self::Custom {
                id,
                purpose,
                min,
                max,
                score,
                require,
                combine,
                ..
            } => TestCommon {
                id,
                purpose: *purpose,
                min: min.as_ref(),
                max: max.as_ref(),
                score: Some(score),
                require: *require,
                combine: *combine,
            },
            Self::Cast {
                id,
                purpose,
                score,
                require,
                combine,
                ..
            } => TestCommon {
                id,
                purpose: *purpose,
                min: None,
                max: None,
                score: Some(score),
                require: *require,
                combine: *combine,
            },
            Self::InsideZone {
                id, purpose, score, ..
            }
            | Self::TagMatch {
                id, purpose, score, ..
            }
            | Self::Clearance {
                id, purpose, score, ..
            } => TestCommon {
                id,
                purpose: *purpose,
                min: None,
                max: None,
                score: Some(score),
                require: Require::Every,
                combine: Combine::Mean,
            },
            Self::Snap { id, purpose, .. } => TestCommon {
                id,
                purpose: *purpose,
                min: None,
                max: None,
                score: None,
                require: Require::Every,
                combine: Combine::Mean,
            },
        }
    }

    /// Whether the raw value is a float (`Distance`, `Facing`, `Route`/`RouteAll` measuring length).
    pub fn is_float(&self) -> bool {
        match self {
            Self::Distance { .. } | Self::Facing { .. } => true,
            Self::Route { measure, .. } | Self::RouteAll { measure, .. } => *measure == RouteMeasure::Length,
            Self::Custom { value, .. } => *value == CustomValue::Float,
            _ => false,
        }
    }

    /// The item types the test accepts.
    fn accepts(&self, item_type: ItemType) -> bool {
        match self {
            Self::Snap { .. } => item_type == ItemType::Point,
            _ => true,
        }
    }

    /// Every parameter name a numeric field of this test reads.
    fn params(&self, out: &mut BTreeSet<String>) {
        let c = self.common();
        for n in [c.min, c.max].into_iter().flatten() {
            push_param(n, out);
        }
        if let Some(score) = c.score {
            score_params(score, out);
        }
        match self {
            Self::RouteAll {
                scan_limit: Some(n), ..
            } => push_param(n, out),
            Self::Snap { up, down, .. } => {
                push_param(up, out);
                push_param(down, out);
            }
            Self::Custom { args, .. } => args.values().for_each(|n| push_param(n, out)),
            _ => {}
        }
    }
}

fn push_param(n: &Num, out: &mut BTreeSet<String>) {
    if let Some(name) = n.param() {
        out.insert(name.to_string());
    }
}

fn score_params(score: &ScoreSpec, out: &mut BTreeSet<String>) {
    push_param(&score.weight, out);
    for clamp in [&score.clamp_min, &score.clamp_max] {
        if let Clamp::Fixed(n) = clamp {
            push_param(n, out);
        }
    }
    if let Some(n) = &score.peak_at {
        push_param(n, out);
    }
}

fn generator_params(generator: &Generator, out: &mut BTreeSet<String>) {
    let mut push = |n: &Num| push_param(n, out);
    match generator {
        Generator::NearbyEntities { radius, .. } => {
            if let Some(n) = radius {
                push(n);
            }
        }
        Generator::AtAnchor { drop_to_ground, .. } => {
            if let Some(n) = drop_to_ground {
                push(n);
            }
        }
        Generator::Ring {
            radius,
            gap,
            count,
            arc,
            height,
            drop_to_ground,
            ..
        } => {
            push(radius);
            push(height);
            for n in [gap, count, drop_to_ground].into_iter().flatten() {
                push(n);
            }
            if let Some(a) = arc {
                push(&a.degrees);
            }
        }
        Generator::Wedge {
            degrees,
            range,
            angle_step,
            aligned_spacing,
            drop_to_ground,
            ..
        } => {
            for n in [degrees, range, angle_step, aligned_spacing] {
                push(n);
            }
            if let Some(n) = drop_to_ground {
                push(n);
            }
        }
        Generator::Annulus {
            inner_radius,
            outer_radius,
            rings,
            points_per_ring,
            arc,
            spiral,
            drop_to_ground,
            ..
        } => {
            for n in [inner_radius, outer_radius, rings, points_per_ring] {
                push(n);
            }
            for n in [spiral, drop_to_ground].into_iter().flatten() {
                push(n);
            }
            if let Some(a) = arc {
                push(&a.degrees);
            }
        }
        Generator::Lattice {
            half_size,
            spacing,
            drop_to_ground,
            ..
        } => {
            push(half_size);
            push(spacing);
            if let Some(n) = drop_to_ground {
                push(n);
            }
        }
        Generator::ReachLattice {
            half_size,
            spacing,
            reach_limit,
            drop_to_ground,
            ..
        } => {
            push(half_size);
            push(spacing);
            push(reach_limit);
            if let Some(n) = drop_to_ground {
                push(n);
            }
        }
        Generator::Union { children, .. } => {
            for child in children {
                generator_params(child, out);
            }
        }
        Generator::Custom {
            args, drop_to_ground, ..
        } => {
            args.values().for_each(&mut push);
            if let Some(n) = drop_to_ground {
                push(n);
            }
        }
    }
}

/// A `.spatialquery.ron` asset (§ 3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpatialQueryAsset {
    /// [`SPATIAL_QUERY_FORMAT_VERSION`].
    pub format_version: u32,
    /// The template name requests use.
    pub name: String,
    /// The item type.
    pub item_type: ItemType,
    /// A client-only package may omit the asset. Stored and ignored until an authority-role build
    /// profile exists (ADR-1585).
    #[serde(default)]
    pub server_only: bool,
    /// The pick used when a request names none.
    #[serde(default)]
    pub default_pick: Pick,
    /// Declared parameters and their defaults.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
    /// Half-extents of the box navigation-mesh lookups search.
    #[serde(default = "default_extents")]
    pub extents: Vec3,
    /// The generator.
    pub generator: Generator,
    /// The tests, in authoring order.
    #[serde(default)]
    pub tests: Vec<Test>,
}

/// Why a `.spatialquery.ron` text did not load. Every variant names a 1-based line and column.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SpatialQueryLoadError {
    /// Bad RON, an unknown field or variant, a bad value, a duplicate test id, an unsupported
    /// version, too many tests (`sq_unreadable`).
    #[error("{line}:{col}: {message}")]
    Unreadable {
        /// Line.
        line: usize,
        /// Column.
        col: usize,
        /// What is wrong.
        message: String,
    },
    /// A generator child or a test does not fit the item type (`sq_item_type_mismatch`).
    #[error("{line}:{col}: {message}")]
    ItemTypeMismatch {
        /// Line.
        line: usize,
        /// Column.
        col: usize,
        /// What is wrong.
        message: String,
    },
    /// A `FilterBound` clamp on a test without that filter bound (`sq_no_threshold`).
    #[error("{line}:{col}: test `{test}` clamps to a filter bound it does not set")]
    NoThreshold {
        /// Line.
        line: usize,
        /// Column.
        col: usize,
        /// The test id.
        test: String,
    },
}

impl SpatialQueryLoadError {
    /// The agent error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreadable { .. } => "sq_unreadable",
            Self::ItemTypeMismatch { .. } => "sq_item_type_mismatch",
            Self::NoThreshold { .. } => "sq_no_threshold",
        }
    }

    /// The (line, column) of the error.
    pub fn position(&self) -> (usize, usize) {
        match self {
            Self::Unreadable { line, col, .. }
            | Self::ItemTypeMismatch { line, col, .. }
            | Self::NoThreshold { line, col, .. } => (*line, *col),
        }
    }
}

/// The 1-based (line, column) of the `nth` occurrence of `needle` in `text`, or (1, 1).
fn locate(text: &str, needle: &str, nth: usize) -> (usize, usize) {
    let Some((offset, _)) = text.match_indices(needle).nth(nth) else {
        return (1, 1);
    };
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

impl SpatialQueryAsset {
    /// Parses and validates a `.spatialquery.ron` text with the default test limit.
    ///
    /// # Errors
    ///
    /// See [`SpatialQueryLoadError`].
    pub fn parse(text: &str) -> Result<Self, SpatialQueryLoadError> {
        Self::parse_with_limit(text, DEFAULT_MAX_TESTS)
    }

    /// Parses and validates with at most `max_tests` tests.
    ///
    /// # Errors
    ///
    /// See [`SpatialQueryLoadError`].
    pub fn parse_with_limit(text: &str, max_tests: usize) -> Result<Self, SpatialQueryLoadError> {
        // Read the version first so a future version reports "unsupported" rather than a shape error.
        let options = ron_options();
        if let Ok(header) = options.from_str::<IgnoreRest>(text)
            && !(SPATIAL_QUERY_MIN_FORMAT_VERSION..=SPATIAL_QUERY_FORMAT_VERSION)
                .contains(&header.format_version)
        {
            let (line, col) = locate(text, "format_version", 0);
            return Err(SpatialQueryLoadError::Unreadable {
                line,
                col,
                message: format!(
                    "format_version {} is not supported (this build reads {}..={})",
                    header.format_version, SPATIAL_QUERY_MIN_FORMAT_VERSION, SPATIAL_QUERY_FORMAT_VERSION
                ),
            });
        }
        let mut asset: SpatialQueryAsset =
            options.from_str(text).map_err(|e: ron::error::SpannedError| {
                SpatialQueryLoadError::Unreadable {
                    line: e.span.start.line,
                    col: e.span.start.col,
                    message: e.code.to_string(),
                }
            })?;
        asset.validate(text, max_tests)?;
        // Version 2 is a superset of version 1, so a loaded asset is a version 2 asset.
        asset.format_version = SPATIAL_QUERY_FORMAT_VERSION;
        Ok(asset)
    }

    /// Checks the rules RON cannot express (§ 3.1, § 3.4, § 7 item 16). `text` locates errors.
    fn validate(&self, text: &str, max_tests: usize) -> Result<(), SpatialQueryLoadError> {
        let unreadable = |(line, col): (usize, usize), message: String| SpatialQueryLoadError::Unreadable {
            line,
            col,
            message,
        };
        if self.tests.len() > max_tests {
            return Err(unreadable(
                locate(text, "tests", 0),
                format!("{} tests; at most {max_tests}", self.tests.len()),
            ));
        }
        if let Pick::Sample { band } = self.default_pick
            && !(0.0..=1.0).contains(&band)
        {
            return Err(unreadable(
                locate(text, "default_pick", 0),
                format!("Sample band {band} is outside 0..=1"),
            ));
        }
        // The custom rows arrived with version 2: a file claiming version 1 is not what it says.
        if self.format_version < 2
            && (self.generator.uses_custom() || self.tests.iter().any(|t| matches!(t, Test::Custom { .. })))
        {
            return Err(unreadable(
                locate(text, "Custom", 0),
                "Custom generators and tests need format_version 2".to_string(),
            ));
        }
        if self.generator.anchors_use_candidate() {
            return Err(unreadable(
                locate(text, "generator", 0),
                "Candidate is only valid inside tests".to_string(),
            ));
        }
        check_generator_type(&self.generator, self.item_type, true, text)?;
        check_ring_spacing(&self.generator, text)?;

        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for test in &self.tests {
            let id = test.id();
            let needle = format!("\"{id}\"");
            let occurrence = seen.entry(id).or_insert(0);
            let at = locate(text, &needle, *occurrence);
            *occurrence += 1;
            if *occurrence > 1 {
                return Err(unreadable(at, format!("duplicate test id `{id}`")));
            }
            if id.is_empty() {
                return Err(unreadable(at, "a test id must not be empty".to_string()));
            }
            if !test.accepts(self.item_type) {
                return Err(SpatialQueryLoadError::ItemTypeMismatch {
                    line: at.0,
                    col: at.1,
                    message: format!(
                        "test `{id}` ({}) does not accept {} items",
                        test.kind(),
                        self.item_type.name()
                    ),
                });
            }
            match test {
                Test::Snap { purpose, .. } if *purpose != Purpose::Filter => {
                    return Err(unreadable(
                        at,
                        format!("Snap test `{id}` must have purpose Filter"),
                    ));
                }
                Test::Clearance {
                    shape: CastShape::Line,
                    ..
                } => {
                    return Err(unreadable(
                        at,
                        format!("Clearance test `{id}` needs a volume, not Line"),
                    ));
                }
                Test::Custom { name, .. } if name.is_empty() => {
                    return Err(unreadable(at, format!("Custom test `{id}` needs a name")));
                }
                _ => {}
            }
            let common = test.common();
            if let Some(score) = common.score
                && test.is_float()
            {
                let needs_min = matches!(score.clamp_min, Clamp::FilterBound) && common.min.is_none();
                let needs_max = matches!(score.clamp_max, Clamp::FilterBound) && common.max.is_none();
                if needs_min || needs_max {
                    return Err(SpatialQueryLoadError::NoThreshold {
                        line: at.0,
                        col: at.1,
                        test: id.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Every parameter name a numeric field reads, sorted.
    pub fn used_params(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        generator_params(&self.generator, &mut out);
        for test in &self.tests {
            test.params(&mut out);
        }
        out
    }

    /// Serializes the asset as pretty RON.
    ///
    /// # Errors
    ///
    /// RON serialization failed.
    pub fn to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
    }
}

/// A `Ring` needs exactly one of `count` and `gap`; a `Custom` generator needs a name.
fn check_ring_spacing(generator: &Generator, text: &str) -> Result<(), SpatialQueryLoadError> {
    match generator {
        Generator::Custom { name, .. } if name.is_empty() => {
            let (line, col) = locate(text, "Custom", 0);
            Err(SpatialQueryLoadError::Unreadable {
                line,
                col,
                message: "a Custom generator needs a name".to_string(),
            })
        }
        Generator::Ring { gap, count, .. } if gap.is_some() == count.is_some() => {
            let (line, col) = locate(text, "Ring", 0);
            Err(SpatialQueryLoadError::Unreadable {
                line,
                col,
                message: "a Ring takes exactly one of `count` and `gap`".to_string(),
            })
        }
        Generator::Union { children, .. } => children.iter().try_for_each(|c| check_ring_spacing(c, text)),
        _ => Ok(()),
    }
}

fn check_generator_type(
    generator: &Generator,
    declared: ItemType,
    top: bool,
    text: &str,
) -> Result<(), SpatialQueryLoadError> {
    let mismatch = |message: String| {
        let (line, col) = locate(text, generator.kind(), 0);
        SpatialQueryLoadError::ItemTypeMismatch { line, col, message }
    };
    if let Generator::Union {
        children,
        allow_mixed,
    } = generator
    {
        for child in children {
            let out = child.output(declared);
            let flattenable = *allow_mixed && declared == ItemType::Point && out == ItemType::Entity;
            if out != declared && !flattenable {
                let (line, col) = locate(text, child.kind(), 0);
                return Err(SpatialQueryLoadError::ItemTypeMismatch {
                    line,
                    col,
                    message: format!(
                        "Union child {} produces {} items in a {} query{}",
                        child.kind(),
                        out.name(),
                        declared.name(),
                        if declared == ItemType::Point {
                            " (set allow_mixed: true to flatten entities to positions)"
                        } else {
                            ""
                        }
                    ),
                });
            }
            if let Generator::Union { .. } = child {
                check_generator_type(child, declared, false, text)?;
            }
        }
        return Ok(());
    }
    let out = generator.output(declared);
    if top && out != declared {
        return Err(mismatch(format!(
            "generator {} produces {} items, the query declares {}",
            generator.kind(),
            out.name(),
            declared.name()
        )));
    }
    Ok(())
}

/// Reads only `format_version` and ignores every other field.
#[derive(Deserialize)]
struct IgnoreRest {
    format_version: u32,
}
