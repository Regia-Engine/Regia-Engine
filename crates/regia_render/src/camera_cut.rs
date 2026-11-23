// SPDX-License-Identifier: MPL-2.0
//! Camera cuts: dropping every temporal history when the view changes discontinuously.
//!
//! A frame blends in what earlier frames left behind: the temporal upscaler's history, the
//! froxel fog's integrated history, the auto-exposure meter, the static-screen SSGI/SSR cache,
//! the cached shadow cascades and pages. Across a camera cut (cockpit → chase, a replay cut,
//! menu → map) that history belongs to another view or another scene, and up to a tenth of the
//! previous image stayed visible four frames later (look lane, 2026-10-08).
//!
//! [`crate::FrameRenderer::camera_cut`] resets all of it before the next frame, so the first
//! frame after a cut is the first frame a fresh renderer would draw. The renderer's detector calls the
//! same reset on its own when the eye jumps, the view turns too far in one frame, or the
//! submitted objects are (almost) all new: a scene change.

use crate::key_hash::KeyMap;

/// When the renderer treats a frame as a camera cut without being told
/// ([`crate::FrameRenderer::set_camera_cut_detection`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraCutDetection {
    /// The eye moving further than this (metres) between two frames is a cut. Fast vehicles
    /// move a few metres per frame; a teleport or a camera switch moves tens to thousands.
    pub max_eye_jump_m: f32,
    /// The view direction turning more than this (degrees) between two frames is a cut.
    pub max_turn_degrees: f32,
    /// A frame is a scene change when less than this share of its objects were submitted
    /// last frame AND less than this share of last frame's objects are still submitted
    /// (both, so a large spawn into a kept scene is not a cut). `0` disables the check.
    pub max_kept_share: f32,
}

impl Default for CameraCutDetection {
    fn default() -> Self {
        Self {
            max_eye_jump_m: 50.0,
            max_turn_degrees: 90.0,
            max_kept_share: 0.25,
        }
    }
}

/// Why a frame started with a camera cut ([`crate::FrameRenderer::last_camera_cut`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraCutReason {
    /// The host called [`crate::FrameRenderer::camera_cut`] (or `render.camera_cut`).
    Requested,
    /// The eye moved further than [`CameraCutDetection::max_eye_jump_m`].
    EyeJump,
    /// The view turned more than [`CameraCutDetection::max_turn_degrees`].
    Turn,
    /// Most submitted objects are new and most of last frame's are gone.
    SceneChange,
}

impl CameraCutReason {
    /// Stable lower-case name (agent protocol, logs).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::EyeJump => "eye_jump",
            Self::Turn => "turn",
            Self::SceneChange => "scene_change",
        }
    }
}

/// What a camera cut does to auto exposure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CutExposure {
    /// Forget the metered luminance: the new view starts at the initial exposure and adopts
    /// its own first histogram outright, as a fresh renderer does. The default.
    #[default]
    Snap,
    /// Keep adapting from the previous view's exposure (a deliberate eye-adaptation fade).
    Fade,
}

/// Objects at or under this count are all compared; larger frames compare a key-hash sample.
const COMPARE_ALL_MAX: usize = 512;

/// One key in this many is sampled in frames above [`COMPARE_ALL_MAX`] objects.
const SAMPLE_SHIFT: u32 = 6;

/// Whether `key` belongs to the fixed scene-change sample (the same keys every frame).
fn sampled(key: u64) -> bool {
    // Fx-style mix so sequential ids spread over the sample.
    (key.wrapping_mul(0x51_7c_c1_b7_27_22_0a_95) >> (64 - SAMPLE_SHIFT)) == 0
}

/// The view a frame is drawn from, as the detector compares it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CutView {
    /// Eye position (world).
    pub eye: [f32; 3],
    /// Point looked at (world).
    pub target: [f32; 3],
}

impl CutView {
    fn forward(&self) -> Option<[f32; 3]> {
        let d = [
            self.target[0] - self.eye[0],
            self.target[1] - self.eye[1],
            self.target[2] - self.eye[2],
        ];
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        (len > 1.0e-6 && len.is_finite()).then(|| [d[0] / len, d[1] / len, d[2] / len])
    }
}

