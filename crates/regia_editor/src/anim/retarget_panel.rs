// SPDX-License-Identifier: MPL-2.0
//! The Retarget panel (`Panel::Retarget`, ADR-7801, `docs/specs/85-retarget-crowds.md` § 2.5,
//! `book/agents/retargeting.md`): pick a source and a target rig, map their bones by body chain,
//! align chains to a reference pose, preview a source clip on both rigs side by side, save the map
//! as a `.retarget.ron` setup and bake clips for the target in a cancellable editor job.
//!
//! - **Document.** The map is a [`RetargetSetup`] (format v2): both sides are
//!   `Explicit` role → joint maps, and each chain's rest-pose alignment is the `rest_offset_deg`
//!   of every role of the chain. Its `ik_goals` are edited in the goals list ([`goals`],
//!   ADR-8476), and the preview can show the bake with its goals solved. The runtime [`Retargeter`] built from the saved file is the one
//!   the preview and the batch use, so what the panel shows is what a game plays.
//! - **Auto-map** ([`auto_map`]). First by chain: both skeletons' humanoid roles are guessed from
//!   joint names (`RetargetProfile::guess_humanoid`, spec 85 § 2.1) and every role both rigs have
//!   is paired; the roles group into the chains root, spine, head, left/right arm and left/right
//!   leg ([`RetargetChain`]). Then by name: every target joint still unmapped pairs with the first
//!   unmapped source joint whose name matches after dropping a `namespace:` prefix, case and every
//!   character that is not a letter or a digit (`mixamorig:Tail1` ↔ `tail_1`), as a custom role
//!   named after the target joint.
//! - **Align** ([`RetargetPanel::align_chain`]). Sets a chain's rest offset so that, with the source
//!   in its bind pose, the target's aim bone points along the T-pose or A-pose direction
//!   ([`chain_alignment_offset`]).
//! - **Batch** ([`run_batch`]). "Retarget clips…" bakes each selected source clip with
//!   `Retargeter::retarget_clip`, encodes it as the cook encodes a retargeted BVH clip (the
//!   compressed `regia_animation::AnimationClip` payload in a cooked-file container) and writes
//!   `<out>/<clip>.cooked`. Every clip is staged first and moved into place only when all are
//!   baked, so a Cancel leaves no clip (and no partial file) behind.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::compress::CompressionSettings;
use regia_animation::pose::Pose;
use regia_animation::retarget::{HumanoidBone, RetargetProfile, Retargeter, RoleSettings};
use regia_animation::retarget_chain::{
    ReferencePose, RetargetChain, bind_bone_direction, chain_alignment_offset, rest_offset_degrees,
};
use regia_animation::retarget_setup::{RetargetSetup, RoleSource};
use regia_animation::skeleton::{LocalBind, Skeleton};
use regia_asset::AssetId;
use regia_asset::{COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::editor_jobs::WorkerJob;
use crate::viewport_agent::Handler;

pub mod goals;
#[cfg(test)]
mod tests;

/// The importer version written into the batch's cooked headers (1.0.0).
pub const BATCH_IMPORTER_VERSION: u32 = 1_000_000;

/// The job kind (and background-task id) of a "Retarget clips…" batch.
pub const BATCH_JOB_KIND: &str = "retarget_batch";

/// Built-in rigs the panel and `editor.retarget_open` accept instead of a file path.
pub const BUILTIN_RIGS: [&str; 3] = ["demo", "fixture:source", "fixture:target"];

// ---------------------------------------------------------------------------------------------
// Rigs
// ---------------------------------------------------------------------------------------------

/// One side of the pairing: a skeleton and the clips that came with it.
#[derive(Clone, Debug)]
pub struct RetargetRig {
    /// What the rig was opened from (a built-in name or the path as given).
    pub name: String,
    /// The skeleton.
    pub skeleton: Arc<Skeleton>,
    /// Its clips (the source side's are the ones the preview plays and the batch bakes).
    pub clips: Vec<Arc<AnimationClip>>,
}

impl RetargetRig {
    /// A built-in rig ([`BUILTIN_RIGS`]): the editor's demo biped, or the test fixture pair (a
    /// namespaced A-pose rig facing −Z with four clips, and a game-style T-pose rig facing +Z with
    /// twist joints).
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        let (skeleton, clips) = match name {
            "demo" => (super::demo_rig::skeleton(), super::demo_rig::clips()),
            "fixture:source" => (Arc::new(fixture_source_skeleton()), fixture_source_clips()),
            "fixture:target" => (Arc::new(fixture_target_skeleton()), Vec::new()),
            _ => return None,
        };
        Some(Self {
            name: name.to_string(),
            skeleton,
            clips,
        })
    }

    /// Loads a rig from a `.glb`/`.gltf` (its first skin and clips) or a `.bvh` (default import
    /// settings); `label` is what the panel shows.
    ///
    /// # Errors
    ///
    /// The file cannot be read, has an unknown extension, or does not import.
    pub fn load(path: &Path, label: &str) -> Result<Self, String> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match ext.as_str() {
            "glb" | "gltf" => {
                let rig = super::PreviewRig::from_gltf(path)?;
                Ok(Self {
                    name: label.to_string(),
                    skeleton: rig.skeleton,
                    clips: rig.clips,
                })
            }
            "bvh" => {
                use regia_asset_pipeline::bvh_import::{BvhImportSettings, convert_bvh};
                let bytes =
                    std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
                let import = convert_bvh(&bytes, stem, &BvhImportSettings::default())
                    .map_err(|e| format!("cannot import {}: {e}", path.display()))?;
                Ok(Self {
                    name: label.to_string(),
                    skeleton: Arc::new(import.skeleton),
                    clips: vec![Arc::new(import.clip)],
                })
            }
            _ => Err(format!("{} is not a .glb, .gltf or .bvh file", path.display())),
        }
    }

    /// The clip called `name`.
    #[must_use]
    pub fn clip(&self, name: &str) -> Option<&Arc<AnimationClip>> {
        self.clips.iter().find(|c| c.name == name)
    }
}

/// Builds a skeleton from `(name, parent, offset)` rows with identity bind rotations.
fn rig_from_rows(rows: &[(&str, Option<u32>, [f32; 3])]) -> Skeleton {
    let names = rows.iter().map(|r| r.0.to_string()).collect();
    let parents = rows.iter().map(|r| r.1).collect();
    let bind = rows
        .iter()
        .map(|r| LocalBind {
            translation: Vec3::from(r.2),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        })
        .collect();
    Skeleton::from_local_bind(names, parents, bind).unwrap_or_default()
}

/// The fixture source: Mixamo-style namespaced joint names, arms in an A-pose (45° down), facing
/// −Z (its left side is −X), with a two-joint tail.
#[must_use]
pub fn fixture_source_skeleton() -> Skeleton {
    let d = std::f32::consts::FRAC_1_SQRT_2;
    rig_from_rows(&[
        ("mixamorig:Hips", None, [0.0, 1.0, 0.0]),
        ("mixamorig:Spine", Some(0), [0.0, 0.1, 0.0]),
        ("mixamorig:Spine1", Some(1), [0.0, 0.12, 0.0]),
        ("mixamorig:Spine2", Some(2), [0.0, 0.12, 0.0]),
        ("mixamorig:Neck", Some(3), [0.0, 0.15, 0.0]),
        ("mixamorig:Head", Some(4), [0.0, 0.1, 0.0]),
        ("mixamorig:LeftShoulder", Some(3), [-0.08, 0.1, 0.0]),
        ("mixamorig:LeftArm", Some(6), [-0.1, 0.0, 0.0]),
        ("mixamorig:LeftForeArm", Some(7), [-0.27 * d, -0.27 * d, 0.0]),
        ("mixamorig:LeftHand", Some(8), [-0.25 * d, -0.25 * d, 0.0]),
        ("mixamorig:RightShoulder", Some(3), [0.08, 0.1, 0.0]),
        ("mixamorig:RightArm", Some(10), [0.1, 0.0, 0.0]),
        ("mixamorig:RightForeArm", Some(11), [0.27 * d, -0.27 * d, 0.0]),
        ("mixamorig:RightHand", Some(12), [0.25 * d, -0.25 * d, 0.0]),
        ("mixamorig:LeftUpLeg", Some(0), [-0.1, -0.05, 0.0]),
        ("mixamorig:LeftLeg", Some(14), [0.0, -0.42, 0.0]),
        ("mixamorig:LeftFoot", Some(15), [0.0, -0.42, 0.0]),
        ("mixamorig:LeftToeBase", Some(16), [0.0, -0.05, -0.12]),
        ("mixamorig:RightUpLeg", Some(0), [0.1, -0.05, 0.0]),
        ("mixamorig:RightLeg", Some(18), [0.0, -0.42, 0.0]),
        ("mixamorig:RightFoot", Some(19), [0.0, -0.42, 0.0]),
        ("mixamorig:RightToeBase", Some(20), [0.0, -0.05, -0.12]),
        ("mixamorig:Tail1", Some(0), [0.0, 0.0, 0.1]),
        ("mixamorig:Tail2", Some(22), [0.0, 0.0, 0.15]),
    ])
}

