// SPDX-License-Identifier: MPL-2.0
//! The Material asset editor: `.material.ron` parameters with a live PBR preview
//! (`docs/specs/19-editor.md` § 2 "Asset editors").
//!
//! Edits stay in panel state until **Save** (an atomic write); a standalone panel keeps its own
//! small undo/redo of whole material snapshots. In the editor (a world with the `UndoStack`) each
//! edit is an entry of the shared editor history instead ([`sync_shared_history`]): Edit ▸ Undo,
//! Ctrl+Z and the Undo History panel list it by file, the panel's Undo and Redo step that history,
//! and undoing an edit of a material that is no longer open opens it again. The preview renders a sphere through the engine's
//! persistent frame renderer (`regia_render::FrameRenderer`) with the material's base PBR
//! parameters — the same pass the viewport uses — so colour/metallic/roughness changes are
//! visible immediately. Parameters the pass does not consume yet (emissive, layers, clearcoat)
//! stay editable and serialize correctly; the preview states what it shows.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regia_asset::{AlphaMode, MaterialAssetRon, MaterialShaderRef};
use regia_core::math::Vec3;
use regia_ecs::prelude::{Resource, World, bevy_ecs};
use regia_render::{
    ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, DeviceFrameRenderer, FrameMesh, FrameScene, LabMaterial,
    LabVertex, RenderDevice,
};

use crate::inspector::{linear_to_srgb_u8, srgb8_to_linear};

/// The panel's open material and its edit history.
#[derive(Debug)]
pub struct MaterialEditorState {
    /// The file being edited (`None` = no material open).
    pub path: Option<PathBuf>,
    /// The in-memory material (edits land here).
    pub material: MaterialAssetRon,
    /// Whole-material snapshots for the panel-local undo.
    undo: Vec<MaterialAssetRon>,
    /// Redo entries.
    redo: Vec<MaterialAssetRon>,
    /// Whether unsaved edits exist: the material differs from the last opened or saved bytes.
    pub dirty: bool,
    /// Status line for the last action.
    pub toast: String,
    /// The material as last opened or saved (`None` before a file is open): `dirty` compares
    /// against it, so an undo back to the file's state is clean again.
    saved: Option<MaterialAssetRon>,
    /// What the preview draws the material on (ADR-9700).
    pub preview_shape: PreviewShape,
    /// The preview's orbit and light turn.
    pub preview_view: PreviewView,
    /// The shared preview scene: lighting preset, environment intensity, floor, turntable
    /// (`crate::preview_scene`, ADR-7771).
    pub scene: crate::preview_scene::PreviewContext,
    /// The project mesh path field.
    mesh_field: String,
    /// Why the preview could not draw the chosen shape (the last image stays).
    pub preview_error: Option<String>,
    /// The last preview image.
    preview_image: LastPreview,
    /// What the last preview render (or failure) was made for: the panel renders again only
    /// when this changes (ADR-9970).
    preview_key: Option<MaterialPreviewKey>,
    /// Preview renders so far (a counter for tests and the agent).
    preview_renders: u64,
    /// Hosted in an editor world with an `UndoStack`: edits, undo and redo use the shared editor
    /// history instead of `undo` / `redo`.
    shared: bool,
    /// The shared history's (undo, redo) depths at the last [`sync_shared_history`].
    shared_depths: (usize, usize),
    /// Edits since the last [`sync_shared_history`], which moves them to the shared history.
    pending: Vec<MaterialEdit>,
    /// A shared-history undo (`false`) or redo (`true`) the next [`sync_shared_history`] runs.
    history_request: Option<bool>,
}

/// One Material panel edit on the shared editor history: the material before and after.
#[derive(Debug)]
struct MaterialEdit {
    path: PathBuf,
    /// The file as the history names it (project-relative when the project is known).
    document: String,
    before: MaterialAssetRon,
    after: MaterialAssetRon,
}

/// What the shared history knows of each material the panel opened: its last in-memory state and
/// its saved baseline, so an undo can reopen a material that is no longer open.
#[derive(Resource, Default)]
struct MaterialHistory {
    docs: std::collections::BTreeMap<PathBuf, (MaterialAssetRon, Option<MaterialAssetRon>)>,
    /// The materials history steps changed since the last sync, oldest first: the panel shows the
    /// last one at the next sync.
    changed: Vec<PathBuf>,
}

impl MaterialEdit {
    fn restore(&self, world: &mut World, material: &MaterialAssetRon) {
        let mut history = world.get_resource_or_insert_with::<MaterialHistory>(Default::default);
        let doc = history
            .docs
            .entry(self.path.clone())
            .or_insert_with(|| (material.clone(), None));
        doc.0 = material.clone();
        history.changed.push(self.path.clone());
    }
}

impl crate::command::EditorCommand for MaterialEdit {
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
        vec![crate::command::CommandTouch::asset(self.document.clone(), None)]
    }
    fn description(&self) -> String {
        format!("Edit material {}", self.document)
    }
}

/// Connects the panel to the shared editor history (call once per editor frame, before and after
/// the panel draws): moves the panel's new edits to the world's `UndoStack`, runs the Undo or Redo
/// the panel asked for, and shows the material a history step changed (reopening it when another
/// one is open). Without an `UndoStack` the panel keeps its own undo and this does nothing.
pub fn sync_shared_history(world: &mut World, state: &mut MaterialEditorState) {
    let _span = tracing::trace_span!(target: "regia_editor", "material_shared_history").entered();
    state.shared = world.contains_resource::<crate::command::UndoStack>();
    if !state.shared {
        return;
    }
    let root = crate::mesh_source::project_root(world);
    {
        let mut stack = world.resource_mut::<crate::command::UndoStack>();
        for mut edit in state.pending.drain(..) {
            edit.document = root
                .as_deref()
                .and_then(|root| edit.path.strip_prefix(root).ok())
                .unwrap_or(&edit.path)
                .to_string_lossy()
                .replace('\\', "/");
            stack.seal();
            stack.push(Box::new(edit));
            stack.seal();
        }
    }
    // Steps run elsewhere since the last sync (Edit ▸ Undo, Ctrl+Z, editor.undo) first, so the
    // panel's stale buffer never overwrites what they restored.
    show_changed(world, state);
    remember_material(world, state);
    match state.history_request.take() {
        Some(true) => {
            crate::command::redo_world(world);
        }
        Some(false) => {
            crate::command::undo_world(world);
        }
        None => {}
    }
    show_changed(world, state);
    let stack = world.resource::<crate::command::UndoStack>();
    state.shared_depths = (stack.undo_depth(), stack.redo_depth());
}

/// Shows the last material history steps changed, reopening it when another one is open. The open
/// material's buffer is kept for a later undo unless a step changed it too.
fn show_changed(world: &mut World, state: &mut MaterialEditorState) {
    let changed = world
        .get_resource_mut::<MaterialHistory>()
        .map(|mut history| std::mem::take(&mut history.changed))
        .unwrap_or_default();
    let Some(target) = changed.last().cloned() else {
        return;
    };
    let open_changed = state.path.as_ref().is_some_and(|open| changed.contains(open));
    if !open_changed {
        remember_material(world, state);
    }
    let Some((material, saved)) = world
        .get_resource::<MaterialHistory>()
        .and_then(|history| history.docs.get(&target).cloned())
    else {
        return;
    };
    if state.path.as_deref() != Some(target.as_path()) {
        state.toast = format!("opened {} for undo", target.display());
        state.path = Some(target);
        state.saved = saved;
    }
    state.material = material;
    state.refresh_dirty();
    remember_material(world, state);
}

fn remember_material(world: &mut World, state: &MaterialEditorState) {
    if let Some(path) = &state.path {
        world
            .get_resource_or_insert_with::<MaterialHistory>(Default::default)
            .docs
            .insert(path.clone(), (state.material.clone(), state.saved.clone()));
    }
}

/// What a Material panel preview is rendered from: the material (its value, so any change to
/// it counts as a new revision), the shape (a mesh by path), the orbit and light turn, and the
/// image size. Floats compare by bits.
#[derive(Clone, Debug, PartialEq)]
struct MaterialPreviewKey {
    material: MaterialAssetRon,
    shape: PreviewShape,
    view: [u32; 3],
    size: u32,
    scene: crate::preview_scene::PreviewContextKey,
}

/// The Material panel's last preview image (kept when a shape fails to load).
#[derive(Default)]
struct LastPreview(Option<egui::TextureHandle>);

