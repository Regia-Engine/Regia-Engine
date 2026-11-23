// SPDX-License-Identifier: MPL-2.0
//! What the motion-database editor knows about a document without the editor around it: the
//! validator's issues ([`issues`]), the editor's own authoring checks ([`lints`]) and the in-memory
//! rebuild ([`rebuild`]) with its stats.
//!
//! The validator runs the runtime's own checks on the source: the schema through
//! `MotionDatabaseSource::resolve_schema` and `FeatureSchema::validate` against a skeleton made of
//! the joint names the source uses (so every range, count and duplicate rule is the runtime's, and
//! only "the skeleton has this joint" waits for the rebuild), the tag table through `TagTable::new`
//! and every entry's spans through `validate_spans`. The rebuild loads the skeleton and the clips
//! (`MotionDbInputs`, the cook's own resolution) and calls `MotionDatabaseSource::build`, the
//! runtime builder; its `warnings` are that builder's warnings, nothing added. The cooked file is
//! never written.

use std::collections::BTreeMap;

use regia_animation::clip::AnimationClip;
use regia_animation::motion_matching::{
    FLAG_PASS_THROUGH, FLAG_UNINDEXED, FeatureGroup, MotionDatabase, MotionDatabaseSource, MotionMatchError,
    Span, SpanKind, SpanKindSource, TagError, TagTable, validate_spans,
};
use regia_animation::skeleton::Skeleton;
use regia_asset_pipeline::motiondb_cook::{MotionDbInputs, scan_clip_finite};
use serde_json::{Value, json};

use super::ops::weight_of;

/// One finding about the document: an error the runtime would refuse, or an editor check.
#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    /// `error` (the runtime refuses the database) or `check` (an editor authoring check; the
    /// database builds).
    pub severity: &'static str,
    /// Stable code (`bad_weight`, `unknown_tag`, `untagged_clip`, ...).
    pub code: &'static str,
    /// The entry it is about, when it is about one.
    pub entry: Option<usize>,
    /// The span of that entry it is about (a `bad_span` the runtime's `validate_spans` names).
    pub span: Option<usize>,
    /// A sentence for people.
    pub message: String,
}

impl Issue {
    fn error(code: &'static str, entry: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            severity: "error",
            code,
            entry,
            span: None,
            message: message.into(),
        }
    }

    fn check(code: &'static str, entry: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            severity: "check",
            code,
            entry,
            span: None,
            message: message.into(),
        }
    }

    /// The agent form.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "severity": self.severity,
            "code": self.code,
            "entry": self.entry,
            "span": self.span,
            "message": self.message,
        })
    }
}

/// The snake_case code of a runtime motion-matching error.
#[must_use]
pub fn error_code(error: &MotionMatchError) -> &'static str {
    match error {
        MotionMatchError::NoClips => "no_clips",
        MotionMatchError::BadSampleRate(_) => "bad_sample_rate",
        MotionMatchError::BadTrajectoryTime(_) => "bad_trajectory_time",
        MotionMatchError::BadWeight { .. } => "bad_weight",
        MotionMatchError::BadForward(_) => "bad_forward",
        MotionMatchError::JointOutOfRange { .. } => "joint_out_of_range",
        MotionMatchError::BadClipDuration { .. } => "bad_clip_duration",
        MotionMatchError::NonFiniteFeature { .. } => "non_finite_feature",
        MotionMatchError::TooManyFrames(_) => "too_many_frames",
        MotionMatchError::BadHistoryTime(_) => "bad_history_time",
        MotionMatchError::TooManyDimensions { .. } => "too_many_dimensions",
        MotionMatchError::DuplicateJoint(_) => "duplicate_joint",
        MotionMatchError::BadMirrorTable(_) => "bad_mirror_table",
        MotionMatchError::UnknownJoint(_) => "unknown_joint",
        MotionMatchError::UnknownClip { .. } => "unknown_clip",
        MotionMatchError::NoMirrorTable { .. } => "no_mirror_table",
        MotionMatchError::UnknownTag { .. } => "unknown_tag",
        MotionMatchError::BadSpan { .. } => "bad_span",
        MotionMatchError::SampleWeightCount { .. } => "sample_weight_count",
        MotionMatchError::BadContacts(_) => "bad_contacts",
        MotionMatchError::QueryShape { .. }
        | MotionMatchError::NonFiniteQuery
        | MotionMatchError::FrameOutOfRange { .. }
        | MotionMatchError::UnsupportedFormatVersion { .. }
        | MotionMatchError::CorruptDatabase(_)
        | MotionMatchError::BadSource(_) => "bad_source",
    }
}

