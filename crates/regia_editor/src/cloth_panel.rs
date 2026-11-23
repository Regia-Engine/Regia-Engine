// SPDX-License-Identifier: MPL-2.0
// Original code (spec 55 § 6, ADR-2884). The paint edits are `regia_physics::cloth::maps`
// (spec 55 § 3.4) through `regia_agent::cloth::edit_cloth_map`, the core
// `physics.cloth_edit_map` runs; the stroke spacing and the ramp are plain interpolation.

//! The cloth inspector and paint mode (spec 55 § 6, ADR-2884): a `.cloth.ron` (or `.soft.ron`)
//! opened from the project, edited through its own types (`ClothSource` / `SoftSource`) and
//! saved atomically. Agent surface: `editor.cloth_panel_state`, `editor.cloth_panel_edit`,
//! `editor.cloth_paint_stroke` (`book/agents/cloth.md`).
//!
//! - **Inspector.** Every `ClothParams` field, the starting point (`preset`), the self-collision
//!   mode and the wind model of the file; the target instance's blend and LOD distances (fields
//!   of its `ClothSimSource` / `SoftSimSource`, saved with the level). A soft body gets the
//!   parameter subset it has: substeps, iterations, damping, gravity scale, collision thickness,
//!   particle mass, edge compliance and its model's numbers.
//! - **Paint.** A brush on the sim mesh issues spec 55 § 3.4 `Sphere` edits along a stroke
//!   (map picker, value, radius, falloff, strength), one undo step per stroke, through
//!   `regia_agent::cloth::edit_cloth_map`, the code `physics.cloth_edit_map` runs. The sim mesh is
//!   the cooked asset of a `ClothSim` in the edit world with the file's name (so those instances
//!   see the paint at once), else the file cooked by `regia_asset_pipeline`. The viewport colors
//!   each sim vertex on a ramp in meters. The simulate-preview toggle steps a private solver of
//!   the asset with the edited parameters; the level is never touched.
//! - **Gizmos.** The `cloth` group (off by default): body proxies, the target solver's world
//!   snapshot shapes, and for selected vertices a `reach` sphere and a `standoff` plane.
//!
//! The editor holds no cloth data that is not in the file (or the instance component): every
//! change is one entry on the shared [`UndoStack`]; a file nobody edited is never rewritten, and
//! a file edited back to how it was opened gets its original bytes again.

use std::sync::Arc;

use regia::physics::cloth::asset::{
    ClothSource, MAP_NAMES, SelfCollisionSetting, SoftModelSource, SoftSource, WindSetting, vertex_normals,
};
use regia::physics::cloth::maps::{Falloff, MapEdit};
use regia::physics::cloth::presets::ClothPreset;
use regia::physics::cloth::{ClothAsset, ClothParams, ClothSet, ClothSolver, PoseTarget, SnapshotKind};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_ecs::reflect::PartialReflect;
use regia_framework::cloth::{ClothBodyProxy, ClothGlueState, ClothSimSource, ProxyShape, SoftSimSource};
use regia_scene::prelude::GlobalTransform;
use serde_json::{Value, json};

use crate::audio_gizmo::Segment;
use crate::command::{EditorCommand, SetProperty, UndoStack};
use crate::data_panels::{PanelError, confined, from_rpc, write_atomic};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/cloth.md";
const PREFIX: &str = "cloth_panel";
const SUFFIXES: &[&str] = &[".cloth.ron", ".soft.ron"];
/// Fixed step of the simulate preview (s).
pub const PREVIEW_STEP: f32 = 1.0 / 60.0;
/// Most preview steps one agent call may run.
pub const MAX_PREVIEW_STEPS: u32 = 600;
/// Most points in one paint stroke.
pub const MAX_STROKE_POINTS: usize = 4096;
/// Most dabs one stroke may expand to.
pub const MAX_STROKE_DABS: usize = 16_384;
/// Most selected vertices the gizmos draw.
pub const MAX_SELECTED: usize = 1024;
/// Side of a `standoff` plane gizmo (m).
pub const STANDOFF_PLANE_SIZE: f32 = 0.1;
/// `reach` spheres are drawn at most this big (m); an unset reach is 1,000 km.
pub const REACH_DRAW_MAX: f32 = 10.0;

/// Proxy colour.
pub const CLOTH_PROXY: egui::Color32 = egui::Color32::from_rgb(120, 200, 255);
/// Snapshot shape colour.
pub const CLOTH_SNAPSHOT: egui::Color32 = egui::Color32::from_rgb(255, 170, 60);
/// `reach` sphere colour.
pub const CLOTH_REACH: egui::Color32 = egui::Color32::from_rgb(110, 230, 140);
/// `standoff` plane colour.
pub const CLOTH_STANDOFF: egui::Color32 = egui::Color32::from_rgb(230, 110, 200);
/// Preview mesh colour.
pub const CLOTH_PREVIEW: egui::Color32 = egui::Color32::from_rgb(200, 200, 210);

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("{PREFIX}_{code}"), message)
}

/// The source being edited: a cloth or a soft body.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelSource {
    /// A `.cloth.ron` (boxed: it is much larger than a soft-body source).
    Cloth(Box<ClothSource>),
    /// A `.soft.ron`.
    Soft(SoftSource),
}

impl PanelSource {
    fn name(&self) -> &str {
        match self {
            Self::Cloth(s) => &s.name,
            Self::Soft(s) => &s.name,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Cloth(_) => "cloth",
            Self::Soft(_) => "soft",
        }
    }

    /// The canonical text (`ClothSource::to_ron_string`; pretty RON for a soft body).
    fn to_text(&self) -> Result<String, PanelError> {
        match self {
            Self::Cloth(s) => s
                .to_ron_string()
                .map_err(|e| err("serialize_failed", e.to_string())),
            Self::Soft(s) => ron::ser::to_string_pretty(s, regia_core::ron_pretty())
                .map_err(|e| err("serialize_failed", e.to_string())),
        }
    }
}

/// Parses `text` as the file at `path` names it.
///
/// # Errors
/// `cloth_panel_parse_failed`.
pub fn parse_source(path: &str, text: &str) -> Result<PanelSource, PanelError> {
    let parsed = if path.to_ascii_lowercase().ends_with(".soft.ron") {
        SoftSource::from_ron_str(text).map(PanelSource::Soft)
    } else {
        ClothSource::from_ron_str(text).map(|c| PanelSource::Cloth(Box::new(c)))
    };
    parsed.map_err(|e| err("parse_failed", format!("{path}: {e} ({})", e.code())))
}

/// The numeric parameters a cloth file has, in inspector order.
pub const CLOTH_PARAMS: &[&str] = &[
    "substeps",
    "iterations",
    "damping",
    "gravity_scale",
    "stretch_compliance",
    "shear_compliance",
    "bend_compliance",
    "pull_compliance",
    "inertia_scale",
    "collision_thickness",
    "friction",
    "self_friction",
    "air_density",
    "teleport_distance",
    "density",
    "wind_scale",
];

/// The numeric parameters a soft-body file has: the inspector's subset plus its model's.
pub const SOFT_PARAMS: &[&str] = &[
    "substeps",
    "iterations",
    "damping",
    "gravity_scale",
    "collision_thickness",
    "particle_mass",
    "edge_compliance",
    "pressure",
    "volume_compliance",
    "stiffness",
    "beta",
];

fn cloth_param(p: &ClothParams, name: &str) -> Option<f32> {
    Some(match name {
        "substeps" => p.substeps as f32,
        "iterations" => p.iterations as f32,
        "damping" => p.damping,
        "gravity_scale" => p.gravity_scale,
        "stretch_compliance" => p.stretch_compliance,
        "shear_compliance" => p.shear_compliance,
        "bend_compliance" => p.bend_compliance,
        "pull_compliance" => p.pull_compliance,
        "inertia_scale" => p.inertia_scale,
        "collision_thickness" => p.collision_thickness,
        "friction" => p.friction,
        "self_friction" => p.self_friction,
        "air_density" => p.air_density,
        "teleport_distance" => p.teleport_distance,
        "density" => p.density,
        "wind_scale" => p.wind_scale,
        _ => return None,
    })
}

fn count_of(v: f32, name: &str) -> Result<u32, PanelError> {
    if v.fract() != 0.0 || !(1.0..=64.0).contains(&v) {
        return Err(err(
            "bad_value",
            format!("`{name}` must be a whole number in 1..=64, got {v}"),
        ));
    }
    Ok(v as u32)
}

fn set_cloth_param(p: &mut ClothParams, name: &str, v: f32) -> Result<(), PanelError> {
    match name {
        "substeps" => p.substeps = count_of(v, name)?,
        "iterations" => p.iterations = count_of(v, name)?,
        "damping" => p.damping = v,
        "gravity_scale" => p.gravity_scale = v,
        "stretch_compliance" => p.stretch_compliance = v,
        "shear_compliance" => p.shear_compliance = v,
        "bend_compliance" => p.bend_compliance = v,
        "pull_compliance" => p.pull_compliance = v,
        "inertia_scale" => p.inertia_scale = v,
        "collision_thickness" => p.collision_thickness = v,
        "friction" => p.friction = v,
        "self_friction" => p.self_friction = v,
        "air_density" => p.air_density = v,
        "teleport_distance" => p.teleport_distance = v,
        "density" => p.density = v,
        "wind_scale" => p.wind_scale = v,
        other => {
            return Err(err(
                "unknown_param",
                format!("a cloth has no parameter `{other}` (see CLOTH_PARAMS)"),
            ));
        }
    }
    Ok(())
}

fn soft_param(s: &SoftSource, name: &str) -> Option<f32> {
    let p = &s.params;
    Some(match (name, &s.model) {
        ("substeps", _) => p.substeps as f32,
        ("iterations", _) => p.iterations as f32,
        ("damping", _) => p.damping,
        ("gravity_scale", _) => p.gravity_scale,
        ("collision_thickness", _) => p.collision_thickness,
        ("particle_mass", _) => p.particle_mass,
        ("edge_compliance", _) => p.edge_compliance,
        ("pressure", SoftModelSource::Pressure { pressure, .. }) => *pressure,
        (
            "volume_compliance",
            SoftModelSource::Pressure {
                volume_compliance, ..
            }
            | SoftModelSource::Tetra {
                volume_compliance, ..
            },
        ) => *volume_compliance,
        ("stiffness", SoftModelSource::ShapeMatch { stiffness, .. }) => *stiffness,
        ("beta", SoftModelSource::ShapeMatch { beta, .. }) => *beta,
        _ => return None,
    })
}

