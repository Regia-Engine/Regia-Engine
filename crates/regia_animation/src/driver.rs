// SPDX-License-Identifier: MPL-2.0
//! The driver system (Tincal C4): expressions that map one channel to others, plus the
//! `.drivers.ron` document that bundles twist chains (C5), drivers and pose-space correctives.
//!
//! A [`Driver`] reads one **input** (a graph parameter, a morph weight, or a joint's rotation:
//! the signed roll or the unsigned swing away from its bind rotation), maps it through an
//! editable [`Curve`] (location = input, value = output), and writes one **output** (a morph
//! weight, a joint rotation about an axis, a joint translation or scale, or a named material
//! parameter). Drivers run in document order and each sees what the earlier ones wrote, so
//! one driver can feed another through a morph weight.
//!
//! A [`DriverSet`] is the on-disk document (`.drivers.ron`, format v1,
//! `docs/formats/drivers-ron.md`). [`CompiledRig`] is it resolved against a skeleton and a set
//! of morph names ([`MorphNames`]). It evaluates in this order each frame:
//!
//! 1. [`TwistChain`]s (C5), 2. [`HelperJoint`]s (C5), 3. [`Driver`]s, 4.
//!    [`PoseSpaceCorrective`]s (they read the final joint rotations).
//!
//! The morph side is a trait ([`MorphNames`]) so the driver system does not depend on how a
//! host stores morph targets: it needs names to indices and a mutable weight slice, which is
//! the shape of [`crate::MorphWeights`] once the morph-target lane gives that component names.
//!
//! Provenance (`docs/provenance.md`): driver curves are the animation-driver idea common to
//! DCC rigging tools (an input channel mapped through a curve to an output channel; Blender's
//! documented "Drivers" and Maya's set-driven-key are the public descriptions); the pose-space
//! correctives are in [`crate::corrective`].

use std::collections::HashSet;
use std::sync::Arc;

use regia_core::math::{Quat, Vec3, det};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::corrective::{CompiledCorrective, PoseSpaceCorrective};
use crate::curve::Curve;
use crate::params::{ParamValue, ParameterSet};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use crate::twist::{CompiledHelper, CompiledTwist, HelperJoint, TwistChain, rotation_angle, swing_twist};

/// The `.drivers.ron` format version. Version 1 is the first freeze.
pub const DRIVERS_FORMAT_VERSION: u32 = 1;

fn one() -> f32 {
    1.0
}

/// Where a driver reads its value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DriverInput {
    /// A float, int or bool animator parameter (bool reads 0 or 1; unset reads 0).
    Param(String),
    /// A morph weight by name (as written by the animation so far and earlier drivers).
    Morph(String),
    /// The signed roll of a joint about `axis` (local frame) away from its bind rotation,
    /// degrees (`-180..=180`): the swing-twist twist angle. Use it for hinge joints too: an
    /// elbow bending about X reads as a roll about X.
    JointAngle {
        /// Joint name.
        joint: String,
        /// Axis in the joint's local frame.
        axis: Vec3,
    },
    /// The unsigned swing of a joint away from `axis` (the bone direction) once the roll about
    /// it is removed, degrees (`0..=180`).
    JointSwing {
        /// Joint name.
        joint: String,
        /// Bone axis in the joint's local frame.
        axis: Vec3,
    },
    /// The total rotation of a joint away from its bind rotation, degrees (`0..=180`).
    JointRotation {
        /// Joint name.
        joint: String,
    },
}

/// What a driver writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DriverOutput {
    /// A morph weight by name; combined with the existing weight by [`Driver::blend`].
    Morph(String),
    /// A named material parameter (wrinkle-map weights and the like), read back from
    /// [`RigState::materials`]; combined by [`Driver::blend`], starting from zero.
    Material(String),
    /// Rotate a joint (post-multiplied in its local frame) about `axis` by the driven value in
    /// degrees. Always additive.
    JointRotate {
        /// Joint name.
        joint: String,
        /// Axis in the joint's local frame.
        axis: Vec3,
    },
    /// Translate a joint's local position by `direction * value` metres. Always additive.
    JointTranslate {
        /// Joint name.
        joint: String,
        /// Direction per unit of driven value.
        direction: Vec3,
    },
    /// Scale a joint: local scale is multiplied by `1 + per_unit * value`. Always additive.
    JointScale {
        /// Joint name.
        joint: String,
        /// Scale change per unit of driven value on each axis.
        per_unit: Vec3,
    },
}

