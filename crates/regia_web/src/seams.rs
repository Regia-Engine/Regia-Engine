// SPDX-License-Identifier: MPL-2.0
//! What the player needs from the renderer, the configurator and the drag view: three small traits, so
//! the loop, input and event logic run and are tested without a browser or a GPU.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::WebError;

/// A pointer's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerClass {
    /// A finger or a pen.
    Touch,
    /// The primary mouse button.
    Mouse,
    /// Another mouse button.
    MouseSecondary,
}

/// The view, as the page reads it and sets it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewValues {
    /// Tilt in degrees: x about the up axis, y about the right axis.
    pub tilt: [f32; 2],
    /// Turn of the piece about its front axis, in degrees.
    pub spin: f32,
    /// Offset of the look-at point in the front plane, in mm.
    pub pan: [f32; 2],
    /// Zoom factor; 1 is the default framing.
    pub zoom: f32,
}

impl Default for ViewValues {
    fn default() -> Self {
        ViewValues {
            tilt: [0.0, 0.0],
            spin: 0.0,
            pan: [0.0, 0.0],
            zoom: 1.0,
        }
    }
}

/// A `setView` argument: what is absent stays as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewRequest {
    /// New tilt in degrees.
    pub tilt: Option<[f32; 2]>,
    /// New spin in degrees.
    pub spin: Option<f32>,
    /// New pan in mm.
    pub pan: Option<[f32; 2]>,
    /// New zoom.
    pub zoom: Option<f32>,
    /// Animate there (the default) or jump.
    pub animate: Option<bool>,
}

impl ViewRequest {
    /// `current` with this request's fields applied.
    pub fn over(&self, current: ViewValues) -> ViewValues {
        ViewValues {
            tilt: self.tilt.unwrap_or(current.tilt),
            spin: self.spin.unwrap_or(current.spin),
            pan: self.pan.unwrap_or(current.pan),
            zoom: self.zoom.unwrap_or(current.zoom),
        }
    }
}

/// The camera the renderer draws from, in mm.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraPose {
    /// Camera position.
    pub eye: [f32; 3],
    /// The point looked at.
    pub look_at: [f32; 3],
    /// Up direction.
    pub up: [f32; 3],
    /// Field of view across the shorter side of the target, in degrees.
    pub fov_short_deg: f32,
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance.
    pub far: f32,
}

/// The drag view: gestures in, a damped and clamped camera out.
pub trait ViewModel {
    /// A pointer went down.
    fn pointer_down(&mut self, id: u64, class: PointerClass, x: f32, y: f32, t_ms: f64);
    /// A pointer moved.
    fn pointer_move(&mut self, id: u64, x: f32, y: f32, t_ms: f64);
    /// A pointer lifted.
    fn pointer_up(&mut self, id: u64, t_ms: f64);
    /// A pointer was cancelled.
    fn pointer_cancel(&mut self, id: u64);
    /// A wheel scroll in lines; positive zooms in.
    fn wheel(&mut self, lines: f32);
    /// The drawing size in device pixels (gestures come in the same unit).
    fn set_viewport(&mut self, width: f32, height: f32);
    /// Drawing pixels per CSS pixel (or per Android dp). Gesture distances are meant for a finger, so they
    /// grow with it. The default ignores it.
    fn set_pixel_ratio(&mut self, _ratio: f32) {}
    /// Advance by `dt_s`; true when the camera moved.
    fn update(&mut self, dt_s: f32) -> bool;
    /// Whether the view still moves without further input.
    fn is_animating(&self) -> bool;
    /// Set the view inside the limits; returns where the target ended up.
    fn set_view(&mut self, view: ViewValues, immediate: bool) -> ViewValues;
    /// Animate back to the default view.
    fn reset(&mut self);
    /// The view shown now.
    fn shown(&self) -> ViewValues;
    /// The camera to draw from.
    fn pose(&self) -> CameraPose;
}

/// What a successful pick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PickApplied {
    /// Groups the configurator changed on top of the pick (`adjust` only).
    pub adjusted: Vec<String>,
    /// Whether meshes must change too, not only materials.
    pub geometry_changed: bool,
}

/// One thing to draw for the current picks.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedDraw {
    /// The package mesh.
    pub mesh: String,
    /// The material zone it takes its material from.
    pub zone: String,
    /// Where each instance goes.
    pub transforms: Vec<glam::Affine3A>,
}

/// What a zone looks like now. Keyed so the renderer can tell a real change from a repeat.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedMaterial {
    /// A key that changes when the look changes (the material and finish ids).
    pub key: String,
    /// The renderer's material. Anodize lookups stay aside in `lut`, since the handle belongs to a device.
    pub look: regia_looks::LookMaterial,
    /// The stone this material is drawn on, as its mesh measures (a default size for metals, which ignore it).
    pub stone: regia_looks::StoneSize,
}

/// What the current picks draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScenePlan {
    /// The draws, in a stable order.
    pub draws: Vec<PlannedDraw>,
    /// Zone to material.
    pub materials: std::collections::BTreeMap<String, PlannedMaterial>,
    /// What had to fall back to a neutral look: a zone with no usable library entry. Empty on a good package.
    pub problems: Vec<String>,
}

/// The configurator: picks, availability, the build.
pub trait ProductModel {
    /// A number that differs between opened products, so a renderer knows when to reload its meshes.
    fn serial(&self) -> u64;
    /// The current picks as draws and materials.
    fn plan(&self) -> Result<ScenePlan, WebError>;
    /// Every mesh the product can show, by name; the whole set goes up at open, so a swap needs no
    /// upload.
    fn meshes(&self) -> &[regia_viewer_pack::MeshData];
    /// The groups and values with one-step availability (the JS `options()` result).
    fn options(&self) -> Value;
    /// Applies a pick (a string, or an array for a multi group) only if the result is sold.
    fn set_pick(&mut self, group: &str, value: &Value, adjust: bool) -> Result<PickApplied, WebError>;
    /// The current `jewelry.build/1` for the shown view.
    fn build(&self, view: &ViewValues) -> Result<Value, WebError>;
    /// Whether the listing is incomplete (never presented as the full range).
    fn incomplete(&self) -> bool;
}

/// What drew or did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawOutcome {
    /// A frame reached the surface.
    Drawn,
    /// The surface had nothing to draw to now; try on the next change.
    Skipped,
}

/// The renderer and its surface.
pub trait SceneBackend {
    /// Show `product`'s current state. `geometry`: meshes changed, not only materials.
    fn sync_product(&mut self, product: &dyn ProductModel, geometry: bool) -> Result<(), WebError>;
    /// The drawing size in device pixels.
    fn resize(&mut self, width: u32, height: u32);
    /// Draw one frame from `pose`.
    fn draw(&mut self, pose: &CameraPose) -> Result<DrawOutcome, WebError>;
    /// Pipelines created so far (constant after a product opens).
    fn pipelines(&self) -> usize;
    /// Whether this backend draws with the `regia_looks` shaders. A `stats()` reader can tell a player showing
    /// the approved looks from one on the renderer's starting-point shaders.
    fn draws_looks(&self) -> bool {
        false
    }
}