fn set_soft_param(s: &mut SoftSource, name: &str, v: f32) -> Result<(), PanelError> {
    let model_missing = || {
        err(
            "unknown_param",
            format!("this soft body's model has no parameter `{name}`"),
        )
    };
    let unit = |v: f32| {
        if (0.0..=1.0).contains(&v) {
            Ok(v)
        } else {
            Err(err("bad_value", format!("`{name}` must be in 0..=1, got {v}")))
        }
    };
    let p = &mut s.params;
    match name {
        "substeps" => p.substeps = count_of(v, name)?,
        "iterations" => p.iterations = count_of(v, name)?,
        "damping" => p.damping = v,
        "gravity_scale" => p.gravity_scale = v,
        "collision_thickness" => p.collision_thickness = v,
        "particle_mass" => p.particle_mass = v,
        "edge_compliance" => p.edge_compliance = v,
        "pressure" => match &mut s.model {
            SoftModelSource::Pressure { pressure, .. } => *pressure = v,
            _ => return Err(model_missing()),
        },
        "volume_compliance" => match &mut s.model {
            SoftModelSource::Pressure {
                volume_compliance, ..
            }
            | SoftModelSource::Tetra {
                volume_compliance, ..
            } => {
                *volume_compliance = v;
            }
            SoftModelSource::ShapeMatch { .. } => return Err(model_missing()),
        },
        "stiffness" => match &mut s.model {
            SoftModelSource::ShapeMatch { stiffness, .. } => *stiffness = unit(v)?,
            _ => return Err(model_missing()),
        },
        "beta" => match &mut s.model {
            SoftModelSource::ShapeMatch { beta, .. } => *beta = unit(v)?,
            _ => return Err(model_missing()),
        },
        other => {
            return Err(err(
                "unknown_param",
                format!("a soft body has no parameter `{other}` (see SOFT_PARAMS)"),
            ));
        }
    }
    Ok(())
}

/// Checks a soft body's parameters (the solver config rules, a positive mass and pressure,
/// non-negative compliances).
fn validate_soft(s: &SoftSource) -> Result<(), PanelError> {
    let bad = |what: &str| err("bad_value", format!("`{what}` is out of range"));
    s.params
        .params(PREVIEW_STEP)
        .config
        .validate()
        .map_err(|e| err("bad_value", e.to_string()))?;
    let p = &s.params;
    if !(0.0..=20.0).contains(&p.damping) {
        return Err(bad("damping"));
    }
    if p.particle_mass.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return Err(bad("particle_mass"));
    }
    if p.edge_compliance
        .partial_cmp(&0.0)
        .is_none_or(|o| o == std::cmp::Ordering::Less)
    {
        return Err(bad("edge_compliance"));
    }
    match &s.model {
        SoftModelSource::Pressure {
            pressure,
            volume_compliance,
        } => {
            if pressure.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                return Err(bad("pressure"));
            }
            if volume_compliance
                .partial_cmp(&0.0)
                .is_none_or(|o| o == std::cmp::Ordering::Less)
            {
                return Err(bad("volume_compliance"));
            }
        }
        SoftModelSource::Tetra {
            volume_compliance, ..
        } => {
            if volume_compliance
                .partial_cmp(&0.0)
                .is_none_or(|o| o == std::cmp::Ordering::Less)
            {
                return Err(bad("volume_compliance"));
            }
        }
        SoftModelSource::ShapeMatch { .. } => {}
    }
    Ok(())
}

/// One change to the open file: what a widget or `editor.cloth_panel_edit` asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum ClothChange {
    /// A numeric parameter by name ([`CLOTH_PARAMS`], [`SOFT_PARAMS`]).
    Param(String, f32),
    /// The tether scale (`None` = off; cloth only).
    Tether(Option<f32>),
    /// The starting point (`None` = none; a [`ClothPreset`] name; cloth only).
    Preset(Option<String>),
    /// The self-collision mode (cloth only).
    SelfCollision(SelfCollisionSetting),
    /// The wind model (cloth only).
    WindModel(WindSetting),
}

impl ClothChange {
    /// Coalescing key: consecutive changes with one key (a slider scrub) are one undo step.
    fn key(&self) -> Option<String> {
        match self {
            Self::Param(name, _) => Some(format!("param:{name}")),
            Self::Tether(_) => Some("tether".into()),
            Self::SelfCollision(_) => Some("self_collision".into()),
            Self::WindModel(_) => Some("wind_model".into()),
            Self::Preset(_) => None,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Param(name, _) => name.clone(),
            Self::Tether(_) => "tether".into(),
            Self::Preset(_) => "starting point".into(),
            Self::SelfCollision(_) => "self-collision".into(),
            Self::WindModel(_) => "wind model".into(),
        }
    }
}

fn finite(v: f32, what: &str) -> Result<f32, PanelError> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(err("bad_value", format!("`{what}` must be a finite number")))
    }
}

fn cloth_only<'a>(source: &'a mut PanelSource, what: &str) -> Result<&'a mut ClothSource, PanelError> {
    match source {
        PanelSource::Cloth(c) => Ok(&mut **c),
        PanelSource::Soft(_) => Err(err(
            "unknown_param",
            format!("`{what}` is a cloth setting; a soft body does not have it"),
        )),
    }
}

/// Applies `change` to `source` and validates the result (`ClothParams::validate`, the preset
/// name, the soft-body rules).
///
/// # Errors
/// `cloth_panel_bad_value`, `cloth_panel_unknown_param` or `cloth_panel_unknown_preset`; `source`
/// may be half-changed on error (callers apply to a copy).
pub fn apply_change(source: &mut PanelSource, change: &ClothChange) -> Result<(), PanelError> {
    match change {
        ClothChange::Param(name, v) => {
            let v = finite(*v, name)?;
            match source {
                PanelSource::Cloth(c) => set_cloth_param(&mut c.params, name, v)?,
                PanelSource::Soft(s) => set_soft_param(s, name, v)?,
            }
        }
        ClothChange::Tether(t) => {
            let c = cloth_only(source, "tether")?;
            c.params.tether = match t {
                Some(t) => Some(finite(*t, "tether")?),
                None => None,
            };
        }
        ClothChange::Preset(p) => {
            let c = cloth_only(source, "preset")?;
            if let Some(name) = p {
                ClothPreset::from_name(name).map_err(|e| err("unknown_preset", e.to_string()))?;
            }
            c.preset.clone_from(p);
        }
        ClothChange::SelfCollision(mode) => {
            cloth_only(source, "self_collision")?.params.self_collision = *mode
        }
        ClothChange::WindModel(model) => cloth_only(source, "wind_model")?.params.wind_model = *model,
    }
    match source {
        PanelSource::Cloth(c) => c
            .params
            .validate()
            .map_err(|e| err("bad_value", format!("{e} ({})", e.code()))),
        PanelSource::Soft(s) => validate_soft(s),
    }
}

/// The paint brush (spec 55 § 3.4 `Sphere` edits along a stroke).
#[derive(Clone, Debug, PartialEq)]
pub struct ClothBrush {
    /// The map painted (one of `MAP_NAMES`).
    pub map: String,
    /// The value painted toward (meters for `reach` and `standoff`).
    pub value: f32,
    /// Brush radius in meters (positive).
    pub radius: f32,
    /// Falloff inside the radius.
    pub falloff: Falloff,
    /// Blend strength per dab, `0..=1`.
    pub strength: f32,
    /// Distance between dabs along a stroke (m); `None` is a quarter of the radius; 0 puts dabs
    /// on the given points only.
    pub spacing: Option<f32>,
}

impl Default for ClothBrush {
    fn default() -> Self {
        Self {
            map: "reach".into(),
            value: 0.05,
            radius: 0.1,
            falloff: Falloff::Smooth,
            strength: 1.0,
            spacing: None,
        }
    }
}

impl ClothBrush {
    fn check(&self) -> Result<(), PanelError> {
        if !MAP_NAMES.contains(&self.map.as_str()) {
            return Err(err(
                "bad_value",
                format!(
                    "`map` must be one of {}, got `{}`",
                    MAP_NAMES.join(", "),
                    self.map
                ),
            ));
        }
        finite(self.value, "value")?;
        if finite(self.radius, "radius")? <= 0.0 {
            return Err(err("bad_value", "`radius` must be positive"));
        }
        if !(0.0..=1.0).contains(&finite(self.strength, "strength")?) {
            return Err(err("bad_value", "`strength` must be in 0..=1"));
        }
        if let Some(s) = self.spacing
            && finite(s, "spacing")? < 0.0
        {
            return Err(err("bad_value", "`spacing` must not be negative"));
        }
        Ok(())
    }

    /// The `Sphere` edit of one dab at `center` (model space, rest pose).
    #[must_use]
    pub fn dab(&self, center: [f32; 3]) -> MapEdit {
        MapEdit::Sphere {
            center,
            radius: self.radius,
            value: self.value,
            falloff: self.falloff,
            strength: self.strength,
        }
    }
}

/// The dab centers of a stroke through `points`: every point, and between two consecutive points
/// evenly spaced dabs no farther apart than `spacing` (none with spacing 0). Deterministic: the
/// same points give the same dabs.
///
/// # Errors
/// `cloth_panel_bad_value` for no points, a non-finite point, too many points or dabs.
pub fn stroke_dabs(points: &[[f32; 3]], spacing: f32) -> Result<Vec<[f32; 3]>, PanelError> {
    if points.is_empty() {
        return Err(err("bad_value", "a stroke needs at least one point"));
    }
    if points.len() > MAX_STROKE_POINTS {
        return Err(err(
            "bad_value",
            format!("a stroke has at most {MAX_STROKE_POINTS} points"),
        ));
    }
    if points.iter().flatten().any(|v| !v.is_finite()) {
        return Err(err("bad_value", "stroke points must be finite"));
    }
    let mut out = vec![points[0]];
    for pair in points.windows(2) {
        let (a, b) = (Vec3::from_array(pair[0]), Vec3::from_array(pair[1]));
        if spacing > 0.0 {
            let steps = (a.distance(b) / spacing).ceil();
            // Capped so a far point cannot run away; the dab cap below refuses the stroke anyway.
            let steps = if steps.is_finite() {
                steps.min(MAX_STROKE_DABS as f32) as usize
            } else {
                1
            };
            for k in 1..steps {
                out.push(a.lerp(b, k as f32 / steps as f32).to_array());
            }
        }
        out.push(pair[1]);
        if out.len() > MAX_STROKE_DABS {
            return Err(err(
                "bad_value",
                format!("a stroke expands to at most {MAX_STROKE_DABS} dabs"),
            ));
        }
    }
    Ok(out)
}

/// The open file and its sim mesh.
#[derive(Clone, Debug)]
pub struct ClothDoc {
    /// Project-relative path.
    pub path: String,
    /// The file's bytes when it was opened (written back when the edits return to `pristine`).
    pub original: String,
    /// The source as `original` parses.
    pub pristine: PanelSource,
    /// The source as the file holds it now (after the last save).
    pub saved: PanelSource,
    /// The source being edited.
    pub source: PanelSource,
    /// The sim mesh paint works on (cloth only; `None` when it could not be found or cooked).
    pub asset: Option<Arc<ClothAsset>>,
    /// Why there is no sim mesh.
    pub sim_error: Option<String>,
    /// Where the sim mesh came from: `instance` (a `ClothSim` in the edit world) or `cooked`.
    pub sim_from: &'static str,
}

