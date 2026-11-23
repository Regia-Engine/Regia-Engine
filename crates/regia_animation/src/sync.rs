// SPDX-License-Identifier: MPL-2.0
//! Sync groups across graph nodes (`.sync.ron`, frozen v1): leader/follower playback with
//! optional sync markers, so locomotion cycles of different lengths stay in step while blending.
//!
//! Specification: `docs/specs/17-animation.md` § 4b (ADR-1465). A [`SyncSetup`] declares groups
//! (with per-clip [`SyncRole`]s) and per-clip [`SyncMarker`]s; every `Clip` graph node whose
//! `sync_group` names a declared group joins it. [`SyncGroups`] is the per-animator runtime
//! state: the leader chosen from the previous frame's weights, its clock, and the step every
//! member follows this frame. Followers either match the leader's position between common
//! markers (marker sync) or its normalised phase (length sync), and fire no notifies.
//!
//! The leader is decided from the previous frame's weights, so the choice is deterministic and
//! needs no second pass over the graph; a change of leader takes effect one frame later and is
//! continuous because the new leader was already in phase.

use serde::{Deserialize, Serialize};

use crate::clip::ClipStep;

/// The `.sync.ron` format version (v1 is the first version).
pub const SYNC_FORMAT_VERSION: u32 = 1;

/// How a clip takes part in leader election.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum SyncRole {
    /// Leads while it has the highest weight in the group.
    #[default]
    MayLead,
    /// Always leads while it plays (the heaviest such clip when there are several).
    AlwaysLead,
    /// Never leads unless every member is `NeverLead`.
    NeverLead,
}

/// A group declaration.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SyncGroupDef {
    /// Group name (matches a `Clip` node's `sync_group`).
    pub name: String,
    /// Per-clip roles; clips not listed are [`SyncRole::MayLead`].
    #[serde(default)]
    pub roles: Vec<(String, SyncRole)>,
}

/// A named sync point on a clip (for example the frame the left foot plants).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SyncMarker {
    /// Marker name, shared by the clips that should line up on it.
    pub name: String,
    /// Clip-local time, seconds.
    pub time: f32,
}

/// The markers of one clip.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClipMarkers {
    /// Clip name.
    pub clip: String,
    /// Markers (any order; sorted on load).
    pub markers: Vec<SyncMarker>,
}

/// The `.sync.ron` document.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SyncSetup {
    /// Format version of this file.
    #[serde(default)]
    pub format_version: u32,
    /// Declared groups.
    #[serde(default)]
    pub groups: Vec<SyncGroupDef>,
    /// Markers per clip.
    #[serde(default)]
    pub markers: Vec<ClipMarkers>,
}

/// Errors loading a `.sync.ron`.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// RON syntax or shape error.
    #[error("sync RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("sync RON write: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file is newer than this engine.
    #[error("sync format version {0} is unknown (current is {SYNC_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The document is malformed.
    #[error("invalid sync setup: {0}")]
    Invalid(String),
}

/// `from` versions of the registered migrations (none: v1 is the first version).
#[must_use]
pub fn sync_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of the format (for `cargo xtask check-formats`).
#[must_use]
pub fn sync_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<SyncSetup>(),
        serde_field_names::<SyncGroupDef>(),
        serde_field_names::<ClipMarkers>(),
        serde_field_names::<SyncMarker>(),
        serde_variant_names::<SyncRole>(),
    ])
}

