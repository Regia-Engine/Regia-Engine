// SPDX-License-Identifier: MPL-2.0
//! The Fracture panel (`book/agents/fracture.md`, ADR-1967): pick a box or a project mesh, set the
//! fracture and cook settings, preview the chunks and write the result.
//!
//! The state lives in the [`FracturePanel`] resource, which the panel and the agent methods share
//! (`editor.fracture_panel_get`, `_set`, `_run`, `_save`, `_apply_meta`), so what an agent sets is
//! what the panel shows. A run happens on a worker thread, so a thousand-chunk fracture never
//! stalls a frame; the viewport draws the chunks' edges pulled apart by `explode`
//! ([`fracture_gizmo_segments`]).
//!
//! Two ways out: **Save** writes `<out>.fracture.ron` and `<out>.destructible.ron` under the
//! project, and **Apply to .meta** stores the settings in the mesh's `.meta`, so `regia cook`
//! makes the chunks from then on (`regia_asset_pipeline::fracture_cook`).

use std::path::{Component, Path, PathBuf};
use std::thread::JoinHandle;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::fracture::{
    FractureChunk, FractureError, FractureSet, FractureSource, box_mesh, fracture_source,
};
use regia_asset_pipeline::fracture_cook::{FractureCookSettings, fracture_settings, set_meta_fracture};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::audio_gizmo::Segment;
use crate::fracture_tools::{ClickMode, SupportPick, TestBreak};
use crate::viewport_agent::Handler;

/// Colour of the chunk edges in the viewport.
pub const FRACTURE_EDGES: egui::Color32 = egui::Color32::from_rgb(235, 170, 80);

/// Most edges the viewport draws; past it the preview thins out, chunk by chunk.
const MAX_GIZMO_SEGMENTS: usize = 20_000;

/// Why a panel action failed: a stable code (`book/agents/fracture.md`) and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelError {
    /// Agent error code.
    pub code: String,
    /// What went wrong.
    pub message: String,
}

impl PanelError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }

    pub(crate) fn rpc(&self) -> RpcError {
        let mut err = RpcError::invalid_params(self.message.clone());
        if let Some(data) = err.data.as_mut() {
            data["reason"] = json!(self.code);
        }
        err
    }
}

impl From<FractureError> for PanelError {
    fn from(e: FractureError) -> Self {
        Self::new(e.code(), e.to_string())
    }
}

/// What the panel fractures.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelSource {
    /// A box of these half extents.
    Box {
        /// Half extents, metres.
        half: [f32; 3],
    },
    /// A closed `.glb` or `.gltf` under the project.
    Mesh {
        /// Path relative to the project root.
        path: String,
    },
}

impl Default for PanelSource {
    fn default() -> Self {
        Self::Box {
            half: [1.0, 1.0, 0.25],
        }
    }
}

/// Impact-biased site sampling as the panel holds it ([`regia_asset_pipeline::fracture::
/// FractureSites::Impact`], ADR-9870): a preview-only choice the mesh `.meta` does not store,
/// like [`FracturePanel::sites`].
#[derive(Clone, Debug, PartialEq)]
pub struct ImpactSites {
    /// Where the hit lands (the source's space).
    pub at: [f32; 3],
    /// Falloff level: the site density per unit volume falls off as `1/r^bias` with the
    /// distance from `at`: 0 (even), 1 or 2 (the strongest).
    pub bias: f32,
}

impl Default for ImpactSites {
    fn default() -> Self {
        Self {
            at: [0.0; 3],
            bias: 1.0,
        }
    }
}

/// Clustered site sampling as the panel holds it ([`regia_asset_pipeline::fracture::
/// FractureSites::Clusters`], ADR-9870): a preview-only choice the mesh `.meta` does not store,
/// like [`FracturePanel::sites`].
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterSites {
    /// Number of clumps, 1 to the chunk count.
    pub clusters: usize,
    /// Radius of one clump, as a fraction of the source's bounding-box half-diagonal
    /// (`MIN_CLUSTER_SPREAD` = 0.001 to 1).
    pub spread: f32,
}

impl Default for ClusterSites {
    fn default() -> Self {
        Self {
            clusters: 4,
            spread: 0.25,
        }
    }
}

/// The panel's state.
#[derive(Resource, Debug)]
pub struct FracturePanel {
    /// What to fracture.
    pub source: PanelSource,
    /// The settings; the same block `.meta` stores.
    pub settings: FractureCookSettings,
    /// How far the preview pulls chunks apart: 0 is the solid, 1 doubles each chunk's distance
    /// from the middle.
    pub explode: f32,
    /// Draw the chunks in the viewport.
    pub show_gizmo: bool,
    /// Where the viewport draws the set.
    pub origin: [f32; 3],
    /// The last finished run.
    pub result: Option<FractureSet>,
    /// The last failure.
    pub error: Option<PanelError>,
    /// What the last save or apply wrote, for the panel's status line.
    pub last_written: Vec<String>,
    /// Explicit Voronoi sites (solid space) instead of the seeded sampler: a preview-only choice
    /// the mesh `.meta` does not store.
    pub sites: Option<Vec<[f32; 3]>>,
    /// Impact-biased site sampling instead of the seeded sampler (a preview-only choice like
    /// [`Self::sites`]; when several are set, the run takes `sites` first, then this, then
    /// [`Self::clustered`]).
    pub impact: Option<ImpactSites>,
    /// Clustered site sampling instead of the seeded sampler (a preview-only choice like
    /// [`Self::sites`]; the panel clears the others when it sets one).
    pub clustered: Option<ClusterSites>,
    /// Which chunks the world holds up.
    pub supports: SupportPick,
    /// What a viewport click does.
    pub click: ClickMode,
    /// Impulse of a click hit, newton-seconds.
    pub hit_impulse: f32,
    /// Radius of a click hit, metres.
    pub hit_radius: f32,
    /// The test break, once hit.
    pub test: Option<TestBreak>,
    /// Draw the bonds, colored by break impulse.
    pub show_bonds: bool,
    /// The bond under the pointer or the list row: the list row's while one is hovered, else the
    /// viewport's; `None` once the pointer is over neither (ADR-7451 (3)).
    pub hover_bond: Option<u32>,
    /// The bond the weakest list's hovered row names (last panel frame).
    pub(crate) hover_list: Option<u32>,
    /// The bond the viewport pointer names (last viewport frame).
    pub(crate) hover_view: Option<u32>,
    /// Counts finished runs: the bond cache's key for "the result changed" (ADR-7451 (4)).
    pub(crate) result_rev: u64,
    /// The `.destructible.ron` the last **Save** wrote (project-relative): what **Place** spawns.
    pub saved: Option<String>,
    /// The bonds the bond view cooked last and what they were cooked from (ADR-7451 (4)).
    pub(crate) bond_cache: std::sync::Mutex<crate::fracture_tools::BondCache>,
    job: Option<JoinHandle<Result<FractureSet, PanelError>>>,
}

