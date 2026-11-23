// SPDX-License-Identifier: MPL-2.0
//! Motion database files (`docs/specs/52-motion-matching.md` § 4): the `.motiondb.ron` source an
//! author writes, and the cooked payload a cook step stores in a `.fckd` file.
//!
//! The source names joints and clips; [`MotionDatabaseSource::build`] resolves them against a
//! skeleton and a clip lookup, builds the database and bakes the tags, spans, end trims and
//! distance-matched flags.
//!
//! The cooked payload is postcard, little-endian, with `format_version` (a `u32`) as its first
//! field, then the resolved schema, clip ranges, frame refs, per-frame tags / bias / flags, raw
//! features, per-dimension mean and scale, the k-d tree arrays, tag counts, warnings and a
//! sample of 16 normalized rows; a 64-bit FNV-1a hash of all of that follows as the last eight
//! bytes. Normalized features are not stored: the loader recomputes them with the build's own
//! `(raw − mean) · scale` code (no fused operations, so bit-exact), then checks the hash and
//! the stored sample rows. Any mismatch is [`MotionMatchError::CorruptDatabase`].
//!
//! Version 2 (ADR-2858) adds the optional contact-label settings and the contact and phase
//! group weights (default 0). A version-1 source is a valid version-2 source with neither, so
//! [`migrate_motiondb`] only rewrites the version; a version-1 cooked payload still loads (its
//! schema has no contacts) and gives the same database bit for bit.
//!
//! Cooked version 3 (ADR-7817) adds the optional arrays of spec 52 § 4 after the k-d tree: the
//! arc-length arrays of each distance-matched clip (cumulative ground distance and neighbor
//! ground speed per frame) and the per-frame clip speed. The runtime reads them when present
//! and computes the same values when absent (the clip speeds at load, from the stored raw
//! features; the arc lengths on first use, from the clips). [`migrate_cooked_motiondb`]
//! rewrites a version-1 or version-2 payload at version 3 with the clip speeds (the arc
//! lengths need the clips: [`MotionDatabase::bake_arc_lengths`]).

// Source: the layout is this engine's own (spec 52 § 4); FNV-1a is Fowler, Noll and Vo's public
// domain hash. Original code; nothing ported.

use serde::{Deserialize, Serialize};

use super::kd::KdParts;
use super::warp::ArcLengthArrays;
use super::{
    BuildWarning, ClipAnnotation, ClipRange, ContactSettings, FeatureSchema, FeatureWeights, Fnv1a,
    FrameMeta, FrameRef, KdTree, MAX_TAGS, MirrorTable, MotionDatabase, MotionEntry, MotionMatchError,
    NormalizationMode, PoseJoint, SampleWeights, Span, SpanKind, TagTable, normalize_in_place,
};
use crate::clip::AnimationClip;
use crate::skeleton::Skeleton;

/// The `.motiondb.ron` source format version.
pub const MOTIONDB_FORMAT_VERSION: u32 = 2;

/// The cooked motion-database payload version (distinct from the cooked container's version).
/// Version 3 (ADR-7817) adds the optional arc-length and clip-speed arrays.
pub const COOKED_MOTIONDB_VERSION: u32 = 3;

/// Normalized rows the cooked payload stores to verify the loader's recomputation.
const SAMPLE_ROWS: usize = 16;

fn default_forward() -> [f32; 3] {
    [0.0, 0.0, 1.0]
}

fn default_trajectory_times() -> Vec<f32> {
    super::DEFAULT_TRAJECTORY_TIMES.to_vec()
}

fn default_sample_rate() -> f32 {
    super::DEFAULT_SAMPLE_RATE
}

fn one() -> f32 {
    1.0
}

fn zero() -> f32 {
    0.0
}

fn yes() -> bool {
    true
}

/// Normalization mode as written in the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizationSource {
    /// [`NormalizationMode::GroupDeviation`].
    #[default]
    GroupDeviation,
    /// [`NormalizationMode::PerDimension`].
    PerDimension,
    /// [`NormalizationMode::None`].
    None,
}

impl From<NormalizationSource> for NormalizationMode {
    fn from(value: NormalizationSource) -> Self {
        match value {
            NormalizationSource::GroupDeviation => NormalizationMode::GroupDeviation,
            NormalizationSource::PerDimension => NormalizationMode::PerDimension,
            NormalizationSource::None => NormalizationMode::None,
        }
    }
}

/// Group weights as written in the source (each defaults to 1).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeightsSource {
    /// Root velocity.
    #[serde(default = "one")]
    pub root_velocity: f32,
    /// Future trajectory positions.
    #[serde(default = "one")]
    pub trajectory_position: f32,
    /// Future facing directions.
    #[serde(default = "one")]
    pub trajectory_direction: f32,
    /// Foot positions.
    #[serde(default = "one")]
    pub foot_position: f32,
    /// Foot velocities.
    #[serde(default = "one")]
    pub foot_velocity: f32,
    /// Past positions.
    #[serde(default = "one")]
    pub trajectory_history: f32,
    /// Extra joint positions.
    #[serde(default = "one")]
    pub pose_position: f32,
    /// Extra joint velocities.
    #[serde(default = "one")]
    pub pose_velocity: f32,
    /// Foot contact labels (v2; default 0).
    #[serde(default = "zero")]
    pub foot_contact: f32,
    /// Foot gait phase (v2; default 0).
    #[serde(default = "zero")]
    pub foot_phase: f32,
}

