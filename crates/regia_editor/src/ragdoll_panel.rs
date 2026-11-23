// SPDX-License-Identifier: MPL-2.0
//! The Ragdoll editor (ADR-2031): the physics asset of a character, a `.ragdoll.ron` profile (spec 93 § 3) —
//! its bodies (one per simulated joint: shape (capsule, sphere or box), radius, mass, length) and joints (the swing-twist limits to
//! the nearest simulated ancestor). Agent surface: `editor.ragdoll_panel_*` (`book/agents/editor-data-panels.md`).
//!
//! A rig picked in the level (an entity with `SkinnedMesh`) gives the joint names to check against, the
//! humanoid generator (`RagdollProfile::humanoid_limited`) and the viewport gizmo: each body's shape in the
//! bind pose and, for a limited joint, its bend range. Edits stay in memory (undoable and redoable, 32
//! steps) until `Save` writes the file atomically in the current format. In the editor (a world
//! with the `UndoStack`) the steps are on the shared editor history: Edit ▸ Undo, Ctrl+Z and the
//! Undo History panel list them by file, the panel's Undo and Redo (and `op: undo` / `redo`) step
//! that history, and undoing an edit of a file that is no longer open opens it again.
//!
//! `Drop test` (ADR-2133) simulates the open profile on the rig's skeleton in a private physics world
//! (`regia_framework::ragdoll::preview`): the ragdoll falls limp onto a floor, and the panel shows
//! whether it settled, how fast it moved, how deep it sank and the final pose, drawn in the viewport
//! beside the rig. The level is not touched. Agent: `editor.ragdoll_panel_preview`.

use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_animation::SkinnedMesh;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_framework::ragdoll::preview::{RagdollPreview, RagdollPreviewOptions, preview_drop};
use regia_framework::ragdoll::{RagdollBone, RagdollFlex, RagdollLimits, RagdollProfile, RagdollShape};
use regia_scene::prelude::{GlobalTransform, Transform};
use serde_json::{Value, json};

use crate::audio_gizmo::Segment;
use crate::data_panels::{PanelError, confined, opt_f32, str_param, write_atomic};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/editor-data-panels.md";
const PREFIX: &str = "ragdoll_panel";
const SUFFIX: &str = ".ragdoll.ron";
/// Undo steps kept.
pub const UNDO_DEPTH: usize = 32;

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("{PREFIX}_{code}"), message)
}

/// One finding about the open profile.
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollIssue {
    /// `ragdoll_unknown_joint`, `ragdoll_duplicate_joint`, `ragdoll_limit_range`, `ragdoll_empty`.
    pub code: &'static str,
    /// `error` or `warning`.
    pub severity: &'static str,
    /// The bone's joint, when it is about one.
    pub joint: Option<String>,
    /// What is wrong.
    pub message: String,
}

/// The Ragdoll editor's state (an edit-world resource).
#[derive(Resource)]
pub struct RagdollPanel {
    /// The open file, project-relative (`None` with a profile = new, not saved yet).
    pub path: Option<String>,
    /// The profile being edited.
    pub profile: Option<RagdollProfile>,
    /// The rig the profile is checked against and drawn on.
    pub rig: Option<Entity>,
    /// The selected bone (index into `profile.bones`).
    pub selected: Option<usize>,
    /// Whether the viewport draws the bodies.
    pub show_gizmo: bool,
    /// Whether there are unsaved edits.
    pub dirty: bool,
    /// Bumped by every applied edit.
    pub generation: u64,
    /// The last failed operation.
    pub error: Option<PanelError>,
    /// The last drop test and the `generation` it simulated.
    pub preview: Option<(u64, RagdollPreview)>,
    undo: Vec<RagdollProfile>,
    redo: Vec<RagdollProfile>,
    path_input: String,
    add_joint: String,
    /// Hosted in an editor world with an `UndoStack`: edits, undo and redo use the shared editor
    /// history instead of `undo` / `redo`.
    shared: bool,
    /// The shared history's (undo, redo) depths when the panel last saw it.
    shared_depths: (usize, usize),
    /// Edits of this call, moved to the shared history when the call ends.
    pending: Vec<RagdollEdit>,
    /// A shared-history undo (`false`) or redo (`true`) to run when the call ends.
    history_request: Option<bool>,
}

/// One ragdoll edit on the shared editor history: the profile before and after.
struct RagdollEdit {
    path: String,
    op: String,
    joint: Option<String>,
    before: RagdollProfile,
    after: RagdollProfile,
}

impl RagdollEdit {
    /// Puts `profile` into the panel, reactivating this edit's file when another one is open.
    fn restore(&self, world: &mut World, profile: &RagdollProfile) {
        let mut panel = world.remove_resource::<RagdollPanel>().unwrap_or_default();
        if panel.path.as_deref() != Some(&self.path) {
            panel.path = Some(self.path.clone());
            self.path.clone_into(&mut panel.path_input);
            panel.selected = None;
        }
        panel.profile = Some(profile.clone());
        panel.selected = panel.selected.filter(|&i| i < profile.bones.len());
        panel.generation += 1;
        panel.dirty = true;
        panel.error = None;
        world.insert_resource(panel);
    }
}

impl crate::command::EditorCommand for RagdollEdit {
    fn apply(&mut self, world: &mut World) {
        self.restore(world, &self.after);
    }
    fn revert(&mut self, world: &mut World) {
        self.restore(world, &self.before);
    }
    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        vec![crate::command::CommandTouch::asset(
            self.path.clone(),
            self.joint.as_ref().map(|j| format!("bones/{j}")),
        )]
    }
    fn description(&self) -> String {
        match &self.joint {
            Some(joint) => format!("Ragdoll: {} {joint} in {}", self.op, self.path),
            None => format!("Ragdoll: {} in {}", self.op, self.path),
        }
    }
}

/// Runs a panel operation; in an editor world with an `UndoStack` its edits join the shared
/// history and its undo or redo runs there (once per egui `frame` when given).
fn with_shared<T>(
    world: &mut World,
    frame: Option<u64>,
    f: impl FnOnce(&mut RagdollPanel, &World, Option<&Path>) -> T,
) -> T {
    let _span = tracing::trace_span!(target: "regia_editor", "ragdoll_shared_history").entered();
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<RagdollPanel>().unwrap_or_default();
    let depths = shared_depths(world);
    panel.shared = depths.is_some();
    panel.shared_depths = depths.unwrap_or_default();
    let out = f(&mut panel, world, project.as_deref());
    if panel.shared {
        let mut stack = world.resource_mut::<crate::command::UndoStack>();
        for command in panel.pending.drain(..) {
            stack.seal();
            stack.push(Box::new(command));
            stack.seal();
        }
    }
    let request = panel.history_request.take();
    world.insert_resource(panel);
    if let Some(redo) = request {
        if let Some(frame) = frame {
            crate::command::navigate_world_once(world, frame, redo);
        } else if redo {
            crate::command::redo_world(world);
        } else {
            crate::command::undo_world(world);
        }
    }
    if let Some(depths) = shared_depths(world)
        && let Some(mut panel) = world.get_resource_mut::<RagdollPanel>()
    {
        panel.shared_depths = depths;
    }
    out
}

