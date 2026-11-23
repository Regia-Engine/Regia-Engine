// SPDX-License-Identifier: MPL-2.0
//! The motion-database editor's edit vocabulary: one [`MmDbOp`] per thing the form, the clip list
//! and the tag chips change, applied to a [`MotionDatabaseSource`] by [`apply_op`]. The panel and
//! `editor.mm_db_edit { ops }` both go through it, so a GUI edit and an agent edit write the same
//! document.
//!
//! Values are clamped to the runtime validator's ranges (`FeatureSchema::validate`,
//! `ContactSettings::validate`): group weights to `[0, MAX_GROUP_WEIGHT]`, sample and joint weights
//! to `[0, MAX_SAMPLE_WEIGHT]`, trims and contact thresholds to `>= 0` (an `*_off` threshold is
//! raised to its `*_on`). What has no range to clamp into (a non-finite number, a time or rate that
//! is not above zero, a list over its size limit, an unknown name) is refused with a code.

use serde::{Deserialize, Serialize};

use regia_animation::motion_matching::{
    ContactSettings, EntrySource, FeatureGroup, MAX_BIAS, MAX_GROUP_WEIGHT, MAX_HISTORY_SAMPLES,
    MAX_SAMPLE_WEIGHT, MAX_TAGS, MAX_TRAJECTORY_SAMPLES, MotionDatabaseSource, NormalizationSource,
    PoseJointSource, SpanKindSource, SpanSource, WeightsSource,
};

