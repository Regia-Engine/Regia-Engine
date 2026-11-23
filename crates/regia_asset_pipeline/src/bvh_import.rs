// SPDX-License-Identifier: MPL-2.0
//! Biovision Hierarchy (BVH) motion-capture import (pure Rust; `book/agents/bvh-import.md`, ADR-2532).
//!
//! A `.bvh` file holds one skeleton (`HIERARCHY`: `ROOT`/`JOINT`/`End Site`, `OFFSET`, `CHANNELS`)
//! and one clip (`MOTION`: `Frames:`, `Frame Time:`, one line of channel values per frame). The
//! importer cooks it straight to a [`Skeleton`] and an [`AnimationClip`] (no mesh):
//!
//! * Euler channels become quaternions in the file's channel order: with `CHANNELS ... Zrotation
//!   Xrotation Yrotation` the joint rotation is `Rz * Xr * Yr` (column vectors, the first listed
//!   channel outermost), angles in degrees.
//! * A joint's local translation is its `OFFSET` plus its position channels, times the unit scale.
//! * `unit` / `unit_scale` bring centimeters, inches and so on to meters; `up_axis: Z` turns a Z-up file
//!   to Y-up; [`crate::Forward::Convert`] (the default) then applies the project's 180° yaw to the
//!   roots, as the glTF importer does (`docs/formats/gltf-conventions.md`). Only root joints change.
//! * `root_motion` tags the first root as the clip's root-motion joint, as the glTF path does.
//! * Keys are reduced per curve: a key is dropped while the curve without it stays within
//!   `rotation_tolerance_degrees` (angle) and `translation_tolerance` (meters) of the original keys.
//! * An optional `retarget` setting names a target skeleton (a glTF/GLB in the project) and a bone map;
//!   the cook then also writes the clip retargeted to that skeleton.
//!
//! The format is the public Biovision BVH description (Meredith and Maddock, "Motion Capture File
//! Formats Explained", Sheffield Hallam University, 2001). Every limit is a field of [`BvhLimits`].

use std::collections::{BTreeMap, HashMap};

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::skeleton::{LocalBind, Skeleton};
use regia_core::math::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::Forward;

/// Caps on one BVH import (untrusted input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BvhLimits {
    /// Largest file, in bytes (default 256 MiB).
    pub max_file_bytes: u64,
    /// Most joints, `End Site`s included (default 1,024).
    pub max_joints: usize,
    /// Most frames (default 1,000,000).
    pub max_frames: usize,
    /// Most channels in one frame, over all joints (default 8,192).
    pub max_channels: usize,
    /// Deepest joint nesting (default 256).
    pub max_depth: usize,
}

impl Default for BvhLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 256 << 20,
            max_joints: 1024,
            max_frames: 1_000_000,
            max_channels: 8192,
            max_depth: 256,
        }
    }
}

/// The unit one BVH length is in. BVH files carry no unit; motion libraries use centimeters or inches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BvhUnit {
    /// Meters (default).
    #[default]
    Meters,
    /// Centimeters.
    Centimeters,
    /// Millimeters.
    Millimeters,
    /// Inches.
    Inches,
    /// Feet.
    Feet,
}

impl BvhUnit {
    /// Meters per unit.
    pub fn meters(self) -> f32 {
        match self {
            BvhUnit::Meters => 1.0,
            BvhUnit::Centimeters => 0.01,
            BvhUnit::Millimeters => 0.001,
            BvhUnit::Inches => 0.0254,
            BvhUnit::Feet => 0.3048,
        }
    }
}

/// The file's up axis. BVH does not say; most libraries are Y-up, some Z-up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BvhUpAxis {
    /// +Y is up (default); no conversion.
    #[default]
    Y,
    /// +Z is up; the file is turned −90° about X so +Z becomes +Y.
    Z,
}

/// Root motion extraction (the glTF importer's modes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BvhRootMotion {
    /// No root motion (default); the root joint carries the motion in the pose.
    #[default]
    None,
    /// Horizontal translation of the first root.
    XZ,
    /// Full translation of the first root.
    XYZ,
    /// Yaw of the first root.
    Yaw,
}

/// Where a BVH clip is retargeted to at cook time.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BvhRetarget {
    /// Project-relative path of the `.glb`/`.gltf` whose skin is the target skeleton
    /// (`assets/characters/guard.glb`).
    pub target: String,
    /// Which skin of that file (default 0).
    pub skin: usize,
    /// Source (BVH) joint name to target joint name. Empty: both skeletons' humanoid roles are guessed
    /// from joint names (`RetargetProfile::guess_humanoid`).
    pub bone_map: BTreeMap<String, String>,
}

/// BVH importer settings (the `.meta` `settings` string, RON).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BvhImportSettings {
    /// The unit of the file's lengths (default meters).
    pub unit: BvhUnit,
    /// Extra uniform factor on top of `unit` (default 1).
    pub unit_scale: f32,
    /// The file's up axis (default Y).
    pub up_axis: BvhUpAxis,
    /// Skip the project's 180° yaw ([`Forward::Keep`]); the default bakes it into the roots ([`Forward::Convert`]).
    pub keep_forward: bool,
    /// Root motion extraction (default none).
    pub root_motion: BvhRootMotion,
    /// Largest rotation error key reduction may introduce, in degrees (default 0.05; 0 keeps every key).
    pub rotation_tolerance_degrees: f32,
    /// Largest translation error key reduction may introduce, in meters (default 0.0005; 0 keeps every key).
    pub translation_tolerance: f32,
    /// Keep each `End Site` as a joint named `<parent>_end` (default false).
    pub end_sites: bool,
    /// Clip name; empty uses the file stem.
    pub clip_name: String,
    /// Retarget the clip to a project skeleton at cook time (default none).
    pub retarget: Option<BvhRetarget>,
    /// Hard caps.
    pub limits: BvhLimits,
    /// Keep the clip (and its retargeted copy) lossless at cook (`.meta` v2, ADR-7819): a `Raw`
    /// `cooked-clip` v3 body, bit-exact to the imported clip, instead of the compressed one. Key
    /// reduction by the two tolerances above still runs at import. Off by default.
    pub lossless_clips: bool,
}