/// How a driver's value combines with the morph or material it writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverBlend {
    /// Add to the existing value.
    #[default]
    Add,
    /// Replace the existing value.
    Set,
    /// Multiply the existing value.
    Multiply,
}

/// One driver: input → curve → output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Driver {
    /// Name for the agent and the editor.
    pub name: String,
    /// The channel read.
    pub input: DriverInput,
    /// Maps the input (the key location) to the output value. An empty curve is rejected.
    pub curve: Curve,
    /// The channel written.
    pub output: DriverOutput,
    /// How morph and material outputs combine (joint outputs always add).
    #[serde(default)]
    pub blend: DriverBlend,
    /// Multiplier on the curve's value (default 1).
    #[serde(default = "one")]
    pub weight: f32,
}

/// The `.drivers.ron` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriverSet {
    /// Format version of this file.
    pub format_version: u32,
    /// Twist distribution chains (C5).
    #[serde(default)]
    pub twists: Vec<TwistChain>,
    /// Helper joints (C5).
    #[serde(default)]
    pub helpers: Vec<HelperJoint>,
    /// Drivers, evaluated in order.
    #[serde(default)]
    pub drivers: Vec<Driver>,
    /// Pose-space correctives, evaluated last.
    #[serde(default)]
    pub correctives: Vec<PoseSpaceCorrective>,
}

impl Default for DriverSet {
    fn default() -> Self {
        Self {
            format_version: DRIVERS_FORMAT_VERSION,
            twists: Vec::new(),
            helpers: Vec::new(),
            drivers: Vec::new(),
            correctives: Vec::new(),
        }
    }
}

/// Errors loading or compiling a `.drivers.ron`.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// RON parse error.
    #[error("drivers RON parse error: {0}")]
    RonParse(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("drivers RON write error: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file is newer than this engine, or no migration reaches the current version.
    #[error("drivers format version {0} is unknown (current is {DRIVERS_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The document is malformed (missing version field).
    #[error("invalid drivers document: {0}")]
    Invalid(String),
    /// Names or shapes that do not resolve; one entry per problem.
    #[error("the drivers do not fit the skeleton: {}", .0.join("; "))]
    Unresolved(Vec<String>),
}

/// The serde field-list fingerprint of the format (every nested struct and enum).
#[must_use]
pub fn drivers_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<DriverSet>(),
        serde_field_names::<Driver>(),
        serde_variant_names::<DriverInput>(),
        serde_variant_names::<DriverOutput>(),
        serde_variant_names::<DriverBlend>(),
        serde_field_names::<TwistChain>(),
        serde_field_names::<crate::twist::TwistTarget>(),
        serde_field_names::<HelperJoint>(),
        serde_field_names::<crate::twist::HelperOffsetKey>(),
        serde_field_names::<PoseSpaceCorrective>(),
        serde_field_names::<crate::corrective::CorrectivePose>(),
        serde_field_names::<Curve>(),
        serde_field_names::<crate::curve::CurveKey>(),
        serde_variant_names::<crate::curve::CurveKeyKind>(),
    ])
}

/// `from` versions of the registered migrations (for `cargo xtask check-formats`).
#[must_use]
pub fn drivers_migration_sources() -> Vec<u32> {
    Vec::new()
}

impl DriverSet {
    /// Parses `.drivers.ron` text: a newer version is rejected even when the body no longer
    /// parses.
    ///
    /// # Errors
    ///
    /// Syntax errors and unknown versions.
    pub fn from_ron_str(text: &str) -> Result<Self, DriverError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| DriverError::Invalid(format!("cannot read format_version: {e}")))?;
        if version != DRIVERS_FORMAT_VERSION {
            return Err(DriverError::UnknownVersion(version));
        }
        Ok(ron::from_str(text)?)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, DriverError> {
        let mut document = self.clone();
        document.format_version = DRIVERS_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&document, pretty)?)
    }

    /// Every morph target name the document reads or writes, in first-appearance order.
    #[must_use]
    pub fn morph_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut add = |name: &str| {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        };
        for driver in &self.drivers {
            if let DriverInput::Morph(m) = &driver.input {
                add(m);
            }
            if let DriverOutput::Morph(m) = &driver.output {
                add(m);
            }
        }
        for corrective in &self.correctives {
            for pose in &corrective.poses {
                for (morph, _) in &pose.morphs {
                    add(morph);
                }
            }
        }
        names
    }

    /// Resolve against a skeleton and a morph set. Convenience for [`CompiledRig::compile`].
    ///
    /// # Errors
    ///
    /// See [`CompiledRig::compile`].
    pub fn compile(&self, skeleton: &Skeleton, morphs: &dyn MorphNames) -> Result<CompiledRig, DriverError> {
        CompiledRig::compile(self, skeleton, morphs)
    }
}