/// One edit of a `.motiondb.ron` document. JSON form: `{ "op": "<snake_case name>", ...fields }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum MmDbOp {
    /// The skeleton asset path.
    SetSkeleton {
        /// Asset path (not empty).
        path: String,
    },
    /// The root (character-frame) joint.
    SetRootJoint {
        /// Joint name (not empty).
        joint: String,
    },
    /// The root's local forward axis.
    SetForward {
        /// `[x, y, z]`, finite, with a ground-plane (x, z) part.
        axis: [f32; 3],
    },
    /// The foot joints.
    SetFeet {
        /// Joint names (no empty or repeated name).
        feet: Vec<String>,
    },
    /// The future trajectory sample times.
    SetTrajectoryTimes {
        /// Seconds, each finite and above zero, at most `MAX_TRAJECTORY_SAMPLES`. The per-sample
        /// weights are resized to match (new samples weigh 1).
        times: Vec<f32>,
    },
    /// The past (history) sample times.
    SetHistoryTimes {
        /// Seconds, each finite and above zero, at most `MAX_HISTORY_SAMPLES`. The per-sample
        /// weights are resized to match.
        times: Vec<f32>,
    },
    /// Database frames per second.
    SetSampleRate {
        /// Finite and above zero.
        rate: f32,
    },
    /// One group weight (clamped to `[0, MAX_GROUP_WEIGHT]`).
    SetWeight {
        /// Group name (`root_velocity`, `trajectory_position`, ...).
        group: String,
        /// The weight.
        value: f32,
    },
    /// The normalization mode.
    SetNormalization {
        /// `group_deviation`, `per_dimension` or `none`.
        mode: NormalizationSource,
    },
    /// One per-sample weight (clamped to `[0, MAX_SAMPLE_WEIGHT]`). Setting one fills the list
    /// with 1 up to the sample count first.
    SetSampleWeight {
        /// `trajectory` or `history`.
        list: String,
        /// Sample index.
        index: usize,
        /// The weight.
        value: f32,
    },
    /// Add an extra pose joint (position and velocity, weight 1).
    AddPoseJoint {
        /// Joint name (not empty, not already a foot or pose joint).
        joint: String,
    },
    /// Remove an extra pose joint.
    RemovePoseJoint {
        /// Index in `pose_joints`.
        index: usize,
    },
    /// Change an extra pose joint (absent fields stay).
    SetPoseJoint {
        /// Index in `pose_joints`.
        index: usize,
        /// Position feature.
        #[serde(default)]
        position: Option<bool>,
        /// Velocity feature.
        #[serde(default)]
        velocity: Option<bool>,
        /// Weight (clamped to `[0, MAX_SAMPLE_WEIGHT]`).
        #[serde(default)]
        weight: Option<f32>,
    },
    /// Contact labels: `None` removes both contact groups; thresholds are clamped (each `>= 0`,
    /// every `*_off` at least its `*_on`).
    SetContacts {
        /// The settings, or `null`.
        contacts: Option<ContactSettings>,
    },
    /// Add a left/right mirror pair.
    AddMirrorPair {
        /// Left joint name.
        left: String,
        /// Right joint name.
        right: String,
    },
    /// Remove a mirror pair.
    RemoveMirrorPair {
        /// Index in `mirror_pairs`.
        index: usize,
    },
    /// Add a tag to the table (at most `MAX_TAGS`).
    AddTag {
        /// Tag name (not empty, not in the table).
        name: String,
    },
    /// Remove a tag from the table, every entry's chips and every tag span.
    RemoveTag {
        /// Tag name.
        name: String,
    },
    /// Rename a tag across the database: the table, every entry's chips and every tag span.
    RenameTag {
        /// Current name.
        name: String,
        /// New name (not empty, not in the table).
        to: String,
    },
    /// Add a clip entry (not looping, no tags, no spans).
    AddClip {
        /// Clip asset path (not empty).
        clip: String,
        /// Insert position; absent appends.
        #[serde(default)]
        index: Option<usize>,
    },
    /// Remove a clip entry.
    RemoveClip {
        /// Entry index.
        index: usize,
    },
    /// Move a clip entry.
    MoveClip {
        /// Entry index.
        from: usize,
        /// New index.
        to: usize,
    },
    /// Change a clip entry's path or flags (absent fields stay).
    SetClip {
        /// Entry index.
        index: usize,
        /// Clip asset path.
        #[serde(default)]
        clip: Option<String>,
        /// Loops.
        #[serde(default)]
        looping: Option<bool>,
        /// Also added mirrored (needs mirror pairs to build).
        #[serde(default)]
        mirrored: Option<bool>,
        /// Played by distance (stops, pivots, landings).
        #[serde(default)]
        distance_matched: Option<bool>,
        /// Seconds at the start and end that are not jump targets (each clamped to `>= 0`).
        #[serde(default)]
        trim: Option<(f32, f32)>,
    },
    /// Add a tag chip to an entry (a tag the table lacks is added to it).
    AddClipTag {
        /// Entry index.
        index: usize,
        /// Tag name.
        tag: String,
    },
    /// Remove a tag chip from an entry (the table keeps the tag).
    RemoveClipTag {
        /// Entry index.
        index: usize,
        /// Tag name.
        tag: String,
    },
    /// Add a span to an entry (part B, the span timeline). Give `range` (clip seconds) or `frames`
    /// (database frames at the schema's sample rate, `[start, end)`). A `pass_through`,
    /// `unindexed` or same-tags `tag` span that overlaps or touches another of the same kind is
    /// merged with it ([`merge_span`]); `landing_bias` spans stack (spec 52 § 2.5: biases add).
    AddSpan {
        /// Entry index.
        index: usize,
        /// `pass_through`, `unindexed`, `{ landing_bias: b }` (clamped to `[-MAX_BIAS, MAX_BIAS]`)
        /// or `{ tag: [names] }` (a tag the table lacks is added to it).
        kind: SpanKindSource,
        /// Half-open `[start, end)`, clip seconds.
        #[serde(default)]
        range: Option<(f32, f32)>,
        /// Half-open `[start, end)`, frames.
        #[serde(default)]
        frames: Option<(u32, u32)>,
    },
    /// Move, resize or retype one span (absent fields stay); merged like [`MmDbOp::AddSpan`].
    SetSpan {
        /// Entry index.
        index: usize,
        /// Span index in the entry.
        span: usize,
        /// New kind.
        #[serde(default)]
        kind: Option<SpanKindSource>,
        /// New `[start, end)`, clip seconds.
        #[serde(default)]
        range: Option<(f32, f32)>,
        /// New `[start, end)`, frames.
        #[serde(default)]
        frames: Option<(u32, u32)>,
    },
    /// Remove one span.
    RemoveSpan {
        /// Entry index.
        index: usize,
        /// Span index in the entry.
        span: usize,
    },
}