fn shared_depths(world: &World) -> Option<(usize, usize)> {
    world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| (s.undo_depth(), s.redo_depth()))
}

impl Default for RagdollPanel {
    fn default() -> Self {
        Self {
            path: None,
            profile: None,
            rig: None,
            selected: None,
            show_gizmo: true,
            dirty: false,
            generation: 0,
            error: None,
            preview: None,
            undo: Vec::new(),
            redo: Vec::new(),
            path_input: String::new(),
            add_joint: String::new(),
            shared: false,
            shared_depths: (0, 0),
            pending: Vec::new(),
            history_request: None,
        }
    }
}

fn limits_json(l: &RagdollLimits) -> Value {
    json!({
        "flex": match l.flex { RagdollFlex::Forward => "forward", RagdollFlex::Backward => "backward" },
        "bend": [l.bend.0, l.bend.1],
        "side": [l.side.0, l.side.1],
        "twist": [l.twist.0, l.twist.1],
    })
}

fn pair(v: &Value, key: &str) -> Result<Option<(f32, f32)>, PanelError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) if a.len() == 2 => {
            let x = a[0].as_f64().map(|x| x as f32).filter(|x| x.is_finite());
            let y = a[1].as_f64().map(|y| y as f32).filter(|y| y.is_finite());
            match (x, y) {
                (Some(x), Some(y)) => Ok(Some((x, y))),
                _ => Err(err(
                    "bad_value",
                    format!("`{key}` must be two finite numbers (radians)"),
                )),
            }
        }
        Some(_) => Err(err("bad_value", format!("`{key}` must be [min, max] in radians"))),
    }
}

/// `limits` from JSON over `current` (fields left out stay; a new limit starts at zero ranges, flexing forward).
/// The shape after an edit: `shape` (`capsule`, `sphere`, `box`) switches it; `width` and `depth` set a
/// box's extents (a new box starts at the bone's diameter on both). The file's checks refuse non-positive
/// extents.
fn shape_from(v: &Value, current: RagdollShape, radius: f32) -> Result<RagdollShape, PanelError> {
    let shape = match v.get("shape") {
        None | Some(Value::Null) => current,
        Some(Value::String(name)) => match (name.as_str(), current) {
            ("capsule", _) => RagdollShape::Capsule,
            ("sphere", _) => RagdollShape::Sphere,
            ("box", RagdollShape::Box { .. }) => current,
            ("box", _) => RagdollShape::Box {
                width: 2.0 * radius,
                depth: 2.0 * radius,
            },
            (other, _) => {
                return Err(err(
                    "bad_value",
                    format!("unknown shape `{other}` (capsule, sphere, box)"),
                ));
            }
        },
        Some(_) => {
            return Err(err(
                "bad_value",
                "`shape` must be \"capsule\", \"sphere\" or \"box\"",
            ));
        }
    };
    let width = opt_f32(v, "width").map_err(|e| err("bad_value", e.message))?;
    let depth = opt_f32(v, "depth").map_err(|e| err("bad_value", e.message))?;
    match shape {
        RagdollShape::Box { width: w, depth: d } => Ok(RagdollShape::Box {
            width: width.unwrap_or(w),
            depth: depth.unwrap_or(d),
        }),
        _ if width.is_some() || depth.is_some() => Err(err(
            "bad_value",
            "`width` and `depth` are box extents; this body is not a box",
        )),
        other => Ok(other),
    }
}

fn limits_from(v: &Value, current: Option<RagdollLimits>) -> Result<RagdollLimits, PanelError> {
    let mut l = current.unwrap_or(RagdollLimits {
        flex: RagdollFlex::Forward,
        bend: (0.0, 0.0),
        side: (0.0, 0.0),
        twist: (0.0, 0.0),
    });
    match v.get("flex").and_then(Value::as_str) {
        None => {}
        Some("forward") => l.flex = RagdollFlex::Forward,
        Some("backward") => l.flex = RagdollFlex::Backward,
        Some(other) => return Err(err("bad_value", format!("unknown flex `{other}`"))),
    }
    if let Some(p) = pair(v, "bend")? {
        l.bend = p;
    }
    if let Some(p) = pair(v, "side")? {
        l.side = p;
    }
    if let Some(p) = pair(v, "twist")? {
        l.twist = p;
    }
    for (name, (min, max)) in [("bend", l.bend), ("side", l.side), ("twist", l.twist)] {
        if min > max {
            return Err(PanelError::new(
                "ragdoll_bad_limits",
                format!("`{name}` min {min} is above max {max}"),
            ));
        }
    }
    Ok(l)
}

/// The rig's world matrix: its propagated `GlobalTransform`, or its local `Transform` while the global one is
/// still the identity it was spawned with (a rig placed this frame, a bare test world).
pub(crate) fn rig_matrix(world: &World, rig: Entity) -> Option<Mat4> {
    let local = world.get::<Transform>(rig);
    match world.get::<GlobalTransform>(rig) {
        Some(global)
            if *global != GlobalTransform::IDENTITY || local.is_none_or(|l| *l == Transform::IDENTITY) =>
        {
            Some(Mat4::from(global.0))
        }
        _ => local.map(|l| Mat4::from(l.compute_affine())),
    }
}

/// A rig's skeleton in its bind pose, in world space.
struct RigBind {
    names: Vec<String>,
    parents: Vec<Option<u32>>,
    globals: Vec<Mat4>,
}

/// The rig's joint names and bind globals in world space (rig transform applied).
fn rig_bind(world: &World, rig: Entity) -> Option<RigBind> {
    let mesh = world.get::<SkinnedMesh>(rig)?;
    let base = rig_matrix(world, rig).unwrap_or(Mat4::IDENTITY);
    let globals = mesh
        .skeleton
        .bind_globals()
        .into_iter()
        .map(|g| base * g)
        .collect();
    Some(RigBind {
        names: mesh.skeleton.joint_names.clone(),
        parents: mesh.skeleton.parents.clone(),
        globals,
    })
}

