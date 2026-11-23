// SPDX-License-Identifier: MPL-2.0
//! Budget meters for the kit viewer and prefab mode (`book/agents/editor-budget.md`).
//!
//! A model, or every mesh a scene references, is measured with a [`ModelReport`] and compared
//! with a [`ModelClass`]'s [`BudgetLimits`]: one [`BudgetRow`] per limit with the value, the
//! limit, the share used and a [`BudgetStatus`] (`ok`, `near` from [`NEAR_FRACTION`] of the
//! limit, `over`, `off` for a limit of 0). A kit adds its LOD chain ([`LodRow`]): each level
//! must have fewer triangles than the level above it.
//!
//! [`report_from_file`] reads accessor counts and image headers only, so measuring never
//! decodes a texture; [`ReportCache`] keeps one report per file until the file changes. A
//! scene budget ([`scene_budget`]) sums every `MeshSource` instance: triangles and nodes count
//! once per instance, texture memory once per model file, materials once per distinct
//! material (the model's own materials count when ANY instance uses them; an override counts
//! once per material file).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_render::MeshSource;
use serde::Serialize;
use serde_json::{Value, json};

use crate::model_report::{BudgetLimits, ModelClass, ModelReport};
use crate::viewport_agent::{Handler, str_param};

/// The share of a limit from which a row reads `near`.
pub const NEAR_FRACTION: f64 = 0.8;

/// How a measured value sits against its limit. Ordered from harmless to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStatus {
    /// The limit is 0: the check is off.
    Off,
    /// Below [`NEAR_FRACTION`] of the limit.
    Ok,
    /// At or above [`NEAR_FRACTION`] of the limit, not over it.
    Near,
    /// Over the limit.
    Over,
}

impl BudgetStatus {
    /// The status of `value` against `limit`.
    #[must_use]
    pub fn of(value: u64, limit: u64) -> Self {
        if limit == 0 {
            Self::Off
        } else if value > limit {
            Self::Over
        } else if value as f64 >= limit as f64 * NEAR_FRACTION {
            Self::Near
        } else {
            Self::Ok
        }
    }

    /// `off`, `ok`, `near` or `over`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ok => "ok",
            Self::Near => "near",
            Self::Over => "over",
        }
    }
}

/// One limit of the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetRow {
    /// Stable key (`triangles`, `node_triangles`, `nodes`, `materials`, `materials_per_node`,
    /// `joints_per_skin`, `texture_side`, `texture_bytes`).
    pub key: &'static str,
    /// What the row measures, for the panel.
    pub label: &'static str,
    /// Where the value comes from: `model`, `scene`, or the node, skin or `material/slot` at
    /// the peak.
    pub subject: String,
    /// The measured value.
    pub value: u64,
    /// The limit (0: off).
    pub limit: u64,
    /// The value against the limit.
    pub status: BudgetStatus,
}

impl BudgetRow {
    /// A row with its status.
    #[must_use]
    pub fn new(
        key: &'static str,
        label: &'static str,
        subject: impl Into<String>,
        value: u64,
        limit: u64,
    ) -> Self {
        Self {
            key,
            label,
            subject: subject.into(),
            value,
            limit,
            status: BudgetStatus::of(value, limit),
        }
    }

    /// `value / limit` (`None` when the check is off).
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        (self.limit > 0).then(|| self.value as f64 / self.limit as f64)
    }

    /// The value as the panel shows it (bytes in MiB).
    #[must_use]
    pub fn display_value(&self) -> String {
        format_value(self.key, self.value)
    }

    /// The limit as the panel shows it.
    #[must_use]
    pub fn display_limit(&self) -> String {
        if self.limit == 0 {
            "off".to_string()
        } else {
            format_value(self.key, self.limit)
        }
    }

    /// `{key, label, subject, value, limit, fraction, status}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "key": self.key,
            "label": self.label,
            "subject": self.subject,
            "value": self.value,
            "limit": self.limit,
            "fraction": self.fraction(),
            "status": self.status.name(),
        })
    }
}

fn format_value(key: &str, value: u64) -> String {
    if key == "texture_bytes" {
        format!("{:.1} MiB", value as f64 / (1024.0 * 1024.0))
    } else {
        value.to_string()
    }
}

/// The largest value seen and where it was seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Peak {
    /// The value.
    pub value: u64,
    /// Where (a node, skin or `material/slot` name; empty when nothing was seen).
    pub subject: String,
}

impl Peak {
    fn raise(&mut self, value: u64, subject: impl FnOnce() -> String) {
        if value > self.value {
            self.value = value;
            self.subject = subject();
        }
    }
}

/// What a model (or a scene) measures, before limits apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Measure {
    /// Triangles (LOD0 and non-LOD nodes when LOD levels are left out).
    pub triangles: u64,
    /// Nodes.
    pub nodes: u64,
    /// Materials in use.
    pub materials: u64,
    /// Estimated texture memory with mips, bytes.
    pub texture_bytes: u64,
    /// The node with the most triangles.
    pub node_triangles: Peak,
    /// The node with the most distinct materials.
    pub node_materials: Peak,
    /// The skin with the most joints.
    pub skin_joints: Peak,
    /// The largest texture side in use.
    pub texture_side: Peak,
}

impl Measure {
    /// Measures a report. `lod0_only` leaves out the triangles of `_LOD<n>` nodes with n > 0
    /// (a kit's lower LODs are not drawn with LOD0).
    #[must_use]
    pub fn of_report(report: &ModelReport, lod0_only: bool) -> Self {
        let mut measure = Self {
            nodes: report.totals.nodes,
            materials: report.totals.materials_in_use,
            texture_bytes: report.totals.texture_bytes,
            ..Self::default()
        };
        for node in &report.nodes {
            let drawn = !lod0_only || crate::kit::lod_suffix(&node.name).is_none_or(|level| level == 0);
            if drawn {
                measure.triangles = measure.triangles.saturating_add(node.triangles);
            }
            measure.node_triangles.raise(node.triangles, || node.name.clone());
            measure
                .node_materials
                .raise(node.materials.len() as u64, || node.name.clone());
        }
        for skin in &report.skins {
            measure.skin_joints.raise(skin.joints, || skin.name.clone());
        }
        for map in report.maps.iter().filter(|map| map.used) {
            if let Some((width, height)) = map.size {
                measure.texture_side.raise(u64::from(width.max(height)), || {
                    format!("{}/{}", map.material, map.slot.name())
                });
            }
        }
        measure
    }
}