/// The fixture target: game-style names (`pelvis`, `spine_01`, `upperarm_l`, `ball_l`), a T-pose,
/// facing +Z (its left side is +X), a ground root, an upper-arm twist joint and a tail.
#[must_use]
pub fn fixture_target_skeleton() -> Skeleton {
    rig_from_rows(&[
        ("root", None, [0.0, 0.0, 0.0]),
        ("pelvis", Some(0), [0.0, 0.9, 0.0]),
        ("spine_01", Some(1), [0.0, 0.1, 0.0]),
        ("spine_02", Some(2), [0.0, 0.1, 0.0]),
        ("spine_03", Some(3), [0.0, 0.1, 0.0]),
        ("neck_01", Some(4), [0.0, 0.14, 0.0]),
        ("head", Some(5), [0.0, 0.1, 0.0]),
        ("clavicle_l", Some(4), [0.07, 0.09, 0.0]),
        ("upperarm_l", Some(7), [0.09, 0.0, 0.0]),
        ("lowerarm_l", Some(8), [0.25, 0.0, 0.0]),
        ("hand_l", Some(9), [0.23, 0.0, 0.0]),
        ("upperarm_twist_01_l", Some(8), [0.1, 0.0, 0.0]),
        ("clavicle_r", Some(4), [-0.07, 0.09, 0.0]),
        ("upperarm_r", Some(12), [-0.09, 0.0, 0.0]),
        ("lowerarm_r", Some(13), [-0.25, 0.0, 0.0]),
        ("hand_r", Some(14), [-0.23, 0.0, 0.0]),
        ("thigh_l", Some(1), [0.09, -0.04, 0.0]),
        ("calf_l", Some(16), [0.0, -0.38, 0.0]),
        ("foot_l", Some(17), [0.0, -0.38, 0.0]),
        ("ball_l", Some(18), [0.0, -0.05, 0.11]),
        ("thigh_r", Some(1), [-0.09, -0.04, 0.0]),
        ("calf_r", Some(20), [0.0, -0.38, 0.0]),
        ("foot_r", Some(21), [0.0, -0.38, 0.0]),
        ("ball_r", Some(22), [0.0, -0.05, 0.11]),
        ("tail_1", Some(1), [0.0, 0.0, -0.1]),
        ("tail_2", Some(24), [0.0, 0.0, -0.13]),
    ])
}

/// A rotation track sampled at 31 keys over `duration` from `f(t)`, the joint's bind translation
/// kept.
fn rotation_track(skeleton: &Skeleton, joint: u32, duration: f32, f: impl Fn(f32) -> Quat) -> JointTrack {
    let bind = skeleton
        .bind_pose
        .get(joint as usize)
        .copied()
        .unwrap_or_default();
    let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0 * duration).collect();
    JointTrack {
        joint,
        translations: TrackCurve::Static(bind.translation),
        rotations: TrackCurve::Animated {
            values: times.iter().map(|&t| f(t)).collect(),
            times,
            exact_rotation: false,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    }
}

