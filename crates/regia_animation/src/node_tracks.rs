// SPDX-License-Identifier: MPL-2.0
//! Source-node animation for rigid parts, independently of skeletal joint numbering.
//!
//! glTF 2.0 § 3.6 and Appendix C define interpolation and parent composition.
//! This is presentation data; it does not modify simulation state (ADR-8157).

use regia_core::math::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::clip::MorphNode;
use crate::skeleton::LocalBind;

/// Version of the node-track body carried inside cooked-clip v3.
pub const NODE_TRACKS_VERSION: u32 = 1;

/// Interpolation of source-node keys.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeInterpolation {
    /// Hold the preceding key until the next key's exact time.
    Step,
    /// Linear vectors and shortest-path spherical quaternion interpolation.
    Linear,
    /// Cubic Hermite; each key stores incoming tangent, value, outgoing tangent.
    CubicSpline,
}

/// A source channel, retaining its authored interpolation and tangents.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeCurve<T> {
    /// Strictly increasing, finite, nonnegative key times in seconds.
    pub times: Vec<f32>,
    /// One value per key, or three per key for cubic splines.
    pub values: Vec<T>,
    /// Authored interpolation mode.
    pub interpolation: NodeInterpolation,
}

impl<T> NodeCurve<T> {
    fn validate_shape(&self) -> Result<(), String> {
        let stride = if self.interpolation == NodeInterpolation::CubicSpline {
            3
        } else {
            1
        };
        if self.times.is_empty() || self.times.len().checked_mul(stride) != Some(self.values.len()) {
            return Err("node channel key counts disagree or are empty".into());
        }
        if self.times.iter().any(|t| !t.is_finite() || *t < 0.0)
            || self.times.windows(2).any(|p| p[0] >= p[1])
        {
            return Err("node channel times must be finite, nonnegative and strictly increasing".into());
        }
        Ok(())
    }

    fn span(&self, time: f32) -> Option<(usize, usize, f32, f32)> {
        let last = self.times.len().checked_sub(1)?;
        if last == 0 || time.is_nan() || time <= self.times[0] {
            return Some((0, 0, 0.0, 0.0));
        }
        if time >= self.times[last] {
            return Some((last, last, 0.0, 0.0));
        }
        let hi = self.times.partition_point(|t| *t <= time).min(last).max(1);
        let lo = hi - 1;
        let dt = self.times[hi] - self.times[lo];
        if !dt.is_finite() || dt <= 0.0 {
            return None;
        }
        Some((lo, hi, (time - self.times[lo]) / dt, dt))
    }
}

// Hermite basis from glTF Appendix C; tangents are derivatives per second.
fn hermite_weights(t: f32, dt: f32) -> [f32; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [
        2.0 * t3 - 3.0 * t2 + 1.0,
        (t3 - 2.0 * t2 + t) * dt,
        -2.0 * t3 + 3.0 * t2,
        (t3 - t2) * dt,
    ]
}

impl NodeCurve<Vec3> {
    /// Validate key times, counts and finite values, including cubic tangents.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_shape()?;
        if self.values.iter().any(|v| !v.is_finite()) {
            return Err("node vector channel contains a non-finite value".into());
        }
        Ok(())
    }

    /// Sample a validated channel, clamped to its key range; malformed indices return `None`.
    pub fn sample(&self, time: f32) -> Option<Vec3> {
        let (lo, hi, t, dt) = self.span(time)?;
        match self.interpolation {
            NodeInterpolation::Step => self.values.get(lo).copied(),
            NodeInterpolation::Linear => Some(self.values.get(lo)?.lerp(*self.values.get(hi)?, t)),
            NodeInterpolation::CubicSpline => {
                let [a, b, c, d] = hermite_weights(t, dt);
                Some(
                    *self.values.get(lo * 3 + 1)? * a
                        + *self.values.get(lo * 3 + 2)? * b
                        + *self.values.get(hi * 3 + 1)? * c
                        + *self.values.get(hi * 3)? * d,
                )
            }
        }
    }
}