impl SyncSetup {
    /// Parses `.sync.ron` text: peeks the version (newer is rejected), validates, sorts markers.
    ///
    /// # Errors
    ///
    /// Syntax errors, unknown versions and failed [`SyncSetup::validate`].
    pub fn from_ron_str(text: &str) -> Result<Self, SyncError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| SyncError::Invalid(format!("cannot read format_version: {e}")))?;
        if version > SYNC_FORMAT_VERSION {
            return Err(SyncError::UnknownVersion(version));
        }
        let mut setup: SyncSetup = ron::from_str(text)?;
        setup.format_version = SYNC_FORMAT_VERSION;
        setup.validate()?;
        setup.sort_markers();
        Ok(setup)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, SyncError> {
        let mut setup = self.clone();
        setup.format_version = SYNC_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&setup, pretty)?)
    }

    /// Checks names and times.
    ///
    /// # Errors
    ///
    /// An empty or duplicate group name, a clip listed twice in one group or in the markers, an
    /// empty marker name, or a non-finite or negative marker time.
    pub fn validate(&self) -> Result<(), SyncError> {
        let bad = |m: String| Err(SyncError::Invalid(m));
        for (i, group) in self.groups.iter().enumerate() {
            if group.name.is_empty() || self.groups[..i].iter().any(|g| g.name == group.name) {
                return bad(format!(
                    "group {i} has an empty or duplicate name {:?}",
                    group.name
                ));
            }
            for (j, (clip, _)) in group.roles.iter().enumerate() {
                if group.roles[..j].iter().any(|(c, _)| c == clip) {
                    return bad(format!("group {:?} lists clip {clip:?} twice", group.name));
                }
            }
        }
        for (i, entry) in self.markers.iter().enumerate() {
            if self.markers[..i].iter().any(|m| m.clip == entry.clip) {
                return bad(format!("clip {:?} has two marker lists", entry.clip));
            }
            for marker in &entry.markers {
                if marker.name.is_empty() || !marker.time.is_finite() || marker.time < 0.0 {
                    return bad(format!(
                        "clip {:?}: markers need a name and a finite time >= 0",
                        entry.clip
                    ));
                }
            }
        }
        Ok(())
    }

    /// Sort every clip's markers by time (done on load; call after editing markers in code).
    pub fn sort_markers(&mut self) {
        for entry in &mut self.markers {
            entry.markers.sort_by(|a, b| a.time.total_cmp(&b.time));
        }
    }

    /// Whether `group` is declared.
    pub fn declares(&self, group: &str) -> bool {
        self.groups.iter().any(|g| g.name == group)
    }

    /// The role of `clip` in `group`.
    pub fn role(&self, group: &str, clip: &str) -> SyncRole {
        self.groups
            .iter()
            .find(|g| g.name == group)
            .and_then(|g| g.roles.iter().find(|(c, _)| c == clip))
            .map_or(SyncRole::MayLead, |(_, role)| *role)
    }

    /// The markers of `clip` (sorted when loaded from RON).
    pub fn markers_of(&self, clip: &str) -> &[SyncMarker] {
        self.markers
            .iter()
            .find(|m| m.clip == clip)
            .map_or(&[], |m| m.markers.as_slice())
    }
}

/// The member that leads a group.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncLeader {
    /// Clip name.
    pub clip: String,
    /// Clip-local time at the end of the last frame.
    pub time: f32,
    /// Playback speed of the leading node.
    pub speed: f32,
    /// Whether the leading node loops.
    pub looping: bool,
    /// Length of the leader's cycle, seconds (a blend space's weighted sample length).
    pub duration: f32,
}

/// A member seen this frame.
#[derive(Clone, Debug, PartialEq)]
struct Candidate {
    clip: String,
    weight: f32,
    time: f32,
    speed: f32,
    looping: bool,
    duration: f32,
    role: SyncRole,
}

/// Runtime state of one group.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncGroupState {
    /// Group name.
    pub name: String,
    /// The leader for this frame (decided from the previous frame's weights).
    pub leader: Option<SyncLeader>,
    /// The leader's step this frame, once computed.
    step: Option<ClipStep>,
    candidates: Vec<Candidate>,
    /// How many entries of `candidates` this frame has filled. Slots past this stay allocated
    /// so the next frame can reuse their clip-name strings.
    candidate_count: usize,
}

/// How a member plays this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SyncPlay {
    /// Play on its own clock (no leader yet).
    Own,
    /// The leader: this absolute step; notifies fire.
    Lead(ClipStep),
    /// A follower: this absolute step; notifies stay quiet.
    Follow(ClipStep),
}

/// One member's frame, as [`SyncGroups::record`] stores it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncMember<'a> {
    /// Clip name.
    pub clip: &'a str,
    /// The member's pose weight this frame.
    pub weight: f32,
    /// Its clip-local time after this frame.
    pub time: f32,
    /// Its node's playback speed.
    pub speed: f32,
    /// Whether its node loops.
    pub looping: bool,
    /// Length of its cycle, seconds (a clip's duration; a blend space's weighted sample length).
    pub duration: f32,
}