impl Default for FracturePanel {
    fn default() -> Self {
        Self {
            source: PanelSource::default(),
            settings: FractureCookSettings::default(),
            explode: 0.0,
            show_gizmo: false,
            origin: [0.0; 3],
            result: None,
            error: None,
            last_written: Vec::new(),
            sites: None,
            impact: None,
            clustered: None,
            supports: SupportPick::default(),
            click: ClickMode::default(),
            hit_impulse: DEFAULT_HIT_IMPULSE,
            hit_radius: DEFAULT_HIT_RADIUS,
            test: None,
            show_bonds: false,
            hover_bond: None,
            hover_list: None,
            hover_view: None,
            result_rev: 0,
            saved: None,
            bond_cache: std::sync::Mutex::default(),
            job: None,
        }
    }
}

/// Impulse of a click hit until the user sets one, newton-seconds.
pub const DEFAULT_HIT_IMPULSE: f32 = 10_000.0;

/// Radius of a click hit until the user sets one, metres.
pub const DEFAULT_HIT_RADIUS: f32 = 0.5;

impl FracturePanel {
    /// How many times the bond view cooked the result for its bonds (ADR-7451 (4)): a cook runs
    /// when the result, the settings or the support pick changed, not on every repaint.
    #[must_use]
    pub fn bond_cooks(&self) -> u64 {
        self.bond_cache
            .lock()
            .map_or_else(|e| e.into_inner().cooks, |c| c.cooks)
    }

    /// Records the bond one hover source names this frame (`list`: the weakest list, else the
    /// viewport) and recomputes [`Self::hover_bond`]: the list row wins while one is hovered.
    pub(crate) fn set_hover(&mut self, list: bool, bond: Option<u32>) {
        if list {
            self.hover_list = bond;
        } else {
            self.hover_view = bond;
        }
        self.hover_bond = self.hover_list.or(self.hover_view);
    }

    /// Keeps the clustered clump count within the chunk count (ADR-7550): lowering `chunks`
    /// through the panel's field or `editor.fracture_panel_set` clamps it, where an explicit
    /// `clusters` above `chunks` is refused (`fracture_panel_bad_clusters`).
    pub fn clamp_clusters(&mut self) {
        let chunks = self.settings.chunks.max(1);
        if let Some(c) = &mut self.clustered {
            c.clusters = c.clusters.clamp(1, chunks);
        }
    }

    /// Whether a run is going.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.job.is_some()
    }

    /// Starts a run on a worker thread.
    ///
    /// # Errors
    /// `fracture_panel_busy` while one runs, `fracture_panel_bad_path` for a mesh path outside the
    /// project, `fracture_panel_no_project` when a mesh is asked for with no project open,
    /// `fracture_panel_io` when the run thread cannot start.
    pub fn start(&mut self, project: Option<&Path>) -> Result<(), PanelError> {
        if self.job.is_some() {
            return Err(PanelError::new("fracture_panel_busy", "a run is going"));
        }
        let mesh = match &self.source {
            PanelSource::Box { .. } => None,
            PanelSource::Mesh { path } => Some(project_path(project, path)?),
        };
        let source = self.source.clone();
        let settings = self.settings.clone();
        let sites = self.sites.clone();
        let impact = self.impact.clone();
        let clustered = self.clustered.clone();
        // `Builder`, not `thread::spawn`: a refused thread is an error answer, not a panic
        // (audit 11-03 editor L2).
        let job = std::thread::Builder::new()
            .name("editor-fracture".to_string())
            .spawn(move || {
                let _span = tracing::info_span!("editor.fracture_panel_run").entered();
                run(&source, mesh.as_deref(), &settings, sites, impact, clustered)
            })
            .map_err(|e| PanelError::new("fracture_panel_io", format!("cannot start the run thread: {e}")))?;
        self.error = None;
        self.job = Some(job);
        Ok(())
    }

    /// Collects a finished run; the panel calls this each frame.
    pub fn poll(&mut self) {
        if self.job.as_ref().is_some_and(JoinHandle::is_finished) {
            self.collect();
        }
    }

    /// Blocks until the running job ends and collects it (tools and tests).
    pub fn wait(&mut self) {
        self.collect();
    }

    fn collect(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        match job.join() {
            Ok(Ok(set)) => {
                self.result = Some(set);
                self.result_rev += 1;
                self.show_gizmo = true;
                // Chunk indices and bonds name one result.
                self.test = None;
                self.hover_bond = None;
                self.hover_list = None;
                self.hover_view = None;
                if matches!(self.supports, SupportPick::Chunks(_)) {
                    self.supports = SupportPick::Rule;
                }
            }
            Ok(Err(e)) => self.error = Some(e),
            Err(_) => {
                self.error = Some(PanelError::new(
                    "fracture_panel_io",
                    "the fracture worker panicked",
                ));
            }
        }
    }

    /// Writes `<out>.fracture.ron` and `<out>.destructible.ron`; `out` is relative to the project.
    /// Returns the two paths.
    ///
    /// # Errors
    /// `fracture_panel_no_result` before a run, the path codes of [`Self::start`],
    /// `fracture_panel_io` for a failed write, `destruction_*` when the cook refuses the settings.
    pub fn save(&mut self, project: Option<&Path>, out: &str) -> Result<Vec<String>, PanelError> {
        let Some(set) = &self.result else {
            return Err(PanelError::new(
                "fracture_panel_no_result",
                "run the fracture before saving it",
            ));
        };
        let set_text = set.to_ron()?;
        let stem = project_path(project, out)?;
        let name = stem
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| PanelError::new("fracture_panel_bad_path", "`out` needs a file name"))?
            .to_string();
        let mut asset = crate::fracture_tools::cook_asset(self, &name)?;
        asset.meshes = Some(format!("{name}.fracture.ron"));
        let asset_text = asset
            .to_ron()
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        let written = vec![format!("{out}.fracture.ron"), format!("{out}.destructible.ron")];
        write_atomic(&suffixed(&stem, ".fracture.ron"), &set_text)?;
        write_atomic(&suffixed(&stem, ".destructible.ron"), &asset_text)?;
        self.last_written.clone_from(&written);
        self.saved = Some(written[1].clone());
        Ok(written)
    }

    /// Stores the settings in the source mesh's `.meta`, so `regia cook` fractures it.
    ///
    /// # Errors
    /// `fracture_panel_no_mesh` for a box, the path codes of [`Self::start`], `fracture_panel_meta`
    /// when the `.meta` cannot be read or updated.
    pub fn apply_meta(&mut self, project: Option<&Path>) -> Result<String, PanelError> {
        let PanelSource::Mesh { path } = &self.source else {
            return Err(PanelError::new(
                "fracture_panel_no_mesh",
                "only a project mesh has a .meta",
            ));
        };
        if self.sites.is_some() || self.impact.is_some() || self.clustered.is_some() {
            return Err(PanelError::new(
                "fracture_panel_meta_sites",
                "explicit, impact or clustered sites are a preview choice the .meta does not \
                 store; clear them first",
            ));
        }
        if self.supports != SupportPick::Rule {
            return Err(PanelError::new(
                "fracture_panel_meta_supports",
                "picked or boxed supports are saved in the .destructible.ron only; the .meta keeps \
                 support_below_y and free",
            ));
        }
        let file = project_path(project, path)?;
        let written = set_meta_fracture(&file, Some(&self.settings))
            .map_err(|e| PanelError::new("fracture_panel_meta", e))?;
        let name = written.display().to_string();
        self.last_written = vec![name.clone()];
        Ok(name)
    }
}