/// A refused op: a stable code (`bad_value`, `bad_index`, `unknown_group`, `unknown_tag`,
/// `duplicate`, `too_many`) and a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpError {
    /// The code.
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl OpError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl MmDbOp {
    /// The op's snake_case name (the `op` field).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::SetSkeleton { .. } => "set_skeleton",
            Self::SetRootJoint { .. } => "set_root_joint",
            Self::SetForward { .. } => "set_forward",
            Self::SetFeet { .. } => "set_feet",
            Self::SetTrajectoryTimes { .. } => "set_trajectory_times",
            Self::SetHistoryTimes { .. } => "set_history_times",
            Self::SetSampleRate { .. } => "set_sample_rate",
            Self::SetWeight { .. } => "set_weight",
            Self::SetNormalization { .. } => "set_normalization",
            Self::SetSampleWeight { .. } => "set_sample_weight",
            Self::AddPoseJoint { .. } => "add_pose_joint",
            Self::RemovePoseJoint { .. } => "remove_pose_joint",
            Self::SetPoseJoint { .. } => "set_pose_joint",
            Self::SetContacts { .. } => "set_contacts",
            Self::AddMirrorPair { .. } => "add_mirror_pair",
            Self::RemoveMirrorPair { .. } => "remove_mirror_pair",
            Self::AddTag { .. } => "add_tag",
            Self::RemoveTag { .. } => "remove_tag",
            Self::RenameTag { .. } => "rename_tag",
            Self::AddClip { .. } => "add_clip",
            Self::RemoveClip { .. } => "remove_clip",
            Self::MoveClip { .. } => "move_clip",
            Self::SetClip { .. } => "set_clip",
            Self::AddClipTag { .. } => "add_clip_tag",
            Self::RemoveClipTag { .. } => "remove_clip_tag",
            Self::AddSpan { .. } => "add_span",
            Self::SetSpan { .. } => "set_span",
            Self::RemoveSpan { .. } => "remove_span",
        }
    }
}

/// Every op name, in declaration order (the agent schema lists them).
pub const OP_NAMES: [&str; 28] = [
    "set_skeleton",
    "set_root_joint",
    "set_forward",
    "set_feet",
    "set_trajectory_times",
    "set_history_times",
    "set_sample_rate",
    "set_weight",
    "set_normalization",
    "set_sample_weight",
    "add_pose_joint",
    "remove_pose_joint",
    "set_pose_joint",
    "set_contacts",
    "add_mirror_pair",
    "remove_mirror_pair",
    "add_tag",
    "remove_tag",
    "rename_tag",
    "add_clip",
    "remove_clip",
    "move_clip",
    "set_clip",
    "add_clip_tag",
    "remove_clip_tag",
    "add_span",
    "set_span",
    "remove_span",
];

fn finite(value: f32, what: &str) -> Result<f32, OpError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(OpError::new(
            "bad_value",
            format!("{what} must be finite, got {value}"),
        ))
    }
}

fn not_empty(text: &str, what: &str) -> Result<String, OpError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Err(OpError::new("bad_value", format!("{what} must not be empty")))
    } else {
        Ok(trimmed.to_string())
    }
}

fn clamp_weight(value: f32, max: f32, what: &str) -> Result<f32, OpError> {
    Ok(finite(value, what)?.clamp(0.0, max))
}