/// One identity per object, this frame's and last frame's, compared for turnover.
#[derive(Default)]
struct TurnoverSet {
    /// Last frame's ids (all, or the sample when `last_all` is false).
    last: KeyMap<u64, ()>,
    last_all: bool,
    /// This frame's ids (swapped into `last` at the end of the frame).
    now: KeyMap<u64, ()>,
    now_all: bool,
}

impl TurnoverSet {
    fn begin(&mut self, all: bool) {
        self.now.clear();
        self.now_all = all;
    }

    fn insert(&mut self, id: u64) {
        if self.now_all || sampled(id) {
            self.now.insert(id, ());
        }
    }

    fn end(&mut self) {
        std::mem::swap(&mut self.now, &mut self.last);
        self.last_all = self.now_all;
    }

    /// Whether less than `max_kept` of this frame's ids were in last frame's AND less than
    /// that share of last frame's remain. `None` when either frame has no ids to compare.
    fn turned_over(&self, max_kept: f32) -> Option<bool> {
        // Compare on the sample unless both frames recorded every id.
        let both_all = self.last_all && self.now_all;
        let in_compare = |k: &u64| both_all || sampled(*k);
        let now = self.now.keys().filter(|k| in_compare(k)).count();
        let before = self.last.keys().filter(|k| in_compare(k)).count();
        if now == 0 || before == 0 {
            return None;
        }
        let kept = self
            .now
            .keys()
            .filter(|k| in_compare(k) && self.last.contains_key(k))
            .count() as f32;
        Some(kept < max_kept * now as f32 && kept < max_kept * before as f32)
    }
}

/// What the scene-change test knows about one submitted object.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CutObject {
    /// Stable object key; `None` for an ephemeral key (no identity across frames).
    pub key: Option<u64>,
    /// Content signature: the model matrix and the vertex and index counts
    /// ([`content_signature`]). The same object rebuilt under a new key keeps it.
    pub content: u64,
}

/// Content signature of an object: its model matrix, vertex and index counts, and the
/// positions of its first, middle and last vertices (meshes baked into world space all carry
/// the identity matrix). Constant time per object.
pub(crate) fn content_signature(model: &[[f32; 4]; 4], vertices: &[crate::LabVertex], indices: usize) -> u64 {
    let mut h = (vertices.len() as u64) ^ ((indices as u64) << 32);
    let mut add = |value: f32| {
        h = (h.rotate_left(5) ^ u64::from(value.to_bits())).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    };
    model.iter().flatten().for_each(|&v| add(v));
    if let (Some(first), Some(last)) = (vertices.first(), vertices.last()) {
        let middle = &vertices[vertices.len() / 2];
        for vertex in [first, middle, last] {
            vertex.position.iter().for_each(|&v| add(v));
        }
    }
    h
}

/// Compares each frame's view and objects with the last frame's.
#[derive(Default)]
pub(crate) struct CutDetector {
    last_view: Option<CutView>,
    /// Object keys: stable while an object lives, also when it moves.
    keys: TurnoverSet,
    /// Content signatures: stable while an object stands still, also when a host rebuilds
    /// its scene every frame under new keys (a lab scene's keys are addresses).
    content: TurnoverSet,
}

impl CutDetector {
    /// Records this frame's view and objects and returns why it is a cut, if it is one.
    /// `count` is the frame's object count (it picks every object or a fixed sample). The
    /// first frame is never a cut (there is nothing to drop).
    pub(crate) fn observe(
        &mut self,
        settings: Option<&CameraCutDetection>,
        view: CutView,
        count: usize,
        objects: impl Iterator<Item = CutObject>,
    ) -> Option<CameraCutReason> {
        let all = count <= COMPARE_ALL_MAX;
        self.keys.begin(all);
        self.content.begin(all);
        for object in objects {
            if let Some(key) = object.key {
                self.keys.insert(key);
            }
            self.content.insert(object.content);
        }
        let reason = settings.and_then(|s| self.classify(s, view));
        self.keys.end();
        self.content.end();
        self.last_view = Some(view);
        reason
    }