fn suffixed(stem: &Path, suffix: &str) -> PathBuf {
    let mut s = stem.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// `rel` under the project, refusing anything that climbs out of it.
fn project_path(project: Option<&Path>, rel: &str) -> Result<PathBuf, PanelError> {
    let project =
        project.ok_or_else(|| PanelError::new("fracture_panel_no_project", "no project is open"))?;
    let path = Path::new(rel);
    if rel.is_empty() || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(PanelError::new(
            "fracture_panel_bad_path",
            format!("`{rel}` must be a plain path relative to the project"),
        ));
    }
    Ok(project.join(path))
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
fn write_atomic(path: &Path, text: &str) -> Result<(), PanelError> {
    let io = |e: std::io::Error| PanelError::new("fracture_panel_io", format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = suffixed(path, &format!(".tmp{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

fn run(
    source: &PanelSource,
    mesh: Option<&Path>,
    settings: &FractureCookSettings,
    sites: Option<Vec<[f32; 3]>>,
    impact: Option<ImpactSites>,
    clustered: Option<ClusterSites>,
) -> Result<FractureSet, PanelError> {
    let mut fracture = fracture_settings(settings);
    if let Some(sites) = sites {
        fracture.sites = regia_asset_pipeline::fracture::FractureSites::Explicit(sites);
    } else if let Some(impact) = impact {
        fracture.sites = regia_asset_pipeline::fracture::FractureSites::Impact {
            count: settings.chunks,
            seed: settings.seed,
            at: impact.at,
            bias: impact.bias,
        };
    } else if let Some(clustered) = clustered {
        fracture.sites = regia_asset_pipeline::fracture::FractureSites::Clusters {
            count: settings.chunks,
            seed: settings.seed,
            clusters: clustered.clusters,
            spread: clustered.spread,
        };
    }
    match (source, mesh) {
        (PanelSource::Mesh { .. }, Some(file)) => {
            let bytes = std::fs::read(file)
                .map_err(|e| PanelError::new("fracture_panel_io", format!("{}: {e}", file.display())))?;
            let mesh = regia_asset_pipeline::import_gltf(&bytes, regia_asset_pipeline::Forward::Convert)
                .map_err(|e| PanelError::new("fracture_panel_io", format!("{}: {e}", file.display())))?;
            let source = FractureSource {
                positions: &mesh.positions,
                indices: &mesh.indices,
                normals: (mesh.normals.len() == mesh.positions.len()).then_some(mesh.normals.as_slice()),
                uvs: (mesh.uvs.len() == mesh.positions.len()).then_some(mesh.uvs.as_slice()),
            };
            Ok(fracture_source(&source, &fracture)?)
        }
        (PanelSource::Box { half }, _) => {
            let (positions, indices) = box_mesh(*half)?;
            let source = FractureSource {
                positions: &positions,
                indices: &indices,
                normals: None,
                uvs: None,
            };
            Ok(fracture_source(&source, &fracture)?)
        }
        (PanelSource::Mesh { .. }, None) => {
            Err(PanelError::new("fracture_panel_no_project", "no project is open"))
        }
    }
}

/// What the status block and `editor.fracture_panel_get` report about a set.
fn summary(set: &FractureSet) -> Value {
    let total = set.total_chunk_volume();
    let source = f64::from(set.source_volume);
    let smallest = set.chunks.iter().map(|c| c.volume).fold(f32::INFINITY, f32::min);
    let largest = set.chunks.iter().map(|c| c.volume).fold(0.0f32, f32::max);
    json!({
        "chunks": set.chunks.len(),
        "joints": set.joints.len(),
        "source_volume": set.source_volume,
        "volume_error": if source > 0.0 { (total - source).abs() / source } else { 0.0 },
        "smallest_chunk": smallest,
        "largest_chunk": largest,
        "triangles": set.chunks.iter().map(FractureChunk::triangle_count).sum::<usize>(),
        "render_vertices": set.chunks.iter().map(|c| c.render.positions.len()).sum::<usize>(),
    })
}

/// Edges of the previewed chunks, pulled apart by `explode` around the set's middle. Empty while
/// the gizmo is off or there is no result.
#[must_use]
pub fn fracture_gizmo_segments(world: &World) -> Vec<Segment> {
    let Some(panel) = world.get_resource::<FracturePanel>() else {
        return Vec::new();
    };
    let mut out = match (&panel.result, panel.show_gizmo) {
        (Some(set), true) => {
            let mut segments = chunk_segments(set, panel.explode, Vec3::from_array(panel.origin));
            // Support, detached and collapsed chunks take their colors (one entry per chunk).
            let colors = crate::fracture_tools::chunk_colors(panel);
            let mut seen = 0usize;
            for (n, c) in set.chunks.iter().enumerate() {
                let edges = chunk_edge_count(c);
                for seg in segments.iter_mut().skip(seen).take(edges) {
                    seg.2 = colors.get(n).copied().unwrap_or(FRACTURE_EDGES);
                }
                seen += edges;
            }
            segments
        }
        _ => Vec::new(),
    };
    out.extend(crate::fracture_tools::bond_segments(panel));
    out.extend(crate::fracture_tools::debris_segments(panel));
    out
}

/// The distinct edges of one chunk (what [`chunk_segments`] draws for it).
fn chunk_edge_count(chunk: &FractureChunk) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for tri in chunk.indices.as_chunks::<3>().0 {
        for k in 0..3 {
            let (a, b) = (tri[k], tri[(k + 1) % 3]);
            if chunk.positions.get(a as usize).is_some() && chunk.positions.get(b as usize).is_some() {
                seen.insert((a.min(b), a.max(b)));
            }
        }
    }
    seen.len()
}

/// The edges of `set`, each once per chunk.
#[must_use]
pub fn chunk_segments(set: &FractureSet, explode: f32, origin: Vec3) -> Vec<Segment> {
    let middle = set
        .chunks
        .iter()
        .map(|c| Vec3::from_array(c.centroid))
        .fold(Vec3::ZERO, |a, b| a + b)
        / set.chunks.len().max(1) as f32;
    let mut out = Vec::new();
    for chunk in &set.chunks {
        let shift = origin + (Vec3::from_array(chunk.centroid) - middle) * explode;
        let mut seen = std::collections::BTreeSet::new();
        for tri in chunk.indices.as_chunks::<3>().0 {
            for k in 0..3 {
                let (a, b) = (tri[k], tri[(k + 1) % 3]);
                if !seen.insert((a.min(b), a.max(b))) {
                    continue;
                }
                let (Some(pa), Some(pb)) = (chunk.positions.get(a as usize), chunk.positions.get(b as usize))
                else {
                    continue;
                };
                if out.len() == MAX_GIZMO_SEGMENTS {
                    return out;
                }
                out.push((
                    Vec3::from_array(*pa) + shift,
                    Vec3::from_array(*pb) + shift,
                    FRACTURE_EDGES,
                ));
            }
        }
    }
    out
}

/// Draws the panel.
pub fn fracture_panel(ui: &mut egui::Ui, world: &mut World) {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<FracturePanel>().unwrap_or_default();
    panel.poll();
    let place = draw(ui, &mut panel, project.as_deref());
    world.insert_resource(panel);
    if place && let Err(e) = crate::fracture_tools::place(world, None) {
        world.resource_mut::<FracturePanel>().error = Some(e);
    }
}

/// Draws the panel; returns whether **Place** was clicked.
fn draw(ui: &mut egui::Ui, panel: &mut FracturePanel, project: Option<&Path>) -> bool {
    let before = (panel.settings.clone(), panel.source.clone(), panel.origin);
    ui.horizontal(|ui| {
        ui.strong("Fracture");
        ui.weak("pre-fractured chunks · bonds · cook");
    });
    ui.separator();
    let running = panel.is_running();
    ui.add_enabled_ui(!running, |ui| {
        let is_mesh = matches!(panel.source, PanelSource::Mesh { .. });
        ui.horizontal(|ui| {
            if ui.radio(!is_mesh, "Box").clicked() && is_mesh {
                panel.source = PanelSource::default();
            }
            if ui.radio(is_mesh, "Project mesh").clicked() && !is_mesh {
                panel.source = PanelSource::Mesh { path: String::new() };
            }
        });
        match &mut panel.source {
            PanelSource::Box { half } => {
                ui.horizontal(|ui| {
                    ui.label("Half extents");
                    // One label for three fields: each DragValue is named for its axis (FU-8).
                    for (axis, h) in ["x", "y", "z"].into_iter().zip(half) {
                        let response = ui.add(egui::DragValue::new(h).speed(0.01).range(0.001..=1000.0));
                        crate::a11y_audit::named_drag_value(
                            response,
                            &format!("Half extent {axis}"),
                            f64::from(*h),
                        );
                    }
                });
            }
            PanelSource::Mesh { path } => {
                ui.horizontal(|ui| {
                    ui.label("Mesh");
                    ui.text_edit_singleline(path);
                });
            }
        }
        let s = &mut panel.settings;
        egui::Grid::new("fracture-settings")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Chunks");
                ui.add(
                    egui::DragValue::new(&mut s.chunks).range(1..=regia_asset_pipeline::fracture::MAX_CHUNKS),
                );
                ui.end_row();
                ui.label("Seed");
                ui.add(egui::DragValue::new(&mut s.seed));
                ui.end_row();
                ui.label("Strength (N/m²)");
                ui.add(
                    egui::DragValue::new(&mut s.strength_per_area)
                        .speed(1000.0)
                        .range(0.0..=1e9),
                );
                ui.end_row();
                ui.label("Density (kg/m³)");
                ui.add(
                    egui::DragValue::new(&mut s.density)
                        .speed(10.0)
                        .range(1.0..=100_000.0),
                );
                ui.end_row();
                ui.label("Bond seconds");
                ui.add(
                    egui::DragValue::new(&mut s.bond_seconds)
                        .speed(0.001)
                        .range(0.0001..=10.0),
                );
                ui.end_row();
                ui.label("Cut-face UV scale");
                ui.add(
                    egui::DragValue::new(&mut s.interior_uv_scale)
                        .speed(0.05)
                        .range(0.01..=100.0),
                );
                ui.end_row();
                ui.label("Effect tag");
                ui.text_edit_singleline(&mut s.fx_tag);
                ui.end_row();
                ui.label("Free object");
                ui.checkbox(&mut s.free, "no support chunks");
                ui.end_row();
            });
        if let PanelSource::Box { half } = panel.source {
            ui.horizontal(|ui| {
                let mut grid = panel.sites.is_some();
                if ui
                    .checkbox(&mut grid, "Grid sites")
                    .on_hover_text("cut the box into a regular grid of cells instead of seeded sites")
                    .changed()
                {
                    panel.sites = None;
                    panel.impact = None;
                    panel.clustered = None;
                    if grid {
                        let n = (panel.settings.chunks as f32).cbrt().round().max(1.0) as u32;
                        panel.sites = crate::fracture_tools::grid_sites(half, [n, n, n]).ok();
                    }
                }
                if let Some(sites) = &panel.sites {
                    ui.weak(format!("{} sites", sites.len()));
                }
            });
        }
        ui.horizontal(|ui| {
            let mut impact = panel.impact.is_some();
            if ui
                .checkbox(&mut impact, "Impact sites")
                .on_hover_text(
                    "bias the site sampling toward a hit: fine debris there, larger chunks away (ADR-9870)",
                )
                .changed()
            {
                panel.sites = None;
                panel.clustered = None;
                panel.impact = impact.then(ImpactSites::default);
            }
            if let Some(im) = &mut panel.impact {
                ui.label("at");
                for v in &mut im.at {
                    ui.add(egui::DragValue::new(v).speed(0.05).range(-1000.0..=1000.0));
                }
                let mut bias = im.bias;
                if ui
                    .add(egui::DragValue::new(&mut bias).speed(1.0).range(0.0..=2.0))
                    .changed()
                {
                    im.bias = bias.round().clamp(0.0, 2.0);
                }
            }
        });
        ui.horizontal(|ui| {
            let mut clustered = panel.clustered.is_some();
            if ui
                .checkbox(&mut clustered, "Clustered sites")
                .on_hover_text("clumps of finer chunks between larger ones (ADR-9870)")
                .changed()
            {
                panel.sites = None;
                panel.impact = None;
                panel.clustered = clustered.then(ClusterSites::default);
            }
            // The chunk field above may have dropped under the clump count this frame.
            panel.clamp_clusters();
            let chunks = panel.settings.chunks.max(1);
            if let Some(cl) = &mut panel.clustered {
                ui.add(egui::DragValue::new(&mut cl.clusters).range(1..=chunks));
                ui.add(
                    egui::DragValue::new(&mut cl.spread)
                        .speed(0.01)
                        .range(regia_asset_pipeline::fracture::MIN_CLUSTER_SPREAD..=1.0),
                )
                .on_hover_text("radius of one clump, a fraction of the source's half-diagonal");
            }
        });
    });
    if before != (panel.settings.clone(), panel.source.clone(), panel.origin) {
        panel.test = None;
    }
    ui.horizontal(|ui| {
        if ui.add_enabled(!running, egui::Button::new("Preview")).clicked()
            && let Err(e) = panel.start(project)
        {
            panel.error = Some(e);
        }
        if running {
            ui.spinner();
        }
        let saved = panel.result.is_some() && !running;
        if ui.add_enabled(saved, egui::Button::new("Save")).clicked() {
            let out = default_out(&panel.source);
            if let Err(e) = panel.save(project, &out) {
                panel.error = Some(e);
            }
        }
        let mesh = matches!(panel.source, PanelSource::Mesh { .. });
        if ui
            .add_enabled(mesh && !running, egui::Button::new("Apply to .meta"))
            .clicked()
            && let Err(e) = panel.apply_meta(project)
        {
            panel.error = Some(e);
        }
    });
    ui.separator();
    if let Some(e) = &panel.error {
        ui.colored_label(
            egui::Color32::from_rgb(230, 90, 90),
            crate::data_panels::sentence(&e.message),
        )
        .on_hover_text(format!("Agent error code: {}", e.code));
    }
    if let Some(set) = &panel.result {
        let s = summary(set);
        ui.label(format!(
            "{} chunks, {} joints, {} triangles",
            s["chunks"], s["joints"], s["triangles"]
        ));
        ui.label(format!(
            "volume {:.3} m³, error {:.1e}",
            set.source_volume,
            s["volume_error"].as_f64().unwrap_or(0.0)
        ));
        ui.checkbox(&mut panel.show_gizmo, "Show in the viewport");
        ui.add(egui::Slider::new(&mut panel.explode, 0.0..=2.0).text("Explode"));
    }
    for line in &panel.last_written {
        ui.weak(format!("wrote {line}"));
    }
    if panel.result.is_some() {
        return crate::fracture_tools::draw_tools(ui, panel);
    }
    false
}

/// Where **Save** writes by default: next to the mesh, or `assets/fracture`.
fn default_out(source: &PanelSource) -> String {
    match source {
        PanelSource::Mesh { path } => Path::new(path)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/"),
        PanelSource::Box { .. } => "assets/fracture".to_string(),
    }
}

fn get_uint(params: &Value, key: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn get_number(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite())
            .map(|x| Some(x as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a finite number"))),
    }
}

fn state(panel: &FracturePanel) -> Value {
    let s = &panel.settings;
    json!({
        "source": match &panel.source {
            PanelSource::Box { half } => json!({ "box": half }),
            PanelSource::Mesh { path } => json!({ "mesh": path }),
        },
        "settings": {
            "chunks": s.chunks,
            "seed": s.seed,
            "strength_per_area": s.strength_per_area,
            "density": s.density,
            "bond_seconds": s.bond_seconds,
            "interior_uv_scale": s.interior_uv_scale,
            "fx_tag": s.fx_tag,
            "free": s.free,
            "support_below_y": s.support_below_y,
        },
        "explode": panel.explode,
        "show_gizmo": panel.show_gizmo,
        "origin": panel.origin,
        "running": panel.is_running(),
        "result": panel.result.as_ref().map(summary),
        "error": panel.error.as_ref().map(|e| json!({ "code": e.code, "message": e.message })),
        "last_written": panel.last_written,
        "sites": panel.sites.as_ref().map(Vec::len),
        "impact": panel.impact.as_ref().map(|i| json!({ "at": i.at, "bias": i.bias })),
        "clustered": panel
            .clustered
            .as_ref()
            .map(|c| json!({ "clusters": c.clusters, "spread": c.spread })),
        "supports": crate::fracture_tools::supports_state(panel),
        "tools": crate::fracture_tools::tools_state(panel),
    })
}

/// Keys of `editor.fracture_panel_set` that change only the view, so the test break stays.
const VIEW_KEYS: [&str; 6] = [
    "explode",
    "show_gizmo",
    "click",
    "hit_impulse",
    "hit_radius",
    "show_bonds",
];

fn with_panel<T>(world: &mut World, f: impl FnOnce(&mut FracturePanel, Option<&Path>) -> T) -> T {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<FracturePanel>().unwrap_or_default();
    let out = f(&mut panel, project.as_deref());
    world.insert_resource(panel);
    out
}

/// `editor.fracture_panel_get`.
fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(with_panel(world, |panel, _| {
        panel.poll();
        state(panel)
    }))
}

/// `editor.fracture_panel_set`: any of the panel's fields; the rest stay.
fn panel_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, _| {
        // A refused call changes nothing (ADR-7550): every key is applied in order below, so the
        // panel's settable state is put back when a later key is refused.
        let before = SetSnapshot::of(panel);
        let out = apply_set(panel, params);
        if out.is_err() {
            before.restore(panel);
        }
        out
    })
}