impl ClothDoc {
    /// Whether the edited source differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.source != self.saved
    }

    /// The bytes [`save_file`] writes for the edited source: the opened bytes when the edits
    /// returned to how the file was opened, else the canonical text.
    ///
    /// # Errors
    /// `cloth_panel_serialize_failed`.
    pub fn save_text(&self) -> Result<String, PanelError> {
        if self.source == self.pristine {
            Ok(self.original.clone())
        } else {
            self.source.to_text()
        }
    }
}

/// The simulate preview: a private solver of the open asset.
#[derive(Clone, Debug)]
pub struct ClothPreview {
    /// The solver.
    pub solver: ClothSolver,
    /// Fixed steps run.
    pub steps: u64,
    /// Pose targets (rest pose placed by the target's transform) for pose-bound assets.
    targets: Vec<PoseTarget>,
}

/// The cloth panel's state (an edit-world resource).
#[derive(Resource, Debug)]
pub struct ClothPanel {
    doc: Option<ClothDoc>,
    /// The instance entity the panel edits and draws on (`ClothSimSource`/`SoftSimSource`/`ClothSim`).
    pub target: Option<Entity>,
    /// The paint brush.
    pub brush: ClothBrush,
    /// Paint mode: left-drag in the viewport paints the sim mesh.
    pub painting: bool,
    /// The viewport color ramp `[low, high]` in map units (meters for `reach` / `standoff`).
    pub ramp: [f32; 2],
    /// The `cloth` gizmo group (off by default).
    pub gizmos: bool,
    /// Selected sim vertices (the gizmos draw their `reach` and `standoff`).
    pub selected: Vec<u32>,
    /// Simulate preview on.
    pub simulate: bool,
    preview: Option<ClothPreview>,
    /// The last failed operation.
    pub error: Option<PanelError>,
    strokes: u64,
    /// The egui frame the preview last stepped in (egui may run a frame's UI more than once).
    ticked_frame: Option<u64>,
    stroke: Vec<[f32; 3]>,
    path_input: String,
}

impl Default for ClothPanel {
    fn default() -> Self {
        Self {
            doc: None,
            target: None,
            brush: ClothBrush::default(),
            painting: false,
            ramp: [0.0, 0.2],
            gizmos: false,
            selected: Vec::new(),
            simulate: false,
            preview: None,
            error: None,
            strokes: 0,
            ticked_frame: None,
            stroke: Vec::new(),
            path_input: String::new(),
        }
    }
}

impl ClothPanel {
    /// The open file.
    #[must_use]
    pub fn doc(&self) -> Option<&ClothDoc> {
        self.doc.as_ref()
    }

    /// The open file, to change (document autosave's restore puts a recovered source in it).
    pub(crate) fn doc_mut(&mut self) -> Option<&mut ClothDoc> {
        self.doc.as_mut()
    }

    /// The simulate preview.
    #[must_use]
    pub fn preview(&self) -> Option<&ClothPreview> {
        self.preview.as_ref()
    }
}

fn panel_mut(world: &mut World) -> Mut<'_, ClothPanel> {
    if !world.contains_resource::<ClothPanel>() {
        world.init_resource::<ClothPanel>();
    }
    world.resource_mut::<ClothPanel>()
}

// ------------------------------------------------------------------------------------- open/save

/// The cooked asset of a `ClothSim` named `name` in the world, else `source` cooked from the
/// project (`regia_asset_pipeline::cloth_cook::cook_cloth`).
fn find_sim_mesh(
    world: &mut World,
    full: &std::path::Path,
    name: &str,
) -> (Option<Arc<ClothAsset>>, Option<String>, &'static str) {
    if let Some((_, asset)) = regia_agent::cloth::cloth_asset_users(world, name)
        .into_iter()
        .next()
    {
        return (Some(asset), None, "instance");
    }
    match regia_asset_pipeline::cloth_cook::cook_cloth(full) {
        Ok(asset) => (Some(Arc::new(asset)), None, "cooked"),
        Err(e) => (None, Some(e.to_string()), "none"),
    }
}

/// Opens the `.cloth.ron` / `.soft.ron` at the project-relative `path` and finds its sim mesh.
/// When no target is set, it becomes the one entity whose `ClothSimSource`/`SoftSimSource` names
/// the same file, else the one `ClothSim` using the asset; with several candidates it stays unset.
///
/// # Errors
/// `cloth_panel_no_project`, `cloth_panel_bad_path`, `cloth_panel_io_failed` or
/// `cloth_panel_parse_failed`.
pub fn open_file(world: &mut World, path: &str) -> Result<(), PanelError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("no_project", "no project is open; open one first"))?;
    let full = confined(&root, path, SUFFIXES, PREFIX)?;
    let text =
        std::fs::read_to_string(&full).map_err(|e| err("io_failed", format!("{}: {e}", full.display())))?;
    let source = parse_source(path, &text)?;
    let (asset, sim_error, sim_from) = match &source {
        PanelSource::Cloth(c) => find_sim_mesh(world, &full, &c.name),
        PanelSource::Soft(_) => (None, None, "none"),
    };
    let target = find_target(world, path, source.name());
    let mut panel = panel_mut(world);
    panel.path_input = path.to_owned();
    if panel.target.is_none() {
        panel.target = target;
    }
    panel.doc = Some(ClothDoc {
        path: path.to_owned(),
        original: text,
        pristine: source.clone(),
        saved: source.clone(),
        source,
        asset,
        sim_error,
        sim_from,
    });
    panel.selected.clear();
    panel.preview = None;
    panel.simulate = false;
    panel.error = None;
    Ok(())
}

/// The instance the panel binds to when it opens `path` (review rc1-rev-editor-k finding 4): the one
/// entity whose `ClothSimSource`/`SoftSimSource` names that same file ([`same_project_file`]), else the
/// one `ClothSim` using the asset `name` on an entity without a source naming some other file. Two or
/// more candidates bind nothing: edits that need an instance then answer `cloth_panel_no_target`
/// until one is picked, instead of the panel silently editing an arbitrary one.
fn find_target(world: &mut World, path: &str, name: &str) -> Option<Entity> {
    let mut found: Vec<Entity> = Vec::new();
    let mut q = world.query::<(Entity, &ClothSimSource)>();
    found.extend(
        q.iter(world)
            .filter(|(_, s)| same_project_file(&s.asset, path))
            .map(|(e, _)| e),
    );
    let mut q = world.query::<(Entity, &SoftSimSource)>();
    found.extend(
        q.iter(world)
            .filter(|(_, s)| same_project_file(&s.asset, path))
            .map(|(e, _)| e),
    );
    if !found.is_empty() {
        return single(found);
    }
    let users: Vec<Entity> = regia_agent::cloth::cloth_asset_users(world, name)
        .into_iter()
        .map(|(e, _)| e)
        .filter(|e| world.get::<ClothSimSource>(*e).is_none() && world.get::<SoftSimSource>(*e).is_none())
        .collect();
    single(users)
}

/// The one entity of `found`, `None` for none or several.
fn single(found: Vec<Entity>) -> Option<Entity> {
    match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// Whether two project-relative paths name the same file: compared component by component (`/` or
/// `\` separated, `.` and empty components dropped), with an optional leading `assets` component on
/// either side (a source may name its file relative to the asset folder, which the cloth loader also
/// accepts). A suffix is not a match: `armor/cloth/flag.cloth.ron` and `loincloth/flag.cloth.ron` are
/// not `cloth/flag.cloth.ron`.
fn same_project_file(a: &str, b: &str) -> bool {
    fn parts(p: &str) -> Vec<&str> {
        p.split(['/', '\\'])
            .filter(|c| !c.is_empty() && *c != ".")
            .collect()
    }
    let (a, b) = (parts(a), parts(b));
    let unprefixed = |p: &[&str], q: &[&str]| p.first() == Some(&"assets") && p[1..] == *q;
    a == b || unprefixed(&a, &b) || unprefixed(&b, &a)
}

/// Saves the open file: nothing when unedited; the original bytes when the edits returned to how
/// the file was opened; else the canonical text, written atomically. Returns the bytes written.
///
/// # Errors
/// `cloth_panel_no_doc`, `cloth_panel_no_project`, `cloth_panel_serialize_failed` or
/// `cloth_panel_io_failed`.
pub fn save_file(world: &mut World) -> Result<usize, PanelError> {
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| err("no_project", "no project is open"))?;
    let mut panel = panel_mut(world);
    let doc = panel
        .doc
        .as_mut()
        .ok_or_else(|| err("no_doc", "no cloth file is open"))?;
    if !doc.dirty() {
        return Ok(0);
    }
    let text = doc.save_text()?;
    let full = confined(&root, &doc.path, SUFFIXES, PREFIX)?;
    write_atomic(&full, &text, PREFIX)?;
    doc.saved = doc.source.clone();
    Ok(text.len())
}

// ------------------------------------------------------------------------------------- undo

#[derive(Clone)]
struct Snapshot {
    source: PanelSource,
    asset: Option<Arc<ClothAsset>>,
}

/// One reversible change to the open file (and, for paint, its sim mesh): one undo step.
pub struct ClothDocEdit {
    before: Snapshot,
    after: Snapshot,
    key: Option<String>,
    label: String,
}

impl ClothDocEdit {
    fn set(world: &mut World, snap: &Snapshot) {
        let users = match &snap.asset {
            Some(asset) => regia_agent::cloth::cloth_asset_users(world, &asset.name),
            None => Vec::new(),
        };
        if let Some(asset) = &snap.asset {
            let entities: Vec<Entity> = users.iter().map(|(e, _)| *e).collect();
            regia_agent::cloth::share_cloth_asset(world, &entities, asset);
        }
        let mut panel = panel_mut(world);
        if let Some(doc) = panel.doc.as_mut() {
            doc.source = snap.source.clone();
            if snap.asset.is_some() {
                doc.asset.clone_from(&snap.asset);
            }
        }
        // The preview runs the old numbers or mesh: rebuild it on the next tick.
        panel.preview = None;
    }
}

impl EditorCommand for ClothDocEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<ClothDocEdit>() else {
            return false;
        };
        if self.key.is_some() && self.key == next.key {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Cloth: {}", self.label)
    }
}

fn snapshot(world: &World) -> Result<Snapshot, PanelError> {
    world
        .get_resource::<ClothPanel>()
        .and_then(|p| p.doc.as_ref())
        .map(|d| Snapshot {
            source: d.source.clone(),
            asset: d.asset.clone(),
        })
        .ok_or_else(|| err("no_doc", "no cloth file is open; pass `path`"))
}