/// A skeleton with exactly the joints `doc` names (root, feet, pose joints, mirror pairs), in
/// order of first use: what the schema validator needs to check everything but joint existence.
fn names_skeleton(doc: &MotionDatabaseSource) -> Skeleton {
    let mut names: Vec<String> = Vec::new();
    let s = &doc.schema;
    let all = std::iter::once(&s.root_joint)
        .chain(&s.feet)
        .chain(s.pose_joints.iter().map(|p| &p.joint))
        .chain(doc.mirror_pairs.iter().flat_map(|(a, b)| [a, b]));
    for name in all {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    Skeleton {
        joint_names: names,
        ..Skeleton::default()
    }
}

/// The validator's errors: what the runtime would refuse in this source before it looks at the
/// skeleton's joints or the clips. Empty means the source is valid (the rebuild still checks the
/// skeleton and the clips).
#[must_use]
pub fn issues(doc: &MotionDatabaseSource) -> Vec<Issue> {
    let mut out = Vec::new();
    if doc.skeleton.trim().is_empty() {
        out.push(Issue::error(
            "no_skeleton",
            None,
            "the database names no skeleton",
        ));
    }
    let skeleton = names_skeleton(doc);
    match doc.resolve_schema(&skeleton) {
        Ok(schema) => {
            if let Err(e) = schema.validate(&skeleton) {
                out.push(Issue::error(error_code(&e), None, e.to_string()));
            }
        }
        Err(e) => out.push(Issue::error(error_code(&e), None, e.to_string())),
    }
    if doc.entries.is_empty() {
        let e = MotionMatchError::NoClips;
        out.push(Issue::error(error_code(&e), None, e.to_string()));
    }
    let table = match TagTable::new(doc.tags.clone()) {
        Ok(table) => Some(table),
        Err(e) => {
            let code = match e {
                TagError::TooManyTags(_) => "too_many_tags",
                _ => "bad_tag_name",
            };
            out.push(Issue::error(code, None, e.to_string()));
            None
        }
    };
    for (index, entry) in doc.entries.iter().enumerate() {
        if entry.clip.trim().is_empty() {
            out.push(Issue::error(
                "no_clip",
                Some(index),
                format!("entry {index} names no clip"),
            ));
        }
        if entry.mirrored && doc.mirror_pairs.is_empty() {
            let e = MotionMatchError::NoMirrorTable { entry: index };
            out.push(Issue::error(error_code(&e), Some(index), e.to_string()));
        }
        let (head, tail) = entry.trim;
        if !(head.is_finite() && tail.is_finite() && head >= 0.0 && tail >= 0.0) {
            out.push(Issue::error(
                "bad_span",
                Some(index),
                format!("entry {index}: trim ({head}, {tail}) must be finite and not negative"),
            ));
        }
        if let Some(table) = &table {
            let span_tags = entry.spans.iter().flat_map(|s| match &s.kind {
                SpanKindSource::Tag(names) => names.as_slice(),
                _ => &[][..],
            });
            for name in entry.tags.iter().chain(span_tags) {
                if table.mask(&[name.as_str()]).is_err() {
                    let e = MotionMatchError::UnknownTag {
                        entry: index,
                        tag: name.clone(),
                    };
                    out.push(Issue::error(error_code(&e), Some(index), e.to_string()));
                }
            }
        }
        let spans: Vec<Span> = entry
            .spans
            .iter()
            .map(|s| Span {
                start: s.range.0,
                end: s.range.1,
                kind: match s.kind {
                    SpanKindSource::PassThrough => SpanKind::PassThrough,
                    SpanKindSource::Unindexed => SpanKind::Unindexed,
                    SpanKindSource::LandingBias(b) => SpanKind::LandingBias(b),
                    SpanKindSource::Tag(_) => SpanKind::Tag(0),
                },
            })
            .collect();
        if let Err(e) = validate_spans(&spans) {
            let span = match e {
                TagError::BadSpan { index, .. } | TagError::BadBias { index, .. } => Some(index),
                _ => None,
            };
            let mut issue = Issue::error("bad_span", Some(index), format!("entry {index}: {e}"));
            issue.span = span;
            out.push(issue);
        }
    }
    out
}

/// The editor's authoring checks (not runtime errors; the database builds): an entry with no tag
/// (neither a chip nor a tag span) while the table has tags, so no tag filter can ever pick it,
/// and a feature group the layout has whose weight is 0, so it costs nothing.
#[must_use]
pub fn lints(doc: &MotionDatabaseSource) -> Vec<Issue> {
    let mut out = Vec::new();
    if !doc.tags.is_empty() {
        for (index, entry) in doc.entries.iter().enumerate() {
            let spans_tag = entry
                .spans
                .iter()
                .any(|s| matches!(&s.kind, SpanKindSource::Tag(names) if !names.is_empty()));
            if entry.tags.is_empty() && !spans_tag {
                out.push(Issue::check(
                    "untagged_clip",
                    Some(index),
                    format!(
                        "entry {index} ({}) has no tag: a search that requires a tag never picks it",
                        entry.clip
                    ),
                ));
            }
        }
    }
    for group in present_groups(doc) {
        if weight_of(&doc.schema.weights, group) == 0.0 {
            out.push(Issue::check(
                "zero_weight",
                None,
                format!(
                    "{} has weight 0: it is in every vector but costs nothing",
                    group.name()
                ),
            ));
        }
    }
    out
}

/// The feature groups the source's layout has (the five baseline groups always, the others when
/// the schema gives them dimensions).
#[must_use]
pub fn present_groups(doc: &MotionDatabaseSource) -> Vec<FeatureGroup> {
    let s = &doc.schema;
    FeatureGroup::ALL
        .iter()
        .copied()
        .filter(|g| match g {
            FeatureGroup::TrajectoryHistory => !s.history_times.is_empty(),
            FeatureGroup::PosePosition => s.pose_joints.iter().any(|p| p.position),
            FeatureGroup::PoseVelocity => s.pose_joints.iter().any(|p| p.velocity),
            FeatureGroup::FootContact => s.contacts.is_some() && !s.feet.is_empty(),
            FeatureGroup::FootPhase => s.contacts.is_some_and(|c| c.phase) && !s.feet.is_empty(),
            _ => true,
        })
        .collect()
}

/// Frames of one source entry in the rebuilt database.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryCoverage {
    /// Source entry index.
    pub entry: usize,
    /// Clip path.
    pub clip: String,
    /// Frames, the mirrored copy included.
    pub frames: usize,
    /// Frames that may be jump targets (not pass-through, not unindexed).
    pub targets: usize,
    /// Whether a mirrored copy was added.
    pub mirrored: bool,
}