fn check_times(times: &[f32], max: usize, what: &str) -> Result<(), OpError> {
    if times.len() > max {
        return Err(OpError::new(
            "too_many",
            format!("{what}: {} samples, at most {max}", times.len()),
        ));
    }
    if let Some(t) = times.iter().find(|t| !t.is_finite() || **t <= 0.0) {
        return Err(OpError::new(
            "bad_value",
            format!("{what}: {t} must be finite and above zero"),
        ));
    }
    Ok(())
}

fn index_in(index: usize, len: usize, what: &str) -> Result<usize, OpError> {
    if index < len {
        Ok(index)
    } else {
        Err(OpError::new(
            "bad_index",
            format!("{what} {index} (there are {len})"),
        ))
    }
}

/// The weight of `group` in a source weight block.
pub fn weight_mut(weights: &mut WeightsSource, group: FeatureGroup) -> &mut f32 {
    match group {
        FeatureGroup::RootVelocity => &mut weights.root_velocity,
        FeatureGroup::TrajectoryPosition => &mut weights.trajectory_position,
        FeatureGroup::TrajectoryDirection => &mut weights.trajectory_direction,
        FeatureGroup::FootPosition => &mut weights.foot_position,
        FeatureGroup::FootVelocity => &mut weights.foot_velocity,
        FeatureGroup::TrajectoryHistory => &mut weights.trajectory_history,
        FeatureGroup::PosePosition => &mut weights.pose_position,
        FeatureGroup::PoseVelocity => &mut weights.pose_velocity,
        FeatureGroup::FootContact => &mut weights.foot_contact,
        FeatureGroup::FootPhase => &mut weights.foot_phase,
    }
}

/// The weight of `group` in a source weight block (by value).
#[must_use]
pub fn weight_of(weights: &WeightsSource, group: FeatureGroup) -> f32 {
    let mut copy = *weights;
    *weight_mut(&mut copy, group)
}

/// Resizes a non-empty per-sample weight list to `len` (new samples weigh 1); an empty list
/// (every sample weighs 1) stays empty.
fn resize_samples(list: &mut Vec<f32>, len: usize) {
    if !list.is_empty() {
        list.resize(len, 1.0);
    }
}

/// Every tag name an entry uses: its chips and its tag spans.
fn rename_in_entry(entry: &mut EntrySource, name: &str, to: Option<&str>) {
    let fix = |list: &mut Vec<String>| match to {
        Some(to) => {
            for tag in list.iter_mut().filter(|t| t.as_str() == name) {
                *tag = to.to_string();
            }
        }
        None => list.retain(|t| t != name),
    };
    fix(&mut entry.tags);
    for span in &mut entry.spans {
        if let SpanKindSource::Tag(names) = &mut span.kind {
            fix(names);
        }
    }
}

fn add_table_tag(doc: &mut MotionDatabaseSource, name: &str) -> Result<(), OpError> {
    if doc.tags.len() >= MAX_TAGS {
        return Err(OpError::new(
            "too_many",
            format!("a database holds at most {MAX_TAGS} tags"),
        ));
    }
    doc.tags.push(name.to_string());
    Ok(())
}