fn push(world: &mut World, before: Snapshot, after: Snapshot, key: Option<String>, label: String) {
    let mut command = ClothDocEdit {
        before,
        after,
        key,
        label,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
}

/// Applies `change` to the open file as one undo step. `Ok(false)` when it changed nothing.
///
/// # Errors
/// `cloth_panel_no_doc`, else [`apply_change`]'s errors (nothing changes on error).
pub fn edit_file(world: &mut World, change: &ClothChange) -> Result<bool, PanelError> {
    let before = snapshot(world)?;
    let mut after = before.clone();
    apply_change(&mut after.source, change)?;
    if after.source == before.source {
        return Ok(false);
    }
    push(world, before, after, change.key(), change.label());
    Ok(true)
}

/// One instance-field change (on the target's `ClothSimSource` / `SoftSimSource`).
#[derive(Clone, Debug, PartialEq)]
pub enum InstanceChange {
    /// Blend between pose (0) and simulation (1); cloth only.
    Blend(f32),
    /// Distance LOD thresholds `[d1, d2, d_off]` (ascending, positive).
    LodDistances([f32; 3]),
}

/// Writes `change` to the target's saved instance component through a [`SetProperty`] on the
/// shared undo stack. `Ok(false)` when the value is already that.
///
/// # Errors
/// `cloth_panel_no_target` without a target that has a `ClothSimSource` (blend, LOD) or a
/// `SoftSimSource` (LOD); `cloth_panel_bad_value` for a blend outside `0..=1` or LOD distances
/// that are not positive and ascending.
pub fn edit_instance(world: &mut World, change: &InstanceChange) -> Result<bool, PanelError> {
    let target = world
        .get_resource::<ClothPanel>()
        .and_then(|p| p.target)
        .ok_or_else(|| {
            err(
                "no_target",
                "no instance is picked; select a cloth entity or pass `entity`",
            )
        })?;
    let cloth = world.get::<ClothSimSource>(target).cloned();
    let soft = world.get::<SoftSimSource>(target).cloned();
    let (component, path, value, unchanged): (&str, &str, Box<dyn PartialReflect>, bool) =
        match (change, &cloth, &soft) {
            (InstanceChange::Blend(b), Some(c), _) => {
                if !(0.0..=1.0).contains(&finite(*b, "blend")?) {
                    return Err(err("bad_value", "`blend` must be in 0..=1"));
                }
                (
                    "ClothSimSource",
                    "blend",
                    Box::new(*b),
                    c.blend.to_bits() == b.to_bits(),
                )
            }
            (InstanceChange::LodDistances(d), c, s) if c.is_some() || s.is_some() => {
                if d.iter().any(|v| !v.is_finite() || *v <= 0.0) || d[0] > d[1] || d[1] > d[2] {
                    return Err(err(
                        "bad_value",
                        "`lod_distances` must be three positive ascending meters",
                    ));
                }
                let current = c
                    .as_ref()
                    .map_or_else(|| s.as_ref().map(|s| s.lod_distances), |c| Some(c.lod_distances));
                let component = if c.is_some() {
                    "ClothSimSource"
                } else {
                    "SoftSimSource"
                };
                (component, "lod_distances", Box::new(*d), current == Some(*d))
            }
            _ => {
                return Err(err(
                    "no_target",
                    "the target has no ClothSimSource (blend, LOD) or SoftSimSource (LOD) to edit",
                ));
            }
        };
    if unchanged {
        return Ok(false);
    }
    let mut command = SetProperty::capture(world, target, component, path, value)
        .map_err(|e| err("no_target", e.to_string()))?;
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(true)
}

// ------------------------------------------------------------------------------------- paint

/// What one paint stroke did.
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeResult {
    /// The dab centers (model space): replaying them as `physics.cloth_edit_map` `sphere` edits
    /// with the same brush gives the same map.
    pub dabs: Vec<[f32; 3]>,
    /// LOD 0 values that differ from before the stroke.
    pub changed: usize,
    /// `ClothSim` instances that got the painted asset.
    pub instances: usize,
}

/// Paints one stroke through `points` (model space, rest pose) with `brush` as one undo step:
/// [`stroke_dabs`], then per dab `regia_agent::cloth::edit_cloth_map` (the code
/// `physics.cloth_edit_map` runs) with [`ClothBrush::dab`]; every `ClothSim` using the asset gets
/// the result, and the file's maps are rewritten from it (`regia_physics::cloth::maps::store_maps`).
///
/// # Errors
/// `cloth_panel_no_doc`, `cloth_panel_no_sim_mesh` (a soft body, or no sim mesh found),
/// `cloth_panel_bad_value`, or the edit's own reason (`cloth_bad_param`, ...). Nothing changes on
/// error.
pub fn paint_stroke(
    world: &mut World,
    points: &[[f32; 3]],
    brush: &ClothBrush,
) -> Result<StrokeResult, PanelError> {
    brush.check()?;
    let before = snapshot(world)?;
    let PanelSource::Cloth(_) = &before.source else {
        return Err(err(
            "no_sim_mesh",
            "paint works on cloth; a soft body has no paintable maps here",
        ));
    };
    let Some(base) = before.asset.clone() else {
        let why = world
            .get_resource::<ClothPanel>()
            .and_then(|p| p.doc.as_ref())
            .and_then(|d| d.sim_error.clone())
            .unwrap_or_default();
        return Err(err(
            "no_sim_mesh",
            format!("no sim mesh: no ClothSim uses this asset and it did not cook ({why})"),
        ));
    };
    let _span = tracing::debug_span!("cloth_paint_stroke", points = points.len()).entered();
    let spacing = brush.spacing.unwrap_or(brush.radius * 0.25);
    let dabs = stroke_dabs(points, spacing)?;
    let mut asset = (*base).clone();
    for center in &dabs {
        let (next, _) = regia_agent::cloth::edit_cloth_map(&asset, &brush.map, &brush.dab(*center))
            .map_err(|e| from_rpc(&e, "cloth_bad_param"))?;
        asset = next;
    }
    let old = base.lod0().maps.get(&brush.map).cloned().unwrap_or_default();
    let new = asset.lod0().maps.get(&brush.map).cloned().unwrap_or_default();
    let changed = if old.is_empty() {
        new.len()
    } else {
        old.iter()
            .zip(&new)
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count()
    };
    let mut after = before.clone();
    if let PanelSource::Cloth(c) = &mut after.source {
        regia::physics::cloth::maps::store_maps(&asset, c);
    }
    let asset = Arc::new(asset);
    after.asset = Some(asset.clone());
    let instances = regia_agent::cloth::cloth_asset_users(world, &asset.name).len();
    let n = {
        let mut panel = panel_mut(world);
        panel.strokes += 1;
        panel.strokes
    };
    push(
        world,
        before,
        after,
        None,
        format!("paint {} (stroke {n})", brush.map),
    );
    Ok(StrokeResult {
        dabs,
        changed,
        instances,
    })
}

// ------------------------------------------------------------------------------------- preview

fn target_world(world: &World, target: Option<Entity>) -> Affine3A {
    target
        .and_then(|e| crate::mesh_source::world_matrix(world, e))
        .map_or(Affine3A::IDENTITY, Affine3A::from_mat4)
}

fn preview_params(source: &PanelSource) -> Option<ClothParams> {
    match source {
        PanelSource::Cloth(c) => c.with_preset_resolved().ok().map(|c| c.params),
        PanelSource::Soft(_) => None,
    }
}

fn build_preview(world: &World) -> Result<ClothPreview, PanelError> {
    let panel = world
        .get_resource::<ClothPanel>()
        .ok_or_else(|| err("no_doc", "no cloth file is open"))?;
    let doc = panel
        .doc
        .as_ref()
        .ok_or_else(|| err("no_doc", "no cloth file is open"))?;
    let asset = doc
        .asset
        .as_ref()
        .ok_or_else(|| err("no_sim_mesh", "the preview needs the sim mesh"))?;
    let params =
        preview_params(&doc.source).ok_or_else(|| err("no_sim_mesh", "the preview runs cloth only"))?;
    let place = target_world(world, panel.target);
    let solver = asset
        .build_solver_with(0, PREVIEW_STEP, place, params)
        .map_err(|e| err("bad_value", format!("{e} ({})", e.code())))?;
    let lod = asset.lod0();
    let targets = if lod.maps.has_pose_maps() {
        let mut normals = Vec::new();
        vertex_normals(&lod.rest_positions, &lod.triangles, &mut normals);
        lod.rest_positions
            .iter()
            .zip(&normals)
            .map(|(p, n)| PoseTarget {
                position: place.transform_point3(*p),
                normal: place.transform_vector3(*n).normalize_or_zero(),
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(ClothPreview {
        solver,
        steps: 0,
        targets,
    })
}

/// Runs `steps` fixed steps of the simulate preview (building it first when needed). Returns the
/// steps run in total.
///
/// # Errors
/// `cloth_panel_no_doc`, `cloth_panel_no_sim_mesh`, `cloth_panel_bad_value`.
pub fn step_preview(world: &mut World, steps: u32) -> Result<u64, PanelError> {
    let needs = world
        .get_resource::<ClothPanel>()
        .is_none_or(|p| p.preview.is_none());
    if needs {
        let built = build_preview(world)?;
        panel_mut(world).preview = Some(built);
    }
    let _span = tracing::debug_span!("cloth_panel_preview", steps).entered();
    let mut panel = panel_mut(world);
    let Some(preview) = panel.preview.as_mut() else {
        return Ok(0);
    };
    for _ in 0..steps {
        if !preview.targets.is_empty() {
            let t = preview.targets.clone();
            preview
                .solver
                .set_pose_targets(&t, &t)
                .map_err(|e| err("bad_value", e.to_string()))?;
        }
        preview
            .solver
            .step(PREVIEW_STEP)
            .map_err(|e| err("bad_value", e.to_string()))?;
        preview.steps += 1;
    }
    Ok(preview.steps)
}

/// One editor frame of the panel: steps the preview once while simulate is on.
pub fn cloth_panel_tick(world: &mut World) {
    if world
        .get_resource::<ClothPanel>()
        .is_some_and(|p| p.simulate && p.doc.is_some())
    {
        let result = step_preview(world, 1);
        if let Err(e) = result {
            let mut panel = panel_mut(world);
            panel.simulate = false;
            panel.error = Some(e);
        }
    }
}

// ------------------------------------------------------------------------------------- gizmos

/// One gizmo of the `cloth` group.
#[derive(Clone, Debug, PartialEq)]
pub struct ClothGizmoShape {
    /// `proxy`, `snapshot`, `reach` or `standoff`.
    pub kind: &'static str,
    /// The selected vertex a `reach` / `standoff` shape belongs to.
    pub vertex: Option<u32>,
    /// World-space segments.
    pub segments: Vec<Segment>,
}

const RING: usize = 16;

fn ring(out: &mut Vec<Segment>, centre: Vec3, axis: Vec3, radius: f32, color: egui::Color32) {
    let axis = axis.normalize_or(Vec3::Y);
    let u = axis.any_orthonormal_vector();
    let v = axis.cross(u);
    let point = |i: usize| {
        let a = std::f32::consts::TAU * i as f32 / RING as f32;
        centre + (u * a.cos() + v * a.sin()) * radius
    };
    for i in 0..RING {
        out.push((point(i), point(i + 1), color));
    }
}

fn sphere(centre: Vec3, rotation: Quat, radius: f32, color: egui::Color32) -> Vec<Segment> {
    let mut out = Vec::new();
    for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
        ring(&mut out, centre, rotation * axis, radius, color);
    }
    out
}

fn capsule(a: Vec3, b: Vec3, radius: f32, color: egui::Color32) -> Vec<Segment> {
    let axis = (b - a).normalize_or(Vec3::Y);
    let mut out = Vec::new();
    ring(&mut out, a, axis, radius, color);
    ring(&mut out, b, axis, radius, color);
    let u = axis.any_orthonormal_vector();
    let v = axis.cross(u);
    for side in [u, -u, v, -v] {
        out.push((a + side * radius, b + side * radius, color));
    }
    out
}

fn box_edges(world: Affine3A, half: Vec3, color: egui::Color32) -> Vec<Segment> {
    let corner = |i: u32| {
        let pick = |bit: u32, h: f32| if i & bit == 0 { -h } else { h };
        world.transform_point3(Vec3::new(pick(1, half.x), pick(2, half.y), pick(4, half.z)))
    };
    let mut out = Vec::new();
    for i in 0..8u32 {
        for bit in [1u32, 2, 4] {
            if i & bit == 0 {
                out.push((corner(i), corner(i | bit), color));
            }
        }
    }
    out
}

fn square(centre: Vec3, normal: Vec3, side: f32, color: egui::Color32) -> Vec<Segment> {
    let n = normal.normalize_or(Vec3::Z);
    let u = n.any_orthonormal_vector() * (side * 0.5);
    let v = n.cross(u);
    let c = [centre + u + v, centre - u + v, centre - u - v, centre + u - v];
    vec![
        (c[0], c[1], color),
        (c[1], c[2], color),
        (c[2], c[3], color),
        (c[3], c[0], color),
        (c[0], c[2], color),
    ]
}

fn snapshot_shape(kind: &SnapshotKind, rotation: Quat, translation: Vec3) -> Vec<Segment> {
    let place = Affine3A::from_rotation_translation(rotation, translation);
    match kind {
        SnapshotKind::Sphere { radius } => sphere(translation, rotation, *radius, CLOTH_SNAPSHOT),
        SnapshotKind::Capsule { half_height, radius } => capsule(
            place.transform_point3(Vec3::new(0.0, -half_height, 0.0)),
            place.transform_point3(Vec3::new(0.0, *half_height, 0.0)),
            *radius,
            CLOTH_SNAPSHOT,
        ),
        SnapshotKind::Box { half_extents } => box_edges(place, *half_extents, CLOTH_SNAPSHOT),
        SnapshotKind::Plane { normal, offset } => square(
            place.transform_point3(*normal * *offset),
            rotation * *normal,
            2.0,
            CLOTH_SNAPSHOT,
        ),
        SnapshotKind::Triangles { triangles } => triangles
            .iter()
            .flat_map(|t| {
                let p = t.map(|v| place.transform_point3(v));
                [
                    (p[0], p[1], CLOTH_SNAPSHOT),
                    (p[1], p[2], CLOTH_SNAPSHOT),
                    (p[2], p[0], CLOTH_SNAPSHOT),
                ]
            })
            .collect(),
        // A hull has planes only; its pose marks where it is.
        SnapshotKind::Hull { .. } => sphere(translation, rotation, 0.05, CLOTH_SNAPSHOT),
    }
}

/// The `cloth` gizmo group's shapes (empty while the group is off): every `ClothBodyProxy`, the
/// target solver's world-snapshot shapes (when a `ClothSet` runs it), and for each selected
/// vertex of the sim mesh a `reach` sphere (radius capped at [`REACH_DRAW_MAX`]) around it and a
/// `standoff` plane through `p - n · standoff` across its rest normal `n`.
#[must_use]
pub fn cloth_gizmo_shapes(world: &World) -> Vec<ClothGizmoShape> {
    let Some(panel) = world.get_resource::<ClothPanel>() else {
        return Vec::new();
    };
    if !panel.gizmos {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut proxies: Vec<(u64, Vec<Segment>)> = Vec::new();
    if let Some(mut q) = world.try_query::<(Entity, &ClothBodyProxy, &GlobalTransform)>() {
        for (entity, proxy, global) in q.iter(world) {
            let m = global.0;
            let segments = match proxy.shape {
                ProxyShape::Sphere { center, radius } => {
                    let (_, rotation, _) = m.to_scale_rotation_translation();
                    sphere(m.transform_point3(center), rotation, radius, CLOTH_PROXY)
                }
                ProxyShape::Capsule { a, b, radius } => {
                    capsule(m.transform_point3(a), m.transform_point3(b), radius, CLOTH_PROXY)
                }
            };
            proxies.push((entity.to_bits(), segments));
        }
    }
    proxies.sort_by_key(|(k, _)| *k);
    out.extend(proxies.into_iter().map(|(_, segments)| ClothGizmoShape {
        kind: "proxy",
        vertex: None,
        segments,
    }));
    let solver_name = panel
        .target
        .and_then(|t| world.get::<ClothGlueState>(t))
        .map(|s| s.solver().to_owned());
    if let (Some(name), Some(set)) = (solver_name, world.get_resource::<ClothSet>())
        && let Some(solver) = set.get(&name)
    {
        for shape in solver.snapshot().shapes() {
            out.push(ClothGizmoShape {
                kind: "snapshot",
                vertex: None,
                segments: snapshot_shape(&shape.kind, shape.cur.rotation, shape.cur.translation),
            });
        }
    }
    if let Some(asset) = panel.doc.as_ref().and_then(|d| d.asset.as_ref())
        && !panel.selected.is_empty()
    {
        let lod = asset.lod0();
        let place = target_world(world, panel.target);
        let mut normals = Vec::new();
        vertex_normals(&lod.rest_positions, &lod.triangles, &mut normals);
        let value = |map: &str, i: usize| {
            lod.maps
                .get(map)
                .and_then(|m| m.get(i).copied())
                .unwrap_or_else(|| regia::physics::cloth::asset::ClothMaps::default_of(map))
        };
        for &v in panel.selected.iter().take(MAX_SELECTED) {
            let i = v as usize;
            let (Some(rest), Some(n)) = (lod.rest_positions.get(i), normals.get(i)) else {
                continue;
            };
            let p = place.transform_point3(*rest);
            let n = place.transform_vector3(*n).normalize_or_zero();
            let reach = value("reach", i).clamp(0.0, REACH_DRAW_MAX);
            out.push(ClothGizmoShape {
                kind: "reach",
                vertex: Some(v),
                segments: sphere(p, Quat::IDENTITY, reach.max(1e-3), CLOTH_REACH),
            });
            out.push(ClothGizmoShape {
                kind: "standoff",
                vertex: Some(v),
                segments: square(
                    p - n * value("standoff", i),
                    n,
                    STANDOFF_PLANE_SIZE,
                    CLOTH_STANDOFF,
                ),
            });
        }
    }
    out
}

/// The panel's color for `value` on the ramp `[low, high]`: blue at or under `low`, through green,
/// to red at or over `high`.
#[must_use]
pub fn ramp_color(value: f32, ramp: [f32; 2]) -> egui::Color32 {
    let span = ramp[1] - ramp[0];
    let t = if span.abs() > f32::EPSILON && value.is_finite() {
        ((value - ramp[0]) / span).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let lerp = |a: f32, b: f32, t: f32| (a + (b - a) * t).round().clamp(0.0, 255.0) as u8;
    let (r, g, b) = if t < 0.5 {
        let k = t * 2.0;
        (lerp(40.0, 60.0, k), lerp(90.0, 210.0, k), lerp(230.0, 80.0, k))
    } else {
        let k = (t - 0.5) * 2.0;
        (lerp(60.0, 240.0, k), lerp(210.0, 70.0, k), lerp(80.0, 50.0, k))
    };
    egui::Color32::from_rgb(r, g, b)
}

/// Everything the panel draws in the viewport: the `cloth` gizmo group, the paint ramp (a cross at
/// each sim vertex, colored by the painted map) while painting, and the preview mesh while
/// simulating.
#[must_use]
pub fn cloth_gizmo_segments(world: &World) -> Vec<Segment> {
    let mut out: Vec<Segment> = cloth_gizmo_shapes(world)
        .into_iter()
        .flat_map(|s| s.segments)
        .collect();
    let Some(panel) = world.get_resource::<ClothPanel>() else {
        return out;
    };
    let Some(asset) = panel.doc.as_ref().and_then(|d| d.asset.as_ref()) else {
        return out;
    };
    let lod = asset.lod0();
    if panel.painting {
        let place = target_world(world, panel.target);
        let values = lod.maps.get(&panel.brush.map);
        let half = 0.01;
        for (i, rest) in lod.rest_positions.iter().enumerate() {
            let value = values
                .and_then(|m| m.get(i).copied())
                .unwrap_or_else(|| regia::physics::cloth::asset::ClothMaps::default_of(&panel.brush.map));
            let color = ramp_color(value, panel.ramp);
            let p = place.transform_point3(*rest);
            out.push((p - Vec3::X * half, p + Vec3::X * half, color));
            out.push((p - Vec3::Y * half, p + Vec3::Y * half, color));
        }
    }
    if let Some(preview) = &panel.preview {
        let particles = preview.solver.particles();
        for t in &lod.triangles {
            let p = t.map(|i| particles.get(i as usize).map_or(Vec3::ZERO, |q| q.position));
            out.push((p[0], p[1], CLOTH_PREVIEW));
            out.push((p[1], p[2], CLOTH_PREVIEW));
        }
    }
    out
}

/// The sim vertex hit by a world ray (nearest triangle hit, then its nearest corner) and the hit
/// point in model space, on the target-placed rest mesh.
#[must_use]
pub fn pick_sim_vertex(world: &World, origin: Vec3, direction: Vec3) -> Option<(u32, Vec3)> {
    let panel = world.get_resource::<ClothPanel>()?;
    let asset = panel.doc.as_ref()?.asset.as_ref()?;
    let lod = asset.lod0();
    let place = target_world(world, panel.target);
    let inverse = place.inverse();
    let (o, d) = (
        inverse.transform_point3(origin),
        inverse.transform_vector3(direction),
    );
    let mut best: Option<(f32, [u32; 3], Vec3)> = None;
    for t in &lod.triangles {
        let [a, b, c] = t.map(|i| lod.rest_positions.get(i as usize).copied().unwrap_or(Vec3::ZERO));
        // Möller–Trumbore ray / triangle test.
        let (e1, e2) = (b - a, c - a);
        let p = d.cross(e2);
        let det = e1.dot(p);
        if det.abs() < 1e-12 {
            continue;
        }
        let inv = 1.0 / det;
        let s = o - a;
        let u = s.dot(p) * inv;
        let q = s.cross(e1);
        let v = d.dot(q) * inv;
        let dist = e2.dot(q) * inv;
        if u < 0.0 || v < 0.0 || u + v > 1.0 || dist <= 0.0 {
            continue;
        }
        if best.is_none_or(|(bd, _, _)| dist < bd) {
            best = Some((dist, *t, o + d * dist));
        }
    }
    let (_, tri, hit) = best?;
    let nearest = tri.into_iter().min_by(|x, y| {
        let px = lod.rest_positions.get(*x as usize).copied().unwrap_or(Vec3::ZERO);
        let py = lod.rest_positions.get(*y as usize).copied().unwrap_or(Vec3::ZERO);
        px.distance_squared(hit).total_cmp(&py.distance_squared(hit))
    })?;
    Some((nearest, hit))
}

/// Paint mode's viewport input: left-press over the sim mesh starts a stroke, dragging adds points,
/// releasing paints the stroke ([`paint_stroke`], one undo step); Ctrl-click selects the vertex
/// under the cursor for the gizmos. Returns whether the panel used the pointer.
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: regia_core::math::Vec2,
) -> bool {
    let painting = world
        .get_resource::<ClothPanel>()
        .is_some_and(|p| p.painting && p.doc.is_some());
    if !painting {
        return false;
    }
    let (ctrl, pressed, down, released) = ui.input(|i| {
        (
            i.modifiers.command,
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
        )
    });
    let hit = response.hover_pos().and_then(|pointer| {
        let local = pointer - response.rect.left_top();
        let (origin, direction) =
            crate::viewport::pick_ray(camera, regia_core::math::Vec2::new(local.x, local.y), size);
        pick_sim_vertex(world, origin, direction)
    });
    if pressed && response.hovered() && ctrl {
        if let Some((v, _)) = hit {
            let mut panel = panel_mut(world);
            if let Some(pos) = panel.selected.iter().position(|s| *s == v) {
                panel.selected.remove(pos);
            } else if panel.selected.len() < MAX_SELECTED {
                panel.selected.push(v);
            }
        }
        return true;
    }
    if ((pressed && response.hovered()) || (down && !world.resource::<ClothPanel>().stroke.is_empty()))
        && let Some((_, p)) = hit
    {
        push_stroke_point(&mut panel_mut(world), p.to_array());
    }
    if released {
        let points = std::mem::take(&mut panel_mut(world).stroke);
        if !points.is_empty() {
            let brush = world.resource::<ClothPanel>().brush.clone();
            let result = paint_stroke(world, &points, &brush);
            panel_mut(world).error = result.err();
        }
    }
    pressed || down || released
}

/// Adds a drag point to the stroke being painted, up to [`MAX_STROKE_POINTS`] (review rc1-rev-editor-k
/// finding 14: the stroke grew without bound while the button was down, and at release the whole
/// over-long stroke was refused). Past the cap the point is dropped and the panel says so at once;
/// the release still paints the points kept.
fn push_stroke_point(panel: &mut ClothPanel, point: [f32; 3]) {
    if panel.stroke.len() < MAX_STROKE_POINTS {
        panel.stroke.push(point);
    } else if panel.error.is_none() {
        panel.error = Some(err(
            "bad_value",
            format!("a stroke has at most {MAX_STROKE_POINTS} points; release to paint them"),
        ));
    }
}

// ------------------------------------------------------------------------------------- panel UI

fn falloff_name(f: Falloff) -> &'static str {
    match f {
        Falloff::Linear => "linear",
        Falloff::Smooth => "smooth",
        Falloff::Hard => "hard",
    }
}

fn self_collision_parts(mode: SelfCollisionSetting) -> (&'static str, f32) {
    match mode {
        SelfCollisionSetting::Off => ("off", 0.0),
        SelfCollisionSetting::Spheres { radius } => ("spheres", radius),
        SelfCollisionSetting::VertexTriangle { radius } => ("vertex_triangle", radius),
    }
}

fn self_collision_of(mode: &str, radius: f32) -> Option<SelfCollisionSetting> {
    match mode {
        "off" => Some(SelfCollisionSetting::Off),
        "spheres" => Some(SelfCollisionSetting::Spheres { radius }),
        "vertex_triangle" => Some(SelfCollisionSetting::VertexTriangle { radius }),
        _ => None,
    }
}

/// Draws the panel (and steps the preview once per egui frame while simulate is on).
pub fn cloth_panel(ui: &mut egui::Ui, world: &mut World) {
    let frame = ui.ctx().cumulative_frame_nr();
    if world
        .get_resource::<ClothPanel>()
        .is_none_or(|p| p.ticked_frame != Some(frame))
    {
        panel_mut(world).ticked_frame = Some(frame);
        cloth_panel_tick(world);
    }
    if let Some(selected) = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        && (world.get::<ClothSimSource>(selected).is_some() || world.get::<SoftSimSource>(selected).is_some())
    {
        panel_mut(world).target = Some(selected);
    }
    let mut panel = std::mem::take(&mut *panel_mut(world));
    let mut open: Option<String> = None;
    let mut change: Option<ClothChange> = None;
    let mut instance: Option<InstanceChange> = None;
    let mut do_save = false;
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.path_input).hint_text("cloth/cape.cloth.ron"));
        if ui.button("Open").clicked() {
            open = Some(panel.path_input.clone());
        }
    });
    let cloth_target = panel.target.and_then(|t| world.get::<ClothSimSource>(t).cloned());
    let soft_target = panel.target.and_then(|t| world.get::<SoftSimSource>(t).cloned());
    if let Some(doc) = panel.doc.clone() {
        ui.horizontal(|ui| {
            ui.strong(doc.source.name());
            ui.weak(format!(
                "{} ({}){}",
                doc.path,
                doc.source.kind(),
                if doc.dirty() { " (edited)" } else { "" }
            ));
            if ui.add_enabled(doc.dirty(), egui::Button::new("Save")).clicked() {
                do_save = true;
            }
        });
        ui.separator();
        if let PanelSource::Cloth(c) = &doc.source {
            let mut preset = c.preset.clone();
            egui::ComboBox::from_id_salt("cloth-preset")
                .selected_text(format!("starting point: {}", preset.as_deref().unwrap_or("none")))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut preset, None, "none");
                    for p in ClothPreset::ALL {
                        ui.selectable_value(&mut preset, Some(p.name().to_owned()), p.name());
                    }
                });
            if preset != c.preset {
                change = Some(ClothChange::Preset(preset));
            }
        }
        egui::Grid::new("cloth-params").num_columns(2).show(ui, |ui| {
            let names = match &doc.source {
                PanelSource::Cloth(_) => CLOTH_PARAMS,
                PanelSource::Soft(_) => SOFT_PARAMS,
            };
            for name in names {
                let current = match &doc.source {
                    PanelSource::Cloth(c) => cloth_param(&c.params, name),
                    PanelSource::Soft(s) => soft_param(s, name),
                };
                let Some(mut v) = current else {
                    continue;
                };
                ui.label(*name);
                let whole = matches!(*name, "substeps" | "iterations");
                let drag = egui::DragValue::new(&mut v).speed(if whole { 0.1 } else { 0.001 });
                let drag = if whole {
                    drag.range(1.0..=64.0).fixed_decimals(0)
                } else {
                    drag.max_decimals(6)
                };
                if ui.add(drag).changed() {
                    change = Some(ClothChange::Param((*name).to_owned(), v));
                }
                ui.end_row();
            }
        });
        if let PanelSource::Cloth(c) = &doc.source {
            let (mode, mut radius) = self_collision_parts(c.params.self_collision);
            let mut picked = mode;
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("cloth-self")
                    .selected_text(format!("self-collision: {picked}"))
                    .show_ui(ui, |ui| {
                        for m in ["off", "spheres", "vertex_triangle"] {
                            ui.selectable_value(&mut picked, m, m);
                        }
                    });
                if picked != "off" {
                    if radius <= 0.0 {
                        radius = 0.01;
                    }
                    ui.add(
                        egui::DragValue::new(&mut radius)
                            .speed(0.001)
                            .range(0.0001..=1.0)
                            .prefix("r "),
                    );
                }
            });
            if let Some(next) = self_collision_of(picked, radius)
                && next != c.params.self_collision
            {
                change = Some(ClothChange::SelfCollision(next));
            }
        }
        if let Some(sim) = &cloth_target {
            ui.horizontal(|ui| {
                ui.label("blend");
                let mut b = sim.blend;
                if ui.add(egui::Slider::new(&mut b, 0.0..=1.0)).changed() {
                    instance = Some(InstanceChange::Blend(b));
                }
            });
        }
        if let Some(mut d) = cloth_target
            .as_ref()
            .map(|c| c.lod_distances)
            .or_else(|| soft_target.as_ref().map(|s| s.lod_distances))
        {
            ui.horizontal(|ui| {
                ui.label("LOD m");
                let before = d;
                for v in &mut d {
                    ui.add(egui::DragValue::new(v).speed(0.1).range(0.1..=10_000.0));
                }
                if d != before {
                    instance = Some(InstanceChange::LodDistances(d));
                }
            });
        } else {
            ui.weak("Select an entity with a ClothSimSource to edit its blend and LOD distances.");
        }
        ui.separator();
        if matches!(doc.source, PanelSource::Cloth(_)) {
            ui.horizontal(|ui| {
                ui.checkbox(&mut panel.painting, "Paint");
                egui::ComboBox::from_id_salt("cloth-map")
                    .selected_text(format!("map: {}", panel.brush.map))
                    .show_ui(ui, |ui| {
                        for m in MAP_NAMES {
                            ui.selectable_value(&mut panel.brush.map, m.to_owned(), m);
                        }
                    });
                ui.add(
                    egui::DragValue::new(&mut panel.brush.value)
                        .speed(0.001)
                        .prefix("value "),
                );
                ui.add(
                    egui::DragValue::new(&mut panel.brush.radius)
                        .speed(0.001)
                        .range(0.001..=10.0)
                        .prefix("r "),
                );
                ui.add(
                    egui::DragValue::new(&mut panel.brush.strength)
                        .speed(0.01)
                        .range(0.0..=1.0)
                        .prefix("str "),
                );
                let mut falloff = panel.brush.falloff;
                egui::ComboBox::from_id_salt("cloth-falloff")
                    .selected_text(falloff_name(falloff))
                    .show_ui(ui, |ui| {
                        for f in [Falloff::Linear, Falloff::Smooth, Falloff::Hard] {
                            ui.selectable_value(&mut falloff, f, falloff_name(f));
                        }
                    });
                panel.brush.falloff = falloff;
            });
            ui.horizontal(|ui| {
                ui.label("ramp m");
                ui.add(egui::DragValue::new(&mut panel.ramp[0]).speed(0.001));
                // The ramp's swatches, low to high.
                for k in 0..=8 {
                    let v = panel.ramp[0] + (panel.ramp[1] - panel.ramp[0]) * k as f32 / 8.0;
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 0.0, ramp_color(v, panel.ramp));
                }
                ui.add(egui::DragValue::new(&mut panel.ramp[1]).speed(0.001));
                ui.checkbox(&mut panel.simulate, "Simulate preview");
                ui.checkbox(&mut panel.gizmos, "Gizmos")
                    .on_hover_text("Gizmo group `cloth`");
            });
            match &doc.asset {
                Some(asset) => {
                    ui.weak(format!(
                        "sim mesh ({}): {} vertices, {} triangles; {} selected (Ctrl-click in paint mode)",
                        doc.sim_from,
                        asset.lod0().rest_positions.len(),
                        asset.lod0().triangles.len(),
                        panel.selected.len()
                    ));
                }
                None => {
                    ui.weak(format!(
                        "no sim mesh: {}",
                        doc.sim_error.as_deref().unwrap_or("not found")
                    ));
                }
            }
            if let Some(p) = &panel.preview {
                ui.weak(format!("preview: {} steps", p.steps));
            }
        }
    } else {
        ui.weak("Open a .cloth.ron or .soft.ron from the project to edit and paint it.");
    }
    if !panel.simulate {
        panel.preview = None;
    }
    if let Some(e) = &panel.error {
        e.draw(ui);
    }
    *panel_mut(world) = panel;

    let mut result: Result<(), PanelError> = Ok(());
    if let Some(path) = open {
        result = open_file(world, &path);
    }
    if let Some(change) = change {
        result = result.and_then(|()| edit_file(world, &change).map(|_| ()));
    }
    if let Some(change) = instance {
        result = result.and_then(|()| edit_instance(world, &change).map(|_| ()));
    }
    if do_save {
        result = result.and_then(|()| save_file(world).map(|_| ()));
    }
    if let Err(e) = result {
        panel_mut(world).error = Some(e);
    }
}

