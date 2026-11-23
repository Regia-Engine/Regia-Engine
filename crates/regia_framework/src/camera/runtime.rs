// SPDX-License-Identifier: MPL-2.0
//! The driver core: rig instances, transitions, layers and actions
//! (`docs/specs/35-camera-framework.md` §§ 6-8). Pure of the ECS: the driver component and the
//! system in [`driver`](super::driver) feed it a clock and a [`CameraWorld`].

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_core::math::{Quat, Vec3, det_cos, det_nlerp, det_yaw_pitch_roll};

use super::asset::{
    Blend, CameraAsset, CameraError, Condition, ParamValue, RigMatch, StartFacing, TargetRef, TransitionDef,
    VarValue, same_structure,
};
use super::director::{CameraHooks, DirectorContext, evaluate};
use super::nodes::{
    CameraPose, CameraWorld, CondCtx, EvalCtx, InputBank, NodeState, RigScratch, VarTable, combine,
    eval_condition, eval_rig, wrap_pi,
};

/// Blend-out time of a released layer whose rig does not set `blend_out` (seconds).
pub const DEFAULT_BLEND_OUT: f32 = 0.2;
/// The `order` director-requested extra rigs take as additive layers.
const DIRECTOR_LAYER_ORDER: i32 = 1_000_000;

/// Identifies a layer of one driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(pub u32);

/// Identifies a `LookAt` action of one driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(pub u32);

/// What happened to a layer (§ 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerEventKind {
    /// The layer was pushed or restarted.
    Entered,
    /// The blend-in finished.
    Settled,
    /// The layer started blending out.
    Leaving,
    /// The layer was removed.
    Gone,
    /// The layer switched in with no interpolation.
    Cut,
}

impl LayerEventKind {
    /// Lowercase name for the protocol.
    pub fn name(self) -> &'static str {
        match self {
            LayerEventKind::Entered => "entered",
            LayerEventKind::Settled => "settled",
            LayerEventKind::Leaving => "leaving",
            LayerEventKind::Gone => "gone",
            LayerEventKind::Cut => "cut",
        }
    }
}

/// One layer event in evaluation order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerEventRecord {
    /// The layer.
    pub id: LayerId,
    /// Its rig name.
    pub rig: String,
    /// What happened.
    pub kind: LayerEventKind,
}

/// Options of [`DriverCore::push_overlay`].
#[derive(Clone, Debug, Default)]
pub struct OverlayOptions {
    /// The blend-in (a missing transition is a cut).
    pub transition: Option<TransitionDef>,
    /// A target the layer's `DriveTarget` resolves to instead of the driver's.
    pub context: Option<TargetRef>,
    /// Always start a new layer even if the same rig is already an overlay.
    pub always_new: bool,
}

/// What [`DriverCore::release`] matches.
#[derive(Clone, Debug, PartialEq)]
pub enum ReleaseBy {
    /// One layer by id.
    Id(LayerId),
    /// Every layer of a rig with this context.
    Rig(String, Option<TargetRef>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Entering,
    Settled,
    Leaving,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Entering => "entering",
            Phase::Settled => "settled",
            Phase::Leaving => "leaving",
        }
    }
}

struct LookAtAction {
    id: ActionId,
    target: TargetRef,
    tolerance: f32,
    hold: bool,
}

struct Instance {
    rig: usize,
    /// Seconds this instance has run; f64 so it keeps advancing in a long session
    /// (audit-v035-numerics L5), cast to f32 at the node and inspector surface.
    elapsed: f64,
    weight: f32,
    start_weight: f32,
    pose: CameraPose,
    scratch: RigScratch,
    frozen: Vec<VarValue>,
    actions: Vec<LookAtAction>,
}

impl Instance {
    fn new(rig: usize, pose: CameraPose, frozen: Vec<VarValue>) -> Self {
        Self {
            rig,
            elapsed: 0.0,
            weight: 0.0,
            start_weight: 0.0,
            pose,
            scratch: RigScratch::default(),
            frozen,
            actions: Vec::new(),
        }
    }
}

struct ActiveBlend {
    transition: TransitionDef,
    elapsed: f32,
    incoming: usize,
    outgoing: Option<usize>,
}

struct Layer {
    id: LayerId,
    rig: usize,
    overlay: bool,
    order: i32,
    context: Option<TargetRef>,
    from_director: bool,
    blend_in: Blend,
    blend_out: Blend,
    in_elapsed: f32,
    out_elapsed: f32,
    out_start: f32,
    phase: Phase,
    weight: f32,
    elapsed: f32,
    held: bool,
    seeded: bool,
    pose: CameraPose,
    scratch: RigScratch,
    frozen: Vec<VarValue>,
}

/// A world that resolves `DriveTarget` to another target (a layer's context).
struct ContextWorld<'a> {
    inner: &'a dyn CameraWorld,
    context: &'a TargetRef,
}

impl CameraWorld for ContextWorld<'_> {
    fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)> {
        match target {
            TargetRef::DriveTarget => self.inner.target(self.context),
            other => self.inner.target(other),
        }
    }

    fn cast_sphere(&self, from: Vec3, dir: Vec3, max: f32, radius: f32, layers: &[String]) -> Option<f32> {
        self.inner.cast_sphere(from, dir, max, radius, layers)
    }
}

/// A snapshot row for the inspector: one active rig.
#[derive(Clone, Debug)]
pub struct RigInfo {
    /// Rig name.
    pub name: String,
    /// Current weight.
    pub weight: f32,
    /// Seconds the instance has run.
    pub elapsed: f32,
    /// Nodes skipped so far.
    pub skipped: u32,
    /// The node-by-node trace of the last evaluation.
    pub trace: Vec<super::nodes::TraceEntry>,
    /// The instance's last pose.
    pub pose: CameraPose,
    /// The pivot point of the last evaluation.
    pub pivot: Option<Vec3>,
    /// The collision probe of the last evaluation.
    pub probe: Option<super::nodes::Probe>,
    /// What the `Rail` nodes reported in the last evaluation.
    pub rails: Vec<super::rail::RailReport>,
    /// What the `Crane` nodes reported in the last evaluation.
    pub cranes: Vec<super::crane::CraneReport>,
}

/// A snapshot row for the inspector: one layer.
#[derive(Clone, Debug)]
pub struct LayerInfo {
    /// Layer id.
    pub id: LayerId,
    /// Rig name.
    pub rig: String,
    /// `overlay` or `additive`.
    pub kind: &'static str,
    /// Additive order (0 for overlays).
    pub order: i32,
    /// `entering`, `settled` or `leaving`.
    pub phase: &'static str,
    /// Current weight.
    pub weight: f32,
    /// The layer is held.
    pub held: bool,
}

/// The evaluation state of one driver.
pub struct DriverCore {
    asset: Arc<CameraAsset>,
    /// Runtime variables.
    pub vars: VarTable,
    /// Input slots.
    pub inputs: InputBank,
    overrides: BTreeMap<String, ParamValue>,
    seed: u64,
    local_player: u8,
    instances: Vec<Instance>,
    primary: Option<usize>,
    blend: Option<ActiveBlend>,
    memory: Vec<Option<CameraPose>>,
    layers: Vec<Layer>,
    next_layer: u32,
    next_action: u32,
    pose: Option<CameraPose>,
    /// Seconds simulated so far; f64 for the same reason as `Instance::elapsed`.
    time: f64,
    cut_pending: bool,
    view: VarTable,
    below: Vec<(CameraPose, f32)>,
    /// Events of the last step, in evaluation order. The system drains it.
    pub events: Vec<LayerEventRecord>,
    /// The last [`EVENT_LOG`] events, oldest first, for the inspector.
    pub event_log: Vec<LayerEventRecord>,
    warned_share: bool,
    /// The camera entity's bits for the `camera_driver{entity}` span (spec 35 § 11); 0 when unset.
    entity_bits: u64,
}

/// How many past events the inspector keeps.
pub const EVENT_LOG: usize = 16;

impl DriverCore {
    /// A core for `asset`. The asset must have no validation errors (the registry guarantees it).
    pub fn new(asset: Arc<CameraAsset>, seed: u64, local_player: u8) -> Self {
        let vars = VarTable::from_asset(&asset);
        let inputs = InputBank::from_asset(&asset);
        let rigs = asset.rigs.len();
        Self {
            vars,
            inputs,
            overrides: BTreeMap::new(),
            seed,
            local_player,
            instances: Vec::new(),
            primary: None,
            blend: None,
            memory: vec![None; rigs],
            layers: Vec::new(),
            next_layer: 1,
            next_action: 1,
            pose: None,
            time: 0.0,
            cut_pending: false,
            view: VarTable::default(),
            below: Vec::new(),
            events: Vec::new(),
            event_log: Vec::new(),
            warned_share: false,
            entity_bits: 0,
            asset,
        }
    }

    /// Names the camera entity this core drives (its `Entity::to_bits`), recorded as the
    /// `entity` field of the `camera_driver` span. The driver system sets it each frame.
    pub fn set_entity_bits(&mut self, bits: u64) {
        self.entity_bits = bits;
    }

    /// The asset.
    pub fn asset(&self) -> &Arc<CameraAsset> {
        &self.asset
    }

    /// The final pose of the last step.
    pub fn pose(&self) -> Option<CameraPose> {
        self.pose
    }

    /// The primary rig's name.
    pub fn primary_name(&self) -> Option<&str> {
        self.primary.map(|i| self.asset.rigs[i].name.as_str())
    }

    /// Seconds simulated so far.
    pub fn time(&self) -> f32 {
        self.time as f32
    }

    // --- parameters, variables, inputs -------------------------------------------------------

