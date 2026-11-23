// SPDX-License-Identifier: MPL-2.0
//! The animation recorder (spec addendum § 11.1, P-206): while play-in-editor runs, record the
//! watched animator's debug snapshots (current state, transition, parameters, node blend weights)
//! and its evaluated pose into a ring of frames; after pause or stop, scrub back through them. A
//! scrubbed frame replaces the live snapshot in the graph panel (states and nodes highlight as
//! recorded) and poses the graph preview.
//!
//! Memory is bounded twice: by a frame count ([`AnimRecorder::capacity`], at most
//! [`MAX_CAPACITY`]) and by [`BYTE_CAP`] bytes; the oldest frames go first and are counted in
//! [`AnimRecorder::dropped`]. Every frame's size is counted ([`RecordedFrame::bytes`]), so
//! [`AnimRecorder::bytes`] is the ring's footprint. Recording follows the play world's frame
//! counter: a paused session adds nothing. The recorder is view state: it never touches a
//! document and is never on the undo stack.

use std::collections::VecDeque;

use regia_animation::Animator;
use regia_animation::debug::{GraphDebug, NodeDebugState};
use regia_animation::pose::Pose;
use regia_ecs::prelude::*;

use super::{AnimEditor, PieDebug};

/// Frames kept when a recording starts without a capacity (10 s at 60 Hz).
pub const DEFAULT_CAPACITY: usize = 600;
/// The largest ring a recording may ask for (10 minutes at 60 Hz).
pub const MAX_CAPACITY: usize = 36_000;
/// The ring never holds more than this many bytes, whatever the frame count.
pub const BYTE_CAP: usize = 64 << 20;

/// One recorded frame of the watched animator.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedFrame {
    /// Position in the recording (0 for the first frame recorded, counting dropped ones).
    pub index: u64,
    /// The play world's frame counter when it was recorded, when the world has one.
    pub play_frame: Option<u64>,
    /// The animator's entity (bits) in the play world.
    pub entity: u64,
    /// The entity's display name.
    pub name: Option<String>,
    /// The runtime snapshot: nodes with weights and states, parameters.
    pub debug: GraphDebug,
    /// The evaluated local pose.
    pub pose: Pose,
    /// [`pose_hash`] of `pose`.
    pub pose_hash: u64,
    /// Counted heap and inline size of this frame, bytes.
    pub bytes: usize,
}

impl RecordedFrame {
    /// A frame from a snapshot and a pose (sizes and hash computed).
    #[must_use]
    pub fn new(index: u64, play_frame: Option<u64>, pie: &PieDebug, pose: Pose) -> Self {
        let mut frame = Self {
            index,
            play_frame,
            entity: pie.entity,
            name: pie.name.clone(),
            debug: pie.debug.clone(),
            pose_hash: pose_hash(&pose),
            pose,
            bytes: 0,
        };
        frame.bytes = frame_bytes(&frame);
        frame
    }

    /// The current state of the first state machine, as recorded.
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        self.debug.first_state()
    }

    /// The first state machine's transition in progress: `(from, to, progress)`.
    #[must_use]
    pub fn transition(&self) -> Option<(&str, &str, f32)> {
        self.debug.nodes.iter().find_map(|n| match &n.state {
            NodeDebugState::StateMachine {
                transition: Some((from, to, progress)),
                ..
            } => Some((from.as_str(), to.as_str(), *progress)),
            _ => None,
        })
    }

    /// `(node path, weight)` of every node that contributed to the pose.
    #[must_use]
    pub fn blend_weights(&self) -> Vec<(&str, f32)> {
        self.debug
            .nodes
            .iter()
            .filter(|n| n.active)
            .map(|n| (n.path.as_str(), n.weight))
            .collect()
    }

    /// The snapshot as the graph panel's PIE debug (what a scrubbed frame shows).
    #[must_use]
    pub fn pie_debug(&self) -> PieDebug {
        PieDebug {
            entity: self.entity,
            name: self.name.clone(),
            debug: self.debug.clone(),
        }
    }
}

/// A 64-bit FNV-1a hash of a pose's bits (translations, rotations, scales in joint order):
/// equal poses hash equal on every platform.
#[must_use]
pub fn pose_hash(pose: &Pose) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut eat = |value: f32| {
        for byte in value.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    for t in &pose.translations {
        t.to_array().into_iter().for_each(&mut eat);
    }
    for r in &pose.rotations {
        r.to_array().into_iter().for_each(&mut eat);
    }
    for s in &pose.scales {
        s.to_array().into_iter().for_each(&mut eat);
    }
    hash
}