// ------------------------------------------------------------------------------------- agent

fn wind_json(w: WindSetting) -> Value {
    match w {
        WindSetting::Aerodynamic { drag, lift } => {
            json!({ "mode": "aerodynamic", "drag": drag, "lift": lift })
        }
        WindSetting::Push { gain } => json!({ "mode": "push", "gain": gain }),
    }
}

fn source_json(source: &PanelSource) -> Value {
    match source {
        PanelSource::Cloth(c) => {
            let params: serde_json::Map<String, Value> = CLOTH_PARAMS
                .iter()
                .filter_map(|n| cloth_param(&c.params, n).map(|v| ((*n).to_owned(), json!(v))))
                .collect();
            let (mode, radius) = self_collision_parts(c.params.self_collision);
            json!({
                "kind": "cloth",
                "name": c.name,
                "preset": c.preset,
                "params": params,
                "tether": c.params.tether,
                "self_collision": { "mode": mode, "radius": radius },
                "wind_model": wind_json(c.params.wind_model),
                "maps": c.maps.keys().collect::<Vec<_>>(),
            })
        }
        PanelSource::Soft(s) => {
            let params: serde_json::Map<String, Value> = SOFT_PARAMS
                .iter()
                .filter_map(|n| soft_param(s, n).map(|v| ((*n).to_owned(), json!(v))))
                .collect();
            json!({ "kind": "soft", "name": s.name, "params": params, "maps": s.maps.keys().collect::<Vec<_>>() })
        }
    }
}