    /// Overrides a parameter by name; the type must match the asset default.
    pub fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), CameraError> {
        let Some(default) = self.asset.params.get(name) else {
            return Err(CameraError::UnknownName(name.to_owned()));
        };
        if !default.same_kind(&value) {
            return Err(CameraError::BadParam {
                name: name.to_owned(),
                message: format!("expected {}, got {}", default.kind(), value.kind()),
            });
        }
        if !value.is_finite() {
            return Err(CameraError::BadValue(format!("parameter `{name}` is not finite")));
        }
        self.overrides.insert(name.to_owned(), value);
        Ok(())
    }

    /// The effective value of a parameter (override, else default).
    pub fn param(&self, name: &str) -> Option<&ParamValue> {
        self.overrides.get(name).or_else(|| self.asset.params.get(name))
    }

    /// The parameter overrides.
    pub fn overrides(&self) -> &BTreeMap<String, ParamValue> {
        &self.overrides
    }

    /// Writes a variable.
    pub fn set_var(&mut self, name: &str, value: VarValue) -> Result<(), CameraError> {
        if self.vars.index(name).is_none() {
            return Err(CameraError::UnknownName(name.to_owned()));
        }
        if self.vars.set(name, value) {
            Ok(())
        } else {
            Err(CameraError::BadValue(format!(
                "variable `{name}` rejected a {} value (wrong type or not finite)",
                value.kind()
            )))
        }
    }

    /// Writes an input slot's raw value for the next step.
    pub fn set_input(&mut self, slot: &str, value: [f32; 2]) -> Result<(), CameraError> {
        if self.inputs.index(slot).is_none() {
            return Err(CameraError::UnknownName(slot.to_owned()));
        }
        if self.inputs.set_raw(slot, value) {
            Ok(())
        } else {
            Err(CameraError::BadValue(format!(
                "input `{slot}` value is not finite"
            )))
        }
    }

    /// Resets a slot's accumulated angles and releases its held raw value.
    pub fn reset_input(&mut self, slot: &str) -> Result<(), CameraError> {
        if self.inputs.reset(slot) {
            Ok(())
        } else {
            Err(CameraError::UnknownName(slot.to_owned()))
        }
    }

    // --- actions -----------------------------------------------------------------------------

    /// Starts a `LookAt` on the active instance of `rig` (§ 8). With `hold: false` it ends when
    /// the target is within `tolerance_deg` of the view direction; with `hold: true` it keeps
    /// framing until [`clear_look_at`](Self::clear_look_at).
    pub fn look_at(
        &mut self,
        rig: &str,
        target: TargetRef,
        tolerance_deg: f32,
        hold: bool,
    ) -> Result<ActionId, CameraError> {
        let index = self
            .asset
            .rig_index(rig)
            .ok_or_else(|| CameraError::UnknownRig(rig.to_owned()))?;
        if !tolerance_deg.is_finite() || tolerance_deg < 0.0 {
            return Err(CameraError::BadValue(
                "tolerance_deg must be finite and not negative".into(),
            ));
        }
        let id = ActionId(self.next_action);
        let Some(instance) = self.instances.iter_mut().find(|i| i.rig == index) else {
            return Err(CameraError::UnknownRig(format!("{rig} (not active)")));
        };
        self.next_action += 1;
        instance.actions.push(LookAtAction {
            id,
            target,
            tolerance: tolerance_deg.to_radians(),
            hold,
        });
        Ok(id)
    }

    /// Removes a `LookAt`; returns whether it was still active.
    pub fn clear_look_at(&mut self, id: ActionId) -> bool {
        let mut found = false;
        for instance in &mut self.instances {
            let before = instance.actions.len();
            instance.actions.retain(|a| a.id != id);
            found |= instance.actions.len() != before;
        }
        found
    }

    /// How many `LookAt` actions are active.
    pub fn action_count(&self) -> usize {
        self.instances.iter().map(|i| i.actions.len()).sum()
    }

    // --- layers ------------------------------------------------------------------------------

    fn emit(&mut self, id: LayerId, rig: usize, kind: LayerEventKind) {
        let record = LayerEventRecord {
            id,
            rig: self.asset.rigs[rig].name.clone(),
            kind,
        };
        self.events.push(record.clone());
        if self.event_log.len() >= EVENT_LOG {
            self.event_log.remove(0);
        }
        self.event_log.push(record);
    }

    fn new_layer(
        &mut self,
        rig: usize,
        overlay: bool,
        order: i32,
        context: Option<TargetRef>,
        blend_in: Blend,
    ) -> LayerId {
        let id = LayerId(self.next_layer);
        self.next_layer += 1;
        let cut = blend_in.duration() <= 0.0;
        self.layers.push(Layer {
            id,
            rig,
            overlay,
            order,
            context,
            from_director: false,
            blend_in,
            blend_out: Blend::Linear(DEFAULT_BLEND_OUT),
            in_elapsed: 0.0,
            out_elapsed: 0.0,
            out_start: 1.0,
            phase: if cut { Phase::Settled } else { Phase::Entering },
            weight: if cut { 1.0 } else { 0.0 },
            elapsed: 0.0,
            held: false,
            seeded: false,
            pose: CameraPose::default(),
            scratch: RigScratch::default(),
            frozen: self.vars.values.clone(),
        });
        self.emit(id, rig, LayerEventKind::Entered);
        if cut {
            self.emit(id, rig, LayerEventKind::Cut);
        }
        id
    }

    /// Pushes an overlay layer (§ 7). If `always_new` is false and the same rig is already an
    /// overlay with the same context, its blend-in restarts and its id is returned.
    pub fn push_overlay(&mut self, rig: &str, options: OverlayOptions) -> Result<LayerId, CameraError> {
        let index = self
            .asset
            .rig_index(rig)
            .ok_or_else(|| CameraError::UnknownRig(rig.to_owned()))?;
        let blend = options.transition.as_ref().map_or(Blend::Cut, |t| t.blend);
        if !options.always_new
            && let Some(existing) = self
                .layers
                .iter()
                .position(|l| l.overlay && !l.from_director && l.rig == index && l.context == options.context)
        {
            let frozen = self.vars.values.clone();
            let layer = &mut self.layers[existing];
            layer.blend_in = blend;
            layer.in_elapsed = 0.0;
            layer.held = false;
            layer.elapsed = 0.0;
            // A restart is a fresh run, as `new_layer` builds it: the previous run's smoothing
            // state, carried pose and frozen variables would otherwise ease the first frames
            // from stale motion (audit-gameplay-muse L8).
            layer.seeded = false;
            layer.pose = CameraPose::default();
            layer.scratch = RigScratch::default();
            layer.frozen = frozen;
            let cut = blend.duration() <= 0.0;
            layer.phase = if cut { Phase::Settled } else { Phase::Entering };
            layer.weight = if cut { 1.0 } else { 0.0 };
            let id = layer.id;
            self.emit(id, index, LayerEventKind::Entered);
            if cut {
                self.emit(id, index, LayerEventKind::Cut);
            }
            return Ok(id);
        }
        Ok(self.new_layer(index, true, 0, options.context, blend))
    }

    /// Adds an additive layer sorted by `order` (§ 7). A duplicate `order` is refused unless
    /// `allow_same_order`.
    pub fn add_additive(
        &mut self,
        rig: &str,
        order: i32,
        context: Option<TargetRef>,
        allow_same_order: bool,
    ) -> Result<LayerId, CameraError> {
        let index = self
            .asset
            .rig_index(rig)
            .ok_or_else(|| CameraError::UnknownRig(rig.to_owned()))?;
        if !allow_same_order
            && self
                .layers
                .iter()
                .any(|l| !l.overlay && !l.from_director && l.order == order && l.phase != Phase::Leaving)
        {
            return Err(CameraError::OrderTaken(order));
        }
        Ok(self.new_layer(
            index,
            false,
            order,
            context,
            Blend::Linear(0.2_f32.min(DEFAULT_BLEND_OUT)),
        ))
    }

    /// Releases layers; returns how many were released. An unknown id is an error; a rig that has
    /// no layer releases zero.
    pub fn release(&mut self, by: ReleaseBy, instant: bool) -> Result<usize, CameraError> {
        let matches: Vec<usize> = match &by {
            ReleaseBy::Id(id) => {
                let Some(i) = self.layers.iter().position(|l| l.id == *id) else {
                    return Err(CameraError::UnknownLayer(id.0));
                };
                vec![i]
            }
            ReleaseBy::Rig(name, context) => {
                let Some(rig) = self.asset.rig_index(name) else {
                    return Err(CameraError::UnknownRig(name.clone()));
                };
                self.layers
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.rig == rig && (context.is_none() || &l.context == context))
                    .map(|(i, _)| i)
                    .collect()
            }
        };
        let mut released = 0;
        let mut gone = Vec::new();
        for i in matches {
            let rig = self.layers[i].rig;
            let id = self.layers[i].id;
            if self.layers[i].phase == Phase::Leaving && !instant {
                continue;
            }
            released += 1;
            if instant {
                gone.push(i);
            } else {
                let seconds = self.asset.rigs[rig].blend_out.unwrap_or(DEFAULT_BLEND_OUT);
                let layer = &mut self.layers[i];
                layer.phase = Phase::Leaving;
                layer.out_start = layer.weight;
                layer.out_elapsed = 0.0;
                layer.blend_out = Blend::Linear(seconds);
                self.emit(id, rig, LayerEventKind::Leaving);
            }
        }
        for i in gone.into_iter().rev() {
            let layer = self.layers.remove(i);
            self.emit(layer.id, layer.rig, LayerEventKind::Gone);
        }
        Ok(released)
    }

    /// Stops a layer updating; its last pose is held.
    pub fn hold_overlay(&mut self, id: LayerId) -> Result<(), CameraError> {
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(layer) => {
                layer.held = true;
                Ok(())
            }
            None => Err(CameraError::UnknownLayer(id.0)),
        }
    }

    /// Whether a layer is still alive.
    pub fn has_layer(&self, id: LayerId) -> bool {
        self.layers.iter().any(|l| l.id == id)
    }

    /// Layer rows for the inspector, in application order (overlays, then additives by order).
    pub fn layer_infos(&self) -> Vec<LayerInfo> {
        let mut rows: Vec<&Layer> = self.layers.iter().collect();
        rows.sort_by_key(|l| (!l.overlay, if l.overlay { 0 } else { l.order }));
        rows.into_iter()
            .map(|l| LayerInfo {
                id: l.id,
                rig: self.asset.rigs[l.rig].name.clone(),
                kind: if l.overlay { "overlay" } else { "additive" },
                order: l.order,
                phase: l.phase.name(),
                weight: l.weight,
                held: l.held,
            })
            .collect()
    }

    /// Active rig rows for the inspector.
    pub fn rig_infos(&self) -> Vec<RigInfo> {
        self.instances
            .iter()
            .map(|i| RigInfo {
                name: self.asset.rigs[i.rig].name.clone(),
                weight: i.weight,
                elapsed: i.elapsed as f32,
                skipped: i.scratch.skipped,
                trace: i.scratch.trace.clone(),
                pose: i.pose,
                pivot: i.scratch.pivot,
                probe: i.scratch.probe,
                rails: i.scratch.rails.clone(),
                cranes: i.scratch.cranes.clone(),
            })
            .collect()
    }

    /// The running blend: `(from, to, elapsed, duration)`.
    pub fn blend_info(&self) -> Option<(Option<String>, String, f32, f32)> {
        self.blend.as_ref().map(|b| {
            (
                b.outgoing.map(|i| self.asset.rigs[i].name.clone()),
                self.asset.rigs[b.incoming].name.clone(),
                b.elapsed,
                b.transition.blend.duration(),
            )
        })
    }

    // --- transitions --------------------------------------------------------------------------

    /// Chooses the transition for a rig change (§ 6.2): the outgoing rig's own list, then the
    /// incoming rig's, then the asset's shared list; the first whose ends match and whose
    /// conditions all pass.
    pub fn select_transition(&self, from: usize, to: usize, elapsed: f32) -> Option<&TransitionDef> {
        let (out_rig, in_rig) = (&self.asset.rigs[from], &self.asset.rigs[to]);
        let cond = CondCtx {
            elapsed,
            vars: &self.vars,
            rig: in_rig,
        };
        let ends = |m: &RigMatch, rig: &super::asset::RigDef| match m {
            RigMatch::Any => true,
            RigMatch::Rig(name) => &rig.name == name,
            RigMatch::Tag(tag) => rig.tags.iter().any(|t| t == tag),
        };
        out_rig
            .transitions
            .iter()
            .chain(in_rig.transitions.iter())
            .chain(self.asset.transitions.iter())
            .find(|t| {
                ends(&t.from, out_rig)
                    && ends(&t.to, in_rig)
                    && t.conditions.iter().all(|c| eval_condition(c, &cond))
            })
    }

    fn instance_pos(&self, rig: usize) -> Option<usize> {
        self.instances.iter().position(|i| i.rig == rig)
    }

    fn seeded_pose(
        &self,
        rig: usize,
        facing: StartFacing,
        base: CameraPose,
        outgoing: Option<&CameraPose>,
        world: &dyn CameraWorld,
    ) -> CameraPose {
        let own = self.memory[rig].unwrap_or(base);
        match facing {
            StartFacing::OwnSeed => own,
            StartFacing::TargetFacing => {
                let mut pose = base;
                if let Some((_, rotation)) = world.target(&TargetRef::DriveTarget) {
                    let (yaw, pitch, _) = det_yaw_pitch_roll(rotation);
                    pose.yaw = yaw;
                    pose.pitch = pitch;
                    pose.roll = 0.0;
                }
                pose
            }
            StartFacing::CarryOver => {
                let mut pose = base;
                if let Some(out) = outgoing {
                    pose.yaw = out.yaw;
                    pose.pitch = out.pitch;
                    pose.roll = 0.0;
                }
                pose
            }
            StartFacing::ContinueAim => {
                let mut pose = base;
                match outgoing.and_then(|o| o.aim) {
                    Some(aim) => pose.look_at(aim),
                    None => return own,
                }
                pose
            }
        }
    }

    /// Makes `rig` the primary rig, choosing the transition from the current primary.
    fn activate(&mut self, rig: usize, base: CameraPose, world: &dyn CameraWorld) {
        let outgoing = self.primary;
        let outgoing_elapsed = outgoing
            .and_then(|o| self.instance_pos(o))
            .map_or(0.0, |i| self.instances[i].elapsed as f32);
        let transition = outgoing
            .and_then(|from| self.select_transition(from, rig, outgoing_elapsed))
            .cloned();
        let out_pose = outgoing
            .and_then(|o| self.instance_pos(o))
            .map(|i| self.instances[i].pose);
        let facing = transition
            .as_ref()
            .map_or(StartFacing::OwnSeed, |t| t.start_facing);
        let cut = transition.as_ref().is_none_or(|t| t.blend.duration() <= 0.0);
        let frozen = self.vars.values.clone();

        if self.instance_pos(rig).is_none() {
            let pose = self.seeded_pose(rig, facing, base, out_pose.as_ref(), world);
            self.instances.push(Instance::new(rig, pose, frozen));
        } else if !matches!(facing, StartFacing::OwnSeed) {
            // A rig that is still fading out restarts its facing from the policy as well.
            let pose = self.seeded_pose(rig, facing, base, out_pose.as_ref(), world);
            if let Some(i) = self.instance_pos(rig) {
                self.instances[i].pose = pose;
            }
        }
        let incoming = self.instance_pos(rig).unwrap_or(0);
        self.primary = Some(rig);

        if cut || outgoing.is_none() {
            // A cut drops every other instance; the incoming one starts afresh.
            let mut kept = self.instances.swap_remove(incoming);
            kept.weight = 1.0;
            kept.start_weight = 1.0;
            kept.elapsed = 0.0;
            self.instances.clear();
            self.instances.push(kept);
            self.blend = None;
            self.cut_pending = outgoing.is_some();
            return;
        }

        let mut transition = transition.unwrap_or_else(cut_transition);
        // Share-state needs the same node structure; otherwise cross-fade and warn once.
        if transition.share_state
            && let Some(from) = outgoing
            && !same_structure(&self.asset.rigs[from], &self.asset.rigs[rig])
        {
            if !self.warned_share {
                self.warned_share = true;
                tracing::warn!(
                    target: "regia_framework",
                    from = %self.asset.rigs[from].name,
                    to = %self.asset.rigs[rig].name,
                    "share_state transition between rigs of different structure; cross-fading poses"
                );
            }
            transition.share_state = false;
        }
        // Snapshot the weights; the incoming starts from its own current weight (0 when new).
        let total: f32 = self.instances.iter().map(|i| i.weight).sum();
        let scale = if total > 0.0 { 1.0 / total } else { 1.0 };
        for instance in &mut self.instances {
            instance.start_weight = instance.weight * scale;
        }
        self.blend = Some(ActiveBlend {
            transition,
            elapsed: 0.0,
            incoming: rig,
            outgoing,
        });
        if let Some(i) = self.instance_pos(rig) {
            self.instances[i].elapsed = 0.0;
            self.instances[i].frozen = self.vars.values.clone();
        }
    }

    // --- the frame ----------------------------------------------------------------------------

    /// Advances the driver by `dt` seconds and returns the final pose (§ 8 steps 1-5). `seed_pose`
    /// is the camera entity's current pose, used on the very first frame.
    #[tracing::instrument(skip_all, target = "regia_framework", name = "camera_driver", fields(entity = self.entity_bits))]
    pub fn step(
        &mut self,
        dt: f32,
        world: &dyn CameraWorld,
        hooks: &CameraHooks,
        seed_pose: CameraPose,
    ) -> CameraPose {
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        // `events` is drained by the owner (the driver system) each frame; keep it bounded for
        // hosts that never drain it.
        if self.events.len() > 256 {
            self.events.drain(..128);
        }
        self.time += f64::from(dt);
        let asset = Arc::clone(&self.asset);
        // A non-finite pose never seeds or carries over (audit-v035-numerics M5): nodes undo a
        // node that goes non-finite by restoring the pose before it, so a NaN seed would be
        // restored by every node and re-stored every frame. The last finite pose, then the
        // seed, then the default pose stand in.
        let base = match self.pose {
            Some(pose) if pose.is_finite() => pose,
            _ if seed_pose.is_finite() => seed_pose,
            _ => CameraPose::default(),
        };
        for instance in &mut self.instances {
            if !instance.pose.is_finite() {
                instance.pose = base;
            }
        }

        // 1) The director.
        let primary_elapsed = self
            .primary
            .and_then(|p| self.instance_pos(p))
            .map_or(0.0, |i| self.instances[i].elapsed as f32);
        let requests = {
            let ctx = DirectorContext {
                asset: &asset,
                vars: &self.vars,
                primary: self.primary.map(|p| asset.rigs[p].name.as_str()),
                primary_elapsed,
                time: self.time as f32,
                local_player: self.local_player,
            };
            evaluate(&asset.director, &ctx, hooks)
        };
        if let Some(requests) = &requests
            && let Some(&wanted) = requests.first()
            && self.primary != Some(wanted)
            && self.blend.as_ref().is_none_or(|b| b.transition.interruptible)
        {
            self.activate(wanted, base, world);
        }
        self.sync_director_layers(requests.as_deref().unwrap_or(&[]));

        // 2) The blend clock and weights.
        self.advance_blend(dt);

        // 3) Input slots, then every active rig.
        self.inputs.integrate(dt, &self.vars);
        let hold_outgoing = self.blend.as_ref().is_some_and(|b| b.transition.hold_outgoing);
        let share = self.blend.as_ref().and_then(|b| {
            b.transition
                .share_state
                .then_some((b.outgoing, b.incoming, b.transition.blend.weight(b.elapsed)))
        });
        if let Some((Some(from), to, c)) = share {
            self.merge_states(from, to, c);
        }
        for index in 0..self.instances.len() {
            let held = hold_outgoing && Some(self.instances[index].rig) != self.primary;
            if held {
                continue;
            }
            self.eval_instance(index, dt, world);
        }
        for instance in &self.instances {
            self.memory[instance.rig] = Some(instance.pose);
        }

        // 4) Combine by weights.
        self.below.clear();
        for instance in &self.instances {
            self.below.push((instance.pose, instance.weight));
        }
        let mut pose = combine(&self.below);

        // 5) Layers.
        pose = self.apply_layers(dt, pose, world);

        pose.cut |= std::mem::take(&mut self.cut_pending);
        if !pose.is_finite() {
            pose = CameraPose {
                cut: pose.cut,
                ..base
            };
        }
        self.vars.end_frame();
        self.pose = Some(pose);
        pose
    }

    fn merge_states(&mut self, from: usize, to: usize, c: f32) {
        let (Some(a), Some(b)) = (self.instance_pos(from), self.instance_pos(to)) else {
            return;
        };
        if a == b || self.instances[a].scratch.states.len() != self.instances[b].scratch.states.len() {
            return;
        }
        let out_states: Vec<NodeState> = self.instances[a].scratch.states.clone();
        for (state, other) in self.instances[b].scratch.states.iter_mut().zip(out_states) {
            *state = other.lerp(*state, c);
        }
    }

    fn advance_blend(&mut self, dt: f32) {
        let Some(blend) = self.blend.as_mut() else {
            if let Some(only) = self.instances.iter_mut().find(|i| Some(i.rig) == self.primary) {
                only.weight = 1.0;
            }
            return;
        };
        blend.elapsed += dt;
        let duration = blend.transition.blend.duration();
        let c = blend.transition.blend.weight(blend.elapsed);
        let incoming = blend.incoming;
        let done = blend.elapsed >= duration;
        for instance in &mut self.instances {
            let is_in = instance.rig == incoming;
            instance.weight = instance.start_weight * (1.0 - c) + if is_in { c } else { 0.0 };
        }
        if done {
            self.instances.retain(|i| i.rig == incoming);
            if let Some(only) = self.instances.first_mut() {
                only.weight = 1.0;
                only.start_weight = 1.0;
            }
            self.blend = None;
        }
    }

    fn eval_instance(&mut self, index: usize, dt: f32, world: &dyn CameraWorld) {
        let Self {
            asset,
            vars,
            view,
            inputs,
            overrides,
            instances,
            seed,
            ..
        } = self;
        let instance = &mut instances[index];
        let rig = &asset.rigs[instance.rig];
        view.blend_view(vars, &instance.frozen, instance.weight);

        // LookAt actions override the aim of this instance (before its nodes run).
        let mut aim_override = None;
        for action in &instance.actions {
            if let Some((point, _)) = world.target(&action.target) {
                aim_override = Some(point);
            }
        }
        let mut pose = instance.pose;
        pose.roll = 0.0;
        pose.cut = false;
        let ctx = EvalCtx {
            dt,
            elapsed: instance.elapsed as f32,
            seed: *seed,
            asset,
            rig,
            overrides,
            vars: view,
            inputs,
            world,
            prev: instance.pose,
            aim_override,
        };
        eval_rig(&ctx, &mut pose, &mut instance.scratch);
        instance.elapsed += f64::from(dt);
        // A non-hold LookAt ends once the target is inside the tolerance cone. The decision
        // compares the dot to `det_cos(tolerance)` — a multiply and a compare, bit-identical on
        // every platform — instead of `acos`, which follows the platform libm (spec 35 § 12.13).
        let forward = pose.forward();
        instance.actions.retain(|action| {
            if action.hold {
                return true;
            }
            let Some((point, _)) = world.target(&action.target) else {
                return true;
            };
            let to = (point - pose.position).normalize_or_zero();
            // Keep aiming while the target is outside the cone; drop it once reached.
            to == Vec3::ZERO || !look_at_finished(forward.dot(to).clamp(-1.0, 1.0), action.tolerance)
        });
        instance.pose = pose;
    }

    fn sync_director_layers(&mut self, requests: &[usize]) {
        let extras: Vec<usize> = requests.iter().skip(1).copied().collect();
        // Release director layers no longer requested.
        let stale: Vec<LayerId> = self
            .layers
            .iter()
            .filter(|l| l.from_director && l.phase != Phase::Leaving && !extras.contains(&l.rig))
            .map(|l| l.id)
            .collect();
        for id in stale {
            let _ = self.release(ReleaseBy::Id(id), false);
        }
        for rig in extras {
            if self
                .layers
                .iter()
                .any(|l| l.from_director && l.rig == rig && l.phase != Phase::Leaving)
            {
                continue;
            }
            let id = self.new_layer(
                rig,
                false,
                DIRECTOR_LAYER_ORDER + self.layers.len() as i32,
                None,
                Blend::Cut,
            );
            if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
                layer.from_director = true;
            }
        }
    }

    fn eval_layer(&mut self, index: usize, below: CameraPose, dt: f32, world: &dyn CameraWorld) {
        let Self {
            asset,
            vars,
            view,
            inputs,
            overrides,
            layers,
            seed,
            ..
        } = self;
        let layer = &mut layers[index];
        if layer.held {
            return;
        }
        let rig = &asset.rigs[layer.rig];
        view.blend_view(vars, &layer.frozen, layer.weight.max(1.0e-3));
        let mut pose = if layer.overlay && layer.seeded {
            layer.pose
        } else {
            below
        };
        pose.roll = 0.0;
        pose.cut = false;
        let prev = pose;
        let context_world;
        let world: &dyn CameraWorld = match &layer.context {
            Some(context) => {
                context_world = ContextWorld {
                    inner: world,
                    context,
                };
                &context_world
            }
            None => world,
        };
        let ctx = EvalCtx {
            dt,
            elapsed: layer.elapsed,
            seed: *seed,
            asset,
            rig,
            overrides,
            vars: view,
            inputs,
            world,
            prev,
            aim_override: None,
        };
        eval_rig(&ctx, &mut pose, &mut layer.scratch);
        layer.elapsed += dt;
        layer.pose = pose;
        layer.seeded = true;
    }

    fn apply_layers(&mut self, dt: f32, base: CameraPose, world: &dyn CameraWorld) -> CameraPose {
        if self.layers.is_empty() {
            return base;
        }
        // Advance phases and weights first.
        let mut gone: Vec<usize> = Vec::new();
        let mut settled: Vec<(LayerId, usize)> = Vec::new();
        let mut self_ending: Vec<(usize, Blend)> = Vec::new();
        for (i, layer) in self.layers.iter_mut().enumerate() {
            match layer.phase {
                Phase::Entering => {
                    layer.in_elapsed += dt;
                    layer.weight = layer.blend_in.weight(layer.in_elapsed);
                    if layer.in_elapsed >= layer.blend_in.duration() {
                        layer.weight = 1.0;
                        layer.phase = Phase::Settled;
                        settled.push((layer.id, layer.rig));
                    }
                }
                Phase::Settled => {}
                Phase::Leaving => {
                    layer.out_elapsed += dt;
                    layer.weight = layer.out_start * (1.0 - layer.blend_out.weight(layer.out_elapsed));
                    if layer.out_elapsed >= layer.blend_out.duration() {
                        layer.weight = 0.0;
                        gone.push(i);
                    }
                }
            }
            // A self-ending overlay: the rig's own transition back, gated on its local clock.
            if layer.overlay && layer.phase != Phase::Leaving {
                let rig = &self.asset.rigs[layer.rig];
                let cond = CondCtx {
                    elapsed: layer.elapsed,
                    vars: &self.vars,
                    rig,
                };
                let hit = rig.transitions.iter().find(|t| {
                    let from_ok = match &t.from {
                        RigMatch::Any => true,
                        RigMatch::Rig(name) => name == &rig.name,
                        RigMatch::Tag(tag) => rig.tags.iter().any(|x| x == tag),
                    };
                    from_ok
                        && !t.conditions.is_empty()
                        && t.conditions.iter().all(|c| eval_condition(c, &cond))
                });
                if let Some(t) = hit {
                    self_ending.push((i, t.blend));
                }
            }
        }
        for (id, rig) in settled {
            self.emit(id, rig, LayerEventKind::Settled);
        }
        for (i, blend) in self_ending {
            let (rig, id) = (self.layers[i].rig, self.layers[i].id);
            let layer = &mut self.layers[i];
            layer.phase = Phase::Leaving;
            layer.out_start = layer.weight;
            layer.out_elapsed = 0.0;
            layer.blend_out = blend;
            self.emit(id, rig, LayerEventKind::Leaving);
            if blend.duration() <= 0.0 {
                gone.push(i);
            }
        }
        // An overlay that a later overlay has fully blended in is gone.
        let overlay_indices: Vec<usize> = (0..self.layers.len())
            .filter(|i| self.layers[*i].overlay)
            .collect();
        for (n, &i) in overlay_indices.iter().enumerate() {
            let covered = overlay_indices[n + 1..]
                .iter()
                .any(|&j| self.layers[j].weight >= 1.0 && self.layers[j].phase != Phase::Leaving);
            if covered {
                gone.push(i);
            }
        }
        gone.sort_unstable();
        gone.dedup();
        for &i in gone.iter().rev() {
            let layer = self.layers.remove(i);
            self.emit(layer.id, layer.rig, LayerEventKind::Gone);
        }

        // Evaluate and apply: overlays in insertion order, then additives by order.
        let mut order: Vec<usize> = (0..self.layers.len()).collect();
        order.sort_by_key(|i| {
            let l = &self.layers[*i];
            (!l.overlay, if l.overlay { 0 } else { l.order })
        });
        let mut pose = base;
        for index in order {
            let below = pose;
            self.eval_layer(index, below, dt, world);
            let layer = &self.layers[index];
            let w = layer.weight.clamp(0.0, 1.0);
            if w <= 0.0 {
                continue;
            }
            if layer.overlay {
                pose = if w >= 1.0 {
                    layer.pose
                } else {
                    combine(&[(pose, 1.0 - w), (layer.pose, w)])
                };
            } else {
                pose = add_delta(pose, layer.pose, w);
            }
        }
        pose
    }
}