impl Default for WeightsSource {
    fn default() -> Self {
        let w = FeatureWeights::default();
        Self {
            root_velocity: w.root_velocity,
            trajectory_position: w.trajectory_position,
            trajectory_direction: w.trajectory_direction,
            foot_position: w.foot_position,
            foot_velocity: w.foot_velocity,
            trajectory_history: w.trajectory_history,
            pose_position: w.pose_position,
            pose_velocity: w.pose_velocity,
            foot_contact: w.foot_contact,
            foot_phase: w.foot_phase,
        }
    }
}

impl From<WeightsSource> for FeatureWeights {
    fn from(w: WeightsSource) -> Self {
        FeatureWeights {
            root_velocity: w.root_velocity,
            trajectory_position: w.trajectory_position,
            trajectory_direction: w.trajectory_direction,
            foot_position: w.foot_position,
            foot_velocity: w.foot_velocity,
            trajectory_history: w.trajectory_history,
            pose_position: w.pose_position,
            pose_velocity: w.pose_velocity,
            foot_contact: w.foot_contact,
            foot_phase: w.foot_phase,
        }
    }
}

/// An extra pose joint, by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PoseJointSource {
    /// Joint name.
    pub joint: String,
    /// Position feature (default true).
    #[serde(default = "yes")]
    pub position: bool,
    /// Velocity feature (default true).
    #[serde(default = "yes")]
    pub velocity: bool,
    /// Weight (default 1).
    #[serde(default = "one")]
    pub weight: f32,
}

/// Per-sample weights as written in the source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SampleWeightsSource {
    /// One per trajectory time (empty = all 1).
    #[serde(default)]
    pub trajectory: Vec<f32>,
    /// One per history time (empty = all 1).
    #[serde(default)]
    pub history: Vec<f32>,
}

/// The schema as written in the source: joints by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaSource {
    /// The root (character-frame) joint.
    pub root_joint: String,
    /// The root's local forward axis (default `(0, 0, 1)`).
    #[serde(default = "default_forward")]
    pub forward: [f32; 3],
    /// Foot joints.
    #[serde(default)]
    pub feet: Vec<String>,
    /// Future sample times, seconds.
    #[serde(default = "default_trajectory_times")]
    pub trajectory_times: Vec<f32>,
    /// Past sample times, seconds.
    #[serde(default)]
    pub history_times: Vec<f32>,
    /// Frames per second (default 30).
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f32,
    /// Group weights.
    #[serde(default)]
    pub weights: WeightsSource,
    /// Normalization mode.
    #[serde(default)]
    pub normalization: NormalizationSource,
    /// Extra pose joints.
    #[serde(default)]
    pub pose_joints: Vec<PoseJointSource>,
    /// Per-sample weights.
    #[serde(default)]
    pub sample_weights: SampleWeightsSource,
    /// Foot contact labels and gait phase (v2; `None` = neither group).
    #[serde(default)]
    pub contacts: Option<ContactSettings>,
}

/// A span kind as written in the source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKindSource {
    /// Played through, never a jump target.
    PassThrough,
    /// Not indexed.
    Unindexed,
    /// Added to the cost as a jump target, `[-100, 100]`.
    LandingBias(f32),
    /// Tag names ORed into the frames' tags.
    Tag(Vec<String>),
}

/// A span as written in the source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpanSource {
    /// What it does.
    pub kind: SpanKindSource,
    /// Half-open `[start, end)`, clip seconds.
    pub range: (f32, f32),
}

/// One clip entry as written in the source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntrySource {
    /// Clip asset path.
    pub clip: String,
    /// The clip loops.
    #[serde(default)]
    pub looping: bool,
    /// Tag names of every frame.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Add the entry a second time mirrored (needs [`MotionDatabaseSource::mirror_pairs`];
    /// without them the build fails with [`MotionMatchError::NoMirrorTable`]).
    #[serde(default)]
    pub mirrored: bool,
    /// Seconds at the start and end that are not jump targets (still played and sampled).
    #[serde(default)]
    pub trim: (f32, f32),
    /// Spans over clip time.
    #[serde(default)]
    pub spans: Vec<SpanSource>,
    /// A stop, pivot or landing played by distance (the player's distance matching).
    #[serde(default)]
    pub distance_matched: bool,
}

/// A `.motiondb.ron` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MotionDatabaseSource {
    /// [`MOTIONDB_FORMAT_VERSION`].
    #[serde(default)]
    pub format_version: u32,
    /// Skeleton asset path.
    pub skeleton: String,
    /// The schema.
    pub schema: SchemaSource,
    /// Tag names, in bit order (at most 32).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Left/right joint name pairs for mirrored entries (lateral axis X).
    #[serde(default)]
    pub mirror_pairs: Vec<(String, String)>,
    /// Clip entries, in database order.
    pub entries: Vec<EntrySource>,
}