/// A member asking [`SyncGroups::play`] how to play this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncQuery<'a> {
    /// Member name: the clip (a blend space uses its dominant sample's clip).
    pub member: &'a str,
    /// Its cycle length, seconds.
    pub duration: f32,
    /// Its time before this frame, seconds.
    pub own_time: f32,
    /// Frame time, seconds (before the member's own speed).
    pub delta: f32,
    /// Whether a follower may use marker sync (clips); `false` = normalised phase (blend spaces).
    pub markers: bool,
}

/// A named cycle: a clip or a blend space's weighted cycle.
#[derive(Clone, Copy, Debug)]
struct Clock<'a> {
    name: &'a str,
    duration: f32,
}

/// Per-animator sync-group runtime state (kept on the animator between frames).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncGroups {
    groups: Vec<SyncGroupState>,
}

/// A point [`SyncGroups::rewind`] returns to: the groups, each one's step and candidate count.
#[derive(Clone, Debug)]
pub(crate) struct SyncMark {
    groups: usize,
    /// `(step, logical candidate count, stored candidate len)` per group.
    state: Vec<(Option<ClipStep>, usize, usize)>,
}

impl SyncGroups {
    /// Where the groups stand now (before a side-effect-free probe evaluation).
    pub(crate) fn mark(&self) -> SyncMark {
        SyncMark {
            groups: self.groups.len(),
            state: self
                .groups
                .iter()
                .map(|g| (g.step, g.candidate_count, g.candidates.len()))
                .collect(),
        }
    }

    /// Undoes everything since `mark`: groups the probe created, members it recorded and steps it
    /// computed (at delta 0, a cached step would freeze the group's clock for the real frame).
    pub(crate) fn rewind(&mut self, mark: &SyncMark) {
        self.groups.truncate(mark.groups);
        for (group, (step, count, len)) in self.groups.iter_mut().zip(&mark.state) {
            group.step = *step;
            group.candidate_count = *count;
            group.candidates.truncate(*len);
        }
    }

    /// The groups that played last frame, with their leaders.
    pub fn groups(&self) -> &[SyncGroupState] {
        &self.groups
    }

    /// Make `leader` lead `group` for the next frame (a montage blending out; it replaces the
    /// leader the graph's weights elected).
    pub fn lead(&mut self, group: &str, leader: SyncLeader) {
        match self.groups.iter_mut().find(|g| g.name == group) {
            Some(state) => state.leader = Some(leader),
            None => self.groups.push(SyncGroupState {
                name: group.to_string(),
                leader: Some(leader),
                step: None,
                candidates: Vec::new(),
                candidate_count: 0,
            }),
        }
    }

    /// Start a frame: forget last frame's steps and which members played (leaders stay).
    ///
    /// Candidate slots stay allocated so the clip-name strings can be reused.
    pub fn begin_frame(&mut self) {
        for group in &mut self.groups {
            group.step = None;
            group.candidate_count = 0;
        }
    }

    /// Decide how a member plays this frame: on its own clock (no leader yet), as the leader
    /// (the group's step), or following the leader's step mapped onto its own clock.
    pub fn play(&mut self, setup: &SyncSetup, group: &str, query: SyncQuery<'_>) -> SyncPlay {
        let Some(state) = self.groups.iter_mut().find(|g| g.name == group) else {
            return SyncPlay::Own;
        };
        let Some(leader) = state.leader.as_ref() else {
            return SyncPlay::Own;
        };
        let (time, speed, duration, looping) = (leader.time, leader.speed, leader.duration, leader.looping);
        let step = *state
            .step
            .get_or_insert_with(|| ClipStep::advance(time, query.delta * speed, duration, looping));
        let Some(leader) = state.leader.as_ref() else {
            return SyncPlay::Own;
        };
        if leader.clip == query.member {
            // The same cycle; a blend space whose weighted length moved since last frame keeps
            // its phase.
            if leader.duration > 0.0 && query.duration > 0.0 && leader.duration != query.duration {
                let k = query.duration / leader.duration;
                return SyncPlay::Lead(ClipStep {
                    prev: step.prev * k,
                    time: step.time * k,
                    loops: step.loops,
                });
            }
            return SyncPlay::Lead(step);
        }
        let own = Clock {
            name: query.member,
            duration: query.duration,
        };
        let lead = Clock {
            name: &leader.clip,
            duration: leader.duration,
        };
        SyncPlay::Follow(follow_step(
            setup,
            lead,
            &step,
            own,
            query.own_time,
            query.markers,
        ))
    }