/// The raw value range of one feature group over every frame.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupRange {
    /// Group name.
    pub group: &'static str,
    /// Its weight in the schema.
    pub weight: f32,
    /// Its dimensions.
    pub dims: usize,
    /// Smallest raw value.
    pub min: f32,
    /// Largest raw value.
    pub max: f32,
}

/// A runtime build warning, with the entry it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct WarningLine {
    /// The warning kind (`missing_root_motion`).
    pub kind: &'static str,
    /// Database clip range.
    pub clip: usize,
    /// Its name.
    pub clip_name: String,
    /// The source entry.
    pub entry: usize,
}

/// What an in-memory rebuild found.
#[derive(Clone, Debug, PartialEq)]
pub struct RebuildReport {
    /// Poses (database frames).
    pub frames: usize,
    /// Floats per feature vector.
    pub dimensions: usize,
    /// Clip ranges (an entry added mirrored has two).
    pub ranges: usize,
    /// Frames that may be jump targets.
    pub targets: usize,
    /// Frames with no tag bit.
    pub untagged_frames: usize,
    /// Frames per tag, table order.
    pub tag_frames: Vec<(String, u32)>,
    /// Per source entry.
    pub coverage: Vec<EntryCoverage>,
    /// Per feature group of the layout.
    pub groups: Vec<GroupRange>,
    /// k-d tree nodes, leaves and depth.
    pub kd: (usize, usize, usize),
    /// The runtime builder's warnings (`MotionDatabase::warnings`), unchanged.
    pub warnings: Vec<WarningLine>,
    /// The cooked payload's content hash the same build would store.
    pub content_hash: u64,
}