/// The budget rows of `measure` against `limits`, in a fixed order. `scope` is the subject of
/// the whole-model rows (`model` or `scene`).
#[must_use]
pub fn budget_rows(measure: &Measure, limits: &BudgetLimits, scope: &str) -> Vec<BudgetRow> {
    let m = measure;
    let l = limits;
    vec![
        BudgetRow::new("triangles", "Triangles", scope, m.triangles, l.max_triangles),
        BudgetRow::new(
            "node_triangles",
            "Triangles on one node",
            m.node_triangles.subject.clone(),
            m.node_triangles.value,
            l.max_node_triangles,
        ),
        BudgetRow::new("nodes", "Nodes", scope, m.nodes, l.max_nodes),
        BudgetRow::new("materials", "Materials", scope, m.materials, l.max_materials),
        BudgetRow::new(
            "materials_per_node",
            "Materials on one node",
            m.node_materials.subject.clone(),
            m.node_materials.value,
            l.max_materials_per_node,
        ),
        BudgetRow::new(
            "joints_per_skin",
            "Joints in one skin",
            m.skin_joints.subject.clone(),
            m.skin_joints.value,
            l.max_joints_per_skin,
        ),
        BudgetRow::new(
            "texture_side",
            "Largest texture side",
            m.texture_side.subject.clone(),
            m.texture_side.value,
            l.max_texture_dimension,
        ),
        BudgetRow::new(
            "texture_bytes",
            "Texture memory",
            scope,
            m.texture_bytes,
            l.max_texture_bytes,
        ),
    ]
}

/// One level of a kit's LOD chain.
#[derive(Debug, Clone, PartialEq)]
pub struct LodRow {
    /// The level.
    pub level: usize,
    /// Authored in the file (`_LOD<n>` nodes) rather than generated by the viewer.
    pub authored: bool,
    /// Triangles at this level.
    pub triangles: u64,
    /// Triangles as a share of LOD0 (1 for LOD0; 0 when LOD0 is empty).
    pub ratio: f64,
    /// Fewer triangles than the level above (always true for LOD0).
    pub reduces: bool,
}

impl LodRow {
    /// `{level, authored, triangles, ratio, reduces}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "level": self.level,
            "authored": self.authored,
            "triangles": self.triangles,
            "ratio": self.ratio,
            "reduces": self.reduces,
        })
    }
}

/// The LOD rows of `(level, authored, triangles)` levels in chain order.
#[must_use]
pub fn lod_rows(levels: &[(usize, bool, u64)]) -> Vec<LodRow> {
    let lod0 = levels.first().map_or(0, |&(_, _, triangles)| triangles);
    let mut previous: Option<u64> = None;
    levels
        .iter()
        .map(|&(level, authored, triangles)| {
            let reduces = previous.is_none_or(|above| triangles < above);
            previous = Some(triangles);
            LodRow {
                level,
                authored,
                triangles,
                ratio: if lod0 == 0 {
                    0.0
                } else {
                    triangles as f64 / lod0 as f64
                },
                reduces,
            }
        })
        .collect()
}

/// A model's or a scene's budget.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    /// What was measured: the model file, or `scene`.
    pub source: String,
    /// The class whose limits apply.
    pub class: ModelClass,
    /// The limits applied.
    pub limits: BudgetLimits,
    /// One row per limit.
    pub rows: Vec<BudgetRow>,
    /// The LOD chain (kits; empty for a scene).
    pub lods: Vec<LodRow>,
    /// Distinct model files measured.
    pub models: usize,
    /// Mesh instances measured.
    pub instances: usize,
    /// Model files that could not be measured.
    pub errors: Vec<String>,
}

impl Budget {
    /// A budget of `measure` against `class`'s `limits`, with no LOD chain.
    #[must_use]
    pub fn from_measure(
        source: &str,
        scope: &str,
        class: ModelClass,
        limits: BudgetLimits,
        measure: &Measure,
    ) -> Self {
        Self {
            source: source.to_string(),
            class,
            limits,
            rows: budget_rows(measure, &limits, scope),
            lods: Vec::new(),
            models: 1,
            instances: 1,
            errors: Vec::new(),
        }
    }

    /// The row with `key`.
    #[must_use]
    pub fn row(&self, key: &str) -> Option<&BudgetRow> {
        self.rows.iter().find(|row| row.key == key)
    }

    /// The worst row status; a LOD that does not reduce, or a model that could not be measured,
    /// is `over`.
    #[must_use]
    pub fn worst(&self) -> BudgetStatus {
        let rows = self
            .rows
            .iter()
            .map(|row| row.status)
            .max()
            .unwrap_or(BudgetStatus::Off);
        if self.lods.iter().any(|lod| !lod.reduces) || !self.errors.is_empty() {
            BudgetStatus::Over
        } else {
            rows
        }
    }

    /// Rows over their limit, plus LOD levels that do not reduce.
    #[must_use]
    pub fn over_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == BudgetStatus::Over)
            .count()
            + self.lods.iter().filter(|lod| !lod.reduces).count()
    }

    /// One line for a banner: `vehicle · 2/150000 tris · 1/12 materials · 0.1/192.0 MiB`.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let part = |key: &str, unit: &str| {
            self.row(key).map_or_else(String::new, |row| {
                format!(" · {}/{}{unit}", row.display_value(), row.display_limit())
            })
        };
        format!(
            "{}{}{}{}",
            self.class.name(),
            part("triangles", " tris"),
            part("materials", " materials"),
            part("texture_bytes", ""),
        )
    }

    /// The budget as JSON (`editor.budget`'s `budget`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "class": self.class.name(),
            "limits": serde_json::to_value(self.limits).unwrap_or(Value::Null),
            "status": self.worst().name(),
            "over": self.over_count(),
            "rows": self.rows.iter().map(BudgetRow::to_json).collect::<Vec<_>>(),
            "lods": self.lods.iter().map(LodRow::to_json).collect::<Vec<_>>(),
            "models": self.models,
            "instances": self.instances,
            "errors": self.errors,
        })
    }
}