    /// Record a member's weight and resulting time this frame (for next frame's election).
    pub fn record(&mut self, setup: &SyncSetup, group: &str, member: SyncMember<'_>) {
        let SyncMember {
            clip,
            weight,
            time,
            speed,
            looping,
            duration,
        } = member;
        let index = match self.groups.iter().position(|g| g.name == group) {
            Some(index) => index,
            None => {
                self.groups.push(SyncGroupState {
                    name: group.to_string(),
                    leader: None,
                    step: None,
                    candidates: Vec::new(),
                    candidate_count: 0,
                });
                self.groups.len() - 1
            }
        };
        let role = setup.role(group, clip);
        let weight = crate::num::finite_or(weight, 0.0);
        let group = &mut self.groups[index];
        if group.candidate_count < group.candidates.len() {
            let slot = &mut group.candidates[group.candidate_count];
            reuse_clip_name(&mut slot.clip, clip);
            slot.weight = weight;
            slot.time = time;
            slot.speed = speed;
            slot.looping = looping;
            slot.duration = duration;
            slot.role = role;
        } else {
            crate::hot_count::count_sync_clip_string();
            group.candidates.push(Candidate {
                clip: clip.to_string(),
                weight,
                time,
                speed,
                looping,
                duration,
                role,
            });
        }
        group.candidate_count += 1;
    }

    /// End a frame: groups nobody played are dropped; the others elect next frame's leader.
    pub fn end_frame(&mut self) {
        self.groups.retain(|g| g.candidate_count > 0);
        for group in &mut self.groups {
            let n = group.candidate_count;
            let chosen = {
                let candidates = &group.candidates[..n];
                let pick = |role: SyncRole| {
                    candidates
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.role == role)
                        // The heaviest; the first evaluated wins a tie (max_by keeps the last).
                        .rev()
                        .max_by(|(_, a), (_, b)| a.weight.total_cmp(&b.weight))
                        .map(|(index, _)| index)
                };
                pick(SyncRole::AlwaysLead)
                    .or_else(|| pick(SyncRole::MayLead))
                    .or_else(|| (n > 0).then_some(0))
            };
            if let Some(index) = chosen {
                let (time, speed, looping, duration) = {
                    let candidate = &group.candidates[index];
                    (
                        candidate.time,
                        candidate.speed,
                        candidate.looping,
                        candidate.duration,
                    )
                };
                let same = group
                    .leader
                    .as_ref()
                    .is_some_and(|leader| leader.clip == group.candidates[index].clip);
                if same {
                    if let Some(leader) = group.leader.as_mut() {
                        leader.time = time;
                        leader.speed = speed;
                        leader.looping = looping;
                        leader.duration = duration;
                    }
                } else if group.leader.is_some() {
                    let SyncGroupState {
                        leader, candidates, ..
                    } = group;
                    if let Some(leader) = leader.as_mut() {
                        reuse_clip_name(&mut leader.clip, &candidates[index].clip);
                        leader.time = time;
                        leader.speed = speed;
                        leader.looping = looping;
                        leader.duration = duration;
                    }
                } else {
                    crate::hot_count::count_sync_clip_string();
                    group.leader = Some(SyncLeader {
                        clip: group.candidates[index].clip.clone(),
                        time,
                        speed,
                        looping,
                        duration,
                    });
                }
            } else {
                group.leader = None;
            }
        }
    }
}

/// Copy `next` into `buf`, counting only when the string storage has to grow.
fn reuse_clip_name(buf: &mut String, next: &str) {
    if buf == next {
        return;
    }
    if buf.capacity() < next.len() {
        crate::hot_count::count_sync_clip_string();
    }
    buf.clear();
    buf.push_str(next);
}

