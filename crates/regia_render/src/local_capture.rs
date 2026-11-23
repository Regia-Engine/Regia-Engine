// SPDX-License-Identifier: MPL-2.0
//! Local reflection captures (ADR-2000): an HDR reflection of what stands around a point, rendered
//! once or on demand and read by the forward pass's image-based specular, without a GI probe
//! volume and on every quality tier that draws the image light.
//!
//! A capture is placed by its box ([`LocalCaptureDesc`]) and baked by
//! [`crate::FrameRenderer::bake_local_capture`]: six linear HDR cube faces rendered from the box's
//! centre, packed into an equirect image and GGX-filtered with the same chain as a studio
//! environment. What a face saw of the world (not of the environment) is the coverage, the map's
//! alpha, so a mirror keeps the sharp environment where the capture only saw sky, and a rough lobe
//! that is half surface blends half with it. The lookup is parallax-corrected on the box (Lagarde,
//! "Local Image-Based Lighting With Parallax-Corrected Cubemaps", SIGGRAPH 2012).
//!
//! The older [`crate::ReflectionProbe`]s are a coarse 64 × 32 display-referred image the screen
//! reflection passes blend in on the tiers that run them. Where a capture's box covers a point,
//! a screen reflection that misses defers to the capture instead of the probe (ADR-2860).
//!
//! Captures are level content (ADR-2860): [`LocalCapture`] is reflected and saved in levels, a
//! level's bake is its `.fcap` side file (`regia_asset::local_capture_bake`) installed through
//! [`LocalCaptureBakes`] with no face rendered, and a renderer holds up to
//! [`MAX_LOCAL_CAPTURES`] of them, of which a frame reads the [`local_capture_budget`] nearest
//! boxes with hysteresis, fading a swap so a slot change does not pop.

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_scene::GlobalTransform;

use crate::reflections::{cube_face_axes, project_to_face};
use crate::studio_env::{CaptureImage, capture_chain, capture_mips};

/// Captures a renderer holds at once (placed, baked or not). Only [`LOCAL_CAPTURE_LAYERS`] of
/// them are read by a frame: the selection picks the nearest boxes within the tier's budget
/// ([`local_capture_budget`], ADR-2860).
pub const MAX_LOCAL_CAPTURES: usize = 32;
/// Captures a frame reads at once: the array layers of the capture texture.
pub const LOCAL_CAPTURE_LAYERS: usize = 4;
/// Frames a capture takes to fade in or out when the selection swaps it (no pop).
pub const LOCAL_CAPTURE_FADE_FRAMES: u32 = 8;
/// How much nearer (meters, distance from the eye to the box) a capture must be than a selected
/// one to take its place, so an eye on the boundary does not swap them back and forth.
pub const LOCAL_CAPTURE_HYSTERESIS_M: f32 = 0.75;
/// Edge of a rendered cube face, texels.
pub const LOCAL_CAPTURE_FACE: u32 = 256;
/// Width of the equirect map (its height is half of it).
pub const LOCAL_CAPTURE_WIDTH: u32 = 512;
/// A face texel at or beyond this depth saw the environment, not a surface.
const FAR_DEPTH: f32 = 1.0 - 1e-6;

/// The captures a frame of `quality` reads at once (ADR-2860): 4 when `max_reflection_probes` is
/// outside `0..=3` (Desktop High, Ultra, and Medium, which copies Desktop High's probe count),
/// 3 when it is 2 or 3, 2 when it is 0 or 1 (Mobile Low, the lab and the editor qualities).
/// Never more than [`LOCAL_CAPTURE_LAYERS`].
#[must_use]
pub fn local_capture_budget(quality: &crate::FrameQuality) -> usize {
    match quality.max_reflection_probes {
        0 | 1 => 2,
        2 | 3 => 3,
        _ => LOCAL_CAPTURE_LAYERS,
    }
}

/// Mips of a capture's filtered chain (what a [`LocalCaptureBake`] must hold).
#[must_use]
pub fn local_capture_mip_count() -> u32 {
    capture_mips(LOCAL_CAPTURE_WIDTH)
}

/// A capture's baked map, kept on the CPU so a slot change, a save and a load need no render:
/// the GGX-filtered equirect chain as the renderer uploads it (`Rgba16Float`, premultiplied
/// radiance in rgb, surface coverage in alpha, mip 0 first).
#[derive(Clone, Debug, PartialEq)]
pub struct LocalCaptureBake {
    /// Width of mip 0 ([`LOCAL_CAPTURE_WIDTH`]).
    pub width: u32,
    /// Share of the capture's directions that saw a surface, 0..1.
    pub coverage: f32,
    /// The mips, 8 bytes per texel.
    pub mips: Vec<Vec<u8>>,
}

impl LocalCaptureBake {
    /// Checks the chain fits the renderer's capture texture.
    ///
    /// # Errors
    /// [`LocalCaptureError::BadValue`] for a wrong width, mip count or mip size.
    pub fn validate(&self) -> Result<(), LocalCaptureError> {
        if self.width != LOCAL_CAPTURE_WIDTH {
            return Err(LocalCaptureError::BadValue("bake width must be 512"));
        }
        if self.mips.len() != capture_mips(LOCAL_CAPTURE_WIDTH) as usize {
            return Err(LocalCaptureError::BadValue("bake mip count does not fit"));
        }
        for (mip, bytes) in self.mips.iter().enumerate() {
            let width = (LOCAL_CAPTURE_WIDTH >> mip).max(1) as usize;
            let height = ((LOCAL_CAPTURE_WIDTH / 2) >> mip).max(1) as usize;
            if bytes.len() != width * height * 8 {
                return Err(LocalCaptureError::BadValue("bake mip size does not fit"));
            }
        }
        if !(self.coverage.is_finite() && (0.0..=1.0).contains(&self.coverage)) {
            return Err(LocalCaptureError::BadValue("bake coverage must be within 0 to 1"));
        }
        Ok(())
    }
}

/// Where a capture sits and how it blends.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalCaptureDesc {
    /// Stable id.
    pub id: u64,
    /// World centre of the box, which is also the capture point.
    pub center: [f32; 3],
    /// Half extents of the parallax box, meters: the walls or the space the reflection stands in.
    pub half_extent: [f32; 3],
    /// Blend weight against the environment, 0..1.
    pub weight: f32,
    /// How far outside the box the capture fades out, meters.
    pub fade: f32,
    /// A bake to install with the placement (a `.fcap` read at level open, ADR-2860): frames
    /// read it without rendering a face. `None` places the capture unbaked. In
    /// [`LocalCaptureInfo::desc`] it is the capture's current bake.
    pub bake: Option<Arc<LocalCaptureBake>>,
}