/// The fixture source's clips: `Wave` (left arm), `Stride` (legs, arms, hips travel with XZ root
/// motion), `Nod` (neck and head) and `Turn` (the hips yaw a quarter turn).
#[must_use]
pub fn fixture_source_clips() -> Vec<Arc<AnimationClip>> {
    use std::f32::consts::TAU;
    let s = fixture_source_skeleton();
    let joint = |name: &str| s.joint_index(&format!("mixamorig:{name}")).unwrap_or(0);
    let mut wave = AnimationClip::empty("Wave", 1.0);
    wave.tracks.push(rotation_track(&s, joint("LeftArm"), 1.0, |t| {
        Quat::from_rotation_z(-1.2 * (t * TAU).sin().abs())
    }));
    wave.tracks
        .push(rotation_track(&s, joint("LeftForeArm"), 1.0, |t| {
            Quat::from_rotation_z(-0.6 * (t * TAU * 2.0).sin())
        }));
    let mut stride = AnimationClip::empty("Stride", 1.0);
    for (name, sign, amp) in [
        ("LeftUpLeg", 1.0, 0.5),
        ("RightUpLeg", -1.0, 0.5),
        ("LeftArm", -1.0, 0.3),
        ("RightArm", 1.0, 0.3),
    ] {
        stride.tracks.push(rotation_track(&s, joint(name), 1.0, move |t| {
            Quat::from_rotation_x(sign * amp * (t * TAU).sin())
        }));
    }
    for name in ["LeftLeg", "RightLeg"] {
        stride.tracks.push(rotation_track(&s, joint(name), 1.0, |t| {
            Quat::from_rotation_x(0.4 * (t * TAU).cos().max(0.0))
        }));
    }
    let hips = joint("Hips");
    let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
    stride.tracks.push(JointTrack {
        joint: hips,
        translations: TrackCurve::Animated {
            values: times
                .iter()
                .map(|&t| Vec3::new(0.0, 1.0 + 0.03 * (t * TAU * 2.0).cos(), -1.2 * t))
                .collect(),
            times: times.clone(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    stride.root_motion = Some(RootMotionTrack {
        joint: hips,
        mode: RootMotionMode::XZ,
    });
    let mut nod = AnimationClip::empty("Nod", 0.5);
    nod.tracks.push(rotation_track(&s, joint("Neck"), 0.5, |t| {
        Quat::from_rotation_x(0.35 * (t * TAU * 2.0).sin())
    }));
    nod.tracks.push(rotation_track(&s, joint("Head"), 0.5, |t| {
        Quat::from_rotation_x(0.2 * (t * TAU * 2.0).sin())
    }));
    let mut turn = AnimationClip::empty("Turn", 1.0);
    turn.tracks.push(rotation_track(&s, hips, 1.0, |t| {
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2 * t)
    }));
    vec![Arc::new(wave), Arc::new(stride), Arc::new(nod), Arc::new(turn)]
}

// ---------------------------------------------------------------------------------------------
// Auto-map
// ---------------------------------------------------------------------------------------------

/// The joint name key of the by-name rule: the text after the last `:` (a namespace), lower case,
/// letters and digits only.
#[must_use]
pub fn name_key(joint: &str) -> String {
    let tail = joint.rsplit(':').next().unwrap_or(joint);
    tail.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The result of [`auto_map`]: role → joint maps for both sides and how each pair was found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutoMap {
    /// Role → source joint.
    pub source: BTreeMap<String, String>,
    /// Role → target joint.
    pub target: BTreeMap<String, String>,
    /// Roles paired by chain (guessed humanoid roles both rigs have).
    pub by_chain: Vec<String>,
    /// Custom roles paired by name.
    pub by_name: Vec<String>,
}

/// Maps `source` onto `target` by chain, then by name (module docs).
#[must_use]
pub fn auto_map(source: &Skeleton, target: &Skeleton) -> AutoMap {
    let sp = RetargetProfile::guess_humanoid(source);
    let tp = RetargetProfile::guess_humanoid(target);
    let mut out = AutoMap::default();
    for (role, sj) in &sp.roles {
        if let Some(tj) = tp.roles.get(role) {
            out.source.insert(role.clone(), sj.clone());
            out.target.insert(role.clone(), tj.clone());
            out.by_chain.push(role.clone());
        }
    }
    let mut used_source: BTreeSet<String> = out.source.values().cloned().collect();
    let used_target: BTreeSet<String> = out.target.values().cloned().collect();
    for tn in &target.joint_names {
        if used_target.contains(tn) || out.source.contains_key(tn) || out.target.contains_key(tn) {
            continue;
        }
        let key = name_key(tn);
        if key.is_empty() {
            continue;
        }
        let Some(sn) = source
            .joint_names
            .iter()
            .find(|sn| !used_source.contains(*sn) && name_key(sn) == key)
        else {
            continue;
        };
        used_source.insert(sn.clone());
        out.source.insert(tn.clone(), sn.clone());
        out.target.insert(tn.clone(), tn.clone());
        out.by_name.push(tn.clone());
    }
    out
}

/// The chain a role is listed under (`None`: a custom role, listed under "Other").
#[must_use]
pub fn chain_of(role: &str) -> Option<RetargetChain> {
    HumanoidBone::from_name(role).map(HumanoidBone::chain)
}

// ---------------------------------------------------------------------------------------------
// The panel model
// ---------------------------------------------------------------------------------------------

/// Which column of the map a joint edit goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The source rig's column.
    Source,
    /// The target rig's column.
    Target,
}

/// One row of the two-column map.
#[derive(Clone, Debug, PartialEq)]
pub struct MapRow {
    /// The role.
    pub role: String,
    /// Its chain (`None`: a custom role).
    pub chain: Option<RetargetChain>,
    /// The source joint, when mapped.
    pub source: Option<String>,
    /// The target joint, when mapped.
    pub target: Option<String>,
}

/// A refused panel edit: a stable lowercase code (`book/agents/retargeting.md`) and a message.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelError {
    /// The reason code.
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl PanelError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// What a finished batch did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BatchReport {
    /// The clip files written, in clip order.
    pub written: Vec<PathBuf>,
    /// The batch was cancelled (then `written` is empty).
    pub cancelled: bool,
}

/// What a batch bakes: the map, both skeletons, the source clips and the output folder.
#[derive(Clone, Debug)]
pub struct BatchInput {
    /// The map (as saved).
    pub setup: RetargetSetup,
    /// The source skeleton.
    pub source: Arc<Skeleton>,
    /// The target skeleton.
    pub target: Arc<Skeleton>,
    /// The clips to bake, in order.
    pub clips: Vec<Arc<AnimationClip>>,
    /// Where the clip files go.
    pub out_dir: PathBuf,
}

/// The Retarget panel's state (a world resource shared by the panel and `editor.retarget_*`).
#[derive(Resource, Debug, Default)]
pub struct RetargetPanel {
    /// The source rig.
    pub source: Option<RetargetRig>,
    /// The target rig.
    pub target: Option<RetargetRig>,
    /// The map being edited (saved as `.retarget.ron`).
    pub setup: RetargetSetup,
    /// Where the map was loaded from or last saved to.
    pub path: Option<PathBuf>,
    /// Unsaved edits.
    pub dirty: bool,
    /// Bumped by every map edit (the cached retargeter follows it).
    pub revision: u64,
    /// The previewed source clip.
    pub clip: Option<String>,
    /// The preview time, seconds.
    pub time: f32,
    /// The preview plays.
    pub playing: bool,
    /// Clips "Retarget clips…" bakes.
    pub batch_clips: BTreeSet<String>,
    /// The batch's output folder (text field).
    pub out_dir: String,
    /// The source field's text.
    pub source_text: String,
    /// The target field's text.
    pub target_text: String,
    /// The save path field's text.
    pub save_text: String,
    /// The last outcome line the panel shows.
    pub status: Option<String>,
    /// The last batch's result (agent JSON).
    pub last_batch: Option<Value>,
    /// The preview shows the bake with the IK goals solved (`setup.ik_goals`) instead of the plain
    /// pose transfer.
    pub goal_preview: bool,
    batch: Option<WorkerJob<Result<BatchReport, String>>>,
    built: Option<(u64, Result<Arc<Retargeter>, String>)>,
    frame: Option<u64>,
    /// The preview clip baked with goals: (revision, clip name, bake).
    baked: Option<(u64, String, Arc<AnimationClip>)>,
    /// The goal check of the preview clip: (revision, clip name, per-goal results).
    goal_report: Option<(u64, String, Vec<regia_animation::GoalCheck>)>,
}

impl RetargetPanel {
    /// Opens `source` → `target` with an empty map (the previous map is dropped).
    pub fn open(&mut self, source: RetargetRig, target: RetargetRig) {
        self.setup = RetargetSetup {
            source: regia_animation::retarget_setup::RigRoles {
                roles: RoleSource::Explicit(BTreeMap::new()),
                overrides: BTreeMap::new(),
            },
            target: regia_animation::retarget_setup::RigRoles {
                roles: RoleSource::Explicit(BTreeMap::new()),
                overrides: BTreeMap::new(),
            },
            ..RetargetSetup::default()
        };
        self.clip = source.clips.first().map(|c| c.name.clone());
        self.batch_clips = source.clips.iter().map(|c| c.name.clone()).collect();
        self.source_text.clone_from(&source.name);
        self.target_text.clone_from(&target.name);
        self.source = Some(source);
        self.target = Some(target);
        self.path = None;
        self.time = 0.0;
        self.dirty = false;
        self.touch();
    }

    /// Loads a saved map onto the open rigs: each side's role source is resolved against its
    /// skeleton into an explicit map (overrides applied), so every row is editable.
    ///
    /// # Errors
    ///
    /// `retarget_not_open`, `bad_setup` (parse, version, values, unknown profile).
    pub fn load_setup(&mut self, text: &str, path: Option<PathBuf>) -> Result<(), PanelError> {
        let (source, target) = self.rigs()?;
        let mut setup =
            RetargetSetup::from_ron_str(text).map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        let sp = setup
            .source
            .profile_for(&source.skeleton)
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        let tp = setup
            .target
            .profile_for(&target.skeleton)
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        setup.source.roles = RoleSource::Explicit(sp.roles);
        setup.source.overrides.clear();
        setup.target.roles = RoleSource::Explicit(tp.roles);
        setup.target.overrides.clear();
        self.setup = setup;
        self.path = path;
        self.dirty = false;
        self.touch();
        Ok(())
    }

    fn touch(&mut self) {
        self.revision += 1;
        self.built = None;
    }

    fn rigs(&self) -> Result<(&RetargetRig, &RetargetRig), PanelError> {
        match (&self.source, &self.target) {
            (Some(s), Some(t)) => Ok((s, t)),
            _ => Err(PanelError::new(
                "retarget_not_open",
                "open a source and a target rig first (editor.retarget_open)",
            )),
        }
    }

    fn map(&self, side: Side) -> &BTreeMap<String, String> {
        static EMPTY: BTreeMap<String, String> = BTreeMap::new();
        let roles = match side {
            Side::Source => &self.setup.source.roles,
            Side::Target => &self.setup.target.roles,
        };
        match roles {
            RoleSource::Explicit(map) => map,
            _ => &EMPTY,
        }
    }

    fn map_mut(&mut self, side: Side) -> &mut BTreeMap<String, String> {
        let roles = match side {
            Side::Source => &mut self.setup.source.roles,
            Side::Target => &mut self.setup.target.roles,
        };
        if !matches!(roles, RoleSource::Explicit(_)) {
            *roles = RoleSource::Explicit(BTreeMap::new());
        }
        match roles {
            RoleSource::Explicit(map) => map,
            _ => unreachable!("set to Explicit above"),
        }
    }

    /// Runs [`auto_map`] on the open rigs and replaces the map with its result (per-role settings
    /// of roles still mapped are kept).
    ///
    /// # Errors
    ///
    /// `retarget_not_open`.
    pub fn auto_map(&mut self) -> Result<AutoMap, PanelError> {
        let (source, target) = self.rigs()?;
        let map = auto_map(&source.skeleton, &target.skeleton);
        *self.map_mut(Side::Source) = map.source.clone();
        *self.map_mut(Side::Target) = map.target.clone();
        let mapped: BTreeSet<String> = map.target.keys().cloned().collect();
        self.setup.roles.retain(|role, _| mapped.contains(role));
        self.dirty = true;
        self.touch();
        Ok(map)
    }

    /// The map's rows: humanoid roles in chain order, then custom roles by name.
    #[must_use]
    pub fn rows(&self) -> Vec<MapRow> {
        let s = self.map(Side::Source);
        let t = self.map(Side::Target);
        let row = |role: &str| MapRow {
            role: role.to_string(),
            chain: chain_of(role),
            source: s.get(role).cloned(),
            target: t.get(role).cloned(),
        };
        let mut rows: Vec<MapRow> = RetargetChain::ALL
            .iter()
            .flat_map(|c| c.roles().iter())
            .filter(|r| s.contains_key(r.name()) || t.contains_key(r.name()))
            .map(|r| row(r.name()))
            .collect();
        let custom: BTreeSet<&String> = s
            .keys()
            .chain(t.keys())
            .filter(|r| HumanoidBone::from_name(r).is_none())
            .collect();
        rows.extend(custom.into_iter().map(|r| row(r)));
        rows
    }

    /// Sets (or with `None` clears) the joint of `role` on `side`.
    ///
    /// # Errors
    ///
    /// `retarget_not_open`, `retarget_unknown_role` (an empty role), `retarget_unknown_joint` (a
    /// joint the rig on that side does not have).
    pub fn set_joint(&mut self, role: &str, side: Side, joint: Option<&str>) -> Result<(), PanelError> {
        let (source, target) = self.rigs()?;
        if role.trim().is_empty() {
            return Err(PanelError::new("retarget_unknown_role", "the role name is empty"));
        }
        if let Some(joint) = joint {
            let skeleton = match side {
                Side::Source => &source.skeleton,
                Side::Target => &target.skeleton,
            };
            if skeleton.joint_index(joint).is_none() {
                return Err(PanelError::new(
                    "retarget_unknown_joint",
                    format!("the {} rig has no joint {joint:?}", side_name(side)),
                ));
            }
        }
        let map = self.map_mut(side);
        match joint {
            Some(joint) => {
                map.insert(role.to_string(), joint.to_string());
            }
            None => {
                map.remove(role);
            }
        }
        self.dirty = true;
        self.touch();
        Ok(())
    }

    /// The rest offset of `chain` (XYZ Euler degrees): the first of its roles that has one.
    #[must_use]
    pub fn chain_offset(&self, chain: RetargetChain) -> Vec3 {
        chain
            .roles()
            .iter()
            .find_map(|r| self.setup.roles.get(r.name()).map(|s| s.rest_offset_deg))
            .unwrap_or(Vec3::ZERO)
    }

    /// Sets the rest offset of every role of `chain` to `degrees` (a zero offset removes the
    /// setting when nothing else is set on the role).
    ///
    /// # Errors
    ///
    /// `retarget_bad_offset` for a non-finite value.
    pub fn set_chain_offset(&mut self, chain: RetargetChain, degrees: Vec3) -> Result<(), PanelError> {
        if !degrees.is_finite() {
            return Err(PanelError::new(
                "retarget_bad_offset",
                "the rest offset must be three finite numbers (degrees)",
            ));
        }
        for role in chain.roles() {
            let name = role.name().to_string();
            let entry = self.setup.roles.entry(name.clone()).or_default();
            entry.rest_offset_deg = degrees;
            if *entry == RoleSettings::default() {
                self.setup.roles.remove(&name);
            }
        }
        self.dirty = true;
        self.touch();
        Ok(())
    }

    /// Aligns `chain` to `pose` ([`chain_alignment_offset`]) and returns the offset it set.
    ///
    /// # Errors
    ///
    /// `retarget_not_open`, `retarget_build_failed`, `retarget_cannot_align` (the root, or a chain
    /// whose aim bone is not mapped on both rigs).
    pub fn align_chain(&mut self, chain: RetargetChain, pose: ReferencePose) -> Result<Vec3, PanelError> {
        let retargeter = self.retargeter()?;
        let offset = chain_alignment_offset(&retargeter, chain, pose).ok_or_else(|| {
            PanelError::new(
                "retarget_cannot_align",
                format!(
                    "{} cannot be aligned: it needs its aim bones mapped on both rigs (and the target's left and \
                     right legs or arms for an arm)",
                    chain.label()
                ),
            )
        })?;
        let degrees = rest_offset_degrees(offset);
        self.set_chain_offset(chain, degrees)?;
        Ok(degrees)
    }

    /// The runtime retargeter of the current map (cached per edit).
    ///
    /// # Errors
    ///
    /// `retarget_not_open`, `retarget_build_failed` (the map pairs nothing, or names a missing
    /// joint).
    pub fn retargeter(&mut self) -> Result<Arc<Retargeter>, PanelError> {
        if let Some((revision, built)) = &self.built
            && *revision == self.revision
        {
            return built
                .clone()
                .map_err(|e| PanelError::new("retarget_build_failed", e.clone()));
        }
        let (source, target) = self.rigs()?;
        let built = self
            .setup
            .build(&source.skeleton, &target.skeleton)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        self.built = Some((self.revision, built.clone()));
        built.map_err(|e| PanelError::new("retarget_build_failed", e))
    }

    /// The source pose of the preview clip at `time` and the target pose it retargets to, as
    /// object-space joint matrices `(source, target)`.
    ///
    /// # Errors
    ///
    /// As [`Self::retargeter`]; `retarget_unknown_clip` without a preview clip.
    pub fn preview_globals(&mut self, time: f32) -> Result<(Vec<Mat4>, Vec<Mat4>), PanelError> {
        let retargeter = self.retargeter()?;
        let clip = self.preview_clip()?;
        let skeleton = Arc::clone(&self.rigs()?.0.skeleton);
        let time = time.clamp(0.0, clip.duration.max(0.0));
        let mut source_pose = skeleton.bind_pose_pose();
        clip.sample_pose(time, &mut source_pose);
        let mut target_pose = Pose::default();
        if self.goal_preview && !self.setup.ik_goals.is_empty() {
            // Goals need the whole clip (contacts): sample the bake, as a game plays it.
            let baked = self.baked_preview(&retargeter, &clip);
            target_pose = retargeter.target().bind_pose_pose();
            baked.sample_pose(time, &mut target_pose);
        } else {
            retargeter.retarget_pose(&source_pose, &mut target_pose);
        }
        Ok((
            skeleton.globals(&source_pose, None),
            retargeter.target().globals(&target_pose, None),
        ))
    }

    /// The previewed source clip.
    fn preview_clip(&self) -> Result<Arc<AnimationClip>, PanelError> {
        let (source, _) = self.rigs()?;
        self.clip
            .as_deref()
            .and_then(|c| source.clip(c))
            .cloned()
            .ok_or_else(|| PanelError::new("retarget_unknown_clip", "the source rig has no clip to preview"))
    }

    /// The preview clip's duration (0 without one).
    #[must_use]
    pub fn clip_duration(&self) -> f32 {
        self.source
            .as_ref()
            .zip(self.clip.as_deref())
            .and_then(|(s, c)| s.clip(c))
            .map_or(0.0, |c| c.duration.max(0.0))
    }

    /// Advances playback by `dt` seconds (looping).
    pub fn advance(&mut self, dt: f32) {
        let duration = self.clip_duration();
        if self.playing && duration > 0.0 && dt.is_finite() {
            self.time = (self.time + dt.max(0.0)).rem_euclid(duration);
        }
    }

    /// The map as `.retarget.ron` text.
    ///
    /// # Errors
    ///
    /// `bad_setup` when it does not serialize.
    pub fn setup_text(&self) -> Result<String, PanelError> {
        self.setup
            .to_ron_string()
            .map_err(|e| PanelError::new(e.code(), e.to_string()))
    }

    /// Saves the map to `path` (atomically) and remembers it.
    ///
    /// # Errors
    ///
    /// `bad_setup`, `retarget_io`.
    pub fn save(&mut self, path: &Path) -> Result<(), PanelError> {
        let text = self.setup_text()?;
        crate::atomic_write::write_atomic(path, text.as_bytes())
            .map_err(|e| PanelError::new("retarget_io", e))?;
        self.path = Some(path.to_path_buf());
        self.dirty = false;
        Ok(())
    }

    /// What a batch of `clips` (all source clips when empty) into `out_dir` would bake.
    ///
    /// # Errors
    ///
    /// `retarget_not_open`, `retarget_unknown_clip`, `retarget_build_failed`.
    pub fn batch_input(&mut self, clips: &[String], out_dir: PathBuf) -> Result<BatchInput, PanelError> {
        self.retargeter()?;
        let (source, target) = self.rigs()?;
        let chosen: Vec<Arc<AnimationClip>> = if clips.is_empty() {
            source.clips.clone()
        } else {
            clips
                .iter()
                .map(|name| {
                    source.clip(name).cloned().ok_or_else(|| {
                        PanelError::new(
                            "retarget_unknown_clip",
                            format!("the source rig has no clip {name:?}"),
                        )
                    })
                })
                .collect::<Result<_, _>>()?
        };
        if chosen.is_empty() {
            return Err(PanelError::new("retarget_unknown_clip", "no clips to retarget"));
        }
        Ok(BatchInput {
            setup: self.setup.clone(),
            source: Arc::clone(&source.skeleton),
            target: Arc::clone(&target.skeleton),
            clips: chosen,
            out_dir,
        })
    }

    /// Whether a batch is running.
    #[must_use]
    pub fn batch_running(&self) -> bool {
        self.batch.is_some()
    }

    /// The running batch's job id.
    #[must_use]
    pub fn batch_job(&self) -> Option<&str> {
        self.batch.as_ref().map(|b| b.id.as_str())
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Source => "source",
        Side::Target => "target",
    }
}

// ---------------------------------------------------------------------------------------------
// Batch
// ---------------------------------------------------------------------------------------------

/// The file name of a baked clip: letters, digits, `-`, `_` and `.` kept, others `_`.
#[must_use]
pub fn clip_file_name(clip: &str) -> String {
    let mut name: String = clip
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.trim_matches('.').is_empty() {
        name = "clip".into();
    }
    format!("{name}.cooked")
}

/// The cooked-file bytes of a baked clip: the compressed clip payload the cook writes for a
/// retargeted clip, in a cooked container whose asset id is derived from the setup name and the
/// clip name.
///
/// # Errors
///
/// Encoding failure (pathological sizes only).
pub fn encode_clip(setup_name: &str, clip: &AnimationClip, target: &Skeleton) -> Result<Vec<u8>, String> {
    let payload = clip
        .to_compressed_cooked_payload(Some(target), &CompressionSettings::default())
        .map_err(|e| format!("cannot encode {:?}: {e}", clip.name))?;
    let parent = AssetId::sub_asset(AssetId::NIL, &format!("retarget-setup/{setup_name}"));
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of("regia_animation::AnimationClip"),
        asset_id: AssetId::sub_asset(parent, &format!("retarget/{}", clip.name)),
        importer_version: BATCH_IMPORTER_VERSION,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, &payload).map_err(|e| format!("cannot encode {:?}: {e}", clip.name))
}

static STAGING: AtomicU64 = AtomicU64::new(0);

/// Bakes `input`'s clips (module docs). `cancelled` is asked before each clip and before the
/// clips are moved into place; `progress(done, total)` follows each clip. A cancel (or an error
/// before the move) removes the staging folder, so the output folder gains nothing.
///
/// # Errors
///
/// The map does not build, a baked clip is not finite, or a file cannot be written or moved.
pub fn run_batch(
    input: &BatchInput,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(usize, usize),
) -> Result<BatchReport, String> {
    let _span =
        tracing::info_span!(target: "regia_editor", "retarget_batch", clips = input.clips.len()).entered();
    let retargeter = input
        .setup
        .build(&input.source, &input.target)
        .map_err(|e| e.to_string())?;
    let staging = input.out_dir.join(format!(
        ".retarget-staging-{}-{}",
        std::process::id(),
        STAGING.fetch_add(1, Ordering::Relaxed)
    ));
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&staging);
    };
    std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
    let total = input.clips.len();
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(total);
    let mut names = BTreeSet::new();
    for (i, clip) in input.clips.iter().enumerate() {
        if cancelled() {
            cleanup();
            return Ok(BatchReport {
                written: Vec::new(),
                cancelled: true,
            });
        }
        let baked = retargeter.retarget_clip(clip);
        if let Err(e) = regia_asset_pipeline::motiondb_cook::scan_clip_finite(&baked) {
            cleanup();
            return Err(format!(
                "{:?} bakes to a clip with non-finite values: {e}",
                clip.name
            ));
        }
        let bytes = match encode_clip(&input.setup.name, &baked, &input.target) {
            Ok(bytes) => bytes,
            Err(e) => {
                cleanup();
                return Err(e);
            }
        };
        let mut file = clip_file_name(&clip.name);
        let mut n = 2;
        while !names.insert(file.clone()) {
            file = clip_file_name(&format!("{}-{n}", clip.name));
            n += 1;
        }
        let temp = staging.join(&file);
        if let Err(e) = std::fs::write(&temp, bytes) {
            cleanup();
            return Err(format!("cannot write {}: {e}", temp.display()));
        }
        staged.push((temp, input.out_dir.join(&file)));
        progress(i + 1, total);
    }
    if cancelled() {
        cleanup();
        return Ok(BatchReport {
            written: Vec::new(),
            cancelled: true,
        });
    }
    let mut written: Vec<PathBuf> = Vec::with_capacity(staged.len());
    for (temp, dest) in &staged {
        if let Err(e) = std::fs::rename(temp, dest) {
            // No partial set: the clips already moved are removed again (review sprint-w12-rev-g7
            // #7), the way a cancel leaves no clip.
            for done in &written {
                let _ = std::fs::remove_file(done);
            }
            cleanup();
            return Err(format!(
                "cannot move {} into place: {e} (no clip written; {} of {} had moved and were removed)",
                dest.display(),
                written.len(),
                staged.len()
            ));
        }
        written.push(dest.clone());
    }
    cleanup();
    Ok(BatchReport {
        written,
        cancelled: false,
    })
}