    fn classify(&self, s: &CameraCutDetection, view: CutView) -> Option<CameraCutReason> {
        let last = self.last_view?;
        let d = [
            view.eye[0] - last.eye[0],
            view.eye[1] - last.eye[1],
            view.eye[2] - last.eye[2],
        ];
        let jump = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if jump > s.max_eye_jump_m {
            return Some(CameraCutReason::EyeJump);
        }
        if let (Some(a), Some(b)) = (last.forward(), view.forward()) {
            let cos = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0);
            if cos.acos().to_degrees() > s.max_turn_degrees {
                return Some(CameraCutReason::Turn);
            }
        }
        if s.max_kept_share > 0.0 {
            // A scene change replaces both what is drawn and where: moving objects keep their
            // keys, a rebuilt scene keeps its content. Without keys to compare, content decides.
            let content = self.content.turned_over(s.max_kept_share);
            let keys = self.keys.turned_over(s.max_kept_share);
            if content == Some(true) && keys != Some(false) {
                return Some(CameraCutReason::SceneChange);
            }
        }
        None
    }
}

/// Views tracked at once (split-screen viewports of one renderer); the oldest is dropped.
const MAX_TRACKED_VIEWS: usize = 4;

/// The renderer's camera-cut bookkeeping ([`crate::FrameRenderer::camera_cut`]).
pub(crate) struct CutState {
    /// Automatic detection; `None` = explicit cuts only.
    pub(crate) detection: Option<CameraCutDetection>,
    /// One detector per view (target size and viewport), so split-screen viewports drawn by
    /// one renderer compare each with its own last frame.
    detectors: Vec<([u32; 6], CutDetector)>,
    /// A cut the host asked for, applied at the start of the next tracked frame.
    pub(crate) pending: Option<CutExposure>,
    /// Nesting depth of frames that are not the host's view (probe faces, texture views):
    /// they neither detect nor consume a pending cut.
    pub(crate) suspended: u32,
    /// Frame index and reason of the last cut.
    pub(crate) last: Option<(u64, CameraCutReason)>,
    /// Cuts since the renderer was built.
    pub(crate) count: u64,
    /// Frame index before the last cut's frame: temporal sequences (jitter, noise phase)
    /// count from it, so a cut restarts them as a fresh renderer starts them.
    pub(crate) origin: u64,
}

impl Default for CutState {
    fn default() -> Self {
        Self {
            detection: Some(CameraCutDetection::default()),
            detectors: Vec::new(),
            pending: None,
            suspended: 0,
            last: None,
            count: 0,
            origin: 0,
        }
    }
}

impl CutState {
    /// Decides whether the frame about to be drawn of view `view_key` starts with a cut:
    /// a pending request, else what detection sees. `objects` as in [`CutDetector::observe`].
    pub(crate) fn begin_frame(
        &mut self,
        view_key: [u32; 6],
        view: CutView,
        count: usize,
        objects: impl Iterator<Item = CutObject>,
    ) -> Option<(CameraCutReason, CutExposure)> {
        if self.suspended > 0 {
            return None;
        }
        let index = match self.detectors.iter().position(|(key, _)| *key == view_key) {
            Some(index) => index,
            None => {
                if self.detectors.len() >= MAX_TRACKED_VIEWS {
                    self.detectors.remove(0);
                }
                self.detectors.push((view_key, CutDetector::default()));
                self.detectors.len() - 1
            }
        };
        let detected = self.detectors[index]
            .1
            .observe(self.detection.as_ref(), view, count, objects);
        match self.pending.take() {
            Some(exposure) => Some((CameraCutReason::Requested, exposure)),
            None => detected.map(|reason| (reason, CutExposure::Snap)),
        }
    }

    /// A floating-origin rebase moved every position by `-shift` (spec 78 § 12.2): each view's last
    /// eye and target move with it, so the rebase frame is not taken for an eye jump. Object keys
    /// are stable across it, so the scene-change check keeps them too.
    pub(crate) fn shift_origin(&mut self, shift: [f32; 3]) {
        for (_, detector) in &mut self.detectors {
            if let Some(view) = &mut detector.last_view {
                for ((eye, target), amount) in view.eye.iter_mut().zip(view.target.iter_mut()).zip(shift) {
                    *eye -= amount;
                    *target -= amount;
                }
            }
        }
    }