impl LocalCaptureDesc {
    /// A capture at `center` with a box of `half_extent`, full weight, fading over a quarter of
    /// the box's largest half extent.
    #[must_use]
    pub fn new(id: u64, center: [f32; 3], half_extent: [f32; 3]) -> Self {
        Self {
            id,
            center,
            half_extent,
            weight: 1.0,
            fade: 0.25 * half_extent[0].max(half_extent[1]).max(half_extent[2]),
            bake: None,
        }
    }

    /// Checks the values.
    ///
    /// # Errors
    /// [`LocalCaptureError::BadValue`] names the first bad value.
    pub fn validate(&self) -> Result<(), LocalCaptureError> {
        if !self.center.iter().all(|c| c.is_finite()) {
            return Err(LocalCaptureError::BadValue("center must be finite"));
        }
        if !self.half_extent.iter().all(|c| c.is_finite() && *c > 0.0) {
            return Err(LocalCaptureError::BadValue("half_extent must be finite and > 0"));
        }
        if !(self.weight.is_finite() && (0.0..=1.0).contains(&self.weight)) {
            return Err(LocalCaptureError::BadValue("weight must be within 0 to 1"));
        }
        if !(self.fade.is_finite() && self.fade >= 0.0) {
            return Err(LocalCaptureError::BadValue("fade must be finite and >= 0"));
        }
        if let Some(bake) = &self.bake {
            bake.validate()?;
        }
        Ok(())
    }

    /// Distance from `eye` to the box, meters (0 inside it).
    #[must_use]
    pub fn distance_to(&self, eye: [f32; 3]) -> f32 {
        let mut sum = 0.0f32;
        for ((e, c), h) in eye.iter().zip(&self.center).zip(&self.half_extent) {
            let outside = ((e - c).abs() - h).max(0.0);
            sum += outside * outside;
        }
        sum.sqrt()
    }
}

/// A capture on an entity: the capture point is the entity's world position. The runtime places
/// it and queues a bake when it appears or moves, and removes it with the component.
///
/// Reflected and registered by [`crate::register_render_types`] (ADR-2860): a level or prefab
/// saves it on the entity and loads it back. A level's bake is its `.fcap` side file.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct LocalCapture {
    /// Stable id, shared with the agent protocol's `render.*_local_capture` methods and the
    /// level's `.fcap`.
    pub id: u64,
    /// Half extents of the parallax box, meters.
    pub half_extent: [f32; 3],
    /// Blend weight against the environment, 0..1.
    pub weight: f32,
    /// How far outside the box the capture fades out, meters.
    pub fade: f32,
}

impl Default for LocalCapture {
    /// Id 0, a 2 m box (1 m half extents).
    fn default() -> Self {
        Self::new(0, [1.0; 3])
    }
}

impl LocalCapture {
    /// A capture of `half_extent`, full weight, with the default fade.
    #[must_use]
    pub fn new(id: u64, half_extent: [f32; 3]) -> Self {
        let desc = LocalCaptureDesc::new(id, [0.0; 3], half_extent);
        Self {
            id,
            half_extent,
            weight: desc.weight,
            fade: desc.fade,
        }
    }

    /// The placement of this capture at `center`, unbaked.
    #[must_use]
    pub fn desc(&self, center: [f32; 3]) -> LocalCaptureDesc {
        LocalCaptureDesc {
            id: self.id,
            center,
            half_extent: self.half_extent,
            weight: self.weight,
            fade: self.fade,
            bake: None,
        }
    }
}

/// Bakes the runtime installs from a level's `.fcap` instead of rendering (ADR-2860), by capture
/// id. [`sync_local_captures`] places a capture with its bake when the bake was made at the
/// capture's position (within 1 mm); otherwise it queues a render as before. A host or the
/// level plugin fills it; an empty resource changes nothing.
#[derive(Resource, Clone, Debug, Default)]
pub struct LocalCaptureBakes {
    /// The bakes by capture id, with the centre each was rendered from.
    pub by_id: HashMap<u64, ([f32; 3], Arc<LocalCaptureBake>)>,
    /// Bumped by the writer when the set changes, so placed captures take the new bakes.
    pub generation: u64,
}

impl LocalCaptureBakes {
    /// The bake for capture `id` placed at `center`, when one was made there.
    #[must_use]
    pub fn matching(&self, id: u64, center: [f32; 3]) -> Option<Arc<LocalCaptureBake>> {
        let (at, bake) = self.by_id.get(&id)?;
        let near = at.iter().zip(center).all(|(a, b)| (a - b).abs() <= 1e-3);
        near.then(|| bake.clone())
    }
}

/// The captures whose component or transform changed since the system last ran.
type ChangedCaptures<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static LocalCapture, &'static GlobalTransform),
    Or<(Changed<LocalCapture>, Changed<GlobalTransform>)>,
>;

/// Every capture, for a re-place when [`LocalCaptureBakes`] changes.
type AllCaptures<'w, 's> = Query<'w, 's, (Entity, &'static LocalCapture, &'static GlobalTransform)>;

/// `Render` system: mirrors the [`LocalCapture`] components onto the runtime renderer. A capture
/// with a matching bake in [`LocalCaptureBakes`] is placed with it (no faces rendered); any other
/// is placed and its bake queued.
pub fn sync_local_captures(
    changed: ChangedCaptures<'_, '_>,
    all: AllCaptures<'_, '_>,
    mut removed: RemovedComponents<LocalCapture>,
    mut ids: Local<HashMap<Entity, u64>>,
    mut seen_generation: Local<Option<u64>>,
    bakes: Option<Res<LocalCaptureBakes>>,
    runtime: Option<ResMut<crate::RuntimeRenderer>>,
) {
    let _span = tracing::debug_span!("sync_local_captures").entered();
    let Some(mut runtime) = runtime else {
        return;
    };
    for entity in removed.read() {
        if let Some(id) = ids.remove(&entity)
            // Another entity may still hold this id: only drop the slot with its last user.
            && !ids.values().any(|live| *live == id)
        {
            runtime.0.remove_local_capture(id);
        }
    }
    let generation = bakes.as_ref().map(|b| b.generation);
    let bakes_changed = generation.is_some() && *seen_generation != generation;
    *seen_generation = generation;
    let mut place = |entity: Entity, capture: &LocalCapture, transform: &GlobalTransform| {
        // An id owned by another entity would move that entity's capture: refuse the duplicate.
        if ids
            .iter()
            .any(|(other, id)| *other != entity && *id == capture.id)
        {
            tracing::warn!(
                target: "regia_render",
                id = capture.id,
                ?entity,
                "local capture id is already used by another entity"
            );
            return;
        }
        let center: [f32; 3] = transform.0.translation.into();
        let mut desc = capture.desc(center);
        desc.bake = bakes.as_ref().and_then(|b| b.matching(capture.id, center));
        let baked = desc.bake.is_some();
        // The id changed on this entity: drop the old capture once no other entity still uses
        // it, before placing the new one, so a renamed capture does not need a free slot (with
        // all MAX_LOCAL_CAPTURES slots used the rename was refused and the old capture stayed).
        if let Some(previous) = ids.get(&entity).copied()
            && previous != capture.id
        {
            if !ids.iter().any(|(other, id)| *other != entity && *id == previous) {
                runtime.0.remove_local_capture(previous);
            }
            ids.remove(&entity);
        }
        if let Err(err) = runtime.0.place_local_capture(desc) {
            tracing::warn!(target: "regia_render", %err, id = capture.id, "local capture refused");
            return;
        }
        ids.insert(entity, capture.id);
        if !baked {
            // The id was just placed, so this cannot name an unknown capture.
            let _ = runtime.0.request_local_capture_bake(Some(capture.id));
        }
    };
    if bakes_changed {
        for (entity, capture, transform) in &all {
            place(entity, capture, transform);
        }
    } else {
        for (entity, capture, transform) in &changed {
            place(entity, capture, transform);
        }
    }
}