impl RagdollPanel {
    /// Undo steps available: the shared editor history's when hosted, else the panel's own.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        if self.shared {
            self.shared_depths.0 + self.pending.len()
        } else {
            self.undo.len()
        }
    }

    /// Redo steps available: the shared editor history's when hosted, else the panel's own.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        match (self.shared, self.pending.is_empty()) {
            (true, true) => self.shared_depths.1,
            (true, false) => 0,
            (false, _) => self.redo.len(),
        }
    }

    fn profile_mut(&mut self) -> Result<&mut RagdollProfile, PanelError> {
        self.profile
            .as_mut()
            .ok_or_else(|| err("no_profile", "open or create a ragdoll profile first"))
    }

    /// Opens `path` (project-relative `.ragdoll.ron`).
    ///
    /// # Errors
    /// `ragdoll_panel_no_project`, `ragdoll_panel_bad_path`, `ragdoll_panel_file_not_found`, or the file's
    /// code (`ragdoll_format_version`, `ragdoll_parse`, `ragdoll_bad_bone`).
    pub fn open(&mut self, project: Option<&Path>, path: &str) -> Result<(), PanelError> {
        let root = project.ok_or_else(|| err("no_project", "open a project first"))?;
        let file = confined(root, path, &[SUFFIX], PREFIX)?;
        let text =
            std::fs::read_to_string(&file).map_err(|e| err("file_not_found", format!("{path}: {e}")))?;
        let profile =
            RagdollProfile::from_ron(&text).map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        self.path = Some(path.to_owned());
        self.profile = Some(profile);
        self.selected = None;
        self.dirty = false;
        self.generation = 0;
        self.undo.clear();
        self.redo.clear();
        self.preview = None;
        Ok(())
    }

    /// Starts a new, empty profile that `save` writes to `path`.
    ///
    /// # Errors
    /// `ragdoll_panel_bad_path`.
    pub fn new_profile(&mut self, path: &str) -> Result<(), PanelError> {
        confined(Path::new("."), path, &[SUFFIX], PREFIX)?;
        self.path = Some(path.to_owned());
        self.profile = Some(RagdollProfile::default());
        self.selected = None;
        self.dirty = true;
        self.generation = 0;
        self.undo.clear();
        self.redo.clear();
        self.preview = None;
        Ok(())
    }

    fn bone_index(&self, params: &Value) -> Result<usize, PanelError> {
        let profile = self
            .profile
            .as_ref()
            .ok_or_else(|| err("no_profile", "open a ragdoll profile first"))?;
        if let Some(joint) = params.get("joint").and_then(Value::as_str) {
            return profile
                .bones
                .iter()
                .position(|b| b.joint == joint)
                .ok_or_else(|| err("bone_not_found", format!("no body for joint `{joint}`")));
        }
        if let Some(i) = params.get("index").and_then(Value::as_u64) {
            let i = usize::try_from(i).unwrap_or(usize::MAX);
            return if i < profile.bones.len() {
                Ok(i)
            } else {
                Err(err("bone_not_found", format!("no body {i}")))
            };
        }
        self.selected
            .ok_or_else(|| err("bone_not_found", "name a `joint` or an `index`"))
    }

    /// Applies one edit (`params.op`: `set_bone`, `add_bone`, `remove_bone`, `set_forward`, `humanoid`,
    /// `select`, `undo`, `redo`). `humanoid` needs the rig (`world`); the others ignore it. An edit
    /// clears the redo steps.
    ///
    /// # Errors
    /// `ragdoll_panel_no_profile`, `ragdoll_panel_bad_op`, `ragdoll_panel_bad_value`,
    /// `ragdoll_panel_bone_not_found`, `ragdoll_panel_duplicate_joint`, `ragdoll_panel_no_rig`,
    /// `ragdoll_panel_nothing_to_undo`, `ragdoll_panel_nothing_to_redo`, `ragdoll_bad_bone`,
    /// `ragdoll_bad_limits`; nothing changes.
    pub fn edit(&mut self, world: &World, params: &Value) -> Result<(), PanelError> {
        let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
        match op {
            "select" => {
                self.selected = match params.get("joint").or_else(|| params.get("index")) {
                    None | Some(Value::Null) => None,
                    Some(_) => Some(self.bone_index(params)?),
                };
                return Ok(());
            }
            "undo" | "redo" if self.shared => {
                self.profile_mut()?;
                let redo = op == "redo";
                let depth = if redo {
                    self.redo_depth()
                } else {
                    self.undo_depth()
                };
                if depth == 0 {
                    return Err(if redo {
                        err("nothing_to_redo", "nothing to redo")
                    } else {
                        err("nothing_to_undo", "nothing to undo")
                    });
                }
                self.history_request = Some(redo);
                return Ok(());
            }
            "undo" | "redo" => {
                self.profile_mut()?;
                let (from, to) = if op == "undo" {
                    (&mut self.undo, &mut self.redo)
                } else {
                    (&mut self.redo, &mut self.undo)
                };
                let previous = from.pop().ok_or_else(|| {
                    if op == "undo" {
                        err("nothing_to_undo", "nothing to undo")
                    } else {
                        err("nothing_to_redo", "nothing to redo")
                    }
                })?;
                let current = self.profile.replace(previous);
                if let Some(current) = current {
                    if to.len() == UNDO_DEPTH {
                        to.remove(0);
                    }
                    to.push(current);
                }
                self.generation += 1;
                self.dirty = true;
                self.selected = self
                    .selected
                    .filter(|&i| self.profile.as_ref().is_some_and(|p| i < p.bones.len()));
                return Ok(());
            }
            _ => {}
        }
        let mut next = self.profile_mut()?.clone();
        let number = |key: &str| opt_f32(params, key).map_err(|e| err("bad_value", e.message));
        let mut select = self.selected;
        match op {
            "set_bone" => {
                let i = self.bone_index(params)?;
                let bone = &mut next.bones[i];
                if let Some(r) = number("radius")? {
                    bone.radius = r;
                }
                if let Some(m) = number("mass")? {
                    bone.mass = m;
                }
                match params.get("length") {
                    None => {}
                    Some(Value::Null) => bone.length = None,
                    Some(_) => bone.length = number("length")?,
                }
                match params.get("limits") {
                    None => {}
                    Some(Value::Null) => bone.limits = None,
                    Some(v) => bone.limits = Some(limits_from(v, bone.limits)?),
                }
                bone.shape = shape_from(params, bone.shape, bone.radius)?;
                select = Some(i);
            }
            "add_bone" => {
                let joint = str_param(params, "joint")
                    .map_err(|e| err("bad_value", e.message))?
                    .to_owned();
                if next.bones.iter().any(|b| b.joint == joint) {
                    return Err(err("duplicate_joint", format!("`{joint}` already has a body")));
                }
                let radius = number("radius")?.unwrap_or(0.08);
                next.bones.push(RagdollBone {
                    joint,
                    radius,
                    mass: number("mass")?.unwrap_or(5.0),
                    length: number("length")?,
                    limits: match params.get("limits") {
                        None | Some(Value::Null) => None,
                        Some(v) => Some(limits_from(v, None)?),
                    },
                    shape: shape_from(params, RagdollShape::Capsule, radius)?,
                });
                select = Some(next.bones.len() - 1);
            }
            "remove_bone" => {
                let i = self.bone_index(params)?;
                next.bones.remove(i);
                select = None;
            }
            "set_forward" => {
                let f = params
                    .get("forward")
                    .and_then(Value::as_array)
                    .filter(|a| a.len() == 3)
                    .and_then(|a| {
                        let v: Vec<f32> = a.iter().filter_map(|x| x.as_f64().map(|x| x as f32)).collect();
                        (v.len() == 3).then(|| Vec3::new(v[0], v[1], v[2]))
                    })
                    .filter(|v| v.is_finite() && v.length_squared() > 1e-8)
                    .ok_or_else(|| err("bad_value", "`forward` must be a non-zero [x, y, z]"))?;
                next.forward = f.normalize();
            }
            "humanoid" => {
                let rig = self
                    .rig
                    .ok_or_else(|| err("no_rig", "pick a rig with a SkinnedMesh first"))?;
                let mesh = world
                    .get::<SkinnedMesh>(rig)
                    .ok_or_else(|| err("no_rig", "the rig has no SkinnedMesh"))?;
                let mass = number("mass")?.unwrap_or(75.0);
                if mass <= 0.0 {
                    return Err(err("bad_value", "`mass` must be positive"));
                }
                let limited = params.get("limited").and_then(Value::as_bool).unwrap_or(true);
                next = if limited {
                    RagdollProfile::humanoid_limited(&mesh.skeleton, mass, next.forward)
                } else {
                    let mut p = RagdollProfile::humanoid(&mesh.skeleton, mass);
                    p.forward = next.forward;
                    p
                };
                select = None;
            }
            other => return Err(err("bad_op", format!("unknown edit `{other}`"))),
        }
        // The file's own checks (positive radius and mass, joint names) refuse the edit.
        let text = next
            .to_ron()
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        RagdollProfile::from_ron(&text).map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        let joint = match op {
            "set_bone" | "remove_bone" => self
                .bone_index(params)
                .ok()
                .and_then(|i| Some(self.profile.as_ref()?.bones.get(i)?.joint.clone())),
            "add_bone" => params.get("joint").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        };
        let after = self.shared.then(|| next.clone());
        let previous = std::mem::replace(self.profile_mut()?, next);
        if let (Some(after), Some(path)) = (after, self.path.clone()) {
            self.pending.push(RagdollEdit {
                path,
                op: op.to_owned(),
                joint,
                before: previous,
                after,
            });
        } else {
            if self.undo.len() == UNDO_DEPTH {
                self.undo.remove(0);
            }
            self.undo.push(previous);
        }
        self.redo.clear();
        self.selected = select;
        self.generation += 1;
        self.dirty = true;
        Ok(())
    }

    /// Runs a drop test of the open profile on the rig's skeleton (module docs) and keeps it as the
    /// panel's preview. `params`: `steps` (1..=4000, default 250), `drop_height` (metres from the floor
    /// to the lowest joint's largest shape extent, default 0.3),
    /// `push` (`[x, y, z]` N·s on the root body, default none).
    ///
    /// # Errors
    /// `ragdoll_panel_no_profile`, `ragdoll_panel_no_rig`, `ragdoll_panel_bad_value`,
    /// `ragdoll_preview_bad_options`, `ragdoll_preview_no_bodies`.
    pub fn run_preview(&mut self, world: &World, params: &Value) -> Result<&RagdollPreview, PanelError> {
        let profile = self
            .profile
            .clone()
            .ok_or_else(|| err("no_profile", "open or create a ragdoll profile first"))?;
        let rig = self
            .rig
            .ok_or_else(|| err("no_rig", "pick a rig with a SkinnedMesh first"))?;
        let skeleton = world
            .get::<SkinnedMesh>(rig)
            .ok_or_else(|| err("no_rig", "the rig has no SkinnedMesh"))?
            .skeleton
            .clone();
        let mut options = RagdollPreviewOptions::default();
        match params.get("steps") {
            None | Some(Value::Null) => {}
            Some(v) => {
                options.steps = v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| err("bad_value", "`steps` must be a whole number"))?;
            }
        }
        if let Some(h) = opt_f32(params, "drop_height").map_err(|e| err("bad_value", e.message))? {
            options.drop_height = h;
        }
        match params.get("push") {
            None | Some(Value::Null) => {}
            Some(v) => {
                let p: Vec<f32> = v
                    .as_array()
                    .filter(|a| a.len() == 3)
                    .map(|a| a.iter().filter_map(|x| x.as_f64().map(|x| x as f32)).collect())
                    .unwrap_or_default();
                if p.len() != 3 {
                    return Err(err("bad_value", "`push` must be [x, y, z]"));
                }
                options.push = Vec3::new(p[0], p[1], p[2]);
            }
        }
        let preview = preview_drop(skeleton, &profile, &options)
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        let entry = self.preview.insert((self.generation, preview));
        Ok(&entry.1)
    }

    /// Writes the profile to its file (atomically, current format). Returns the path.
    ///
    /// # Errors
    /// `ragdoll_panel_no_project`, `ragdoll_panel_no_profile`, `ragdoll_panel_bad_path`,
    /// `ragdoll_panel_io_failed`.
    pub fn save(&mut self, project: Option<&Path>) -> Result<String, PanelError> {
        let root = project.ok_or_else(|| err("no_project", "open a project first"))?;
        let path = self
            .path
            .clone()
            .ok_or_else(|| err("no_profile", "open or create a ragdoll profile first"))?;
        let profile = self
            .profile
            .as_ref()
            .ok_or_else(|| err("no_profile", "open a ragdoll profile first"))?;
        let text = profile
            .to_ron()
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        let file = confined(root, &path, &[SUFFIX], PREFIX)?;
        write_atomic(&file, &text, PREFIX)?;
        self.dirty = false;
        Ok(path)
    }

    /// The findings about the profile (against the rig's joints when one is picked).
    #[must_use]
    pub fn issues(&self, world: &World) -> Vec<RagdollIssue> {
        let Some(profile) = &self.profile else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if profile.bones.is_empty() {
            out.push(RagdollIssue {
                code: "ragdoll_empty",
                severity: "warning",
                joint: None,
                message: "the profile has no bodies".to_owned(),
            });
        }
        let joints = self.rig.and_then(|r| rig_bind(world, r)).map(|bind| bind.names);
        for (i, bone) in profile.bones.iter().enumerate() {
            if profile.bones[..i].iter().any(|b| b.joint == bone.joint) {
                out.push(RagdollIssue {
                    code: "ragdoll_duplicate_joint",
                    severity: "error",
                    joint: Some(bone.joint.clone()),
                    message: format!("`{}` has more than one body", bone.joint),
                });
            }
            if let Some(names) = &joints
                && !names.contains(&bone.joint)
            {
                out.push(RagdollIssue {
                    code: "ragdoll_unknown_joint",
                    severity: "error",
                    joint: Some(bone.joint.clone()),
                    message: format!("the rig's skeleton has no joint `{}`", bone.joint),
                });
            }
            if let Some(l) = &bone.limits {
                let pi = std::f32::consts::PI;
                if [l.bend, l.side, l.twist].iter().any(|(a, b)| *a < -pi || *b > pi) {
                    out.push(RagdollIssue {
                        code: "ragdoll_limit_range",
                        severity: "warning",
                        joint: Some(bone.joint.clone()),
                        message: "a limit reaches past ±π".to_owned(),
                    });
                }
            }
        }
        out
    }

    /// The panel as the agent reads it.
    #[must_use]
    pub fn json(&self, world: &World) -> Value {
        json!({
            "path": self.path,
            "rig": self.rig.map(Entity::to_bits),
            "show_gizmo": self.show_gizmo,
            "dirty": self.dirty,
            "generation": self.generation,
            "undo_depth": self.undo_depth(),
            "redo_depth": self.redo_depth(),
            "preview": self.preview.as_ref().map(|(generation, p)| preview_json(p, *generation != self.generation)),
            "selected": self.selected,
            "profile": self.profile.as_ref().map(|p| json!({
                "forward": [p.forward.x, p.forward.y, p.forward.z],
                "total_mass": p.bones.iter().map(|b| b.mass).sum::<f32>(),
                "bones": p.bones.iter().map(|b| json!({
                    "joint": b.joint, "radius": b.radius, "mass": b.mass, "length": b.length,
                    "limits": b.limits.as_ref().map(limits_json),
                    "shape": shape_json(&b.shape),
                })).collect::<Vec<_>>(),
            })),
            "issues": self.issues(world).iter().map(|i| json!({
                "code": i.code, "severity": i.severity, "joint": i.joint, "message": i.message,
            })).collect::<Vec<_>>(),
            "gizmo_segments": ragdoll_gizmo_segments(world).len(),
            "error": self.error.as_ref().map(PanelError::json),
        })
    }
}