impl std::fmt::Debug for LastPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "LastPreview(image)"
        } else {
            "LastPreview(none)"
        })
    }
}

/// What the Material panel has open, mirrored into the world by the shell each frame so
/// `editor.edit_material` can see a panel buffer it would otherwise overwrite (audit v033b).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct MaterialPanelStatus {
    /// The file the panel edits.
    pub path: Option<PathBuf>,
    /// Whether the panel holds unsaved edits of it.
    pub dirty: bool,
}

/// `editor.edit_material` saved the file the (clean) Material panel has open: the shell adopts
/// the saved material as one panel undo step instead of keeping a stale buffer whose next save
/// would drop the agent's edit.
#[derive(Resource, Debug, Clone)]
pub struct MaterialPanelReload {
    /// The material file the agent wrote.
    pub path: PathBuf,
    /// The material as written.
    pub material: MaterialAssetRon,
}

/// Whether two paths name the same file (equal, or equal once canonicalized).
#[must_use]
pub fn same_material_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

impl Default for MaterialEditorState {
    fn default() -> Self {
        Self {
            path: None,
            material: MaterialAssetRon::default_standard(),
            undo: Vec::new(),
            redo: Vec::new(),
            dirty: false,
            toast: String::new(),
            saved: None,
            preview_shape: PreviewShape::Sphere,
            preview_view: PreviewView::default(),
            scene: crate::preview_scene::PreviewContext::default(),
            mesh_field: String::new(),
            preview_error: None,
            preview_image: LastPreview::default(),
            preview_key: None,
            preview_renders: 0,
            shared: false,
            shared_depths: (0, 0),
            pending: Vec::new(),
            history_request: None,
        }
    }
}

impl MaterialEditorState {
    /// Opens `path` from disk (replacing any open material).
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed.
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let material = MaterialAssetRon::from_ron(&bytes)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        self.path = Some(path.to_path_buf());
        self.saved = Some(material.clone());
        self.material = material;
        self.undo.clear();
        self.redo.clear();
        self.dirty = false;
        self.toast = format!("opened {}", path.display());
        Ok(())
    }

    /// Whether a material is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.path.is_some()
    }

    /// How many times the panel has rendered its preview: once per change of the material,
    /// the shape, the view or the size, not once per repaint (ADR-9970).
    #[must_use]
    pub fn preview_render_count(&self) -> u64 {
        self.preview_renders
    }

    /// Whether the preview must render for this key; records the key when it must.
    fn preview_due(&mut self, size: f32) -> bool {
        let key = MaterialPreviewKey {
            material: self.material.clone(),
            shape: self.preview_shape.clone(),
            view: [
                self.preview_view.yaw.to_bits(),
                self.preview_view.pitch.to_bits(),
                self.preview_view.light_yaw.to_bits(),
            ],
            size: size.max(64.0) as u32,
            scene: self.scene.key(),
        };
        if self.preview_key.as_ref() == Some(&key) {
            return false;
        }
        self.preview_key = Some(key);
        true
    }

    /// Applies `edit` to the material, snapshotting for undo.
    pub fn edit(&mut self, edit: impl FnOnce(&mut MaterialAssetRon)) {
        let mut next = self.material.clone();
        edit(&mut next);
        let previous = std::mem::replace(&mut self.material, next);
        self.record(previous);
        self.refresh_dirty();
    }

    /// Records `previous` (the material before an edit) on the panel's or the shared history.
    fn record(&mut self, previous: MaterialAssetRon) {
        match (&self.path, self.shared) {
            (Some(path), true) => self.pending.push(MaterialEdit {
                path: path.clone(),
                document: path.display().to_string(),
                before: previous,
                after: self.material.clone(),
            }),
            _ => self.undo.push(previous),
        }
        self.redo.clear();
    }

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

    /// `dirty` from the material against the last opened or saved state.
    fn refresh_dirty(&mut self) {
        self.dirty = self.saved.as_ref() != Some(&self.material);
    }

    /// Adopts `material`, which another writer (`editor.edit_material`) just saved to this
    /// panel's file, as one undo step; the panel stays clean because it matches the file.
    pub fn adopt_saved(&mut self, material: MaterialAssetRon) {
        if material == self.material {
            self.saved = Some(material);
            self.refresh_dirty();
            return;
        }
        let previous = std::mem::replace(&mut self.material, material.clone());
        self.record(previous);
        self.saved = Some(material);
        self.refresh_dirty();
    }

    /// One undo: on the shared history when hosted (it runs at the next [`sync_shared_history`]),
    /// else the panel's own. Returns whether there is a step to undo.
    pub fn undo(&mut self) -> bool {
        if self.shared {
            let any = self.undo_depth() > 0;
            if any {
                self.history_request = Some(false);
            }
            return any;
        }
        match self.undo.pop() {
            Some(previous) => {
                self.redo.push(std::mem::replace(&mut self.material, previous));
                self.refresh_dirty();
                true
            }
            None => false,
        }
    }

    /// One redo: on the shared history when hosted (it runs at the next [`sync_shared_history`]),
    /// else the panel's own. Returns whether there is a step to redo.
    pub fn redo(&mut self) -> bool {
        if self.shared {
            let any = self.redo_depth() > 0;
            if any {
                self.history_request = Some(true);
            }
            return any;
        }
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut self.material, next));
                self.refresh_dirty();
                true
            }
            None => false,
        }
    }

    /// Saves the material back to its file (atomic write).
    ///
    /// # Errors
    ///
    /// Serialization or IO failure.
    pub fn save(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err("no material is open".to_string());
        };
        let text = self
            .material
            .to_ron_string()
            .map_err(|e| format!("cannot serialize {}: {e}", path.display()))?;
        let tmp = path.with_extension("ron.new");
        std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))?;
        self.saved = Some(self.material.clone());
        self.dirty = false;
        self.toast = format!("saved {}", path.display());
        Ok(())
    }
}

/// The [`LabMaterial`] the frame renderer consumes: base PBR parameters plus
/// emissive and alpha mode, so those edits show in the preview immediately.
#[must_use]
pub fn preview_material(material: &MaterialAssetRon) -> LabMaterial {
    LabMaterial {
        base_color: material.base.base_color,
        metallic: material.base.metallic,
        roughness: material.base.roughness,
        emissive: material.base.emissive,
        alpha_mode: match material.render_state.alpha_mode {
            AlphaMode::Opaque => ALPHA_OPAQUE,
            AlphaMode::Mask => ALPHA_MASK,
            AlphaMode::Blend => ALPHA_BLEND,
        },
        ..LabMaterial::default()
    }
}

/// Renders the preview sphere with the material (the engine's persistent frame
/// renderer, the same one the viewport uses, on the editor profile so the
/// preview does not jitter). `None` without a GPU device or when the render fails.
#[must_use]
#[tracing::instrument(target = "regia_editor", skip_all, name = "material_preview")]
pub fn render_material_preview(
    device: &RenderDevice,
    material: &MaterialAssetRon,
    size: [f32; 2],
) -> Option<egui::ColorImage> {
    /// The Material panel's material identity (uploaded once, updated in place).
    const SPHERE_MATERIAL_KEY: u64 = 1;
    render_sphere_preview(device, &preview_material(material), size, SPHERE_MATERIAL_KEY)
}

/// Renders the preview sphere with any [`LabMaterial`] (a standard material, or a material
/// graph's, `regia_matgraph::lab_material`) on the editor's persistent preview renderer.
/// `material_key` 0 draws the material once without caching it (the material graph editor
/// passes 0, since each edit may change its code). `None` when the render fails.
#[must_use]
#[tracing::instrument(target = "regia_editor", skip_all, name = "sphere_preview")]
pub fn render_sphere_preview(
    device: &RenderDevice,
    lab_material: &LabMaterial,
    size: [f32; 2],
    material_key: u64,
) -> Option<egui::ColorImage> {
    let sphere = preview_geometry(&PreviewShape::Sphere, None).ok()?;
    render_preview(
        device,
        lab_material,
        &sphere,
        &PreviewView::default(),
        size,
        material_key,
    )
}

/// What the material previews draw the material on (ADR-9700).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum PreviewShape {
    /// The unit UV sphere.
    #[default]
    Sphere,
    /// A cube with flat faces.
    Cube,
    /// A flat square, facing up.
    Plane,
    /// A project mesh (`.glb` / `.gltf`, project-relative or absolute): its first part,
    /// centered and scaled to the preview's frame.
    Mesh(String),
}