/// Markers of `clip` whose names also occur in `other`'s markers.
fn common<'m>(markers: &'m [SyncMarker], other: &[SyncMarker]) -> Vec<&'m SyncMarker> {
    markers
        .iter()
        .filter(|m| other.iter().any(|o| o.name == m.name))
        .collect()
}

/// Position of `time` between consecutive markers of a looping cycle: the index of the marker
/// at or before it (cyclically) and the fraction toward the next.
fn marker_position(markers: &[&SyncMarker], time: f32, duration: f32) -> Option<(usize, f32)> {
    let n = markers.len();
    if n < 2 || duration <= 0.0 {
        return None;
    }
    let index = markers.iter().rposition(|m| m.time <= time).unwrap_or(n - 1);
    let from = markers[index].time;
    let mut to = markers[(index + 1) % n].time;
    let mut at = time;
    if index + 1 >= n {
        to += duration; // wraps to the first marker of the next lap
    }
    if at < from {
        at += duration; // before the first marker: still in the previous lap's last gap
    }
    let gap = to - from;
    (gap > 0.0).then(|| (index, ((at - from) / gap).clamp(0.0, 1.0)))
}

/// The follower time at the leader's marker position `(index, fraction)`: the follower gap with
/// the same marker names nearest after the follower's current time.
fn follower_time(
    leader: &[&SyncMarker],
    position: (usize, f32),
    follower: &[&SyncMarker],
    own_time: f32,
    duration: f32,
) -> Option<f32> {
    let n = leader.len();
    let (from_name, to_name) = (&leader[position.0].name, &leader[(position.0 + 1) % n].name);
    let m = follower.len();
    let mut best: Option<(f32, f32)> = None; // (distance ahead, time)
    for (j, &a) in follower.iter().enumerate() {
        let b = follower[(j + 1) % m];
        if &a.name != from_name || &b.name != to_name {
            continue;
        }
        let mut end = b.time;
        if j + 1 >= m {
            end += duration;
        }
        let time = (a.time + (end - a.time) * position.1).rem_euclid(duration);
        let ahead = (time - own_time).rem_euclid(duration);
        if best.is_none_or(|(d, _)| ahead < d) {
            best = Some((ahead, time));
        }
    }
    best.map(|(_, time)| time)
}