/// Why a capture call was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LocalCaptureError {
    /// A parameter is out of range.
    #[error("{0}")]
    BadValue(&'static str),
    /// Every slot is taken.
    #[error("at most 32 local captures: remove one first")]
    Full,
    /// No capture has this id.
    #[error("no local capture {0}")]
    Unknown(u64),
}

/// One capture as [`crate::FrameRenderer::local_captures`] lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalCaptureInfo {
    /// The placement, with the current bake in [`LocalCaptureDesc::bake`].
    pub desc: LocalCaptureDesc,
    /// It has a bake (rendered, or installed from a `.fcap`).
    pub baked: bool,
    /// A bake is queued for the next frame of the runtime renderer.
    pub pending: bool,
    /// Bakes so far (renders and installs).
    pub bakes: u32,
    /// Share of the capture's directions that saw a surface, 0..1 (0 before a bake).
    pub coverage: f32,
    /// The array layer frames read it from, while selected.
    pub layer: Option<u32>,
    /// Its weight in frames now: [`LocalCaptureDesc::weight`] times the selection's fade
    /// (0 while not read).
    pub blend: f32,
    /// Times its map was written to a layer.
    pub uploads: u32,
}

struct Slot {
    desc: LocalCaptureDesc,
    pending: bool,
    bakes: u32,
    uploads: u32,
}

/// A layer of the capture texture and the capture it holds.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layer {
    id: u64,
    /// Fade, 0..1.
    fade: f32,
    /// Where the fade goes: 1 selected, 0 leaving.
    target: f32,
    /// Its map is in the layer.
    uploaded: bool,
}

/// A layer write [`LocalCaptures::plan`] asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "called by the frame hookup, which ships as a patch under the wave's frame rule (ADR-2860 § 5)"
)]
pub(crate) struct LayerWrite {
    pub(crate) layer: usize,
    pub(crate) id: u64,
}

/// What one [`LocalCaptures::prepare`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "called by the frame hookup, which ships as a patch under the wave's frame rule (ADR-2860 § 5)"
)]
pub(crate) struct Prepared {
    /// The array texture was created (the view groups must rebuild).
    pub(crate) created: bool,
    /// Any weight, layer or map changed (a cached screen must go).
    pub(crate) changed: bool,
}

/// The renderer's captures and their array texture.
///
/// Until the first [`Self::prepare`] (the frame's selection, ADR-2860) slot `i` is layer `i`, as
/// before the selection existed; from it on, layers hold the selected captures.
#[derive(Default)]
#[allow(
    dead_code,
    reason = "called by the frame hookup, which ships as a patch under the wave's frame rule (ADR-2860 § 5)"
)]
pub(crate) struct LocalCaptures {
    slots: Vec<Slot>,
    layers: [Option<Layer>; LOCAL_CAPTURE_LAYERS],
    selection: bool,
    /// Captures that won a swap and wait for a free layer: they fade in when they get one.
    waiting: Vec<u64>,
    texture: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// Array textures created (one per renderer; a test counter).
    pub(crate) textures_created: u32,
    /// Selection swaps so far (a test counter).
    pub(crate) swaps: u32,
}

impl LocalCaptures {
    pub(crate) fn view(&self) -> Option<&wgpu::TextureView> {
        self.texture.as_ref().map(|(_, view)| view)
    }

    /// Array layers frames loop over: up to the last layer that holds a map (a layer without one
    /// weighs 0).
    pub(crate) fn layer_count(&self) -> usize {
        self.layers
            .iter()
            .rposition(|l| l.is_some_and(|l| l.uploaded))
            .map_or(0, |i| i + 1)
    }

    pub(crate) fn top_mip(&self) -> f32 {
        capture_mips(LOCAL_CAPTURE_WIDTH).saturating_sub(1) as f32
    }

    fn slot(&self, id: u64) -> Option<&Slot> {
        self.slots.iter().find(|s| s.desc.id == id)
    }

    /// One entry per layer; a layer without a map stays zero (weight 0).
    fn uniform(&self, f: impl Fn(&Slot, f32) -> [f32; 4]) -> [[f32; 4]; 4] {
        let mut out = [[0.0; 4]; 4];
        for (o, layer) in out.iter_mut().zip(&self.layers) {
            if let Some(layer) = layer.filter(|l| l.uploaded)
                && let Some(slot) = self.slot(layer.id)
            {
                *o = f(slot, layer.fade);
            }
        }
        out
    }

    pub(crate) fn uniform_centers(&self) -> [[f32; 4]; 4] {
        self.uniform(|s, fade| {
            let [x, y, z] = s.desc.center;
            // Exact for a settled layer (fade 1): the weight as before the selection.
            let weight = if fade >= 1.0 {
                s.desc.weight
            } else {
                s.desc.weight * fade
            };
            [x, y, z, weight]
        })
    }

    pub(crate) fn uniform_halves(&self) -> [[f32; 4]; 4] {
        self.uniform(|s, _| {
            let [x, y, z] = s.desc.half_extent;
            [x, y, z, s.desc.fade]
        })
    }

    pub(crate) fn place(&mut self, desc: LocalCaptureDesc) -> Result<(), LocalCaptureError> {
        desc.validate()?;
        let installs = desc.bake.is_some();
        if let Some(index) = self.slots.iter().position(|s| s.desc.id == desc.id) {
            let slot = &mut self.slots[index];
            // A move keeps the old map until the bake the caller queues replaces it.
            let keep = if installs { None } else { slot.desc.bake.take() };
            slot.desc = desc;
            if keep.is_some() {
                slot.desc.bake = keep;
            }
            if installs {
                slot.bakes += 1;
                slot.pending = false;
                self.mark_stale(index);
            }
            return Ok(());
        }
        if self.slots.len() >= MAX_LOCAL_CAPTURES {
            return Err(LocalCaptureError::Full);
        }
        self.slots.push(Slot {
            desc,
            pending: false,
            bakes: u32::from(installs),
            uploads: 0,
        });
        if !self.selection && self.slots.len() <= LOCAL_CAPTURE_LAYERS {
            let index = self.slots.len() - 1;
            self.layers[index] = Some(Layer {
                id: self.slots[index].desc.id,
                fade: 1.0,
                target: 1.0,
                uploaded: false,
            });
        }
        Ok(())
    }