impl PreviewShape {
    /// The protocol names, in menu order.
    pub const NAMES: [&'static str; 4] = ["sphere", "cube", "plane", "mesh"];

    /// The protocol name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sphere => "sphere",
            Self::Cube => "cube",
            Self::Plane => "plane",
            Self::Mesh(_) => "mesh",
        }
    }

    /// Parses a protocol name; `mesh` needs the mesh path.
    ///
    /// # Errors
    ///
    /// Unknown name, or `mesh` without a path.
    pub fn parse(name: &str, mesh: Option<&str>) -> Result<Self, String> {
        match (name, mesh) {
            ("sphere", _) => Ok(Self::Sphere),
            ("cube", _) => Ok(Self::Cube),
            ("plane", _) => Ok(Self::Plane),
            ("mesh", Some(path)) if !path.trim().is_empty() => Ok(Self::Mesh(path.trim().to_string())),
            ("mesh", _) => Err("shape `mesh` needs `mesh`: a project .glb or .gltf path".into()),
            (other, _) => Err(format!(
                "unknown preview shape `{other}` (sphere, cube, plane, mesh)"
            )),
        }
    }
}

/// The preview's fixed eye (the default view).
const PREVIEW_EYE: Vec3 = Vec3::new(2.4, 1.6, 2.6);
/// The preview light's default direction.
const PREVIEW_LIGHT: [f32; 3] = [0.35, -0.85, 0.4];
/// Pitch stays this far short of the poles (radians).
const PREVIEW_PITCH_LIMIT: f32 = 1.45;

/// The preview camera's orbit and the light's turn (ADR-9700). The default is the preview's
/// fixed camera and light.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PreviewView {
    /// Orbit around the vertical axis, radians.
    pub yaw: f32,
    /// Orbit up (+) or down (-), radians; the eye stays short of the poles.
    pub pitch: f32,
    /// The light's turn around the vertical axis, radians.
    pub light_yaw: f32,
}

impl PreviewView {
    /// The eye position (looking at the origin, at the default distance).
    #[must_use]
    pub fn eye(&self) -> Vec3 {
        if self.yaw == 0.0 && self.pitch == 0.0 {
            return PREVIEW_EYE;
        }
        let distance = PREVIEW_EYE.length();
        let yaw = PREVIEW_EYE.x.atan2(PREVIEW_EYE.z) + self.yaw;
        let pitch =
            ((PREVIEW_EYE.y / distance).asin() + self.pitch).clamp(-PREVIEW_PITCH_LIMIT, PREVIEW_PITCH_LIMIT);
        Vec3::new(
            distance * pitch.cos() * yaw.sin(),
            distance * pitch.sin(),
            distance * pitch.cos() * yaw.cos(),
        )
    }

    /// The light's travel direction (the default turned by `light_yaw` around the vertical).
    #[must_use]
    pub fn light_direction(&self) -> [f32; 3] {
        if self.light_yaw == 0.0 {
            return PREVIEW_LIGHT;
        }
        let (sin, cos) = self.light_yaw.sin_cos();
        let [x, y, z] = PREVIEW_LIGHT;
        [x * cos + z * sin, y, -x * sin + z * cos]
    }
}

/// Geometry a preview draws, with the renderer's stable mesh identity.
#[derive(Clone, Debug)]
pub struct PreviewGeometry {
    /// The renderer's mesh key (uploaded once per key).
    pub key: u64,
    /// Vertices (centered at the origin).
    pub vertices: Vec<LabVertex>,
    /// Triangle-list indices.
    pub indices: Vec<u32>,
}

/// A loaded mesh's identity (path, size, modification time) and its geometry.
type MeshCache = Vec<(
    (PathBuf, u64, Option<std::time::SystemTime>),
    std::sync::Arc<PreviewGeometry>,
)>;

/// A project mesh with more vertices than this is refused by the preview.
pub const PREVIEW_MESH_VERTEX_CAP: usize = 250_000;

/// A project mesh is scaled so its farthest vertex sits this far from its center (the frame
/// the unit sphere fills).
pub const PREVIEW_MESH_RADIUS: f32 = 1.1;

/// The geometry of `shape`; a mesh path resolves against the project `root` (absolute paths
/// as given). Built-in shapes and loaded meshes are cached (a mesh by path, size and
/// modification time).
///
/// # Errors
///
/// A mesh that is not `.glb` / `.gltf`, cannot load, has no triangles or exceeds
/// [`PREVIEW_MESH_VERTEX_CAP`]; the message says which.
pub fn preview_geometry(
    shape: &PreviewShape,
    root: Option<&Path>,
) -> Result<std::sync::Arc<PreviewGeometry>, String> {
    use std::sync::Arc;
    static BUILTIN: OnceLock<[Arc<PreviewGeometry>; 3]> = OnceLock::new();
    static MESHES: OnceLock<Mutex<MeshCache>> = OnceLock::new();
    let builtin = BUILTIN.get_or_init(|| {
        let (sv, si) = preview_sphere(1.0, 20, 14);
        let (cv, ci) = preview_cube();
        let (pv, pi) = preview_plane();
        [
            Arc::new(PreviewGeometry {
                key: 0x00ed_17a1,
                vertices: sv,
                indices: si,
            }),
            Arc::new(PreviewGeometry {
                key: 0x00ed_17a2,
                vertices: cv,
                indices: ci,
            }),
            Arc::new(PreviewGeometry {
                key: 0x00ed_17a3,
                vertices: pv,
                indices: pi,
            }),
        ]
    });
    let rel = match shape {
        PreviewShape::Sphere => return Ok(Arc::clone(&builtin[0])),
        PreviewShape::Cube => return Ok(Arc::clone(&builtin[1])),
        PreviewShape::Plane => return Ok(Arc::clone(&builtin[2])),
        PreviewShape::Mesh(rel) => rel,
    };
    let lower = rel.to_ascii_lowercase();
    if !(lower.ends_with(".glb") || lower.ends_with(".gltf")) {
        return Err(format!(
            "`{rel}`: the preview loads glTF project meshes (.glb or .gltf) only"
        ));
    }
    let path = crate::mesh_source::resolve_asset_path(root, rel);
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let id = (path.clone(), meta.len(), meta.modified().ok());
    let cache = MESHES.get_or_init(|| Mutex::new(Vec::new()));
    if let Some((_, hit)) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(k, _)| *k == id)
    {
        return Ok(Arc::clone(hit));
    }
    let geometry = Arc::new(load_preview_mesh(&path, PREVIEW_MESH_VERTEX_CAP)?);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    // A small cache: the last few meshes the previews used.
    if cache.len() >= 8 {
        cache.remove(0);
    }
    cache.push((id, Arc::clone(&geometry)));
    Ok(geometry)
}

/// Loads a glTF mesh for the preview: its first part (the first primitive's material group,
/// node transforms baked), centered on its bounds and scaled so the farthest vertex is
/// [`PREVIEW_MESH_RADIUS`] away. Missing normals are rebuilt from the faces (area-weighted).
///
/// # Errors
///
/// The file cannot load, has no triangles, or has more than `vertex_cap` vertices.
pub fn load_preview_mesh(path: &Path, vertex_cap: usize) -> Result<PreviewGeometry, String> {
    let default = std::sync::Arc::new(LabMaterial::default());
    let model = crate::mesh_source::load_gltf_model(path, &default)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let part = model
        .parts
        .first()
        .ok_or_else(|| format!("{}: no triangles", path.display()))?;
    let mut vertices = part.mesh.vertices.clone();
    let indices = part.mesh.indices.clone();
    if vertices.len() > vertex_cap {
        return Err(format!(
            "{}: {} vertices; the preview's vertex cap is {vertex_cap}",
            path.display(),
            vertices.len()
        ));
    }
    if indices.len() < 3 {
        return Err(format!("{}: no triangles", path.display()));
    }
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for v in &vertices {
        let p = Vec3::from_array(v.position);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let center = (lo + hi) * 0.5;
    let radius = vertices
        .iter()
        .map(|v| (Vec3::from_array(v.position) - center).length())
        .fold(0.0f32, f32::max);
    if !radius.is_finite() || radius <= 0.0 {
        return Err(format!("{}: the mesh has no extent", path.display()));
    }
    let scale = PREVIEW_MESH_RADIUS / radius;
    for v in &mut vertices {
        v.position = ((Vec3::from_array(v.position) - center) * scale).to_array();
    }
    rebuild_missing_normals(&mut vertices, &indices);
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&path, &mut hash);
    std::hash::Hash::hash(&vertices.len(), &mut hash);
    std::hash::Hash::hash(&indices.len(), &mut hash);
    let key = std::hash::Hasher::finish(&hash) | (1 << 63);
    Ok(PreviewGeometry {
        key,
        vertices,
        indices,
    })
}