/// Everything `editor.fracture_panel_set` may change, for putting it back on a refusal.
struct SetSnapshot {
    source: PanelSource,
    settings: FractureCookSettings,
    explode: f32,
    show_gizmo: bool,
    origin: [f32; 3],
    sites: Option<Vec<[f32; 3]>>,
    impact: Option<ImpactSites>,
    clustered: Option<ClusterSites>,
    supports: crate::fracture_tools::SupportPick,
    click: crate::fracture_tools::ClickMode,
    hit_impulse: f32,
    hit_radius: f32,
    show_bonds: bool,
}

impl SetSnapshot {
    fn of(p: &FracturePanel) -> Self {
        Self {
            source: p.source.clone(),
            settings: p.settings.clone(),
            explode: p.explode,
            show_gizmo: p.show_gizmo,
            origin: p.origin,
            sites: p.sites.clone(),
            impact: p.impact.clone(),
            clustered: p.clustered.clone(),
            supports: p.supports.clone(),
            click: p.click,
            hit_impulse: p.hit_impulse,
            hit_radius: p.hit_radius,
            show_bonds: p.show_bonds,
        }
    }

    fn restore(self, p: &mut FracturePanel) {
        p.source = self.source;
        p.settings = self.settings;
        p.explode = self.explode;
        p.show_gizmo = self.show_gizmo;
        p.origin = self.origin;
        p.sites = self.sites;
        p.impact = self.impact;
        p.clustered = self.clustered;
        p.supports = self.supports;
        p.click = self.click;
        p.hit_impulse = self.hit_impulse;
        p.hit_radius = self.hit_radius;
        p.show_bonds = self.show_bonds;
    }
}