    /// The layer of slot `index`'s capture no longer holds its current map.
    fn mark_stale(&mut self, index: usize) {
        let id = self.slots[index].desc.id;
        for layer in self.layers.iter_mut().flatten() {
            if layer.id == id {
                layer.uploaded = false;
            }
        }
    }

    pub(crate) fn remove(&mut self, id: u64) -> bool {
        let Some(index) = self.slots.iter().position(|s| s.desc.id == id) else {
            return false;
        };
        self.slots.remove(index);
        if self.selection {
            // The selection keeps every other layer; the next frame fills the free one.
            for layer in &mut self.layers {
                if layer.is_some_and(|l| l.id == id) {
                    *layer = None;
                }
            }
            return true;
        }
        // Before the selection a slot is its layer, and the slots after this one move down:
        // what was in a layer bakes again (the runtime's next frame).
        self.layers = [None; LOCAL_CAPTURE_LAYERS];
        for (i, slot) in self.slots.iter_mut().enumerate() {
            slot.pending = slot.pending || slot.bakes > 0;
            if i < LOCAL_CAPTURE_LAYERS {
                self.layers[i] = Some(Layer {
                    id: slot.desc.id,
                    fade: 1.0,
                    target: 1.0,
                    uploaded: false,
                });
            }
        }
        true
    }

    pub(crate) fn request_bake(&mut self, id: Option<u64>) -> Result<usize, LocalCaptureError> {
        let mut queued = 0;
        for slot in &mut self.slots {
            if id.is_none_or(|id| slot.desc.id == id) {
                slot.pending = true;
                queued += 1;
            }
        }
        if let (Some(id), 0) = (id, queued) {
            return Err(LocalCaptureError::Unknown(id));
        }
        Ok(queued)
    }