/// A body's shape as the agent reads it: `{ kind }`, plus `width` and `depth` for a box.
fn shape_json(shape: &RagdollShape) -> Value {
    match *shape {
        RagdollShape::Box { width, depth } => json!({ "kind": "box", "width": width, "depth": depth }),
        other => json!({ "kind": other.name() }),
    }
}

/// A drop test as the agent reads it; `stale` once the profile changed after it ran.
fn preview_json(p: &RagdollPreview, stale: bool) -> Value {
    json!({
        "stale": stale,
        "steps": p.steps,
        "settled": p.settled_at.is_some(),
        "settled_at": p.settled_at,
        "face_up": p.face_up,
        "max_speed": p.max_speed,
        "lowest": p.lowest,
        "finite": p.finite,
        "digest": format!("{:016x}", p.digest),
        "bodies": p.bodies.iter().map(|b| json!({
            "joint": b.joint,
            "parent": b.parent,
            "position": [b.position.x, b.position.y, b.position.z],
        })).collect::<Vec<_>>(),
    })
}

/// A circle of `radius` around `centre`, perpendicular to unit `axis`.
fn ring(out: &mut Vec<Segment>, centre: Vec3, axis: Vec3, radius: f32, color: egui::Color32) {
    const SIDES: usize = 12;
    let u = axis.any_orthonormal_vector();
    let v = axis.cross(u);
    let point = |i: usize| {
        let a = std::f32::consts::TAU * i as f32 / SIDES as f32;
        centre + (u * a.cos() + v * a.sin()) * radius
    };
    for i in 0..SIDES {
        out.push((point(i), point(i + 1), color));
    }
}