/// `from` versions of the registered `.motiondb.ron` migrations (v1 → v2).
#[must_use]
pub fn motiondb_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Migrate `.motiondb.ron` text of any supported version to the current one (v1 → v2 adds
/// nothing but the version: the contact settings and the new weights are optional with
/// defaults that leave the database unchanged). `regia upgrade-project` calls this.
///
/// # Errors
///
/// The errors of [`MotionDatabaseSource::from_ron_str`] and
/// [`MotionDatabaseSource::to_ron_string`].
pub fn migrate_motiondb(text: &str) -> Result<String, MotionMatchError> {
    MotionDatabaseSource::from_ron_str(text)?.to_ron_string()
}

/// The serde field-list fingerprint of `.motiondb.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn motiondb_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<MotionDatabaseSource>(),
        serde_field_names::<SchemaSource>(),
        serde_field_names::<WeightsSource>(),
        serde_field_names::<PoseJointSource>(),
        serde_field_names::<SampleWeightsSource>(),
        serde_field_names::<ContactSettings>(),
        serde_field_names::<EntrySource>(),
        serde_field_names::<SpanSource>(),
    ])
}

/// `from` versions the cooked payload loader still reads (`motiondb-cooked` v1 → v2, ADR-2858;
/// v2 → v3, ADR-7817): [`MotionDatabase::from_cooked_payload`] upgrades a v1 payload in memory
/// (the contact settings absent, the two new group weights zero) and loads a v1 or v2 payload
/// without the optional arrays, so an old cache loads as the same database.
/// [`migrate_cooked_motiondb`] rewrites either at the current version. Cooking writes v3.
#[must_use]
pub fn cooked_motiondb_migration_sources() -> Vec<u32> {
    vec![1, 2]
}

/// Rewrite a cooked motion-database payload of any supported version at
/// [`COOKED_MOTIONDB_VERSION`] (`regia upgrade-project`, ADR-7817): the same database, plus the
/// per-frame clip-speed array (computed from the stored raw features). The arc-length arrays
/// need the clips, so a payload without them stays without them (the player samples the clips
/// on first use, with the same result); a re-cook or [`MotionDatabase::bake_arc_lengths`] adds
/// them. A current payload comes back byte for byte.
///
/// # Errors
///
/// The errors of [`MotionDatabase::from_cooked_payload`] and
/// [`MotionDatabase::to_cooked_payload`].
pub fn migrate_cooked_motiondb(payload: &[u8]) -> Result<Vec<u8>, MotionMatchError> {
    // The loader fills a missing clip-speed array from the raw features.
    MotionDatabase::from_cooked_payload(payload)?.to_cooked_payload()
}

/// The serde field-list fingerprint of the cooked database payload (`motiondb-cooked`, for
/// `cargo xtask check-formats`). A cache: a shape change bumps [`COOKED_MOTIONDB_VERSION`] and
/// cooked files are cooked again; the loader also reads the versions in
/// [`cooked_motiondb_migration_sources`] (spec 52 § 4).
#[must_use]
pub fn cooked_motiondb_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<CookedBody<CookedSchema, CookedPaths>>(),
        serde_field_names::<CookedSchema>(),
        serde_field_names::<CookedClip>(),
        serde_field_names::<KdParts>(),
        serde_field_names::<CookedPaths>(),
        serde_field_names::<CookedArcLengths>(),
    ])
}