    pub(crate) fn clear_pending(&mut self, id: u64) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.desc.id == id) {
            slot.pending = false;
        }
    }

    pub(crate) fn pending(&self) -> Vec<u64> {
        self.slots
            .iter()
            .filter(|s| s.pending)
            .map(|s| s.desc.id)
            .collect()
    }

    pub(crate) fn desc(&self, id: u64) -> Option<LocalCaptureDesc> {
        self.slot(id).map(|s| s.desc.clone())
    }

    pub(crate) fn infos(&self) -> Vec<LocalCaptureInfo> {
        self.slots
            .iter()
            .map(|s| {
                let layer = self
                    .layers
                    .iter()
                    .position(|l| l.is_some_and(|l| l.id == s.desc.id && l.uploaded));
                let fade = layer.and_then(|i| self.layers[i]).map_or(0.0, |l| l.fade);
                LocalCaptureInfo {
                    desc: s.desc.clone(),
                    baked: s.desc.bake.is_some(),
                    pending: s.pending,
                    bakes: s.bakes,
                    coverage: s.desc.bake.as_ref().map_or(0.0, |b| b.coverage),
                    layer: layer.map(|i| i as u32),
                    blend: s.desc.weight * fade,
                    uploads: s.uploads,
                }
            })
            .collect()
    }

    /// The frame's selection (ADR-2860), without GPU work: picks the `budget` captures nearest
    /// to `eye` (by distance to their boxes; slot order without an eye) among the baked ones,
    /// keeps a selected capture until another is nearer by [`LOCAL_CAPTURE_HYSTERESIS_M`], fades
    /// a swap over [`LOCAL_CAPTURE_FADE_FRAMES`] frames (a capture that fills a free place with
    /// nothing leaving shows at once), and returns the layer writes it needs: none on a frame
    /// where nothing changed.
    #[allow(
        dead_code,
        reason = "called by the frame hookup, which ships as a patch under the wave's frame rule (ADR-2860 § 5)"
    )]
    pub(crate) fn plan(&mut self, eye: Option<[f32; 3]>, budget: usize) -> (Vec<LayerWrite>, bool) {
        let budget = budget.clamp(1, LOCAL_CAPTURE_LAYERS);
        if !self.selection {
            // From slot-is-layer to the selection: a layer that never showed a map holds nothing to
            // fade from, so the selection starts without it.
            for layer in &mut self.layers {
                if layer.is_some_and(|l| !l.uploaded) {
                    *layer = None;
                }
            }
        }
        self.selection = true;
        // No capture: nothing to select and no allocation (steady frames allocate nothing).
        if self.slots.is_empty() && self.layers.iter().all(Option::is_none) {
            return (Vec::new(), false);
        }
        let mut changed = false;
        // Layers of removed or unbaked captures go at once.
        for layer in &mut self.layers {
            if let Some(l) = layer {
                let baked = self
                    .slots
                    .iter()
                    .any(|s| s.desc.id == l.id && s.desc.bake.is_some());
                if !baked {
                    *layer = None;
                    changed = true;
                }
            }
        }
        let score = |slots: &[Slot], id: u64| -> f32 {
            let Some(i) = slots.iter().position(|s| s.desc.id == id) else {
                return f32::INFINITY;
            };
            match eye {
                Some(eye) => slots[i].desc.distance_to(eye),
                None => i as f32,
            }
        };
        let slot_index =
            |slots: &[Slot], id: u64| slots.iter().position(|s| s.desc.id == id).unwrap_or(usize::MAX);
        // The selected set: layers heading in.
        let mut selected: Vec<u64> = self
            .layers
            .iter()
            .flatten()
            .filter(|l| l.target > 0.0)
            .map(|l| l.id)
            .collect();
        // A smaller budget drops the farthest first.
        selected.sort_by(|a, b| {
            score(&self.slots, *a)
                .total_cmp(&score(&self.slots, *b))
                .then(slot_index(&self.slots, *a).cmp(&slot_index(&self.slots, *b)))
        });
        let mut leaving: Vec<u64> = selected.split_off(selected.len().min(budget));
        let mut entering: Vec<u64> = Vec::new();
        let mut candidates: Vec<u64> = self
            .slots
            .iter()
            .filter(|s| s.desc.bake.is_some() && !selected.contains(&s.desc.id))
            .map(|s| s.desc.id)
            .collect();
        candidates.sort_by(|a, b| {
            score(&self.slots, *a)
                .total_cmp(&score(&self.slots, *b))
                .then(slot_index(&self.slots, *a).cmp(&slot_index(&self.slots, *b)))
        });
        let margin = if eye.is_some() {
            LOCAL_CAPTURE_HYSTERESIS_M
        } else {
            0.0
        };
        for id in candidates {
            if leaving.contains(&id) {
                continue;
            }
            if selected.len() < budget {
                selected.push(id);
                entering.push(id);
                continue;
            }
            // The farthest selected capture.
            let Some((worst_at, worst)) = selected
                .iter()
                .enumerate()
                .map(|(i, id)| (i, score(&self.slots, *id)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
            else {
                break;
            };
            if score(&self.slots, id) + margin < worst {
                leaving.push(selected[worst_at]);
                selected[worst_at] = id;
                entering.push(id);
                self.swaps += 1;
            } else {
                break;
            }
        }
        // A leaving capture that never showed goes at once; one that showed fades out, and the
        // captures entering in its place fade in.
        let mut swapping = false;
        for layer in &mut self.layers {
            if let Some(l) = layer
                && leaving.contains(&l.id)
                && l.target > 0.0
            {
                changed = true;
                if l.uploaded {
                    l.target = 0.0;
                    swapping = true;
                } else {
                    *layer = None;
                }
            }
        }
        // Entering captures take free layers; one with no free layer waits for a leaving one.
        let step = 1.0 / LOCAL_CAPTURE_FADE_FRAMES as f32;
        for id in entering {
            if let Some(layer) = self.layers.iter_mut().find(|l| l.is_some_and(|l| l.id == id)) {
                // It was leaving: it turns back.
                if let Some(l) = layer {
                    l.target = 1.0;
                }
                changed = true;
                continue;
            }
            let won_a_swap = swapping || self.waiting.contains(&id);
            if let Some(free) = self.layers.iter_mut().find(|l| l.is_none()) {
                self.waiting.retain(|w| *w != id);
                let fade = if won_a_swap { 0.0 } else { 1.0 };
                *free = Some(Layer {
                    id,
                    fade,
                    target: 1.0,
                    uploaded: false,
                });
                changed = true;
            } else if won_a_swap && !self.waiting.contains(&id) {
                self.waiting.push(id);
            }
        }
        self.waiting.retain(|id| {
            self.slots
                .iter()
                .any(|s| s.desc.id == *id && s.desc.bake.is_some())
        });
        // Fades move one step; a faded-out layer frees.
        for layer in &mut self.layers {
            if let Some(l) = layer {
                if l.fade < l.target {
                    l.fade = (l.fade + step).min(1.0);
                    changed = true;
                } else if l.fade > l.target {
                    l.fade = (l.fade - step).max(0.0);
                    changed = true;
                }
                if l.target <= 0.0 && l.fade <= 0.0 {
                    *layer = None;
                }
            }
        }
        let writes = self
            .layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                l.filter(|l| !l.uploaded)
                    .map(|l| LayerWrite { layer: i, id: l.id })
            })
            .collect::<Vec<_>>();
        changed |= !writes.is_empty();
        (writes, changed)
    }

    /// The frame's selection ([`Self::plan`]) and its layer writes. A frame where nothing moved
    /// writes nothing and creates nothing.
    #[allow(
        dead_code,
        reason = "called by the frame hookup, which ships as a patch under the wave's frame rule (ADR-2860 § 5)"
    )]
    pub(crate) fn prepare(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        eye: Option<[f32; 3]>,
        budget: usize,
    ) -> Prepared {
        let _span = tracing::trace_span!("local_capture_selection").entered();
        let (writes, changed) = self.plan(eye, budget);
        let mut created = false;
        for write in writes {
            created |= self.upload(d, q, write.layer, write.id);
        }
        Prepared { created, changed }
    }

    fn ensure_texture(&mut self, d: &wgpu::Device) -> bool {
        if self.texture.is_some() {
            return false;
        }
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("local captures"),
            size: wgpu::Extent3d {
                width: LOCAL_CAPTURE_WIDTH,
                height: LOCAL_CAPTURE_WIDTH / 2,
                depth_or_array_layers: LOCAL_CAPTURE_LAYERS as u32,
            },
            mip_level_count: capture_mips(LOCAL_CAPTURE_WIDTH),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        self.texture = Some((texture, view));
        self.textures_created += 1;
        true
    }

    /// Writes capture `id`'s bake into `layer`. Returns whether the array texture was created.
    fn upload(&mut self, d: &wgpu::Device, q: &crate::device::TrackedQueue, layer: usize, id: u64) -> bool {
        let created = self.ensure_texture(d);
        let Some(index) = self.slots.iter().position(|s| s.desc.id == id) else {
            return created;
        };
        let Some(bake) = self.slots[index].desc.bake.clone() else {
            return created;
        };
        let Some((texture, _)) = self.texture.as_ref() else {
            return created;
        };
        for (mip, bytes) in bake.mips.iter().enumerate() {
            let width = (LOCAL_CAPTURE_WIDTH >> mip).max(1);
            let height = ((LOCAL_CAPTURE_WIDTH / 2) >> mip).max(1);
            let (padded, bytes_per_row) = crate::outdoor::pad_rows_bytes(bytes, width * 8, height);
            q.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: mip as u32,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &padded,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.slots[index].uploads += 1;
        if let Some(l) = self.layers[layer].as_mut() {
            l.uploaded = true;
        }
        created
    }

    /// Keeps a rendered bake of `id` and writes it into its layer when it has one. Returns
    /// whether the array texture was created (the view groups must rebuild).
    pub(crate) fn store(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        id: u64,
        chain: &[Vec<u8>],
        coverage: f32,
    ) -> bool {
        // As before the selection, the texture exists once anything was baked.
        let mut created = self.ensure_texture(d);
        let Some(index) = self.slots.iter().position(|s| s.desc.id == id) else {
            return created;
        };
        let slot = &mut self.slots[index];
        slot.pending = false;
        slot.bakes += 1;
        slot.desc.bake = Some(Arc::new(LocalCaptureBake {
            width: LOCAL_CAPTURE_WIDTH,
            coverage,
            mips: chain.to_vec(),
        }));
        self.mark_stale(index);
        if let Some(layer) = self.layers.iter().position(|l| l.is_some_and(|l| l.id == id)) {
            created |= self.upload(d, q, layer, id);
        }
        created
    }
}

/// One rendered face: linear radiance and depth, `face × face`.
pub(crate) struct Face {
    pub(crate) rgb: Vec<[f32; 3]>,
    pub(crate) depth: Vec<f32>,
}