/// Applies `op` to `doc`. On error `doc` may be partly changed: callers apply to a copy
/// ([`apply_ops`]).
///
/// # Errors
///
/// [`OpError`] with the codes listed on it.
pub fn apply_op(doc: &mut MotionDatabaseSource, op: &MmDbOp) -> Result<(), OpError> {
    match op {
        MmDbOp::SetSkeleton { path } => doc.skeleton = not_empty(path, "skeleton path")?,
        MmDbOp::SetRootJoint { joint } => doc.schema.root_joint = not_empty(joint, "root joint")?,
        MmDbOp::SetForward { axis } => {
            for v in axis {
                finite(*v, "forward axis")?;
            }
            if axis[0] * axis[0] + axis[2] * axis[2] < 1e-12 {
                return Err(OpError::new(
                    "bad_value",
                    "the forward axis needs a ground-plane (x, z) part",
                ));
            }
            doc.schema.forward = *axis;
        }
        MmDbOp::SetFeet { feet } => {
            let mut names = Vec::with_capacity(feet.len());
            for foot in feet {
                let name = not_empty(foot, "foot joint")?;
                if names.contains(&name) {
                    return Err(OpError::new("duplicate", format!("foot {name:?} is named twice")));
                }
                names.push(name);
            }
            doc.schema.feet = names;
        }
        MmDbOp::SetTrajectoryTimes { times } => {
            check_times(times, MAX_TRAJECTORY_SAMPLES, "trajectory times")?;
            doc.schema.trajectory_times = times.clone();
            resize_samples(&mut doc.schema.sample_weights.trajectory, times.len());
        }
        MmDbOp::SetHistoryTimes { times } => {
            check_times(times, MAX_HISTORY_SAMPLES, "history times")?;
            doc.schema.history_times = times.clone();
            resize_samples(&mut doc.schema.sample_weights.history, times.len());
        }
        MmDbOp::SetSampleRate { rate } => {
            let rate = finite(*rate, "sample rate")?;
            if rate <= 0.0 {
                return Err(OpError::new("bad_value", "the sample rate must be above zero"));
            }
            doc.schema.sample_rate = rate;
        }
        MmDbOp::SetWeight { group, value } => {
            let group = FeatureGroup::from_name(group)
                .ok_or_else(|| OpError::new("unknown_group", format!("no feature group {group:?}")))?;
            *weight_mut(&mut doc.schema.weights, group) =
                clamp_weight(*value, MAX_GROUP_WEIGHT, "group weight")?;
        }
        MmDbOp::SetNormalization { mode } => doc.schema.normalization = *mode,
        MmDbOp::SetSampleWeight { list, index, value } => {
            let value = clamp_weight(*value, MAX_SAMPLE_WEIGHT, "sample weight")?;
            let schema = &mut doc.schema;
            let (weights, count) = match list.as_str() {
                "trajectory" => (
                    &mut schema.sample_weights.trajectory,
                    schema.trajectory_times.len(),
                ),
                "history" => (&mut schema.sample_weights.history, schema.history_times.len()),
                other => {
                    return Err(OpError::new(
                        "bad_value",
                        format!("list must be trajectory or history, got {other:?}"),
                    ));
                }
            };
            let index = index_in(*index, count, "sample")?;
            if weights.len() != count {
                weights.resize(count, 1.0);
            }
            weights[index] = value;
        }
        MmDbOp::AddPoseJoint { joint } => {
            let joint = not_empty(joint, "pose joint")?;
            let schema = &mut doc.schema;
            if schema.feet.contains(&joint) || schema.pose_joints.iter().any(|p| p.joint == joint) {
                return Err(OpError::new(
                    "duplicate",
                    format!("joint {joint:?} is already a foot or pose joint"),
                ));
            }
            schema.pose_joints.push(PoseJointSource {
                joint,
                position: true,
                velocity: true,
                weight: 1.0,
            });
        }
        MmDbOp::RemovePoseJoint { index } => {
            let index = index_in(*index, doc.schema.pose_joints.len(), "pose joint")?;
            doc.schema.pose_joints.remove(index);
        }
        MmDbOp::SetPoseJoint {
            index,
            position,
            velocity,
            weight,
        } => {
            let index = index_in(*index, doc.schema.pose_joints.len(), "pose joint")?;
            let weight = weight
                .map(|w| clamp_weight(w, MAX_SAMPLE_WEIGHT, "pose joint weight"))
                .transpose()?;
            let joint = &mut doc.schema.pose_joints[index];
            if let Some(p) = position {
                joint.position = *p;
            }
            if let Some(v) = velocity {
                joint.velocity = *v;
            }
            if let Some(w) = weight {
                joint.weight = w;
            }
        }
        MmDbOp::SetContacts { contacts } => {
            doc.schema.contacts = match contacts {
                None => None,
                Some(c) => {
                    let height_on = finite(c.height_on, "height_on")?.max(0.0);
                    let speed_on = finite(c.speed_on, "speed_on")?.max(0.0);
                    Some(ContactSettings {
                        height_on,
                        height_off: finite(c.height_off, "height_off")?.max(height_on),
                        speed_on,
                        speed_off: finite(c.speed_off, "speed_off")?.max(speed_on),
                        phase: c.phase,
                    })
                }
            };
        }
        MmDbOp::AddMirrorPair { left, right } => {
            let left = not_empty(left, "left joint")?;
            let right = not_empty(right, "right joint")?;
            if doc
                .mirror_pairs
                .iter()
                .any(|(a, b)| [a, b].iter().any(|n| **n == left || **n == right))
            {
                return Err(OpError::new(
                    "duplicate",
                    format!("{left:?} or {right:?} is already in a mirror pair"),
                ));
            }
            doc.mirror_pairs.push((left, right));
        }
        MmDbOp::RemoveMirrorPair { index } => {
            let index = index_in(*index, doc.mirror_pairs.len(), "mirror pair")?;
            doc.mirror_pairs.remove(index);
        }
        MmDbOp::AddTag { name } => {
            let name = not_empty(name, "tag name")?;
            if doc.tags.contains(&name) {
                return Err(OpError::new("duplicate", format!("tag {name:?} exists")));
            }
            add_table_tag(doc, &name)?;
        }
        MmDbOp::RemoveTag { name } => {
            let position = doc
                .tags
                .iter()
                .position(|t| t == name)
                .ok_or_else(|| OpError::new("unknown_tag", format!("no tag {name:?}")))?;
            doc.tags.remove(position);
            for entry in &mut doc.entries {
                rename_in_entry(entry, name, None);
            }
        }
        MmDbOp::RenameTag { name, to } => {
            let to = not_empty(to, "tag name")?;
            let position = doc
                .tags
                .iter()
                .position(|t| t == name)
                .ok_or_else(|| OpError::new("unknown_tag", format!("no tag {name:?}")))?;
            if to == *name {
                return Ok(());
            }
            if doc.tags.contains(&to) {
                return Err(OpError::new("duplicate", format!("tag {to:?} exists")));
            }
            doc.tags[position] = to.clone();
            for entry in &mut doc.entries {
                rename_in_entry(entry, name, Some(&to));
            }
        }
        MmDbOp::AddClip { clip, index } => {
            let clip = not_empty(clip, "clip path")?;
            let at = match index {
                Some(i) if *i <= doc.entries.len() => *i,
                Some(i) => {
                    return Err(OpError::new(
                        "bad_index",
                        format!("insert position {i} (there are {})", doc.entries.len()),
                    ));
                }
                None => doc.entries.len(),
            };
            doc.entries.insert(
                at,
                EntrySource {
                    clip,
                    looping: false,
                    tags: Vec::new(),
                    mirrored: false,
                    trim: (0.0, 0.0),
                    spans: Vec::new(),
                    distance_matched: false,
                },
            );
        }
        MmDbOp::RemoveClip { index } => {
            let index = index_in(*index, doc.entries.len(), "entry")?;
            doc.entries.remove(index);
        }
        MmDbOp::MoveClip { from, to } => {
            let from = index_in(*from, doc.entries.len(), "entry")?;
            let to = index_in(*to, doc.entries.len(), "entry")?;
            let entry = doc.entries.remove(from);
            doc.entries.insert(to, entry);
        }
        MmDbOp::SetClip {
            index,
            clip,
            looping,
            mirrored,
            distance_matched,
            trim,
        } => {
            let index = index_in(*index, doc.entries.len(), "entry")?;
            let clip = clip.as_deref().map(|c| not_empty(c, "clip path")).transpose()?;
            let trim = trim
                .map(|(a, b)| Ok::<_, OpError>((finite(a, "trim")?.max(0.0), finite(b, "trim")?.max(0.0))))
                .transpose()?;
            let entry = &mut doc.entries[index];
            if let Some(clip) = clip {
                entry.clip = clip;
            }
            if let Some(v) = looping {
                entry.looping = *v;
            }
            if let Some(v) = mirrored {
                entry.mirrored = *v;
            }
            if let Some(v) = distance_matched {
                entry.distance_matched = *v;
            }
            if let Some(t) = trim {
                entry.trim = t;
            }
        }
        MmDbOp::AddClipTag { index, tag } => {
            let index = index_in(*index, doc.entries.len(), "entry")?;
            let tag = not_empty(tag, "tag name")?;
            if doc.entries[index].tags.contains(&tag) {
                return Ok(());
            }
            if !doc.tags.contains(&tag) {
                add_table_tag(doc, &tag)?;
            }
            doc.entries[index].tags.push(tag);
        }
        MmDbOp::RemoveClipTag { index, tag } => {
            let index = index_in(*index, doc.entries.len(), "entry")?;
            let tags = &mut doc.entries[index].tags;
            let position = tags
                .iter()
                .position(|t| t == tag)
                .ok_or_else(|| OpError::new("unknown_tag", format!("entry {index} has no tag {tag:?}")))?;
            tags.remove(position);
        }
        MmDbOp::AddSpan {
            index,
            kind,
            range,
            frames,
        } => {
            let index = index_in(*index, doc.entries.len(), "entry")?;
            let range = span_range(doc.schema.sample_rate, *range, *frames)?
                .ok_or_else(|| OpError::new("bad_value", "a span needs `range` or `frames`"))?;
            let kind = span_kind(doc, kind)?;
            let spans = &mut doc.entries[index].spans;
            spans.push(SpanSource { kind, range });
            let at = spans.len() - 1;
            merge_span(spans, at);
        }
        MmDbOp::SetSpan {
            index,
            span,
            kind,
            range,
            frames,
        } => {
            let index = index_in(*index, doc.entries.len(), "entry")?;
            let span = index_in(*span, doc.entries[index].spans.len(), "span")?;
            let range = span_range(doc.schema.sample_rate, *range, *frames)?;
            let kind = kind.as_ref().map(|k| span_kind(doc, k)).transpose()?;
            let spans = &mut doc.entries[index].spans;
            if let Some(range) = range {
                spans[span].range = range;
            }
            if let Some(kind) = kind {
                spans[span].kind = kind;
            }
            merge_span(spans, span);
        }
        MmDbOp::RemoveSpan { index, span } => {
            let index = index_in(*index, doc.entries.len(), "entry")?;
            let span = index_in(*span, doc.entries[index].spans.len(), "span")?;
            doc.entries[index].spans.remove(span);
        }
    }
    Ok(())
}