/// The class a kit is budgeted as: a character kit or a `pilot` is a character, a `hull` a
/// vehicle, a `structure` a building, anything else (`prop`, `weapon`) a prop.
#[must_use]
pub fn kit_class(kind: &str, character_kit: bool) -> ModelClass {
    if character_kit {
        return ModelClass::Character;
    }
    match kind {
        "pilot" => ModelClass::Character,
        "hull" => ModelClass::Vehicle,
        "structure" => ModelClass::Building,
        _ => ModelClass::Prop,
    }
}

/// A kit model's budget: `report` measured LOD0-only, plus the LOD chain
/// (`(level, authored, triangles)`).
#[must_use]
pub fn kit_budget(
    report: &ModelReport,
    lods: &[(usize, bool, u64)],
    class: ModelClass,
    limits: BudgetLimits,
) -> Budget {
    let _span = tracing::info_span!(target: "regia_editor", "kit_budget").entered();
    let measure = Measure::of_report(report, true);
    Budget {
        lods: lod_rows(lods),
        ..Budget::from_measure(&report.source, "model", class, limits, &measure)
    }
}

/// Reports a glTF or GLB from its JSON and image headers: no buffer is read and no texture is
/// decoded. Embedded images (GLB `BIN` views) and relative image files beside the model are
/// sized from their headers; other images (data URIs, paths leaving the folder) count as 0×0.
///
/// # Errors
///
/// The file cannot be read or is not valid glTF.
pub fn report_from_file(path: &Path) -> Result<ModelReport, String> {
    let _span = tracing::info_span!(target: "regia_editor", "budget_report").entered();
    let bytes = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let gltf = regia_asset_pipeline::parse_gltf_document(&bytes)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new(""));
    let sizes: Vec<(u32, u32)> = gltf
        .document
        .images()
        .map(|image| image_size(&image, gltf.blob.as_deref(), dir).unwrap_or((0, 0)))
        .collect();
    let class = ModelClass::Prop;
    Ok(ModelReport::from_document(
        &path.display().to_string(),
        &gltf.document,
        &sizes,
        class,
        class.limits(),
    ))
}

/// An image's size from its header.
fn image_size(image: &gltf::Image<'_>, blob: Option<&[u8]>, dir: &Path) -> Option<(u32, u32)> {
    match image.source() {
        gltf::image::Source::View { view, .. } => {
            if !matches!(view.buffer().source(), gltf::buffer::Source::Bin) {
                return None;
            }
            let end = view.offset().checked_add(view.length())?;
            let bytes = blob?.get(view.offset()..end)?;
            image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .ok()?
                .into_dimensions()
                .ok()
        }
        gltf::image::Source::Uri { uri, .. } => {
            let rel = Path::new(uri);
            let inside = !uri.contains(':')
                && rel
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)));
            if !inside {
                return None;
            }
            image::ImageReader::open(dir.join(rel))
                .ok()?
                .with_guessed_format()
                .ok()?
                .into_dimensions()
                .ok()
        }
    }
}

struct CachedReport {
    stamp: Option<(SystemTime, u64)>,
    report: Result<Arc<ModelReport>, String>,
}

/// Model reports by file, kept until the file's modification time or size changes (a world
/// resource shared by prefab mode and `editor.budget`).
#[derive(Resource, Default)]
pub struct ReportCache {
    entries: HashMap<PathBuf, CachedReport>,
    /// Files read so far (a cache hit reads nothing).
    pub reads: u64,
}

impl ReportCache {
    /// The report of `path`, read when it is new or changed.
    ///
    /// # Errors
    ///
    /// What [`report_from_file`] refuses (cached like a report until the file changes).
    pub fn report(&mut self, path: &Path) -> Result<Arc<ModelReport>, String> {
        let stamp = std::fs::metadata(path)
            .ok()
            .and_then(|meta| meta.modified().ok().map(|modified| (modified, meta.len())));
        if let Some(entry) = self.entries.get(path)
            && stamp.is_some()
            && entry.stamp == stamp
        {
            return entry.report.clone();
        }
        self.reads += 1;
        let report = report_from_file(path).map(Arc::new);
        self.entries.insert(
            path.to_path_buf(),
            CachedReport {
                stamp,
                report: report.clone(),
            },
        );
        report
    }
}