fn batch_json(report: &BatchReport) -> Value {
    json!({
        "written": report.written.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "count": report.written.len(),
        "cancelled": report.cancelled,
    })
}

/// Starts "Retarget clips…" for `clips` (all when empty) into `out_dir` as an editor job.
///
/// # Errors
///
/// `retarget_busy` while a batch runs; the [`RetargetPanel::batch_input`] errors;
/// `job_start_failed`.
pub fn start_batch(world: &mut World, clips: &[String], out_dir: PathBuf) -> Result<String, PanelError> {
    let input = {
        let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
        if panel.batch.is_some() {
            return Err(PanelError::new(
                "retarget_busy",
                "a retarget batch is already running",
            ));
        }
        panel.batch_input(clips, out_dir)?
    };
    let label = format!("Retarget {} clips", input.clips.len());
    let job = WorkerJob::spawn(
        world,
        BATCH_JOB_KIND,
        BATCH_JOB_KIND,
        label,
        "retarget-batch",
        move |cx| {
            run_batch(&input, &|| cx.cancelled(), &|done, total| {
                cx.progress(
                    Some(done as f64 / total.max(1) as f64),
                    &format!("{done} of {total} clips"),
                );
            })
        },
    )
    .map_err(|e| PanelError::new("job_start_failed", e))?;
    let id = job.id.clone();
    let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
    panel.batch = Some(job);
    panel.status = Some(format!("{id} running"));
    Ok(id)
}