/// The 12 edges of a box: centre, unit axes and half extents along each.
fn box_edges(out: &mut Vec<Segment>, centre: Vec3, axes: [Vec3; 3], half: Vec3, color: egui::Color32) {
    let [x, y, z] = [axes[0] * half.x, axes[1] * half.y, axes[2] * half.z];
    let corner = |i: u32| {
        let pick = |bit: u32, v: Vec3| if i & bit == 0 { -v } else { v };
        centre + pick(1, x) + pick(2, y) + pick(4, z)
    };
    for i in 0..8u32 {
        for bit in [1u32, 2, 4] {
            if i & bit == 0 {
                out.push((corner(i), corner(i | bit), color));
            }
        }
    }
}

/// The viewport gizmo (empty without a rig, a profile, or with the gizmo off): per body, the axis from its
/// joint towards its child and its shape (a capsule's ring at each end, a sphere's three rings, a box's 12
/// edges), and for a limited joint the bend range as two lines rotated from the axis about the bend axis.
#[must_use]
pub fn ragdoll_gizmo_segments(world: &World) -> Vec<Segment> {
    let Some(panel) = world.get_resource::<RagdollPanel>() else {
        return Vec::new();
    };
    let (Some(profile), Some(rig), true) = (&panel.profile, panel.rig, panel.show_gizmo) else {
        return Vec::new();
    };
    let Some(RigBind {
        names,
        parents,
        globals,
    }) = rig_bind(world, rig)
    else {
        return Vec::new();
    };
    let base_rotation =
        rig_matrix(world, rig).map_or(Quat::IDENTITY, |m| m.to_scale_rotation_translation().1);
    let forward = (base_rotation * profile.forward).normalize_or(Vec3::NEG_Z);
    let mut out = Vec::new();
    for (i, bone) in profile.bones.iter().enumerate() {
        let Some(j) = names.iter().position(|n| *n == bone.joint) else {
            continue;
        };
        let at = globals[j].w_axis.truncate();
        let child = (0..names.len()).find(|&c| parents[c] == Some(j as u32));
        let direction = match child {
            Some(c) => globals[c].w_axis.truncate() - at,
            None => parents[j].map_or(Vec3::Y, |p| at - globals[p as usize].w_axis.truncate()),
        }
        .normalize_or(Vec3::Y);
        let length = bone.length.unwrap_or_else(|| match child {
            Some(c) => (globals[c].w_axis.truncate() - at).length(),
            None => bone.shape.leaf_length(bone.radius),
        });
        let end = at + direction * length;
        let color = if panel.selected == Some(i) {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(110, 200, 240)
        };
        out.push((at, end, color));
        match bone.shape {
            RagdollShape::Capsule => {
                ring(&mut out, at, direction, bone.radius, color);
                ring(&mut out, end, direction, bone.radius, color);
            }
            RagdollShape::Sphere => {
                let centre = at + direction * (length * 0.5);
                let side = direction.any_orthonormal_vector();
                for axis in [direction, side, direction.cross(side)] {
                    ring(&mut out, centre, axis, bone.radius, color);
                }
            }
            RagdollShape::Box { width, depth } => {
                // Depth runs along the character's forward, made perpendicular to the bone (as the runtime).
                let depth_axis = (forward - direction * forward.dot(direction))
                    .try_normalize()
                    .unwrap_or_else(|| direction.any_orthonormal_vector());
                let axes = [direction.cross(depth_axis), direction, depth_axis];
                let half = Vec3::new(width, length, depth) * 0.5;
                box_edges(&mut out, at + direction * half.y, axes, half, color);
            }
            RagdollShape::Sole { width, height } => {
                // A level foot (ADR-2963): along the level direction to the child, a quarter behind the joint.
                let child_offset = child.map_or(direction * length, |c| globals[c].w_axis.truncate() - at);
                let level = |v: Vec3| Vec3::new(v.x, 0.0, v.z).try_normalize();
                let ahead = level(child_offset)
                    .or_else(|| level(forward))
                    .unwrap_or(Vec3::NEG_Z);
                let half = Vec3::new(width, length, height) * 0.5;
                let low = child_offset.y.min(0.0);
                box_edges(
                    &mut out,
                    at + ahead * (half.y * 0.5) + Vec3::Y * (low + half.z),
                    [ahead.cross(Vec3::Y), ahead, Vec3::Y],
                    half,
                    color,
                );
            }
        }
        if let Some(l) = &bone.limits {
            let toward = match l.flex {
                RagdollFlex::Forward => forward,
                RagdollFlex::Backward => -forward,
            };
            let bend_axis = direction
                .cross(toward)
                .try_normalize()
                .unwrap_or_else(|| direction.any_orthonormal_vector());
            let reach = length.max(4.0 * bone.shape.extent(bone.radius)) * 0.6;
            for angle in [l.bend.0, l.bend.1] {
                let dir = Quat::from_axis_angle(bend_axis, -angle) * direction;
                out.push((at, at + dir * reach, egui::Color32::from_rgb(240, 170, 60)));
            }
        }
    }
    // The drop test's final pose, standing where the rig stands (its floor at the rig's origin).
    if let Some((generation, preview)) = &panel.preview
        && *generation == panel.generation
    {
        let origin = rig_matrix(world, rig).map_or(Vec3::ZERO, |m| m.w_axis.truncate());
        let color = egui::Color32::from_rgb(120, 230, 140);
        for body in &preview.bodies {
            let at = origin + body.position;
            if let Some(parent) = body.parent.and_then(|p| preview.bodies.get(p)) {
                out.push((origin + parent.position, at, color));
            }
            ring(&mut out, at, Vec3::Y, 0.05, color);
        }
    }
    out
}