/// The budget of every `MeshSource` in `world` (built-in primitives and editor-internal
/// entities are left out). Paths resolve against `root` like the viewport's. Spellings that
/// resolve to the same file (case variants on a case-insensitive filesystem, `sub/../` forms)
/// measure once — the budget reports per model file (audit G finding 9).
#[must_use]
pub fn scene_budget(
    world: &World,
    root: Option<&Path>,
    class: ModelClass,
    limits: BudgetLimits,
    cache: &mut ReportCache,
) -> Budget {
    let _span = tracing::info_span!(target: "regia_editor", "scene_budget").entered();
    // Group by the source string first so the resolve + canonicalize cost below is paid
    // once per distinct mesh, not once per instance (audit G editor4-platform L4): a level
    // with tens of thousands of instances of one model recompute the banner on any edit.
    // Per source string: (instance count, whether ANY instance uses the model's own
    // materials); override material strings are global and resolved once each below.
    let mut meshes: BTreeMap<String, (u64, bool)> = BTreeMap::new();
    let mut override_sources: BTreeSet<String> = BTreeSet::new();
    for entity_ref in world.iter_entities() {
        let entity = entity_ref.id();
        if crate::visibility::is_hidden(world, entity) {
            continue;
        }
        let Some(source) = world.get::<MeshSource>(entity) else {
            continue;
        };
        if source.mesh.is_empty() || source.mesh.starts_with("builtin:") {
            continue;
        }
        let entry = meshes.entry(source.mesh.clone()).or_insert((0, false));
        entry.0 += 1;
        if source.material.is_empty() {
            entry.1 = true;
        } else {
            override_sources.insert(source.material.clone());
        }
    }
    // One entry per resolved file: (instance count, a representative source string, whether
    // any instance uses the model's own materials). Spellings that name one file (case
    // variants on case-insensitive filesystems, `sub/../`) merge into one entry.
    let mut files: BTreeMap<PathBuf, (u64, String, bool)> = BTreeMap::new();
    for (mesh, (count, any_own)) in meshes {
        let key = crate::mesh_source::resolve_asset_path(root, &mesh);
        let key = key.canonicalize().unwrap_or(key);
        let entry = files.entry(key).or_insert((0, mesh, false));
        entry.0 += count;
        entry.2 |= any_own;
    }
    // Override materials, keyed like the models: one entry per resolved file however its path
    // is spelled (`assets/./bark.material.ron`, a case variant on a case-insensitive disk).
    let mut overrides: BTreeSet<PathBuf> = BTreeSet::new();
    for material in override_sources {
        let material = crate::mesh_source::resolve_asset_path(root, &material);
        let material: PathBuf = material.components().collect();
        overrides.insert(material.canonicalize().unwrap_or(material));
    }

    let mut measure = Measure::default();
    let mut models = 0;
    let mut errors = Vec::new();
    for (key, &(count, ref mesh, own_materials)) in &files {
        let report = match cache.report(key) {
            Ok(report) => report,
            Err(err) => {
                errors.push(format!("{mesh}: {err}"));
                continue;
            }
        };
        models += 1;
        let one = Measure::of_report(&report, true);
        measure.triangles = measure
            .triangles
            .saturating_add(one.triangles.saturating_mul(count));
        measure.nodes = measure.nodes.saturating_add(one.nodes.saturating_mul(count));
        if own_materials {
            measure.materials = measure.materials.saturating_add(one.materials);
        }
        measure.texture_bytes = measure.texture_bytes.saturating_add(one.texture_bytes);
        let peaks = [
            (&mut measure.node_triangles, &one.node_triangles),
            (&mut measure.node_materials, &one.node_materials),
            (&mut measure.skin_joints, &one.skin_joints),
            (&mut measure.texture_side, &one.texture_side),
        ];
        for (total, peak) in peaks {
            total.raise(peak.value, || format!("{mesh}: {}", peak.subject));
        }
    }
    let instances = files.values().map(|entry| entry.0).sum::<u64>() as usize;
    measure.materials = measure.materials.saturating_add(overrides.len() as u64);
    Budget {
        models,
        instances,
        errors,
        ..Budget::from_measure("scene", "scene", class, limits, &measure)
    }
}

// ---------------------------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------------------------

/// The color of a status in the panels.
#[must_use]
pub fn status_color(status: BudgetStatus, visuals: &egui::Visuals) -> egui::Color32 {
    match status {
        BudgetStatus::Off => visuals.weak_text_color(),
        BudgetStatus::Ok => egui::Color32::from_rgb(110, 200, 120),
        BudgetStatus::Near => egui::Color32::from_rgb(230, 180, 70),
        BudgetStatus::Over => egui::Color32::from_rgb(230, 100, 90),
    }
}

/// Draws a budget: one meter per limit, the LOD chain and the files that failed.
pub fn draw_budget(ui: &mut egui::Ui, budget: &Budget, id_salt: &str) {
    let visuals = ui.visuals().clone();
    egui::Grid::new(("budget-rows", id_salt))
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            for row in &budget.rows {
                let color = status_color(row.status, &visuals);
                ui.label(row.label);
                ui.label(format!("{} / {}", row.display_value(), row.display_limit()));
                match row.fraction() {
                    Some(fraction) => {
                        ui.add(
                            egui::ProgressBar::new(fraction.min(1.0) as f32)
                                .desired_width(110.0)
                                .fill(color)
                                .text(format!("{:.0} %", fraction * 100.0)),
                        );
                    }
                    None => {
                        ui.weak("off");
                    }
                }
                if row.subject.is_empty() {
                    ui.weak("-");
                } else {
                    ui.colored_label(color, &row.subject);
                }
                ui.end_row();
            }
        });
    if !budget.lods.is_empty() {
        ui.add_space(4.0);
        egui::Grid::new(("budget-lods", id_salt))
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for lod in &budget.lods {
                    let status = if lod.reduces {
                        BudgetStatus::Ok
                    } else {
                        BudgetStatus::Over
                    };
                    ui.label(format!(
                        "LOD{}{}",
                        lod.level,
                        if lod.authored { "" } else { " (generated)" }
                    ));
                    ui.label(format!(
                        "{} tris · {:.0} % of LOD0",
                        lod.triangles,
                        lod.ratio * 100.0
                    ));
                    ui.colored_label(
                        status_color(status, &visuals),
                        if lod.reduces {
                            "reduces"
                        } else {
                            "not fewer than the level above"
                        },
                    );
                    ui.end_row();
                }
            });
    }
    for error in &budget.errors {
        ui.colored_label(status_color(BudgetStatus::Over, &visuals), error);
    }
}

/// A class picker; returns whether the class changed.
pub fn class_picker(ui: &mut egui::Ui, class: &mut ModelClass, id_salt: &str) -> bool {
    let before = *class;
    egui::ComboBox::from_id_salt(("budget-class", id_salt))
        .selected_text(class.name())
        .show_ui(ui, |ui| {
            for option in ModelClass::ALL {
                ui.selectable_value(class, option, option.name());
            }
        });
    *class != before
}

/// Prefab mode's budget state: the class picked in the banner and the last budget, recomputed
/// when the prefab, the class, the entity count or the undo history changes.
#[derive(Resource, Default)]
pub struct SceneBudgetView {
    /// The class the prefab is budgeted as.
    pub class: ModelClass,
    /// The last budget.
    pub budget: Option<Budget>,
    key: Option<SceneKey>,
}

type SceneKey = (Option<PathBuf>, ModelClass, usize, usize, usize);