/// Applies a finished batch to the world (ends its job, records the result). The editor's job
/// tick calls it every frame; the panel and `editor.retarget_batch` call it too.
pub fn tick_batch(world: &mut World) {
    let Some(mut panel) = world.get_resource_mut::<RetargetPanel>() else {
        return;
    };
    let Some(job) = panel.batch.as_mut() else {
        return;
    };
    let fraction = job.context().fraction();
    let Some(outcome) = job.take() else {
        let label = job.context().message();
        let job = panel.batch.take();
        if let Some(job) = job {
            job.sync_row(world, &label, fraction);
            world.resource_mut::<RetargetPanel>().batch = Some(job);
        }
        return;
    };
    let Some(job) = panel.batch.take() else {
        return;
    };
    let (status, result) = match outcome.and_then(|r| r) {
        Ok(report) => {
            let value = batch_json(&report);
            let line = if report.cancelled {
                "Retarget clips: cancelled, no clips written".to_string()
            } else {
                format!("Retarget clips: {} written", report.written.len())
            };
            (line, Ok((value, report.cancelled)))
        }
        Err(message) => (format!("Retarget clips failed: {message}"), Err(message)),
    };
    panel.status = Some(status);
    panel.last_batch = Some(match &result {
        Ok((value, _)) => value.clone(),
        Err(message) => json!({ "error": message }),
    });
    match result {
        Ok(done) => job.end(world, Ok(done)),
        Err(message) => job.end(world, Err(("retarget_batch_failed", message))),
    }
}

/// Waits for the running batch to finish and applies it (tests and `editor.retarget_batch {
/// wait: true }`).
pub fn wait_batch(world: &mut World) {
    if let Some(mut panel) = world.get_resource_mut::<RetargetPanel>()
        && let Some(job) = panel.batch.as_mut()
    {
        job.wait();
    }
    tick_batch(world);
}

// ---------------------------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------------------------

fn rpc(error: PanelError) -> RpcError {
    let mut out = RpcError::invalid_params(error.message);
    if let Some(data) = out.data.as_mut() {
        data["reason"] = json!(error.code);
    }
    out
}

fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// The panel's state as agent JSON: rigs, rows by chain, chain offsets and the preview clip.
fn state_json(panel: &RetargetPanel) -> Value {
    let rows = panel.rows();
    let chains: Vec<Value> = RetargetChain::ALL
        .iter()
        .map(|&chain| {
            let roles: Vec<&MapRow> = rows.iter().filter(|r| r.chain == Some(chain)).collect();
            json!({
                "chain": chain.name(),
                "label": chain.label(),
                "mapped": roles.iter().filter(|r| r.source.is_some() && r.target.is_some()).count(),
                "roles": chain.roles().len(),
                "rest_offset_deg": vec3_json(panel.chain_offset(chain)),
                "rows": roles.iter().map(|r| json!({ "role": r.role, "source": r.source, "target": r.target })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let other: Vec<Value> = rows
        .iter()
        .filter(|r| r.chain.is_none())
        .map(|r| json!({ "role": r.role, "source": r.source, "target": r.target }))
        .collect();
    json!({
        "open": panel.source.is_some() && panel.target.is_some(),
        "source": panel.source.as_ref().map(|r| json!({
            "name": r.name,
            "joints": r.skeleton.len(),
            "clips": r.clips.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
        })),
        "target": panel.target.as_ref().map(|r| json!({ "name": r.name, "joints": r.skeleton.len() })),
        "chains": chains,
        "other": other,
        "path": panel.path.as_ref().map(|p| p.display().to_string()),
        "dirty": panel.dirty,
        "clip": panel.clip,
        "batch_job": panel.batch_job(),
        "ik_goals": serde_json::to_value(&panel.setup.ik_goals).unwrap_or(Value::Null),
        "goal_preview": panel.goal_preview,
    })
}

fn rig_param(world: &World, params: &Value, key: &str) -> Result<RetargetRig, RpcError> {
    let Some(given) = params.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return Err(rpc(PanelError::new(
            "retarget_bad_rig",
            format!(
                "`{key}` is required: a built-in rig (demo, fixture:source, fixture:target) or a .glb/.gltf/.bvh path"
            ),
        )));
    };
    if let Some(rig) = RetargetRig::builtin(given) {
        return Ok(rig);
    }
    let path = crate::agent_ext::document_request_path(world, key, given)?;
    RetargetRig::load(&path, given).map_err(|e| rpc(PanelError::new("retarget_bad_rig", e)))
}

/// `editor.retarget_open { source, target, setup? }`.
fn retarget_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let source = rig_param(world, params, "source")?;
    let target = rig_param(world, params, "target")?;
    let setup = match params.get("setup").and_then(Value::as_str) {
        Some(given) => {
            let path = crate::agent_ext::document_request_path(world, "setup", given)?;
            let text = std::fs::read_to_string(&path).map_err(|e| {
                rpc(PanelError::new(
                    "retarget_io",
                    format!("cannot read {given}: {e}"),
                ))
            })?;
            Some((text, path))
        }
        None => None,
    };
    let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
    if panel.batch.is_some() {
        return Err(rpc(PanelError::new(
            "retarget_busy",
            "a retarget batch is running",
        )));
    }
    panel.open(source, target);
    if let Some((text, path)) = setup {
        panel.load_setup(&text, Some(path)).map_err(rpc)?;
    }
    Ok(state_json(&panel))
}

/// `editor.retarget_auto_map {}`.
fn retarget_auto_map(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
    let map = panel.auto_map().map_err(rpc)?;
    let mut state = state_json(&panel);
    state["by_chain"] = json!(map.by_chain);
    state["by_name"] = json!(map.by_name);
    Ok(state)
}

fn vec3_param(value: &Value) -> Option<Vec3> {
    let a = value.as_array()?;
    if a.len() != 3 {
        return None;
    }
    let f = |i: usize| a.get(i).and_then(Value::as_f64).map(|v| v as f32);
    Some(Vec3::new(f(0)?, f(1)?, f(2)?))
}

/// `editor.retarget_map_set { role, source?, target? } | { chain, rest_offset_deg } | { chain, align }`.
fn retarget_map_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
    let mut changed = Vec::new();
    if let Some(role) = params.get("role").and_then(Value::as_str) {
        for (key, side) in [("source", Side::Source), ("target", Side::Target)] {
            match params.get(key) {
                None => {}
                Some(Value::Null) => panel.set_joint(role, side, None).map_err(rpc)?,
                Some(Value::String(joint)) => panel.set_joint(role, side, Some(joint)).map_err(rpc)?,
                Some(_) => {
                    return Err(rpc(PanelError::new(
                        "retarget_unknown_joint",
                        format!("`{key}` must be a joint name or null"),
                    )));
                }
            }
            if params.get(key).is_some() {
                changed.push(key);
            }
        }
        if changed.is_empty() {
            return Err(rpc(PanelError::new(
                "retarget_nothing_to_set",
                "give `source` and/or `target` (a joint name, or null to clear) with `role`",
            )));
        }
    } else if let Some(name) = params.get("chain").and_then(Value::as_str) {
        let chain = RetargetChain::parse(name).ok_or_else(|| {
            rpc(PanelError::new(
                "retarget_unknown_chain",
                format!(
                    "unknown chain {name:?} (root, spine, head, left_arm, right_arm, left_leg, right_leg)"
                ),
            ))
        })?;
        if let Some(value) = params.get("rest_offset_deg") {
            let deg = vec3_param(value).ok_or_else(|| {
                rpc(PanelError::new(
                    "retarget_bad_offset",
                    "`rest_offset_deg` must be [x, y, z] degrees",
                ))
            })?;
            panel.set_chain_offset(chain, deg).map_err(rpc)?;
            changed.push("rest_offset_deg");
        } else if let Some(pose) = params.get("align").and_then(Value::as_str) {
            let pose = ReferencePose::parse(pose).ok_or_else(|| {
                rpc(PanelError::new(
                    "retarget_bad_pose",
                    format!("unknown pose {pose:?} (t_pose, a_pose)"),
                ))
            })?;
            panel.align_chain(chain, pose).map_err(rpc)?;
            changed.push("align");
        } else {
            return Err(rpc(PanelError::new(
                "retarget_nothing_to_set",
                "give `rest_offset_deg` or `align` with `chain`",
            )));
        }
    } else {
        return Err(rpc(PanelError::new(
            "retarget_nothing_to_set",
            "give `role` (with source/target) or `chain` (with rest_offset_deg or align)",
        )));
    }
    let mut state = state_json(&panel);
    state["changed"] = json!(changed);
    Ok(state)
}

/// `editor.retarget_preview { clip?, time?, playing?, goals? }`.
fn retarget_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
    if let Some(clip) = params.get("clip").and_then(Value::as_str) {
        let known = panel.source.as_ref().is_some_and(|s| s.clip(clip).is_some());
        if !known {
            return Err(rpc(PanelError::new(
                "retarget_unknown_clip",
                format!("the source rig has no clip {clip:?}"),
            )));
        }
        panel.clip = Some(clip.to_string());
    }
    if let Some(time) = params.get("time").and_then(Value::as_f64) {
        panel.time = (time as f32).clamp(0.0, panel.clip_duration());
    }
    if let Some(playing) = params.get("playing").and_then(Value::as_bool) {
        panel.playing = playing;
    }
    if let Some(goals) = params.get("goals").and_then(Value::as_bool) {
        panel.goal_preview = goals;
    }
    let time = panel.time;
    let (source_globals, target_globals) = panel.preview_globals(time).map_err(rpc)?;
    let retargeter = panel.retargeter().map_err(rpc)?;
    let facing = Quat::from_rotation_y(retargeter.facing_yaw());
    let target = retargeter.target();
    let position = |m: &Mat4| m.w_axis.truncate();
    let chains: Vec<Value> = RetargetChain::ALL
        .iter()
        .filter_map(|&chain| {
            let (from, to) = chain.aim_roles()?;
            let joint = |role: HumanoidBone| {
                (0..target.len() as u32).find(|&t| retargeter.role_of(t) == Some(role.name()))
            };
            let (t0, t1) = (joint(from)?, joint(to)?);
            let (s0, s1) = (retargeter.source_of(t0)?, retargeter.source_of(t1)?);
            let dir = |g: &[Mat4], a: u32, b: u32| {
                let d = position(g.get(b as usize)?) - position(g.get(a as usize)?);
                (d.length_squared() > 1e-12).then(|| d.normalize())
            };
            let ds = facing * dir(&source_globals, s0, s1)?;
            let dt = dir(&target_globals, t0, t1)?;
            Some(json!({
                "chain": chain.name(),
                "source_dir": vec3_json(ds),
                "target_dir": vec3_json(dt),
                "angle_deg": ds.dot(dt).clamp(-1.0, 1.0).acos().to_degrees(),
            }))
        })
        .collect();
    let root = |g: &[Mat4]| g.first().map(|m| vec3_json(position(m)));
    Ok(json!({
        "clip": panel.clip,
        "time": time,
        "duration": panel.clip_duration(),
        "playing": panel.playing,
        "goals": panel.goal_preview && !panel.setup.ik_goals.is_empty(),
        "facing_yaw_deg": retargeter.facing_yaw().to_degrees(),
        "translation_scale": retargeter.scale(),
        "mapped_joints": retargeter.mapped_joints(),
        "source_root": root(&source_globals),
        "target_root": root(&target_globals),
        "chains": chains,
    }))
}

/// `editor.retarget_save { path? }`.
fn retarget_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let given = params.get("path").and_then(Value::as_str);
    let path = match given {
        Some(given) => crate::agent_ext::document_request_path(world, "path", given)?,
        None => world
            .get_resource::<RetargetPanel>()
            .and_then(|p| p.path.clone())
            .ok_or_else(|| {
                rpc(PanelError::new(
                    "retarget_io",
                    "`path` is required: the map has not been saved before",
                ))
            })?,
    };
    let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
    panel.rigs().map_err(rpc)?;
    panel.save(&path).map_err(rpc)?;
    Ok(json!({ "path": path.display().to_string(), "text": panel.setup_text().map_err(rpc)? }))
}