/// The clip time of database frame `frame` at `rate` frames per second: the runtime builder's own
/// expression (`k as f32 * (1 / rate)`), so a span edge snapped here covers exactly the frames
/// from `frame` on (a frame is covered when `start <= time < end`).
#[must_use]
pub fn frame_time(rate: f32, frame: u32) -> f32 {
    let step = 1.0 / rate;
    frame as f32 * step
}

/// The nearest frame of clip time `time` at `rate` (never below zero).
#[must_use]
pub fn time_frame(rate: f32, time: f32) -> u32 {
    if !(rate.is_finite() && rate > 0.0 && time.is_finite()) {
        return 0;
    }
    (time * rate).round().max(0.0) as u32
}

/// A span's `[start, end)` from seconds or frames, checked like the runtime's `validate_spans`
/// (finite, `0 <= start < end`). `None` when neither is given.
fn span_range(
    rate: f32,
    range: Option<(f32, f32)>,
    frames: Option<(u32, u32)>,
) -> Result<Option<(f32, f32)>, OpError> {
    let range = match (range, frames) {
        (Some(_), Some(_)) => {
            return Err(OpError::new("bad_value", "give `range` or `frames`, not both"));
        }
        (Some(r), None) => r,
        (None, Some((a, b))) => {
            if !(rate.is_finite() && rate > 0.0) {
                return Err(OpError::new(
                    "bad_value",
                    "`frames` needs a sample rate above zero",
                ));
            }
            (frame_time(rate, a), frame_time(rate, b))
        }
        (None, None) => return Ok(None),
    };
    let (start, end) = (finite(range.0, "span start")?, finite(range.1, "span end")?);
    if start < 0.0 || end <= start {
        return Err(OpError::new(
            "bad_value",
            format!("a span needs 0 <= start < end, got [{start}, {end})"),
        ));
    }
    Ok(Some((start, end)))
}