/// The morph-target side of the driver system: names to indices.
///
/// Implemented for name lists (`[String]`, `Vec<String>`) and [`NamedMorphs`]. A morph-target
/// container with names implements it to be driven.
pub trait MorphNames {
    /// Number of morph targets (the weight bank is at least this long).
    fn morph_count(&self) -> usize;
    /// Index of the morph target called `name`.
    fn morph_index(&self, name: &str) -> Option<usize>;
}

impl MorphNames for [String] {
    fn morph_count(&self) -> usize {
        self.len()
    }
    fn morph_index(&self, name: &str) -> Option<usize> {
        self.iter().position(|n| n == name)
    }
}

impl MorphNames for Vec<String> {
    fn morph_count(&self) -> usize {
        self.len()
    }
    fn morph_index(&self, name: &str) -> Option<usize> {
        self.as_slice().morph_index(name)
    }
}

/// A morph bank with names: the names that go with a [`crate::MorphWeights`] vector.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NamedMorphs {
    /// Morph target names, indexed like the weights.
    pub names: Vec<String>,
}

impl NamedMorphs {
    /// The names a document needs, in first-appearance order.
    #[must_use]
    pub fn from_set(set: &DriverSet) -> Self {
        Self {
            names: set.morph_names(),
        }
    }
}

impl MorphNames for NamedMorphs {
    fn morph_count(&self) -> usize {
        self.names.len()
    }
    fn morph_index(&self, name: &str) -> Option<usize> {
        self.names.morph_index(name)
    }
}

#[derive(Clone, Debug)]
enum CInput {
    Param(String),
    Morph(usize),
    Angle(usize, Vec3),
    Swing(usize, Vec3),
    Rotation(usize),
}

#[derive(Clone, Debug)]
enum COutput {
    Morph(usize),
    Material(usize),
    Rotate(usize, Vec3),
    Translate(usize, Vec3),
    Scale(usize, Vec3),
}

#[derive(Clone, Debug)]
struct CDriver {
    name: String,
    input: CInput,
    curve: Curve,
    output: COutput,
    blend: DriverBlend,
    weight: f32,
}

/// What the last evaluation produced, for the agent protocol and debugging; also holds the
/// material parameter outputs. Reused across frames (no allocation after the first).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RigState {
    /// Material parameter values (indexed like [`CompiledRig::material_names`]); reset to zero
    /// at the start of each evaluation.
    pub materials: Vec<f32>,
    /// Per driver: the input value read (degrees for joint inputs).
    pub driver_inputs: Vec<f32>,
    /// Per driver: the curve value written (after `weight`).
    pub driver_outputs: Vec<f32>,
    /// Per twist chain: the roll measured on its source, degrees.
    pub twist_degrees: Vec<f32>,
    /// Per helper: the source's rotation away from bind, degrees.
    pub helper_degrees: Vec<f32>,
    /// Per corrective: the kernel activation of each example pose (`0..=1`).
    pub corrective_activations: Vec<Vec<f32>>,
}

/// A [`DriverSet`] resolved against a skeleton and a morph set, ready to evaluate.
#[derive(Clone, Debug)]
pub struct CompiledRig {
    twists: Vec<CompiledTwist>,
    helpers: Vec<CompiledHelper>,
    drivers: Vec<CDriver>,
    correctives: Vec<CompiledCorrective>,
    bind: Vec<Quat>,
    morph_count: usize,
    material_names: Vec<String>,
}