/// The scene budget for the banner, recomputed only when its key changes (or `force`).
pub fn refresh_scene_budget(world: &mut World, force: bool) -> Budget {
    let mut view = world.remove_resource::<SceneBudgetView>().unwrap_or_default();
    let prefab = world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .and_then(|mode| mode.editing().map(|path| path.to_path_buf()));
    let (undo, redo) = world
        .get_resource::<crate::command::UndoStack>()
        .map_or((0, 0), |stack| (stack.undo_depth(), stack.redo_depth()));
    let key = (prefab, view.class, world.iter_entities().count(), undo, redo);
    if force || view.key.as_ref() != Some(&key) || view.budget.is_none() {
        let root = crate::mesh_source::project_root(world);
        let mut cache = world.remove_resource::<ReportCache>().unwrap_or_default();
        let limits = view.class.limits();
        view.budget = Some(scene_budget(
            world,
            root.as_deref(),
            view.class,
            limits,
            &mut cache,
        ));
        world.insert_resource(cache);
        view.key = Some(key);
    }
    let budget = view.budget.clone().unwrap_or_else(|| {
        Budget::from_measure(
            "scene",
            "scene",
            view.class,
            view.class.limits(),
            &Measure::default(),
        )
    });
    world.insert_resource(view);
    budget
}