/// The default used only if a transition list changes under us mid-activation (never in
/// practice); a plain cut.
fn cut_transition() -> TransitionDef {
    TransitionDef {
        from: RigMatch::Any,
        to: RigMatch::Any,
        blend: Blend::Cut,
        start_facing: StartFacing::OwnSeed,
        hold_outgoing: false,
        share_state: false,
        interruptible: true,
        conditions: Vec::<Condition>::new(),
    }
}

/// `true` when a non-hold `LookAt` has reached its tolerance cone. Exactly
/// `acos(forward_dot) <= tolerance`, rewritten as `forward_dot >= det_cos(tolerance)` with the
/// pinned cosine series, so two platforms' libms cannot split the keep-or-drop decision (spec 35
/// § 12.13).
fn look_at_finished(forward_dot: f32, tolerance: f32) -> bool {
    if tolerance >= std::f32::consts::PI {
        return true; // `acos` never exceeds π, so the cone always counts as reached.
    }
    forward_dot >= det_cos(tolerance)
}

/// Applies the delta `layer − below` to `below` at `weight` (§ 7): position delta, rotation delta
/// composed by quaternion, fov delta.
fn add_delta(below: CameraPose, layer: CameraPose, weight: f32) -> CameraPose {
    let mut out = below;
    out.position = below.position + (layer.position - below.position) * weight;
    out.fov_deg = below.fov_deg + (layer.fov_deg - below.fov_deg) * weight;
    let delta = layer.rotation() * below.rotation().inverse();
    // The share of the arc by pinned nlerp (add, multiply, one sqrt), not `slerp`'s libm
    // `acos`/`sin`, so an additive layer's pose is bit-identical across platforms.
    let scaled = det_nlerp(Quat::IDENTITY, delta, weight);
    out.set_rotation(scaled * below.rotation());
    // Keep the yaw continuous across the wrap.
    out.yaw = wrap_pi(out.yaw);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::nodes::EmptyWorld;

    const DT: f32 = 1.0 / 60.0;

    fn asset(text: &str) -> Arc<CameraAsset> {
        let asset = CameraAsset::from_ron(text).expect("test asset parses");
        let issues = asset.validate(&|_| false);
        assert!(
            issues.iter().all(|i| !i.is_error()),
            "test asset is valid: {issues:?}"
        );
        Arc::new(asset)
    }

    fn core(text: &str) -> DriverCore {
        DriverCore::new(asset(text), 7, 0)
    }

    fn step(core: &mut DriverCore) -> CameraPose {
        core.step(DT, &EmptyWorld, &CameraHooks::default(), CameraPose::default())
    }

    fn run(core: &mut DriverCore, frames: usize) -> CameraPose {
        let mut pose = CameraPose::default();
        for _ in 0..frames {
            pose = step(core);
        }
        pose
    }

    /// Three rigs at x = 0, 10, -10 with distinct fov, switched by two variables.
    const THREE: &str = r#"(format_version: 1, name: "t",
        variables: [(name: "go", default: Bool(false)), (name: "go2", default: Bool(false))],
        rigs: [
            (name: "a", tags: ["x"], root: [Pivot(target: Point((0.0, 0.0, 0.0))), Fov(degrees: Literal(40.0))]),
            (name: "b", tags: ["y"], root: [Pivot(target: Point((10.0, 0.0, 0.0))), Fov(degrees: Literal(80.0))]),
            (name: "c", root: [Pivot(target: Point((-10.0, 0.0, 0.0))), Fov(degrees: Literal(60.0))]),
        ],
        transitions: [(from: Any, to: Any, blend: Linear(1.0))],
        director: Rules(default: "a", rules: [
            (when: VarBool("go2", true), rig: "c"),
            (when: VarBool("go", true), rig: "b"),
        ]))"#;

    #[test]
    fn look_at_end_decision_is_pinned_one_ulp_around_the_cone() {
        // The keep-or-drop decision is `dot >= det_cos(tolerance)`: one f32 ulp on either side of
        // the pinned boundary must drop and keep respectively, whatever the platform libm's `acos`
        // would say (spec 35 § 12.13).
        let tol = 0.7f32;
        let boundary = det_cos(tol);
        assert!(
            look_at_finished(boundary, tol),
            "exactly on the cone counts as reached"
        );
        // A larger dot is a smaller angle: one ulp toward the target is inside the cone.
        let inside = f32::from_bits(boundary.to_bits() + 1);
        assert!(look_at_finished(inside, tol), "one ulp inside the cone ends it");
        let outside = f32::from_bits(boundary.to_bits() - 1);
        assert!(
            !look_at_finished(outside, tol),
            "one ulp away from the target still aims"
        );
        assert!(look_at_finished(1.0, 0.0), "zero tolerance: dead on ends it");
        assert!(!look_at_finished(-1.0, 0.0));
        assert!(
            look_at_finished(0.0, std::f32::consts::PI),
            "a 180-degree cone always counts as reached"
        );
        assert!(!look_at_finished(-0.5, 89.9f32.to_radians()));
    }

    #[test]
    fn a_look_at_one_ulp_outside_the_cone_keeps_the_aim_override_next_frame() {
        // The end-of-action test used `acos`, so a target sitting one ulp outside the cone could
        // read as inside on one libm and outside on another; the pose then stopped tracking on
        // one peer only. The dot is now compared to the pinned `det_cos` boundary.
        let mut c = core(
            r#"(format_version: 1, name: "t",
            rigs: [(name: "a", root: [])],
            director: Fixed("a"))"#,
        );
        let tol_deg = 0.7f32.to_degrees();
        let boundary = det_cos(0.7f32);
        // A smaller dot is a larger angle: one ulp away from the target stays outside the cone.
        let dot = f32::from_bits(boundary.to_bits() - 1);
        let lateral = (1.0 - dot * dot).max(0.0).sqrt();
        let point = Vec3::new(lateral, 0.0, -dot);
        step(&mut c);
        let id = c
            .look_at("a", TargetRef::Point(point.to_array()), tol_deg, false)
            .expect("action added");
        let first = step(&mut c);
        assert_eq!(first.aim, Some(point), "the override drives the aim");
        let second = step(&mut c);
        assert_eq!(second.aim, Some(point), "one ulp outside the cone keeps aiming");
        assert!(c.clear_look_at(id));
    }

    #[test]
    fn a_look_at_one_ulp_inside_the_cone_drops_the_action() {
        let mut c = core(
            r#"(format_version: 1, name: "t",
            rigs: [(name: "a", root: [])],
            director: Fixed("a"))"#,
        );
        let tol_deg = 0.7f32.to_degrees();
        let boundary = det_cos(0.7f32);
        // A larger dot is a smaller angle: one ulp toward the target lands inside the cone.
        let dot = f32::from_bits(boundary.to_bits() + 1);
        let lateral = (1.0 - dot * dot).max(0.0).sqrt();
        let point = Vec3::new(lateral, 0.0, -dot);
        step(&mut c);
        c.look_at("a", TargetRef::Point(point.to_array()), tol_deg, false)
            .expect("action added");
        let first = step(&mut c);
        assert_eq!(first.aim, Some(point));
        step(&mut c);
        assert_eq!(c.action_count(), 0, "one ulp inside the cone ends the action");
    }

    #[test]
    fn first_rig_is_a_cut_and_a_switch_blends_with_weights_summing_to_one() {
        let mut c = core(THREE);
        let first = step(&mut c);
        assert_eq!(first.position.x, 0.0);
        assert_eq!(c.primary_name(), Some("a"));
        c.set_var("go", VarValue::Bool(true)).expect("var");
        let mut last = 0.0f32;
        for frame in 1..=62 {
            let pose = step(&mut c);
            let total: f32 = c.rig_infos().iter().map(|r| r.weight).sum();
            assert!(
                (total - 1.0).abs() < 1.0e-5,
                "weights sum to 1 at frame {frame}: {total}"
            );
            assert!(pose.position.x >= last, "the blend is monotonic");
            last = pose.position.x;
        }
        assert_eq!(last, 10.0, "the blend ends exactly on the incoming rig");
        assert_eq!(c.rig_infos().len(), 1);
        assert!(c.blend_info().is_none());
    }

    #[test]
    fn interrupting_a_blend_continues_from_the_blended_pose() {
        let mut c = core(THREE);
        step(&mut c);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        let mut previous = step(&mut c).position.x;
        let mut worst = 0.0f32;
        for frame in 0..90 {
            if frame == 20 {
                c.set_var("go2", VarValue::Bool(true)).expect("var");
            }
            let x = step(&mut c).position.x;
            worst = worst.max((x - previous).abs());
            previous = x;
        }
        // The fastest legitimate motion is 10 m over 1 s: 0.17 m a frame. A pop would be metres.
        assert!(worst < 0.23, "no pop at the interruption: worst step {worst}");
        assert_eq!(previous, -10.0, "ended on the newest rig");
    }

    #[test]
    fn non_interruptible_blend_defers_the_new_rig_until_done() {
        let text = THREE.replace(
            "transitions: [(from: Any, to: Any, blend: Linear(1.0))]",
            "transitions: [(from: Any, to: Any, blend: Linear(1.0), interruptible: false)]",
        );
        let mut c = core(&text);
        step(&mut c);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        run(&mut c, 10);
        c.set_var("go2", VarValue::Bool(true)).expect("var");
        run(&mut c, 5);
        assert_eq!(c.primary_name(), Some("b"), "still on the first blend's rig");
        run(&mut c, 70);
        assert_eq!(
            c.primary_name(),
            Some("c"),
            "the deferred switch happened afterwards"
        );
    }

    #[test]
    fn transition_choice_order_tags_rigs_conditions_and_cut() {
        let text = r#"(format_version: 1, name: "t",
            variables: [(name: "ok", default: Bool(false))],
            rigs: [
                (name: "a", tags: ["ground"], root: [Fov(degrees: Literal(40.0))],
                    transitions: [(from: Any, to: Rig("b"), blend: Linear(1.0))]),
                (name: "b", tags: ["aim"], root: [Fov(degrees: Literal(50.0))],
                    transitions: [(from: Any, to: Any, blend: Linear(2.0))]),
                (name: "c", tags: ["aim"], root: [Fov(degrees: Literal(60.0))]),
                (name: "d", root: [Fov(degrees: Literal(70.0))]),
            ],
            transitions: [
                (from: Any, to: Tag("aim"), blend: Linear(3.0), conditions: [VarBool("ok", true)]),
                (from: Tag("ground"), to: Rig("d"), blend: Linear(4.0)),
            ],
            director: Fixed("a"))"#;
        let mut c = core(text);
        let dur = |c: &DriverCore, from: usize, to: usize| {
            c.select_transition(from, to, 0.0).map(|t| t.blend.duration())
        };
        assert_eq!(dur(&c, 0, 1), Some(1.0), "the outgoing rig's own list wins");
        assert_eq!(dur(&c, 2, 1), Some(2.0), "then the incoming rig's own list");
        assert_eq!(
            dur(&c, 3, 2),
            None,
            "the shared Tag rule needs its condition: no match is a cut"
        );
        c.set_var("ok", VarValue::Bool(true)).expect("var");
        assert_eq!(
            dur(&c, 3, 2),
            Some(3.0),
            "shared list, tag match, condition passes"
        );
        assert_eq!(dur(&c, 0, 3), Some(4.0), "from-tag and to-rig matching");
        assert_eq!(dur(&c, 3, 0), None);
    }

    /// Two rigs facing differently, to see what the incoming rig starts with.
    const FACING: &str = r#"(format_version: 1, name: "t",
        inputs: [(name: "look", kind: Axis2, speed: (0.0, 0.0))],
        variables: [(name: "go", default: Bool(false))],
        rigs: [
            (name: "a", root: [Pivot(target: Point((0.0, 0.0, 0.0))), LookInput(slot: "look"), Boom(length: Literal(5.0))]),
            (name: "b", root: [Fov(degrees: Literal(50.0))]),
        ],
        transitions: [(from: Any, to: Any, blend: Linear(1.0), start_facing: FACING)],
        director: Rules(default: "a", rules: [(when: VarBool("go", true), rig: "b")]))"#;

    fn facing_pose(policy: &str) -> CameraPose {
        let mut c = core(&FACING.replace("FACING", policy));
        assert!(c.inputs.set_angles("look", 0.8, 0.2));
        run(&mut c, 3);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        step(&mut c);
        c.rig_infos()
            .iter()
            .find(|r| r.name == "b")
            .expect("rig b is active")
            .pose
    }

    /// A world whose drive target is rotated 1.2 rad about +Y.
    struct TurnedTarget;

    impl CameraWorld for TurnedTarget {
        fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)> {
            match target {
                TargetRef::DriveTarget => Some((Vec3::ZERO, Quat::from_rotation_y(1.2))),
                TargetRef::Point(p) => Some((Vec3::from(*p), Quat::IDENTITY)),
                TargetRef::Entity(_) => None,
            }
        }

        fn cast_sphere(&self, _: Vec3, _: Vec3, _: f32, _: f32, _: &[String]) -> Option<f32> {
            None
        }
    }

    #[test]
    fn start_facing_policies() {
        // A rig that never ran starts from the camera's current facing (0.8 rad of yaw).
        let own = facing_pose("OwnSeed");
        assert!((own.yaw - 0.8).abs() < 1e-3, "own seed {}", own.yaw);
        let carried = facing_pose("CarryOver");
        assert!((carried.yaw - 0.8).abs() < 1e-3, "carry over yaw {}", carried.yaw);
        let aimed = facing_pose("ContinueAim");
        assert!(
            aimed.is_finite() && (aimed.yaw - 0.8).abs() < 1e-3,
            "continue aim {}",
            aimed.yaw
        );
        // TargetFacing takes the drive target's yaw.
        let mut c = core(&FACING.replace("FACING", "TargetFacing"));
        assert!(c.inputs.set_angles("look", 0.8, 0.2));
        let hooks = CameraHooks::default();
        for _ in 0..3 {
            c.step(DT, &TurnedTarget, &hooks, CameraPose::default());
        }
        c.set_var("go", VarValue::Bool(true)).expect("var");
        c.step(DT, &TurnedTarget, &hooks, CameraPose::default());
        let b = c
            .rig_infos()
            .into_iter()
            .find(|r| r.name == "b")
            .expect("b active");
        assert!((b.pose.yaw - 1.2).abs() < 1e-4, "target facing {}", b.pose.yaw);
    }

    #[test]
    fn own_seed_resumes_a_rig_from_its_remembered_pose() {
        let text = r#"(format_version: 1, name: "t",
            variables: [(name: "go", default: Bool(false))],
            inputs: [(name: "look", kind: Axis2, speed: (0.0, 0.0))],
            rigs: [
                (name: "a", root: [LookInput(slot: "look")]),
                (name: "b", root: [Fov(degrees: Literal(50.0))]),
            ],
            transitions: [(from: Any, to: Any, blend: Cut)],
            director: Rules(default: "a", rules: [(when: VarBool("go", true), rig: "b")]))"#;
        let mut c = core(text);
        assert!(c.inputs.set_angles("look", 0.5, 0.0));
        run(&mut c, 2);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        run(&mut c, 2);
        assert!(
            (c.pose().expect("pose").yaw - 0.5).abs() < 1e-4,
            "b seeded from the camera's facing"
        );
        assert!(c.inputs.set_angles("look", 1.0, 0.0));
        c.set_var("go", VarValue::Bool(false)).expect("var");
        run(&mut c, 2);
        assert!((c.pose().expect("pose").yaw - 1.0).abs() < 1e-4);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        run(&mut c, 2);
        assert!(
            (c.pose().expect("pose").yaw - 0.5).abs() < 1e-4,
            "b remembers its own last pose"
        );
    }

    #[test]
    fn hold_outgoing_freezes_the_outgoing_pose() {
        let text = THREE.replace(
            "blend: Linear(1.0))]",
            "blend: Linear(1.0), hold_outgoing: true)]",
        );
        let mut c = core(&text);
        run(&mut c, 3);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        step(&mut c);
        let before = c.rig_infos().iter().find(|r| r.name == "a").expect("a").elapsed;
        run(&mut c, 10);
        let after = c.rig_infos().iter().find(|r| r.name == "a").expect("a").elapsed;
        assert_eq!(before, after, "the outgoing rig stopped updating");
    }

    const SHARED: &str = r#"(format_version: 1, name: "t",
        variables: [(name: "go", default: Bool(false))],
        rigs: [
            (name: "a", root: [Pivot(target: Point((0.0, 0.0, 0.0))), Offset(translation: (STEP, 0.0, 0.0), space: World), SmoothPosition(depth: 2.0, side: 2.0, height: 2.0, space: World)]),
            (name: "b", root: [Pivot(target: Point((0.0, 0.0, 0.0))), Offset(translation: (STEP2, 0.0, 0.0), space: World), SmoothPosition(depth: 2.0, side: 2.0, height: 2.0, space: World)]),
        ],
        transitions: [(from: Any, to: Any, blend: Linear(1.0), share_state: SHARE)],
        director: Rules(default: "a", rules: [(when: VarBool("go", true), rig: "b")]))"#;

    /// The worst acceleration (change of per-frame velocity) across a blend of two smoothed rigs.
    fn worst_jerk(share: bool) -> f32 {
        let text = SHARED
            .replace("STEP2", "-20.0")
            .replace("STEP", "20.0")
            .replace("SHARE", if share { "true" } else { "false" });
        let mut c = core(&text);
        run(&mut c, 240);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        let mut last = c.pose().expect("pose").position.x;
        let mut last_velocity = 0.0;
        let mut worst = 0.0f32;
        for _ in 0..90 {
            let x = step(&mut c).position.x;
            let velocity = x - last;
            worst = worst.max((velocity - last_velocity).abs());
            last = x;
            last_velocity = velocity;
        }
        worst
    }

    #[test]
    fn share_state_removes_the_velocity_discontinuity_of_a_cross_fade() {
        let shared = worst_jerk(true);
        let faded = worst_jerk(false);
        assert!(shared <= faded + 1.0e-6, "shared {shared} vs cross-fade {faded}");
    }

    #[test]
    fn share_state_between_different_structures_warns_once_and_cross_fades() {
        let text = THREE
            .replace("blend: Linear(1.0))]", "blend: Linear(1.0), share_state: true)]")
            .replace(
                "Fov(degrees: Literal(80.0))]),",
                "Fov(degrees: Literal(80.0)), Fov(degrees: Literal(80.0))]),",
            );
        let mut c = core(&text);
        run(&mut c, 3);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        run(&mut c, 70);
        assert!(c.warned_share, "the mismatch was reported");
        assert_eq!(c.primary_name(), Some("b"));
        assert_eq!(c.pose().expect("pose").position.x, 10.0);
    }

    const LAYERS: &str = r#"(format_version: 1, name: "t",
        rigs: [
            (name: "main", root: [Pivot(target: Point((0.0, 0.0, 0.0))), Fov(degrees: Literal(60.0))]),
            (name: "hit", root: [Fov(degrees: Literal(30.0))], blend_out: Some(0.5),
                transitions: [(from: Any, to: Any, blend: Linear(0.1), conditions: [Elapsed(0.5)])]),
            (name: "cam", root: [Fov(degrees: Literal(30.0))], blend_out: Some(0.5)),
            (name: "lift", root: [Offset(translation: (0.0, 1.0, 0.0), space: World)]),
            (name: "sway", root: [Offset(translation: (0.0, 0.0, 2.0), space: World)]),
        ],
        director: Fixed("main"))"#;

    fn kinds(c: &DriverCore) -> Vec<LayerEventKind> {
        c.event_log.iter().map(|e| e.kind).collect()
    }

    fn ramp(seconds: f32) -> OverlayOptions {
        OverlayOptions {
            transition: Some(TransitionDef {
                from: RigMatch::Any,
                to: RigMatch::Any,
                blend: Blend::Linear(seconds),
                start_facing: StartFacing::OwnSeed,
                hold_outgoing: false,
                share_state: false,
                interruptible: true,
                conditions: vec![],
            }),
            ..OverlayOptions::default()
        }
    }

    #[test]
    fn overlay_blends_in_settles_and_blends_out_through_release() {
        let mut c = core(LAYERS);
        run(&mut c, 2);
        let id = c.push_overlay("cam", ramp(0.5)).expect("push");
        let mid = run(&mut c, 15).fov_deg;
        assert!(mid < 60.0 && mid > 30.0, "blending in: {mid}");
        let settled = run(&mut c, 30).fov_deg;
        assert!(
            (settled - 30.0).abs() < 1.0e-4,
            "settled on the overlay: {settled}"
        );
        assert_eq!(c.release(ReleaseBy::Id(id), false), Ok(1));
        run(&mut c, 5);
        let out = c.pose().expect("pose").fov_deg;
        assert!(out > 30.0 && out < 60.0, "blending out: {out}");
        let back = run(&mut c, 40).fov_deg;
        assert!(
            (back - 60.0).abs() < 1.0e-4,
            "the director's pose is back: {back}"
        );
        assert!(!c.has_layer(id), "a released layer removes itself");
        assert_eq!(
            kinds(&c),
            [
                LayerEventKind::Entered,
                LayerEventKind::Settled,
                LayerEventKind::Leaving,
                LayerEventKind::Gone
            ]
        );
    }

    #[test]
    fn overlay_always_new_and_reuse() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let a = c.push_overlay("hit", ramp(0.2)).expect("push");
        run(&mut c, 20);
        let again = c.push_overlay("hit", ramp(0.2)).expect("push");
        assert_eq!(a, again, "the same rig is reused and its blend-in restarts");
        run(&mut c, 1);
        assert!(
            c.pose().expect("pose").fov_deg > 30.0,
            "the restarted blend is not settled yet"
        );
        let options = OverlayOptions {
            always_new: true,
            ..ramp(0.2)
        };
        let other = c.push_overlay("hit", options).expect("push");
        assert_ne!(a, other);
        assert_eq!(
            c.push_overlay("nope", OverlayOptions::default()),
            Err(CameraError::UnknownRig("nope".into()))
        );
    }

    /// Re-pushing a live overlay restarts it like a new layer: the smoothing state, the carried
    /// pose and the frozen variables of the previous run are dropped, so the first frame
    /// reseeds from the pose below instead of easing from stale motion (audit-gameplay-muse L8).
    #[test]
    fn restarting_an_overlay_reseeds_its_state() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let a = c.push_overlay("hit", ramp(0.2)).expect("push");
        run(&mut c, 20);
        let layer = |c: &DriverCore| {
            c.layers
                .iter()
                .find(|l| l.id == a)
                .map(|l| (l.seeded, l.scratch.trace.len()))
        };
        assert_eq!(
            layer(&c).map(|(s, _)| s),
            Some(true),
            "the running overlay is seeded"
        );
        let again = c.push_overlay("hit", ramp(0.2)).expect("push");
        assert_eq!(a, again);
        let (seeded, trace) = layer(&c).expect("layer kept");
        assert!(!seeded, "a restarted overlay reseeds from the pose below");
        assert_eq!(trace, 0, "the previous run's scratch is dropped");
    }

    #[test]
    fn a_later_overlay_that_is_fully_in_removes_the_earlier_one() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let first = c.push_overlay("hit", ramp(0.1)).expect("push");
        run(&mut c, 10);
        let second = c
            .push_overlay(
                "lift",
                OverlayOptions {
                    always_new: true,
                    ..ramp(0.1)
                },
            )
            .expect("push");
        run(&mut c, 20);
        assert!(!c.has_layer(first), "covered overlay is gone");
        assert!(c.has_layer(second));
    }

    #[test]
    fn hold_overlay_stops_updating_it() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let id = c.push_overlay("hit", OverlayOptions::default()).expect("push");
        run(&mut c, 3);
        c.hold_overlay(id).expect("hold");
        let elapsed = |c: &DriverCore| c.layers.iter().find(|l| l.id == id).map(|l| l.elapsed);
        let before = elapsed(&c);
        run(&mut c, 10);
        assert_eq!(before, elapsed(&c));
        assert_eq!(c.hold_overlay(LayerId(999)), Err(CameraError::UnknownLayer(999)));
    }

    #[test]
    fn cut_overlay_switches_with_no_interpolation() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let id = c.push_overlay("hit", OverlayOptions::default()).expect("push");
        let pose = step(&mut c);
        assert_eq!(pose.fov_deg, 30.0, "no blend");
        assert_eq!(kinds(&c)[..2], [LayerEventKind::Entered, LayerEventKind::Cut]);
        let _ = id;
    }

    #[test]
    fn self_ending_overlay_leaves_on_its_own_elapsed_condition() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let id = c.push_overlay("hit", OverlayOptions::default()).expect("push");
        run(&mut c, 20);
        assert!(c.has_layer(id), "still running before 0.5 s");
        run(&mut c, 30);
        assert!(!c.has_layer(id), "gone after its Elapsed transition fired");
        assert!(kinds(&c).contains(&LayerEventKind::Leaving) && kinds(&c).contains(&LayerEventKind::Gone));
    }

    #[test]
    fn additive_layers_sort_by_order_and_refuse_duplicates() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let lift = c.add_additive("lift", 5, None, false).expect("add");
        assert_eq!(
            c.add_additive("sway", 5, None, false),
            Err(CameraError::OrderTaken(5))
        );
        let sway = c
            .add_additive("sway", 5, None, true)
            .expect("same order allowed on request");
        let early = c.add_additive("sway", 1, None, false).expect("add");
        let order: Vec<LayerId> = c.layer_infos().iter().map(|l| l.id).collect();
        assert_eq!(order, [early, lift, sway], "by order, ties by insertion");
        run(&mut c, 30);
        let pose = c.pose().expect("pose");
        assert!(
            (pose.position.y - 1.0).abs() < 1.0e-4,
            "lift adds +1 on y: {}",
            pose.position.y
        );
        assert!(
            (pose.position.z - 4.0).abs() < 1.0e-4,
            "two sways add +2 each: {}",
            pose.position.z
        );
    }

    #[test]
    fn release_instant_vs_blended() {
        let mut c = core(LAYERS);
        run(&mut c, 1);
        let a = c.add_additive("lift", 1, None, false).expect("add");
        let b = c.add_additive("sway", 2, None, false).expect("add");
        run(&mut c, 30);
        assert_eq!(c.release(ReleaseBy::Id(a), true), Ok(1));
        assert!(!c.has_layer(a), "instant drops at once");
        assert_eq!(c.release(ReleaseBy::Rig("sway".into(), None), false), Ok(1));
        assert!(c.has_layer(b), "blended release keeps it while it blends out");
        run(&mut c, 40);
        assert!(!c.has_layer(b));
        assert_eq!(
            c.release(ReleaseBy::Id(LayerId(77)), false),
            Err(CameraError::UnknownLayer(77))
        );
        assert_eq!(c.release(ReleaseBy::Rig("lift".into(), None), true), Ok(0));
        let events: Vec<_> = c.event_log.iter().map(|e| (e.id, e.kind)).collect();
        assert!(
            events.contains(&(a, LayerEventKind::Gone)) && events.contains(&(b, LayerEventKind::Leaving))
        );
    }

    #[test]
    fn params_variables_and_inputs_check_types() {
        let mut c = core(
            r#"(format_version: 1, name: "t",
                params: {"len": Float(4.0), "on": Bool(true)},
                variables: [(name: "v", default: Float(1.0))],
                inputs: [(name: "look", kind: Axis2, speed: (90.0, 90.0))],
                rigs: [(name: "m", root: [Boom(length: Param("len")), Group(enabled: Param("on"), nodes: [])])],
                director: Fixed("m"))"#,
        );
        assert_eq!(c.set_param("len", ParamValue::Float(6.0)), Ok(()));
        assert!(matches!(
            c.set_param("len", ParamValue::Bool(true)),
            Err(CameraError::BadParam { .. })
        ));
        assert_eq!(
            c.set_param("zzz", ParamValue::Float(1.0)),
            Err(CameraError::UnknownName("zzz".into()))
        );
        assert_eq!(c.param("len"), Some(&ParamValue::Float(6.0)));
        assert_eq!(c.set_var("v", VarValue::Float(2.0)), Ok(()));
        assert!(matches!(
            c.set_var("v", VarValue::Bool(true)),
            Err(CameraError::BadValue(_))
        ));
        assert_eq!(
            c.set_var("none", VarValue::Float(0.0)),
            Err(CameraError::UnknownName("none".into()))
        );
        assert_eq!(c.set_input("look", [1.0, 0.0]), Ok(()));
        assert!(matches!(
            c.set_input("look", [f32::NAN, 0.0]),
            Err(CameraError::BadValue(_))
        ));
        assert_eq!(c.reset_input("look"), Ok(()));
        let pose = step(&mut c);
        assert!(
            (pose.position.z - 6.0).abs() < 1.0e-5,
            "the override drives the boom: {}",
            pose.position.z
        );
        assert!(
            !c.vars.written_this_frame("v"),
            "written flags clear at the end of the frame"
        );
    }

    #[test]
    fn look_at_ends_within_tolerance_or_holds_until_cleared() {
        let text = r#"(format_version: 1, name: "t",
            rigs: [(name: "m", root: [Pivot(target: Point((0.0, 0.0, 0.0))), Boom(length: Literal(5.0))])],
            director: Fixed("m"))"#;
        let mut c = core(text);
        run(&mut c, 2);
        let once = c
            .look_at("m", TargetRef::Point([5.0, 0.0, 0.0]), 5.0, false)
            .expect("look_at");
        assert_eq!(c.action_count(), 1);
        let pose = step(&mut c);
        assert_eq!(
            c.action_count(),
            0,
            "the non-hold action ended once the target was inside the tolerance"
        );
        let to = (Vec3::new(5.0, 0.0, 0.0) - pose.position).normalize();
        assert!(
            pose.forward().dot(to) > 0.999,
            "the camera framed the override point"
        );
        assert!(!c.clear_look_at(once));
        let held = c
            .look_at("m", TargetRef::Point([-5.0, 0.0, 0.0]), 5.0, true)
            .expect("look_at");
        let pose = run(&mut c, 30);
        assert_eq!(c.action_count(), 1, "the hold action persists");
        let to = (Vec3::new(-5.0, 0.0, 0.0) - pose.position).normalize();
        assert!(pose.forward().dot(to) > 0.999);
        assert!(c.clear_look_at(held));
        assert_eq!(c.action_count(), 0);
        assert!(matches!(
            c.look_at("nope", TargetRef::DriveTarget, 1.0, false),
            Err(CameraError::UnknownRig(_))
        ));
    }

    #[test]
    fn two_runs_are_bit_identical_and_a_recorded_stream_replays() {
        let script = |c: &mut DriverCore| {
            let mut poses = Vec::new();
            for frame in 0..180 {
                if frame == 40 {
                    c.set_var("go", VarValue::Bool(true)).expect("var");
                }
                if frame == 100 {
                    c.set_var("go2", VarValue::Bool(true)).expect("var");
                }
                if frame == 70 {
                    c.push_overlay("c", ramp(0.3)).expect("push");
                }
                poses.push(step(c));
            }
            poses
        };
        let (mut one, mut two) = (core(THREE), core(THREE));
        let (a, b) = (script(&mut one), script(&mut two));
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(
                x.position.to_array().map(f32::to_bits),
                y.position.to_array().map(f32::to_bits)
            );
            assert_eq!(x.fov_deg.to_bits(), y.fov_deg.to_bits());
            assert_eq!(x.yaw.to_bits(), y.yaw.to_bits());
        }
    }

    #[test]
    fn steady_state_reuses_its_buffers() {
        let mut c = core(THREE);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        run(&mut c, 120);
        let caps = |c: &DriverCore| {
            (
                c.below.capacity(),
                c.view.names.capacity(),
                c.view.values.capacity(),
                c.instances.capacity(),
                c.instances[0].scratch.trace.capacity(),
                c.instances[0].scratch.states.capacity(),
            )
        };
        let before = caps(&c);
        run(&mut c, 300);
        assert_eq!(before, caps(&c), "no buffer grew after warm-up");
    }

    /// Hand-run microbenchmark (spec 35 § 11; not part of the gate): one driver with two active rigs
    /// of eight nodes each. `cargo test -p regia_framework --lib camera::runtime::tests::perf_two_rigs -- --ignored --nocapture`.
    #[test]
    #[ignore = "hand-run timing measurement"]
    fn perf_two_rigs_of_eight_nodes() {
        let nodes = r#"[
            Pivot(target: Point((0.0, 1.0, 0.0))), LookInput(slot: "look"), Boom(length: Literal(5.0)),
            Collision(radius: 0.25, close_speed: 20.0, open_speed: 4.0), Fov(degrees: Literal(70.0)),
            SmoothPosition(depth: 8.0, side: 8.0, height: 6.0), SmoothRotation(pitch: 9.0, yaw: 9.0, roll: 9.0),
            Lean(gain: 0.1, max: 0.2, speed: 6.0)
        ]"#;
        let text = format!(
            r#"(format_version: 1, name: "perf", variables: [(name: "go", default: Bool(false))],
                inputs: [(name: "look", kind: Axis2, speed: (90.0, 90.0))],
                rigs: [(name: "a", root: {nodes}), (name: "b", root: {nodes})],
                transitions: [(from: Any, to: Any, blend: Linear(100000.0))],
                director: Rules(default: "a", rules: [(when: VarBool("go", true), rig: "b")]))"#
        );
        let mut c = core(&text);
        run(&mut c, 3);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        run(&mut c, 3);
        assert_eq!(
            c.rig_infos().len(),
            2,
            "two rigs stay active through the long blend"
        );
        let frames = 20_000;
        let started = std::time::Instant::now();
        run(&mut c, frames);
        let per_frame = started.elapsed().as_secs_f64() * 1.0e6 / frames as f64;
        eprintln!("camera driver, 2 rigs x 8 nodes: {per_frame:.2} us per frame");
    }

    // ---- Spec 35 conformance additions (lane spec-15-35, ADR-5778) ----

    /// Rig `a` aims at the origin from a camera shifted 3 m sideways, so its aim is not
    /// straight ahead; rig `b` (only a fov) shows its seed unchanged; `c` looks without an aim.
    const POLICIES: &str = r#"(format_version: 1, name: "t",
        inputs: [(name: "look", kind: Axis2, speed: (0.0, 0.0)), (name: "look2", kind: Axis2, speed: (0.0, 0.0))],
        variables: [(name: "to_b", default: Bool(false)), (name: "to_c", default: Bool(false))],
        rigs: [
            (name: "a", root: [Pivot(target: Point((0.0, 0.0, 0.0))), LookInput(slot: "look"), Boom(length: Literal(5.0)),
                Offset(translation: (3.0, 0.0, 0.0), space: World)]),
            (name: "b", root: [Fov(degrees: Literal(50.0))]),
            (name: "c", root: [LookInput(slot: "look2")]),
        ],
        transitions: [(from: Any, to: Any, blend: Linear(1.0), start_facing: FACING)],
        director: Rules(default: "a", rules: [(when: VarBool("to_b", true), rig: "b"), (when: VarBool("to_c", true), rig: "c")]))"#;

    fn b_pose(c: &DriverCore) -> CameraPose {
        c.rig_infos()
            .iter()
            .find(|r| r.name == "b")
            .expect("rig b is active")
            .pose
    }

    /// § 6.2 / § 12.6: the four start-facing policies on a scripted switch give four different,
    /// predictable facings for the incoming rig. `b` ran first (facing yaw 0), then `a` took over
    /// (yaw 0.8, aiming at the origin from a shifted camera); then the switch back to `b`.
    #[test]
    fn start_facing_policies_are_told_apart_on_a_scripted_switch() {
        let switch_back = |policy: &str| {
            let mut c = core(&POLICIES.replace("FACING", policy));
            assert!(c.inputs.set_angles("look", 0.8, 0.2));
            c.set_var("to_b", VarValue::Bool(true)).expect("var");
            run(&mut c, 3);
            c.set_var("to_b", VarValue::Bool(false)).expect("var");
            run(&mut c, 120);
            assert_eq!(c.rig_infos().len(), 1, "the blend to a is over");
            let base = c.pose().expect("pose");
            c.set_var("to_b", VarValue::Bool(true)).expect("var");
            step(&mut c);
            (base, b_pose(&c))
        };
        let (base, own) = switch_back("OwnSeed");
        assert!((base.yaw - 0.8).abs() < 1e-3, "a faces yaw 0.8: {}", base.yaw);
        assert!(
            own.yaw.abs() < 1e-4,
            "own seed: b's remembered facing (0): {}",
            own.yaw
        );
        let (base, carried) = switch_back("CarryOver");
        assert!(
            (carried.yaw - base.yaw).abs() < 1e-4 && (carried.pitch - base.pitch).abs() < 1e-4,
            "carry over: the outgoing facing: {} vs {}",
            carried.yaw,
            base.yaw
        );
        let (base, aimed) = switch_back("ContinueAim");
        let mut want = base;
        want.look_at(Vec3::ZERO);
        assert!(
            (want.yaw - base.yaw).abs() > 0.1,
            "the scene tells ContinueAim from CarryOver"
        );
        assert!(
            (aimed.yaw - want.yaw).abs() < 1e-4 && (aimed.pitch - want.pitch).abs() < 1e-4,
            "continue aim looks at the outgoing aim: {} want {}",
            aimed.yaw,
            want.yaw
        );
        // TargetFacing with no drive target keeps the base facing.
        let (base, target) = switch_back("TargetFacing");
        assert!((target.yaw - base.yaw).abs() < 1e-4);
    }

    /// § 6.2: `ContinueAim` from an outgoing rig without an aim falls back to `OwnSeed`.
    #[test]
    fn continue_aim_without_an_outgoing_aim_falls_back_to_own_seed() {
        let mut c = core(&POLICIES.replace("FACING", "ContinueAim"));
        assert!(c.inputs.set_angles("look2", -0.6, 0.0));
        // b first (yaw 0, no aim), then c (looks to -0.6, sets no aim), then back to b.
        c.set_var("to_b", VarValue::Bool(true)).expect("var");
        run(&mut c, 3);
        c.set_var("to_b", VarValue::Bool(false)).expect("var");
        c.set_var("to_c", VarValue::Bool(true)).expect("var");
        run(&mut c, 120);
        let base = c.pose().expect("pose");
        assert!((base.yaw + 0.6).abs() < 1e-3 && base.aim.is_none(), "{base:?}");
        c.set_var("to_c", VarValue::Bool(false)).expect("var");
        c.set_var("to_b", VarValue::Bool(true)).expect("var");
        step(&mut c);
        let b = b_pose(&c);
        assert!(
            b.yaw.abs() < 1e-4,
            "fell back to b's own seed, not c's facing: {}",
            b.yaw
        );
    }

    /// § 7: an additive layer adds its rotation delta (composed by quaternion) and its fov
    /// delta, not only its position delta.
    #[test]
    fn additive_layers_add_rotation_and_fov_deltas() {
        let mut c = core(
            r#"(format_version: 1, name: "t",
                rigs: [
                    (name: "main", root: [Pivot(target: Point((0.0, 0.0, 0.0))), Fov(degrees: Literal(60.0))]),
                    (name: "tilt", root: [Offset(translation: (0.0, 0.0, 0.0), rotation_deg: (0.0, 20.0, 0.0), space: World), Fov(degrees: Literal(45.0))]),
                    (name: "nod", root: [Offset(translation: (0.0, 0.0, 0.0), rotation_deg: (0.0, 10.0, 0.0), space: World)]),
                ],
                director: Fixed("main"))"#,
        );
        run(&mut c, 1);
        c.add_additive("tilt", 1, None, false).expect("add");
        let pose = run(&mut c, 30);
        assert!(
            (pose.yaw - 20.0f32.to_radians()).abs() < 1e-4,
            "the yaw delta is added: {}",
            pose.yaw
        );
        assert!(
            (pose.fov_deg - 45.0).abs() < 1e-4,
            "the fov delta is added: {}",
            pose.fov_deg
        );
        // A second additive composes on top of the first.
        c.add_additive("nod", 2, None, false).expect("add");
        let pose = run(&mut c, 30);
        assert!(
            (pose.yaw - 30.0f32.to_radians()).abs() < 1e-4,
            "deltas compose: {}",
            pose.yaw
        );
        assert!((pose.fov_deg - 45.0).abs() < 1e-4);
    }

    /// § 7: a layer's context replaces the driver target for the layer's `DriveTarget` nodes.
    #[test]
    fn a_layer_context_replaces_the_drive_target() {
        let mut c = core(
            r#"(format_version: 1, name: "t",
                rigs: [
                    (name: "main", root: [Pivot(target: Point((0.0, 0.0, 0.0)))]),
                    (name: "follow", root: [Pivot(target: DriveTarget)]),
                ],
                director: Fixed("main"))"#,
        );
        run(&mut c, 1);
        c.push_overlay(
            "follow",
            OverlayOptions {
                context: Some(TargetRef::Point([5.0, 1.0, -2.0])),
                ..OverlayOptions::default()
            },
        )
        .expect("push");
        let pose = run(&mut c, 2);
        assert!(
            (pose.position - Vec3::new(5.0, 1.0, -2.0)).length() < 1e-5,
            "the overlay pivots on its context: {:?}",
            pose.position
        );
        // Without a context the overlay's DriveTarget is the driver's (none here): the pivot is
        // skipped and the pose stays where the director put it.
        let mut c = core(
            r#"(format_version: 1, name: "t",
                rigs: [
                    (name: "main", root: [Pivot(target: Point((0.0, 0.0, 0.0)))]),
                    (name: "follow", root: [Pivot(target: DriveTarget)]),
                ],
                director: Fixed("main"))"#,
        );
        run(&mut c, 1);
        c.push_overlay("follow", OverlayOptions::default()).expect("push");
        let pose = run(&mut c, 2);
        assert!(pose.position.length() < 1e-5, "{:?}", pose.position);
    }

    /// § 6.1: extra rigs a director requests join as additive layers with weight 1 and are
    /// released once the director stops asking for them.
    #[test]
    fn director_extra_requests_join_as_additive_layers_and_leave_when_dropped() {
        use crate::camera::director::RigRequest;
        fn pick(ctx: &DirectorContext<'_>) -> Vec<RigRequest> {
            if ctx.vars.get_bool("extra") == Some(true) {
                vec![RigRequest::new("main"), RigRequest::new("lift")]
            } else {
                vec![RigRequest::new("main")]
            }
        }
        let asset = CameraAsset::from_ron(
            r#"(format_version: 1, name: "t",
                variables: [(name: "extra", default: Bool(false))],
                rigs: [
                    (name: "main", root: [Pivot(target: Point((0.0, 0.0, 0.0)))]),
                    (name: "lift", root: [Offset(translation: (0.0, 1.0, 0.0), space: World)]),
                ],
                director: Hook("h"))"#,
        )
        .expect("parses");
        let mut hooks = CameraHooks::default();
        hooks.register("h", pick);
        assert!(
            asset
                .validate(&|name| hooks.contains(name))
                .iter()
                .all(|i| !i.is_error())
        );
        let mut c = DriverCore::new(Arc::new(asset), 7, 0);
        let go = |c: &mut DriverCore, frames: usize| {
            let mut pose = CameraPose::default();
            for _ in 0..frames {
                pose = c.step(DT, &EmptyWorld, &hooks, CameraPose::default());
            }
            pose
        };
        go(&mut c, 2);
        assert!(c.layer_infos().is_empty());
        c.set_var("extra", VarValue::Bool(true)).expect("var");
        let pose = go(&mut c, 2);
        let layers = c.layer_infos();
        assert_eq!(layers.len(), 1);
        assert_eq!((layers[0].rig.as_str(), layers[0].kind), ("lift", "additive"));
        assert_eq!(layers[0].weight, 1.0, "joins with weight 1");
        assert!((pose.position.y - 1.0).abs() < 1e-5, "{:?}", pose.position);
        assert_eq!(c.primary_name(), Some("main"), "the first request stays primary");
        c.set_var("extra", VarValue::Bool(false)).expect("var");
        go(&mut c, 1);
        assert_eq!(c.layer_infos().first().map(|l| l.phase), Some("leaving"));
        let pose = go(&mut c, 60);
        assert!(c.layer_infos().is_empty(), "released and gone");
        assert!(pose.position.y.abs() < 1e-5);
    }

    /// § 11 / § 12.13: a recorded stream of every input the evaluation reads (input values,
    /// variables, the drive target's transform, the physics query result and `dt`) replays to
    /// bit-identical poses in a fresh driver that sees only the recording.
    #[test]
    fn a_recorded_input_variable_and_target_stream_replays_bit_identically() {
        #[derive(Clone, Copy)]
        struct Frame {
            dt: f32,
            look: [f32; 2],
            len: f32,
            target: (Vec3, f32),
            wall: Option<f32>,
        }
        struct Recorded(Frame);
        impl CameraWorld for Recorded {
            fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)> {
                match target {
                    TargetRef::DriveTarget => Some((self.0.target.0, Quat::from_rotation_y(self.0.target.1))),
                    TargetRef::Point(p) => Some((Vec3::from(*p), Quat::IDENTITY)),
                    TargetRef::Entity(_) => None,
                }
            }
            fn cast_sphere(&self, _: Vec3, _: Vec3, max: f32, _: f32, _: &[String]) -> Option<f32> {
                self.0.wall.filter(|d| *d < max)
            }
        }
        const RIG: &str = r#"(format_version: 1, name: "t",
            variables: [(name: "len", default: Float(4.0))],
            inputs: [(name: "look", kind: Axis2, speed: (120.0, 90.0), pitch_clamp: (-70.0, 40.0))],
            rigs: [(name: "a", root: [
                Pivot(target: DriveTarget, offset: (0.0, 1.5, 0.0), space: Target),
                LookInput(slot: "look"),
                Boom(length: Var("len"), side_offset: Literal(0.4), length_speed: Literal(3.0)),
                Collision(radius: 0.25, close_speed: 20.0, open_speed: 4.0),
                SmoothPosition(depth: 8.0, side: 8.0, height: 6.0),
                SmoothRotation(pitch: 10.0, yaw: 12.0, roll: 0.0),
                Lean(gain: 0.1, max: 0.2, speed: 6.0),
                Shake(amplitude: 0.02, frequency: 5.0, decay: 0.0),
            ])],
            director: Fixed("a"))"#;
        // A deterministic pseudo-random stream (64-bit LCG), recorded while it drives the live run.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 40) as f32) / ((1u64 << 24) as f32)
        };
        let hooks = CameraHooks::default();
        let mut live = core(RIG);
        let mut recording = Vec::new();
        let mut live_poses = Vec::new();
        let mut walk = Vec3::ZERO;
        for frame in 0..400 {
            walk += Vec3::new(next() - 0.5, 0.0, next() - 0.5) * 0.2;
            let f = Frame {
                dt: if next() < 0.2 { 1.0 / 30.0 } else { 1.0 / 60.0 },
                look: [next() * 2.0 - 1.0, next() * 2.0 - 1.0],
                len: 3.0 + next() * 3.0,
                target: (walk, frame as f32 * 0.01),
                wall: (next() < 0.3).then(|| 1.0 + next() * 3.0),
            };
            live.set_input("look", f.look).expect("input");
            live.set_var("len", VarValue::Float(f.len)).expect("var");
            live_poses.push(live.step(f.dt, &Recorded(f), &hooks, CameraPose::default()));
            recording.push(f);
        }
        let mut replay = core(RIG);
        for (frame, f) in recording.iter().enumerate() {
            replay.set_input("look", f.look).expect("input");
            replay.set_var("len", VarValue::Float(f.len)).expect("var");
            let pose = replay.step(f.dt, &Recorded(*f), &hooks, CameraPose::default());
            let want = live_poses[frame];
            let bits = |p: &CameraPose| {
                (
                    p.position.to_array().map(f32::to_bits),
                    [p.yaw, p.pitch, p.roll, p.fov_deg].map(f32::to_bits),
                    p.cut,
                )
            };
            assert_eq!(bits(&pose), bits(&want), "frame {frame}");
        }
        assert!(live_poses.iter().all(CameraPose::is_finite));
        let moved = live_poses[399].position.distance(live_poses[0].position);
        assert!(moved > 0.5, "the stream exercised the rig: moved {moved}");
    }

    /// § 12.5 at its literal bound: interrupting a running blend starts the new blend from the
    /// current blended pose, so on the interrupting frame the pose moves by at most 1e-3 m (the
    /// interrupting transition is a 10 s smoothstep, whose first-frame weight is about 8e-6; a pop
    /// back to a rig's own pose would be metres). The old blend's rigs keep their mix and fade.
    #[test]
    fn an_interruption_moves_the_pose_less_than_a_millimetre_on_its_frame() {
        let text = THREE.replace(
            r#"(name: "c", root: [Pivot(target: Point((-10.0, 0.0, 0.0))), Fov(degrees: Literal(60.0))]),"#,
            r#"(name: "c", root: [Pivot(target: Point((-10.0, 0.0, 0.0))), Fov(degrees: Literal(60.0))],
                transitions: [(from: Any, to: Rig("c"), blend: Smooth(10.0))]),"#,
        );
        assert_ne!(text, THREE, "the incoming rig has its own transition");
        let mut c = core(&text);
        step(&mut c);
        c.set_var("go", VarValue::Bool(true)).expect("var");
        let mut previous = run(&mut c, 20).position.x;
        assert!(previous > 2.0 && previous < 5.0, "mid-blend toward b: {previous}");
        c.set_var("go2", VarValue::Bool(true)).expect("var");
        let x = step(&mut c).position.x;
        assert!(
            (x - previous).abs() <= 1.0e-3,
            "pose continuity on the interrupting frame: {previous} -> {x}"
        );
        assert_eq!(c.primary_name(), Some("c"));
        let total: f32 = c.rig_infos().iter().map(|r| r.weight).sum();
        assert!((total - 1.0).abs() < 1.0e-5, "weights still sum to 1: {total}");
        // The new blend then carries the pose on toward c, monotonically.
        for _ in 0..60 {
            let next = step(&mut c).position.x;
            assert!(
                next <= previous + 1.0e-6,
                "monotonic toward c: {previous} -> {next}"
            );
            previous = next;
        }
    }
}