/// The body of [`panel_set`]: applies `params` in order (the caller restores on a refusal).
fn apply_set(panel: &mut FracturePanel, params: &Value) -> Result<Value, RpcError> {
    match (params.get("box"), params.get("mesh")) {
        (Some(b), None) => {
            let half: Option<Vec<f32>> = b
                .as_array()
                .filter(|a| a.len() == 3)
                .and_then(|a| a.iter().map(|v| v.as_f64().map(|x| x as f32)).collect());
            let Some(half) = half else {
                return Err(RpcError::invalid_params("`box` must be [x, y, z] half extents"));
            };
            panel.source = PanelSource::Box {
                half: [half[0], half[1], half[2]],
            };
        }
        (None, Some(m)) => {
            let Some(path) = m.as_str() else {
                return Err(RpcError::invalid_params("`mesh` must be a project path"));
            };
            panel.source = PanelSource::Mesh {
                path: path.to_string(),
            };
        }
        (None, None) => {}
        _ => return Err(RpcError::invalid_params("give `box` or `mesh`, not both")),
    }
    let s = &mut panel.settings;
    if let Some(v) = get_uint(params, "chunks")? {
        s.chunks = v as usize;
    }
    if let Some(v) = get_uint(params, "seed")? {
        s.seed = v;
    }
    for (key, slot) in [
        ("strength_per_area", &mut s.strength_per_area),
        ("density", &mut s.density),
        ("bond_seconds", &mut s.bond_seconds),
        ("interior_uv_scale", &mut s.interior_uv_scale),
    ] {
        if let Some(v) = get_number(params, key)? {
            *slot = v;
        }
    }
    if let Some(tag) = params.get("fx_tag").and_then(Value::as_str) {
        s.fx_tag = tag.to_string();
    }
    if let Some(free) = params.get("free").and_then(Value::as_bool) {
        s.free = free;
    }
    if let Some(v) = get_number(params, "explode")? {
        panel.explode = v.clamp(0.0, 8.0);
    }
    if let Some(v) = params.get("show_gizmo").and_then(Value::as_bool) {
        panel.show_gizmo = v;
    }
    if let Some(v) = params.get("origin") {
        let origin: Option<Vec<f32>> = v
            .as_array()
            .filter(|a| a.len() == 3)
            .and_then(|a| a.iter().map(|x| x.as_f64().map(|x| x as f32)).collect());
        let Some(origin) = origin else {
            return Err(RpcError::invalid_params("`origin` must be [x, y, z]"));
        };
        panel.origin = [origin[0], origin[1], origin[2]];
    }
    crate::fracture_tools::set_tools(panel, params)?;
    panel.clamp_clusters();
    if params
        .as_object()
        .is_some_and(|o| o.keys().any(|k| !VIEW_KEYS.contains(&k.as_str())))
    {
        panel.test = None;
    }
    Ok(state(panel))
}