    /// Records a cut on the frame that will be `frame_index + 1`.
    pub(crate) fn note(&mut self, frame_index: u64, reason: CameraCutReason) {
        self.origin = frame_index;
        self.last = Some((frame_index + 1, reason));
        self.count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(eye: [f32; 3], target: [f32; 3]) -> CutView {
        CutView { eye, target }
    }

    /// Objects whose content is tied to their key (a static object per key).
    fn objects(keys: &[u64]) -> Vec<CutObject> {
        keys.iter()
            .map(|&k| CutObject {
                key: Some(k),
                content: k ^ 0xA5A5_0000,
            })
            .collect()
    }

    fn observe(d: &mut CutDetector, v: CutView, objects: &[CutObject]) -> Option<CameraCutReason> {
        d.observe(
            Some(&CameraCutDetection::default()),
            v,
            objects.len(),
            objects.iter().copied(),
        )
    }

    fn run(d: &mut CutDetector, v: CutView, keys: &[u64]) -> Option<CameraCutReason> {
        observe(d, v, &objects(keys))
    }

    #[test]
    fn first_frame_and_small_motion_are_not_cuts() {
        let mut d = CutDetector::default();
        let keys = [1, 2, 3];
        assert_eq!(run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &keys), None);
        // A vehicle at 300 m/s, 40 fps, turning 10 degrees per frame.
        let mut eye = [0.0f32; 3];
        let mut yaw = 0.0f32;
        for _ in 0..20 {
            eye[2] -= 7.5;
            yaw += 10.0f32.to_radians();
            let t = [eye[0] + yaw.sin(), eye[1], eye[2] - yaw.cos()];
            assert_eq!(run(&mut d, view(eye, t), &keys), None);
        }
    }