fn falloff_of(s: &str) -> Result<Falloff, PanelError> {
    match s {
        "linear" => Ok(Falloff::Linear),
        "smooth" => Ok(Falloff::Smooth),
        "hard" => Ok(Falloff::Hard),
        other => Err(err(
            "bad_value",
            format!("`falloff` must be linear, smooth or hard, got `{other}`"),
        )),
    }
}

/// The panel as the agent reads it.
#[must_use]
pub fn state_json(world: &World) -> Value {
    let default = ClothPanel::default();
    let panel = world.get_resource::<ClothPanel>().unwrap_or(&default);
    let shapes = cloth_gizmo_shapes(world);
    let count = |k: &str| shapes.iter().filter(|s| s.kind == k).count();
    let instance = panel.target.map(|t| {
        let cloth = world.get::<ClothSimSource>(t);
        let soft = world.get::<SoftSimSource>(t);
        json!({
            "entity": t.to_bits(),
            "blend": cloth.map(|c| c.blend),
            "lod_distances": cloth.map(|c| c.lod_distances).or_else(|| soft.map(|s| s.lod_distances)),
        })
    });
    let doc = panel.doc.as_ref();
    json!({
        "open": doc.is_some(),
        "path": doc.map(|d| d.path.clone()),
        "dirty": doc.is_some_and(ClothDoc::dirty),
        "source": doc.map(|d| source_json(&d.source)),
        "sim_mesh": doc.and_then(|d| d.asset.as_ref()).map(|a| json!({
            "from": doc.map_or("none", |d| d.sim_from),
            "vertices": a.lod0().rest_positions.len(),
            "triangles": a.lod0().triangles.len(),
            "dirty": a.dirty,
        })),
        "sim_error": doc.and_then(|d| d.sim_error.clone()),
        "instance": instance,
        "paint": {
            "on": panel.painting,
            "map": panel.brush.map,
            "value": panel.brush.value,
            "radius": panel.brush.radius,
            "falloff": falloff_name(panel.brush.falloff),
            "strength": panel.brush.strength,
            "spacing": panel.brush.spacing,
            "ramp": panel.ramp,
        },
        "simulate": panel.simulate,
        "preview": panel.preview.as_ref().map(|p| json!({
            "steps": p.steps,
            "digest": format!("{:016x}", p.solver.state_digest()),
            "centroid": p.solver.centroid().to_array(),
        })),
        "gizmos": panel.gizmos,
        "selected": panel.selected,
        "gizmo_shapes": {
            "proxy": count("proxy"),
            "snapshot": count("snapshot"),
            "reach": count("reach"),
            "standoff": count("standoff"),
        },
        "undo_depth": world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
        "redo_depth": world.get_resource::<UndoStack>().map_or(0, UndoStack::redo_depth),
        "error": panel.error.as_ref().map(PanelError::json),
    })
}