impl CompiledRig {
    /// Resolve every joint, morph and material name; the problems of the whole document are
    /// reported together.
    ///
    /// # Errors
    ///
    /// [`DriverError::Unresolved`] with one entry per problem: unknown joints or morph
    /// targets, empty curves, zero axes, duplicate driver names, bad corrective shapes.
    pub fn compile(
        set: &DriverSet,
        skeleton: &Skeleton,
        morphs: &dyn MorphNames,
    ) -> Result<Self, DriverError> {
        let mut problems: Vec<String> = Vec::new();
        let mut twists = Vec::new();
        for chain in &set.twists {
            match CompiledTwist::compile(chain, skeleton) {
                Ok(t) => twists.push(t),
                Err(p) => problems.extend(p),
            }
        }
        let mut helpers = Vec::new();
        for helper in &set.helpers {
            match CompiledHelper::compile(helper, skeleton) {
                Ok(h) => helpers.push(h),
                Err(p) => problems.extend(p),
            }
        }
        let mut material_names: Vec<String> = Vec::new();
        let mut drivers = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for driver in &set.drivers {
            let label = format!("driver {:?}", driver.name);
            if !seen.insert(driver.name.as_str()) {
                problems.push(format!("{label}: duplicate name"));
            }
            if driver.curve.is_empty() {
                problems.push(format!("{label}: curve has no keys"));
            }
            if driver
                .curve
                .keys
                .iter()
                .any(|k| !k.location.is_finite() || !k.value.is_finite())
            {
                problems.push(format!("{label}: curve has a non-finite key"));
            }
            let joint = |name: &str, problems: &mut Vec<String>| {
                let found = skeleton.joint_index(name).map(|j| j as usize);
                if found.is_none() {
                    problems.push(format!("{label}: no joint {name:?}"));
                }
                found
            };
            let axis_of = |axis: Vec3, problems: &mut Vec<String>| {
                let unit = axis.try_normalize();
                if unit.is_none() {
                    problems.push(format!("{label}: axis must not be zero"));
                }
                unit
            };
            let input = match &driver.input {
                DriverInput::Param(p) => Some(CInput::Param(p.clone())),
                DriverInput::Morph(m) => match morphs.morph_index(m) {
                    Some(i) => Some(CInput::Morph(i)),
                    None => {
                        problems.push(format!("{label}: no morph target {m:?}"));
                        None
                    }
                },
                DriverInput::JointAngle { joint: j, axis } => {
                    let j = joint(j, &mut problems);
                    let axis = axis_of(*axis, &mut problems);
                    j.zip(axis).map(|(j, a)| CInput::Angle(j, a))
                }
                DriverInput::JointSwing { joint: j, axis } => {
                    let j = joint(j, &mut problems);
                    let axis = axis_of(*axis, &mut problems);
                    j.zip(axis).map(|(j, a)| CInput::Swing(j, a))
                }
                DriverInput::JointRotation { joint: j } => joint(j, &mut problems).map(CInput::Rotation),
            };
            let output = match &driver.output {
                DriverOutput::Morph(m) => match morphs.morph_index(m) {
                    Some(i) => Some(COutput::Morph(i)),
                    None => {
                        problems.push(format!("{label}: no morph target {m:?}"));
                        None
                    }
                },
                DriverOutput::Material(name) => {
                    let index = material_names.iter().position(|n| n == name).unwrap_or_else(|| {
                        material_names.push(name.clone());
                        material_names.len() - 1
                    });
                    Some(COutput::Material(index))
                }
                DriverOutput::JointRotate { joint: j, axis } => {
                    let j = joint(j, &mut problems);
                    let axis = axis_of(*axis, &mut problems);
                    j.zip(axis).map(|(j, a)| COutput::Rotate(j, a))
                }
                DriverOutput::JointTranslate { joint: j, direction } => {
                    joint(j, &mut problems).map(|j| COutput::Translate(j, *direction))
                }
                DriverOutput::JointScale { joint: j, per_unit } => {
                    joint(j, &mut problems).map(|j| COutput::Scale(j, *per_unit))
                }
            };
            if let (Some(input), Some(output)) = (input, output) {
                drivers.push(CDriver {
                    name: driver.name.clone(),
                    input,
                    curve: driver.curve.clone(),
                    output,
                    blend: driver.blend,
                    weight: driver.weight,
                });
            }
        }
        let mut correctives = Vec::new();
        for corrective in &set.correctives {
            match CompiledCorrective::compile(corrective, skeleton, &|m| morphs.morph_index(m)) {
                Ok(c) => correctives.push(c),
                Err(p) => problems.extend(p),
            }
        }
        if !problems.is_empty() {
            return Err(DriverError::Unresolved(problems));
        }
        Ok(CompiledRig {
            twists,
            helpers,
            drivers,
            correctives,
            bind: skeleton.bind_pose.iter().map(|b| b.rotation).collect(),
            morph_count: morphs.morph_count(),
            material_names,
        })
    }