impl Default for BvhImportSettings {
    fn default() -> Self {
        Self {
            unit: BvhUnit::Meters,
            unit_scale: 1.0,
            up_axis: BvhUpAxis::Y,
            keep_forward: false,
            root_motion: BvhRootMotion::None,
            rotation_tolerance_degrees: 0.05,
            translation_tolerance: 0.0005,
            end_sites: false,
            clip_name: String::new(),
            retarget: None,
            limits: BvhLimits::default(),
            lossless_clips: false,
        }
    }
}

impl BvhImportSettings {
    /// Settings from the `.meta` `settings` string; blank is the defaults, a malformed string or a
    /// non-finite or non-positive scale is an error.
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, BvhError> {
        let parsed = match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s).map_err(|e| BvhError::Settings(e.to_string()))?,
            None => Self::default(),
        };
        parsed.validate()?;
        Ok(parsed)
    }

    /// Checks the numeric settings.
    pub fn validate(&self) -> Result<(), BvhError> {
        if !self.unit_scale.is_finite() || self.unit_scale <= 0.0 {
            return Err(BvhError::Settings(format!(
                "unit_scale must be a positive number, got {}",
                self.unit_scale
            )));
        }
        for (name, v) in [
            ("rotation_tolerance_degrees", self.rotation_tolerance_degrees),
            ("translation_tolerance", self.translation_tolerance),
        ] {
            if !v.is_finite() || v < 0.0 {
                return Err(BvhError::Settings(format!("{name} must be 0 or more, got {v}")));
            }
        }
        if let Some(r) = &self.retarget
            && r.target.trim().is_empty()
        {
            return Err(BvhError::Settings("retarget.target is empty".to_string()));
        }
        Ok(())
    }

    /// Meters per file length unit.
    pub fn meters_per_unit(&self) -> f32 {
        self.unit.meters() * self.unit_scale
    }

    /// A digest of every setting that changes the cooked output.
    pub fn fingerprint(&self) -> String {
        format!("{self:?}")
    }
}

/// Why a BVH import failed. [`BvhError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum BvhError {
    /// The file could not be read.
    #[error("BVH file read error: {0}")]
    Io(String),
    /// The file is larger than [`BvhLimits::max_file_bytes`].
    #[error("BVH file is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// File size in bytes.
        size: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// A malformed statement.
    #[error("BVH line {line}: {message}")]
    Syntax {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// A bad `CHANNELS` statement (a repeated or unknown channel, more than three rotations).
    #[error("BVH line {line}: {message}")]
    Channels {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// Two joints share a name.
    #[error("BVH line {line}: duplicate joint name {name:?}")]
    DuplicateJoint {
        /// 1-based line number.
        line: usize,
        /// The repeated name.
        name: String,
    },
    /// The `MOTION` section disagrees with the hierarchy or `Frames:`.
    #[error("BVH line {line}: {message}")]
    Motion {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// A count limit was exceeded.
    #[error("BVH import limit exceeded: {0}")]
    Limit(String),
    /// The `.meta` settings string is malformed.
    #[error("BVH importer settings: {0}")]
    Settings(String),
    /// The retarget setting could not be applied.
    #[error("BVH retarget: {0}")]
    Retarget(String),
}

impl BvhError {
    /// The agent-protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            BvhError::Io(_) => "bvh_io",
            BvhError::TooLarge { .. } => "bvh_too_large",
            BvhError::Syntax { .. } => "bvh_syntax",
            BvhError::Channels { .. } => "bvh_channels",
            BvhError::DuplicateJoint { .. } => "bvh_duplicate_joint",
            BvhError::Motion { .. } => "bvh_motion",
            BvhError::Limit(_) => "bvh_limit",
            BvhError::Settings(_) => "bvh_bad_settings",
            BvhError::Retarget(_) => "bvh_retarget",
        }
    }
}

fn syntax(line: usize, message: impl Into<String>) -> BvhError {
    BvhError::Syntax {
        line,
        message: message.into(),
    }
}

/// One channel of a joint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Channel {
    /// Position along axis 0..3.
    Pos(usize),
    /// Rotation about axis 0..3 (degrees).
    Rot(usize),
}

/// A joint as parsed, before conversion.
#[derive(Clone, Debug)]
struct RawJoint {
    name: String,
    parent: Option<usize>,
    offset: Vec3,
    channels: Vec<Channel>,
    end_site: bool,
}

/// A parsed file: joints in file (topological) order and per-frame channel values.
#[derive(Clone, Debug)]
pub struct BvhFile {
    joints: Vec<RawJoint>,
    /// Seconds per frame.
    pub frame_time: f32,
    /// Frame count.
    pub frames: usize,
    /// `frames * channel_count` values, row-major.
    values: Vec<f32>,
    channel_count: usize,
}

impl BvhFile {
    /// Joint count, `End Site`s included.
    pub fn joint_count(&self) -> usize {
        self.joints.len()
    }

    /// Joint names in file order.
    pub fn joint_names(&self) -> Vec<&str> {
        self.joints.iter().map(|j| j.name.as_str()).collect()
    }
}

fn parse_f32(token: &str, line: usize, what: &str) -> Result<f32, BvhError> {
    match token.parse::<f32>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(syntax(line, format!("{what}: {token:?} is not a finite number"))),
    }
}