/// `editor.retarget_batch { clips?, out_dir, wait? }`.
fn retarget_batch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick_batch(world);
    let Some(given) = params.get("out_dir").and_then(Value::as_str) else {
        return Err(rpc(PanelError::new("retarget_io", "`out_dir` is required")));
    };
    let out_dir = crate::agent_ext::document_request_path(world, "out_dir", given)?;
    let clips: Vec<String> = match params.get("clips") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    rpc(PanelError::new(
                        "retarget_unknown_clip",
                        "`clips` must be clip names",
                    ))
                })
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(rpc(PanelError::new(
                "retarget_unknown_clip",
                "`clips` must be an array of clip names",
            )));
        }
    };
    let job = start_batch(world, &clips, out_dir).map_err(rpc)?;
    if params.get("wait").and_then(Value::as_bool) == Some(true) {
        wait_batch(world);
        let result = world
            .get_resource::<RetargetPanel>()
            .and_then(|p| p.last_batch.clone())
            .unwrap_or(Value::Null);
        return Ok(json!({ "job": job, "done": true, "result": result }));
    }
    Ok(json!({ "job": job, "done": false }))
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const OPEN_PARAMS: &[ParamSchema] = &[
    param(
        "source",
        "string",
        true,
        "The source rig: a built-in name or a .glb/.gltf/.bvh path.",
    ),
    param(
        "target",
        "string",
        true,
        "The target rig: a built-in name or a .glb/.gltf/.bvh path.",
    ),
    param("setup", "string", false, "A .retarget.ron to load as the map."),
];
const AUTO_MAP_PARAMS: &[ParamSchema] = &[];
const MAP_SET_PARAMS: &[ParamSchema] = &[
    param(
        "role",
        "string",
        false,
        "A role: a humanoid role (hips, leftUpperArm, ...) or a custom one.",
    ),
    param(
        "source",
        "string|null",
        false,
        "The role's source joint (null clears it).",
    ),
    param(
        "target",
        "string|null",
        false,
        "The role's target joint (null clears it).",
    ),
    param(
        "chain",
        "string",
        false,
        "root, spine, head, left_arm, right_arm, left_leg or right_leg.",
    ),
    param(
        "rest_offset_deg",
        "array",
        false,
        "[x, y, z] degrees for the chain.",
    ),
    param(
        "align",
        "string",
        false,
        "t_pose or a_pose: align the chain to that pose.",
    ),
];
const PREVIEW_PARAMS: &[ParamSchema] = &[
    param("clip", "string", false, "A source clip name."),
    param(
        "time",
        "number",
        false,
        "Seconds into the clip (clamped to its duration).",
    ),
    param("playing", "boolean", false, "Play the preview in the panel."),
    param(
        "goals",
        "boolean",
        false,
        "Preview the bake with the IK goals solved (true) or the plain pose transfer (false).",
    ),
];
const SAVE_PARAMS: &[ParamSchema] = &[param("path", "string", false, "The .retarget.ron to write.")];
const BATCH_PARAMS: &[ParamSchema] = &[
    param("out_dir", "string", true, "The output folder."),
    param("clips", "array", false, "Source clip names (default: all)."),
    param(
        "wait",
        "boolean",
        false,
        "Return when the job ends, with its result.",
    ),
];

/// The panel's agent methods (`editor.retarget_*`, `book/agents/retargeting.md`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.retarget_open",
                tool: "retarget_open",
                description: "Open the Retarget panel on a source and a target rig (a built-in rig: demo, \
                    fixture:source, fixture:target; or a .glb/.gltf/.bvh path) with an empty bone map, or with the \
                    map of a saved .retarget.ron (`setup`). Returns the panel state: rigs, chains with their rows \
                    and rest offsets, custom rows.",
                params: OPEN_PARAMS,
                result: "{ open, source: { name, joints, clips }, target: { name, joints }, chains: [{ chain, label, \
                    mapped, roles, rest_offset_deg, rows: [{ role, source, target }] }], other, path, dirty, clip, \
                    batch_job, ik_goals: [{ roles, mode, weight }], goal_preview }",
                mutating: true,
                permission: Permission::EditScene,
            },
            retarget_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.retarget_auto_map",
                tool: "retarget_auto_map",
                description: "Map the open rigs' bones automatically: by chain (humanoid roles guessed from joint \
                    names on both rigs, grouped into root, spine, head, arms and legs), then by name (a target \
                    joint still unmapped pairs with the source joint of the same name, ignoring a namespace \
                    prefix, case and punctuation). Replaces the map; keeps rest offsets of roles still mapped.",
                params: AUTO_MAP_PARAMS,
                result: "the panel state plus { by_chain: [role], by_name: [role] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            retarget_auto_map as Handler,
        ),
        (
            MethodSchema {
                name: "editor.retarget_map_set",
                tool: "retarget_map_set",
                description: "Edit the bone map: `role` with `source` and/or `target` sets (a joint name) or clears \
                    (null) that role's joint on each side; `chain` with `rest_offset_deg` [x, y, z] sets the \
                    chain's rest-pose alignment (XYZ Euler degrees, object space, on every role of the chain); \
                    `chain` with `align` (t_pose or a_pose) computes that offset so the target's aim bone points \
                    along the reference pose while the source is at rest.",
                params: MAP_SET_PARAMS,
                result: "the panel state plus { changed }",
                mutating: true,
                permission: Permission::EditScene,
            },
            retarget_map_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.retarget_preview",
                tool: "retarget_preview",
                description: "Set the side-by-side preview (`clip`, `time` in seconds, `playing`, `goals`) and read \
                    it: the source clip is sampled on the source rig and retargeted by the runtime Retargeter of \
                    the current map (with `goals: true` and IK goals in the map, the bake with its goals solved). \
                    Returns per chain the aim bone direction on both rigs (the source's turned by the facing yaw) \
                    and the angle between them, the roots, the facing yaw and the translation scale.",
                params: PREVIEW_PARAMS,
                result: "{ clip, time, duration, playing, goals, facing_yaw_deg, translation_scale, mapped_joints, \
                    source_root, target_root, chains: [{ chain, source_dir, target_dir, angle_deg }] }",
                mutating: false,
                permission: Permission::Read,
            },
            retarget_preview as Handler,
        ),
        (
            MethodSchema {
                name: "editor.retarget_save",
                tool: "retarget_save",
                description: "Save the bone map as a .retarget.ron setup (format v2; both sides as explicit role \
                    maps, chain alignments as per-role rest offsets, the IK goals list). Without `path` the file it was loaded from \
                    or last saved to.",
                params: SAVE_PARAMS,
                result: "{ path, text }",
                mutating: true,
                permission: Permission::Assets,
            },
            retarget_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.retarget_batch",
                tool: "retarget_batch",
                description: "Retarget clips: bake the selected source clips (all when `clips` is absent) for the \
                    target with the current map and write each as `<out_dir>/<clip>.cooked` (the cook's compressed \
                    clip payload) in a cancellable editor job (`editor.cancel_job { id }`). Clips are staged and \
                    moved into place only when every clip is baked, so a cancel writes none. With `wait: true` \
                    the call returns when the job ends.",
                params: BATCH_PARAMS,
                result: "{ job, done, result?: { written, count, cancelled } }",
                mutating: true,
                permission: Permission::Assets,
            },
            retarget_batch as Handler,
        ),
        goals::method(),
    ]
}