/// The counted size of a frame: the struct itself plus the heap it owns.
fn frame_bytes(frame: &RecordedFrame) -> usize {
    use std::mem::size_of;
    let pose = frame.pose.translations.len() * size_of::<regia_core::math::Vec3>() * 2
        + frame.pose.rotations.len() * size_of::<regia_core::math::Quat>();
    let nodes: usize = frame
        .debug
        .nodes
        .iter()
        .map(|n| {
            size_of::<regia_animation::debug::NodeDebug>()
                + n.path.len()
                + match &n.state {
                    NodeDebugState::Clip { clip, .. } => clip.len(),
                    NodeDebugState::BlendSpace { weights, .. } => weights
                        .iter()
                        .map(|(c, _)| size_of::<(String, f32)>() + c.len())
                        .sum(),
                    NodeDebugState::StateMachine {
                        current, transition, ..
                    } => current.len() + transition.as_ref().map_or(0, |(a, b, _)| a.len() + b.len()),
                    NodeDebugState::Layers { weights } => weights.len() * size_of::<f32>(),
                    NodeDebugState::Blend { .. }
                    | NodeDebugState::Wrapper
                    | NodeDebugState::StrideWarp { .. }
                    | NodeDebugState::OrientationWarp { .. } => 0,
                }
        })
        .sum();
    let parameters: usize = frame
        .debug
        .parameters
        .iter()
        .map(|(name, _)| size_of::<(String, regia_animation::params::ParamValue)>() + name.len())
        .sum();
    size_of::<RecordedFrame>() + pose + nodes + parameters + frame.name.as_ref().map_or(0, String::len)
}

/// The recording: a bounded ring of [`RecordedFrame`]s and the scrub position.
#[derive(Clone, Debug, Default)]
pub struct AnimRecorder {
    recording: bool,
    capacity: usize,
    frames: VecDeque<RecordedFrame>,
    next_index: u64,
    dropped: u64,
    bytes: usize,
    scrub: Option<usize>,
    last_play_frame: Option<u64>,
}

impl AnimRecorder {
    /// Starts a new recording of up to `capacity` frames (0 = [`DEFAULT_CAPACITY`]; clamped to
    /// [`MAX_CAPACITY`]); the previous recording is dropped.
    pub fn start(&mut self, capacity: usize) {
        *self = Self {
            recording: true,
            capacity: if capacity == 0 {
                DEFAULT_CAPACITY
            } else {
                capacity.min(MAX_CAPACITY)
            },
            ..Self::default()
        };
    }

    /// Stops recording; the frames stay for scrubbing.
    pub fn stop(&mut self) {
        self.recording = false;
    }

    /// Whether frames are being recorded.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// The ring's frame capacity ([`DEFAULT_CAPACITY`] before the first recording).
    #[must_use]
    pub fn capacity(&self) -> usize {
        if self.capacity == 0 {
            DEFAULT_CAPACITY
        } else {
            self.capacity
        }
    }

    /// Recorded frames, oldest first.
    #[must_use]
    pub fn frames(&self) -> &VecDeque<RecordedFrame> {
        &self.frames
    }

    /// The counted bytes of every frame held.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Frames evicted (oldest first) to stay within the capacity or [`BYTE_CAP`].
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Adds a frame, evicting the oldest ones past the capacity or the byte cap. A new frame
    /// ends scrubbing (the view follows the live session again).
    pub fn push(&mut self, mut frame: RecordedFrame) {
        frame.index = self.next_index;
        self.next_index += 1;
        self.bytes += frame.bytes;
        self.frames.push_back(frame);
        while self.frames.len() > self.capacity() || (self.bytes > BYTE_CAP && self.frames.len() > 1) {
            if let Some(old) = self.frames.pop_front() {
                self.bytes -= old.bytes;
                self.dropped += 1;
            }
        }
        self.scrub = None;
    }

    /// Records one frame of the watched animator when recording and the play world has
    /// advanced since the last one (a paused session records nothing). Returns whether a frame
    /// was added.
    pub fn capture(&mut self, play: &World, pie: &PieDebug) -> bool {
        if !self.recording {
            return false;
        }
        let play_frame = play
            .get_resource::<regia_app::Time>()
            .map(regia_app::Time::frame_count);
        if play_frame.is_some() && play_frame == self.last_play_frame {
            return false;
        }
        let Some(pose) = crate::command::entity_from_bits(pie.entity)
            .and_then(|e| play.get::<Animator>(e))
            .map(|a| a.evaluated_pose().clone())
        else {
            return false;
        };
        self.last_play_frame = play_frame;
        self.push(RecordedFrame::new(0, play_frame, pie, pose));
        true
    }