/// Parses BVH text into a [`BvhFile`].
///
/// # Errors
///
/// A [`BvhError`] with the 1-based line number for a malformed file, or [`BvhError::Limit`] /
/// [`BvhError::TooLarge`] past the limits.
pub fn parse_bvh(bytes: &[u8], limits: &BvhLimits) -> Result<BvhFile, BvhError> {
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(BvhError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_file_bytes,
        });
    }
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty());

    let (n, first) = lines.next().ok_or_else(|| syntax(1, "empty file"))?;
    if !first.eq_ignore_ascii_case("HIERARCHY") {
        return Err(syntax(n, format!("expected HIERARCHY, found {first:?}")));
    }

    let mut joints: Vec<RawJoint> = Vec::new();
    let mut names: HashMap<String, usize> = HashMap::new();
    // Open scopes: (joint index, whether an OFFSET was seen), innermost last.
    let mut stack: Vec<usize> = Vec::new();
    let mut pending: Option<usize> = None; // joint declared, waiting for `{`
    let mut channel_count = 0usize;
    let mut last_line = n;
    let mut motion_seen = false;
    let mut offset_seen: Vec<bool> = Vec::new();

    for (n, line) in lines.by_ref() {
        last_line = n;
        let mut tok = line.split_whitespace();
        let Some(head) = tok.next() else { continue };
        let head_l = head.to_ascii_lowercase();
        match head_l.as_str() {
            "root" | "joint" => {
                if pending.is_some() {
                    return Err(syntax(n, "expected '{' after the previous joint"));
                }
                if head_l == "root" && !stack.is_empty() {
                    return Err(syntax(n, "ROOT inside a joint"));
                }
                if head_l == "joint" && stack.is_empty() {
                    return Err(syntax(n, "JOINT outside a ROOT"));
                }
                let name: Vec<&str> = tok.collect();
                if name.is_empty() {
                    return Err(syntax(n, format!("{} needs a name", head.to_ascii_uppercase())));
                }
                let name = name.join(" ");
                if names.contains_key(&name) {
                    return Err(BvhError::DuplicateJoint { line: n, name });
                }
                if joints.len() >= limits.max_joints {
                    return Err(BvhError::Limit(format!(
                        "more than {} joints (line {n})",
                        limits.max_joints
                    )));
                }
                names.insert(name.clone(), joints.len());
                joints.push(RawJoint {
                    name,
                    parent: stack.last().copied(),
                    offset: Vec3::ZERO,
                    channels: Vec::new(),
                    end_site: false,
                });
                offset_seen.push(false);
                pending = Some(joints.len() - 1);
            }
            "end" => {
                if pending.is_some() {
                    return Err(syntax(n, "expected '{' after the previous joint"));
                }
                let Some(&parent) = stack.last() else {
                    return Err(syntax(n, "End Site outside a joint"));
                };
                if !tok.next().is_some_and(|t| t.eq_ignore_ascii_case("site")) {
                    return Err(syntax(n, "expected 'End Site'"));
                }
                if joints.len() >= limits.max_joints {
                    return Err(BvhError::Limit(format!(
                        "more than {} joints (line {n})",
                        limits.max_joints
                    )));
                }
                let name = format!("{}_end", joints[parent].name);
                // An `End Site` name may collide with a real joint; disambiguate it silently.
                let mut unique = name.clone();
                let mut k = 1;
                while names.contains_key(&unique) {
                    unique = format!("{name}{k}");
                    k += 1;
                }
                names.insert(unique.clone(), joints.len());
                joints.push(RawJoint {
                    name: unique,
                    parent: Some(parent),
                    offset: Vec3::ZERO,
                    channels: Vec::new(),
                    end_site: true,
                });
                offset_seen.push(false);
                pending = Some(joints.len() - 1);
            }
            "{" => {
                let Some(j) = pending.take() else {
                    return Err(syntax(n, "unexpected '{'"));
                };
                if stack.len() >= limits.max_depth {
                    return Err(BvhError::Limit(format!(
                        "joint nesting deeper than {} (line {n})",
                        limits.max_depth
                    )));
                }
                stack.push(j);
            }
            "}" => {
                let Some(j) = stack.pop() else {
                    return Err(syntax(n, "unexpected '}'"));
                };
                if !offset_seen[j] {
                    return Err(syntax(n, format!("joint {:?} has no OFFSET", joints[j].name)));
                }
            }
            "offset" => {
                let Some(&j) = stack.last() else {
                    return Err(syntax(n, "OFFSET outside a joint"));
                };
                if offset_seen[j] {
                    return Err(syntax(n, "second OFFSET in one joint"));
                }
                let v: Vec<&str> = tok.collect();
                if v.len() != 3 {
                    return Err(syntax(n, format!("OFFSET needs 3 numbers, found {}", v.len())));
                }
                joints[j].offset = Vec3::new(
                    parse_f32(v[0], n, "OFFSET x")?,
                    parse_f32(v[1], n, "OFFSET y")?,
                    parse_f32(v[2], n, "OFFSET z")?,
                );
                offset_seen[j] = true;
            }
            "channels" => {
                let Some(&j) = stack.last() else {
                    return Err(syntax(n, "CHANNELS outside a joint"));
                };
                if joints[j].end_site {
                    return Err(BvhError::Channels {
                        line: n,
                        message: "an End Site has no channels".to_string(),
                    });
                }
                if !joints[j].channels.is_empty() {
                    return Err(BvhError::Channels {
                        line: n,
                        message: "second CHANNELS in one joint".to_string(),
                    });
                }
                let Some(count) = tok.next().and_then(|t| t.parse::<usize>().ok()) else {
                    return Err(syntax(n, "CHANNELS needs a count"));
                };
                if count > 6 {
                    return Err(BvhError::Channels {
                        line: n,
                        message: format!("a joint has at most 6 channels, found {count}"),
                    });
                }
                let rest: Vec<&str> = tok.collect();
                if rest.len() != count {
                    return Err(BvhError::Channels {
                        line: n,
                        message: format!("CHANNELS declares {count} but lists {}", rest.len()),
                    });
                }
                let mut pos_seen = [false; 3];
                let mut rot_count = 0;
                for name in rest {
                    let lower = name.to_ascii_lowercase();
                    let axis = match lower.as_bytes().first() {
                        Some(b'x') => 0,
                        Some(b'y') => 1,
                        Some(b'z') => 2,
                        _ => usize::MAX,
                    };
                    let channel = match (axis, &lower[lower.len().min(1)..]) {
                        (0..=2, "position") => {
                            if pos_seen[axis] {
                                return Err(BvhError::Channels {
                                    line: n,
                                    message: format!("channel {name:?} repeated"),
                                });
                            }
                            pos_seen[axis] = true;
                            Channel::Pos(axis)
                        }
                        (0..=2, "rotation") => {
                            rot_count += 1;
                            if rot_count > 3 {
                                return Err(BvhError::Channels {
                                    line: n,
                                    message: "more than 3 rotation channels".to_string(),
                                });
                            }
                            Channel::Rot(axis)
                        }
                        _ => {
                            return Err(BvhError::Channels {
                                line: n,
                                message: format!("unknown channel {name:?}"),
                            });
                        }
                    };
                    joints[j].channels.push(channel);
                }
                channel_count += count;
                if channel_count > limits.max_channels {
                    return Err(BvhError::Limit(format!(
                        "more than {} channels (line {n})",
                        limits.max_channels
                    )));
                }
            }
            "motion" => {
                if !stack.is_empty() || pending.is_some() {
                    return Err(syntax(n, "MOTION inside an unclosed joint"));
                }
                motion_seen = true;
                break;
            }
            other => return Err(syntax(n, format!("unexpected {other:?} in HIERARCHY"))),
        }
    }
    if !motion_seen {
        return Err(syntax(last_line, "missing MOTION section"));
    }
    if joints.iter().all(|j| j.end_site) {
        return Err(syntax(last_line, "the file has no joints"));
    }

    // MOTION header.
    let header =
        |lines: &mut dyn Iterator<Item = (usize, &str)>, key: &str| -> Result<(usize, f32), BvhError> {
            let (n, line) = lines.next().ok_or_else(|| BvhError::Motion {
                line: last_line,
                message: format!("missing {key}"),
            })?;
            let lower = line.to_ascii_lowercase();
            let value = lower
                .strip_prefix(&key.to_ascii_lowercase())
                .map(str::trim)
                .ok_or_else(|| BvhError::Motion {
                    line: n,
                    message: format!("expected {key:?}, found {line:?}"),
                })?;
            let v = value.parse::<f32>().map_err(|_| BvhError::Motion {
                line: n,
                message: format!("{key} value {value:?} is not a number"),
            })?;
            Ok((n, v))
        };
    let (n_frames, frames_f) = header(&mut lines, "Frames:")?;
    if frames_f < 0.0 || frames_f.fract() != 0.0 || !frames_f.is_finite() {
        return Err(BvhError::Motion {
            line: n_frames,
            message: format!("Frames: {frames_f} is not a whole number"),
        });
    }
    let frames = frames_f as usize;
    if frames == 0 {
        return Err(BvhError::Motion {
            line: n_frames,
            message: "Frames: is 0".to_string(),
        });
    }
    if frames > limits.max_frames {
        return Err(BvhError::Limit(format!(
            "{frames} frames, more than {} (line {n_frames})",
            limits.max_frames
        )));
    }
    let (n_time, frame_time) = header(&mut lines, "Frame Time:")?;
    if frame_time <= 0.0 || !frame_time.is_finite() {
        return Err(BvhError::Motion {
            line: n_time,
            message: format!("Frame Time: {frame_time} must be positive"),
        });
    }
    if channel_count == 0 {
        return Err(BvhError::Motion {
            line: n_time,
            message: "the hierarchy declares no channels".to_string(),
        });
    }
    let total = frames
        .checked_mul(channel_count)
        .ok_or_else(|| BvhError::Limit("frames x channels overflows".to_string()))?;
    if total > 256 << 20 {
        return Err(BvhError::Limit(format!(
            "{frames} frames x {channel_count} channels is more than {} values",
            256 << 20
        )));
    }
    let mut values = Vec::with_capacity(total);
    let mut rows = 0;
    for (n, line) in lines {
        if rows == frames {
            return Err(BvhError::Motion {
                line: n,
                message: format!("more frame lines than Frames: {frames}"),
            });
        }
        let before = values.len();
        for t in line.split_whitespace() {
            values.push(parse_f32(t, n, "motion value")?);
        }
        let got = values.len() - before;
        if got != channel_count {
            return Err(BvhError::Motion {
                line: n,
                message: format!("frame has {got} values, the hierarchy declares {channel_count} channels"),
            });
        }
        rows += 1;
    }
    if rows != frames {
        return Err(BvhError::Motion {
            line: last_line,
            message: format!("Frames: {frames} but the file has {rows} frame lines"),
        });
    }
    Ok(BvhFile {
        joints,
        frame_time,
        frames,
        values,
        channel_count,
    })
}