fn open_and_target(world: &mut World, params: &Value) -> Result<(), RpcError> {
    if let Some(bits) = params.get("entity").filter(|v| !v.is_null()) {
        let bits = bits
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`entity` must be entity bits (an integer)"))?;
        let entity = crate::command::resolve_entity(world, bits)
            .ok_or_else(|| err("no_target", format!("no entity {bits}")).rpc(HINT))?;
        panel_mut(world).target = Some(entity);
    }
    if let Some(path) = params.get("path").and_then(Value::as_str) {
        open_file(world, path).map_err(|e| e.rpc(HINT))?;
    }
    Ok(())
}

fn cloth_panel_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(bits) = params.get("entity").filter(|v| !v.is_null()) {
        let bits = bits
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`entity` must be entity bits (an integer)"))?;
        if crate::command::resolve_entity(world, bits).is_none() {
            return Err(err("no_target", format!("no entity {bits}")).rpc(HINT));
        }
    }
    Ok(state_json(world))
}

fn num(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    crate::data_panels::opt_f32(params, key)
}

fn bool_param(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    }
}

/// The file changes an edit call asks for, in a fixed order.
fn changes_of(params: &Value) -> Result<Vec<ClothChange>, RpcError> {
    let mut out = Vec::new();
    match params.get("preset") {
        None => {}
        Some(Value::Null) => out.push(ClothChange::Preset(None)),
        Some(Value::String(s)) => out.push(ClothChange::Preset(Some(s.clone()))),
        Some(_) => return Err(RpcError::invalid_params("`preset` must be a preset name or null")),
    }
    if let Some(p) = params.get("params") {
        let map = p
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("`params` must be an object { name: number }"))?;
        for (name, v) in map {
            let v = v
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`params.{name}` must be a number")))?;
            out.push(ClothChange::Param(name.clone(), v as f32));
        }
    }
    match params.get("tether") {
        None => {}
        Some(Value::Null) => out.push(ClothChange::Tether(None)),
        Some(v) => out.push(ClothChange::Tether(Some(
            v.as_f64()
                .ok_or_else(|| RpcError::invalid_params("`tether` must be a number or null"))?
                as f32,
        ))),
    }
    if let Some(sc) = params.get("self_collision") {
        let mode = sc.get("mode").and_then(Value::as_str).ok_or_else(|| {
            RpcError::invalid_params("`self_collision.mode` must be off, spheres or vertex_triangle")
        })?;
        let radius = sc.get("radius").and_then(Value::as_f64).unwrap_or(0.01) as f32;
        let setting = self_collision_of(mode, radius).ok_or_else(|| {
            RpcError::invalid_params("`self_collision.mode` must be off, spheres or vertex_triangle")
        })?;
        out.push(ClothChange::SelfCollision(setting));
    }
    if let Some(w) = params.get("wind_model") {
        let get = |k: &str, d: f64| w.get(k).and_then(Value::as_f64).unwrap_or(d) as f32;
        let model = match w.get("mode").and_then(Value::as_str) {
            Some("aerodynamic") => WindSetting::Aerodynamic {
                drag: get("drag", 1.0),
                lift: get("lift", 0.3),
            },
            Some("push") => WindSetting::Push {
                gain: get("gain", 1.0),
            },
            _ => {
                return Err(RpcError::invalid_params(
                    "`wind_model.mode` must be aerodynamic or push",
                ));
            }
        };
        out.push(ClothChange::WindModel(model));
    }
    Ok(out)
}

fn brush_of(base: &ClothBrush, params: &Value) -> Result<ClothBrush, RpcError> {
    let mut brush = base.clone();
    if let Some(m) = params.get("map").and_then(Value::as_str) {
        m.clone_into(&mut brush.map);
    }
    if let Some(v) = num(params, "value")? {
        brush.value = v;
    }
    if let Some(v) = num(params, "radius")? {
        brush.radius = v;
    }
    if let Some(v) = num(params, "strength")? {
        brush.strength = v;
    }
    if let Some(f) = params.get("falloff").and_then(Value::as_str) {
        brush.falloff = falloff_of(f).map_err(|e| e.rpc(HINT))?;
    }
    match params.get("spacing") {
        None => {}
        Some(Value::Null) => brush.spacing = None,
        Some(_) => brush.spacing = num(params, "spacing")?,
    }
    brush.check().map_err(|e| e.rpc(HINT))?;
    Ok(brush)
}