/// Bilinear read of a face at `(u, v)` in 0..1, as premultiplied radiance and coverage.
fn read_face(face: &Face, size: u32, u: f32, v: f32) -> ([f32; 3], f32) {
    let s = size as f32;
    let (x, y) = (u * s - 0.5, v * s - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let at = |ix: i64, iy: i64| {
        let ix = ix.clamp(0, i64::from(size) - 1) as usize;
        let iy = iy.clamp(0, i64::from(size) - 1) as usize;
        let i = iy * size as usize + ix;
        let covered = face.depth[i] < FAR_DEPTH;
        (
            if covered { face.rgb[i] } else { [0.0; 3] },
            f32::from(u8::from(covered)),
        )
    };
    let (ix, iy) = (x0 as i64, y0 as i64);
    let (a, b, c, d) = (at(ix, iy), at(ix + 1, iy), at(ix, iy + 1), at(ix + 1, iy + 1));
    let mix = |p: f32, q: f32, r: f32, t: f32| {
        let top = p + (q - p) * fx;
        let bottom = r + (t - r) * fx;
        top + (bottom - top) * fy
    };
    (
        [0, 1, 2].map(|k| mix(a.0[k], b.0[k], c.0[k], d.0[k])),
        mix(a.1, b.1, c.1, d.1),
    )
}

/// The equirect image of six faces (the order of [`cube_face_axes`]). Rows of the output are the
/// directions of `dir_from_equirect`, the layout the forward pass's `equirect_uv` reads.
pub(crate) fn pack_faces(faces: &[Face; 6], size: u32) -> CaptureImage {
    debug_assert_eq!(cube_face_axes(0).0, [1.0, 0.0, 0.0]);
    let (width, height) = (LOCAL_CAPTURE_WIDTH, LOCAL_CAPTURE_WIDTH / 2);
    let mut texels = Vec::with_capacity((width * height) as usize);
    let mut coverage = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let dir = crate::ibl::dir_from_equirect(
                (x as f32 + 0.5) / width as f32,
                (y as f32 + 0.5) / height as f32,
            );
            let (face, u, v) = project_to_face(dir);
            let (rgb, c) = read_face(&faces[face as usize], size, u, v);
            texels.push(rgb);
            coverage.push(c);
        }
    }
    CaptureImage {
        width,
        texels,
        coverage,
    }
}