/// The result of [`import_bvh`].
#[derive(Clone, Debug)]
pub struct BvhImport {
    /// The skeleton (meters, Y-up, project forward).
    pub skeleton: Skeleton,
    /// The clip, with reduced keys.
    pub clip: AnimationClip,
    /// Keys before and after reduction, over all curves.
    pub keys_before: usize,
    /// Keys after reduction.
    pub keys_after: usize,
}

/// Unit quaternion for a rotation of `degrees` about axis 0..3.
fn axis_quat(axis: usize, degrees: f32) -> Quat {
    let a = degrees.to_radians();
    match axis {
        0 => Quat::from_rotation_x(a),
        1 => Quat::from_rotation_y(a),
        _ => Quat::from_rotation_z(a),
    }
}

/// The pipeline [`Forward`] the settings select.
fn settings_forward(settings: &BvhImportSettings) -> Forward {
    if settings.keep_forward {
        Forward::Keep
    } else {
        Forward::Convert
    }
}

/// The frame change applied to root joints: up-axis turn, then the project yaw.
fn root_frame(settings: &BvhImportSettings) -> Quat {
    let up = match settings.up_axis {
        BvhUpAxis::Y => Quat::IDENTITY,
        BvhUpAxis::Z => Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
    };
    let yaw = match settings_forward(settings) {
        Forward::Convert => Quat::from_rotation_y(std::f32::consts::PI),
        Forward::Keep => Quat::IDENTITY,
    };
    (yaw * up).normalize()
}