// ------------------------------------------------------------------------------------------- drawing

/// Draws the panel.
pub fn ragdoll_panel(ui: &mut egui::Ui, world: &mut World) {
    let selection = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .filter(|e| world.get::<SkinnedMesh>(*e).is_some());
    let frame = ui.ctx().cumulative_frame_nr();
    with_shared(world, Some(frame), |panel, world, project| {
        let issues = panel.issues(world);
        let mut edits: Vec<Value> = Vec::new();
        let mut action = Action::None;
        draw(ui, panel, &issues, selection, &mut edits, &mut action);
        let result = match action {
            Action::None => Ok(()),
            Action::Open(path) => panel.open(project, &path),
            Action::New(path) => panel.new_profile(&path),
            Action::Save => panel.save(project).map(|_| ()),
            Action::Preview => panel.run_preview(world, &json!({})).map(|_| ()),
        };
        if let Err(e) = result {
            panel.error = Some(e);
        }
        for edit in edits {
            if let Err(e) = panel.edit(world, &edit) {
                panel.error = Some(e);
                break;
            }
            panel.error = None;
        }
    });
}

enum Action {
    None,
    Open(String),
    New(String),
    Save,
    Preview,
}

fn draw(
    ui: &mut egui::Ui,
    panel: &mut RagdollPanel,
    issues: &[RagdollIssue],
    selection: Option<Entity>,
    edits: &mut Vec<Value>,
    action: &mut Action,
) {
    crate::data_panels::header(ui, "Ragdoll", "physics asset: bodies · joints · limits");
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut panel.path_input).hint_text("assets/chars/hero.ragdoll.ron"));
        if ui.button("Open").clicked() {
            *action = Action::Open(panel.path_input.trim().to_owned());
        }
        if ui.button("New").clicked() {
            *action = Action::New(panel.path_input.trim().to_owned());
        }
        if ui.add_enabled(panel.dirty, egui::Button::new("Save")).clicked() {
            *action = Action::Save;
        }
        if ui
            .add_enabled(panel.undo_depth() > 0, egui::Button::new("Undo"))
            .clicked()
        {
            edits.push(json!({ "op": "undo" }));
        }
        if ui
            .add_enabled(panel.redo_depth() > 0, egui::Button::new("Redo"))
            .clicked()
        {
            edits.push(json!({ "op": "redo" }));
        }
    });
    ui.horizontal(|ui| {
        match panel.rig {
            Some(rig) => ui.label(format!("rig: entity {}", rig.to_bits())),
            None => ui.weak("no rig"),
        };
        if ui
            .add_enabled(selection.is_some(), egui::Button::new("Use selected rig"))
            .on_disabled_hover_text("Select an entity with a SkinnedMesh")
            .clicked()
        {
            panel.rig = selection;
        }
        ui.checkbox(&mut panel.show_gizmo, "Gizmo");
        if ui
            .add_enabled(panel.rig.is_some(), egui::Button::new("Humanoid from rig"))
            .clicked()
        {
            edits.push(json!({ "op": "humanoid" }));
        }
        if ui
            .add_enabled(
                panel.rig.is_some() && panel.profile.is_some(),
                egui::Button::new("Drop test"),
            )
            .on_hover_text("Simulate the ragdoll falling limp onto a floor (the level is not touched)")
            .clicked()
        {
            *action = Action::Preview;
        }
    });
    if let Some((generation, p)) = &panel.preview {
        let settled = p.settled_at.map_or_else(
            || "did not settle".to_owned(),
            |s| {
                format!(
                    "settled at step {s}{}",
                    if p.face_up == Some(true) { ", face up" } else { "" }
                )
            },
        );
        let text = format!(
            "drop test: {settled} · fastest {:.2} m/s · lowest centre {:.3} m{}{}",
            p.max_speed,
            p.lowest,
            if p.finite { "" } else { " · NOT FINITE" },
            if *generation == panel.generation {
                ""
            } else {
                " (stale)"
            },
        );
        ui.weak(text);
    }
    if let Some(e) = &panel.error {
        e.draw(ui);
    }
    let Some(profile) = panel.profile.clone() else {
        ui.weak("Open a .ragdoll.ron or start a new one.");
        return;
    };
    ui.label(format!(
        "{} bodies · {:.1} kg · {}{}",
        profile.bones.len(),
        profile.bones.iter().map(|b| b.mass).sum::<f32>(),
        panel.path.as_deref().unwrap_or("unsaved"),
        if panel.dirty { " *" } else { "" }
    ));
    egui::Grid::new("ragdoll-bones").striped(true).num_columns(6).show(ui, |ui| {
        for h in ["joint", "radius", "mass", "length", "limits", ""] {
            ui.weak(h);
        }
        ui.end_row();
        for (i, bone) in profile.bones.iter().enumerate() {
            let flagged = issues.iter().any(|x| x.joint.as_deref() == Some(bone.joint.as_str()));
            let text = egui::RichText::new(&bone.joint).color(if flagged {
                egui::Color32::from_rgb(240, 170, 60)
            } else {
                ui.visuals().text_color()
            });
            if ui.selectable_label(panel.selected == Some(i), text).clicked() {
                edits.push(json!({ "op": "select", "index": i }));
            }
            let (mut r, mut m) = (bone.radius, bone.mass);
            if ui.add(egui::DragValue::new(&mut r).speed(0.005).range(0.001..=2.0)).changed() {
                edits.push(json!({ "op": "set_bone", "index": i, "radius": r }));
            }
            if ui.add(egui::DragValue::new(&mut m).speed(0.1).range(0.01..=1000.0)).changed() {
                edits.push(json!({ "op": "set_bone", "index": i, "mass": m }));
            }
            ui.label(bone.length.map_or_else(|| "auto".to_owned(), |l| format!("{l:.3}")));
            let mut limited = bone.limits.is_some();
            if ui.checkbox(&mut limited, "").changed() {
                edits.push(json!({ "op": "set_bone", "index": i, "limits": if limited { json!({}) } else { Value::Null } }));
            }
            if ui.small_button("Remove").clicked() {
                edits.push(json!({ "op": "remove_bone", "index": i }));
            }
            ui.end_row();
        }
    });
    // The selected body's shape (capsule, sphere or box), its length measured from the skeleton or set.
    if let Some(i) = panel.selected
        && let Some(bone) = profile.bones.get(i)
    {
        ui.horizontal(|ui| {
            ui.strong(format!("{} shape:", bone.joint));
            for name in ["capsule", "sphere", "box"] {
                if ui.selectable_label(bone.shape.name() == name, name).clicked() && bone.shape.name() != name
                {
                    edits.push(json!({ "op": "set_bone", "index": i, "shape": name }));
                }
            }
            if let RagdollShape::Box { width, depth } = bone.shape {
                for (key, value) in [("width", width), ("depth", depth)] {
                    let mut v = value;
                    ui.label(key);
                    if ui
                        .add(
                            egui::DragValue::new(&mut v)
                                .speed(0.005)
                                .range(0.01..=5.0)
                                .suffix(" m"),
                        )
                        .changed()
                    {
                        edits.push(json!({ "op": "set_bone", "index": i, key: v }));
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            let mut auto = bone.length.is_none();
            if ui.checkbox(&mut auto, "length from skeleton").changed() {
                let length = if auto {
                    Value::Null
                } else {
                    json!(bone.shape.leaf_length(bone.radius))
                };
                edits.push(json!({ "op": "set_bone", "index": i, "length": length }));
            }
            if let Some(mut length) = bone.length
                && ui
                    .add(
                        egui::DragValue::new(&mut length)
                            .speed(0.005)
                            .range(0.001..=5.0)
                            .suffix(" m"),
                    )
                    .changed()
            {
                edits.push(json!({ "op": "set_bone", "index": i, "length": length }));
            }
        });
    }
    // The selected joint's limits, in degrees.
    if let Some(i) = panel.selected
        && let Some(bone) = profile.bones.get(i)
        && let Some(l) = bone.limits
    {
        ui.strong(format!("{} limits (degrees)", bone.joint));
        let mut flex_back = l.flex == RagdollFlex::Backward;
        if ui.checkbox(&mut flex_back, "flexes backward (knee)").changed() {
            edits.push(json!({ "op": "set_bone", "index": i, "limits": { "flex": if flex_back { "backward" } else { "forward" } } }));
        }
        for (name, (min, max)) in [("bend", l.bend), ("side", l.side), ("twist", l.twist)] {
            ui.horizontal(|ui| {
                ui.label(name);
                let (mut a, mut b) = (min.to_degrees(), max.to_degrees());
                let ca = ui.add(egui::DragValue::new(&mut a).speed(0.5).range(-180.0..=180.0)).changed();
                let cb = ui.add(egui::DragValue::new(&mut b).speed(0.5).range(-180.0..=180.0)).changed();
                if (ca || cb) && a <= b {
                    edits.push(json!({ "op": "set_bone", "index": i, "limits": { name: [a.to_radians(), b.to_radians()] } }));
                }
            });
        }
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut panel.add_joint)
                .hint_text("joint")
                .desired_width(100.0),
        );
        if ui.button("Add body").clicked() && !panel.add_joint.trim().is_empty() {
            edits.push(json!({ "op": "add_bone", "joint": panel.add_joint.trim() }));
        }
    });
    for issue in issues {
        ui.colored_label(
            egui::Color32::from_rgb(230, 170, 80),
            format!("{}: {}", issue.code, issue.message),
        );
    }
}