/// [`capture_chain`] of six faces and the mean coverage over the sphere.
pub(crate) fn bake_faces(faces: &[Face; 6], size: u32) -> (Vec<Vec<u8>>, f32) {
    let image = pack_faces(faces, size);
    let (mut sum, mut weight) = (0.0f32, 0.0f32);
    let (width, height) = (image.width as usize, image.width as usize / 2);
    for y in 0..height {
        let w = ((y as f32 + 0.5) / height as f32 * std::f32::consts::PI).sin();
        for x in 0..width {
            sum += image.coverage[y * width + x] * w;
            weight += w;
        }
    }
    (capture_chain(&image), sum / weight.max(1e-6))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: u64) -> LocalCaptureDesc {
        LocalCaptureDesc::new(id, [0.0, 1.0, 0.0], [2.0, 1.0, 2.0])
    }

    fn bake(seed: u8) -> Arc<LocalCaptureBake> {
        Arc::new(LocalCaptureBake {
            width: LOCAL_CAPTURE_WIDTH,
            coverage: 0.5,
            mips: (0..capture_mips(LOCAL_CAPTURE_WIDTH) as usize)
                .map(|m| {
                    let w = (LOCAL_CAPTURE_WIDTH >> m).max(1) as usize;
                    let h = ((LOCAL_CAPTURE_WIDTH / 2) >> m).max(1) as usize;
                    vec![seed; w * h * 8]
                })
                .collect(),
        })
    }

    /// Six baked 2 m rooms in a row along +X, 4 m apart (centres at x = 0, 4, 8, ...).
    fn corridor() -> LocalCaptures {
        let mut set = LocalCaptures::default();
        for id in 0..6u64 {
            let mut d = LocalCaptureDesc::new(id, [id as f32 * 4.0, 1.0, 0.0], [2.0, 1.0, 2.0]);
            d.bake = Some(bake(id as u8));
            set.place(d).expect("placed");
        }
        set
    }

    /// Marks every planned write done (what `prepare` does on the GPU).
    fn upload_all(set: &mut LocalCaptures, writes: &[LayerWrite]) {
        for w in writes {
            if let Some(l) = set.layers[w.layer].as_mut() {
                l.uploaded = true;
            }
        }
    }

    /// The capture weights a frame reads, by id.
    fn weights(set: &LocalCaptures) -> HashMap<u64, f32> {
        let centers = set.uniform_centers();
        let mut out = HashMap::new();
        for (i, layer) in set.layers.iter().enumerate() {
            if let Some(l) = layer {
                out.insert(l.id, centers[i][3]);
            }
        }
        out
    }

    #[test]
    fn a_new_capture_fades_over_a_quarter_of_its_box() {
        let d = desc(1);
        assert_eq!(d.fade, 0.5);
        assert_eq!(d.weight, 1.0);
        assert!(d.validate().is_ok());
    }

    #[test]
    fn bad_values_are_refused() {
        for (bad, why) in [
            (
                LocalCaptureDesc {
                    center: [f32::NAN, 0.0, 0.0],
                    ..desc(1)
                },
                "center",
            ),
            (
                LocalCaptureDesc {
                    half_extent: [1.0, 0.0, 1.0],
                    ..desc(1)
                },
                "half_extent",
            ),
            (
                LocalCaptureDesc {
                    weight: 1.5,
                    ..desc(1)
                },
                "weight",
            ),
            (
                LocalCaptureDesc {
                    fade: -1.0,
                    ..desc(1)
                },
                "fade",
            ),
        ] {
            let err = bad.validate().expect_err("refused").to_string();
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn thirty_two_captures_fit_and_one_more_is_refused_until_one_goes() {
        let mut set = LocalCaptures::default();
        for id in 0..MAX_LOCAL_CAPTURES as u64 {
            set.place(desc(id)).expect("placed");
        }
        assert_eq!(set.place(desc(99)), Err(LocalCaptureError::Full));
        // The same id moves the capture instead of taking a slot.
        set.place(LocalCaptureDesc {
            weight: 0.5,
            ..desc(2)
        })
        .expect("moved");
        assert_eq!(set.infos().len(), MAX_LOCAL_CAPTURES);
        assert_eq!(set.desc(2).map(|d| d.weight), Some(0.5));
        assert!(set.remove(0));
        assert!(!set.remove(0));
        set.place(desc(99)).expect("a free slot");
    }

    #[test]
    fn a_bake_request_queues_one_or_all_and_names_an_unknown_id() {
        let mut set = LocalCaptures::default();
        set.place(desc(1)).expect("placed");
        set.place(desc(2)).expect("placed");
        assert_eq!(set.request_bake(Some(2)), Ok(1));
        assert_eq!(set.pending(), vec![2]);
        assert_eq!(set.request_bake(None), Ok(2));
        assert_eq!(set.pending(), vec![1, 2]);
        assert_eq!(set.request_bake(Some(7)), Err(LocalCaptureError::Unknown(7)));
    }

    #[test]
    fn faces_pack_with_coverage_and_the_sky_stays_uncovered() {
        // +X (face 0) is a red surface; every other face saw only the environment.
        let size = 8u32;
        let n = (size * size) as usize;
        let empty = || Face {
            rgb: vec![[0.0; 3]; n],
            depth: vec![1.0; n],
        };
        let mut faces: [Face; 6] = std::array::from_fn(|_| empty());
        faces[0] = Face {
            rgb: vec![[9000.0, 0.0, 0.0]; n],
            depth: vec![0.5; n],
        };
        let image = pack_faces(&faces, size);
        let (w, h) = (image.width as usize, image.width as usize / 2);
        // The centre of the image looks along +X.
        let centre = h / 2 * w + w / 2;
        assert_eq!(image.coverage[centre], 1.0);
        assert!((image.texels[centre][0] - 9000.0).abs() < 1.0);
        // Looking along −X sees no surface.
        let back = h / 2 * w + 2;
        assert_eq!(image.coverage[back], 0.0);
        assert_eq!(image.texels[back], [0.0; 3]);
        let (chain, coverage) = bake_faces(&faces, size);
        assert_eq!(chain.len() as u32, capture_mips(LOCAL_CAPTURE_WIDTH));
        // One face of six, by solid angle.
        assert!((coverage - 1.0 / 6.0).abs() < 0.02, "{coverage}");
        // The first mip holds the full image: 8 bytes per texel.
        assert_eq!(chain[0].len(), w * h * 8);
    }

    #[test]
    fn the_selection_takes_the_nearest_boxes_within_the_budget() {
        let mut set = corridor();
        // Eye in room 3 (x = 12): rooms 3, then 2 and 4 (2 m from their boxes) are nearest.
        let (writes, changed) = set.plan(Some([12.0, 1.0, 0.0]), 2);
        assert!(changed);
        let ids: Vec<u64> = writes.iter().map(|w| w.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&3), "{ids:?}");
        upload_all(&mut set, &writes);
        // The first fill shows at once: nothing left to fade from.
        assert!(weights(&set).values().all(|w| *w == 1.0), "{:?}", weights(&set));
        assert_eq!(set.swaps, 0);
        let listed: Vec<_> = set.infos().into_iter().filter(|i| i.layer.is_some()).collect();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn a_slot_change_fades_over_frames_and_never_pops() {
        let mut set = corridor();
        let step = 1.0 / LOCAL_CAPTURE_FADE_FRAMES as f32;
        let mut last: HashMap<u64, f32> = HashMap::new();
        // Walk the corridor from room 0 to room 5 in 5 cm steps, budget 2.
        for (frames, k) in (0..=400).enumerate() {
            let eye = [k as f32 * 0.05, 1.0, 0.0];
            let (writes, _) = set.plan(Some(eye), 2);
            upload_all(&mut set, &writes);
            let now = weights(&set);
            if frames > 0 {
                for id in 0..6u64 {
                    let (a, b) = (
                        last.get(&id).copied().unwrap_or(0.0),
                        now.get(&id).copied().unwrap_or(0.0),
                    );
                    assert!(
                        (a - b).abs() <= step + 1e-6,
                        "frame {k}: capture {id} jumped {a} -> {b}"
                    );
                }
            }
            assert!(set.layers.iter().flatten().count() <= LOCAL_CAPTURE_LAYERS);
            last = now;
        }
        // Rooms 0+1 became 4+5: at least four swaps, each faded.
        assert!(set.swaps >= 4, "{}", set.swaps);
        let end = weights(&set);
        assert_eq!(end.get(&5), Some(&1.0), "{end:?}");
    }

    #[test]
    fn the_budget_of_every_layer_also_fades() {
        // Budget 4 uses every layer: the incoming capture waits for the leaving one, then fades in.
        let mut set = corridor();
        let step = 1.0 / LOCAL_CAPTURE_FADE_FRAMES as f32;
        let mut last: HashMap<u64, f32> = HashMap::new();
        for k in 0..=400 {
            let (writes, _) = set.plan(Some([k as f32 * 0.05, 1.0, 0.0]), 4);
            upload_all(&mut set, &writes);
            let now = weights(&set);
            if k > 0 {
                for id in 0..6u64 {
                    let (a, b) = (
                        last.get(&id).copied().unwrap_or(0.0),
                        now.get(&id).copied().unwrap_or(0.0),
                    );
                    assert!(
                        (a - b).abs() <= step + 1e-6,
                        "frame {k}: capture {id} jumped {a} -> {b}"
                    );
                }
            }
            last = now;
        }
        assert!(set.swaps >= 2, "{}", set.swaps);
        assert_eq!(last.get(&5), Some(&1.0), "{last:?}");
    }

    #[test]
    fn an_eye_on_the_boundary_does_not_swap_back_and_forth() {
        let mut set = corridor();
        // Settle in room 1, budget 1.
        for _ in 0..20 {
            let (w, _) = set.plan(Some([4.0, 1.0, 0.0]), 1);
            upload_all(&mut set, &w);
        }
        assert_eq!(set.swaps, 0);
        // The boundary between rooms 1 and 2 is x = 6: wobble 0.3 m around it.
        for k in 0..60 {
            let x = if k % 2 == 0 { 5.7 } else { 6.3 };
            let (w, _) = set.plan(Some([x, 1.0, 0.0]), 1);
            upload_all(&mut set, &w);
        }
        assert_eq!(set.swaps, 0, "the hysteresis keeps room 1");
        // Well inside room 2 it swaps once.
        for _ in 0..20 {
            let (w, _) = set.plan(Some([8.0, 1.0, 0.0]), 1);
            upload_all(&mut set, &w);
        }
        assert_eq!(set.swaps, 1);
    }

    #[test]
    fn warm_frames_write_nothing() {
        let mut set = corridor();
        for _ in 0..(LOCAL_CAPTURE_FADE_FRAMES + 2) {
            let (w, _) = set.plan(Some([12.0, 1.0, 0.0]), 3);
            upload_all(&mut set, &w);
        }
        for _ in 0..10 {
            let (writes, changed) = set.plan(Some([12.0, 1.0, 0.0]), 3);
            assert!(writes.is_empty() && !changed, "{writes:?}");
        }
        // A new bake of a selected capture writes its layer again, and only that one.
        let mut d = set.desc(3).expect("placed");
        d.bake = Some(bake(77));
        set.place(d).expect("installed");
        let (writes, changed) = set.plan(Some([12.0, 1.0, 0.0]), 3);
        assert!(changed);
        assert_eq!(writes.iter().map(|w| w.id).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn tier_budgets_stay_within_the_layers() {
        use crate::profile::{RenderProfile, RenderProfilePreset};
        let budget =
            |p| local_capture_budget(&crate::FrameQuality::from_profile(&RenderProfile::from_preset(p)));
        assert_eq!(budget(RenderProfilePreset::DesktopUltra), 4);
        assert_eq!(budget(RenderProfilePreset::DesktopHigh), 4);
        assert_eq!(
            local_capture_budget(&crate::FrameQuality::from_profile(
                &crate::QualityTier::Medium.profile()
            )),
            4
        );
        assert_eq!(budget(RenderProfilePreset::MobileLow), 2);
        for p in [
            RenderProfilePreset::MobileLow,
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
            RenderProfilePreset::Deck,
        ] {
            assert!((2..=LOCAL_CAPTURE_LAYERS).contains(&budget(p)), "{p:?}");
        }
        assert_eq!(local_capture_budget(&crate::FrameQuality::LAB), 2);
        // A plan never selects more than its budget.
        let mut set = corridor();
        let (writes, _) = set.plan(Some([10.0, 1.0, 0.0]), 9);
        assert_eq!(writes.len(), LOCAL_CAPTURE_LAYERS);
    }

    /// Drives `sync_local_captures` against a default runtime renderer; the schedule (not
    /// `run_system_once`) keeps the system's entity-to-id map alive between runs.
    fn sync_world() -> (World, Schedule, Entity) {
        let mut world = World::new();
        world.init_resource::<crate::RuntimeRenderer>();
        let entity = world
            .spawn((LocalCapture::default(), GlobalTransform::IDENTITY))
            .id();
        let mut schedule = Schedule::default();
        schedule.add_systems(sync_local_captures);
        (world, schedule, entity)
    }

    fn capture_ids(world: &World) -> Vec<u64> {
        world
            .resource::<crate::RuntimeRenderer>()
            .0
            .local_captures()
            .into_iter()
            .map(|i| i.desc.id)
            .collect()
    }

    #[test]
    fn changing_a_capture_id_removes_the_old_slot() {
        let (mut world, mut schedule, entity) = sync_world();
        schedule.run(&mut world);
        assert_eq!(capture_ids(&world), vec![0]);
        world.entity_mut(entity).insert(LocalCapture::new(7, [1.0; 3]));
        schedule.run(&mut world);
        assert_eq!(
            capture_ids(&world),
            vec![7],
            "the renamed id replaces the old slot"
        );
        world.despawn(entity);
        schedule.run(&mut world);
        assert!(
            capture_ids(&world).is_empty(),
            "despawn drops the renamed capture"
        );
    }

    /// With every slot used, renaming one capture's id frees its old slot for the new id.
    #[test]
    fn changing_a_capture_id_with_every_slot_used_replaces_the_slot() {
        let mut world = World::new();
        world.init_resource::<crate::RuntimeRenderer>();
        let entities: Vec<Entity> = (0..MAX_LOCAL_CAPTURES as u64)
            .map(|id| {
                world
                    .spawn((
                        LocalCapture::new(id, [1.0; 3]),
                        GlobalTransform(regia_core::glam::Affine3A::from_translation(
                            regia_core::glam::Vec3::new(id as f32 * 4.0, 0.0, 0.0),
                        )),
                    ))
                    .id()
            })
            .collect();
        let mut schedule = Schedule::default();
        schedule.add_systems(sync_local_captures);
        schedule.run(&mut world);
        assert_eq!(capture_ids(&world).len(), MAX_LOCAL_CAPTURES);
        let renamed = MAX_LOCAL_CAPTURES as u64 + 7;
        world
            .entity_mut(entities[0])
            .insert(LocalCapture::new(renamed, [1.0; 3]));
        schedule.run(&mut world);
        let ids = capture_ids(&world);
        assert_eq!(ids.len(), MAX_LOCAL_CAPTURES);
        assert!(ids.contains(&renamed), "the rename was refused: {ids:?}");
        assert!(!ids.contains(&0), "the old id stayed: {ids:?}");
    }

    #[test]
    fn two_entities_with_the_same_id_do_not_drop_the_survivor() {
        let mut world = World::new();
        world.init_resource::<crate::RuntimeRenderer>();
        let a = world
            .spawn((
                LocalCapture::default(),
                GlobalTransform(regia_core::glam::Affine3A::from_translation(
                    regia_core::glam::Vec3::new(1.0, 0.0, 0.0),
                )),
            ))
            .id();
        let b = world
            .spawn((
                LocalCapture::default(),
                GlobalTransform(regia_core::glam::Affine3A::from_translation(
                    regia_core::glam::Vec3::new(5.0, 0.0, 0.0),
                )),
            ))
            .id();
        let mut schedule = Schedule::default();
        schedule.add_systems(sync_local_captures);
        schedule.run(&mut world);
        let infos = world.resource::<crate::RuntimeRenderer>().0.local_captures();
        assert_eq!(infos.len(), 1, "the duplicate id is refused, not moved");
        assert_eq!(infos[0].desc.center, [1.0, 0.0, 0.0]);
        // Despawning the placed entity frees the id; the survivor can take it with its next edit.
        world.despawn(a);
        world.entity_mut(b).insert(LocalCapture::new(3, [1.0; 3]));
        schedule.run(&mut world);
        let infos = world.resource::<crate::RuntimeRenderer>().0.local_captures();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].desc.id, 3);
        assert_eq!(infos[0].desc.center, [5.0, 0.0, 0.0]);
    }

    #[test]
    fn before_the_selection_a_slot_is_its_layer() {
        // No plan ran: slot i reads layer i as before ADR-2860, and only an uploaded map shows.
        let mut set = LocalCaptures::default();
        for id in 0..5 {
            set.place(desc(id)).expect("placed");
        }
        assert_eq!(set.layer_count(), 0);
        let ids: Vec<Option<u64>> = set.layers.iter().map(|l| l.map(|l| l.id)).collect();
        assert_eq!(ids, vec![Some(0), Some(1), Some(2), Some(3)]);
        // A bake placed with a capture is kept but waits for a layer write.
        let mut d = desc(1);
        d.bake = Some(bake(1));
        set.place(d).expect("installed");
        assert_eq!(set.layer_count(), 0);
        assert!(set.infos()[1].baked);
        assert_eq!(set.infos()[1].bakes, 1);
        // A bake that does not fit the texture is refused.
        let mut bad = desc(2);
        bad.bake = Some(Arc::new(LocalCaptureBake {
            width: 64,
            coverage: 0.0,
            mips: vec![],
        }));
        assert!(set.place(bad).is_err());
    }
}