impl MotionDatabaseSource {
    /// Parse `.motiondb.ron` text (a newer `format_version` is refused; an older one is read
    /// as the current version, which only adds optional fields).
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::UnsupportedFormatVersion`] or [`MotionMatchError::BadSource`].
    pub fn from_ron_str(text: &str) -> Result<Self, MotionMatchError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| MotionMatchError::BadSource(format!("cannot read format_version: {e}")))?;
        if version > MOTIONDB_FORMAT_VERSION {
            return Err(MotionMatchError::UnsupportedFormatVersion {
                found: version,
                supported: MOTIONDB_FORMAT_VERSION,
            });
        }
        let mut source: MotionDatabaseSource =
            ron::from_str(text).map_err(|e| MotionMatchError::BadSource(e.to_string()))?;
        source.format_version = MOTIONDB_FORMAT_VERSION;
        Ok(source)
    }

    /// Write the source at the current version.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::BadSource`] when serialization fails.
    pub fn to_ron_string(&self) -> Result<String, MotionMatchError> {
        let mut source = self.clone();
        source.format_version = MOTIONDB_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        ron::ser::to_string_pretty(&source, pretty).map_err(|e| MotionMatchError::BadSource(e.to_string()))
    }

    /// The schema with joint names resolved against `skeleton`.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::UnknownJoint`] for a name the skeleton lacks.
    pub fn resolve_schema(&self, skeleton: &Skeleton) -> Result<FeatureSchema, MotionMatchError> {
        let joint = |name: &str| {
            skeleton
                .joint_index(name)
                .ok_or_else(|| MotionMatchError::UnknownJoint(name.to_owned()))
        };
        let s = &self.schema;
        let mut schema = FeatureSchema::new(
            joint(&s.root_joint)?,
            s.feet.iter().map(|f| joint(f)).collect::<Result<_, _>>()?,
        );
        schema.forward = regia_core::math::Vec3::from_array(s.forward);
        schema.trajectory_times = s.trajectory_times.clone();
        schema.history_times = s.history_times.clone();
        schema.sample_rate = s.sample_rate;
        schema.weights = s.weights.into();
        schema.normalization = s.normalization.into();
        schema.sample_weights = SampleWeights {
            trajectory: s.sample_weights.trajectory.clone(),
            history: s.sample_weights.history.clone(),
        };
        schema.contacts = s.contacts;
        schema.pose_joints = s
            .pose_joints
            .iter()
            .map(|p| {
                Ok(PoseJoint {
                    joint: joint(&p.joint)?,
                    position: p.position,
                    velocity: p.velocity,
                    weight: p.weight,
                })
            })
            .collect::<Result<_, MotionMatchError>>()?;
        Ok(schema)
    }

    /// Build the database: resolve the schema, look up every entry's clip with `clips`, build,
    /// then bake the tags, spans and end trims (a trim is a pass-through span at the clip's
    /// ends) and the distance-matched flags.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::UnknownJoint`], [`MotionMatchError::UnknownClip`],
    /// [`MotionMatchError::NoMirrorTable`], [`MotionMatchError::UnknownTag`],
    /// [`MotionMatchError::BadSpan`] and the errors of [`MotionDatabase::build`].
    pub fn build<'c>(
        &self,
        skeleton: &Skeleton,
        clips: impl Fn(&str) -> Option<&'c AnimationClip>,
    ) -> Result<MotionDatabase, MotionMatchError> {
        let schema = self.resolve_schema(skeleton)?;
        let table = TagTable::new(self.tags.clone()).map_err(|e| MotionMatchError::BadSpan {
            entry: 0,
            reason: e.to_string(),
        })?;
        let joint = |name: &str| {
            skeleton
                .joint_index(name)
                .ok_or_else(|| MotionMatchError::UnknownJoint(name.to_owned()))
        };
        let mirror = MirrorTable {
            pairs: self
                .mirror_pairs
                .iter()
                .map(|(a, b)| Ok((joint(a)?, joint(b)?)))
                .collect::<Result<_, MotionMatchError>>()?,
        };
        let mut sources = Vec::with_capacity(self.entries.len());
        let mut annotations = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.mirrored && mirror.pairs.is_empty() {
                return Err(MotionMatchError::NoMirrorTable { entry: index });
            }
            let clip = clips(&entry.clip).ok_or_else(|| MotionMatchError::UnknownClip {
                entry: index,
                clip: entry.clip.clone(),
            })?;
            let tag = |names: &[String]| {
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                table.mask(&refs).map_err(|_| MotionMatchError::UnknownTag {
                    entry: index,
                    tag: names
                        .iter()
                        .find(|n| table.mask(&[n.as_str()]).is_err())
                        .cloned()
                        .unwrap_or_default(),
                })
            };
            let mut spans = Vec::with_capacity(entry.spans.len() + 2);
            for span in &entry.spans {
                let kind = match &span.kind {
                    SpanKindSource::PassThrough => SpanKind::PassThrough,
                    SpanKindSource::Unindexed => SpanKind::Unindexed,
                    SpanKindSource::LandingBias(b) => SpanKind::LandingBias(*b),
                    SpanKindSource::Tag(names) => SpanKind::Tag(tag(names)?),
                };
                spans.push(Span {
                    start: span.range.0,
                    end: span.range.1,
                    kind,
                });
            }
            let (head, tail) = entry.trim;
            if !(head.is_finite() && tail.is_finite() && head >= 0.0 && tail >= 0.0) {
                return Err(MotionMatchError::BadSpan {
                    entry: index,
                    reason: format!("trim ({head}, {tail}) must be finite and not negative"),
                });
            }
            if head > 0.0 {
                spans.push(Span {
                    start: 0.0,
                    end: head,
                    kind: SpanKind::PassThrough,
                });
            }
            if tail > 0.0 {
                spans.push(Span {
                    start: (clip.duration - tail).max(0.0),
                    end: clip.duration + 1.0,
                    kind: SpanKind::PassThrough,
                });
            }
            let annotation = ClipAnnotation {
                tags: tag(&entry.tags)?,
                spans,
            };
            if entry.mirrored {
                // The mirrored copy is the next range and carries the same tags and spans.
                annotations.push(annotation.clone());
            }
            annotations.push(annotation);
            sources.push(MotionEntry {
                clip,
                looping: entry.looping,
                mirrored: entry.mirrored,
            });
        }
        let mirror = (!mirror.pairs.is_empty()).then_some(&mirror);
        let mut database = MotionDatabase::build_entries(skeleton, &sources, schema, mirror)?;
        let entry_clips: Vec<&AnimationClip> = sources.iter().map(|s| s.clip).collect();
        let annotated =
            annotations.iter().any(|a| a.tags != 0 || !a.spans.is_empty()) || !table.names().is_empty();
        if annotated {
            database.annotate(table, &annotations).map_err(|e| {
                let entry = match &e {
                    super::TagError::BadSpan { .. } | super::TagError::BadBias { .. } => annotations
                        .iter()
                        .position(|a| super::validate_spans(&a.spans).is_err())
                        .and_then(|range| database.clips.get(range))
                        .map_or(0, |r| r.source),
                    _ => 0,
                };
                MotionMatchError::BadSpan {
                    entry,
                    reason: e.to_string(),
                }
            })?;
        }
        for range in &mut database.clips {
            range.distance_matched = self.entries.get(range.source).is_some_and(|e| e.distance_matched);
        }
        database.bake_arc_lengths_with(skeleton, |entry| entry_clips.get(entry).copied())?;
        Ok(database)
    }
}