// --------------------------------------------------------------------------------------- agent surface

fn with_panel<T>(
    world: &mut World,
    f: impl FnOnce(&mut RagdollPanel, &World, Option<&Path>) -> Result<T, PanelError>,
) -> Result<T, RpcError> {
    let result = with_shared(world, None, |panel, world, project| {
        let result = f(panel, world, project);
        match &result {
            Ok(_) => panel.error = None,
            Err(e) => panel.error = Some(e.clone()),
        }
        result
    });
    result.map_err(|e| e.rpc(HINT))
}

fn state(world: &World) -> Value {
    match world.get_resource::<RagdollPanel>() {
        Some(panel) => panel.json(world),
        None => RagdollPanel::default().json(world),
    }
}

fn panel_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let create = params.get("new").and_then(Value::as_bool).unwrap_or(false);
    with_panel(world, |panel, _, project| {
        if create {
            panel.new_profile(&path)
        } else {
            panel.open(project, &path)
        }
    })?;
    Ok(state(world))
}

fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state(world))
}

fn panel_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world, _| panel.edit(world, params))?;
    Ok(state(world))
}

fn panel_set_rig(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world, _| {
        match params.get("entity") {
            None => {}
            Some(Value::Null) => panel.rig = None,
            Some(v) => {
                let e = v
                    .as_u64()
                    .and_then(Entity::try_from_bits)
                    .ok_or_else(|| err("bad_value", "`entity` must be an entity id or null"))?;
                if world.get::<SkinnedMesh>(e).is_none() {
                    return Err(err(
                        "no_rig",
                        format!("entity {} has no SkinnedMesh", e.to_bits()),
                    ));
                }
                panel.rig = Some(e);
            }
        }
        match params.get("show_gizmo") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => panel.show_gizmo = *b,
            Some(_) => return Err(err("bad_value", "`show_gizmo` must be a boolean")),
        }
        Ok(())
    })?;
    Ok(state(world))
}