    #[test]
    fn jump_turn_and_scene_change_are_cuts() {
        let mut d = CutDetector::default();
        let keys = [1, 2, 3, 4];
        run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &keys);
        assert_eq!(
            run(&mut d, view([0.0, 0.0, 400.0], [0.0, 0.0, 399.0]), &keys),
            Some(CameraCutReason::EyeJump)
        );
        assert_eq!(
            run(&mut d, view([0.0, 0.0, 400.0], [0.0, 0.0, 401.0]), &keys),
            Some(CameraCutReason::Turn)
        );
        assert_eq!(
            run(&mut d, view([0.0, 0.0, 400.0], [0.0, 0.0, 401.0]), &[7, 8, 9, 10]),
            Some(CameraCutReason::SceneChange)
        );
        // Disabled detection reports nothing but still tracks the frame.
        assert_eq!(
            d.observe(
                None,
                view([1.0e4; 3], [0.0; 3]),
                2,
                objects(&[20, 21]).into_iter()
            ),
            None
        );
    }

    #[test]
    fn a_large_spawn_into_a_kept_scene_is_not_a_cut() {
        let mut d = CutDetector::default();
        let old: Vec<u64> = (1..=10).collect();
        run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &old);
        // Ten old objects stay, two hundred appear: most of the frame is new, but the old
        // scene is still all there.
        let spawned: Vec<u64> = (1..=210).collect();
        assert_eq!(run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &spawned), None);
        // Half the objects despawn: not a cut either.
        assert_eq!(
            run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &spawned[..105]),
            None
        );
    }

    #[test]
    fn a_rebuilt_scene_or_moving_objects_are_not_a_scene_change() {
        let v = view([0.0; 3], [0.0, 0.0, -1.0]);
        // The same objects rebuilt every frame under new keys (a lab scene's keys are addresses).
        let mut d = CutDetector::default();
        for frame in 0..6u64 {
            let rebuilt: Vec<CutObject> = (0..5u64)
                .map(|i| CutObject {
                    key: Some(1_000 * frame + i),
                    content: 77 + i,
                })
                .collect();
            assert_eq!(observe(&mut d, v, &rebuilt), None, "frame {frame}");
        }
        // Every object moving (new content each frame) under stable keys.
        let mut d = CutDetector::default();
        for frame in 0..6u64 {
            let moving: Vec<CutObject> = (0..5u64)
                .map(|i| CutObject {
                    key: Some(i + 1),
                    content: 1_000 * frame + i,
                })
                .collect();
            assert_eq!(observe(&mut d, v, &moving), None, "frame {frame}");
        }
        // Objects without keys: the content decides.
        let mut d = CutDetector::default();
        let keyless = |base: u64| -> Vec<CutObject> {
            (0..5u64)
                .map(|i| CutObject {
                    key: None,
                    content: base + i,
                })
                .collect()
        };
        assert_eq!(observe(&mut d, v, &keyless(10)), None);
        assert_eq!(observe(&mut d, v, &keyless(10)), None);
        assert_eq!(
            observe(&mut d, v, &keyless(500)),
            Some(CameraCutReason::SceneChange)
        );
    }

    #[test]
    fn content_signature_follows_the_object() {
        let mut model = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0; 4],
        ];
        model[3][3] = 1.0;
        let quad = |x: f32| -> Vec<crate::LabVertex> {
            (0..4)
                .map(|i| {
                    crate::LabVertex::from_pos_normal_uv([x + i as f32, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0; 2])
                })
                .collect()
        };
        let a = content_signature(&model, &quad(0.0), 6);
        assert_eq!(a, content_signature(&model, &quad(0.0), 6), "a rebuilt copy");
        assert_ne!(a, content_signature(&model, &quad(0.0), 3));
        assert_ne!(a, content_signature(&model, &quad(5.0), 6), "baked elsewhere");
        model[3][0] = 0.01;
        assert_ne!(a, content_signature(&model, &quad(0.0), 6), "moved");
    }

    #[test]
    fn large_frames_compare_a_stable_sample() {
        let mut d = CutDetector::default();
        let a: Vec<u64> = (1..=20_000).collect();
        let b: Vec<u64> = (100_001..=120_000).collect();
        run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &a);
        assert_eq!(run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &a), None);
        // The sample holds about 1/64 of the keys.
        assert!(
            d.keys.last.len() > 200 && d.keys.last.len() < 450,
            "{}",
            d.keys.last.len()
        );
        assert_eq!(
            run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &b),
            Some(CameraCutReason::SceneChange)
        );
        // Crossing the all/sample boundary keeps comparing like with like.
        let small: Vec<u64> = b[..400].to_vec();
        assert_eq!(run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &small), None);
        assert_eq!(run(&mut d, view([0.0; 3], [0.0, 0.0, -1.0]), &b), None);
    }

    #[test]
    fn pending_cuts_wait_for_a_tracked_view() {
        let mut state = CutState::default();
        let v = view([0.0; 3], [0.0, 0.0, -1.0]);
        assert_eq!(state.begin_frame([0; 6], v, 1, objects(&[1]).into_iter()), None);
        state.pending = Some(CutExposure::Fade);
        state.suspended = 1;
        assert_eq!(state.begin_frame([1; 6], v, 1, objects(&[1]).into_iter()), None);
        state.suspended = 0;
        assert_eq!(
            state.begin_frame([0; 6], v, 1, objects(&[1]).into_iter()),
            Some((CameraCutReason::Requested, CutExposure::Fade))
        );
        // Another viewport's first frame is not a cut, and does not disturb the first view.
        let far = view([500.0; 3], [500.0, 500.0, 499.0]);
        assert_eq!(state.begin_frame([2; 6], far, 1, objects(&[1]).into_iter()), None);
        assert_eq!(state.begin_frame([0; 6], v, 1, objects(&[1]).into_iter()), None);
        assert_eq!(
            state.begin_frame([0; 6], far, 1, objects(&[1]).into_iter()),
            Some((CameraCutReason::EyeJump, CutExposure::Snap))
        );
        state.note(41, CameraCutReason::EyeJump);
        assert_eq!(
            (state.origin, state.last, state.count),
            (41, Some((42, CameraCutReason::EyeJump)), 1)
        );
    }
}