impl NodeCurve<Quat> {
    /// Validate keys and tangents; quaternion values must have nonzero finite length.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_shape()?;
        let cubic = self.interpolation == NodeInterpolation::CubicSpline;
        for (i, value) in self.values.iter().enumerate() {
            if !value.is_finite()
                || ((!cubic || i % 3 == 1)
                    && (!value.length_squared().is_finite() || value.length_squared() <= f32::MIN_POSITIVE))
            {
                return Err("node rotation channel contains a non-finite or degenerate value".into());
            }
        }
        Ok(())
    }

    /// Sample a validated quaternion channel; cubic tangents are never normalized or flipped.
    pub fn sample(&self, time: f32) -> Option<Quat> {
        let (lo, hi, t, dt) = self.span(time)?;
        let value = match self.interpolation {
            NodeInterpolation::Step => *self.values.get(lo)?,
            NodeInterpolation::Linear => regia_core::math::det_slerp(
                self.values.get(lo)?.normalize(),
                self.values.get(hi)?.normalize(),
                t,
            ),
            NodeInterpolation::CubicSpline => {
                let [a, b, c, d] = hermite_weights(t, dt);
                *self.values.get(lo * 3 + 1)? * a
                    + *self.values.get(lo * 3 + 2)? * b
                    + *self.values.get(hi * 3 + 1)? * c
                    + *self.values.get(hi * 3)? * d
            }
        };
        (value.is_finite()
            && value.length_squared().is_finite()
            && value.length_squared() > f32::MIN_POSITIVE)
            .then(|| value.normalize())
    }
}

/// One source node, including static ancestors and descendants needed for composition.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeTrack {
    /// Original source index and informational name; duplicate names remain distinct.
    pub node: MorphNode,
    /// Parent's slot in [`NodeAnimation::nodes`], always before this slot.
    pub parent: Option<u32>,
    /// Whether this node is a joint of any source skin. Hosts must not apply its delta to a plain part.
    pub is_joint: bool,
    /// Authored local TRS, used for each channel that has no keys.
    pub bind: LocalBind,
    /// Exact static local matrix, for a source node authored as a matrix. Such nodes have no channels.
    pub matrix: Option<Mat4>,
    /// Local translation channel in source units.
    pub translations: Option<NodeCurve<Vec3>>,
    /// Local rotation channel.
    pub rotations: Option<NodeCurve<Quat>>,
    /// Local scale channel.
    pub scales: Option<NodeCurve<Vec3>>,
}

impl NodeTrack {
    /// Whether this node has an authored TRS channel.
    pub fn animated(&self) -> bool {
        self.translations.is_some() || self.rotations.is_some() || self.scales.is_some()
    }

    fn local(&self, time: Option<f32>) -> Mat4 {
        if let Some(matrix) = self.matrix {
            return matrix;
        }
        let t = time
            .and_then(|t| self.translations.as_ref()?.sample(t))
            .unwrap_or(self.bind.translation);
        let r = time
            .and_then(|t| self.rotations.as_ref()?.sample(t))
            .unwrap_or(self.bind.rotation);
        let s = time
            .and_then(|t| self.scales.as_ref()?.sample(t))
            .unwrap_or(self.bind.scale);
        Mat4::from_scale_rotation_translation(s, r, t)
    }
}

/// Versioned source-node hierarchy and tracks, stored losslessly even in a compressed cooked clip.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NodeAnimation {
    /// Must equal [`NODE_TRACKS_VERSION`]; unknown versions are rejected.
    pub version: u32,
    /// Source scene to engine scene conversion (axis and uniform import scale), applied once.
    pub frame: Mat4,
    /// Source-time start of this clip or slice.
    pub start: f32,
    /// Source-time end of this clip or slice.
    pub end: f32,
    /// Nodes in parent-before-child order, identified by original indices.
    pub nodes: Vec<NodeTrack>,
}

/// A sampled node's matrices, ready for a host to bind by source index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeSample {
    /// Original source-node index.
    pub index: u32,
    /// Whether the source node belongs to a skin's joint list.
    pub is_joint: bool,
    /// Local matrix in the source parent's frame.
    pub local: Mat4,
    /// Animated matrix in the imported scene frame.
    pub world: Mat4,
    /// Bind matrix in the imported scene frame.
    pub bind_world: Mat4,
    /// `world * inverse(bind_world)` for vertices baked into the imported scene frame.
    /// `None` for a singular bind matrix; callers may use `world` for local-space geometry.
    pub delta: Option<Mat4>,
}