impl RebuildReport {
    /// Fills the report from a built database.
    #[must_use]
    pub fn of(database: &MotionDatabase, source: &MotionDatabaseSource) -> Self {
        let frames = database.len();
        let mut targets_per_range = vec![0usize; database.clips().len()];
        let mut targets = 0;
        let mut untagged = 0;
        for frame in 0..frames {
            let meta = database.frame_meta(frame);
            let target = meta.is_none_or(|m| m.flags & (FLAG_PASS_THROUGH | FLAG_UNINDEXED) == 0);
            if target {
                targets += 1;
                if let Some(r) = database.frame(frame) {
                    targets_per_range[r.clip] += 1;
                }
            }
            if meta.is_none_or(|m| m.tags == 0) {
                untagged += 1;
            }
        }
        let mut coverage: BTreeMap<usize, EntryCoverage> = BTreeMap::new();
        for (range, clip) in database.clips().iter().enumerate() {
            let row = coverage.entry(clip.source).or_insert_with(|| EntryCoverage {
                entry: clip.source,
                clip: source
                    .entries
                    .get(clip.source)
                    .map_or_else(|| clip.name.clone(), |e| e.clip.clone()),
                frames: 0,
                targets: 0,
                mirrored: false,
            });
            row.frames += clip.frames;
            row.targets += targets_per_range[range];
            row.mirrored |= clip.mirrored;
        }
        let schema = database.schema();
        let groups = FeatureGroup::ALL
            .iter()
            .copied()
            .filter(|&g| !schema.group_range(g).is_empty())
            .map(|g| {
                let range = schema.group_range(g);
                let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
                for frame in 0..frames {
                    if let Some(values) = database.raw_features(frame).and_then(|r| r.get(range.clone())) {
                        for &v in values {
                            min = min.min(v);
                            max = max.max(v);
                        }
                    }
                }
                GroupRange {
                    group: g.name(),
                    weight: schema.weights.get(g),
                    dims: range.len(),
                    min,
                    max,
                }
            })
            .collect();
        let table = database.tag_table();
        let tag_frames = table
            .names()
            .iter()
            .enumerate()
            .map(|(bit, name)| (name.clone(), database.tag_count(bit)))
            .collect();
        let warnings = database
            .warnings()
            .iter()
            .map(|w| {
                let range = database.clips().get(w.clip);
                WarningLine {
                    kind: w.kind.name(),
                    clip: w.clip,
                    clip_name: range.map_or_else(String::new, |r| r.name.clone()),
                    entry: range.map_or(0, |r| r.source),
                }
            })
            .collect();
        let kd = database.kd_stats();
        Self {
            frames,
            dimensions: database.dimensions(),
            ranges: database.clips().len(),
            targets,
            untagged_frames: untagged,
            tag_frames,
            coverage: coverage.into_values().collect(),
            groups,
            kd: (kd.nodes, kd.leaves, kd.depth),
            warnings,
            content_hash: database.content_hash(),
        }
    }