/// The prefab-mode banner's budget: the summary line in the worst status's color (hover for
/// every meter), a class picker and a re-measure button.
pub fn prefab_budget_ui(ui: &mut egui::Ui, world: &mut World) {
    let mut force = false;
    let mut class = world
        .get_resource::<SceneBudgetView>()
        .map(|view| view.class)
        .unwrap_or_default();
    let budget = refresh_scene_budget(world, false);
    let color = status_color(budget.worst(), ui.visuals());
    ui.separator();
    ui.colored_label(color, format!("Budget {}", budget.summary_line()))
        .on_hover_ui(|ui| draw_budget(ui, &budget, "prefab"));
    if class_picker(ui, &mut class, "prefab") {
        world.get_resource_or_insert_with(SceneBudgetView::default).class = class;
        force = true;
    }
    if ui
        .small_button("↻")
        .on_hover_text("Measure the prefab again")
        .clicked()
    {
        force = true;
    }
    if force {
        let _ = refresh_scene_budget(world, true);
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn budget_error(code: i64, budget_error: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(code, "F0002", message, hint);
    if let Some(data) = error.data.as_mut() {
        data["budget_error"] = json!(budget_error);
    }
    error
}

/// `editor.budget`.
fn budget_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let class = match str_param(params, "class") {
        None => None,
        Some(name) => Some(ModelClass::from_name(name).ok_or_else(|| {
            budget_error(
                regia_agent::protocol::INVALID_PARAMS,
                "budget_bad_class",
                format!("unknown class `{name}`"),
                "use prop, character, vehicle, building or hero",
            )
        })?),
    };
    let limits_for = |class: ModelClass| -> Result<BudgetLimits, RpcError> {
        match params.get("limits") {
            None | Some(Value::Null) => Ok(class.limits()),
            Some(overrides) => class.limits().with_overrides(overrides).map_err(|err| {
                budget_error(
                    regia_agent::protocol::INVALID_PARAMS,
                    "budget_bad_limits",
                    err,
                    "limits take the editor.model_report limit names; 0 turns a check off",
                )
            }),
        }
    };
    match str_param(params, "source").unwrap_or("scene") {
        "kit" => {
            let view = world.get_resource::<crate::kit_view::KitView>();
            let Some(view) = view.filter(|view| view.kit.is_some()) else {
                return Err(budget_error(
                    regia_agent::protocol::NOT_FOUND,
                    "budget_no_kit",
                    "no kit is open",
                    "open one with editor.open_kit",
                ));
            };
            let class = class.unwrap_or_else(|| view.budget_class());
            let limits = limits_for(class)?;
            let budget = view.budget_with(class, limits).ok_or_else(|| {
                budget_error(
                    regia_agent::protocol::NOT_FOUND,
                    "budget_not_measured",
                    "the kit's model could not be measured (see the kit warnings)",
                    "fix the model file the warning names and reopen the kit",
                )
            })?;
            Ok(json!({ "source": "kit", "budget": budget.to_json() }))
        }
        "scene" => {
            let class = class
                .or_else(|| world.get_resource::<SceneBudgetView>().map(|view| view.class))
                .unwrap_or_default();
            let limits = limits_for(class)?;
            let root = crate::mesh_source::project_root(world);
            let mut cache = world.remove_resource::<ReportCache>().unwrap_or_default();
            let budget = scene_budget(world, root.as_deref(), class, limits, &mut cache);
            let reads = cache.reads;
            world.insert_resource(cache);
            let prefab = world
                .get_resource::<crate::prefab_edit::PrefabEditMode>()
                .and_then(|mode| mode.editing().map(|path| path.display().to_string()));
            Ok(json!({
                "source": "scene",
                "prefab": prefab,
                "budget": budget.to_json(),
                "reports_read": reads,
            }))
        }
        other => Err(budget_error(
            regia_agent::protocol::INVALID_PARAMS,
            "budget_bad_source",
            format!("unknown source `{other}`"),
            "use \"kit\" (the Kit panel's model) or \"scene\" (the edit world: the open prefab in prefab mode)",
        )),
    }
}

/// The budget agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.budget",
            tool: "budget",
            description: "Budget meters (CPU only, no texture decode): the open kit's model (LOD0 plus its \
                LOD chain) or every MeshSource of the edit world (the open prefab in prefab mode) \
                against a model class's limits. Each row has value, limit, fraction and status \
                (ok, near from 80 %, over, off).",
            params: &[
                ParamSchema {
                    name: "source",
                    kind: "string",
                    required: false,
                    description: "\"scene\" (default: the edit world) or \"kit\" (the Kit panel's model).",
                },
                ParamSchema {
                    name: "class",
                    kind: "string",
                    required: false,
                    description: "prop, character, vehicle, building or hero. Default: from kit.json for a kit, the banner's class (prop) for a scene.",
                },
                ParamSchema {
                    name: "limits",
                    kind: "object",
                    required: false,
                    description: "Overrides over the class limits (editor.model_report's names); 0 turns a check off.",
                },
            ],
            result: "{ source, prefab?, budget: { source, class, limits, status, over, rows: [{key, label, subject, value, limit, fraction, status}], lods: [{level, authored, triangles, ratio, reduces}], models, instances, errors }, reports_read? }",
            mutating: false,
            permission: Permission::Read,
        },
        budget_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::Transform;

    fn f32s(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer
                .write_image_data(&vec![90u8; (width * height * 4) as usize])
                .expect("png data");
        }
        out
    }

    /// A GLB with `body_LOD0` (`lod0` triangles), `body_LOD1` (`lod1` triangles) and a base
    /// color texture of `side`×`side` embedded in BIN.
    fn glb(lod0: usize, lod1: usize, side: u32) -> Vec<u8> {
        let tri = |z: f32| [0.0, 0.0, z, 1.0, 0.0, z, 0.0, 1.0, z];
        let positions = |n: usize| -> Vec<u8> {
            let floats: Vec<f32> = (0..n).flat_map(|i| tri(i as f32)).collect();
            f32s(&floats)
        };
        let mut bin = Vec::new();
        let mut views = Vec::new();
        let mut push = |bytes: Vec<u8>, bin: &mut Vec<u8>| {
            while !bin.len().is_multiple_of(4) {
                bin.push(0);
            }
            views.push(json!({ "buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len() }));
            bin.extend(bytes);
            views.len() - 1
        };
        let v0 = push(positions(lod0), &mut bin);
        let v1 = push(positions(lod1), &mut bin);
        let vi = push(png_bytes(side, side), &mut bin);
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let accessor = |view: usize, n: usize| {
            json!({ "bufferView": view, "componentType": 5126, "count": n * 3, "type": "VEC3",
                    "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, n.saturating_sub(1) as f32] })
        };
        let doc = json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "name": "Root", "children": [1, 2] },
                { "name": "body_LOD0", "mesh": 0 },
                { "name": "body_LOD1", "mesh": 1 }
            ],
            "meshes": [
                { "primitives": [{ "attributes": { "POSITION": 0 }, "material": 0 }] },
                { "primitives": [{ "attributes": { "POSITION": 1 }, "material": 0 }] }
            ],
            "materials": [{ "name": "Paint", "pbrMetallicRoughness": { "baseColorTexture": { "index": 0 } } }],
            "textures": [{ "source": 0 }],
            "images": [{ "bufferView": vi, "mimeType": "image/png" }],
            "accessors": [accessor(v0, lod0), accessor(v1, lod1)],
            "bufferViews": views,
            "buffers": [{ "byteLength": bin.len() }]
        });
        let mut text = serde_json::to_vec(&doc).expect("json");
        while !text.len().is_multiple_of(4) {
            text.push(b' ');
        }
        let total = 12 + 8 + text.len() + 8 + bin.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&text);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
        out
    }

    fn write_glb(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        regia_test_utils::write_atomic(&path, bytes).expect("write glb");
        path
    }

    #[test]
    fn statuses_follow_the_limit() {
        assert_eq!(BudgetStatus::of(5, 0), BudgetStatus::Off);
        assert_eq!(BudgetStatus::of(79, 100), BudgetStatus::Ok);
        assert_eq!(BudgetStatus::of(80, 100), BudgetStatus::Near);
        assert_eq!(
            BudgetStatus::of(100, 100),
            BudgetStatus::Near,
            "at the limit is not over"
        );
        assert_eq!(BudgetStatus::of(101, 100), BudgetStatus::Over);
        assert!(BudgetStatus::Over > BudgetStatus::Near && BudgetStatus::Near > BudgetStatus::Ok);
        let row = BudgetRow::new("texture_bytes", "Texture memory", "model", 3 * 1024 * 1024, 0);
        assert_eq!(row.fraction(), None);
        assert_eq!(row.display_value(), "3.0 MiB");
        assert_eq!(row.display_limit(), "off");
    }

    #[test]
    fn lod_rows_flag_a_level_that_does_not_reduce() {
        let rows = lod_rows(&[(0, true, 1000), (1, true, 400), (2, true, 400), (3, false, 100)]);
        assert_eq!(
            rows.iter().map(|row| row.reduces).collect::<Vec<_>>(),
            [true, true, false, true]
        );
        assert!((rows[1].ratio - 0.4).abs() < 1e-9);
        assert!(
            lod_rows(&[(0, true, 0)])[0].ratio.abs() < f64::EPSILON,
            "an empty LOD0 is 0, not NaN"
        );
        assert!(lod_rows(&[]).is_empty());
    }

    #[test]
    fn kit_classes_follow_the_manifest() {
        assert_eq!(kit_class("hull", false), ModelClass::Vehicle);
        assert_eq!(kit_class("structure", false), ModelClass::Building);
        assert_eq!(kit_class("pilot", false), ModelClass::Character);
        assert_eq!(kit_class("weapon", false), ModelClass::Prop);
        assert_eq!(
            kit_class("hull", true),
            ModelClass::Character,
            "a tagged skeleton wins"
        );
    }

    #[test]
    fn a_model_is_measured_from_headers_with_lod0_triangles() {
        let dir = regia_test_utils::unique_temp_dir("budget_model");
        let path = write_glb(&dir, "crate.glb", &glb(6, 2, 64));
        let report = report_from_file(&path).expect("report");
        // With LODs the total is LOD0 only, the others are alternatives to it (ADR-511).
        assert_eq!(report.totals.triangles, 6, "the report counts the LOD0 node");
        assert_eq!(
            report.lods.iter().map(|l| l.triangles).sum::<u64>(),
            8,
            "every LOD is measured"
        );
        let measure = Measure::of_report(&report, true);
        assert_eq!(measure.triangles, 6, "LOD1 is left out of the drawn triangles");
        assert_eq!(Measure::of_report(&report, false).triangles, 8);
        assert_eq!(measure.texture_side.value, 64, "the PNG header gives the size");
        assert_eq!(measure.texture_side.subject, "Paint/base_color");
        assert_eq!(measure.node_triangles.subject, "body_LOD0");

        let limits = BudgetLimits {
            max_triangles: 5,
            max_texture_dimension: 64,
            ..ModelClass::Prop.limits()
        };
        let budget = kit_budget(&report, &[(0, true, 6), (1, true, 2)], ModelClass::Prop, limits);
        let triangles = budget.row("triangles").expect("row");
        assert_eq!(
            (triangles.value, triangles.limit, triangles.status),
            (6, 5, BudgetStatus::Over)
        );
        assert_eq!(
            budget.row("texture_side").map(|row| row.status),
            Some(BudgetStatus::Near)
        );
        assert_eq!(budget.worst(), BudgetStatus::Over);
        assert_eq!(budget.over_count(), 1);
        let json = budget.to_json();
        assert_eq!(json["status"], "over");
        assert_eq!(json["rows"][0]["key"], "triangles");
        assert_eq!(json["lods"][1]["reduces"], true);
        assert!(
            budget.summary_line().starts_with("prop · 6/5 tris"),
            "{}",
            budget.summary_line()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_model_is_an_error_not_a_panic() {
        let dir = regia_test_utils::unique_temp_dir("budget_broken");
        let path = write_glb(&dir, "bad.glb", b"glTF\x02\0\0\0garbage");
        assert!(report_from_file(&path).is_err());
        assert!(report_from_file(&dir.join("missing.glb")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_reads_a_file_once_until_it_changes() {
        let dir = regia_test_utils::unique_temp_dir("budget_cache");
        let path = write_glb(&dir, "a.glb", &glb(2, 1, 8));
        let mut cache = ReportCache::default();
        let first = cache.report(&path).expect("report");
        let again = cache.report(&path).expect("cached");
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(cache.reads, 1, "the second call is a cache hit");
        // A different size is a change even when the clock does not move.
        write_glb(&dir, "a.glb", &glb(3, 1, 8));
        let changed = cache.report(&path).expect("reread");
        assert_eq!(cache.reads, 2);
        assert_eq!(changed.totals.triangles, 3, "LOD0 grew from 2 to 3 triangles");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scene_sums_instances_and_counts_shared_textures_once() {
        let dir = regia_test_utils::unique_temp_dir("budget_scene");
        write_glb(&dir, "assets/rock.glb", &glb(10, 4, 32));
        write_glb(&dir, "assets/tree.glb", &glb(20, 5, 16));
        let mut world = World::new();
        for _ in 0..3 {
            world.spawn((Transform::default(), MeshSource::new("assets/rock.glb", "")));
        }
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/tree.glb", "assets/bark.material.ron"),
        ));
        world.spawn((
            Transform::default(),
            MeshSource::new(MeshSource::BUILTIN_CUBE, ""),
        ));
        world.spawn((Transform::default(), MeshSource::new("assets/missing.glb", "")));
        let mut cache = ReportCache::default();
        let budget = scene_budget(
            &world,
            Some(&dir),
            ModelClass::Prop,
            ModelClass::Prop.limits(),
            &mut cache,
        );
        assert_eq!(budget.row("triangles").map(|row| row.value), Some(3 * 10 + 20));
        assert_eq!(budget.row("nodes").map(|row| row.value), Some(3 * 3 + 3));
        assert_eq!(
            budget.row("materials").map(|row| row.value),
            Some(2),
            "rock's own Paint and the tree's override"
        );
        let rock_bytes = 32 * 32 * 4 * 4 / 3;
        let tree_bytes = 16 * 16 * 4 * 4 / 3;
        assert_eq!(
            budget.row("texture_bytes").map(|row| row.value),
            Some(rock_bytes + tree_bytes),
            "a model's textures count once however many instances"
        );
        assert_eq!(
            budget.row("node_triangles").map(|row| row.subject.as_str()),
            Some("assets/tree.glb: body_LOD0")
        );
        assert_eq!((budget.models, budget.instances), (2, 5));
        assert_eq!(budget.errors.len(), 1, "{:?}", budget.errors);
        assert!(budget.errors[0].starts_with("assets/missing.glb"));
        assert_eq!(
            budget.worst(),
            BudgetStatus::Over,
            "an unmeasured model is not a pass"
        );
        let reads = cache.reads;
        let _ = scene_budget(
            &world,
            Some(&dir),
            ModelClass::Prop,
            ModelClass::Prop.limits(),
            &mut cache,
        );
        assert_eq!(cache.reads, reads + 1, "only the missing file is tried again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit G editor4-platform M2 (sprint-fixrev-08-k): one model whose instances mix
    /// "own materials" (`material: ""`) with an override keeps counting its own materials —
    /// the module contract (budget.rs, top) counts the model's materials when ANY instance
    /// uses them; the regression counted them only when EVERY instance did, dropping them
    /// from the `materials` row entirely here.
    #[test]
    fn scene_budget_counts_own_materials_when_any_instance_uses_them() {
        let dir = regia_test_utils::unique_temp_dir("budget_mixed_own_override");
        write_glb(&dir, "assets/rock.glb", &glb(10, 4, 32));
        let mut world = World::new();
        world.spawn((Transform::default(), MeshSource::new("assets/rock.glb", "")));
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/rock.glb", "assets/moss.material.ron"),
        ));
        let mut cache = ReportCache::default();
        let budget = scene_budget(
            &world,
            Some(&dir),
            ModelClass::Prop,
            ModelClass::Prop.limits(),
            &mut cache,
        );
        assert_eq!(
            budget.row("materials").map(|row| row.value),
            Some(2),
            "the model's own material (counted once, any instance) plus the override"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit G finding 9: two `MeshSource` strings that resolve to the same file (on a
    /// case-insensitive filesystem, spellings like `Assets/Rock.glb` vs `assets/rock.glb`; the
    /// `sub/../` spelling proves the same merge on every host) must measure once — the budget
    /// reports per model file.
    #[test]
    fn case_variant_mesh_paths_count_once() {
        let dir = regia_test_utils::unique_temp_dir("budget_case_variant");
        write_glb(&dir, "assets/rock.glb", &glb(10, 4, 32));
        let mut world = World::new();
        world.spawn((Transform::default(), MeshSource::new("assets/rock.glb", "")));
        world.spawn((Transform::default(), MeshSource::new("assets/./rock.glb", "")));
        let mut cache = ReportCache::default();
        let budget = scene_budget(
            &world,
            Some(&dir),
            ModelClass::Prop,
            ModelClass::Prop.limits(),
            &mut cache,
        );
        assert_eq!(
            (budget.models, budget.instances),
            (1, 2),
            "one model file however its path is spelled"
        );
        assert_eq!(budget.row("triangles").map(|row| row.value), Some(2 * 10));
        assert_eq!(cache.reads, 1, "the file is measured once");
    }

    /// A material override replaces the model's own materials on that instance only. Another
    /// instance of the same file that still uses the model's materials keeps them in the scene
    /// budget (module docs: materials once per distinct material).
    #[test]
    fn mixed_material_instances_of_one_file_keep_the_models_materials() {
        let dir = regia_test_utils::unique_temp_dir("budget_mixed_materials");
        write_glb(&dir, "assets/rock.glb", &glb(10, 4, 32));
        let mut world = World::new();
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/rock.glb", "assets/bark.material.ron"),
        ));
        world.spawn((Transform::default(), MeshSource::new("assets/./rock.glb", "")));
        let mut cache = ReportCache::default();
        let budget = scene_budget(
            &world,
            Some(&dir),
            ModelClass::Prop,
            ModelClass::Prop.limits(),
            &mut cache,
        );
        assert_eq!((budget.models, budget.instances), (1, 2));
        assert_eq!(
            budget.row("materials").map(|row| row.value),
            Some(2),
            "the model's Paint stays in use beside the override, got {:?}",
            budget.row("materials").map(|row| row.value)
        );
        assert_eq!(budget.row("triangles").map(|row| row.value), Some(2 * 10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every instance of a file that carries an override drops the model's own materials.
    /// Two different overrides count as two materials, not those plus the model's Paint.
    #[test]
    fn override_only_instances_count_the_overrides_not_the_model() {
        let dir = regia_test_utils::unique_temp_dir("budget_override_only");
        write_glb(&dir, "assets/rock.glb", &glb(10, 4, 32));
        let mut world = World::new();
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/rock.glb", "assets/bark.material.ron"),
        ));
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/./rock.glb", "assets/leaf.material.ron"),
        ));
        let mut cache = ReportCache::default();
        let budget = scene_budget(
            &world,
            Some(&dir),
            ModelClass::Prop,
            ModelClass::Prop.limits(),
            &mut cache,
        );
        assert_eq!((budget.models, budget.instances), (1, 2));
        assert_eq!(
            budget.row("materials").map(|row| row.value),
            Some(2),
            "two overrides and no own-material instance, got {:?}",
            budget.row("materials").map(|row| row.value)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Corr editor-a finding 5: the override materials are counted per file like the models; two
    /// spellings of one material file used to count as two materials.
    #[test]
    fn override_material_spellings_count_once() {
        let dir = regia_test_utils::unique_temp_dir("budget_override_spellings");
        write_glb(&dir, "assets/rock.glb", &glb(10, 4, 32));
        regia_test_utils::write_atomic(&dir.join("assets/bark.material.ron"), b"()").expect("material");
        std::fs::create_dir_all(dir.join("assets/never")).expect("dir");
        let mut world = World::new();
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/rock.glb", "assets/bark.material.ron"),
        ));
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/rock.glb", "assets/./bark.material.ron"),
        ));
        world.spawn((
            Transform::default(),
            MeshSource::new("assets/rock.glb", "assets/never/../bark.material.ron"),
        ));
        let mut cache = ReportCache::default();
        let budget = scene_budget(
            &world,
            Some(&dir),
            ModelClass::Prop,
            ModelClass::Prop.limits(),
            &mut cache,
        );
        assert_eq!(
            budget.row("materials").map(|row| row.value),
            Some(1),
            "one material file however its path is spelled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_banner_budget_recomputes_only_when_its_key_changes() {
        let dir = regia_test_utils::unique_temp_dir("budget_banner");
        let path = write_glb(&dir, "rock.glb", &glb(10, 4, 8));
        let mut world = World::new();
        world.spawn((
            Transform::default(),
            MeshSource::new(path.display().to_string(), ""),
        ));
        let first = refresh_scene_budget(&mut world, false);
        assert_eq!(first.row("triangles").map(|row| row.value), Some(10));
        let reads = world.resource::<ReportCache>().reads;
        let _ = refresh_scene_budget(&mut world, false);
        assert_eq!(
            world.resource::<ReportCache>().reads,
            reads,
            "same key: nothing re-measured"
        );
        world.spawn((
            Transform::default(),
            MeshSource::new(path.display().to_string(), ""),
        ));
        let second = refresh_scene_budget(&mut world, false);
        assert_eq!(
            second.row("triangles").map(|row| row.value),
            Some(20),
            "a new entity re-measures"
        );
        world.resource_mut::<SceneBudgetView>().class = ModelClass::Building;
        assert_eq!(
            refresh_scene_budget(&mut world, false).class,
            ModelClass::Building
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agent_method_measures_the_scene_and_rejects_bad_input() {
        let dir = regia_test_utils::unique_temp_dir("budget_agent");
        let path = write_glb(&dir, "rock.glb", &glb(10, 4, 8));
        let mut world = World::new();
        world.spawn((
            Transform::default(),
            MeshSource::new(path.display().to_string(), ""),
        ));
        let reply = budget_method(
            &mut world,
            &json!({ "class": "building", "limits": { "max_triangles": 9 } }),
        )
        .expect("editor.budget");
        assert_eq!(reply["source"], "scene");
        assert_eq!(reply["budget"]["class"], "building");
        assert_eq!(reply["budget"]["rows"][0]["value"], 10);
        assert_eq!(reply["budget"]["rows"][0]["status"], "over");
        assert_eq!(reply["budget"]["status"], "over");

        let code = |err: RpcError| err.data.and_then(|d| d.get("budget_error").cloned());
        let err = budget_method(&mut world, &json!({ "class": "tank" })).expect_err("bad class");
        assert_eq!(code(err), Some(json!("budget_bad_class")));
        let err = budget_method(&mut world, &json!({ "limits": { "max_tris": 1 } })).expect_err("bad limits");
        assert_eq!(code(err), Some(json!("budget_bad_limits")));
        let err = budget_method(&mut world, &json!({ "source": "level" })).expect_err("bad source");
        assert_eq!(code(err), Some(json!("budget_bad_source")));
        let err = budget_method(&mut world, &json!({ "source": "kit" })).expect_err("no kit");
        assert_eq!(code(err), Some(json!("budget_no_kit")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