/// Builds the skeleton and clip from a parsed file.
///
/// # Errors
///
/// [`BvhError::Settings`] for bad settings. A parsed [`BvhFile`] always converts.
pub fn import_bvh(
    file: &BvhFile,
    name_hint: &str,
    settings: &BvhImportSettings,
) -> Result<BvhImport, BvhError> {
    settings.validate()?;
    let scale = settings.meters_per_unit();
    let frame = root_frame(settings);

    // Joint list: drop End Sites unless asked for.
    let mut keep: Vec<usize> = Vec::new();
    let mut new_index: Vec<Option<u32>> = vec![None; file.joints.len()];
    for (i, j) in file.joints.iter().enumerate() {
        if j.end_site && !settings.end_sites {
            continue;
        }
        new_index[i] = Some(keep.len() as u32);
        keep.push(i);
    }
    let mut skeleton = Skeleton::default();
    for &i in &keep {
        let j = &file.joints[i];
        skeleton.joint_names.push(j.name.clone());
        let parent = j.parent.and_then(|p| new_index[p]);
        skeleton.parents.push(parent);
        let mut offset = j.offset * scale;
        let mut rotation = Quat::IDENTITY;
        if parent.is_none() {
            offset = frame * offset;
            rotation = frame;
        }
        skeleton.bind_pose.push(LocalBind {
            translation: offset,
            rotation,
            scale: Vec3::ONE,
        });
    }
    // Inverse bind matrices from the bind globals (one pass; parents come first).
    let mut globals: Vec<Mat4> = Vec::with_capacity(keep.len());
    for (k, b) in skeleton.bind_pose.iter().enumerate() {
        let local = Mat4::from_scale_rotation_translation(b.scale, b.rotation, b.translation);
        let g = match skeleton.parents[k] {
            Some(p) => globals[p as usize] * local,
            None => local,
        };
        globals.push(g);
    }
    skeleton.inverse_bind_matrices = globals.iter().map(|g| g.inverse()).collect();
    if let Err(e) = skeleton.validate() {
        return Err(BvhError::Settings(format!("skeleton is invalid: {e}")));
    }

    // Channel column offsets per joint.
    let mut column = vec![0usize; file.joints.len()];
    let mut next = 0;
    for (i, j) in file.joints.iter().enumerate() {
        column[i] = next;
        next += j.channels.len();
    }

    let tol_rot = settings.rotation_tolerance_degrees.to_radians();
    let tol_pos = settings.translation_tolerance;
    let times: Vec<f32> = (0..file.frames).map(|f| f as f32 * file.frame_time).collect();
    let mut clip = AnimationClip::empty(
        if settings.clip_name.is_empty() {
            name_hint.to_string()
        } else {
            settings.clip_name.clone()
        },
        times.last().copied().unwrap_or(0.0),
    );
    clip.sample_rate = 1.0 / file.frame_time;
    let (mut before, mut after) = (0usize, 0usize);
    for &i in &keep {
        let j = &file.joints[i];
        if j.channels.is_empty() {
            continue;
        }
        let joint = new_index[i].unwrap_or(0);
        let is_root = skeleton.parents[joint as usize].is_none();
        let base = column[i];
        let mut rot_keys = Vec::with_capacity(file.frames);
        let mut pos_keys = Vec::with_capacity(file.frames);
        let has_pos = j.channels.iter().any(|c| matches!(c, Channel::Pos(_)));
        let has_rot = j.channels.iter().any(|c| matches!(c, Channel::Rot(_)));
        for f in 0..file.frames {
            let row =
                &file.values[f * file.channel_count + base..f * file.channel_count + base + j.channels.len()];
            let mut t = j.offset;
            let mut q = Quat::IDENTITY;
            for (c, &v) in j.channels.iter().zip(row) {
                match *c {
                    Channel::Pos(a) => t[a] += v,
                    Channel::Rot(a) => q *= axis_quat(a, v),
                }
            }
            let mut t = t * scale;
            let mut q = q.normalize();
            if is_root {
                t = frame * t;
                q = (frame * q).normalize();
            }
            // Keep q in one hemisphere so reduction and interpolation take the short way.
            if let Some(prev) = rot_keys.last().copied()
                && Quat::dot(prev, q) < 0.0
            {
                q = -q;
            }
            rot_keys.push(q);
            pos_keys.push(t);
        }
        before += (if has_rot { file.frames } else { 0 }) + (if has_pos { file.frames } else { 0 });
        let rotations = if has_rot {
            reduce_quats(&times, &rot_keys, tol_rot)
        } else {
            TrackCurve::Static(skeleton.bind_pose[joint as usize].rotation)
        };
        let translations = if has_pos {
            reduce_vec3(&times, &pos_keys, tol_pos)
        } else {
            TrackCurve::Static(skeleton.bind_pose[joint as usize].translation)
        };
        after += (if has_rot { rotations.key_count() } else { 0 })
            + (if has_pos { translations.key_count() } else { 0 });
        clip.tracks.push(JointTrack {
            joint,
            translations,
            rotations,
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    clip.tracks.sort_by_key(|t| t.joint);
    let first_root = skeleton
        .parents
        .iter()
        .position(Option::is_none)
        .map(|r| r as u32);
    let mode = match settings.root_motion {
        BvhRootMotion::None => RootMotionMode::None,
        BvhRootMotion::XZ => RootMotionMode::XZ,
        BvhRootMotion::XYZ => RootMotionMode::XYZ,
        BvhRootMotion::Yaw => RootMotionMode::Yaw,
    };
    if mode != RootMotionMode::None
        && let Some(joint) = first_root
    {
        clip.root_motion = Some(RootMotionTrack { joint, mode });
    }
    Ok(BvhImport {
        skeleton,
        clip,
        keys_before: before,
        keys_after: after,
    })
}

/// Parses and imports in one step.
///
/// # Errors
///
/// As [`parse_bvh`] and [`import_bvh`].
pub fn convert_bvh(
    bytes: &[u8],
    name_hint: &str,
    settings: &BvhImportSettings,
) -> Result<BvhImport, BvhError> {
    let file = parse_bvh(bytes, &settings.limits)?;
    import_bvh(&file, name_hint, settings)
}

/// Greedy recursive key reduction (Ramer–Douglas–Peucker over time): keeps the endpoints and splits at
/// the key with the largest error against the straight interpolation of the segment until every dropped
/// key is within `tol`.
fn reduce_indices(n: usize, tol: f32, error: impl Fn(usize, usize, usize) -> f32) -> Vec<usize> {
    if n <= 2 {
        return (0..n).collect();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut work = vec![(0usize, n - 1)];
    while let Some((a, b)) = work.pop() {
        if b <= a + 1 {
            continue;
        }
        let (mut worst, mut at) = (0.0f32, a);
        for m in a + 1..b {
            let e = error(a, b, m);
            if e > worst {
                worst = e;
                at = m;
            }
        }
        if worst > tol {
            keep[at] = true;
            work.push((a, at));
            work.push((at, b));
        }
    }
    (0..n).filter(|&i| keep[i]).collect()
}

fn reduce_quats(times: &[f32], keys: &[Quat], tol: f32) -> TrackCurve<Quat> {
    let kept = reduce_indices(keys.len(), tol, |a, b, m| {
        let t = (times[m] - times[a]) / (times[b] - times[a]);
        let interp = keys[a].lerp(keys[b], t).normalize();
        interp.angle_between(keys[m])
    });
    if keys.iter().all(|k| k.angle_between(keys[0]) <= tol) {
        return TrackCurve::Static(keys[0]);
    }
    TrackCurve::Animated {
        times: kept.iter().map(|&i| times[i]).collect(),
        values: kept.iter().map(|&i| keys[i]).collect(),
        exact_rotation: false,
    }
}

fn reduce_vec3(times: &[f32], keys: &[Vec3], tol: f32) -> TrackCurve<Vec3> {
    let kept = reduce_indices(keys.len(), tol, |a, b, m| {
        let t = (times[m] - times[a]) / (times[b] - times[a]);
        keys[a].lerp(keys[b], t).distance(keys[m])
    });
    if keys.iter().all(|k| k.distance(keys[0]) <= tol) {
        return TrackCurve::Static(keys[0]);
    }
    TrackCurve::Animated {
        times: kept.iter().map(|&i| times[i]).collect(),
        values: kept.iter().map(|&i| keys[i]).collect(),
        exact_rotation: false,
    }
}

/// Retargets `import`'s clip to `target` through the bone map (or guessed humanoid roles).
///
/// With a non-empty `bone_map` each pair (source joint, target joint) shares one role named after the
/// target joint; otherwise both skeletons' roles are guessed from joint names.
///
/// # Errors
///
/// [`BvhError::Retarget`] when a mapped joint is missing or no roles pair.
pub fn retarget_import(
    import: &BvhImport,
    target: &Skeleton,
    bone_map: &BTreeMap<String, String>,
) -> Result<AnimationClip, BvhError> {
    use regia_animation::retarget::{RetargetProfile, Retargeter};
    let (source_profile, target_profile) = if bone_map.is_empty() {
        (
            RetargetProfile::guess_humanoid(&import.skeleton),
            RetargetProfile::guess_humanoid(target),
        )
    } else {
        let mut sp = RetargetProfile::default();
        let mut tp = RetargetProfile::default();
        for (src, tgt) in bone_map {
            sp = sp.with(tgt.clone(), src.clone());
            tp = tp.with(tgt.clone(), tgt.clone());
        }
        (sp, tp)
    };
    let retargeter = Retargeter::new(&import.skeleton, &source_profile, target, &target_profile)
        .map_err(|e| BvhError::Retarget(e.to_string()))?;
    let mut clip = retargeter.retarget_clip(&import.clip);
    clip.name = import.clip.name.clone();
    Ok(clip)
}

/// A valid BVH used by the fuzz registry and the docs: a hip with one spine joint and an end site.
pub const FIXTURE_BVH: &str = "HIERARCHY\nROOT Hips\n{\n  OFFSET 0 0 0\n  CHANNELS 6 Xposition Yposition Zposition Zrotation Xrotation Yrotation\n  JOINT Spine\n  {\n    OFFSET 0 10 0\n    CHANNELS 3 Zrotation Xrotation Yrotation\n    End Site\n    {\n      OFFSET 0 10 0\n    }\n  }\n}\nMOTION\nFrames: 2\nFrame Time: 0.0333333\n0 0 0 0 0 0 0 0 0\n0 0 100 0 0 0 0 90 0\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn exact() -> BvhImportSettings {
        BvhImportSettings {
            keep_forward: true,
            rotation_tolerance_degrees: 0.0,
            translation_tolerance: 0.0,
            ..Default::default()
        }
    }

    fn near(a: Quat, b: Quat) -> bool {
        (1.0 - a.dot(b).abs()) < 1e-6
    }

    #[test]
    fn fixture_parses() {
        let f = parse_bvh(FIXTURE_BVH.as_bytes(), &BvhLimits::default()).expect("parse");
        assert_eq!(f.joint_count(), 3);
        assert_eq!(f.frames, 2);
        assert_eq!(f.joint_names(), vec!["Hips", "Spine", "Spine_end"]);
    }

    #[test]
    fn zxy_order_composes_first_channel_outermost() {
        let bvh = "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 3 Zrotation Xrotation Yrotation\nEnd Site\n{\nOFFSET 0 1 0\n}\n}\nMOTION\nFrames: 1\nFrame Time: 0.1\n30 40 50\n";
        let out = convert_bvh(bvh.as_bytes(), "t", &exact()).expect("import");
        let want = Quat::from_rotation_z(30f32.to_radians())
            * Quat::from_rotation_x(40f32.to_radians())
            * Quat::from_rotation_y(50f32.to_radians());
        let mut pose = out.skeleton.bind_pose_pose();
        out.clip.sample_pose(0.0, &mut pose);
        assert!(
            near(pose.rotations[0], want),
            "{:?} vs {:?}",
            pose.rotations[0],
            want
        );
    }

    #[test]
    fn xyz_order_differs_from_zyx() {
        let mk = |order: &str| {
            format!(
                "HIERARCHY\nROOT A\n{{\nOFFSET 0 0 0\nCHANNELS 3 {order}\n}}\nMOTION\nFrames: 1\nFrame Time: 0.1\n30 40 50\n"
            )
        };
        let a = convert_bvh(mk("Xrotation Yrotation Zrotation").as_bytes(), "t", &exact()).expect("a");
        let want = Quat::from_rotation_x(30f32.to_radians())
            * Quat::from_rotation_y(40f32.to_radians())
            * Quat::from_rotation_z(50f32.to_radians());
        let mut pose = a.skeleton.bind_pose_pose();
        a.clip.sample_pose(0.0, &mut pose);
        assert!(near(pose.rotations[0], want));
    }

    #[test]
    fn offsets_and_scale_apply() {
        let settings = BvhImportSettings {
            unit: BvhUnit::Centimeters,
            ..exact()
        };
        let out = convert_bvh(FIXTURE_BVH.as_bytes(), "t", &settings).expect("import");
        assert!((out.skeleton.bind_pose[1].translation - Vec3::new(0.0, 0.1, 0.0)).length() < 1e-6);
        let mut pose = out.skeleton.bind_pose_pose();
        out.clip.sample_pose(0.0333333, &mut pose);
        assert!(
            (pose.translations[0] - Vec3::new(0.0, 0.0, 1.0)).length() < 1e-5,
            "{:?}",
            pose.translations[0]
        );
        assert!(near(pose.rotations[1], Quat::from_rotation_x(90f32.to_radians())));
    }

    #[test]
    fn convert_forward_yaws_roots_only() {
        let settings = BvhImportSettings {
            keep_forward: false,
            ..exact()
        };
        let out = convert_bvh(FIXTURE_BVH.as_bytes(), "t", &settings).expect("import");
        let mut pose = out.skeleton.bind_pose_pose();
        out.clip.sample_pose(0.0333333, &mut pose);
        // Root translation (0,0,1) yawed 180 degrees about Y is (0,0,-1); the child stays local.
        assert!(
            (pose.translations[0] - Vec3::new(0.0, 0.0, -100.0)).length() < 1e-3,
            "{:?}",
            pose.translations[0]
        );
        assert!((out.skeleton.bind_pose[1].translation - Vec3::new(0.0, 10.0, 0.0)).length() < 1e-5);
    }

    #[test]
    fn z_up_turns_to_y_up() {
        let settings = BvhImportSettings {
            up_axis: BvhUpAxis::Z,
            ..exact()
        };
        let bvh = "HIERARCHY\nROOT A\n{\nOFFSET 0 0 5\nCHANNELS 0\nJOINT B\n{\nOFFSET 0 0 1\nCHANNELS 3 Xrotation Yrotation Zrotation\nEnd Site\n{\nOFFSET 0 0 1\n}\n}\n}\nMOTION\nFrames: 1\nFrame Time: 0.1\n0 0 0\n";
        let out = convert_bvh(bvh.as_bytes(), "t", &settings).expect("import");
        assert!((out.skeleton.bind_pose[0].translation - Vec3::new(0.0, 5.0, 0.0)).length() < 1e-5);
    }

    #[test]
    fn root_motion_tag() {
        let settings = BvhImportSettings {
            root_motion: BvhRootMotion::XZ,
            ..exact()
        };
        let out = convert_bvh(FIXTURE_BVH.as_bytes(), "t", &settings).expect("import");
        let rm = out.clip.root_motion.expect("root motion");
        assert_eq!((rm.joint, rm.mode), (0, RootMotionMode::XZ));
        assert!(
            convert_bvh(FIXTURE_BVH.as_bytes(), "t", &exact())
                .expect("import")
                .clip
                .root_motion
                .is_none()
        );
    }

    #[test]
    fn key_reduction_respects_tolerance() {
        // A straight Y-rotation ramp over 11 frames reduces to its two ends; a bend keeps three.
        let mut bvh = String::from(
            "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 3 Yrotation Xrotation Zrotation\n}\nMOTION\nFrames: 11\nFrame Time: 0.1\n",
        );
        for f in 0..11 {
            let y = if f <= 5 {
                f as f32 * 2.0
            } else {
                10.0 + (f as f32 - 5.0) * 8.0
            };
            bvh.push_str(&format!("{y} 0 0\n"));
        }
        let loose = BvhImportSettings {
            rotation_tolerance_degrees: 0.5,
            ..exact()
        };
        let out = convert_bvh(bvh.as_bytes(), "t", &loose).expect("import");
        assert_eq!(out.clip.tracks[0].rotations.key_count(), 3);
        assert!(out.keys_after < out.keys_before);
        let full = convert_bvh(bvh.as_bytes(), "t", &exact()).expect("import");
        // Zero tolerance still drops exactly collinear keys; it never keeps fewer than the bend needs.
        assert!(full.clip.tracks[0].rotations.key_count() >= 3);
        // Every original key is within the stated tolerance of the reduced curve.
        for f in 0..11 {
            let t = f as f32 * 0.1;
            let a = out.clip.tracks[0].rotations.sample(t);
            let b = full.clip.tracks[0].rotations.sample(t);
            assert!(a.angle_between(b) <= 0.5f32.to_radians() + 2e-3);
        }
    }

    #[test]
    fn constant_curves_collapse() {
        let out = convert_bvh(FIXTURE_BVH.as_bytes(), "t", &exact()).expect("import");
        // Hips rotation is zero in both frames.
        assert!(matches!(out.clip.tracks[0].rotations, TrackCurve::Static(_)));
    }

    #[test]
    fn end_sites_optional() {
        let with = BvhImportSettings {
            end_sites: true,
            ..exact()
        };
        assert_eq!(
            convert_bvh(FIXTURE_BVH.as_bytes(), "t", &with)
                .expect("a")
                .skeleton
                .len(),
            3
        );
        assert_eq!(
            convert_bvh(FIXTURE_BVH.as_bytes(), "t", &exact())
                .expect("b")
                .skeleton
                .len(),
            2
        );
    }

    fn err_of(text: &str) -> BvhError {
        parse_bvh(text.as_bytes(), &BvhLimits::default()).expect_err("must fail")
    }

    #[test]
    fn malformed_files_report_line_and_code() {
        let e = err_of("HIERARCH\n");
        assert_eq!(
            (e.code(), matches!(e, BvhError::Syntax { line: 1, .. })),
            ("bvh_syntax", true)
        );
        let e = err_of("HIERARCHY\nROOT A\n{\nOFFSET 0 0\n");
        assert!(matches!(e, BvhError::Syntax { line: 4, .. }), "{e:?}");
        let e = err_of("HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 2 Xposition Xposition\n}\nMOTION\n");
        assert!(matches!(e, BvhError::Channels { line: 5, .. }), "{e:?}");
        assert_eq!(e.code(), "bvh_channels");
        let e = err_of("HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 1 Wrotation\n}\nMOTION\n");
        assert_eq!(e.code(), "bvh_channels");
        let e = err_of(
            "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 4 Xrotation Yrotation Zrotation Xrotation\n}\nMOTION\n",
        );
        assert_eq!(e.code(), "bvh_channels");
        let e = err_of("HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 1 Xrotation\nJOINT A\n{\n");
        assert!(matches!(e, BvhError::DuplicateJoint { line: 6, .. }), "{e:?}");
        assert_eq!(e.code(), "bvh_duplicate_joint");
        let e = err_of(
            "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 1 Xrotation\n}\nMOTION\nFrames: 2\nFrame Time: 0.1\n1\n",
        );
        assert!(matches!(e, BvhError::Motion { .. }), "{e:?}");
        assert_eq!(e.code(), "bvh_motion");
        let e = err_of(
            "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 1 Xrotation\n}\nMOTION\nFrames: 1\nFrame Time: 0.1\n1 2\n",
        );
        assert!(matches!(e, BvhError::Motion { line: 10, .. }), "{e:?}");
        let e = err_of(
            "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 1 Xrotation\n}\nMOTION\nFrames: 1\nFrame Time: 0\n1\n",
        );
        assert!(matches!(e, BvhError::Motion { line: 9, .. }), "{e:?}");
        let e = err_of(
            "HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\nCHANNELS 1 Xrotation\n}\nMOTION\nFrames: 1\nFrame Time: 0.1\nnan\n",
        );
        assert!(matches!(e, BvhError::Syntax { line: 10, .. }), "{e:?}");
        let e = err_of("HIERARCHY\nROOT A\n{\nOFFSET 0 0 0\n");
        assert_eq!(e.code(), "bvh_syntax");
        assert_eq!(err_of("").code(), "bvh_syntax");
    }

    #[test]
    fn limits_are_enforced() {
        let tight = BvhLimits {
            max_joints: 1,
            ..Default::default()
        };
        let e = parse_bvh(FIXTURE_BVH.as_bytes(), &tight).expect_err("joints");
        assert_eq!(e.code(), "bvh_limit");
        let tight = BvhLimits {
            max_frames: 1,
            ..Default::default()
        };
        assert_eq!(
            parse_bvh(FIXTURE_BVH.as_bytes(), &tight)
                .expect_err("frames")
                .code(),
            "bvh_limit"
        );
        let tight = BvhLimits {
            max_channels: 3,
            ..Default::default()
        };
        assert_eq!(
            parse_bvh(FIXTURE_BVH.as_bytes(), &tight)
                .expect_err("channels")
                .code(),
            "bvh_limit"
        );
        let tight = BvhLimits {
            max_file_bytes: 10,
            ..Default::default()
        };
        assert_eq!(
            parse_bvh(FIXTURE_BVH.as_bytes(), &tight)
                .expect_err("size")
                .code(),
            "bvh_too_large"
        );
        let tight = BvhLimits {
            max_depth: 1,
            ..Default::default()
        };
        assert_eq!(
            parse_bvh(FIXTURE_BVH.as_bytes(), &tight)
                .expect_err("depth")
                .code(),
            "bvh_limit"
        );
    }

    #[test]
    fn settings_parse_and_validate() {
        let s = BvhImportSettings::try_from_meta_string(Some("(unit: Centimeters, root_motion: XZ)"))
            .expect("ron");
        assert_eq!(s.unit, BvhUnit::Centimeters);
        assert_eq!(
            BvhImportSettings::try_from_meta_string(Some("(unit_scale: 0.0)"))
                .expect_err("zero")
                .code(),
            "bvh_bad_settings"
        );
        assert_eq!(
            BvhImportSettings::try_from_meta_string(Some("(bogus: 1)"))
                .expect_err("unknown")
                .code(),
            "bvh_bad_settings"
        );
        assert!(BvhImportSettings::try_from_meta_string(None).is_ok());
        assert_eq!(
            BvhImportSettings::try_from_meta_string(Some("("))
                .expect_err("bad")
                .code(),
            "bvh_bad_settings"
        );
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 1u64;
        let mut bytes = FIXTURE_BVH.as_bytes().to_vec();
        for _ in 0..500 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let i = (seed >> 33) as usize % bytes.len();
            bytes[i] = (seed >> 20) as u8;
            let _ = convert_bvh(&bytes, "t", &BvhImportSettings::default());
            bytes.truncate(bytes.len().max(10));
        }
    }
    #[test]
    fn retarget_moves_the_mapped_bones() {
        let target_bvh = FIXTURE_BVH.replace("Hips", "Pelvis").replace("Spine", "Chest");
        let target = convert_bvh(target_bvh.as_bytes(), "target", &exact()).expect("target");
        let source = convert_bvh(FIXTURE_BVH.as_bytes(), "src", &exact()).expect("source");
        let map: BTreeMap<String, String> = [("Hips", "Pelvis"), ("Spine", "Chest")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let clip = retarget_import(&source, &target.skeleton, &map).expect("retarget");
        let mut pose = target.skeleton.bind_pose_pose();
        clip.sample_pose(0.0333333, &mut pose);
        // Chest bends 90 degrees about X in the source's second frame; the mapped bone follows.
        assert!(
            near(pose.rotations[1], Quat::from_rotation_x(90f32.to_radians())),
            "{:?}",
            pose.rotations[1]
        );
        // A map naming a joint the skeleton lacks is a documented error.
        let mut bad = map.clone();
        bad.insert("Hips".to_string(), "Nope".to_string());
        let err = retarget_import(&source, &target.skeleton, &bad).expect_err("bad map");
        assert_eq!(err.code(), "bvh_retarget");
    }
}