    /// The agent form.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "frames": self.frames,
            "dimensions": self.dimensions,
            "ranges": self.ranges,
            "targets": self.targets,
            "untagged_frames": self.untagged_frames,
            "tag_frames": self.tag_frames.iter().map(|(n, c)| json!({ "name": n, "frames": c })).collect::<Vec<_>>(),
            "coverage": self.coverage.iter().map(|c| json!({
                "entry": c.entry, "clip": c.clip, "frames": c.frames, "targets": c.targets, "mirrored": c.mirrored,
            })).collect::<Vec<_>>(),
            "groups": self.groups.iter().map(|g| json!({
                "group": g.group, "weight": g.weight, "dims": g.dims, "min": g.min, "max": g.max,
            })).collect::<Vec<_>>(),
            "kd": { "nodes": self.kd.0, "leaves": self.kd.1, "depth": self.kd.2 },
            "warnings": self.warnings.iter().map(|w| json!({
                "kind": w.kind, "clip": w.clip, "clip_name": w.clip_name, "entry": w.entry,
            })).collect::<Vec<_>>(),
            "content_hash": format!("{:016x}", self.content_hash),
        })
    }
}

/// A rebuild that did not finish: a stable code (`unknown_skeleton`, `unknown_clip`,
/// `clip_non_finite`, `clip_skeleton`, `build_failed`) and a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildError {
    /// The code.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl RebuildError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Rebuilds `source` in memory: loads the skeleton and every clip through `inputs` (checking
/// `cancelled` before each), scans each clip for non-finite values, then runs the runtime builder.
/// `Ok(None)` when cancelled. Writes nothing.
///
/// # Errors
///
/// [`RebuildError`] with the codes listed on it.
pub fn rebuild(
    source: &MotionDatabaseSource,
    inputs: &mut dyn MotionDbInputs,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(f64, &str),
) -> Result<Option<RebuildReport>, RebuildError> {
    let _span = tracing::info_span!(target: "regia_editor", "mm_db_rebuild").entered();
    progress(0.0, "loading the skeleton");
    let (_, skeleton) = inputs
        .skeleton(&source.skeleton)
        .map_err(|e| RebuildError::new("unknown_skeleton", e))?;
    let mut clips: BTreeMap<String, AnimationClip> = BTreeMap::new();
    let total = source.entries.len().max(1) as f64;
    for (index, entry) in source.entries.iter().enumerate() {
        if cancelled() {
            return Ok(None);
        }
        progress(
            0.1 + 0.6 * index as f64 / total,
            &format!("loading {}", entry.clip),
        );
        if clips.contains_key(&entry.clip) {
            continue;
        }
        let (_, clip) = inputs
            .clip(&entry.clip)
            .map_err(|e| RebuildError::new("unknown_clip", format!("entry {index}: {e}")))?;
        scan_clip_finite(&clip)
            .map_err(|e| RebuildError::new("clip_non_finite", format!("entry {index}: {}", e.message)))?;
        if let Some(track) = clip.tracks.iter().find(|t| t.joint as usize >= skeleton.len()) {
            return Err(RebuildError::new(
                "clip_skeleton",
                format!(
                    "entry {index}: clip {:?} animates joint {} but the skeleton has {} joints",
                    clip.name,
                    track.joint,
                    skeleton.len()
                ),
            ));
        }
        clips.insert(entry.clip.clone(), clip);
    }
    if cancelled() {
        return Ok(None);
    }
    progress(0.7, "building");
    let database = source
        .build(&skeleton, |path| clips.get(path))
        .map_err(|e| RebuildError::new("build_failed", format!("{}: {e}", error_code(&e))))?;
    if cancelled() {
        return Ok(None);
    }
    progress(0.95, "measuring");
    Ok(Some(RebuildReport::of(&database, source)))
}