/// Gives every vertex whose normal is zero (the file had none) the area-weighted sum of its
/// faces' normals, and a tangent perpendicular to it.
fn rebuild_missing_normals(vertices: &mut [LabVertex], indices: &[u32]) {
    let missing: Vec<bool> = vertices
        .iter()
        .map(|v| Vec3::from_array(v.normal).length_squared() < 1e-12)
        .collect();
    if !missing.iter().any(|m| *m) {
        return;
    }
    let mut sums = vec![Vec3::ZERO; vertices.len()];
    for tri in indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| i as usize);
        let (Some(pa), Some(pb), Some(pc)) = (vertices.get(a), vertices.get(b), vertices.get(c)) else {
            continue;
        };
        let (pa, pb, pc) = (
            Vec3::from_array(pa.position),
            Vec3::from_array(pb.position),
            Vec3::from_array(pc.position),
        );
        let face = (pb - pa).cross(pc - pa);
        for i in [a, b, c] {
            sums[i] += face;
        }
    }
    for ((v, sum), missing) in vertices.iter_mut().zip(sums).zip(missing) {
        if !missing {
            continue;
        }
        let n = sum.normalize_or(Vec3::Y);
        v.normal = n.to_array();
        let t = if n.y.abs() < 0.9 {
            Vec3::Y.cross(n)
        } else {
            Vec3::X.cross(n)
        }
        .normalize_or(Vec3::X);
        v.tangent = [t.x, t.y, t.z, 1.0];
    }
}