    /// Number of morph weights the rig reads and writes.
    #[must_use]
    pub fn morph_count(&self) -> usize {
        self.morph_count
    }

    /// Material parameter names, indexed like [`RigState::materials`].
    #[must_use]
    pub fn material_names(&self) -> &[String] {
        &self.material_names
    }

    /// Driver names, in evaluation order.
    pub fn driver_names(&self) -> impl Iterator<Item = &str> {
        self.drivers.iter().map(|d| d.name.as_str())
    }

    /// Twist chain names.
    pub fn twist_names(&self) -> impl Iterator<Item = &str> {
        self.twists.iter().map(|t| t.name.as_str())
    }

    /// Helper joint names.
    pub fn helper_names(&self) -> impl Iterator<Item = &str> {
        self.helpers.iter().map(|h| h.name.as_str())
    }

    /// Corrective names.
    pub fn corrective_names(&self) -> impl Iterator<Item = &str> {
        self.correctives.iter().map(|c| c.name.as_str())
    }

    fn read(&self, input: &CInput, pose: &Pose, params: &ParameterSet, morphs: &[f32]) -> f32 {
        let delta = |joint: usize| match pose.rotations.get(joint) {
            Some(rotation) => self.bind[joint].inverse() * *rotation,
            None => Quat::IDENTITY,
        };
        match input {
            CInput::Param(name) => match params.get(name) {
                Some(ParamValue::Float(v)) => v,
                Some(ParamValue::Int(v)) => v as f32,
                Some(ParamValue::Bool(v)) => f32::from(u8::from(v)),
                _ => 0.0,
            },
            CInput::Morph(index) => morphs.get(*index).copied().unwrap_or(0.0),
            CInput::Angle(joint, axis) => swing_twist(delta(*joint), *axis).1.to_degrees(),
            CInput::Swing(joint, axis) => {
                let (swing, _) = swing_twist(delta(*joint), *axis);
                rotation_angle(swing).to_degrees()
            }
            CInput::Rotation(joint) => rotation_angle(delta(*joint)).to_degrees(),
        }
    }

    /// Evaluate the rig: modifies `pose` (twists, helpers, joint-driving drivers), adds to
    /// `morphs` (grown to at least [`CompiledRig::morph_count`]) and fills `state`.
    ///
    /// Call it on a copy of the animator's pose: joint outputs add to the pose they find.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn evaluate(
        &self,
        pose: &mut Pose,
        params: &ParameterSet,
        morphs: &mut Vec<f32>,
        state: &mut RigState,
    ) {
        if morphs.len() < self.morph_count {
            morphs.resize(self.morph_count, 0.0);
        }
        state.materials.clear();
        state.materials.resize(self.material_names.len(), 0.0);
        state.twist_degrees.clear();
        state.helper_degrees.clear();
        state.driver_inputs.clear();
        state.driver_outputs.clear();
        for twist in &self.twists {
            state.twist_degrees.push(twist.apply(pose));
        }
        for helper in &self.helpers {
            state.helper_degrees.push(helper.apply(pose));
        }
        for driver in &self.drivers {
            let input = self.read(&driver.input, pose, params, morphs);
            let value = driver.curve.value_at(input) * driver.weight;
            state.driver_inputs.push(input);
            state.driver_outputs.push(value);
            let combine = |old: f32| match driver.blend {
                DriverBlend::Add => old + value,
                DriverBlend::Set => value,
                DriverBlend::Multiply => old * value,
            };
            match &driver.output {
                COutput::Morph(index) => {
                    if let Some(slot) = morphs.get_mut(*index) {
                        *slot = combine(*slot);
                    }
                }
                COutput::Material(index) => {
                    if let Some(slot) = state.materials.get_mut(*index) {
                        *slot = combine(*slot);
                    }
                }
                COutput::Rotate(joint, axis) => {
                    if let Some(rotation) = pose.rotations.get_mut(*joint) {
                        *rotation =
                            (*rotation * det::quat_from_axis_angle(*axis, value.to_radians())).normalize();
                    }
                }
                COutput::Translate(joint, direction) => {
                    if let Some(t) = pose.translations.get_mut(*joint) {
                        *t += *direction * value;
                    }
                }
                COutput::Scale(joint, per_unit) => {
                    if let Some(s) = pose.scales.get_mut(*joint) {
                        *s *= Vec3::ONE + *per_unit * value;
                    }
                }
            }
        }
        state
            .corrective_activations
            .resize_with(self.correctives.len(), Vec::new);
        for (corrective, activations) in self.correctives.iter().zip(&mut state.corrective_activations) {
            corrective.apply(pose, morphs, activations);
        }
    }
}