// ---- cooked payload -------------------------------------------------------------------------------

/// The version-1 cooked schema (still loaded: no contacts, eight weights).
#[derive(Serialize, Deserialize)]
struct CookedSchemaV1 {
    root_joint: u32,
    forward: [f32; 3],
    feet: Vec<u32>,
    trajectory_times: Vec<f32>,
    history_times: Vec<f32>,
    pose_joints: Vec<(u32, bool, bool, f32)>,
    sample_weights_trajectory: Vec<f32>,
    sample_weights_history: Vec<f32>,
    weights: [f32; 8],
    sample_rate: f32,
    normalization: u8,
}

/// The version-2 cooked schema: version 1 plus ten weights and the contact settings
/// (`height_on`, `height_off`, `speed_on`, `speed_off`, `phase`).
#[derive(Serialize, Deserialize)]
struct CookedSchema {
    root_joint: u32,
    forward: [f32; 3],
    feet: Vec<u32>,
    trajectory_times: Vec<f32>,
    history_times: Vec<f32>,
    pose_joints: Vec<(u32, bool, bool, f32)>,
    sample_weights_trajectory: Vec<f32>,
    sample_weights_history: Vec<f32>,
    weights: [f32; 10],
    sample_rate: f32,
    normalization: u8,
    contacts: Option<(f32, f32, f32, f32, bool)>,
}