fn vertex(position: Vec3, normal: Vec3, uv: [f32; 2], tangent: Vec3) -> LabVertex {
    LabVertex {
        position: position.to_array(),
        normal: normal.to_array(),
        uv,
        tangent: [tangent.x, tangent.y, tangent.z, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// The preview cube: side 1.2 centered at the origin (corners about as far out as the unit sphere), four vertices per face (flat normals,
/// a full 0..1 UV square per face).
#[must_use]
pub fn preview_cube() -> (Vec<LabVertex>, Vec<u32>) {
    const H: f32 = 0.6;
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    // (normal, u axis, v axis): u × v = normal, so the corners wind counter-clockwise outside.
    let faces = [
        (Vec3::X, Vec3::NEG_Z, Vec3::Y),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::X, Vec3::NEG_Z),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y),
    ];
    for (normal, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = (normal + u * su + v * sv) * H;
            vertices.push(vertex(p, normal, [(su + 1.0) * 0.5, (1.0 - sv) * 0.5], u));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// The preview plane: a 2.4 × 2.4 square at y = 0 facing up (+Y), UV 0..1.
#[must_use]
pub fn preview_plane() -> (Vec<LabVertex>, Vec<u32>) {
    const H: f32 = 1.2;
    let vertices = [(-1.0f32, 1.0f32), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)]
        .into_iter()
        .map(|(x, z)| {
            vertex(
                Vec3::new(x * H, 0.0, z * H),
                Vec3::Y,
                [(x + 1.0) * 0.5, (1.0 - z) * 0.5],
                Vec3::X,
            )
        })
        .collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

/// Renders `geometry` with `lab_material` from `view` on the editor's persistent preview
/// renderer (the engine's frame renderer, editor profile, as the viewport). `material_key` 0
/// draws the material once without caching it. `None` when the render fails. This is the
/// default preview scene ([`render_preview_scene`] with `PreviewContext::default()`).
#[must_use]
pub fn render_preview(
    device: &RenderDevice,
    lab_material: &LabMaterial,
    geometry: &PreviewGeometry,
    view: &PreviewView,
    size: [f32; 2],
    material_key: u64,
) -> Option<egui::ColorImage> {
    render_preview_scene(
        device,
        lab_material,
        geometry,
        view,
        size,
        material_key,
        &crate::preview_scene::PreviewContext::default(),
    )
}

/// [`render_preview`] in a shared preview scene (`crate::preview_scene`, ADR-7771): the lighting
/// preset and environment intensity, an optional floor with a grid under the subject, and the
/// turntable's turn. The material graph editor hooks in here: pass its panel's
/// `PreviewContext` where it calls [`render_preview`], and draw
/// `crate::preview_scene::preview_scene_controls` beside its shape controls.
#[must_use]
pub fn render_preview_scene(
    device: &RenderDevice,
    lab_material: &LabMaterial,
    geometry: &PreviewGeometry,
    view: &PreviewView,
    size: [f32; 2],
    material_key: u64,
    scene: &crate::preview_scene::PreviewContext,
) -> Option<egui::ColorImage> {
    render_preview_look(
        device,
        lab_material,
        geometry,
        view,
        size,
        material_key,
        (scene, regia_render::FrameQuality::EDITOR),
    )
}

/// [`render_preview`] with the frame quality given (the material graph editor's tier preview,
/// w12 ADR-7851: a render profile's `FrameQuality::from_profile`), in the default preview scene.
#[must_use]
pub fn render_preview_with_quality(
    device: &RenderDevice,
    lab_material: &LabMaterial,
    geometry: &PreviewGeometry,
    view: &PreviewView,
    size: [f32; 2],
    material_key: u64,
    quality: regia_render::FrameQuality,
) -> Option<egui::ColorImage> {
    render_preview_look(
        device,
        lab_material,
        geometry,
        view,
        size,
        material_key,
        (&crate::preview_scene::PreviewContext::default(), quality),
    )
}

/// [`render_preview_scene`] with the frame quality given: the material graph editor's preview in
/// its preview scene at its tier (w13, P-083).
#[allow(clippy::too_many_arguments)] // ADR-7851's preview inputs plus the scene (ADR-7771).
#[must_use]
pub fn render_preview_scene_with_quality(
    device: &RenderDevice,
    lab_material: &LabMaterial,
    geometry: &PreviewGeometry,
    view: &PreviewView,
    size: [f32; 2],
    material_key: u64,
    scene: &crate::preview_scene::PreviewContext,
    quality: regia_render::FrameQuality,
) -> Option<egui::ColorImage> {
    render_preview_look(
        device,
        lab_material,
        geometry,
        view,
        size,
        material_key,
        (scene, quality),
    )
}

/// The shared body of [`render_preview_scene`] and [`render_preview_with_quality`]: `look` is the
/// preview scene and the frame quality.
#[tracing::instrument(target = "regia_editor", skip_all, name = "material_shape_preview")]
fn render_preview_look(
    device: &RenderDevice,
    lab_material: &LabMaterial,
    geometry: &PreviewGeometry,
    view: &PreviewView,
    size: [f32; 2],
    material_key: u64,
    look: (&crate::preview_scene::PreviewContext, regia_render::FrameQuality),
) -> Option<egui::ColorImage> {
    use crate::preview_scene::{Backdrop, PreviewFloor, SunBase};
    let (scene, quality) = look;
    /// One stable object identity per preview.
    const PREVIEW_OBJECT_KEY: u64 = 1;

    static RENDERER: OnceLock<Mutex<DeviceFrameRenderer>> = OnceLock::new();

    let width = size[0].max(64.0) as u32;
    let height = size[1].max(64.0) as u32;
    let sky_mat = crate::render::sky_material();
    let sky = crate::render::sky_sphere();
    let eye = view.eye();
    let rig = scene.rig(
        SunBase {
            direction: view.light_direction(),
            color: [1.0, 1.0, 1.0],
        },
        PREVIEW_MESH_RADIUS,
    );
    let flat = crate::preview_scene::flat_backdrop_material(match rig.backdrop {
        Backdrop::Flat(color) => color,
        Backdrop::Sky => [0.0; 3],
    });
    let floor_materials = PreviewFloor::default();
    let mut meshes = Vec::new();
    crate::preview_scene::push_backdrop(&mut meshes, rig.backdrop, (&sky_mat, sky), &flat, eye, 18.0);
    if scene.settings.floor {
        let min_y = geometry
            .vertices
            .iter()
            .map(|v| v.position[1])
            .fold(f32::INFINITY, f32::min);
        let (y, half) = crate::preview_scene::floor_placement(min_y, PREVIEW_MESH_RADIUS);
        crate::preview_scene::push_floor(&mut meshes, &floor_materials, y, half);
    }
    let turn = scene.spin.model(&scene.settings, Vec3::ZERO);
    meshes.push(FrameMesh {
        is_static: false,
        mesh_key: geometry.key,
        material_key,
        object_key: PREVIEW_OBJECT_KEY,
        vertices: &geometry.vertices,
        indices: &geometry.indices,
        material: lab_material,
        model: turn.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: regia_render::ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    });
    let mut frame = FrameScene {
        width,
        height,
        eye: [eye.x, eye.y, eye.z],
        target: [0.0, 0.0, 0.0],
        fov_y: std::f32::consts::FRAC_PI_4,
        near: 0.05,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: rig.ibl,
        exposure_ev: crate::render::VIEWPORT_EXPOSURE_EV,
        ambient_lux: rig.ambient_lux,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: rig.directional,
        local_lights: &rig.local_lights,
        quality,
        meshes,
    };
    let mut renderer = RENDERER
        .get_or_init(|| Mutex::new(DeviceFrameRenderer::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let frame_renderer = renderer.get(device);
    frame.ibl = rig.ibl && crate::preview_scene::apply_environment(frame_renderer, device, &rig.environment);
    let result = frame_renderer.render_offscreen(device, &frame).ok()?;
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [result.width as usize, result.height as usize],
        &result.pixels,
    ))
}

/// A UV sphere (standard spherical parameterization; see `docs/provenance.md`) as vertex and
/// index lists, centred at the origin with radius `r`.
///
/// Rings run pole-to-pole (`rings`), stacks around the equator (`stacks`); normals equal the
/// (scaled) positions, so the preview lights it smoothly.
#[must_use]
pub fn preview_sphere(r: f32, rings: usize, stacks: usize) -> (Vec<LabVertex>, Vec<u32>) {
    let rings = rings.max(3);
    let stacks = stacks.max(2);
    let mut vertices: Vec<LabVertex> = Vec::with_capacity((rings + 1) * (stacks + 1));
    let mut indices: Vec<u32> = Vec::with_capacity(rings * stacks * 6);
    for stack in 0..=stacks {
        let phi = std::f32::consts::PI * stack as f32 / stacks as f32;
        let (sin_phi, cos_phi) = phi.sin_cos();
        for ring in 0..=rings {
            let theta = 2.0 * std::f32::consts::PI * ring as f32 / rings as f32;
            let (sin_theta, cos_theta) = theta.sin_cos();
            let normal = Vec3::new(sin_phi * cos_theta, cos_phi, sin_phi * sin_theta);
            let position = normal * r;
            vertices.push(LabVertex {
                position: [position.x, position.y, position.z],
                normal: [normal.x, normal.y, normal.z],
                uv: [ring as f32 / rings as f32, stack as f32 / stacks as f32],
                tangent: [cos_theta, 0.0, -sin_theta, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    for stack in 0..stacks {
        for ring in 0..rings {
            let a = (stack * (rings + 1) + ring) as u32;
            let b = a + 1;
            let c = a + (rings + 1) as u32;
            let d = c + 1;
            indices.extend_from_slice(&[a, b, d, a, d, c]);
        }
    }
    (vertices, indices)
}

/// The project root of a file under `<root>/assets/` (the folder above the nearest `assets`).
fn project_root_of(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "assets"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// Draws the Material editor panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "material_editor_panel")]
pub fn material_editor_panel(
    ui: &mut egui::Ui,
    state: &mut MaterialEditorState,
    device: Option<&RenderDevice>,
) {
    ui.horizontal(|ui| {
        ui.strong("Material");
        match &state.path {
            Some(path) => {
                ui.weak(path.display().to_string());
                if state.dirty {
                    ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.unsaved, "· unsaved");
                }
            }
            None => {
                ui.weak("(no material open)");
            }
        }
    });
    ui.separator();
    if !state.is_open() {
        ui.weak("Double-click a .material.ron in the Content Browser to edit it.");
        return;
    }

    egui::Panel::left("material-preview")
        .resizable(false)
        .exact_size(220.0)
        .show(ui, |ui| {
            crate::matgraph_editor::panel::preview_controls_ui(
                ui,
                &mut state.preview_shape,
                &mut state.preview_view,
                &mut state.mesh_field,
            );
            crate::preview_scene::preview_scene_controls(ui, &mut state.scene.settings);
            state.scene.tick(ui.ctx());
            let side = ui.available_width().clamp(64.0, 220.0);
            match device {
                Some(device) if state.preview_due(side) => {
                    let _span = tracing::info_span!(target: "regia_editor", "material_preview_render").entered();
                    state.preview_renders += 1;
                    let root = state.path.as_deref().and_then(project_root_of);
                    let rendered = preview_geometry(&state.preview_shape, root.as_deref())
                        .map_err(|e| format!("the preview mesh cannot load: {e}"))
                        .and_then(|geometry| {
                            render_preview_scene(
                                device,
                                &preview_material(&state.material),
                                &geometry,
                                &state.preview_view,
                                [side, side],
                                1,
                                &state.scene,
                            )
                            .ok_or_else(|| "the preview render failed (see the log)".to_string())
                        });
                    match rendered {
                        Ok(image) => {
                            state.preview_image.0 = Some(ui.ctx().load_texture(
                                "material-preview",
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                            state.preview_error = None;
                        }
                        // Keep the last image; say why this one has none.
                        Err(err) => state.preview_error = Some(err),
                    }
                }
                // Unchanged since the last render: keep its image (or its error).
                Some(_) => {}
                None => {
                    ui.weak("(preview needs a GPU device)");
                }
            }
            if let Some(texture) = &state.preview_image.0 {
                let response = ui.add(
                    egui::Image::new((texture.id(), egui::vec2(side, side))).sense(egui::Sense::drag()),
                );
                crate::matgraph_editor::panel::orbit_drag(&response, &mut state.preview_view);
            }
            if let Some(err) = &state.preview_error {
                ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.log_warn, err);
            }
            ui.weak("base PBR (color, metallic, roughness); emissive and layers serialize but the forward pass does not show them yet");
        });

    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let material = state.material.clone();

            // Base parameters.
            ui.strong("Base");
            let mut color = [
                linear_to_srgb_u8(material.base.base_color[0]),
                linear_to_srgb_u8(material.base.base_color[1]),
                linear_to_srgb_u8(material.base.base_color[2]),
            ];
            if ui
                .color_edit_button_srgb(&mut color)
                .on_hover_text("Linear base color factor")
                .changed()
            {
                let linear = [
                    srgb8_to_linear(color[0]),
                    srgb8_to_linear(color[1]),
                    srgb8_to_linear(color[2]),
                ];
                state.edit(|m| {
                    m.base.base_color = [linear[0], linear[1], linear[2], m.base.base_color[3]];
                });
            }
            slider_01(ui, state, "Metallic", material.base.metallic, |m, v| {
                m.base.metallic = v
            });
            slider_01(ui, state, "Roughness", material.base.roughness, |m, v| {
                m.base.roughness = v
            });

            // Emissive.
            ui.strong("Emissive");
            let mut emissive = [
                linear_to_srgb_u8(material.base.emissive[0]),
                linear_to_srgb_u8(material.base.emissive[1]),
                linear_to_srgb_u8(material.base.emissive[2]),
            ];
            if ui.color_edit_button_srgb(&mut emissive).changed() {
                let linear = [
                    srgb8_to_linear(emissive[0]),
                    srgb8_to_linear(emissive[1]),
                    srgb8_to_linear(emissive[2]),
                ];
                state.edit(|m| m.base.emissive = linear);
            }
            let mut emissive_boost = material.base.emissive[0]
                .max(material.base.emissive[1])
                .max(material.base.emissive[2]);
            if ui
                .add(
                    egui::DragValue::new(&mut emissive_boost)
                        .speed(0.05)
                        .range(0.0..=64.0)
                        .suffix("×"),
                )
                .on_hover_text("Scale the emissive color's brightness")
                .changed()
                && emissive_boost > 0.0
            {
                let scale = emissive_boost / material.base.emissive[0].max(1e-6);
                state.edit(|m| {
                    m.base.emissive = [
                        m.base.emissive[0] * scale,
                        m.base.emissive[1] * scale,
                        m.base.emissive[2] * scale,
                    ];
                });
            }

            // Render state.
            ui.strong("Render state");
            let alpha = format!("{:?}", material.render_state.alpha_mode).to_lowercase();
            egui::ComboBox::from_id_salt("alpha-mode")
                .selected_text(alpha.as_str())
                .show_ui(ui, |ui| {
                    for mode in ["opaque", "mask", "blend"] {
                        if ui.selectable_label(alpha == mode, mode).clicked() {
                            let parsed = match mode {
                                "mask" => regia_asset::AlphaMode::Mask,
                                "blend" => regia_asset::AlphaMode::Blend,
                                _ => regia_asset::AlphaMode::Opaque,
                            };
                            state.edit(|m| m.render_state.alpha_mode = parsed);
                        }
                    }
                });

            // Shader + parent info (read-only summary).
            ui.separator();
            let shader = match &material.shader {
                MaterialShaderRef::Standard => "Standard".to_string(),
                MaterialShaderRef::Custom(id) => format!("custom {id}"),
            };
            ui.weak(format!("shader {shader} · {} layers", material.layers.len()));

            if !state.toast.is_empty() {
                ui.separator();
                ui.weak(&state.toast);
            }
        });
    });

    // The panel-local toolbar (Save / Reload / Undo / Redo).
    ui.horizontal(|ui| {
        if ui
            .add_enabled(state.dirty, egui::Button::new("Save material"))
            .clicked()
            && let Err(err) = state.save()
        {
            state.toast = err;
        }
        if crate::agent_parity::action_enabled(
            ui,
            state.undo_depth() > 0,
            &crate::agent_parity::MATERIAL_UNDO,
            None,
        )
        .clicked()
        {
            state.undo();
        }
        if crate::agent_parity::action_enabled(
            ui,
            state.redo_depth() > 0,
            &crate::agent_parity::MATERIAL_REDO,
            None,
        )
        .clicked()
        {
            state.redo();
        }
        if let Some(path) = state.path.clone()
            && crate::agent_parity::action_button(ui, &crate::agent_parity::MATERIAL_RELOAD).clicked()
        {
            let _ = state.open(&path);
        }
    });
}

/// A 0..1 slider that edits the material through the undo snapshot.
fn slider_01(
    ui: &mut egui::Ui,
    state: &mut MaterialEditorState,
    label: &str,
    value: f32,
    apply: impl Fn(&mut MaterialAssetRon, f32) + Copy,
) {
    let mut v = value;
    if ui.add(egui::Slider::new(&mut v, 0.0..=1.0).text(label)).changed() {
        state.edit(|m| apply(m, v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-editor-{label}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn preview_sphere_is_well_formed() {
        let (vertices, indices) = preview_sphere(1.0, 20, 14);
        assert_eq!(vertices.len(), 21 * 15, "(rings+1) * (stacks+1)");
        assert_eq!(indices.len(), 20 * 14 * 6, "six indices per quad");
        assert!(indices.iter().all(|i| (*i as usize) < vertices.len()));
        // Every vertex sits on the unit sphere.
        for vertex in &vertices {
            let p = Vec3::new(vertex.position[0], vertex.position[1], vertex.position[2]);
            assert!((p.length() - 1.0).abs() < 1e-5, "radius 1, got {}", p.length());
        }
        // The pole vertices exist (phi = 0 and π).
        let top = vertices[0].position;
        assert!((top[1] - 1.0).abs() < 1e-5, "north pole: {top:?}");
        // Normals point outwards (equal to the position for a unit sphere).
        for vertex in &vertices {
            let p = Vec3::new(vertex.position[0], vertex.position[1], vertex.position[2]);
            let n = Vec3::new(vertex.normal[0], vertex.normal[1], vertex.normal[2]);
            assert!((p.normalize() - n).length() < 1e-5);
        }
    }

    /// Audit v033b (Low): every undo and redo set `dirty`, even the undo that put the opened
    /// material back, so "Save material" stayed enabled for a buffer equal to the file.
    #[test]
    fn undo_back_to_the_file_is_clean() {
        let dir = temp_project("mat-undo-clean");
        let path = dir.join("stone.material.ron");
        let material = MaterialAssetRon::default_standard();
        std::fs::write(&path, material.to_ron_string().expect("serialize")).expect("write");
        let mut state = MaterialEditorState::default();
        state.open(&path).expect("open");
        state.edit(|m| m.base.roughness = 0.9);
        assert!(state.dirty);
        assert!(state.undo());
        assert!(!state.dirty, "the buffer equals the file again");
        assert!(state.redo());
        assert!(state.dirty);
        state.save().expect("save");
        assert!(!state.dirty);
        assert!(state.undo());
        assert!(state.dirty, "the undo now differs from the saved file");

        // An agent save the panel adopts: one undo step, clean, and undo makes it dirty.
        let mut agent = MaterialAssetRon::default_standard();
        agent.base.metallic = 0.75;
        state.adopt_saved(agent.clone());
        assert!(!state.dirty);
        assert_eq!(state.material, agent);
        assert!(state.undo());
        assert!(state.dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edits_undo_redo_and_save_round_trip() {
        let dir = temp_project("mat-editor");
        let path = dir.join("paint.material.ron");
        let mut material = MaterialAssetRon::default_standard();
        material.name = Some("paint".to_string());
        std::fs::write(&path, material.to_ron_string().expect("serialize")).expect("write");

        let mut state = MaterialEditorState::default();
        state.open(&path).expect("open");
        assert!(!state.dirty);

        // Edit through the panel path: roughness scrub.
        for value in [0.4f32, 0.35, 0.25] {
            state.edit(|m| m.base.roughness = value);
        }
        assert_eq!(state.material.base.roughness, 0.25);
        assert!(state.dirty);
        assert_eq!(state.undo.len(), 3);

        // Undo twice, redo once.
        assert!(state.undo());
        assert!(state.undo());
        assert_eq!(state.material.base.roughness, 0.4);
        assert!(state.redo());
        assert_eq!(state.material.base.roughness, 0.35);

        // Save + reopen: the file round-trips.
        state.save().expect("save");
        assert!(!state.dirty);
        let bytes = std::fs::read(&path).expect("read");
        let parsed = MaterialAssetRon::from_ron(&bytes).expect("parse");
        assert_eq!(parsed.base.roughness, 0.35);

        // A fresh open sees the saved value (not the in-memory one).
        let mut second = MaterialEditorState::default();
        second.open(&path).expect("reopen");
        assert_eq!(second.material.base.roughness, 0.35);
        assert!(!second.dirty);

        // Opening garbage is a clean error.
        let bad = dir.join("bad.material.ron");
        std::fs::write(&bad, "not a material").expect("write");
        assert!(MaterialEditorState::default().open(&bad).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_material_carries_base_parameters() {
        let mut material = MaterialAssetRon::default_standard();
        material.base.base_color = [0.2, 0.4, 0.6, 1.0];
        material.base.metallic = 0.9;
        material.base.roughness = 0.15;
        material.base.emissive = [0.1, 0.9, 0.2];
        material.render_state.alpha_mode = AlphaMode::Mask;
        let lab = preview_material(&material);
        assert_eq!(lab.base_color, [0.2, 0.4, 0.6, 1.0]);
        assert_eq!(lab.metallic, 0.9);
        assert_eq!(lab.roughness, 0.15);
        assert_eq!(lab.emissive, [0.1, 0.9, 0.2]);
        assert_eq!(
            lab.emissive_intensity, 1.0,
            "asset format has no emissive strength"
        );
        assert_eq!(lab.alpha_mode, ALPHA_MASK);
    }

    /// The preview renders end to end on a GPU (skips without one) and lights
    /// the sphere: the brightest pixel must clearly beat the clear colour.
    #[test]
    fn preview_renders_a_lit_sphere() {
        let device = match crate::render::headless_device() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip preview_renders_a_lit_sphere: {err}");
                return;
            }
        };
        let image = render_material_preview(&device, &MaterialAssetRon::default_standard(), [96.0, 96.0])
            .expect("preview renders");
        assert_eq!(image.size, [96, 96]);
        let max_channel = image
            .pixels
            .iter()
            .flat_map(|c| [c.r(), c.g(), c.b()])
            .max()
            .unwrap_or(0);
        assert!(
            max_channel > 60,
            "sphere must be lit, brightest channel {max_channel}"
        );
        let report = crate::viewport::measure_viewport(&image);
        assert!(report.sky_visible(), "preview sky: {report:?}");
        assert!(report.shading_visible(), "preview shading: {report:?}");
    }

    /// The preview key changes with the material, the shape, the view and the size only.
    #[test]
    fn the_preview_is_due_only_on_change() {
        let mut state = MaterialEditorState::default();
        assert!(state.preview_due(200.0), "the first paint renders");
        assert!(!state.preview_due(200.0), "a repaint does not");
        state.edit(|m| m.base.metallic = 0.5);
        assert!(state.preview_due(200.0), "a parameter change does");
        state.preview_shape = PreviewShape::Cube;
        assert!(state.preview_due(200.0), "a shape change does");
        state.preview_view.light_yaw = 0.3;
        assert!(state.preview_due(200.0), "turning the light does");
        assert!(state.preview_due(180.0), "a new size does");
        assert!(!state.preview_due(180.0));
        assert!(state.undo());
        assert!(state.preview_due(180.0), "an undo does");
    }

    /// w5 (ADR-9970): the Material panel renders its preview once over 10 repaints of an
    /// unchanged material, and once more after a parameter change. Counted as preview image
    /// uploads (texture deltas other than the font atlas). Skips without a GPU.
    #[test]
    fn the_panel_preview_renders_only_on_change() {
        let device = match RenderDevice::headless() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip the_panel_preview_renders_only_on_change: {err}");
                return;
            }
        };
        let dir = temp_project("mat-preview-cache");
        let path = dir.join("paint.material.ron");
        std::fs::write(
            &path,
            MaterialAssetRon::default_standard()
                .to_ron_string()
                .expect("serialize"),
        )
        .expect("write");
        let mut state = MaterialEditorState::default();
        state.open(&path).expect("open");
        let ctx = egui::Context::default();
        let repaint = |state: &mut MaterialEditorState| -> usize {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| material_editor_panel(ui, state, Some(&device)));
            let uploads = output
                .textures_delta
                .set
                .iter()
                .filter(|(id, _)| **id != egui::TextureId::default())
                .count();
            output.textures_delta.clear();
            uploads
        };
        let first: usize = (0..10).map(|_| repaint(&mut state)).sum();
        assert_eq!(first, 1, "one render over 10 repaints of the same material");
        state.edit(|m| m.base.roughness = 0.2);
        let after: usize = (0..3).map(|_| repaint(&mut state)).sum();
        assert_eq!(after, 1, "a parameter change renders once more");
        assert_eq!(state.preview_render_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-7771: the scene key moves with every control, and a steady scene keeps one render.
    #[test]
    fn the_preview_is_due_again_when_the_scene_changes() {
        use crate::preview_scene::PreviewLighting;
        let mut state = MaterialEditorState::default();
        assert!(state.preview_due(200.0));
        assert!(!state.preview_due(200.0));
        state.scene.settings.lighting = PreviewLighting::Studio;
        assert!(state.preview_due(200.0), "a new lighting preset renders");
        assert!(!state.preview_due(200.0));
        state.scene.settings.env_intensity = 2.0;
        assert!(state.preview_due(200.0), "the environment intensity renders");
        state.scene.settings.floor = true;
        assert!(state.preview_due(200.0), "the floor renders");
        assert!(!state.preview_due(200.0));
        // The turntable: a steady preview keeps one key, a turning one has a new key each step.
        state.scene.settings.auto_rotate = true;
        assert!(state.preview_due(200.0), "turning the turntable on renders");
        state.scene.spin.advance_by(0.5);
        assert!(state.preview_due(200.0), "a turned angle renders again");
        state.scene.settings.auto_rotate = false;
        assert!(state.preview_due(200.0), "turning it off renders once more");
        assert!(!state.preview_due(200.0), "and then stays steady");
        // The level's sun only matters to the level preset.
        state.scene.level = Some(crate::preview_scene::LevelLighting {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: 40_000.0,
        });
        assert!(!state.preview_due(200.0), "an unused level sun does not render");
        state.scene.settings.lighting = PreviewLighting::Level;
        assert!(state.preview_due(200.0));
        state.scene.level = None;
        assert!(
            state.preview_due(200.0),
            "losing the level's sun falls back and renders"
        );
    }

    /// ADR-7771 on lavapipe: each control changes the picture in the way it says. Skips without a GPU.
    #[test]
    fn the_preview_scene_controls_change_the_picture() {
        use crate::preview_scene::{PreviewContext, PreviewLighting};
        let device = match crate::render::headless_device() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip the_preview_scene_controls_change_the_picture: {err}");
                return;
            }
        };
        let cube = preview_geometry(&PreviewShape::Cube, None).expect("cube");
        let material = preview_material(&MaterialAssetRon::default_standard());
        let render = |scene: &PreviewContext| {
            render_preview_scene(
                &device,
                &material,
                &cube,
                &PreviewView::default(),
                [128.0, 128.0],
                0,
                scene,
            )
            .expect("renders")
        };
        let diff = |a: &egui::ColorImage, b: &egui::ColorImage| -> Vec<usize> {
            a.pixels
                .iter()
                .zip(&b.pixels)
                .enumerate()
                .filter(|(_, (x, y))| x != y)
                .map(|(i, _)| i)
                .collect()
        };
        let control = render(&PreviewContext::default());
        let plain = render_preview(
            &device,
            &material,
            &cube,
            &PreviewView::default(),
            [128.0, 128.0],
            0,
        )
        .expect("plain");
        assert_eq!(
            control.pixels, plain.pixels,
            "the default scene is the old preview"
        );
        assert_eq!(
            control.pixels,
            render(&PreviewContext::default()).pixels,
            "deterministic"
        );

        // Presets: each differs from the default and from each other.
        let mut seen = vec![control.pixels.clone()];
        for lighting in [PreviewLighting::Studio, PreviewLighting::Neutral] {
            let mut scene = PreviewContext::default();
            scene.settings.lighting = lighting;
            let image = render(&scene);
            assert!(
                !seen.contains(&image.pixels),
                "{lighting:?} looks like an earlier preset"
            );
            seen.push(image.pixels);
        }
        // The level preset with a level sun differs; without one it is the outdoor picture.
        let mut level = PreviewContext::default();
        level.settings.lighting = PreviewLighting::Level;
        assert_eq!(
            render(&level).pixels,
            control.pixels,
            "no level sun: outdoor stands in"
        );
        level.level = Some(crate::preview_scene::LevelLighting {
            direction: [0.9, -0.3, 0.1],
            color: [1.0, 0.4, 0.2],
            illuminance: 60_000.0,
        });
        assert_ne!(render(&level).pixels, control.pixels, "the level's sun shows");

        // Environment intensity: more ambient light lifts the shadow side.
        let mut dark = PreviewContext::default();
        dark.settings.env_intensity = 0.0;
        let mut bright = PreviewContext::default();
        bright.settings.env_intensity = 4.0;
        let sum = |image: &egui::ColorImage| -> u64 {
            image
                .pixels
                .iter()
                .map(|c| u64::from(c.r()) + u64::from(c.g()) + u64::from(c.b()))
                .sum()
        };
        assert!(
            sum(&render(&bright)) > sum(&render(&dark)),
            "intensity 4 is brighter than 0"
        );

        // The floor adds pixels below the subject's middle and leaves the top row alone.
        let mut floor = PreviewContext::default();
        floor.settings.floor = true;
        let with_floor = render(&floor);
        let changed = diff(&control, &with_floor);
        assert!(changed.len() > 200, "the floor draws: {} pixels", changed.len());
        let mean_row = changed.iter().map(|i| (i / 128) as f32).sum::<f32>() / changed.len() as f32;
        assert!(
            mean_row > 64.0,
            "the floor is under the subject: mean row {mean_row}"
        );

        // The turntable turns a cube: 30 degrees looks different, a full turn looks the same.
        let mut turn = PreviewContext::default();
        turn.settings.auto_rotate = true;
        turn.spin.advance_by(1.0);
        assert_ne!(render(&turn).pixels, control.pixels, "a turned cube differs");
        let mut full = PreviewContext::default();
        full.settings.auto_rotate = true;
        assert_eq!(
            render(&full).pixels,
            control.pixels,
            "angle 0 is the untouched picture"
        );
    }

    /// Writes a contact sheet of the preview scene's presets to the folder named by
    /// `REGIA_PREVIEW_SCENE_PLATES` (a look review, not a check): `cargo test -p regia_editor
    /// --lib preview_scene_plates -- --ignored`.
    #[test]
    #[ignore = "writes plates for a look review"]
    fn preview_scene_plates() {
        use crate::preview_scene::{LevelLighting, PreviewContext, PreviewLighting};
        let Some(dir) = std::env::var_os("REGIA_PREVIEW_SCENE_PLATES") else {
            eprintln!("REGIA_PREVIEW_SCENE_PLATES is not set");
            return;
        };
        let device = crate::render::headless_device().expect("a headless device");
        let mut material = MaterialAssetRon::default_standard();
        material.base.base_color = [0.8, 0.35, 0.2, 1.0];
        material.base.metallic = 0.0;
        material.base.roughness = 0.35;
        let lab = preview_material(&material);
        let level = LevelLighting {
            direction: [0.8, -0.4, 0.2],
            color: [1.0, 0.6, 0.35],
            illuminance: 60_000.0,
        };
        let mut cells: Vec<egui::ColorImage> = Vec::new();
        for shape in [PreviewShape::Sphere, PreviewShape::Cube] {
            let geometry = preview_geometry(&shape, None).expect("shape");
            for (lighting, intensity, floor, rotate) in [
                (PreviewLighting::Outdoor, 1.0, false, false),
                (PreviewLighting::Outdoor, 0.3, false, false),
                (PreviewLighting::Outdoor, 2.5, false, false),
                (PreviewLighting::Studio, 1.0, false, false),
                (PreviewLighting::Studio, 1.0, true, true),
                (PreviewLighting::Neutral, 1.0, false, false),
                (PreviewLighting::Neutral, 1.0, true, false),
                (PreviewLighting::Level, 1.0, true, false),
            ] {
                let mut scene = PreviewContext::default();
                scene.settings.lighting = lighting;
                scene.settings.env_intensity = intensity;
                scene.settings.floor = floor;
                scene.settings.auto_rotate = rotate;
                scene.spin.advance_by(2.0);
                scene.level = Some(level);
                cells.push(
                    render_preview_scene(
                        &device,
                        &lab,
                        &geometry,
                        &PreviewView::default(),
                        [200.0, 200.0],
                        0,
                        &scene,
                    )
                    .expect("renders"),
                );
            }
        }
        let columns = 8;
        let rows = cells.len().div_ceil(columns);
        let (w, h) = (cells[0].size[0], cells[0].size[1]);
        let mut sheet = vec![0u8; columns * w * rows * h * 4];
        for (n, cell) in cells.iter().enumerate() {
            let (cx, cy) = ((n % columns) * w, (n / columns) * h);
            for y in 0..h {
                for x in 0..w {
                    let px = cell.pixels[y * w + x];
                    let at = ((cy + y) * columns * w + cx + x) * 4;
                    sheet[at..at + 4].copy_from_slice(&[px.r(), px.g(), px.b(), 255]);
                }
            }
        }
        let path = std::path::Path::new(&dir).join("preview-scene-sheet.png");
        let file = std::fs::File::create(&path).expect("file");
        let mut encoder = png::Encoder::new(file, (columns * w) as u32, (rows * h) as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("header")
            .write_image_data(&sheet)
            .expect("png");
        eprintln!("wrote {}", path.display());
    }

    /// An editor world for `dir`: the project and the shared undo history.
    fn hosted_world(dir: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.to_path_buf()),
        });
        world.insert_resource(crate::command::UndoStack::new());
        world
    }

    fn write_material(path: &Path, metallic: f32) {
        let mut material = MaterialAssetRon::default_standard();
        material.base.metallic = metallic;
        std::fs::write(path, material.to_ron_string().expect("serialize")).expect("write");
    }

    /// Global undo (P-322): in the editor a Material panel edit is an entry of the shared history,
    /// which the editor's Undo and Redo (and the panel's own) step.
    #[test]
    fn an_edit_in_material_editor_undoes_from_the_global_stack() {
        let dir = temp_project("mat-global-undo");
        let path = dir.join("stone.material.ron");
        write_material(&path, 0.0);
        let mut world = hosted_world(&dir);
        let mut state = MaterialEditorState::default();
        state.open(&path).expect("open");
        sync_shared_history(&mut world, &mut state);
        state.edit(|m| m.base.metallic = 0.7);
        state.edit(|m| m.base.roughness = 0.2);
        assert_eq!(state.undo_depth(), 2);
        sync_shared_history(&mut world, &mut state);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 2);
        assert!(state.undo.is_empty(), "no private step");
        // The editor's Undo (Edit ▸ Undo, Ctrl+Z, editor.undo).
        assert!(crate::command::undo_world(&mut world));
        sync_shared_history(&mut world, &mut state);
        assert_eq!(state.material.base.metallic, 0.7);
        assert!(state.dirty);
        // The panel's own Undo steps the same history.
        assert!(state.undo());
        sync_shared_history(&mut world, &mut state);
        assert_eq!(state.material.base.metallic, 0.0);
        assert!(!state.dirty, "back to the file");
        assert!(!state.undo(), "nothing left to undo");
        assert_eq!(state.redo_depth(), 2);
        assert!(state.redo());
        sync_shared_history(&mut world, &mut state);
        assert_eq!(state.material.base.metallic, 0.7);
        assert!(crate::command::redo_world(&mut world));
        sync_shared_history(&mut world, &mut state);
        assert_eq!(state.material.base.roughness, 0.2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Global undo (P-322): the Undo History panel lists a Material panel edit by its
    /// project-relative file; undoing it after another material was opened reopens the edited one.
    #[test]
    fn an_edit_in_material_editor_appears_in_undo_history() {
        let dir = temp_project("mat-global-history");
        let stone = dir.join("stone.material.ron");
        let paint = dir.join("paint.material.ron");
        write_material(&stone, 0.0);
        write_material(&paint, 0.5);
        let mut world = hosted_world(&dir);
        let mut state = MaterialEditorState::default();
        state.open(&stone).expect("open");
        sync_shared_history(&mut world, &mut state);
        state.edit(|m| m.base.metallic = 0.9);
        sync_shared_history(&mut world, &mut state);
        let history = world.resource::<crate::command::UndoStack>().history();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].documents,
            vec!["stone.material.ron".to_owned()],
            "{history:?}"
        );
        assert!(history[0].description.contains("stone.material.ron"));
        assert!(
            !world.resource::<crate::command::UndoStack>().is_dirty(),
            "a material edit does not dirty the level"
        );
        state.save().expect("save");
        // The shell syncs once per frame: the save and the next open are separate clicks.
        sync_shared_history(&mut world, &mut state);
        state.open(&paint).expect("open paint");
        sync_shared_history(&mut world, &mut state);
        assert!(crate::command::undo_world(&mut world));
        sync_shared_history(&mut world, &mut state);
        assert_eq!(
            state.path.as_deref(),
            Some(stone.as_path()),
            "undo reopens the edited material"
        );
        assert_eq!(state.material.base.metallic, 0.0);
        assert!(state.dirty, "the saved file holds the edit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Global undo (P-322): an external undo is shown before the panel's buffer is remembered,
    /// and a history jump over edits of two materials restores both and shows the last one.
    #[test]
    fn a_history_jump_over_two_materials_restores_both() {
        let dir = temp_project("mat-global-jump");
        let stone = dir.join("stone.material.ron");
        let paint = dir.join("paint.material.ron");
        write_material(&stone, 0.0);
        write_material(&paint, 0.5);
        let mut world = hosted_world(&dir);
        let mut state = MaterialEditorState::default();
        state.open(&stone).expect("open");
        sync_shared_history(&mut world, &mut state);
        state.edit(|m| m.base.metallic = 0.1);
        sync_shared_history(&mut world, &mut state);
        state.open(&paint).expect("open paint");
        sync_shared_history(&mut world, &mut state);
        state.edit(|m| m.base.metallic = 0.6);
        sync_shared_history(&mut world, &mut state);
        crate::command::jump_world(&mut world, 0).expect("jump to the start");
        sync_shared_history(&mut world, &mut state);
        assert_eq!(
            state.path.as_deref(),
            Some(stone.as_path()),
            "the last step reverted stone"
        );
        assert_eq!(state.material.base.metallic, 0.0);
        crate::command::jump_world(&mut world, 2).expect("jump to the end");
        sync_shared_history(&mut world, &mut state);
        assert_eq!(state.path.as_deref(), Some(paint.as_path()));
        assert_eq!(
            state.material.base.metallic, 0.6,
            "the paint edit survived the jump"
        );
        crate::command::jump_world(&mut world, 1).expect("jump to the middle");
        sync_shared_history(&mut world, &mut state);
        assert_eq!(state.material.base.metallic, 0.5, "paint back to its file");
        assert!(!state.dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