impl NodeAnimation {
    /// Decode a standalone node-track body and validate its version and channels.
    /// The same body is embedded in the cooked clip's node variants.
    pub fn from_payload(bytes: &[u8]) -> Result<Self, String> {
        let (body, rest): (Self, _) = postcard::take_from_bytes(bytes).map_err(|e| e.to_string())?;
        if !rest.is_empty() {
            return Err("trailing bytes in node-track body".into());
        }
        body.validate()?;
        Ok(body)
    }

    /// Encode a validated node-track body; unknown versions and malformed channels are errors.
    pub fn to_payload(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        postcard::to_allocvec(self).map_err(|e| e.to_string())
    }

    /// Validate a body before playback or cooking.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != NODE_TRACKS_VERSION {
            return Err(format!("unsupported node-track version {}", self.version));
        }
        if !self.frame.is_finite()
            || !self.start.is_finite()
            || !self.end.is_finite()
            || self.start < 0.0
            || self.end < self.start
        {
            return Err("invalid node animation frame or time range".into());
        }
        let mut identities = std::collections::BTreeSet::new();
        for (slot, node) in self.nodes.iter().enumerate() {
            if node.parent.is_some_and(|parent| parent as usize >= slot)
                || !identities.insert(node.node.index)
            {
                return Err("node hierarchy needs unique identities and parent-before-child order".into());
            }
            if !node.bind.translation.is_finite()
                || !node.bind.scale.is_finite()
                || !node.bind.rotation.is_finite()
                || (node.bind.rotation.length_squared() - 1.0).abs() > 0.001
                || node.matrix.is_some_and(|m| !m.is_finite() || node.animated())
            {
                return Err("invalid node bind transform or animated matrix node".into());
            }
            if let Some(curve) = &node.translations {
                curve.validate()?;
            }
            if let Some(curve) = &node.rotations {
                curve.validate()?;
            }
            if let Some(curve) = &node.scales {
                curve.validate()?;
            }
        }
        Ok(())
    }

    /// Sample in linear hierarchy time, reusing `out`'s allocation. Time clamps to this slice.
    /// Invalid hierarchy or non-finite composed transforms return an error and clear `out`.
    pub fn sample(&self, time: f32, out: &mut Vec<NodeSample>) -> Result<(), String> {
        let _span = tracing::trace_span!(target: "regia_animation", "sample_node_tracks").entered();
        out.clear();
        if self.version != NODE_TRACKS_VERSION
            || !self.start.is_finite()
            || !self.end.is_finite()
            || self.end < self.start
        {
            return Err("invalid node animation version or time range".into());
        }
        let time = if time.is_nan() { 0.0 } else { time };
        let time = self.start + time.clamp(0.0, self.end - self.start);
        for node in &self.nodes {
            let (parent, bind_parent) = match node.parent {
                Some(parent) => match out.get(parent as usize) {
                    Some(parent) => (parent.world, parent.bind_world),
                    None => {
                        out.clear();
                        return Err("invalid node parent slot".into());
                    }
                },
                None => (self.frame, self.frame),
            };
            let local = node.local(Some(time));
            let world = parent * local;
            let bind_world = bind_parent * node.local(None);
            if !world.is_finite() || !bind_world.is_finite() {
                out.clear();
                return Err("non-finite composed node transform".into());
            }
            let determinant = bind_world.determinant();
            let delta = if determinant.is_finite() && determinant != 0.0 {
                let delta = world * bind_world.inverse();
                delta.is_finite().then_some(delta)
            } else {
                None
            };
            out.push(NodeSample {
                index: node.node.index,
                is_joint: node.is_joint,
                local,
                world,
                bind_world,
                delta,
            });
        }
        Ok(())
    }

    pub(crate) fn slice(&self, start: f32, end: f32) -> Self {
        let mut sliced = self.clone();
        // A host clip may outlast this body's retained interval. Preserve its held pose.
        sliced.start = (self.start + start).min(self.end);
        sliced.end = (self.start + end).min(self.end);
        sliced
    }
}

/// Serde shape for format registration (`node-tracks` v1).
pub fn node_tracks_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<NodeAnimation>(),
        serde_field_names::<NodeTrack>(),
        serde_field_names::<NodeCurve<f32>>(),
        serde_variant_names::<NodeInterpolation>(),
        serde_field_names::<LocalBind>(),
        serde_field_names::<MorphNode>(),
    ])
}