fn panel_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world, _| {
        panel.run_preview(world, params).map(|_| ())
    })?;
    Ok(state(world))
}

fn panel_save(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let written = with_panel(world, |panel, _, project| panel.save(project))?;
    Ok(json!({ "written": written }))
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

const STATE: &str = "{ path, rig, show_gizmo, dirty, generation, undo_depth, redo_depth, preview: { stale, steps, settled, \
    settled_at, face_up, max_speed, lowest, finite, digest, bodies: [{ joint, parent, position }] } or null, selected, \
    profile: { forward, total_mass, bones: [{ joint, radius, mass, length, limits: { flex, bend, side, twist }, shape: { kind, width, depth } }] }, \
    issues: [{ code, severity, joint, message }], gizmo_segments, error }";

const OPEN_PARAMS: &[ParamSchema] = &[
    param("path", "string", true, "Project-relative .ragdoll.ron."),
    param(
        "new",
        "boolean",
        false,
        "Start an empty profile that save writes to path.",
    ),
];
const EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "op",
        "string",
        true,
        "set_bone | add_bone | remove_bone | set_forward | humanoid | select | undo | redo",
    ),
    param("joint", "string", false, "The body's joint (or give index)."),
    param("index", "number", false, "The body's index."),
    param("radius", "number", false, "Capsule or sphere radius, metres."),
    param(
        "mass",
        "number",
        false,
        "Body mass, kg (humanoid: the whole body, default 75).",
    ),
    param(
        "length",
        "number",
        false,
        "Body length along the bone, metres; null measures it from the skeleton.",
    ),
    param(
        "shape",
        "string",
        false,
        "set_bone/add_bone: capsule | sphere | box (a new box starts at the bone's diameter).",
    ),
    param(
        "width",
        "number",
        false,
        "Box side-to-side extent, metres (box bodies only).",
    ),
    param(
        "depth",
        "number",
        false,
        "Box front-to-back extent, metres (box bodies only).",
    ),
    param(
        "limits",
        "object",
        false,
        "{ flex: forward|backward, bend: [min, max], side, twist } in radians; null = unlimited.",
    ),
    param(
        "forward",
        "array",
        false,
        "set_forward: the rig-space direction the character faces.",
    ),
    param(
        "limited",
        "boolean",
        false,
        "humanoid: add the humanoid joint limits (default true).",
    ),
];
const PREVIEW_PARAMS: &[ParamSchema] = &[
    param(
        "steps",
        "number",
        false,
        "Fixed steps (62.5 per second) after the ragdoll goes limp, 1..=4000 (default 250).",
    ),
    param(
        "drop_height",
        "number",
        false,
        "Metres from the floor to the lowest joint's largest shape extent, 0..=20 (default 0.3). The joint centre sits that far plus the extent.",
    ),
    param(
        "push",
        "array",
        false,
        "[x, y, z] N·s on the root body at release (default none).",
    ),
];
const RIG_PARAMS: &[ParamSchema] = &[
    param(
        "entity",
        "number",
        false,
        "An entity with a SkinnedMesh; null clears.",
    ),
    param("show_gizmo", "boolean", false, "Draw the bodies in the viewport."),
];

/// The Ragdoll editor's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.ragdoll_panel_open",
                tool: "ragdoll_panel_open",
                description: "Open a .ragdoll.ron (the character's physics asset) in the Ragdoll editor, or start a \
                    new one.",
                params: OPEN_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ragdoll_panel_get",
                tool: "ragdoll_panel_get",
                description: "The Ragdoll editor: bodies, joint limits, findings against the rig, the gizmo.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ragdoll_panel_edit",
                tool: "ragdoll_panel_edit",
                description: "Edit the open profile's bodies and joint limits (in memory, undoable; \
                    editor.ragdoll_panel_save writes it).",
                params: EDIT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ragdoll_panel_set_rig",
                tool: "ragdoll_panel_set_rig",
                description: "Pick the rig the profile is checked against, generated from and drawn on.",
                params: RIG_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_set_rig as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ragdoll_panel_preview",
                tool: "ragdoll_panel_preview",
                description: "Drop test: simulate the open profile on the rig's skeleton falling limp onto a floor, in a \
                    private physics world (deterministic; the level is not touched). The result is the panel's \
                    preview and is drawn beside the rig.",
                params: PREVIEW_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_preview as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ragdoll_panel_save",
                tool: "ragdoll_panel_save",
                description: "Write the open profile to its .ragdoll.ron, atomically.",
                params: &[],
                result: "{ written }",
                mutating: true,
                permission: Permission::Assets,
            },
            panel_save as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
