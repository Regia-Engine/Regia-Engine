// SPDX-License-Identifier: MPL-2.0
//! Morph weights end to end (Tincal engine item C2, `docs/specs/17-animation.md` § 4,
//! ADR-243): clips and graphs → [`MorphWeights`] → the renderer (`regia_render::morph`, B1).
//!
//! Every frame a rig's weights are composed in this order:
//!
//! 1. **Base**: the animator's morph tracks (clips and graphs; blended by node weight), or zero
//!    without an [`crate::Animator`].
//! 2. **Layers** ([`MorphLayers::layers`], in order): an [`MorphLayerMode::Override`] layer moves
//!    each of its targets toward its value by the layer weight; an [`MorphLayerMode::Additive`]
//!    layer adds `value × weight` (expressions on top of a viseme track). A layer's weight can
//!    follow a float graph parameter ([`MorphLayer::weight_param`]).
//! 3. **Clamp** every weight to [`MorphLayers::clamp`] (default `0..=1`). The clamp applies to
//!    this layer composition only.
//! 4. **Normalize** each [`MorphSet`]: when its weights sum past 1, they are scaled down to sum
//!    to 1 (one mouth shape at a time for viseme sets).
//!
//! `apply_animation` then writes gaze morph channels (they replace those indices) and runs rig
//! drivers (`Add`, `Set`, or `Multiply`). Those steps do not clamp or normalize again, so a
//! stored [`MorphWeights`] entry can sit outside the layer clamp. On an animator,
//! [`apply_morph_layers`] does not run.
//!
//! Targets are named by index or by name; names resolve through the entity's
//! [`MorphTargetNames`]. With [`MorphLayers`], [`MorphWeights`] is an output, rewritten every
//! frame from that chain; without layers, an animated rig's [`MorphWeights`] are its
//! animator's (still after gaze and drivers), and a rig with neither keeps whatever its
//! [`MorphWeights`] holds (set by game code or the agent).
//!
//! The composition rules are Regia Engine-original (`docs/provenance.md`).

use std::sync::Arc;

use regia_app::Time;
use regia_ecs::prelude::*;

use regia_scene::components::MAX_MORPH_TARGETS;
pub use regia_scene::components::{MorphTargetNames, MorphWeights};

use crate::clip::AnimationClip;
use crate::params::ParameterSet;

/// A morph target, by index or by name ([`MorphTargetNames`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MorphTarget {
    /// Target index in the mesh.
    Index(u32),
    /// Target name (glTF `extras.targetNames`).
    Name(String),
}

impl MorphTarget {
    /// The target index: an index as is, a name through `names` (`None` when unknown).
    #[must_use]
    pub fn resolve(&self, names: Option<&MorphTargetNames>) -> Option<usize> {
        match self {
            Self::Index(index) => Some(*index as usize),
            Self::Name(name) => names?.index_of(name),
        }
    }
}

impl From<u32> for MorphTarget {
    fn from(index: u32) -> Self {
        Self::Index(index)
    }
}

impl From<&str> for MorphTarget {
    fn from(name: &str) -> Self {
        Self::Name(name.to_string())
    }
}

impl From<String> for MorphTarget {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

/// How a [`MorphLayer`] combines with the weights below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MorphLayerMode {
    /// `w += (value − w) × layer weight`: at weight 1 the layer's value replaces the weight.
    #[default]
    Override,
    /// `w += value × layer weight`.
    Additive,
}

impl MorphLayerMode {
    /// `"override"` or `"additive"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Additive => "additive",
        }
    }
}

/// One expression layer: target values applied over the weights below it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MorphLayer {
    /// Layer name (how the agent and game code address it).
    pub name: String,
    /// How it combines.
    pub mode: MorphLayerMode,
    /// Layer weight, 0–1.
    pub weight: f32,
    /// A float graph parameter that multiplies [`Self::weight`] (an animated rig only; read as 1
    /// without an animator).
    pub weight_param: Option<String>,
    /// Target values.
    pub values: Vec<(MorphTarget, f32)>,
}