/// A span kind with its bias clamped to `[-MAX_BIAS, MAX_BIAS]` and its tag names in the table
/// (added when missing, like a chip).
fn span_kind(doc: &mut MotionDatabaseSource, kind: &SpanKindSource) -> Result<SpanKindSource, OpError> {
    Ok(match kind {
        SpanKindSource::LandingBias(b) => {
            SpanKindSource::LandingBias(finite(*b, "landing bias")?.clamp(-MAX_BIAS, MAX_BIAS))
        }
        SpanKindSource::Tag(names) => {
            let mut out: Vec<String> = Vec::with_capacity(names.len());
            for name in names {
                let name = not_empty(name, "tag name")?;
                if !doc.tags.contains(&name) {
                    add_table_tag(doc, &name)?;
                }
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            if out.is_empty() {
                return Err(OpError::new("bad_value", "a tag span needs at least one tag"));
            }
            SpanKindSource::Tag(out)
        }
        other => other.clone(),
    })
}

/// Whether two span kinds merge when their ranges overlap or touch: the same sticky flag
/// (`pass_through`, `unindexed`) or a `tag` span with the same set of names. Spec 52 § 2.5:
/// "overlapping spans combine: biases add, masks OR, `unindexed` and `pass_through` are sticky",
/// so the union of two such spans bakes the same frames as the pair; two `landing_bias` spans
/// add where they overlap, so they never merge.
#[must_use]
pub fn kinds_merge(a: &SpanKindSource, b: &SpanKindSource) -> bool {
    match (a, b) {
        (SpanKindSource::PassThrough, SpanKindSource::PassThrough)
        | (SpanKindSource::Unindexed, SpanKindSource::Unindexed) => true,
        (SpanKindSource::Tag(x), SpanKindSource::Tag(y)) => {
            x.len() == y.len() && x.iter().all(|n| y.contains(n))
        }
        _ => false,
    }
}

/// Merges span `at` with every span of a merging kind ([`kinds_merge`]) whose range overlaps or
/// touches it, repeatedly, into their union at the lowest index of the group; returns the merged
/// span's index.
pub fn merge_span(spans: &mut Vec<SpanSource>, at: usize) -> usize {
    let mut at = at;
    loop {
        let Some(current) = spans.get(at).cloned() else {
            return at;
        };
        let other = spans.iter().enumerate().position(|(i, s)| {
            i != at
                && kinds_merge(&s.kind, &current.kind)
                && s.range.0 <= current.range.1
                && current.range.0 <= s.range.1
        });
        let Some(other) = other else {
            return at;
        };
        let union = (
            current.range.0.min(spans[other].range.0),
            current.range.1.max(spans[other].range.1),
        );
        let (keep, drop) = if other < at { (other, at) } else { (at, other) };
        spans[keep].range = union;
        spans[keep].kind = current.kind;
        spans.remove(drop);
        at = keep;
    }
}

/// A refused batch: the failing op's position and its error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchError {
    /// Index of the op that failed.
    pub at: usize,
    /// Why.
    pub error: OpError,
}

/// Applies `ops` in order to a copy of `doc` and returns it; all or nothing.
///
/// # Errors
///
/// The first op that fails, with its position.
pub fn apply_ops(doc: &MotionDatabaseSource, ops: &[MmDbOp]) -> Result<MotionDatabaseSource, BatchError> {
    let mut out = doc.clone();
    for (at, op) in ops.iter().enumerate() {
        apply_op(&mut out, op).map_err(|error| BatchError { at, error })?;
    }
    Ok(out)
}