impl CookedSchemaV1 {
    fn upgrade(self) -> CookedSchema {
        let [rv, tp, td, fp, fv, th, pp, pv] = self.weights;
        CookedSchema {
            root_joint: self.root_joint,
            forward: self.forward,
            feet: self.feet,
            trajectory_times: self.trajectory_times,
            history_times: self.history_times,
            pose_joints: self.pose_joints,
            sample_weights_trajectory: self.sample_weights_trajectory,
            sample_weights_history: self.sample_weights_history,
            weights: [rv, tp, td, fp, fv, th, pp, pv, 0.0, 0.0],
            sample_rate: self.sample_rate,
            normalization: self.normalization,
            contacts: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CookedClip {
    name: String,
    duration: f32,
    looping: bool,
    start: u32,
    frames: u32,
    distance_matched: bool,
    source: u32,
    mirrored: bool,
}

/// The arc-length arrays of one distance-matched clip (cooked v3, ADR-7817).
#[derive(Serialize, Deserialize)]
struct CookedArcLengths {
    clip: u32,
    distances: Vec<f32>,
    speeds: Vec<f32>,
}

/// The optional arrays of cooked v3 (ADR-7817): empty lists mean "not stored".
#[derive(Serialize, Deserialize, Default)]
struct CookedPaths {
    arc_lengths: Vec<CookedArcLengths>,
    clip_speed: Vec<f32>,
}

/// The cooked body; `S` is the schema of the payload's version and `P` its optional arrays
/// (`()` before v3, which postcard writes as nothing, so v1 and v2 bytes are unchanged).
#[derive(Serialize, Deserialize)]
struct CookedBody<S, P> {
    format_version: u32,
    schema: S,
    clips: Vec<CookedClip>,
    frames: Vec<(u32, f32)>,
    tag_names: Vec<String>,
    meta_tags: Vec<u32>,
    meta_bias: Vec<f32>,
    meta_flags: Vec<u8>,
    raw: Vec<f32>,
    mean: Vec<f32>,
    scale: Vec<f32>,
    kd: KdParts,
    paths: P,
    tag_counts: Vec<u32>,
    target_count: u64,
    min_bias: f32,
    warnings: Vec<(u32, u8)>,
    sample_rows: Vec<u32>,
    sample_values: Vec<f32>,
    mirror: Vec<u32>,
}

impl<S, P> CookedBody<S, P> {
    /// The same body at `format_version` with its schema and arrays converted.
    fn convert<S2, P2>(
        self,
        format_version: u32,
        schema: impl FnOnce(S) -> S2,
        paths: impl FnOnce(P) -> P2,
    ) -> CookedBody<S2, P2> {
        CookedBody {
            format_version,
            schema: schema(self.schema),
            clips: self.clips,
            frames: self.frames,
            tag_names: self.tag_names,
            meta_tags: self.meta_tags,
            meta_bias: self.meta_bias,
            meta_flags: self.meta_flags,
            raw: self.raw,
            mean: self.mean,
            scale: self.scale,
            kd: self.kd,
            paths: paths(self.paths),
            tag_counts: self.tag_counts,
            target_count: self.target_count,
            min_bias: self.min_bias,
            warnings: self.warnings,
            sample_rows: self.sample_rows,
            sample_values: self.sample_values,
            mirror: self.mirror,
        }
    }
}

/// Postcard bytes of `body` followed by their FNV-1a hash.
fn hashed_payload<T: Serialize>(body: &T) -> Result<Vec<u8>, MotionMatchError> {
    let mut bytes = postcard::to_allocvec(body).map_err(|e| corrupt(e.to_string()))?;
    let mut hash = Fnv1a::new();
    hash.write(&bytes);
    bytes.extend_from_slice(&hash.finish().to_le_bytes());
    Ok(bytes)
}

fn normalization_code(mode: NormalizationMode) -> u8 {
    match mode {
        NormalizationMode::GroupDeviation => 0,
        NormalizationMode::PerDimension => 1,
        NormalizationMode::None => 2,
    }
}

fn corrupt(what: impl Into<String>) -> MotionMatchError {
    MotionMatchError::CorruptDatabase(what.into())
}

/// The rows the cooked payload samples: 16 evenly spaced frames (all when fewer).
fn sample_rows(frames: usize) -> Vec<u32> {
    if frames <= SAMPLE_ROWS {
        return (0..frames as u32).collect();
    }
    (0..SAMPLE_ROWS)
        .map(|i| (i * (frames - 1) / (SAMPLE_ROWS - 1)) as u32)
        .collect()
}

impl MotionDatabase {
    fn cooked_body(&self) -> CookedBody<CookedSchema, CookedPaths> {
        let s = &self.schema;
        let w = s.weights;
        let dims = self.dimensions();
        let rows = sample_rows(self.frames.len());
        let sample_values = rows
            .iter()
            .flat_map(|&r| {
                self.features[r as usize * dims..(r as usize + 1) * dims]
                    .iter()
                    .copied()
            })
            .collect();
        CookedBody {
            format_version: COOKED_MOTIONDB_VERSION,
            schema: CookedSchema {
                root_joint: s.root_joint,
                forward: s.forward.to_array(),
                feet: s.feet.clone(),
                trajectory_times: s.trajectory_times.clone(),
                history_times: s.history_times.clone(),
                pose_joints: s
                    .pose_joints
                    .iter()
                    .map(|p| (p.joint, p.position, p.velocity, p.weight))
                    .collect(),
                sample_weights_trajectory: s.sample_weights.trajectory.clone(),
                sample_weights_history: s.sample_weights.history.clone(),
                weights: [
                    w.root_velocity,
                    w.trajectory_position,
                    w.trajectory_direction,
                    w.foot_position,
                    w.foot_velocity,
                    w.trajectory_history,
                    w.pose_position,
                    w.pose_velocity,
                    w.foot_contact,
                    w.foot_phase,
                ],
                sample_rate: s.sample_rate,
                normalization: normalization_code(s.normalization),
                contacts: s
                    .contacts
                    .map(|c| (c.height_on, c.height_off, c.speed_on, c.speed_off, c.phase)),
            },
            clips: self
                .clips
                .iter()
                .map(|c| CookedClip {
                    name: c.name.clone(),
                    duration: c.duration,
                    looping: c.looping,
                    start: c.start as u32,
                    frames: c.frames as u32,
                    distance_matched: c.distance_matched,
                    source: c.source as u32,
                    mirrored: c.mirrored,
                })
                .collect(),
            frames: self.frames.iter().map(|f| (f.clip as u32, f.time)).collect(),
            tag_names: self.tags.names().to_vec(),
            meta_tags: self.meta.iter().map(|m| m.tags).collect(),
            meta_bias: self.meta.iter().map(|m| m.bias).collect(),
            meta_flags: self.meta.iter().map(|m| m.flags).collect(),
            raw: self.raw.clone(),
            mean: self.mean.clone(),
            scale: self.scale.clone(),
            kd: self.tree.to_parts(),
            paths: CookedPaths {
                arc_lengths: self
                    .arc_lengths
                    .iter()
                    .enumerate()
                    .filter_map(|(clip, arrays)| {
                        arrays.as_ref().map(|a| CookedArcLengths {
                            clip: clip as u32,
                            distances: a.distances.clone(),
                            speeds: a.speeds.clone(),
                        })
                    })
                    .collect(),
                clip_speed: self.clip_speed.clone(),
            },
            tag_counts: self.tag_counts.to_vec(),
            target_count: self.target_count as u64,
            min_bias: self.min_bias,
            warnings: self
                .warnings
                .iter()
                .map(|w| (w.clip as u32, w.kind.code()))
                .collect(),
            sample_rows: rows,
            sample_values,
            mirror: self.mirror.clone(),
        }
    }

    /// The cooked payload (`docs/specs/52-motion-matching.md` § 4): deterministic, so two
    /// builds of the same inputs give the same bytes.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::CorruptDatabase`] when serialization fails (never for a built
    /// database).
    pub fn to_cooked_payload(&self) -> Result<Vec<u8>, MotionMatchError> {
        hashed_payload(&self.cooked_body())
    }

    /// The payload in the version-1 layout (eight weights, no contacts, no optional arrays):
    /// what a build before ADR-2858 cooked. Tests only; weights beyond the eight must be zero.
    #[cfg(test)]
    pub(crate) fn to_cooked_payload_v1(&self) -> Result<Vec<u8>, MotionMatchError> {
        let b = self.cooked_body();
        if b.schema.contacts.is_some() {
            return Err(corrupt("a database with contacts has no version-1 layout"));
        }
        let v1 = b.convert(
            1,
            |s| {
                let w = s.weights;
                CookedSchemaV1 {
                    root_joint: s.root_joint,
                    forward: s.forward,
                    feet: s.feet,
                    trajectory_times: s.trajectory_times,
                    history_times: s.history_times,
                    pose_joints: s.pose_joints,
                    sample_weights_trajectory: s.sample_weights_trajectory,
                    sample_weights_history: s.sample_weights_history,
                    weights: [w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]],
                    sample_rate: s.sample_rate,
                    normalization: s.normalization,
                }
            },
            |_| (),
        );
        hashed_payload(&v1)
    }

    /// The payload in the version-2 layout (no optional arrays): what a build before ADR-7817
    /// cooked. Tests only.
    #[cfg(test)]
    pub(crate) fn to_cooked_payload_v2(&self) -> Result<Vec<u8>, MotionMatchError> {
        hashed_payload(&self.cooked_body().convert(2, |s| s, |_| ()))
    }

    /// The payload's content hash: 64-bit FNV-1a over the cooked bytes before the hash (the
    /// value a cooked file stores in its last eight bytes).
    pub fn content_hash(&self) -> u64 {
        let mut hash = Fnv1a::new();
        if let Ok(bytes) = postcard::to_allocvec(&self.cooked_body()) {
            hash.write(&bytes);
        }
        hash.finish()
    }

    /// The `format_version` of a cooked payload (after checking its hash).
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::CorruptDatabase`] for a short payload or a hash mismatch.
    pub fn cooked_payload_version(payload: &[u8]) -> Result<u32, MotionMatchError> {
        let body = checked_body(payload)?;
        postcard::take_from_bytes::<u32>(body)
            .map(|(v, _)| v)
            .map_err(|e| corrupt(e.to_string()))
    }

    /// Load a cooked payload: check the hash and the version, rebuild the normalized features
    /// and the tree, and verify the stored sample rows bit for bit.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::CorruptDatabase`] or [`MotionMatchError::UnsupportedFormatVersion`].
    #[tracing::instrument(target = "regia_animation", name = "motion_matching.load", skip_all)]
    pub fn from_cooked_payload(payload: &[u8]) -> Result<MotionDatabase, MotionMatchError> {
        let version = Self::cooked_payload_version(payload)?;
        let bytes = checked_body(payload)?;
        let body: CookedBody<CookedSchema, CookedPaths> = match version {
            1 => {
                let old: CookedBody<CookedSchemaV1, ()> =
                    postcard::from_bytes(bytes).map_err(|e| corrupt(e.to_string()))?;
                old.convert(1, CookedSchemaV1::upgrade, |()| CookedPaths::default())
            }
            2 => {
                let old: CookedBody<CookedSchema, ()> =
                    postcard::from_bytes(bytes).map_err(|e| corrupt(e.to_string()))?;
                old.convert(2, |s| s, |()| CookedPaths::default())
            }
            COOKED_MOTIONDB_VERSION => postcard::from_bytes(bytes).map_err(|e| corrupt(e.to_string()))?,
            found => {
                return Err(MotionMatchError::UnsupportedFormatVersion {
                    found,
                    supported: COOKED_MOTIONDB_VERSION,
                });
            }
        };
        let s = body.schema;
        let normalization = match s.normalization {
            0 => NormalizationMode::GroupDeviation,
            1 => NormalizationMode::PerDimension,
            2 => NormalizationMode::None,
            other => return Err(corrupt(format!("normalization code {other}"))),
        };
        let [rv, tp, td, fp, fv, th, pp, pv, fc, fph] = s.weights;
        let contacts =
            s.contacts.map(
                |(height_on, height_off, speed_on, speed_off, phase)| ContactSettings {
                    height_on,
                    height_off,
                    speed_on,
                    speed_off,
                    phase,
                },
            );
        if let Some(c) = &contacts {
            c.validate().map_err(|e| corrupt(e.to_string()))?;
        }
        let schema = FeatureSchema {
            root_joint: s.root_joint,
            forward: regia_core::math::Vec3::from_array(s.forward),
            feet: s.feet,
            trajectory_times: s.trajectory_times,
            sample_rate: s.sample_rate,
            weights: FeatureWeights {
                root_velocity: rv,
                trajectory_position: tp,
                trajectory_direction: td,
                foot_position: fp,
                foot_velocity: fv,
                trajectory_history: th,
                pose_position: pp,
                pose_velocity: pv,
                foot_contact: fc,
                foot_phase: fph,
            },
            history_times: s.history_times,
            pose_joints: s
                .pose_joints
                .into_iter()
                .map(|(joint, position, velocity, weight)| PoseJoint {
                    joint,
                    position,
                    velocity,
                    weight,
                })
                .collect(),
            sample_weights: SampleWeights {
                trajectory: s.sample_weights_trajectory,
                history: s.sample_weights_history,
            },
            normalization,
            contacts,
        };
        // The cooked schema never meets `FeatureSchema::validate` (it needs a skeleton), so the
        // rate every exclusion window and history step divides or multiplies by is checked here.
        if !schema.sample_rate.is_finite() || schema.sample_rate <= 0.0 {
            return Err(corrupt("the sample rate is not finite and positive"));
        }
        let dims = schema.dimensions();
        let frames = body.frames.len();
        if dims == 0
            || body.raw.len() != frames * dims
            || body.mean.len() != dims
            || body.scale.len() != dims
            || frames == 0
        {
            return Err(corrupt("feature arrays do not match the schema"));
        }
        let clips: Vec<ClipRange> = body
            .clips
            .into_iter()
            .map(|c| ClipRange {
                name: c.name,
                duration: c.duration,
                looping: c.looping,
                start: c.start as usize,
                frames: c.frames as usize,
                distance_matched: c.distance_matched,
                source: c.source as usize,
                mirrored: c.mirrored,
            })
            .collect();
        if clips.iter().any(|c| !c.duration.is_finite() || c.duration < 0.0) {
            return Err(corrupt("a clip range has a non-finite or negative duration"));
        }
        if clips.iter().any(|c| c.mirrored) && body.mirror.is_empty() {
            return Err(corrupt("a mirrored range without a mirror table"));
        }
        if body.mirror.iter().any(|&p| p as usize >= body.mirror.len()) {
            return Err(corrupt("mirror partner out of range"));
        }
        let mut next = 0usize;
        for c in &clips {
            if c.start != next || c.frames == 0 {
                return Err(corrupt("clip ranges are not contiguous"));
            }
            next += c.frames;
        }
        if next != frames {
            return Err(corrupt("clip ranges do not cover the frames"));
        }
        let frame_refs: Vec<FrameRef> = body
            .frames
            .into_iter()
            .map(|(clip, time)| FrameRef {
                clip: clip as usize,
                time,
            })
            .collect();
        if frame_refs
            .iter()
            .any(|f| f.clip >= clips.len() || !f.time.is_finite())
        {
            return Err(corrupt("a frame names a missing clip"));
        }
        let annotated = !body.meta_tags.is_empty();
        if annotated
            && (body.meta_tags.len() != frames
                || body.meta_bias.len() != frames
                || body.meta_flags.len() != frames)
        {
            return Err(corrupt("per-frame tag arrays do not match the frames"));
        }
        let meta: Vec<FrameMeta> = body
            .meta_tags
            .iter()
            .zip(&body.meta_bias)
            .zip(&body.meta_flags)
            .map(|((&tags, &bias), &flags)| FrameMeta { tags, bias, flags })
            .collect();
        let tags = TagTable::new(body.tag_names).map_err(|e| corrupt(e.to_string()))?;
        let mut features = body.raw.clone();
        for values in features.chunks_exact_mut(dims) {
            normalize_in_place(values, &body.mean, &body.scale);
        }
        for (i, &row) in body.sample_rows.iter().enumerate() {
            let row = row as usize;
            let stored = body.sample_values.get(i * dims..(i + 1) * dims);
            let recomputed = features.get(row * dims..(row + 1) * dims);
            let equal = match (stored, recomputed) {
                (Some(a), Some(b)) => a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits()),
                _ => false,
            };
            if !equal {
                return Err(corrupt(format!(
                    "normalized row {row} does not match the stored sample"
                )));
            }
        }
        let tree = KdTree::from_parts(body.kd, features.clone(), dims)
            .ok_or_else(|| corrupt("bad k-d tree arrays"))?;
        let mut tag_counts = [0u32; MAX_TAGS];
        if body.tag_counts.len() != MAX_TAGS {
            return Err(corrupt("tag counts"));
        }
        tag_counts.copy_from_slice(&body.tag_counts);
        let warnings = body
            .warnings
            .into_iter()
            .filter_map(|(clip, code)| BuildWarning::from_code(clip as usize, code))
            .collect();
        let paths = body.paths;
        if !paths.clip_speed.is_empty()
            && (paths.clip_speed.len() != frames
                || paths.clip_speed.iter().any(|v| !v.is_finite() || *v < 0.0))
        {
            return Err(corrupt("the clip-speed array does not match the frames"));
        }
        let mut arc_lengths: Vec<Option<ArcLengthArrays>> = Vec::new();
        for cooked in paths.arc_lengths {
            let clip = cooked.clip as usize;
            let range = clips
                .get(clip)
                .filter(|r| r.distance_matched && clip >= arc_lengths.len())
                .ok_or_else(|| {
                    corrupt("arc lengths of a clip that is not distance matched, or out of order")
                })?;
            let arrays = ArcLengthArrays {
                distances: cooked.distances,
                speeds: cooked.speeds,
            };
            if !arrays.is_valid(range.frames) {
                return Err(corrupt(format!(
                    "the arc-length arrays of clip {clip} are malformed"
                )));
            }
            arc_lengths.resize(clip, None);
            arc_lengths.push(Some(arrays));
        }
        let mut database = MotionDatabase {
            schema,
            clips,
            frames: frame_refs,
            raw: body.raw,
            features,
            mean: body.mean,
            scale: body.scale,
            tree,
            tags,
            meta,
            min_bias: body.min_bias,
            tag_counts,
            target_count: body.target_count as usize,
            median_jump_cost: std::sync::OnceLock::new(),
            force_brute_force: false,
            warnings,
            mirror: body.mirror,
            clip_speed: paths.clip_speed,
            arc_lengths,
        };
        if database.clip_speed.is_empty() {
            // Not stored (a payload before v3): the same values from the stored raw features.
            database.bake_clip_speeds();
        }
        Ok(database)
    }
}

/// The payload body after checking the trailing hash.
fn checked_body(payload: &[u8]) -> Result<&[u8], MotionMatchError> {
    if payload.len() < 9 {
        return Err(corrupt("payload too short"));
    }
    let (body, tail) = payload.split_at(payload.len() - 8);
    let mut stored = [0u8; 8];
    stored.copy_from_slice(tail);
    let mut hash = Fnv1a::new();
    hash.write(body);
    if hash.finish() != u64::from_le_bytes(stored) {
        return Err(corrupt("content hash mismatch"));
    }
    Ok(body)
}