// ---------------------------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------------------------

/// Draws the Retarget panel.
pub fn retarget_panel(ui: &mut egui::Ui, world: &mut World) {
    tick_batch(world);
    let tokens = crate::theme::tokens(world).clone();
    let project = crate::agent_ext::agent_root(world);
    let mut panel = world.remove_resource::<RetargetPanel>().unwrap_or_default();
    let mut request = Request::default();
    // egui may run a panel twice in one frame: playback advances once per frame.
    let frame = ui.ctx().cumulative_frame_nr();
    let first_pass = panel.frame != Some(frame);
    if first_pass {
        panel.frame = Some(frame);
        let dt = ui.ctx().input(|i| i.stable_dt);
        panel.advance(dt);
    }
    let playing_before = panel.playing;
    let goal_preview_before = panel.goal_preview;
    draw(ui, &mut panel, &tokens, &mut request);
    // A second pass sees the same click: the batch starts once (the other requests are idempotent),
    // and Play/Pause flips once (review sprint-w12-rev-g7 #9). Goal adds and removes run once too.
    request.batch &= first_pass;
    panel.playing = gate_toggle(playing_before, panel.playing, first_pass);
    panel.goal_preview = gate_toggle(goal_preview_before, panel.goal_preview, first_pass);
    if let Some(edit) = request.goal.take().filter(|_| first_pass) {
        panel.status = Some(goals::apply_edit(&mut panel, edit));
    }
    if panel.playing {
        ui.ctx().request_repaint();
    }
    world.insert_resource(panel);
    apply(world, &project, request);
}

/// A toggle drawn on every pass keeps its new value only on the frame's first pass; a repeat of
/// the click on egui's discarded second pass leaves the first pass's value.
pub(crate) fn gate_toggle(before: bool, after: bool, first_pass: bool) -> bool {
    if first_pass { after } else { before }
}

/// What the UI asked for that needs the world (rig loads, the batch job).
#[derive(Default)]
struct Request {
    open: bool,
    batch: bool,
    cancel: bool,
    save: bool,
    /// An IK goal add or remove (applied on the frame's first pass only).
    goal: Option<goals::GoalEdit>,
}

/// A path the panel opens, saves or writes clips to: the agent methods' rule (relative under the
/// project, absolute only under it or the temp directory, never `..` or a symlink leading out;
/// `path_outside_project`, review sprint-w12-rev-g7 #6). The error is the status line.
pub(crate) fn panel_path(project: &Path, key: &str, text: &str) -> Result<PathBuf, String> {
    regia_agent::req_path::document_path_in(project, key, text).map_err(|e| e.message)
}

fn apply(world: &mut World, project: &Path, request: Request) {
    let resolve = |key: &str, text: &str| panel_path(project, key, text);
    if request.open {
        let (s, t) = {
            let panel = world.resource::<RetargetPanel>();
            (panel.source_text.clone(), panel.target_text.clone())
        };
        let load = |text: &str| {
            RetargetRig::builtin(text.trim()).map_or_else(
                || RetargetRig::load(&resolve("source", text.trim())?, text.trim()),
                Ok,
            )
        };
        let outcome = load(&s).and_then(|s| Ok((s, load(&t)?)));
        let mut panel = world.resource_mut::<RetargetPanel>();
        match outcome {
            Ok((s, t)) => {
                panel.open(s, t);
                panel.status = Some("Opened; press Auto-map to pair the bones".into());
            }
            Err(e) => panel.status = Some(e),
        }
    }
    if request.save {
        let mut panel = world.resource_mut::<RetargetPanel>();
        let text = panel.save_text.trim().to_string();
        if text.is_empty() {
            panel.status = Some("Enter a .retarget.ron path to save to".into());
        } else {
            panel.status = Some(match resolve("path", &text) {
                Ok(path) => match panel.save(&path) {
                    Ok(()) => format!("Saved {}", path.display()),
                    Err(e) => e.message,
                },
                Err(e) => e,
            });
        }
    }
    if request.batch {
        let (clips, out) = {
            let panel = world.resource::<RetargetPanel>();
            (
                panel.batch_clips.iter().cloned().collect::<Vec<_>>(),
                panel.out_dir.trim().to_string(),
            )
        };
        if out.is_empty() {
            world.resource_mut::<RetargetPanel>().status =
                Some("Enter an output folder for the clips".into());
        } else if clips.is_empty() {
            world.resource_mut::<RetargetPanel>().status = Some("Select at least one clip".into());
        } else {
            match resolve("out_dir", &out) {
                Ok(dir) => {
                    if let Err(e) = start_batch(world, &clips, dir) {
                        world.resource_mut::<RetargetPanel>().status = Some(e.message);
                    }
                }
                Err(e) => world.resource_mut::<RetargetPanel>().status = Some(e),
            }
        }
    }
    if request.cancel
        && let Some(panel) = world.get_resource::<RetargetPanel>()
        && let Some(job) = panel.batch.as_ref()
    {
        job.cancel();
    }
}

fn joint_combo(
    ui: &mut egui::Ui,
    id: (&str, &str),
    skeleton: &Skeleton,
    current: Option<&str>,
    spoken: &str,
) -> Option<Option<String>> {
    let mut picked: Option<Option<String>> = None;
    let shown = current.unwrap_or("(none)").to_string();
    let response = egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .show_ui(ui, |ui| {
            if ui.pick_row(current.is_none(), "(none)") {
                picked = Some(None);
            }
            for name in &skeleton.joint_names {
                if ui.pick_row(current == Some(name.as_str()), name) {
                    picked = Some(Some(name.clone()));
                }
            }
        })
        .response;
    crate::a11y_audit::name(response, egui::WidgetType::ComboBox, spoken);
    picked.filter(|p| p.as_deref() != current)
}

/// `selectable_value` without a bound value: true when clicked.
trait SelectableValue {
    fn pick_row(&mut self, selected: bool, text: &str) -> bool;
}

impl SelectableValue for egui::Ui {
    fn pick_row(&mut self, selected: bool, text: &str) -> bool {
        let mut value = selected;
        self.selectable_value(&mut value, true, text).clicked()
    }
}

fn draw(
    ui: &mut egui::Ui,
    panel: &mut RetargetPanel,
    tokens: &crate::theme::ThemeTokens,
    request: &mut Request,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Source");
        let r = ui.add(egui::TextEdit::singleline(&mut panel.source_text).desired_width(160.0));
        crate::a11y_audit::name(r, egui::WidgetType::TextEdit, "Source rig");
        ui.label("Target");
        let r = ui.add(egui::TextEdit::singleline(&mut panel.target_text).desired_width(160.0));
        crate::a11y_audit::name(r, egui::WidgetType::TextEdit, "Target rig");
        if ui.button("Open rigs").clicked() {
            request.open = true;
        }
        let open = panel.source.is_some() && panel.target.is_some();
        if ui.add_enabled(open, egui::Button::new("Auto-map")).clicked() {
            panel.status = Some(match panel.auto_map() {
                Ok(map) => format!(
                    "Auto-map: {} by chain, {} by name",
                    map.by_chain.len(),
                    map.by_name.len()
                ),
                Err(e) => e.message,
            });
        }
    });
    ui.horizontal_wrapped(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut panel.save_text)
                .desired_width(220.0)
                .hint_text("path.retarget.ron"),
        );
        crate::a11y_audit::name(r, egui::WidgetType::TextEdit, "Save path");
        let open = panel.source.is_some() && panel.target.is_some();
        if ui.add_enabled(open, egui::Button::new("Save map")).clicked() {
            request.save = true;
        }
        if panel.dirty {
            ui.colored_label(tokens.accents.unsaved, "unsaved");
        }
    });
    if let Some(status) = &panel.status {
        ui.colored_label(tokens.muted, status.as_str());
    }
    let (Some(source), Some(target)) = (panel.source.clone(), panel.target.clone()) else {
        ui.colored_label(
            tokens.muted,
            "Type a rig (demo, or a .glb/.gltf/.bvh path) on each side and press Open rigs.",
        );
        return;
    };
    ui.separator();
    ui.columns(2, |cols| {
        map_list(&mut cols[0], panel, &source, &target, tokens);
        preview(&mut cols[1], panel, &source, tokens);
        goals::goal_list(&mut cols[1], panel, tokens, &mut request.goal);
        batch(&mut cols[1], panel, &source, tokens, request);
    });
}