/// audit-v035-numerics (fix-audit-wave12, ADR-2395) M5 and L5.
#[cfg(test)]
mod audit_v035_tests {
    use super::*;
    use crate::camera::nodes::EmptyWorld;

    const DT: f32 = 1.0 / 60.0;

    const ONE: &str = r#"(format_version: 1, name: "t",
        rigs: [(name: "a", root: [Pivot(target: Point((0.0, 0.0, 0.0))), Fov(degrees: Literal(40.0))])],
        director: Fixed("a"))"#;

    fn core() -> DriverCore {
        let asset = CameraAsset::from_ron(ONE).expect("test asset parses");
        DriverCore::new(Arc::new(asset), 7, 0)
    }

    /// M5: a NaN seed on the first frame was restored by every node and re-stored every frame,
    /// so the driven camera stayed NaN for the rest of the session.
    #[test]
    fn a_nan_seed_pose_does_not_latch() {
        let mut c = core();
        let nan = CameraPose {
            position: Vec3::NAN,
            yaw: f32::NAN,
            ..CameraPose::default()
        };
        let first = c.step(DT, &EmptyWorld, &CameraHooks::default(), nan);
        assert!(first.is_finite(), "first frame: {first:?}");
        let mut pose = first;
        for _ in 0..10 {
            pose = c.step(DT, &EmptyWorld, &CameraHooks::default(), CameraPose::default());
        }
        assert!(pose.is_finite(), "later frames: {pose:?}");
        assert!(c.instances.iter().all(|i| i.pose.is_finite()));
    }

    /// L5: the driver clock and the instance clock were f32 seconds; at 2^24 s a 60 Hz step
    /// no longer advanced them, so elapsed-driven nodes and transitions stopped.
    #[test]
    fn the_clocks_keep_advancing_in_a_long_session() {
        let mut c = core();
        c.step(DT, &EmptyWorld, &CameraHooks::default(), CameraPose::default());
        c.time = 16_777_216.0;
        c.instances[0].elapsed = 16_777_216.0;
        c.step(DT, &EmptyWorld, &CameraHooks::default(), CameraPose::default());
        assert!(c.time > 16_777_216.0, "driver clock stuck at {}", c.time);
        assert!(
            c.instances[0].elapsed > 16_777_216.0,
            "instance clock stuck at {}",
            c.instances[0].elapsed
        );
    }
}