fn cloth_panel_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    open_and_target(world, params)?;
    let changes = changes_of(params)?;
    if !changes.is_empty() {
        // All or nothing: validate every change on a copy first.
        let mut probe = snapshot(world).map_err(|e| e.rpc(HINT))?.source;
        for c in &changes {
            apply_change(&mut probe, c).map_err(|e| e.rpc(HINT))?;
        }
        for c in &changes {
            edit_file(world, c).map_err(|e| e.rpc(HINT))?;
        }
    }
    if let Some(b) = num(params, "blend")? {
        edit_instance(world, &InstanceChange::Blend(b)).map_err(|e| e.rpc(HINT))?;
    }
    if let Some(d) = params.get("lod_distances").filter(|v| !v.is_null()) {
        let arr: Vec<f32> = d
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_f64).map(|v| v as f32).collect())
            .unwrap_or_default();
        let d: [f32; 3] = arr
            .try_into()
            .map_err(|_| RpcError::invalid_params("`lod_distances` must be three numbers"))?;
        edit_instance(world, &InstanceChange::LodDistances(d)).map_err(|e| e.rpc(HINT))?;
    }
    if let Some(on) = bool_param(params, "gizmos")? {
        panel_mut(world).gizmos = on;
    }
    if let Some(sel) = params.get("selected").filter(|v| !v.is_null()) {
        let list: Vec<u32> = sel
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`selected` must be an array of vertex indices"))?
            .iter()
            .map(|v| v.as_u64().and_then(|v| u32::try_from(v).ok()))
            .collect::<Option<Vec<u32>>>()
            .ok_or_else(|| RpcError::invalid_params("`selected` must hold vertex indices"))?;
        if list.len() > MAX_SELECTED {
            return Err(err("bad_value", format!("at most {MAX_SELECTED} selected vertices")).rpc(HINT));
        }
        let len = world
            .get_resource::<ClothPanel>()
            .and_then(|p| p.doc.as_ref())
            .and_then(|d| d.asset.as_ref())
            .map(|a| a.lod0().rest_positions.len());
        if let Some(len) = len
            && let Some(bad) = list.iter().find(|&&v| v as usize >= len)
        {
            return Err(err(
                "bad_value",
                format!("vertex {bad} is past the sim mesh's {len} vertices"),
            )
            .rpc(HINT));
        }
        panel_mut(world).selected = list;
    }
    if let Some(brush) = params.get("paint").filter(|v| !v.is_null()) {
        let base = world
            .get_resource::<ClothPanel>()
            .map(|p| p.brush.clone())
            .unwrap_or_default();
        let next = brush_of(&base, brush)?;
        let mut panel = panel_mut(world);
        panel.brush = next;
        if let Some(on) = bool_param(brush, "on")? {
            panel.painting = on;
        }
        if let Some(r) = brush.get("ramp").and_then(Value::as_array) {
            let r: Vec<f32> = r.iter().filter_map(Value::as_f64).map(|v| v as f32).collect();
            if let [lo, hi] = r[..]
                && lo.is_finite()
                && hi.is_finite()
            {
                panel.ramp = [lo, hi];
            } else {
                return Err(err("bad_value", "`paint.ramp` must be two finite numbers").rpc(HINT));
            }
        }
    }
    if let Some(on) = bool_param(params, "simulate")? {
        let mut panel = panel_mut(world);
        panel.simulate = on;
        if !on {
            panel.preview = None;
        }
    }
    if let Some(steps) = params.get("preview_steps").filter(|v| !v.is_null()) {
        let steps = steps
            .as_u64()
            .and_then(|s| u32::try_from(s).ok())
            .filter(|s| (1..=MAX_PREVIEW_STEPS).contains(s))
            .ok_or_else(|| {
                err(
                    "bad_value",
                    format!("`preview_steps` must be 1..={MAX_PREVIEW_STEPS}"),
                )
                .rpc(HINT)
            })?;
        step_preview(world, steps).map_err(|e| e.rpc(HINT))?;
    }
    if bool_param(params, "undo")? == Some(true) {
        crate::command::undo_world(world);
    }
    if bool_param(params, "redo")? == Some(true) {
        crate::command::redo_world(world);
    }
    let mut written = None;
    if bool_param(params, "save")? == Some(true) {
        written = Some(save_file(world).map_err(|e| e.rpc(HINT))?);
    }
    let mut out = state_json(world);
    out["written"] = json!(written);
    Ok(out)
}

fn cloth_paint_stroke(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    open_and_target(world, params)?;
    let points: Vec<[f32; 3]> = params
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`points` must be an array of [x, y, z] (model space)"))?
        .iter()
        .map(|p| {
            let a = p.as_array()?;
            let v: Vec<f32> = a.iter().filter_map(Value::as_f64).map(|v| v as f32).collect();
            <[f32; 3]>::try_from(v).ok()
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| RpcError::invalid_params("each point must be [x, y, z]"))?;
    let base = world
        .get_resource::<ClothPanel>()
        .map(|p| p.brush.clone())
        .unwrap_or_default();
    let brush = brush_of(&base, params)?;
    let result = paint_stroke(world, &points, &brush).map_err(|e| e.rpc(HINT))?;
    let mut out = json!({
        "dabs": result.dabs,
        "changed": result.changed,
        "instances": result.instances,
        "map": brush.map,
        "undo_depth": world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
    });
    if bool_param(params, "values")? == Some(true) {
        out["values"] = json!(
            world
                .get_resource::<ClothPanel>()
                .and_then(|p| p.doc.as_ref())
                .and_then(|d| d.asset.as_ref())
                .and_then(|a| a.lod0().maps.get(&brush.map).cloned())
        );
    }
    Ok(out)
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

const STATE_PARAMS: &[ParamSchema] = &[param(
    "entity",
    "number",
    false,
    "Check that this entity exists (the state is the panel's).",
)];

const EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        false,
        "Project-relative .cloth.ron or .soft.ron to open first.",
    ),
    param(
        "entity",
        "number",
        false,
        "The instance (ClothSimSource / SoftSimSource entity) to edit and draw on.",
    ),
    param(
        "params",
        "object",
        false,
        "{ name: number } for the CLOTH_PARAMS / SOFT_PARAMS names (each one undo step).",
    ),
    param(
        "preset",
        "string",
        false,
        "The starting point: a preset name (cloth_presets lists them) or null.",
    ),
    param("tether", "number", false, "Tether scale (>= 1) or null (cloth)."),
    param(
        "self_collision",
        "object",
        false,
        "{ mode: off | spheres | vertex_triangle, radius } (cloth).",
    ),
    param(
        "wind_model",
        "object",
        false,
        "{ mode: aerodynamic, drag, lift } or { mode: push, gain } (cloth).",
    ),
    param(
        "blend",
        "number",
        false,
        "The instance's blend 0..=1 (its ClothSimSource).",
    ),
    param(
        "lod_distances",
        "array",
        false,
        "The instance's [d1, d2, d_off] meters, ascending.",
    ),
    param(
        "gizmos",
        "boolean",
        false,
        "The `cloth` gizmo group (off by default).",
    ),
    param(
        "selected",
        "array",
        false,
        "Selected sim vertices (the gizmos draw their reach and standoff).",
    ),
    param(
        "paint",
        "object",
        false,
        "Brush: { on, map, value, radius, falloff, strength, spacing, ramp: [low, high] }.",
    ),
    param(
        "simulate",
        "boolean",
        false,
        "The simulate preview (a private solver; the level is not touched).",
    ),
    param("preview_steps", "number", false, "Run 1..=600 preview steps now."),
    param(
        "undo",
        "boolean",
        false,
        "Undo the last editor change (the shared undo stack).",
    ),
    param("redo", "boolean", false, "Redo."),
    param(
        "save",
        "boolean",
        false,
        "Save the file (atomic; unedited files keep their bytes).",
    ),
];

const STROKE_PARAMS: &[ParamSchema] = &[
    param(
        "points",
        "array",
        true,
        "Stroke points [[x, y, z], ...], model space on the rest pose.",
    ),
    param("path", "string", false, "Open this file first."),
    param("entity", "number", false, "The instance the stroke is placed by."),
    param(
        "map",
        "string",
        false,
        "reach, standoff, pose_pull, sim_mix, mass_scale or layer (default: the panel's).",
    ),
    param("value", "number", false, "Value painted toward."),
    param("radius", "number", false, "Brush radius, meters."),
    param("falloff", "string", false, "linear, smooth or hard."),
    param("strength", "number", false, "Blend per dab 0..=1."),
    param(
        "spacing",
        "number",
        false,
        "Dab spacing in meters (default radius / 4; 0 = only the points).",
    ),
    param(
        "values",
        "boolean",
        false,
        "Also return the painted map's values.",
    ),
];

const STATE_RESULT: &str = "{ open, path, dirty, source: { kind, name, preset?, params, tether?, self_collision?, wind_model?, maps }, \
    sim_mesh: { from, vertices, triangles, dirty } | null, sim_error, instance: { entity, blend, lod_distances } | null, \
    paint: { on, map, value, radius, falloff, strength, spacing, ramp }, simulate, preview: { steps, digest, centroid } | null, \
    gizmos, selected, gizmo_shapes: { proxy, snapshot, reach, standoff }, undo_depth, redo_depth, error }";

/// The cloth panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.cloth_panel_state",
                tool: "cloth_panel_state",
                description: "The cloth inspector and paint mode (spec 55 § 6): the open .cloth.ron / .soft.ron, its \
                    parameters, the target instance's blend and LOD distances, the brush, the preview and the `cloth` \
                    gizmo counts.",
                params: STATE_PARAMS,
                result: STATE_RESULT,
                mutating: false,
                permission: Permission::Read,
            },
            cloth_panel_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.cloth_panel_edit",
                tool: "cloth_panel_edit",
                description: "Edit through the cloth panel: open a file, change parameters, the starting point, \
                    self-collision, wind model, the instance's blend and LOD distances (each change one undo step), set \
                    the brush, gizmos, selection and simulate preview, undo / redo, save atomically.",
                params: EDIT_PARAMS,
                result: "the editor.cloth_panel_state result plus { written }",
                mutating: true,
                permission: Permission::Assets,
            },
            cloth_panel_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.cloth_paint_stroke",
                tool: "cloth_paint_stroke",
                description: "Paint one stroke on the open cloth's sim mesh: spec 55 § 3.4 sphere edits at the returned \
                    dabs, through the code physics.cloth_edit_map runs (one undo step; ClothSim instances get it).",
                params: STROKE_PARAMS,
                result: "{ dabs: [[x, y, z]], changed, instances, map, undo_depth, values? }",
                mutating: true,
                permission: Permission::Assets,
            },
            cloth_paint_stroke as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