impl MorphLayer {
    /// A layer named `name` of `mode` at full weight with no values.
    #[must_use]
    pub fn new(name: impl Into<String>, mode: MorphLayerMode) -> Self {
        Self {
            name: name.into(),
            mode,
            weight: 1.0,
            weight_param: None,
            values: Vec::new(),
        }
    }

    /// With `target` at `value` (replacing an earlier value for the same target).
    #[must_use]
    pub fn with(mut self, target: impl Into<MorphTarget>, value: f32) -> Self {
        self.set(target, value);
        self
    }

    /// Sets `target` to `value` (replacing an earlier value for the same target).
    pub fn set(&mut self, target: impl Into<MorphTarget>, value: f32) {
        let target = target.into();
        match self.values.iter_mut().find(|(t, _)| *t == target) {
            Some(slot) => slot.1 = value,
            None => self.values.push((target, value)),
        }
    }
}

/// Targets whose weights are normalized together: when they sum past 1 they are scaled down to
/// sum to 1 (viseme sets).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MorphSet {
    /// Set name.
    pub name: String,
    /// Its targets.
    pub targets: Vec<MorphTarget>,
}

/// Expression layers, clamps and normalized sets over a rig's morph weights (see the module
/// docs). With this component, [`MorphWeights`] is rewritten every frame.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct MorphLayers {
    /// Layers, applied in order.
    pub layers: Vec<MorphLayer>,
    /// Every composed weight is clamped to `[min, max]`; `None` leaves them as they are.
    pub clamp: Option<[f32; 2]>,
    /// Sets normalized after the clamp.
    pub normalized_sets: Vec<MorphSet>,
}

impl Default for MorphLayers {
    fn default() -> Self {
        Self {
            layers: Vec::new(),
            clamp: Some([0.0, 1.0]),
            normalized_sets: Vec::new(),
        }
    }
}

impl MorphLayers {
    /// The layer named `name`.
    #[must_use]
    pub fn layer(&self, name: &str) -> Option<&MorphLayer> {
        self.layers.iter().find(|l| l.name == name)
    }

    /// The layer named `name`, appended (empty, `mode`, weight 1) when missing.
    pub fn layer_mut(&mut self, name: &str, mode: MorphLayerMode) -> &mut MorphLayer {
        let index = match self.layers.iter().position(|l| l.name == name) {
            Some(index) => index,
            None => {
                self.layers.push(MorphLayer::new(name, mode));
                self.layers.len() - 1
            }
        };
        &mut self.layers[index]
    }

    /// Composes the weights: `base` (the animator's, or empty), then the layers, the clamp and
    /// the normalized sets. `params` supplies [`MorphLayer::weight_param`] values. The result
    /// covers every target named by `base`, `names` or a layer; targets that do not resolve are
    /// skipped.
    #[must_use]
    pub fn compose(
        &self,
        base: &[f32],
        names: Option<&MorphTargetNames>,
        params: Option<&ParameterSet>,
    ) -> Vec<f32> {
        // A non-finite base weight reads as zero (it must not poison the layers above it).
        let mut weights: Vec<f32> = base.iter().map(|w| finite(*w)).collect();
        if let Some(names) = names
            && weights.len() < names.len()
        {
            weights.resize(names.len(), 0.0);
        }
        for layer in &self.layers {
            let scale = match (&layer.weight_param, params) {
                (Some(param), Some(params)) => params.float(param),
                _ => 1.0,
            };
            let amount = finite(layer.weight * scale).clamp(0.0, 1.0);
            if amount == 0.0 {
                continue;
            }
            for (target, value) in &layer.values {
                let Some(index) = target.resolve(names) else {
                    continue;
                };
                if index >= weights.len() {
                    weights.resize(index + 1, 0.0);
                }
                let value = finite(*value);
                let w = &mut weights[index];
                match layer.mode {
                    MorphLayerMode::Override => *w += (value - *w) * amount,
                    MorphLayerMode::Additive => *w += value * amount,
                }
            }
        }
        for w in &mut weights {
            *w = finite(*w);
            if let Some([min, max]) = self.clamp {
                *w = w.clamp(min.min(max), max.max(min));
            }
        }
        for set in &self.normalized_sets {
            let indices: Vec<usize> = set
                .targets
                .iter()
                .filter_map(|t| t.resolve(names))
                .filter(|&i| i < weights.len())
                .collect();
            let sum: f32 = indices.iter().map(|&i| weights[i].max(0.0)).sum();
            if sum > 1.0 {
                for &i in &indices {
                    if weights[i] > 0.0 {
                        weights[i] /= sum;
                    }
                }
            }
        }
        weights
    }
}