/// Attach to a rig entity to run its [`CompiledRig`] in `PostUpdateSet::AnimationApply`,
/// after the animator's pose is evaluated and before it is written to the joints. The animator's
/// own pose is not modified.
#[derive(Component, Clone, Debug)]
pub struct RigDrivers {
    /// The compiled rig (shared between instances of one character).
    pub rig: Arc<CompiledRig>,
    /// The last evaluation (filled by the apply system).
    pub state: RigState,
}

impl RigDrivers {
    /// Wrap a compiled rig.
    #[must_use]
    pub fn new(rig: Arc<CompiledRig>) -> Self {
        Self {
            rig,
            state: RigState::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{CurveKey, CurveKeyKind};
    use crate::skeleton::LocalBind;

    fn skeleton() -> Skeleton {
        Skeleton::from_local_bind(
            ["root", "upper", "fore", "hand", "pad"]
                .map(String::from)
                .to_vec(),
            vec![None, Some(0), Some(1), Some(2), Some(0)],
            vec![LocalBind::default(); 5],
        )
        .expect("valid")
    }

    fn line(points: &[(f32, f32)]) -> Curve {
        Curve::from(
            points
                .iter()
                .map(|&(x, y)| CurveKey::new(x, y, CurveKeyKind::Linear))
                .collect::<Vec<_>>(),
        )
    }

    fn bend_driver(output: DriverOutput) -> Driver {
        Driver {
            name: "elbow".into(),
            input: DriverInput::JointAngle {
                joint: "fore".into(),
                axis: Vec3::X,
            },
            curve: line(&[(0.0, 0.0), (90.0, 1.0)]),
            output,
            blend: DriverBlend::Add,
            weight: 1.0,
        }
    }

    fn run(set: &DriverSet, pose: &mut Pose, params: &ParameterSet) -> (Vec<f32>, RigState) {
        let morphs = NamedMorphs::from_set(set);
        let rig = set.compile(&skeleton(), &morphs).expect("compiles");
        let mut weights = Vec::new();
        let mut state = RigState::default();
        rig.evaluate(pose, params, &mut weights, &mut state);
        (weights, state)
    }

    #[test]
    fn a_joint_angle_drives_a_morph_through_its_curve() {
        let set = DriverSet {
            drivers: vec![bend_driver(DriverOutput::Morph("elbow_bulge".into()))],
            ..DriverSet::default()
        };
        let mut pose = skeleton().bind_pose_pose();
        pose.rotations[2] = Quat::from_rotation_x(45f32.to_radians());
        let (weights, state) = run(&set, &mut pose, &ParameterSet::new());
        assert!((weights[0] - 0.5).abs() < 1e-4, "{weights:?}");
        assert!((state.driver_inputs[0] - 45.0).abs() < 1e-3);
        // Beyond the last key the curve clamps; the opposite bend reads as negative and clamps at zero.
        pose.rotations[2] = Quat::from_rotation_x(120f32.to_radians());
        assert!((run(&set, &mut pose, &ParameterSet::new()).0[0] - 1.0).abs() < 1e-4);
        pose.rotations[2] = Quat::from_rotation_x(-30f32.to_radians());
        assert!(run(&set, &mut pose, &ParameterSet::new()).0[0].abs() < 1e-4);
    }

    #[test]
    fn parameters_drive_morphs_and_blend_modes_combine() {
        let mut set = DriverSet::default();
        let from_param = |name: &str, blend, weight| Driver {
            name: name.into(),
            input: DriverInput::Param("smile".into()),
            curve: line(&[(0.0, 0.0), (1.0, 1.0)]),
            output: DriverOutput::Morph("mouth".into()),
            blend,
            weight,
        };
        set.drivers.push(from_param("a", DriverBlend::Add, 1.0));
        set.drivers.push(from_param("b", DriverBlend::Add, 0.5));
        let mut params = ParameterSet::new();
        params.set_float("smile", 0.8);
        let (weights, _) = run(&set, &mut skeleton().bind_pose_pose(), &params);
        assert!((weights[0] - 1.2).abs() < 1e-5, "{weights:?}");
        set.drivers.push(from_param("c", DriverBlend::Set, 1.0));
        assert!((run(&set, &mut skeleton().bind_pose_pose(), &params).0[0] - 0.8).abs() < 1e-5);
        set.drivers.push(from_param("d", DriverBlend::Multiply, 0.5));
        assert!((run(&set, &mut skeleton().bind_pose_pose(), &params).0[0] - 0.8 * 0.4).abs() < 1e-5);
        // An unset parameter reads as zero; a bool as 0 or 1.
        let mut params = ParameterSet::new();
        params.set_bool("smile", true);
        assert!((run(&set, &mut skeleton().bind_pose_pose(), &params).0[0] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn drivers_chain_through_morph_weights_in_order() {
        let set = DriverSet {
            drivers: vec![
                Driver {
                    name: "first".into(),
                    input: DriverInput::Param("x".into()),
                    curve: line(&[(0.0, 0.0), (1.0, 2.0)]),
                    output: DriverOutput::Morph("a".into()),
                    blend: DriverBlend::Set,
                    weight: 1.0,
                },
                Driver {
                    name: "second".into(),
                    input: DriverInput::Morph("a".into()),
                    curve: line(&[(0.0, 0.0), (2.0, 1.0)]),
                    output: DriverOutput::Morph("b".into()),
                    blend: DriverBlend::Set,
                    weight: 1.0,
                },
            ],
            ..DriverSet::default()
        };
        let mut params = ParameterSet::new();
        params.set_float("x", 0.5);
        let (weights, _) = run(&set, &mut skeleton().bind_pose_pose(), &params);
        assert_eq!(set.morph_names(), ["a", "b"]);
        assert!(
            (weights[0] - 1.0).abs() < 1e-5 && (weights[1] - 0.5).abs() < 1e-5,
            "{weights:?}"
        );
    }

    #[test]
    fn a_morph_weight_can_drive_a_joint_and_a_material() {
        let set = DriverSet {
            drivers: vec![
                Driver {
                    name: "pad".into(),
                    input: DriverInput::Param("lift".into()),
                    curve: line(&[(0.0, 0.0), (1.0, 30.0)]),
                    output: DriverOutput::JointRotate {
                        joint: "pad".into(),
                        axis: Vec3::Z,
                    },
                    blend: DriverBlend::Add,
                    weight: 1.0,
                },
                Driver {
                    name: "wrinkle".into(),
                    input: DriverInput::JointRotation { joint: "fore".into() },
                    curve: line(&[(0.0, 0.0), (100.0, 1.0)]),
                    output: DriverOutput::Material("wrinkle_elbow".into()),
                    blend: DriverBlend::Add,
                    weight: 1.0,
                },
                Driver {
                    name: "slide".into(),
                    input: DriverInput::Param("lift".into()),
                    curve: line(&[(0.0, 0.0), (1.0, 0.1)]),
                    output: DriverOutput::JointTranslate {
                        joint: "pad".into(),
                        direction: Vec3::Y,
                    },
                    blend: DriverBlend::Add,
                    weight: 1.0,
                },
                Driver {
                    name: "swell".into(),
                    input: DriverInput::Param("lift".into()),
                    curve: line(&[(0.0, 0.0), (1.0, 1.0)]),
                    output: DriverOutput::JointScale {
                        joint: "pad".into(),
                        per_unit: Vec3::splat(0.5),
                    },
                    blend: DriverBlend::Add,
                    weight: 1.0,
                },
            ],
            ..DriverSet::default()
        };
        let mut pose = skeleton().bind_pose_pose();
        pose.rotations[2] = Quat::from_rotation_x(50f32.to_radians());
        let mut params = ParameterSet::new();
        params.set_float("lift", 1.0);
        let (_, state) = run(&set, &mut pose, &params);
        assert!(pose.rotations[4].angle_between(Quat::from_rotation_z(30f32.to_radians())) < 1e-5);
        assert!((pose.translations[4].y - 0.1).abs() < 1e-6);
        assert!((pose.scales[4].x - 1.5).abs() < 1e-6);
        assert!((state.materials[0] - 0.5).abs() < 1e-4, "{:?}", state.materials);
    }

    #[test]
    fn swing_input_ignores_roll() {
        let set = DriverSet {
            drivers: vec![Driver {
                input: DriverInput::JointSwing {
                    joint: "upper".into(),
                    axis: Vec3::Y,
                },
                ..bend_driver(DriverOutput::Morph("m".into()))
            }],
            ..DriverSet::default()
        };
        let mut pose = skeleton().bind_pose_pose();
        pose.rotations[1] = Quat::from_rotation_x(30f32.to_radians()) * Quat::from_rotation_y(1.0);
        let (_, state) = run(&set, &mut pose, &ParameterSet::new());
        assert!(
            (state.driver_inputs[0] - 30.0).abs() < 1e-3,
            "{:?}",
            state.driver_inputs
        );
    }

    #[test]
    fn twists_and_correctives_run_through_the_rig() {
        let ron = r#"(
            format_version: 1,
            twists: [(name: "fore", source: "hand", axis: (0.0, 1.0, 0.0),
                driven: [(joint: "upper", weight: 0.5)], source_keep: 1.0)],
            correctives: [(name: "elbow", joints: ["fore"],
                poses: [(name: "bent", rotations: [(100.0, 0.0, 0.0)], morphs: [("bulge", 1.0)])])],
        )"#;
        let set = DriverSet::from_ron_str(ron).expect("parses");
        let mut pose = skeleton().bind_pose_pose();
        pose.rotations[3] = Quat::from_rotation_y(1.0);
        pose.rotations[2] = Quat::from_rotation_x(100f32.to_radians());
        let (weights, state) = run(&set, &mut pose, &ParameterSet::new());
        assert!((state.twist_degrees[0] - 1.0f32.to_degrees()).abs() < 1e-3);
        assert!((weights[0] - 1.0).abs() < 1e-2, "{weights:?}");
        // The twist rotated "upper" about its own Y by half the roll.
        let (_, upper_roll) = swing_twist(pose.rotations[1], Vec3::Y);
        assert!((upper_roll - 0.5).abs() < 1e-5, "{upper_roll}");
    }

    #[test]
    fn compile_reports_every_problem_at_once() {
        let set = DriverSet {
            drivers: vec![
                Driver {
                    curve: Curve::default(),
                    ..bend_driver(DriverOutput::Morph("missing".into()))
                },
                Driver {
                    input: DriverInput::JointAngle {
                        joint: "tail".into(),
                        axis: Vec3::ZERO,
                    },
                    ..bend_driver(DriverOutput::Morph("m".into()))
                },
            ],
            ..DriverSet::default()
        };
        let names: Vec<String> = vec!["m".into()];
        let Err(DriverError::Unresolved(problems)) = set.compile(&skeleton(), &names) else {
            panic!("should not compile");
        };
        // empty curve, unknown morph, duplicate name, unknown joint, zero axis.
        assert_eq!(problems.len(), 5, "{problems:?}");
    }

    #[test]
    fn ron_round_trips_and_future_versions_are_rejected() {
        let set = DriverSet {
            drivers: vec![bend_driver(DriverOutput::Morph("m".into()))],
            twists: vec![crate::twist::TwistChain {
                name: "t".into(),
                source: "hand".into(),
                axis: Vec3::Y,
                driven: Vec::new(),
                source_keep: 1.0,
            }],
            ..DriverSet::default()
        };
        let text = set.to_ron_string().expect("writes");
        assert_eq!(DriverSet::from_ron_str(&text).expect("reads"), set);
        assert!(matches!(
            DriverSet::from_ron_str("(format_version: 2)"),
            Err(DriverError::UnknownVersion(2))
        ));
        assert!(matches!(
            DriverSet::from_ron_str("(drivers: [])"),
            Err(DriverError::Invalid(_))
        ));
    }

    #[test]
    fn fingerprint_lists_the_fields() {
        let fp = drivers_schema_fingerprint();
        for field in ["source_keep", "correctives", "JointAngle", "Multiply", "radius"] {
            assert!(fp.contains(field), "{field} missing from {fp}");
        }
        assert!(drivers_migration_sources().is_empty());
    }
}