/// `editor.fracture_panel_run { wait? }`.
fn panel_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, project| {
        panel.start(project).map_err(|e| e.rpc())?;
        if params.get("wait").and_then(Value::as_bool) == Some(true) {
            panel.wait();
            if let Some(e) = &panel.error {
                return Err(e.rpc());
            }
        }
        Ok(state(panel))
    })
}

/// `editor.fracture_panel_save { out }`.
fn panel_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(out) = params.get("out").and_then(Value::as_str).map(str::to_string) else {
        return Err(RpcError::invalid_params(
            "`out` must be a project path without suffixes",
        ));
    };
    with_panel(world, |panel, project| {
        panel.poll();
        let written = panel.save(project, &out).map_err(|e| e.rpc())?;
        Ok(json!({ "written": written }))
    })
}

/// `editor.fracture_panel_apply_meta`.
fn panel_apply_meta(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, project| {
        let meta = panel.apply_meta(project).map_err(|e| e.rpc())?;
        Ok(json!({ "meta": meta }))
    })
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const SET_PARAMS: &[ParamSchema] = &[
    param("box", "array", "[x, y, z] half extents to fracture a box."),
    param(
        "mesh",
        "string",
        "A closed .glb or .gltf, relative to the project.",
    ),
    param("chunks", "integer", "Chunks to make, 1 to 1024."),
    param("seed", "integer", "Site sampling seed."),
    param(
        "strength_per_area",
        "number",
        "Newtons of break force per square metre of contact.",
    ),
    param("density", "number", "Kilograms per cubic metre."),
    param(
        "bond_seconds",
        "number",
        "Seconds a joint's break force must act.",
    ),
    param(
        "interior_uv_scale",
        "number",
        "UV units per metre on the cut faces.",
    ),
    param("fx_tag", "string", "Sound/VFX tag of the material."),
    param("free", "boolean", "No support chunks (a free object)."),
    param(
        "explode",
        "number",
        "How far the preview pulls chunks apart, 0 to 8.",
    ),
    param("show_gizmo", "boolean", "Draw the chunks in the viewport."),
    param("origin", "array", "[x, y, z] where the viewport draws the set."),
    param(
        "supports",
        "any",
        "\"rule\" (the settings' rule), \"none\", [chunk, ...] (picked), { below_y } (stored in \
         .meta; null clears the plane) or { box: [[min], [max]] }.",
    ),
    param(
        "sites",
        "array",
        "[[x, y, z], ...] explicit Voronoi sites instead of the seeded sampler; null clears them.",
    ),
    param(
        "grid",
        "array",
        "[x, y, z] cell counts: grid sites filling the box source (a 3x3x1 wall).",
    ),
    param(
        "impact",
        "object",
        "{ at: [x, y, z], bias: 0|1|2 } impact-biased sites: fine debris at the hit (ADR-9870); \
         null clears it.",
    ),
    param(
        "clustered",
        "object",
        "{ clusters: n, spread: s } clustered sites: n clumps of finer chunks (spread is a \
         fraction of the source's half-diagonal); null clears it.",
    ),
    param(
        "click",
        "string",
        "What a viewport click does: off, supports or test_hit.",
    ),
    param("hit_impulse", "number", "Impulse of a click hit, newton-seconds."),
    param("hit_radius", "number", "Radius of a click hit, metres."),
    param(
        "show_bonds",
        "boolean",
        "Draw the bonds colored by break impulse.",
    ),
];

const RUN_PARAMS: &[ParamSchema] = &[param("wait", "boolean", "Block until the run ends.")];

const SAVE_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "out",
    kind: "string",
    required: true,
    description: "Path relative to the project, without suffixes.",
}];

const STATE: &str = "{ source, settings, explode, show_gizmo, origin, running, result: { chunks, joints, source_volume, \
    volume_error, smallest_chunk, largest_chunk, triangles, render_vertices }, error: { code, message }, last_written, \
    sites, impact, clustered, supports: { pick, chunks }, tools: { click, hit_impulse, hit_radius, show_bonds, \
    hover_bond, test, saved } }";