fn finite(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// Dense weights from an animator's sparse `(target, weight)` pairs.
///
/// A target at or past `MAX_MORPH_TARGETS` (a corrupt clip) is dropped, as
/// [`MorphWeights::set`] drops it, so a stray index cannot ask for a huge allocation.
pub(crate) fn dense_weights(sparse: &[(u32, f32)]) -> Vec<f32> {
    let mut weights = Vec::<f32>::new();
    for &(target, value) in sparse {
        let target = target as usize;
        if target >= MAX_MORPH_TARGETS {
            continue;
        }
        if target >= weights.len() {
            weights.resize(target + 1, 0.0);
        }
        weights[target] = value;
    }
    weights
}

/// What [`apply_morph_layers`] reads from a rig.
type LayeredRig = (
    Entity,
    &'static MorphLayers,
    Option<&'static MorphTargetNames>,
    Option<&'static mut MorphWeights>,
);

/// `PostUpdate/AnimationApply`: composes the [`MorphLayers`] of rigs without an animator (an
/// animated rig's layers are composed by [`crate::systems::apply_animation`]).
#[tracing::instrument(skip_all)]
pub fn apply_morph_layers(mut commands: Commands, rigs: Query<LayeredRig, Without<crate::Animator>>) {
    let _span = tracing::debug_span!("apply_morph_layers").entered();
    for (entity, layers, names, weights) in rigs {
        let composed = layers.compose(&[], names, None);
        match weights {
            Some(mut weights) => {
                if weights.0 != composed {
                    weights.0 = composed;
                }
            }
            None => {
                commands.entity(entity).insert(MorphWeights(composed));
            }
        }
    }
}

/// Marks an entity as the mesh of one node of the glTF file it was spawned from
/// (`spawn_gltf_hierarchy` inserts it on every entity that carries the node's morph weights).
/// Per-node morph tracks ([`crate::clip::MorphNode`]) drive the entities whose `index` matches
/// (ADR-297).
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct SourceNode {
    /// Index of the node in the source file's node list.
    pub index: u32,
    /// The node's name in the source file (empty when it had none).
    pub name: String,
}

/// Plays a clip's per-node morph tracks on the node entities below (or on) this entity, for
/// models without a skeleton (props, faces on rigid meshes). A skinned rig uses its
/// [`crate::Animator`] instead; the same per-node tracks work through it.
#[derive(Component, Clone, Debug)]
pub struct NodeMorphPlayer {
    /// The clip; only its per-node morph tracks are read.
    pub clip: Arc<AnimationClip>,
    /// Playback time in seconds.
    pub time: f32,
    /// Speed multiplier.
    pub speed: f32,
    /// Loop playback (otherwise the clip holds its last values).
    pub looping: bool,
    /// Whether time advances.
    pub playing: bool,
    // (runtime) last sample: (node index, target, weight).
    sampled: Vec<(u32, u32, f32)>,
}

impl NodeMorphPlayer {
    /// A playing player at time zero and speed one.
    #[must_use]
    pub fn new(clip: Arc<AnimationClip>, looping: bool) -> Self {
        let mut sampled = Vec::new();
        clip.sample_node_morphs(0.0, &mut sampled);
        Self {
            clip,
            time: 0.0,
            speed: 1.0,
            looping,
            playing: true,
            sampled,
        }
    }

    /// The last sampled `(node index, target, weight)` triples.
    #[must_use]
    pub fn sampled(&self) -> &[(u32, u32, f32)] {
        &self.sampled
    }
}

/// `Update`: advances every [`NodeMorphPlayer`] and samples its per-node morph tracks.
#[tracing::instrument(skip_all)]
pub fn advance_node_morph_players(
    time: Res<Time>,
    mut players: Query<&mut NodeMorphPlayer, Without<regia_scene::layers::LayerParked>>,
) {
    let _span = tracing::debug_span!("advance_node_morph_players").entered();
    let delta = time.delta_secs();
    for mut player in &mut players {
        let NodeMorphPlayer {
            clip,
            time,
            speed,
            looping,
            playing,
            sampled,
        } = &mut *player;
        if *playing {
            let duration = clip.duration;
            let next = *time + delta * *speed;
            *time = if !(duration.is_finite() && duration > 0.0) {
                0.0
            } else if *looping {
                next.rem_euclid(duration)
            } else {
                next.clamp(0.0, duration)
            };
        }
        clip.sample_node_morphs(*time, sampled);
    }
}

/// What [`apply_node_morphs`] writes to.
type NodeMorphMesh = (
    Entity,
    &'static SourceNode,
    &'static mut MorphWeights,
    Option<&'static MorphLayers>,
    Option<&'static MorphTargetNames>,
);

/// Sampled node-morph tracks plus the animator parameters (if any) that drive a mesh.
type MorphDriver<'a> = (&'a [(u32, u32, f32)], Option<&'a ParameterSet>);

/// `PostUpdate/AnimationApply`: writes each per-node morph track's weights to the
/// [`MorphWeights`] of the entity spawned for its node. The driver is the nearest [`NodeMorphPlayer`]
/// or [`crate::Animator`] on the entity or an ancestor; the entity's [`MorphLayers`] compose over the
/// result like they do over an animator's own weights. An entity no track names keeps its weights.
#[tracing::instrument(skip_all)]
pub fn apply_node_morphs(
    mut meshes: Query<NodeMorphMesh>,
    players: Query<&NodeMorphPlayer>,
    animators: Query<&crate::Animator>,
    parents: Query<&ChildOf>,
) {
    let _span = tracing::debug_span!("apply_node_morphs").entered();
    for (entity, node, mut weights, layers, names) in &mut meshes {
        let mut cursor = Some(entity);
        let mut driver: Option<MorphDriver> = None;
        let mut hops = 0usize;
        while let Some(current) = cursor {
            if let Ok(player) = players.get(current) {
                driver = Some((player.sampled(), None));
                break;
            }
            if let Ok(animator) = animators.get(current) {
                driver = Some((&animator.last_node_morphs, Some(&animator.parameters)));
                break;
            }
            hops += 1;
            if hops > 1024 {
                break;
            }
            cursor = parents.get(current).ok().map(ChildOf::parent);
        }
        let Some((samples, params)) = driver else {
            continue;
        };
        // A mesh no sample names keeps its weights and allocates nothing. A matched mesh
        // is written in place; only a layered compose (which returns a new vector) allocates.
        let mut matched = false;
        let mut needed = weights.0.len();
        for &(sample_node, target, _) in samples {
            if sample_node != node.index || target as usize >= MAX_MORPH_TARGETS {
                continue;
            }
            matched = true;
            needed = needed.max(target as usize + 1);
        }
        if !matched {
            continue;
        }
        let Some(layers) = layers else {
            if weights.0.len() < needed {
                crate::hot_count::count_node_morph_dense();
                weights.0.resize(needed, 0.0);
            }
            weights.0.fill(0.0);
            for &(sample_node, target, value) in samples {
                if sample_node == node.index && (target as usize) < MAX_MORPH_TARGETS {
                    weights.0[target as usize] = value;
                }
            }
            continue;
        };
        crate::hot_count::count_node_morph_dense();
        let mut dense = vec![0.0f32; needed];
        for &(sample_node, target, value) in samples {
            if sample_node == node.index && (target as usize) < MAX_MORPH_TARGETS {
                dense[target as usize] = value;
            }
        }
        let composed = layers.compose(&dense, names, params);
        if weights.0 != composed {
            weights.0 = composed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> MorphTargetNames {
        MorphTargetNames::new(["aa", "oh", "ee", "smile", "blink"])
    }

    #[test]
    fn layers_override_then_add_then_clamp() {
        let layers = MorphLayers {
            layers: vec![
                MorphLayer::new("talk", MorphLayerMode::Override).with("aa", 0.8),
                MorphLayer {
                    weight: 0.5,
                    ..MorphLayer::new("happy", MorphLayerMode::Additive)
                        .with("smile", 1.0)
                        .with(0u32, 0.6)
                },
            ],
            ..MorphLayers::default()
        };
        let base = [0.2, 0.0, 0.0, 0.1];
        let w = layers.compose(&base, Some(&names()), None);
        assert_eq!(w.len(), 5, "covers every named target");
        // aa: override to 0.8, then +0.6 × 0.5 = 1.1 → clamped to 1.
        assert_eq!(w[0], 1.0);
        // smile: 0.1 + 0.5.
        assert!((w[3] - 0.6).abs() < 1e-6);
        assert_eq!(w[4], 0.0);
    }

    #[test]
    fn clamp_is_layer_composition_only_and_a_driver_can_leave_it() {
        use crate::curve::{Curve, CurveKey, CurveKeyKind};
        use crate::driver::{Driver, DriverBlend, DriverInput, DriverOutput, DriverSet, RigState};
        use crate::params::ParameterSet;
        use crate::skeleton::{LocalBind, Skeleton};

        let layers = MorphLayers {
            layers: vec![MorphLayer::new("add", MorphLayerMode::Additive).with("aa", 1.0)],
            ..MorphLayers::default()
        };
        let composed = layers.compose(&[0.0], Some(&names()), None);
        assert_eq!(composed[0], 1.0, "the default clamp holds an additive 1");

        let skeleton = Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
            .expect("skeleton");
        let morph_names = ["aa", "oh", "ee", "smile", "blink"].map(str::to_string).to_vec();
        let set = DriverSet {
            drivers: vec![Driver {
                name: "past".into(),
                input: DriverInput::Param("unused".into()),
                curve: Curve::from(vec![
                    CurveKey::new(0.0, 0.5, CurveKeyKind::Linear),
                    CurveKey::new(1.0, 0.5, CurveKeyKind::Linear),
                ]),
                output: DriverOutput::Morph("aa".into()),
                blend: DriverBlend::Add,
                weight: 1.0,
            }],
            ..DriverSet::default()
        };
        let mut weights = composed;
        let mut pose = skeleton.bind_pose_pose();
        set.compile(&skeleton, &morph_names).expect("compiles").evaluate(
            &mut pose,
            &ParameterSet::new(),
            &mut weights,
            &mut RigState::default(),
        );
        assert!(
            (weights[0] - 1.5).abs() < 1e-5,
            "drivers run after the clamp and do not clamp again: {weights:?}"
        );
    }

    #[test]
    fn partial_override_blends_and_unknown_names_are_skipped() {
        let layers = MorphLayers {
            layers: vec![MorphLayer {
                weight: 0.25,
                ..MorphLayer::new("blink", MorphLayerMode::Override)
                    .with("blink", 1.0)
                    .with("nope", 1.0)
            }],
            ..MorphLayers::default()
        };
        let w = layers.compose(&[0.0; 5], Some(&names()), None);
        assert!((w[4] - 0.25).abs() < 1e-6);
        assert_eq!(w.len(), 5);
    }

    #[test]
    fn viseme_sets_normalize_only_past_one() {
        let set = MorphSet {
            name: "visemes".into(),
            targets: vec!["aa".into(), "oh".into(), "ee".into()],
        };
        let layers = MorphLayers {
            normalized_sets: vec![set],
            ..MorphLayers::default()
        };
        let w = layers.compose(&[0.6, 0.6, 0.3, 0.9], Some(&names()), None);
        let sum = w[0] + w[1] + w[2];
        assert!((sum - 1.0).abs() < 1e-6, "{w:?}");
        assert!((w[0] - 0.4).abs() < 1e-6);
        assert_eq!(w[3], 0.9, "outside the set");
        let under = layers.compose(&[0.2, 0.3, 0.0], Some(&names()), None);
        assert_eq!(&under[..3], &[0.2, 0.3, 0.0], "under 1: unchanged");
    }

    #[test]
    fn a_parameter_drives_a_layer_weight() {
        let mut params = ParameterSet::new();
        params.set_float("Smile", 0.5);
        let layers = MorphLayers {
            layers: vec![MorphLayer {
                weight_param: Some("Smile".into()),
                ..MorphLayer::new("smile", MorphLayerMode::Override).with(3u32, 1.0)
            }],
            ..MorphLayers::default()
        };
        assert!((layers.compose(&[], None, Some(&params))[3] - 0.5).abs() < 1e-6);
        assert_eq!(
            layers.compose(&[], None, None)[3],
            1.0,
            "no animator: full weight"
        );
        let unclamped = MorphLayers {
            clamp: None,
            layers: vec![MorphLayer::new("x", MorphLayerMode::Additive).with(0u32, 3.0)],
            ..MorphLayers::default()
        };
        assert_eq!(unclamped.compose(&[f32::NAN], None, None), vec![3.0]);
    }

    #[test]
    fn dense_weights_fill_gaps_with_zero() {
        assert_eq!(dense_weights(&[(2, 0.5), (0, 1.0)]), vec![1.0, 0.0, 0.5]);
    }

    /// Audit (corr-animation): a clip's morph target index was never capped, so a corrupt
    /// `u32::MAX` resized the weights to 16 GiB and aborted.
    #[test]
    fn dense_weights_drop_targets_past_the_cap() {
        let weights = dense_weights(&[(1, 0.5), (MAX_MORPH_TARGETS as u32, 1.0), (u32::MAX, 1.0)]);
        assert_eq!(weights, vec![0.0, 0.5]);
    }

    /// Audit (corr-animation): the per-node path sized a mesh's weights from an uncapped clip
    /// target index.
    #[test]
    fn a_node_morph_target_past_the_cap_does_not_grow_the_weights() {
        use crate::clip::{MorphNode, MorphTrack, TrackCurve};
        let node = MorphNode {
            index: 0,
            name: String::new(),
        };
        let mut clip = crate::clip::AnimationClip::empty("face", 1.0);
        clip.morph_tracks = vec![
            MorphTrack::for_node(node.clone(), 1, TrackCurve::Static(0.5)),
            MorphTrack::for_node(node, MAX_MORPH_TARGETS as u32 + 7, TrackCurve::Static(1.0)),
        ];
        let mut app = regia_app::App::new();
        app.init_resource::<Time>();
        app.add_systems(
            regia_app::Update,
            (advance_node_morph_players, apply_node_morphs).chain(),
        );
        let e = app
            .world_mut()
            .spawn((
                NodeMorphPlayer::new(Arc::new(clip), false),
                SourceNode {
                    index: 0,
                    name: String::new(),
                },
                MorphWeights::default(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<Time>()
            .advance(std::time::Duration::from_millis(16));
        app.update();
        let weights = app.world().get::<MorphWeights>(e).map(|w| w.0.clone());
        assert_eq!(weights, Some(vec![0.0, 0.5]));
    }

    /// A cooked clip with a NaN duration must not reach `clamp` with a NaN bound.
    #[test]
    fn a_nan_duration_clip_does_not_panic_the_node_morph_player() {
        let mut clip = crate::clip::AnimationClip::empty("nan", 1.0);
        clip.duration = f32::NAN;
        let mut app = regia_app::App::new();
        app.init_resource::<Time>();
        app.add_systems(regia_app::Update, advance_node_morph_players);
        let e = app
            .world_mut()
            .spawn(NodeMorphPlayer::new(Arc::new(clip), false))
            .id();
        app.world_mut()
            .resource_mut::<Time>()
            .advance(std::time::Duration::from_millis(100));
        app.update();
        let t = app.world().get::<NodeMorphPlayer>(e).map(|p| p.time);
        assert_eq!(t, Some(0.0));
    }
}