/// A follower's step for the leader's `step`: marker sync when `markers` is set and both
/// clips share at least two marker names, else normalised phase.
fn follow_step(
    setup: &SyncSetup,
    leader: Clock<'_>,
    step: &ClipStep,
    own: Clock<'_>,
    own_time: f32,
    markers: bool,
) -> ClipStep {
    let lead_markers = setup.markers_of(leader.name);
    let own_markers = setup.markers_of(own.name);
    let lead_common = common(lead_markers, own_markers);
    let own_common = common(own_markers, lead_markers);
    if markers && lead_common.len() >= 2 && own_common.len() >= 2 && own.duration > 0.0 {
        let to = marker_position(&lead_common, step.time, leader.duration)
            .and_then(|p| follower_time(&lead_common, p, &own_common, own_time, own.duration));
        if let Some(time) = to {
            let prev = own_time.clamp(0.0, own.duration);
            // `step.loops` already counts the leader's own wrap; only its whole extra laps
            // ride along, so marker sync keeps the same cycle count as the normalised-phase
            // branch without counting a shared wrap twice (a wrap is a negative lap in
            // reverse). The follower's own wrap in the playback direction adds the remaining
            // lap (correctness audit, finding 2; review sprint-rev-kfix-app-anim-audio,
            // finding 1).
            let reverse = step.is_reverse();
            let leader_wrap = if reverse {
                -i32::from(step.time > step.prev)
            } else {
                i32::from(step.time < step.prev)
            };
            let own_wrap = if reverse {
                -i32::from(time > prev)
            } else {
                i32::from(time < prev)
            };
            let loops = step.loops - leader_wrap + own_wrap;
            return ClipStep { prev, time, loops };
        }
    }
    let scale = |t: f32| {
        if leader.duration > 0.0 {
            (t / leader.duration) * own.duration
        } else {
            0.0
        }
    };
    ClipStep {
        prev: scale(step.prev),
        time: scale(step.time),
        loops: step.loops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::AnimationClip;

    fn clock(clip: &AnimationClip) -> Clock<'_> {
        Clock {
            name: &clip.name,
            duration: clip.duration,
        }
    }

    fn marker(name: &str, time: f32) -> SyncMarker {
        SyncMarker {
            name: name.into(),
            time,
        }
    }

    fn setup() -> SyncSetup {
        SyncSetup {
            format_version: SYNC_FORMAT_VERSION,
            groups: vec![SyncGroupDef {
                name: "gait".into(),
                roles: vec![("start".into(), SyncRole::AlwaysLead)],
            }],
            markers: vec![
                ClipMarkers {
                    clip: "walk".into(),
                    markers: vec![marker("l", 0.0), marker("r", 0.5)],
                },
                ClipMarkers {
                    clip: "run".into(),
                    markers: vec![marker("l", 0.1), marker("r", 0.4)],
                },
            ],
        }
    }

    #[test]
    fn marker_sync_matches_the_position_between_markers() {
        let setup = setup();
        let walk = AnimationClip::empty("walk", 1.0);
        let run = AnimationClip::empty("run", 0.6);
        // Leader halfway from l (0.0) to r (0.5): follower halfway from l (0.1) to r (0.4).
        let step = ClipStep {
            prev: 0.2,
            time: 0.25,
            loops: 0,
        };
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.2, true);
        assert!((follow.time - 0.25).abs() < 1e-5, "{follow:?}");
        // Leader 3/4 of the way from r (0.5) round to l (1.0 ≡ 0.0): follower 3/4 from r (0.4)
        // to l (0.1 + 0.6), at 0.625, which is 0.025 into its next lap.
        let step = ClipStep {
            prev: 0.7,
            time: 0.875,
            loops: 0,
        };
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.5, true);
        assert!((follow.time - 0.025).abs() < 1e-5, "{follow:?}");
        assert_eq!(follow.loops, 1, "wrapped past the run's end");
    }

    #[test]
    fn marker_sync_keeps_the_leaders_extra_loops() {
        let setup = setup();
        let walk = AnimationClip::empty("walk", 1.0);
        let run = AnimationClip::empty("run", 0.6);
        // The leader's step covers two whole extra laps of its 1 s clip (2.25 s of playback
        // landing at 0.25); the follower must keep both laps of root motion and clip
        // notifies, like the normalised-phase branch does (correctness audit, finding 2).
        let step = ClipStep {
            prev: 0.0,
            time: 0.25,
            loops: 2,
        };
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.0, true);
        assert!((follow.time - 0.25).abs() < 1e-5, "{follow:?}");
        assert_eq!(follow.loops, 2, "both extra leader laps kept");
    }

    #[test]
    fn marker_sync_keeps_the_leaders_extra_laps_in_reverse() {
        let setup = setup();
        let walk = AnimationClip::empty("walk", 1.0);
        let run = AnimationClip::empty("run", 0.6);
        // A reverse leader step that rewinds two laps keeps its negative loop count, like the
        // normalised-phase branch (correctness audit, finding 2, reverse direction).
        let step = ClipStep {
            prev: 0.25,
            time: 0.0,
            loops: -2,
        };
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.25, true);
        assert!((follow.time - 0.1).abs() < 1e-5, "{follow:?}");
        assert_eq!(follow.loops, -2, "reverse laps kept: {follow:?}");
    }

    #[test]
    fn marker_sync_counts_a_shared_single_wrap_once() {
        let setup = setup();
        let walk = AnimationClip::empty("walk", 1.0);
        let run = AnimationClip::empty("run", 0.6);
        // The leader wraps once (0.9 -> 1.1 unwrapped); the follower, snapping to the same
        // cycle position, wraps once too (0.55 -> 0.76 unwrapped). That shared wrap is ONE
        // lap of root motion, not two (review sprint-rev-kfix-app-anim-audio, finding 1).
        let step = ClipStep::advance(0.9, 0.2, 1.0, true);
        assert_eq!(step.loops, 1);
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.55, true);
        assert!((follow.time - 0.16).abs() < 1e-5, "{follow:?}");
        assert_eq!(follow.loops, 1, "one shared wrap, not two: {follow:?}");
    }

    #[test]
    fn marker_sync_counts_a_shared_reverse_wrap_once() {
        let setup = setup();
        let walk = AnimationClip::empty("walk", 1.0);
        let run = AnimationClip::empty("run", 0.6);
        // Reverse mirror: the leader rewinds across its start once (0.1 -> -0.1 unwrapped)
        // and the follower follows across its own start once (0.0 -> -0.56 unwrapped).
        let step = ClipStep::advance(0.1, -0.2, 1.0, true);
        assert_eq!(step.loops, -1);
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.0, true);
        assert!((follow.time - 0.04).abs() < 1e-5, "{follow:?}");
        assert_eq!(follow.loops, -1, "one shared reverse wrap: {follow:?}");
    }

    #[test]
    fn marker_sync_keeps_extra_laps_beyond_the_shared_wrap() {
        let setup = setup();
        let walk = AnimationClip::empty("walk", 1.0);
        let run = AnimationClip::empty("run", 0.6);
        // Multi-lap: the leader crosses its end twice (0.9 -> 2.3 unwrapped). The wrap the
        // follower shares is one of those laps; the OTHER whole lap still rides along, so the
        // follower reports two laps, not three.
        let step = ClipStep::advance(0.9, 1.4, 1.0, true);
        assert_eq!(step.loops, 2);
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.5, true);
        assert!((follow.time - 0.28).abs() < 1e-5, "{follow:?}");
        assert_eq!(
            follow.loops, 2,
            "one extra leader lap plus the shared wrap: {follow:?}"
        );
    }

    #[test]
    fn length_sync_without_markers_keeps_the_normalised_phase() {
        let setup = SyncSetup::default();
        let walk = AnimationClip::empty("walk", 1.0);
        let run = AnimationClip::empty("run", 0.6);
        let step = ClipStep {
            prev: 0.9,
            time: 0.1,
            loops: 1,
        };
        let follow = follow_step(&setup, clock(&walk), &step, clock(&run), 0.0, true);
        assert!((follow.prev - 0.54).abs() < 1e-5 && (follow.time - 0.06).abs() < 1e-5);
        assert_eq!(follow.loops, 1);
    }

    #[test]
    fn election_prefers_always_lead_then_weight() {
        let setup = setup();
        let mut groups = SyncGroups::default();
        groups.begin_frame();
        groups.record(
            &setup,
            "gait",
            SyncMember {
                clip: "walk",
                weight: 0.7,
                time: 0.1,
                speed: 1.0,
                looping: true,
                duration: 1.0,
            },
        );
        groups.record(
            &setup,
            "gait",
            SyncMember {
                clip: "run",
                weight: 0.3,
                time: 0.2,
                speed: 1.0,
                looping: true,
                duration: 1.0,
            },
        );
        groups.end_frame();
        assert_eq!(
            groups.groups()[0].leader.as_ref().map(|l| l.clip.as_str()),
            Some("walk")
        );
        groups.begin_frame();
        groups.record(
            &setup,
            "gait",
            SyncMember {
                clip: "walk",
                weight: 0.7,
                time: 0.1,
                speed: 1.0,
                looping: true,
                duration: 1.0,
            },
        );
        groups.record(
            &setup,
            "gait",
            SyncMember {
                clip: "start",
                weight: 0.3,
                time: 0.2,
                speed: 1.0,
                looping: false,
                duration: 1.0,
            },
        );
        groups.end_frame();
        assert_eq!(
            groups.groups()[0].leader.as_ref().map(|l| l.clip.as_str()),
            Some("start")
        );
        groups.begin_frame();
        groups.end_frame();
        assert!(groups.groups().is_empty(), "a group nobody played is dropped");
    }

    #[test]
    fn ron_round_trip_and_validation() {
        let setup = setup();
        let text = setup.to_ron_string().expect("write");
        assert_eq!(SyncSetup::from_ron_str(&text).expect("read"), setup);
        let mut bad = setup;
        bad.markers[0].markers[0].time = f32::NAN;
        assert!(bad.validate().is_err());
        assert!(SyncSetup::from_ron_str("(format_version: 9)").is_err());
        let fp = sync_schema_fingerprint();
        assert!(fp.contains("markers") && fp.contains("AlwaysLead"), "{fp}");
    }
}