    /// The scrubbed frame's position in [`Self::frames`], `None` when following the live session.
    #[must_use]
    pub fn scrub(&self) -> Option<usize> {
        self.scrub
    }

    /// Scrubs to frame `at` of the ring, or back to live with `None`.
    ///
    /// # Errors
    ///
    /// `at` is past the last recorded frame.
    pub fn set_scrub(&mut self, at: Option<usize>) -> Result<(), String> {
        if let Some(i) = at
            && i >= self.frames.len()
        {
            return Err(if self.frames.is_empty() {
                "nothing is recorded: record while play runs first".to_string()
            } else {
                format!("frame {i} is out of range (0..{} recorded)", self.frames.len())
            });
        }
        self.scrub = at;
        Ok(())
    }

    /// The scrubbed frame.
    #[must_use]
    pub fn scrub_frame(&self) -> Option<&RecordedFrame> {
        self.scrub.and_then(|i| self.frames.get(i))
    }
}

/// The Recorder section of the graph panel: record / stop, the ring's size and memory, and the
/// scrub bar (a focusable slider: arrow keys step one frame).
pub fn recorder_ui(ui: &mut egui::Ui, editor: &mut AnimEditor) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let watching = editor.pie_debug.is_some() && editor.recorder.scrub().is_none();
    ui.horizontal_wrapped(|ui| {
        ui.strong("Recorder");
        let recorder = &mut editor.recorder;
        if recorder.is_recording() {
            ui.colored_label(tokens.accents.alert, "Recording");
            if ui.button("Stop recording").clicked() {
                recorder.stop();
            }
        } else {
            let capacity_id = egui::Id::new("anim-recorder-capacity");
            let mut capacity: usize = ui.data(|d| d.get_temp(capacity_id)).unwrap_or(DEFAULT_CAPACITY);
            let field = ui.add(
                egui::DragValue::new(&mut capacity)
                    .range(1..=MAX_CAPACITY)
                    .suffix(" frames"),
            );
            crate::a11y_audit::name(field, egui::WidgetType::DragValue, "Recorder capacity");
            ui.data_mut(|d| d.insert_temp(capacity_id, capacity));
            if ui
                .add_enabled(watching, egui::Button::new("Record"))
                .on_disabled_hover_text("Start play first: the recorder follows the watched animator.")
                .clicked()
            {
                recorder.start(capacity);
            }
        }
    });
    let recorder = &mut editor.recorder;
    ui.weak(format!(
        "{} of {} frames · {:.1} KiB · {} dropped",
        recorder.frames().len(),
        recorder.capacity(),
        recorder.bytes() as f64 / 1024.0,
        recorder.dropped()
    ));
    if recorder.frames().is_empty() {
        return;
    }
    ui.horizontal(|ui| {
        let last = recorder.frames().len() - 1;
        let mut at = recorder.scrub().unwrap_or(last);
        let slider = ui.add(egui::Slider::new(&mut at, 0..=last).text("frame"));
        let slider = crate::a11y_audit::name(slider, egui::WidgetType::Slider, "Recorded frame");
        if slider.changed() {
            let _ = recorder.set_scrub(Some(at));
        }
        if ui
            .add_enabled(recorder.scrub().is_some(), egui::Button::new("Live"))
            .on_hover_text("Follow the running session again")
            .clicked()
        {
            let _ = recorder.set_scrub(None);
        }
    });
    if let Some(frame) = recorder.scrub_frame() {
        ui.colored_label(
            tokens.accents.select,
            format!(
                "Replaying frame {} · state {} · pose {:016x}",
                frame.index,
                frame.state().unwrap_or("-"),
                frame.pose_hash
            ),
        );
        if let Some((from, to, progress)) = frame.transition() {
            ui.weak(format!("transition {from} -> {to} ({:.0} %)", progress * 100.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::debug::NodeDebug;

    fn pie(state: &str, speed: f32) -> PieDebug {
        PieDebug {
            entity: 7,
            name: Some("Hero".into()),
            debug: GraphDebug {
                nodes: vec![NodeDebug {
                    path: "root".into(),
                    active: true,
                    weight: 1.0,
                    state: NodeDebugState::StateMachine {
                        current: state.into(),
                        transition: Some(("Idle".into(), state.into(), 0.25)),
                        blend: None,
                        inertial_offset: 0.0,
                    },
                }],
                parameters: vec![("Speed".into(), regia_animation::params::ParamValue::Float(speed))],
                ik: Vec::new(),
            },
        }
    }

    fn pose(x: f32) -> Pose {
        Pose {
            translations: vec![regia_core::math::Vec3::new(x, 0.0, 0.0); 3],
            rotations: vec![regia_core::math::Quat::IDENTITY; 3],
            scales: vec![regia_core::math::Vec3::ONE; 3],
        }
    }

    #[test]
    fn the_ring_is_bounded_counted_and_scrubs() {
        let mut recorder = AnimRecorder::default();
        recorder.start(4);
        for i in 0..10 {
            recorder.push(RecordedFrame::new(
                0,
                Some(i),
                &pie("Walk", i as f32),
                pose(i as f32),
            ));
        }
        assert_eq!(recorder.frames().len(), 4, "capacity bounds the ring");
        assert_eq!(recorder.dropped(), 6);
        assert_eq!(recorder.frames()[0].index, 6, "the oldest frames went first");
        let counted: usize = recorder.frames().iter().map(|f| f.bytes).sum();
        assert_eq!(recorder.bytes(), counted);
        assert!(recorder.bytes() > 4 * std::mem::size_of::<RecordedFrame>());
        assert!(recorder.set_scrub(Some(4)).is_err());
        recorder.set_scrub(Some(1)).expect("scrub");
        let frame = recorder.scrub_frame().expect("frame");
        assert_eq!(frame.state(), Some("Walk"));
        assert_eq!(frame.transition(), Some(("Idle", "Walk", 0.25)));
        assert_eq!(frame.blend_weights(), vec![("root", 1.0)]);
        assert_eq!(
            frame.debug.parameters[0].1,
            regia_animation::params::ParamValue::Float(7.0)
        );
        assert_eq!(frame.pose_hash, pose_hash(&pose(7.0)));
        assert_ne!(pose_hash(&pose(7.0)), pose_hash(&pose(8.0)));
        // A new frame ends scrubbing.
        recorder.push(RecordedFrame::new(0, Some(99), &pie("Run", 1.0), pose(1.0)));
        assert_eq!(recorder.scrub(), None);
        // Capacity clamps; zero means the default.
        recorder.start(MAX_CAPACITY + 5);
        assert_eq!(recorder.capacity(), MAX_CAPACITY);
        recorder.start(0);
        assert_eq!(recorder.capacity(), DEFAULT_CAPACITY);
        assert!(recorder.frames().is_empty());
    }

    #[test]
    fn the_byte_cap_bounds_a_ring_of_large_frames() {
        let mut recorder = AnimRecorder::default();
        recorder.start(MAX_CAPACITY);
        let big = Pose {
            translations: vec![regia_core::math::Vec3::ZERO; 20_000],
            rotations: vec![regia_core::math::Quat::IDENTITY; 20_000],
            scales: vec![regia_core::math::Vec3::ONE; 20_000],
        };
        let frame = RecordedFrame::new(0, None, &pie("Walk", 0.0), big);
        let per = frame.bytes;
        for _ in 0..(BYTE_CAP / per + 10) {
            recorder.push(frame.clone());
        }
        assert!(recorder.bytes() <= BYTE_CAP, "{} > {BYTE_CAP}", recorder.bytes());
        assert!(recorder.dropped() > 0);
    }

    #[test]
    fn capture_follows_the_play_frame_counter() {
        let mut play = World::new();
        let entity = play
            .spawn(Animator::single(
                regia_animation::clip::AnimationClip::empty("idle", 1.0),
                true,
            ))
            .id();
        let mut watched = pie("Walk", 1.0);
        watched.entity = entity.to_bits();
        let mut recorder = AnimRecorder::default();
        assert!(!recorder.capture(&play, &watched), "not recording");
        recorder.start(10);
        play.insert_resource(regia_app::Time::default());
        assert!(recorder.capture(&play, &watched));
        assert!(
            !recorder.capture(&play, &watched),
            "a paused session (same frame) records nothing"
        );
        assert_eq!(recorder.frames().len(), 1);
        recorder.stop();
        assert!(!recorder.capture(&play, &watched));
    }
}