/// The Fracture panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.fracture_panel_get",
                tool: "fracture_panel_get",
                description: "The Fracture panel: what it fractures, its settings, the state of a run and the \
                    last result.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_panel_set",
                tool: "fracture_panel_set",
                description: "Change the Fracture panel's source, settings or preview; fields left out stay.",
                params: SET_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_panel_run",
                tool: "fracture_panel_run",
                description: "Fracture the panel's source on a worker thread, as the Preview button does. Errors \
                    carry data.reason (fracture_*, book/agents/fracture.md).",
                params: RUN_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_run as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_panel_save",
                tool: "fracture_panel_save",
                description: "Write the last result as <out>.fracture.ron and <out>.destructible.ron under the \
                    project.",
                params: SAVE_PARAMS,
                result: "{ written: [path] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_panel_apply_meta",
                tool: "fracture_panel_apply_meta",
                description: "Store the panel's settings in the source mesh's .meta, so regia cook makes the \
                    chunks.",
                params: &[],
                result: "{ meta }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_apply_meta as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_project(dir: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.to_path_buf()),
        });
        world
    }

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    fn call(world: &mut World, f: Handler, params: Value) -> Result<Value, RpcError> {
        f(world, &params)
    }

    /// Runs the panel for `frames` frames on a headless egui context.
    fn draw_frames(world: &mut World, frames: usize) -> Vec<egui::ClippedPrimitive> {
        let ctx = egui::Context::default();
        let mut shapes = Vec::new();
        for _ in 0..frames {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(480.0, 640.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                fracture_panel(ui, world);
            });
            output.textures_delta.clear();
            shapes = ctx.tessellate(output.shapes, output.pixels_per_point);
        }
        shapes
    }

    #[test]
    fn the_panel_draws_headless_in_every_state() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-draw");
        let mut world = world_with_project(&dir);
        // First frame, no state yet.
        assert!(!draw_frames(&mut world, 2).is_empty());
        // Mesh source, running, finished, failed.
        call(&mut world, panel_set, json!({ "mesh": "assets/none.glb" })).expect("set");
        draw_frames(&mut world, 1);
        call(
            &mut world,
            panel_set,
            json!({ "box": [1.0, 1.0, 1.0], "chunks": 6, "seed": 2 }),
        )
        .expect("set");
        call(&mut world, panel_run, json!({})).expect("run");
        draw_frames(&mut world, 1);
        world.resource_mut::<FracturePanel>().wait();
        assert!(!draw_frames(&mut world, 1).is_empty());
        assert_eq!(
            world
                .resource::<FracturePanel>()
                .result
                .as_ref()
                .map(|s| s.chunks.len()),
            Some(6)
        );
        call(&mut world, panel_set, json!({ "chunks": 0 })).expect("set");
        call(&mut world, panel_run, json!({})).expect("run");
        world.resource_mut::<FracturePanel>().wait();
        draw_frames(&mut world, 1);
        assert_eq!(
            world
                .resource::<FracturePanel>()
                .error
                .as_ref()
                .map(|e| e.code.as_str()),
            Some("fracture_invalid_settings")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_reports_the_set_and_the_viewport_draws_it_pulled_apart() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-run");
        let mut world = world_with_project(&dir);
        call(
            &mut world,
            panel_set,
            json!({ "box": [1.0, 0.5, 0.25], "chunks": 8, "seed": 11 }),
        )
        .expect("set");
        assert!(fracture_gizmo_segments(&world).is_empty(), "nothing to draw yet");
        let v = call(&mut world, panel_run, json!({ "wait": true })).expect("run");
        assert_eq!(v["running"], false);
        assert_eq!(v["result"]["chunks"], 8);
        assert!(v["result"]["joints"].as_u64().is_some_and(|n| n > 0));
        assert!(v["result"]["volume_error"].as_f64().is_some_and(|e| e < 1e-4));
        assert!(v["result"]["render_vertices"].as_u64().is_some_and(|n| n > 0));
        let solid = fracture_gizmo_segments(&world);
        assert!(!solid.is_empty());
        call(
            &mut world,
            panel_set,
            json!({ "explode": 1.0, "origin": [0.0, 5.0, 0.0] }),
        )
        .expect("set");
        let apart = fracture_gizmo_segments(&world);
        assert_eq!(apart.len(), solid.len());
        assert_ne!(apart[0].0, solid[0].0);
        call(&mut world, panel_set, json!({ "show_gizmo": false })).expect("set");
        assert!(fracture_gizmo_segments(&world).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_a_set_and_an_asset_that_load() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-save");
        let mut world = world_with_project(&dir);
        let e = call(&mut world, panel_save, json!({ "out": "assets/wall" })).expect_err("no result");
        assert_eq!(reason(&e).as_deref(), Some("fracture_panel_no_result"));
        call(
            &mut world,
            panel_set,
            json!({ "box": [1.0, 1.0, 0.25], "chunks": 5 }),
        )
        .expect("set");
        call(&mut world, panel_run, json!({ "wait": true })).expect("run");
        let v = call(&mut world, panel_save, json!({ "out": "assets/wall" })).expect("save");
        assert_eq!(v["written"][0], "assets/wall.fracture.ron");
        let set = std::fs::read_to_string(dir.join("assets/wall.fracture.ron")).expect("set file");
        assert_eq!(FractureSet::from_ron(&set).expect("set").chunks.len(), 5);
        let asset = std::fs::read_to_string(dir.join("assets/wall.destructible.ron")).expect("asset file");
        let asset = regia::physics::destruction::DestructibleAsset::from_ron(&asset).expect("asset");
        assert_eq!(asset.meshes.as_deref(), Some("wall.fracture.ron"));
        assert_eq!(asset.chunks.len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_stay_inside_the_project() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-paths");
        let mut world = world_with_project(&dir);
        call(&mut world, panel_set, json!({ "chunks": 2 })).expect("set");
        call(&mut world, panel_run, json!({ "wait": true })).expect("run");
        for out in ["../outside", "/etc/outside", "a/../../b", ""] {
            let e = call(&mut world, panel_save, json!({ "out": out })).expect_err("refused");
            assert_eq!(reason(&e).as_deref(), Some("fracture_panel_bad_path"), "{out}");
        }
        call(&mut world, panel_set, json!({ "mesh": "../x.glb" })).expect("set");
        let e = call(&mut world, panel_run, json!({})).expect_err("refused");
        assert_eq!(reason(&e).as_deref(), Some("fracture_panel_bad_path"));
        let mut empty = World::new();
        call(&mut empty, panel_set, json!({ "mesh": "a.glb" })).expect("set");
        let e = call(&mut empty, panel_run, json!({})).expect_err("no project");
        assert_eq!(reason(&e).as_deref(), Some("fracture_panel_no_project"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_to_meta_stores_the_settings_and_keeps_the_others() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-meta");
        let mut world = world_with_project(&dir);
        let e = call(&mut world, panel_apply_meta, json!({})).expect_err("a box has no meta");
        assert_eq!(reason(&e).as_deref(), Some("fracture_panel_no_mesh"));
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let glb = dir.join("assets/wall.glb");
        std::fs::write(&glb, b"not read here").expect("file");
        let meta_path = regia_asset::meta_path_for(&glb);
        std::fs::write(
            &meta_path,
            "(format_version: 1, id: \"11111111-2222-4333-8444-555555555555\", importer: \"gltf\", settings: Some(\"(meshlets: true)\"))",
        )
        .expect("meta");
        call(
            &mut world,
            panel_set,
            json!({ "mesh": "assets/wall.glb", "chunks": 7, "seed": 4 }),
        )
        .expect("set");
        call(&mut world, panel_apply_meta, json!({})).expect("apply");
        let meta =
            regia_asset::MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("read")).expect("meta");
        let settings = regia_asset_pipeline::GltfImporterSettings::from_meta_string(meta.settings.as_deref());
        assert!(settings.meshlets, "the other settings stay");
        let f = settings.fracture.expect("fracture block");
        assert_eq!((f.chunks, f.seed), (7, 4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_run_waits_for_the_first() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-busy");
        let mut panel = FracturePanel::default();
        panel.settings.chunks = 4;
        panel.start(Some(&dir)).expect("first");
        let second = panel.start(Some(&dir)).map_err(|e| e.code);
        assert_eq!(second, Err("fracture_panel_busy".to_string()));
        panel.wait();
        assert!(!panel.is_running());
        assert!(panel.result.is_some());
    }

    #[test]
    fn every_method_answers_by_its_registered_name() {
        let list = methods();
        let by_name = |name: &str| {
            list.iter()
                .find(|(m, _)| m.name == name)
                .map(|(m, h)| (m.mutating, *h))
                .expect("registered")
        };
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-names");
        let mut world = world_with_project(&dir);
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        std::fs::write(dir.join("assets/m.glb"), b"not read here").expect("file");
        let (mutating, get) = by_name("editor.fracture_panel_get");
        assert!(!mutating);
        assert_eq!(get(&mut world, &json!({})).expect("get")["running"], false);
        let (mutating, set) = by_name("editor.fracture_panel_set");
        assert!(mutating);
        set(&mut world, &json!({ "chunks": 3 })).expect("set");
        let (mutating, run) = by_name("editor.fracture_panel_run");
        assert!(mutating);
        run(&mut world, &json!({ "wait": true })).expect("run");
        let (mutating, save) = by_name("editor.fracture_panel_save");
        assert!(mutating);
        save(&mut world, &json!({ "out": "assets/named" })).expect("save");
        assert!(dir.join("assets/named.destructible.ron").is_file());
        set(&mut world, &json!({ "mesh": "assets/m.glb" })).expect("set");
        let (mutating, apply) = by_name("editor.fracture_panel_apply_meta");
        assert!(mutating);
        apply(&mut world, &json!({})).expect("apply");
        assert!(regia_asset::meta_path_for(&dir.join("assets/m.glb")).is_file());
        assert_eq!(list.len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_panel_draws_with_impact_and_clustered_sites() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-sites");
        let mut world = world_with_project(&dir);
        call(
            &mut world,
            panel_set,
            json!({ "box": [1.0, 1.0, 1.0], "chunks": 6, "impact": { "at": [1.0, 0.0, 0.0], "bias": 1.0 } }),
        )
        .expect("set");
        assert!(!draw_frames(&mut world, 1).is_empty());
        let state = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(state["impact"]["bias"], 1.0, "{state}");
        call(
            &mut world,
            panel_set,
            json!({ "clustered": { "clusters": 2, "spread": 0.4 } }),
        )
        .expect("set");
        assert!(!draw_frames(&mut world, 1).is_empty());
        let state = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(state["impact"], json!(null), "{state}");
        assert_eq!(state["clustered"]["clusters"], 2, "{state}");
        // Grid replaces any site choice, exactly as the panel's Grid checkbox does.
        call(&mut world, panel_set, json!({ "impact": { "bias": 1.0 } })).expect("set");
        call(&mut world, panel_set, json!({ "grid": [2, 2, 1] })).expect("set");
        let state = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(state["sites"], json!(4), "{state}");
        assert_eq!(state["impact"], json!(null), "grid clears impact: {state}");
        assert_eq!(state["clustered"], json!(null), "grid clears clustered: {state}");
        call(&mut world, panel_run, json!({ "wait": true })).expect("run");
        assert!(!draw_frames(&mut world, 1).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refused `editor.fracture_panel_set` leaves the panel as it was (sprint-rev-claude-60 § 4
    /// finding 3): a valid `impact` next to a bad `clustered` used to set the impact and clear the
    /// sites before answering `fracture_panel_bad_clusters`, and two valid site choices in one
    /// call let the last one win silently.
    #[test]
    fn a_refused_set_changes_nothing() {
        let mut world = World::new();
        let before = call(
            &mut world,
            panel_set,
            json!({ "box": [1.0, 1.0, 1.0], "chunks": 8, "sites": [[0.5, 0.0, 0.0], [-0.5, 0.0, 0.0]] }),
        )
        .expect("set");
        // Keys before the refused one (chunks, seed) are not kept.
        let e = call(
            &mut world,
            panel_set,
            json!({ "chunks": 6, "seed": 9, "clustered": { "clusters": 0 } }),
        )
        .expect_err("refused");
        assert_eq!(
            reason(&e).as_deref(),
            Some("fracture_panel_bad_clusters"),
            "{e:?}"
        );
        let after = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(after, before, "a refused set changes nothing");
        // The review's case: a valid impact next to a bad clustered no longer sets the impact.
        call(
            &mut world,
            panel_set,
            json!({ "impact": { "at": [1.0, 0.0, 0.0], "bias": 2 }, "clustered": { "clusters": 0 } }),
        )
        .expect_err("refused");
        let after = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(after, before, "a refused set changes nothing");
        // Two valid site choices in one call: refused, not "the last one wins".
        let e = call(
            &mut world,
            panel_set,
            json!({ "impact": { "at": [1.0, 0.0, 0.0], "bias": 2 },
                    "clustered": { "clusters": 2, "spread": 0.3 } }),
        )
        .expect_err("refused");
        assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{e:?}");
        let after = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(after, before, "a refused set changes nothing");
        // A bad support pick after valid view keys: nothing applies either.
        let e = call(
            &mut world,
            panel_set,
            json!({ "explode": 2.0, "hit_radius": 0.5, "supports": [99] }),
        )
        .expect_err("refused");
        assert_eq!(reason(&e).as_deref(), Some("fracture_panel_bad_chunk"), "{e:?}");
        let after = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(after, before, "a refused set changes nothing");
    }

    /// The clump count follows the chunk count the same way on both surfaces (sprint-rev-claude-60
    /// § 4 finding 4): an explicit `clusters` above `chunks` is refused, and lowering `chunks`
    /// below the panel's clump count clamps it, whether through `editor.fracture_panel_set` or the
    /// panel's chunk field, so a run never fails with `fracture_invalid_settings` for it.
    #[test]
    fn lowering_chunks_clamps_the_clump_count_on_both_surfaces() {
        let dir = regia_test_utils::unique_temp_dir("fracture-panel-clumps");
        let mut world = world_with_project(&dir);
        call(
            &mut world,
            panel_set,
            json!({ "box": [1.0, 1.0, 1.0], "chunks": 12, "clustered": { "clusters": 6, "spread": 0.3 } }),
        )
        .expect("set");
        let state = call(&mut world, panel_set, json!({ "chunks": 4 })).expect("set");
        assert_eq!(
            state["clustered"]["clusters"],
            json!(4),
            "RPC chunks clamp: {state}"
        );
        // The panel's chunk field (a DragValue edit) lowers chunks under the clump count.
        world.resource_mut::<FracturePanel>().settings.chunks = 3;
        assert!(!draw_frames(&mut world, 1).is_empty());
        let state = call(&mut world, panel_get, json!({})).expect("get");
        assert_eq!(
            state["clustered"]["clusters"],
            json!(3),
            "UI chunks clamp: {state}"
        );
        call(&mut world, panel_run, json!({ "wait": true })).expect("the clamped clumps run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A clump spread below `MIN_CLUSTER_SPREAD` cannot separate its sites (sprint-rev-claude-60
    /// § 4 finding 5): the panel refuses it with `fracture_panel_bad_clusters` instead of a run
    /// that always fails with `fracture_site_sampling`.
    #[test]
    fn a_clump_spread_below_the_floor_is_refused() {
        let mut world = World::new();
        for spread in [1e-7, 5e-4] {
            let e = call(
                &mut world,
                panel_set,
                json!({ "box": [1.0, 1.0, 1.0], "chunks": 8,
                        "clustered": { "clusters": 1, "spread": spread } }),
            )
            .expect_err("refused");
            assert_eq!(
                reason(&e).as_deref(),
                Some("fracture_panel_bad_clusters"),
                "{spread}: {e:?}"
            );
        }
        let state = call(
            &mut world,
            panel_set,
            json!({ "clustered": { "clusters": 1, "spread": 1e-3 } }),
        )
        .expect("the floor is accepted");
        assert!(state["clustered"].is_object(), "{state}");
    }

    #[test]
    fn bad_parameters_are_invalid_params() {
        let mut world = World::new();
        for params in [
            json!({ "box": [1.0] }),
            json!({ "box": [1.0, 1.0, 1.0], "mesh": "a" }),
            json!({ "mesh": 3 }),
            json!({ "chunks": -1 }),
            json!({ "density": "x" }),
            json!({ "origin": [1.0] }),
        ] {
            let e = call(&mut world, panel_set, params.clone()).expect_err("refused");
            assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{params}");
        }
        let e = call(&mut world, panel_save, json!({})).expect_err("no out");
        assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS);
    }

    /// FU-8 (`docs/evidence/editor-a11y-2026-11.md`): the box source's "Half extents" row is
    /// one label for three DragValues; each is named for its axis.
    #[test]
    fn half_extent_drag_values_are_named_per_axis() {
        let mut world = World::new();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(480.0, 640.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| fracture_panel(ui, &mut world));
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.expect("accesskit is on");
        let names: Vec<String> = crate::a11y_audit::widget_names(&update)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        for want in ["Half extent x", "Half extent y", "Half extent z"] {
            assert!(names.iter().any(|name| name == want), "{want} in {names:?}");
        }
    }
}