fn map_list(
    ui: &mut egui::Ui,
    panel: &mut RetargetPanel,
    source: &RetargetRig,
    target: &RetargetRig,
    tokens: &crate::theme::ThemeTokens,
) {
    ui.strong("Bone map");
    let rows = panel.rows();
    egui::ScrollArea::vertical()
        .id_salt("retarget_map")
        .max_height(ui.available_height() * 0.9)
        .show(ui, |ui| {
            let mut chains: Vec<Option<RetargetChain>> =
                RetargetChain::ALL.iter().copied().map(Some).collect();
            chains.push(None);
            for chain in chains {
                let label = chain.map_or("Other", RetargetChain::label);
                let in_chain: Vec<&MapRow> = rows.iter().filter(|r| r.chain == chain).collect();
                let roles: Vec<String> = match chain {
                    Some(c) => c.roles().iter().map(|r| r.name().to_string()).collect(),
                    None => in_chain.iter().map(|r| r.role.clone()).collect(),
                };
                if chain.is_none() && roles.is_empty() {
                    continue;
                }
                let mapped = in_chain
                    .iter()
                    .filter(|r| r.source.is_some() && r.target.is_some())
                    .count();
                egui::CollapsingHeader::new(format!("{label}  {mapped}/{}", roles.len()))
                    .id_salt(("retarget_chain", label))
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::Grid::new(("retarget_rows", label))
                            .num_columns(3)
                            .striped(true)
                            .show(ui, |ui| {
                                ui.colored_label(tokens.muted, "Role");
                                ui.colored_label(tokens.muted, "Source joint");
                                ui.colored_label(tokens.muted, "Target joint");
                                ui.end_row();
                                for role in &roles {
                                    let row = in_chain.iter().find(|r| &r.role == role);
                                    let s = row.and_then(|r| r.source.as_deref());
                                    let t = row.and_then(|r| r.target.as_deref());
                                    let color = if s.is_some() && t.is_some() {
                                        tokens.text
                                    } else {
                                        tokens.accents.alert
                                    };
                                    ui.colored_label(color, role);
                                    if let Some(pick) = joint_combo(
                                        ui,
                                        ("rs", role),
                                        &source.skeleton,
                                        s,
                                        &format!("Source joint for {role}"),
                                    ) {
                                        let _ = panel.set_joint(role, Side::Source, pick.as_deref());
                                    }
                                    if let Some(pick) = joint_combo(
                                        ui,
                                        ("rt", role),
                                        &target.skeleton,
                                        t,
                                        &format!("Target joint for {role}"),
                                    ) {
                                        let _ = panel.set_joint(role, Side::Target, pick.as_deref());
                                    }
                                    ui.end_row();
                                }
                            });
                        if let Some(chain) = chain.filter(|c| c.aim_roles().is_some()) {
                            chain_alignment(ui, panel, chain);
                        }
                    });
            }
        });
}

fn chain_alignment(ui: &mut egui::Ui, panel: &mut RetargetPanel, chain: RetargetChain) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Rest offset");
        let mut deg = panel.chain_offset(chain);
        let mut changed = false;
        for (axis, value) in [("X", &mut deg.x), ("Y", &mut deg.y), ("Z", &mut deg.z)] {
            let before = *value;
            let r = ui.add(
                egui::DragValue::new(value)
                    .speed(0.5)
                    .suffix("°")
                    .range(-180.0..=180.0),
            );
            crate::a11y_audit::named_drag_value(
                r,
                &format!("{} rest offset {axis}", chain.label()),
                f64::from(*value),
            );
            changed |= *value != before;
        }
        if changed {
            let _ = panel.set_chain_offset(chain, deg);
        }
        for (text, pose) in [
            ("Align to T-pose", ReferencePose::TPose),
            ("Align to A-pose", ReferencePose::APose),
        ] {
            if ui.button(text).clicked() {
                panel.status = Some(match panel.align_chain(chain, pose) {
                    Ok(d) => format!("{}: offset ({:.1}, {:.1}, {:.1})°", chain.label(), d.x, d.y, d.z),
                    Err(e) => e.message,
                });
            }
        }
    });
}

fn preview(
    ui: &mut egui::Ui,
    panel: &mut RetargetPanel,
    source: &RetargetRig,
    tokens: &crate::theme::ThemeTokens,
) {
    ui.strong("Preview");
    ui.horizontal_wrapped(|ui| {
        let current = panel.clip.clone();
        let response = egui::ComboBox::from_id_salt("retarget_clip")
            .selected_text(current.clone().unwrap_or_else(|| "(no clip)".into()))
            .show_ui(ui, |ui| {
                for clip in &source.clips {
                    if ui.pick_row(current.as_deref() == Some(clip.name.as_str()), &clip.name) {
                        panel.clip = Some(clip.name.clone());
                        panel.time = 0.0;
                    }
                }
            })
            .response;
        crate::a11y_audit::name(response, egui::WidgetType::ComboBox, "Preview clip");
        let play = if panel.playing { "Pause" } else { "Play" };
        ui.toggle_value(&mut panel.playing, play);
        let duration = panel.clip_duration();
        let r = ui.add(egui::Slider::new(&mut panel.time, 0.0..=duration.max(0.0)).suffix(" s"));
        crate::a11y_audit::named_drag_value(r, "Preview time", f64::from(panel.time));
    });
    let width = ui.available_width();
    let size = egui::vec2(width, (width * 0.5).clamp(120.0, 360.0));
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = response.rect;
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        "Retarget preview: source rig left, target rig right",
    );
    painter.rect_filled(rect, 4.0, tokens.well);
    let time = panel.time;
    match panel.preview_globals(time) {
        Ok((sg, tg)) => {
            let half = rect.width() * 0.5;
            let left = egui::Rect::from_min_size(rect.min, egui::vec2(half, rect.height()));
            let right =
                egui::Rect::from_min_size(rect.min + egui::vec2(half, 0.0), egui::vec2(half, rect.height()));
            let src = panel.source.as_ref().map(|s| Arc::clone(&s.skeleton));
            let tgt = panel.target.as_ref().map(|t| Arc::clone(&t.skeleton));
            // The source turned by the facing yaw, so both rigs face the viewer the same way.
            let yaw = panel.retargeter().map_or(0.0, |r| r.facing_yaw());
            if let Some(s) = src {
                stick_figure(&painter, left, &s, &sg, yaw, tokens.accents.active, tokens.line);
            }
            if let Some(t) = tgt {
                stick_figure(&painter, right, &t, &tg, 0.0, tokens.accents.ok, tokens.line);
            }
            painter.text(
                left.left_top() + egui::vec2(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                "Source",
                egui::FontId::proportional(12.0),
                tokens.muted,
            );
            painter.text(
                right.left_top() + egui::vec2(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                "Target",
                egui::FontId::proportional(12.0),
                tokens.muted,
            );
        }
        Err(e) => {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                e.message,
                egui::FontId::proportional(13.0),
                tokens.muted,
            );
        }
    }
}

/// Draws a rig's bones as lines, seen from its front (−Z facing turned to the viewer), fitted to
/// `rect`.
fn stick_figure(
    painter: &egui::Painter,
    rect: egui::Rect,
    skeleton: &Skeleton,
    globals: &[Mat4],
    yaw: f32,
    bone: egui::Color32,
    joint: egui::Color32,
) {
    let turn = Quat::from_rotation_y(yaw);
    let points: Vec<Vec3> = globals.iter().map(|m| turn * m.w_axis.truncate()).collect();
    if points.is_empty() {
        return;
    }
    let (mut min, mut max) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for p in &points {
        min = min.min(*p);
        max = max.max(*p);
    }
    let extent = (max - min).max_element().max(0.1);
    let scale = rect.height().min(rect.width()) * 0.8 / extent;
    let center = (min + max) * 0.5;
    let to_screen = |p: Vec3| rect.center() + egui::vec2((p.x - center.x) * scale, -(p.y - center.y) * scale);
    for (j, parent) in skeleton.parents.iter().enumerate() {
        if let (Some(p), Some(a)) = (parent.and_then(|p| points.get(p as usize)), points.get(j)) {
            painter.line_segment([to_screen(*p), to_screen(*a)], egui::Stroke::new(2.0, bone));
        }
    }
    for p in &points {
        painter.circle_filled(to_screen(*p), 2.0, joint);
    }
}

fn batch(
    ui: &mut egui::Ui,
    panel: &mut RetargetPanel,
    source: &RetargetRig,
    tokens: &crate::theme::ThemeTokens,
    request: &mut Request,
) {
    ui.separator();
    ui.strong("Retarget clips");
    ui.horizontal_wrapped(|ui| {
        for clip in &source.clips {
            let mut on = panel.batch_clips.contains(&clip.name);
            if ui.checkbox(&mut on, clip.name.as_str()).changed() {
                if on {
                    panel.batch_clips.insert(clip.name.clone());
                } else {
                    panel.batch_clips.remove(&clip.name);
                }
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut panel.out_dir)
                .desired_width(220.0)
                .hint_text("output folder"),
        );
        crate::a11y_audit::name(r, egui::WidgetType::TextEdit, "Output folder");
        let running = panel.batch.is_some();
        if ui
            .add_enabled(!running, egui::Button::new("Retarget clips…"))
            .clicked()
        {
            request.batch = true;
        }
        if running {
            if let Some(job) = panel.batch.as_ref() {
                ui.add(egui::ProgressBar::new(job.context().fraction().unwrap_or(0.0)).desired_width(120.0));
            }
            if ui.button("Cancel").clicked() {
                request.cancel = true;
            }
        }
    });
    if let Some(Value::Object(result)) = &panel.last_batch
        && let Some(count) = result.get("count").and_then(Value::as_u64)
    {
        ui.colored_label(tokens.muted, format!("Last batch: {count} clips written"));
    }
}

/// Bone directions of `skeleton`'s bind pose for one chain's aim bone (diagnostics for tests and
/// the book page's worked example): `None` when the chain is not mapped on that rig.
#[must_use]
pub fn bind_aim_direction(
    skeleton: &Skeleton,
    map: &BTreeMap<String, String>,
    chain: RetargetChain,
) -> Option<Vec3> {
    let (from, to) = chain.aim_roles()?;
    let a = skeleton.joint_index(map.get(from.name())?)?;
    let b = skeleton.joint_index(map.get(to.name())?)?;
    bind_bone_direction(skeleton, a, b)
}
