// SPDX-License-Identifier: MPL-2.0
//! The Lightmaps panel: runs `regia bake-lightmaps` (`book/agents/lightmaps.md`, ADR-2024) on the
//! open level, a project level, a `.bake.ron` or a built-in, with progress and cancel, and shows
//! what the bake laid out: the atlas with each entry's rectangle, a density preview (each entry's
//! `texels_per_metre` against the density asked for) and the chart seams (per entry, in the atlas
//! and as lines in the viewport).
//!
//! The bake runs in a child process, as the Bake runner's does ([`crate::bake_runner`], ADR-866):
//! the editor does not link the path tracer, its frame never waits on a bake, and **Cancel** is a
//! process kill. What the panel shows comes only from the CLI's JSON ([`AtlasView::from_summary`]),
//! so an agent reads the same thing a user sees (`editor.lightmap_status`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use regia_asset::lightmap::LightmapTexelEncoding;

use crate::audio_gizmo::Segment;
use crate::bake_runner::{
    self, BUILTIN_SCENES, BakeFailure, BakeJob, BakeProgress, BakeRequest, BakeState, SPAWN_FAILED,
};
use crate::viewport_agent::{Handler, str_param};

/// The lightmap baker's own reference scene, a built-in of `regia bake-lightmaps` only.
pub const QUAD_BOX: &str = "quad-box";

/// The chart-seam budget of a project that sets none (`[lightmaps] seam_budget`).
pub const DEFAULT_SEAM_BUDGET: f32 = regia_asset::lightmap::DEFAULT_SEAM_BUDGET;

/// What the atlas preview colors the entries by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AtlasPreview {
    /// Each entry's density against the density asked for.
    #[default]
    Density,
    /// Each entry's chart-seam step against the budget, with the worst seams drawn.
    Seams,
}

impl AtlasPreview {
    /// `density` or `seams`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Density => "density",
            Self::Seams => "seams",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "density" => Some(Self::Density),
            "seams" => Some(Self::Seams),
            _ => None,
        }
    }
}

/// The `regia bake-lightmaps` settings the panel sets.
#[derive(Clone, Debug, PartialEq)]
pub struct LightmapFields {
    /// `--density`: texels per metre.
    pub density: f32,
    /// `--samples`: hemisphere samples per texel.
    pub samples: u32,
    /// `--bounces`: path vertices after the texel.
    pub bounces: u32,
    /// `--seed`.
    pub seed: u32,
    /// `--max-atlas`: the largest atlas side.
    pub max_atlas: u32,
    /// `--max-pages`: atlas pages before the density is lowered (ADR-3300; the CLI's default,
    /// `regia_render::lightmap::DEFAULT_LIGHTMAP_MAX_PAGES`).
    pub max_pages: u32,
    /// `--direct-sun`: also bake the sun seen directly.
    pub direct_sun: bool,
    /// `--no-filter`: skip the edge-aware filter.
    pub no_filter: bool,
    /// `--unwrap charts` in place of the UV atlas generator.
    pub planar_charts: bool,
    /// `--gpu`: bake on the GPU (a CLI built with `lightmap-gpu`).
    pub gpu: bool,
    /// `--meshlets`: store clusters over each entry's unwrap for the visibility buffer (ADR-2107).
    pub meshlets: bool,
    /// `--denoise on|off`: the à-trous denoiser (ADR-2314; on for new bakes).
    pub denoise: bool,
    /// `--encoding`: how the `.flmap` stores its texels (ADR-2314; RGB9E5 by default).
    pub encoding: LightmapTexelEncoding,
    /// `--crease on|off`: crease-aware texel placement (ADR-2404; on for new bakes).
    pub crease: bool,
    /// `--stitch on|off`: seam stitching (ADR-2404; on for new bakes).
    pub stitch: bool,
    /// `--auto-density on|off`: the automatic density of small curved receivers (ADR-2404; on for
    /// new bakes).
    pub auto_density: bool,
}

/// The CLI's `--encoding` value of `encoding`.
fn encoding_arg(encoding: LightmapTexelEncoding) -> &'static str {
    match encoding {
        LightmapTexelEncoding::Rgb9e5 => "rgb9e5",
        LightmapTexelEncoding::Bc6h => "bc6h",
        LightmapTexelEncoding::AstcHdr => "astc-hdr",
    }
}

/// `on` or `off`.
fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

impl Default for LightmapFields {
    fn default() -> Self {
        Self {
            density: 8.0,
            samples: 256,
            bounces: 3,
            seed: 1,
            max_atlas: 4096,
            max_pages: regia_render::lightmap::DEFAULT_LIGHTMAP_MAX_PAGES,
            direct_sun: false,
            no_filter: false,
            planar_charts: false,
            gpu: false,
            meshlets: false,
            denoise: true,
            encoding: LightmapTexelEncoding::Rgb9e5,
            crease: true,
            stitch: true,
            auto_density: true,
        }
    }
}

/// One bake to run.
#[derive(Clone, Debug, PartialEq)]
pub struct LightmapRequest {
    /// A level, `.level.ron` or `.level.d` (`--scene`), a `.bake.ron` or a built-in name.
    pub source: String,
    /// The project root, passed as `--project` for a level.
    pub project: Option<PathBuf>,
    /// The output lightmap (`None`: the CLI's default, `assets/maps/<level>.flmap` for a level).
    pub out: Option<PathBuf>,
    /// The settings.
    pub fields: LightmapFields,
    /// `--dry-run`: lay the atlas out, no tracing, nothing written.
    pub dry_run: bool,
}

/// Whether `source` is a level, which `regia bake-lightmaps` takes as `--scene`.
fn is_level(source: &str) -> bool {
    source.ends_with(".level.ron")
        || source
            .trim_end_matches(['/', '\\'])
            .ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX)
}

/// `--gpu-tile-rays` of a GPU bake started from the panel: about 1 M rays a dispatch, a quarter of
/// the CLI's default, so a frame of the editor waits at most one short tile.
pub const EDITOR_GPU_TILE_RAYS: &str = "1048576";
/// `--gpu-yield-ms` of a GPU bake started from the panel: the GPU is left free this long after
/// every tile.
pub const EDITOR_GPU_YIELD_MS: &str = "2";

impl LightmapRequest {
    /// The `regia` arguments.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["bake-lightmaps".into()];
        if is_level(&self.source) {
            args.push("--scene".into());
            args.push(self.source.clone().into());
            if let Some(project) = &self.project {
                args.push("--project".into());
                args.push(project.clone().into_os_string());
            }
        } else {
            args.push(self.source.clone().into());
        }
        if let Some(out) = &self.out {
            args.push("--out".into());
            args.push(out.clone().into_os_string());
        }
        let f = &self.fields;
        for (flag, value) in [
            ("--density", f.density.to_string()),
            ("--samples", f.samples.to_string()),
            ("--bounces", f.bounces.to_string()),
            ("--seed", f.seed.to_string()),
            ("--max-atlas", f.max_atlas.to_string()),
            ("--max-pages", f.max_pages.to_string()),
            ("--denoise", on_off(f.denoise).to_string()),
            ("--encoding", encoding_arg(f.encoding).to_string()),
            ("--crease", on_off(f.crease).to_string()),
            ("--stitch", on_off(f.stitch).to_string()),
            ("--auto-density", on_off(f.auto_density).to_string()),
        ] {
            args.push(flag.into());
            args.push(value.into());
        }
        if f.planar_charts {
            args.push("--unwrap".into());
            args.push("charts".into());
        }
        if f.direct_sun {
            args.push("--direct-sun".into());
        }
        if f.no_filter {
            args.push("--no-filter".into());
        }
        if f.gpu {
            // Short tiles and a pause between them, so the editor's own frames keep the GPU
            // (ADR-2125); the light baked is the same.
            for arg in [
                "--gpu",
                "--gpu-tile-rays",
                EDITOR_GPU_TILE_RAYS,
                "--gpu-yield-ms",
                EDITOR_GPU_YIELD_MS,
            ] {
                args.push(arg.into());
            }
        }
        if f.meshlets {
            args.push("--meshlets".into());
        }
        if self.dry_run {
            args.push("--dry-run".into());
        } else {
            // A bake from the panel checkpoints beside its output and continues a checkpoint left
            // by a bake that died or was cancelled (ADR-3095).
            args.push("--checkpoint".into());
        }
        args
    }

    /// The lightmap this request writes, as the CLI decides it: `out`, or for a level in a
    /// project `<project>/assets/maps/<level stem>.flmap`. Where its checkpoint lives (ADR-3095).
    /// Without a project the project is found the way the CLI finds it (`find_project`): the
    /// nearest ancestor holding a `Regia.toml`, else the directory above the nearest `assets`
    /// ancestor, else the level's directory — so a bake the CLI writes one is reported here too.
    #[must_use]
    pub fn written(&self) -> Option<PathBuf> {
        if let Some(out) = &self.out {
            return Some(out.clone());
        }
        if !is_level(&self.source) {
            return None;
        }
        let project = match &self.project {
            Some(project) => project.clone(),
            None => find_project_of(Path::new(&self.source)),
        };
        let stem = regia_agent::bake_export::file_stem_to_first_dot(&self.source);
        Some(project.join("assets").join("maps").join(format!("{stem}.flmap")))
    }

    /// What the shared job records for status lines.
    ///
    /// `out` is the file the CLI writes, including a level's default
    /// `assets/maps/<stem>.flmap`. Status reads the checkpoint beside that path. The process
    /// itself is started from [`Self::args`], which still omits `--out` when the caller left it
    /// empty.
    fn job_request(&self) -> BakeRequest {
        BakeRequest {
            scene: self.source.clone(),
            out: self.written(),
            overrides: bake_runner::BakeOverrides::default(),
            dry_run: self.dry_run,
        }
    }
}

/// A built-in stays a name; a relative path is resolved against the project.
#[must_use]
pub fn resolve_source(source: &str, project: Option<&Path>) -> String {
    if source == QUAD_BOX {
        return source.to_string();
    }
    bake_runner::resolve_scene(source, project)
}

/// Where the panel writes a bake by default: the CLI's own rule for a level
/// (`assets/maps/<level>.flmap`), `<dir>/<stem>.flmap` beside a description, and
/// `<project>/.regia/bake/<name>.flmap` for a built-in. `None` leaves it to the CLI.
#[must_use]
pub fn default_out(source: &str, project: Option<&Path>) -> Option<PathBuf> {
    if is_level(source) {
        return None;
    }
    if BUILTIN_SCENES.contains(&source) || source == QUAD_BOX {
        return project.map(|root| {
            regia_core::brand::project_state_dir(root)
                .join("bake")
                .join(format!("{source}.flmap"))
        });
    }
    let path = Path::new(source);
    let stem = path.file_stem()?.to_str()?.trim_end_matches(".bake");
    if stem.is_empty() {
        return None;
    }
    Some(path.with_file_name(format!("{stem}.flmap")))
}

/// The project root the CLI would find for `start` (a level path): the nearest ancestor
/// holding a `Regia.toml`, else the directory above the nearest `assets` ancestor, else
/// `start`'s directory. Lexical only, like the CLI's `find_project`.
fn find_project_of(start: &Path) -> PathBuf {
    let start = std::path::absolute(start).unwrap_or_else(|_| start.to_path_buf());
    let dir = if start.is_dir() {
        start.clone()
    } else {
        start.parent().map_or_else(|| start.clone(), Path::to_path_buf)
    };
    if let Some(found) = dir.ancestors().find(|a| regia_core::brand::is_project_root(a)) {
        return found.to_path_buf();
    }
    if let Some(assets) = dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "assets"))
        && let Some(parent) = assets.parent()
    {
        return parent.to_path_buf();
    }
    dir
}

/// The project's levels (`*.level.ron`), project-relative with `/` separators, sorted.
#[must_use]
pub fn find_levels(root: &Path) -> Vec<String> {
    bake_runner::find_with_suffix(root, ".level.ron")
}

/// One entry of the atlas.
#[derive(Clone, Debug, PartialEq)]
pub struct AtlasEntry {
    /// The entry's name in the `.flmap`.
    pub name: String,
    /// `name (guid)` of its entity, for a level's bake.
    pub label: Option<String>,
    /// `x, y, width, height` in texels.
    pub rect: [u32; 4],
    /// Texels per metre the entry got.
    pub texels_per_metre: f32,
    /// [`Self::texels_per_metre`] over the density asked for: below 1 the atlas fit lowered it, and
    /// a level's `LightmapSurface::density_scale` moves it either way.
    pub density_ratio: f32,
    /// Seam edges measured (a dry run measures none).
    pub seam_edges: u64,
    /// The seam step's 95th percentile, what the budget limits.
    pub seam_p95: Option<f32>,
    /// The largest seam step.
    pub seam_max: Option<f32>,
    /// The atlas page its rectangle is in (ADR-3300; 0 for a one-page atlas).
    pub page: u32,
}

/// One of the worst seam edges, where the bake measured it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasSeam {
    /// Atlas UV of one end.
    pub a: [f32; 2],
    /// Atlas UV of the other end.
    pub b: [f32; 2],
    /// World position of one end.
    pub world_a: [f32; 3],
    /// World position of the other end.
    pub world_b: [f32; 3],
    /// The step on this edge.
    pub error: f32,
    /// The page of the entry the edge is on (ADR-3300; 0 for a one-page atlas).
    pub page: u32,
}

/// What the panel draws: the atlas of the last dry run or bake.
#[derive(Clone, Debug, PartialEq)]
pub struct AtlasView {
    /// Atlas width, texels.
    pub width: u32,
    /// Atlas height, texels.
    pub height: u32,
    /// The density asked for, texels per metre.
    pub density: f32,
    /// The share of the requested density the atlas fit left.
    pub fit_scale: f32,
    /// Atlas pages the layout opened (ADR-3300; 1 for one atlas).
    pub pages: u32,
    /// A dry run: nothing baked, no seams.
    pub dry_run: bool,
    /// The entries, in file order.
    pub entries: Vec<AtlasEntry>,
    /// The worst seam edges, worst first.
    pub seams: Vec<AtlasSeam>,
}

fn f32_at(v: &Value, key: &str) -> Option<f32> {
    v.get(key).and_then(Value::as_f64).map(|x| x as f32)
}

fn u32_at(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .and_then(|x| u32::try_from(x).ok())
}

fn array_of<const N: usize>(v: Option<&Value>) -> Option<[f32; N]> {
    let list = v?.as_array()?;
    if list.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    for (slot, value) in out.iter_mut().zip(list) {
        *slot = value.as_f64()? as f32;
    }
    Some(out)
}

impl AtlasView {
    /// Reads the JSON `regia bake-lightmaps` prints. `None` when it does not have the expected
    /// shape (a CLI from another version).
    #[must_use]
    pub fn from_summary(summary: &Value) -> Option<Self> {
        let atlas = summary.get("atlas")?;
        let density = summary.get("settings").and_then(|s| f32_at(s, "density"))?;
        let seam_rows = summary
            .get("seams")
            .and_then(|s| s.get("entries"))
            .and_then(Value::as_array);
        let entries = summary
            .get("entries")?
            .as_array()?
            .iter()
            .map(|e| {
                let name = e.get("name")?.as_str()?.to_string();
                let rect = e.get("rect")?.as_array()?;
                let rect: [u32; 4] = [
                    u32::try_from(rect.first()?.as_u64()?).ok()?,
                    u32::try_from(rect.get(1)?.as_u64()?).ok()?,
                    u32::try_from(rect.get(2)?.as_u64()?).ok()?,
                    u32::try_from(rect.get(3)?.as_u64()?).ok()?,
                ];
                let texels_per_metre = f32_at(e, "texels_per_metre")?;
                let seam = seam_rows.and_then(|rows| {
                    rows.iter()
                        .find(|row| row.get("name").and_then(Value::as_str) == Some(name.as_str()))
                });
                Some(AtlasEntry {
                    label: e.get("label").and_then(Value::as_str).map(str::to_string),
                    rect,
                    texels_per_metre,
                    density_ratio: if density > 0.0 {
                        texels_per_metre / density
                    } else {
                        1.0
                    },
                    seam_edges: seam
                        .and_then(|s| s.get("edges"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    seam_p95: seam.and_then(|s| f32_at(s, "p95")),
                    seam_max: seam.and_then(|s| f32_at(s, "max")),
                    page: u32_at(e, "page").unwrap_or(0),
                    name,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let seams = summary
            .get("seams")
            .and_then(|s| s.get("overlay"))
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        Some(AtlasSeam {
                            a: array_of(row.get("a"))?,
                            b: array_of(row.get("b"))?,
                            world_a: array_of(row.get("world_a"))?,
                            world_b: array_of(row.get("world_b"))?,
                            error: f32_at(row, "error")?,
                            page: u32_at(row, "entry")
                                .and_then(|k| entries.get(k as usize))
                                .map_or(0, |e: &AtlasEntry| e.page),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            width: u32_at(atlas, "width")?,
            height: u32_at(atlas, "height")?,
            density,
            fit_scale: f32_at(atlas, "fit_scale").unwrap_or(1.0),
            pages: u32_at(atlas, "pages").unwrap_or(1).max(1),
            dry_run: summary.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
            entries,
            seams,
        })
    }

    /// The names of the entries past `budget`.
    #[must_use]
    pub fn over_budget(&self, budget: f32) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| e.seam_p95.is_some_and(|p| p > budget))
            .map(|e| e.name.as_str())
            .collect()
    }

    /// The view as `editor.lightmap_status` reports it, with the budget applied.
    #[must_use]
    pub fn to_json(&self, budget: f32) -> Value {
        json!({
            "width": self.width,
            "height": self.height,
            "density": self.density,
            "fit_scale": self.fit_scale,
            "pages": self.pages,
            "dry_run": self.dry_run,
            "entries": self.entries.iter().map(|e| json!({
                "name": e.name,
                "label": e.label,
                "page": e.page,
                "rect": e.rect,
                "texels_per_metre": e.texels_per_metre,
                "density_ratio": e.density_ratio,
                "seam_edges": e.seam_edges,
                "seam_p95": e.seam_p95,
                "seam_max": e.seam_max,
            })).collect::<Vec<_>>(),
            "seam_segments": self.seams.len(),
            "worst_seam": self.seams.first().map(|s| s.error),
            "over_budget": self.over_budget(budget),
        })
    }
}

/// The color of a density ratio: green at the density asked for, blue when the entry got less,
/// orange when it got more, saturated at a factor of 4.
#[must_use]
pub fn density_color(ratio: f32) -> egui::Color32 {
    let t = if ratio.is_finite() && ratio > 0.0 {
        (ratio.log2() / 2.0).clamp(-1.0, 1.0)
    } else {
        -1.0
    };
    let mix = |a: u8, b: u8, s: f32| (f32::from(a) + (f32::from(b) - f32::from(a)) * s) as u8;
    if t < 0.0 {
        egui::Color32::from_rgb(mix(90, 60, -t), mix(190, 90, -t), mix(110, 220, -t))
    } else {
        egui::Color32::from_rgb(mix(90, 240, t), mix(190, 150, t), mix(110, 60, t))
    }
}

/// The color of a seam step against `budget`: green at no step, yellow at half the budget, red at
/// or past it.
#[must_use]
pub fn seam_color(error: f32, budget: f32) -> egui::Color32 {
    let t = if budget > 0.0 {
        (error / budget).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let mix = |a: u8, b: u8, s: f32| (f32::from(a) + (f32::from(b) - f32::from(a)) * s) as u8;
    if t < 0.5 {
        let s = t * 2.0;
        egui::Color32::from_rgb(mix(110, 235, s), mix(200, 205, s), 90)
    } else {
        let s = (t - 0.5) * 2.0;
        egui::Color32::from_rgb(mix(235, 235, s), mix(205, 80, s), 90)
    }
}

/// Starts a bake: a child process in the editor, a fake in tests.
pub type LightmapLauncher = fn(&Path, &LightmapRequest, Option<&Path>) -> Result<BakeJob, BakeFailure>;

fn spawn_job(program: &Path, request: &LightmapRequest, cwd: Option<&Path>) -> Result<BakeJob, BakeFailure> {
    BakeJob::spawn_args(program, request.job_request(), &request.args(), cwd)
}

/// The Lightmaps panel's state (a world resource shared by the panel, the agent methods and the
/// viewport overlay).
#[derive(Resource)]
pub struct LightmapPanel {
    /// A project-relative level or description, a built-in, or empty for the open level.
    pub source: String,
    /// The output typed in the panel (`""`: [`default_out`]).
    pub out: String,
    /// The settings.
    pub fields: LightmapFields,
    /// The seam budget the preview colors against.
    pub seam_budget: f32,
    /// What the atlas preview colors by.
    pub preview: AtlasPreview,
    /// Draw the worst seams in the viewport.
    pub seam_overlay: bool,
    /// The project's levels and descriptions, once listed (`None`: list on the next draw).
    pub listed: Option<Vec<String>>,
    /// The CLI to run (`None`: [`bake_runner::cli_program`]).
    pub program: Option<PathBuf>,
    /// How bakes start.
    pub launcher: LightmapLauncher,
    /// Bakes started in this session.
    pub started: u64,
    job: Option<BakeJob>,
    start_error: Option<BakeFailure>,
    view: Option<AtlasView>,
    args: Vec<String>,
    /// Where the Bake button was drawn, for the test that clicks it.
    #[cfg(test)]
    bake_button: Option<egui::Pos2>,
}

impl Default for LightmapPanel {
    fn default() -> Self {
        Self {
            source: String::new(),
            out: String::new(),
            fields: LightmapFields::default(),
            seam_budget: DEFAULT_SEAM_BUDGET,
            preview: AtlasPreview::default(),
            seam_overlay: false,
            listed: None,
            program: None,
            launcher: spawn_job,
            started: 0,
            job: None,
            start_error: None,
            view: None,
            args: Vec::new(),
            #[cfg(test)]
            bake_button: None,
        }
    }
}

impl LightmapPanel {
    /// The current (or last) job's state; a failed start is a `Failed` state.
    #[must_use]
    pub fn state(&self) -> BakeState {
        match (&self.job, &self.start_error) {
            (_, Some(failure)) => BakeState::Failed(failure.clone()),
            (Some(job), None) => job.state().clone(),
            (None, None) => BakeState::Idle,
        }
    }

    /// Whether a bake is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.state().is_running())
    }

    /// The atlas of the last dry run or bake that finished.
    #[must_use]
    pub fn view(&self) -> Option<&AtlasView> {
        self.view.as_ref()
    }

    /// The request the panel's fields describe, resolved against `project`; an empty source is
    /// `open_level`.
    ///
    /// # Errors
    ///
    /// `lightmap_no_source`: nothing is selected and no saved level is open.
    pub fn request(
        &self,
        project: Option<&Path>,
        open_level: Option<&Path>,
        dry_run: bool,
    ) -> Result<LightmapRequest, BakeFailure> {
        let source = match (self.source.trim(), open_level) {
            ("", Some(level)) => level.display().to_string(),
            ("", None) => {
                return Err(failure(
                    "lightmap_no_source",
                    "save the level or pick a source first",
                ));
            }
            (source, _) => resolve_source(source, project),
        };
        let out = if self.out.trim().is_empty() {
            default_out(&source, project)
        } else {
            let out = Path::new(self.out.trim());
            Some(match project {
                Some(root) if out.is_relative() => root.join(out),
                _ => out.to_path_buf(),
            })
        };
        Ok(LightmapRequest {
            source,
            project: project.map(Path::to_path_buf),
            out: if dry_run { None } else { out },
            fields: self.fields.clone(),
            dry_run,
        })
    }

    /// Starts `request` with the CLI in `project` (its working directory). Returns the job number.
    ///
    /// # Errors
    ///
    /// `lightmap_busy` while a bake runs, `bake_out_dir` when the output folder cannot be made,
    /// [`SPAWN_FAILED`] when the CLI does not start. A failed start also shows as the state.
    pub fn start(&mut self, request: &LightmapRequest, project: Option<&Path>) -> Result<u64, BakeFailure> {
        self.poll();
        if self.is_running() {
            return Err(failure(
                "lightmap_busy",
                "a lightmap bake is running: wait for it or cancel it first",
            ));
        }
        self.job = None;
        self.start_error = None;
        self.view = None;
        self.started += 1;
        if let Some(dir) = request.out.as_deref().and_then(Path::parent)
            && !dir.as_os_str().is_empty()
            && let Err(err) = std::fs::create_dir_all(dir)
        {
            let e = failure("bake_out_dir", format!("cannot create {}: {err}", dir.display()));
            self.start_error = Some(e.clone());
            return Err(e);
        }
        let program = self.program.clone().unwrap_or_else(bake_runner::cli_program);
        self.args = request
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        tracing::info!(target: "regia_editor", "lightmap bake {}: {} {:?}", self.started, program.display(), self.args);
        match (self.launcher)(&program, request, project) {
            Ok(job) => {
                self.job = Some(job);
                Ok(self.started)
            }
            Err(e) => {
                self.start_error = Some(e.clone());
                Err(e)
            }
        }
    }

    fn after_poll(&mut self) -> BakeState {
        let state = self.state();
        if let BakeState::Succeeded(summary) = &state
            && self.view.is_none()
        {
            self.view = AtlasView::from_summary(&summary.json);
        }
        state
    }

    /// Drains the running job's output (no blocking) and returns the state.
    pub fn poll(&mut self) -> BakeState {
        if let Some(job) = self.job.as_mut() {
            job.poll();
        }
        self.after_poll()
    }

    /// Blocks until the running job finishes and returns the state (dry runs only: they do not
    /// trace).
    pub fn wait(&mut self) -> BakeState {
        if let Some(job) = self.job.as_mut() {
            job.wait();
        }
        self.after_poll()
    }

    /// Cancels the running bake. Returns whether one was running.
    pub fn cancel(&mut self) -> bool {
        self.job.as_mut().is_some_and(BakeJob::cancel)
    }

    /// What `editor.lightmap_status` returns.
    #[must_use]
    pub fn status_json(&self) -> Value {
        let state = self.state();
        let (summary, error) = match &state {
            BakeState::Succeeded(summary) => (summary.json.clone(), Value::Null),
            BakeState::Failed(failure) => (Value::Null, failure.to_json()),
            _ => (Value::Null, Value::Null),
        };
        let job = self.job.as_ref();
        json!({
            "job": self.started,
            "state": state.name(),
            "running": state.is_running(),
            "source": job.map(|job| job.request().scene.clone()),
            "dry_run": job.map(|job| job.request().dry_run),
            "args": self.args,
            "progress": job.and_then(BakeJob::progress).map(BakeProgress::to_json),
            "summary": summary,
            "error": error,
            "log": job.map(BakeJob::log).unwrap_or_default(),
            "view": self.view.as_ref().map(|v| v.to_json(self.seam_budget)),
            "preview": self.preview.name(),
            "seam_budget": self.seam_budget,
            "seam_overlay": self.seam_overlay,
            "overlay_segments": self.overlay_segments().len(),
            // A resumable checkpoint of the last bake's output (ADR-3095).
            "checkpoint": job
                .and_then(|job| job.request().out.as_deref())
                .map(regia_asset::bake_checkpoint::checkpoint_status),
        })
    }

    /// The checkpoint of the bake the panel would start (ADR-3095): `None` when it has no file.
    #[must_use]
    pub fn pending_checkpoint(
        &self,
        project: Option<&Path>,
        open_level: Option<&Path>,
    ) -> Option<regia_asset::bake_checkpoint::BakeCheckpointStatus> {
        // Drawn every frame: a stat, and a 112-byte header read when a checkpoint is there.
        let _span = tracing::trace_span!("lightmap_panel_checkpoint").entered();
        let out = self.request(project, open_level, false).ok()?.written()?;
        let status = regia_asset::bake_checkpoint::checkpoint_status(&out);
        status.exists.then_some(status)
    }

    /// The seam lines the viewport draws: the worst seams while the overlay is on.
    #[must_use]
    pub fn overlay_segments(&self) -> Vec<Segment> {
        if !self.seam_overlay {
            return Vec::new();
        }
        let Some(view) = &self.view else {
            return Vec::new();
        };
        view.seams
            .iter()
            .map(|s| {
                (
                    Vec3::from_array(s.world_a),
                    Vec3::from_array(s.world_b),
                    seam_color(s.error, self.seam_budget),
                )
            })
            .collect()
    }
}

fn failure(code: &str, message: impl Into<String>) -> BakeFailure {
    BakeFailure {
        code: code.to_string(),
        path: None,
        message: message.into(),
    }
}

/// The seam lines of the Lightmaps panel for the viewport overlay.
#[must_use]
pub fn lightmap_gizmo_segments(world: &World) -> Vec<Segment> {
    world
        .get_resource::<LightmapPanel>()
        .map_or_else(Vec::new, LightmapPanel::overlay_segments)
}

// ---------------------------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------------------------

const GOOD: egui::Color32 = egui::Color32::from_rgb(110, 200, 120);
const BAD: egui::Color32 = egui::Color32::from_rgb(230, 100, 90);
/// Height of the atlas preview, points.
const PREVIEW_HEIGHT: f32 = 220.0;

fn open_level_path(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|level| level.path.clone())
}

/// The panel's checkpoint Discard as a command (`editor.command { run: "lightmap.discard_checkpoint" }`,
/// ADR-7307): deletes the checkpoint of the bake the panel would start, so the next bake starts over.
/// Returns the status line.
pub fn discard_checkpoint(world: &mut World) -> String {
    let project = crate::mesh_source::project_root(world);
    let open_level = open_level_path(world);
    let panel = world.get_resource_or_insert_with(LightmapPanel::default);
    discard_checkpoint_of(&panel, project.as_deref(), open_level.as_deref())
}

fn discard_checkpoint_of(panel: &LightmapPanel, project: Option<&Path>, open_level: Option<&Path>) -> String {
    match panel.pending_checkpoint(project, open_level) {
        Some(status) => match std::fs::remove_file(&status.path) {
            Ok(()) => format!("Discarded the checkpoint {}", status.path),
            Err(err) => format!("Could not discard {}: {err}", status.path),
        },
        None => "No bake checkpoint to discard".to_owned(),
    }
}

/// Draws the Lightmaps panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "lightmap_panel")]
pub fn lightmap_panel(ui: &mut egui::Ui, world: &mut World) {
    let project = crate::mesh_source::project_root(world);
    let open_level = open_level_path(world);
    let mut panel = world.remove_resource::<LightmapPanel>().unwrap_or_default();
    panel.poll();
    draw(ui, &mut panel, project.as_deref(), open_level.as_deref());
    if panel.is_running() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }
    world.insert_resource(panel);
}

fn draw(ui: &mut egui::Ui, panel: &mut LightmapPanel, project: Option<&Path>, open_level: Option<&Path>) {
    if panel.listed.is_none() {
        panel.listed = Some(
            project
                .map(|root| {
                    let mut all = find_levels(root);
                    all.extend(bake_runner::find_bake_descriptions(root));
                    all
                })
                .unwrap_or_default(),
        );
    }
    let running = panel.is_running();
    ui.horizontal(|ui| {
        ui.strong("Lightmaps");
        ui.weak("static meshes · regia bake-lightmaps");
        if ui.small_button("Refresh").clicked() {
            panel.listed = None;
        }
    });
    ui.separator();
    ui.add_enabled_ui(!running, |ui| draw_settings(ui, panel, project, open_level));
    ui.horizontal(|ui| {
        let validate = ui
            .add_enabled(!running, egui::Button::new("Lay out"))
            .on_hover_text("Dry run: lay the atlas out and preview the density, nothing is baked");
        let bake = ui.add_enabled(!running, egui::Button::new("Bake"));
        let cancel = ui.add_enabled(running, egui::Button::new("Cancel"));
        #[cfg(test)]
        {
            panel.bake_button = Some(bake.rect.center());
        }
        if validate.clicked() || bake.clicked() {
            match panel.request(project, open_level, validate.clicked()) {
                Ok(request) => {
                    let _ = panel.start(&request, project);
                }
                Err(e) => panel.start_error = Some(e),
            }
        }
        if cancel.clicked() {
            panel.cancel();
        }
    });
    ui.separator();
    draw_state(ui, panel);
    if !running && let Some(status) = panel.pending_checkpoint(project, open_level) {
        ui.horizontal(|ui| {
            ui.weak(status.line().unwrap_or_default());
            if ui
                .small_button("Discard")
                .on_hover_text("Delete the checkpoint: the next bake starts over")
                .clicked()
            {
                let _ = std::fs::remove_file(&status.path);
            }
        });
    }
    if panel.view.is_some() {
        ui.separator();
        draw_view(ui, panel);
    }
}

fn draw_settings(
    ui: &mut egui::Ui,
    panel: &mut LightmapPanel,
    project: Option<&Path>,
    open_level: Option<&Path>,
) {
    egui::Grid::new("lightmap-settings")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Source");
            let shown = if panel.source.is_empty() {
                "the open level".to_string()
            } else {
                panel.source.clone()
            };
            egui::ComboBox::from_id_salt("lightmap-source")
                .selected_text(shown)
                .width(260.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut panel.source, String::new(), "the open level");
                    for rel in panel.listed.clone().unwrap_or_default() {
                        ui.selectable_value(&mut panel.source, rel.clone(), rel);
                    }
                    for name in BUILTIN_SCENES.into_iter().chain([QUAD_BOX]) {
                        ui.selectable_value(
                            &mut panel.source,
                            name.to_string(),
                            format!("{name} (built-in)"),
                        );
                    }
                });
            ui.end_row();
            ui.label("Output");
            let hint = panel.request(project, open_level, false).ok().map_or_else(
                || "assets/maps/<level>.flmap".to_string(),
                |r| {
                    r.out.map_or_else(
                        || "assets/maps/<level>.flmap".to_string(),
                        |o| o.display().to_string(),
                    )
                },
            );
            ui.add(
                egui::TextEdit::singleline(&mut panel.out)
                    .hint_text(hint)
                    .desired_width(260.0),
            );
            ui.end_row();
            let f = &mut panel.fields;
            ui.label("Texels per metre");
            ui.add(
                egui::DragValue::new(&mut f.density)
                    .range(0.01..=1024.0)
                    .speed(0.25),
            );
            ui.end_row();
            ui.label("Samples per texel");
            ui.add(egui::DragValue::new(&mut f.samples).range(1..=65536));
            ui.end_row();
            ui.label("Bounces");
            ui.add(egui::DragValue::new(&mut f.bounces).range(1..=64));
            ui.end_row();
            ui.label("Largest atlas side");
            ui.add(egui::DragValue::new(&mut f.max_atlas).range(16..=8192));
            ui.end_row();
            ui.label("Atlas pages at most");
            ui.add(
                egui::DragValue::new(&mut f.max_pages).range(1..=regia_asset::lightmap::MAX_LIGHTMAP_PAGES),
            );
            ui.end_row();
        });
    ui.horizontal(|ui| {
        let f = &mut panel.fields;
        ui.checkbox(&mut f.direct_sun, "Bake the direct sun")
            .on_hover_text("Fully static sun light: a receiver then keeps only the live sun's specular");
        ui.checkbox(&mut f.no_filter, "Skip the filter");
        ui.checkbox(&mut f.planar_charts, "Planar charts");
        ui.checkbox(&mut f.gpu, "On the GPU")
            .on_hover_text("Needs a CLI built with the lightmap-gpu feature");
        ui.checkbox(&mut f.meshlets, "Visibility clusters").on_hover_text(
            "Store clusters over the unwrap, so receivers can draw through the visibility buffer",
        );
    });
    ui.horizontal(|ui| {
        let f = &mut panel.fields;
        ui.checkbox(&mut f.denoise, "Denoise")
            .on_hover_text("Edge-avoiding à-trous denoiser within each chart (replaces the filter)");
        ui.checkbox(&mut f.crease, "Crease placement").on_hover_text(
            "Texels under a wall or a slab sample the light at the joint instead of being refilled",
        );
        ui.checkbox(&mut f.stitch, "Stitch seams")
            .on_hover_text("Make both sides of every smooth chart seam read the same light");
        ui.checkbox(&mut f.auto_density, "Auto density")
            .on_hover_text("Raise the density of small curved receivers whose seams would pass the budget");
        ui.label("Encoding");
        egui::ComboBox::from_id_salt("lightmap-encoding")
            .selected_text(f.encoding.name())
            .show_ui(ui, |ui| {
                for e in LightmapTexelEncoding::ALL {
                    ui.selectable_value(&mut f.encoding, e, e.name());
                }
            });
    });
}

fn draw_state(ui: &mut egui::Ui, panel: &LightmapPanel) {
    match panel.state() {
        BakeState::Idle => {
            ui.weak("Lay out to preview the atlas, then Bake.");
        }
        BakeState::Running(Some(progress)) => {
            ui.add(egui::ProgressBar::new(progress.fraction()).text(format!(
                "{}: {}/{} texels",
                progress.stage, progress.done, progress.total
            )));
        }
        BakeState::Running(None) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Starting the bake…");
            });
        }
        BakeState::Succeeded(summary) => {
            let text = if summary.dry_run {
                "Atlas laid out".to_string()
            } else {
                match &summary.out {
                    Some(out) => format!("Bake finished in {:.1} s: {out}", summary.seconds),
                    None => format!("Bake finished in {:.1} s", summary.seconds),
                }
            };
            ui.colored_label(GOOD, text);
        }
        BakeState::Failed(e) => {
            ui.colored_label(BAD, format!("Bake failed ({})", e.code));
            if let Some(path) = &e.path {
                ui.label(format!("at {path}"));
            }
            ui.label(&e.message);
        }
        BakeState::Cancelled => {
            ui.weak("Cancelled.");
        }
    }
}

/// Screen position of atlas UV `uv` in `rect`.
fn atlas_point(rect: egui::Rect, uv: [f32; 2]) -> egui::Pos2 {
    rect.min + egui::vec2(uv[0] * rect.width(), uv[1] * rect.height())
}

/// The preview's shapes: every entry's rectangle by the preview's color, then, for the seam
/// preview, the worst seams as lines.
fn atlas_shapes(view: &AtlasView, rect: egui::Rect, preview: AtlasPreview, budget: f32) -> Vec<egui::Shape> {
    // The pages stacked top to bottom (ADR-3300).
    let pages = view.pages.max(1) as f32;
    let (w, h) = (view.width.max(1) as f32, view.height.max(1) as f32 * pages);
    // A page past the summary's page count (a broken CLI summary) draws on the last page,
    // inside the preview, instead of below it.
    let last_page = view.pages.max(1) - 1;
    let page_v = |page: u32, v: f32| (page.min(last_page) as f32 + v) / pages;
    let mut shapes = vec![egui::Shape::rect_filled(rect, 0.0, egui::Color32::from_gray(24))];
    for e in &view.entries {
        let [x, y, ew, eh] = e.rect.map(|v| v as f32);
        let y = y + e.page.min(last_page) as f32 * view.height as f32;
        let area = egui::Rect::from_min_max(
            atlas_point(rect, [x / w, y / h]),
            atlas_point(rect, [(x + ew) / w, (y + eh) / h]),
        );
        let fill = match preview {
            AtlasPreview::Density => density_color(e.density_ratio),
            AtlasPreview::Seams => e
                .seam_p95
                .map_or(egui::Color32::from_gray(70), |p| seam_color(p, budget)),
        };
        shapes.push(egui::Shape::rect_filled(area, 0.0, fill.gamma_multiply(0.6)));
    }
    if preview == AtlasPreview::Seams {
        for s in &view.seams {
            shapes.push(egui::Shape::line_segment(
                [
                    atlas_point(rect, [s.a[0], page_v(s.page, s.a[1])]),
                    atlas_point(rect, [s.b[0], page_v(s.page, s.b[1])]),
                ],
                egui::Stroke::new(1.5, seam_color(s.error, budget)),
            ));
        }
    }
    shapes
}

fn draw_view(ui: &mut egui::Ui, panel: &mut LightmapPanel) {
    let Some(view) = panel.view.clone() else {
        return;
    };
    ui.horizontal(|ui| {
        let pages = if view.pages > 1 {
            format!(" × {} pages", view.pages)
        } else {
            String::new()
        };
        ui.label(format!(
            "Atlas {}×{}{pages}, {} entries",
            view.width,
            view.height,
            view.entries.len()
        ));
        if view.fit_scale < 1.0 {
            ui.colored_label(
                BAD,
                format!(
                    "density lowered to {:.0} %: raise atlas or split level",
                    view.fit_scale * 100.0
                ),
            );
        }
    });
    ui.horizontal(|ui| {
        ui.selectable_value(&mut panel.preview, AtlasPreview::Density, "Density");
        ui.selectable_value(&mut panel.preview, AtlasPreview::Seams, "Seams");
        ui.checkbox(&mut panel.seam_overlay, "Seams in the viewport");
        ui.label("Budget");
        ui.add(
            egui::DragValue::new(&mut panel.seam_budget)
                .range(0.0..=100.0)
                .speed(0.01),
        );
    });
    let width = ui.available_width().max(1.0);
    // In f32: a `u32` product overflows (a debug panic) for a tall atlas with many pages.
    let aspect = view.height.max(1) as f32 * view.pages.max(1) as f32 / view.width.max(1) as f32;
    let size = egui::vec2(width, (width * aspect).min(PREVIEW_HEIGHT));
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter()
        .extend(atlas_shapes(&view, rect, panel.preview, panel.seam_budget));
    let over = view.over_budget(panel.seam_budget);
    if !over.is_empty() {
        ui.colored_label(
            BAD,
            format!(
                "{} entries pass the seam budget: re-bake with more samples",
                over.len()
            ),
        );
    }
    egui::CollapsingHeader::new("Entries")
        .id_salt("lightmap-entries")
        .show(ui, |ui| {
            // Only the entries in view are laid out (ADR-7306): one per lightmapped mesh.
            let row_height = ui.spacing().interact_size.y;
            egui::ScrollArea::vertical()
                .id_salt("lightmap-entry-rows")
                .max_height(320.0)
                .show_rows(ui, row_height, view.entries.len() + 1, |ui, range| {
                    egui::Grid::new("lightmap-entry-grid")
                        .num_columns(4)
                        .striped(true)
                        .show(ui, |ui| {
                            if range.start == 0 {
                                ui.strong("Entry");
                                ui.strong("Texels/m");
                                ui.strong("Seam p95");
                                ui.strong("Seam max");
                                ui.end_row();
                            }
                            let entries = range.start.saturating_sub(1)..range.end.saturating_sub(1);
                            for e in &view.entries[entries] {
                                ui.label(e.label.as_deref().unwrap_or(&e.name));
                                ui.colored_label(
                                    density_color(e.density_ratio),
                                    format!("{:.2}", e.texels_per_metre),
                                );
                                let number =
                                    |v: Option<f32>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.3}"));
                                ui.label(number(e.seam_p95));
                                ui.label(number(e.seam_max));
                                ui.end_row();
                            }
                        });
                });
        });
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn f32_param(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .map(|v| Some(v as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number"))),
    }
}

fn apply_fields(fields: &mut LightmapFields, params: &Value) -> Result<(), RpcError> {
    if let Some(density) = f32_param(params, "density")? {
        if !(density.is_finite() && density > 0.0) {
            return Err(RpcError::invalid_params("`density` must be a number above 0"));
        }
        fields.density = density;
    }
    for (name, slot) in [
        ("samples", &mut fields.samples),
        ("bounces", &mut fields.bounces),
        ("seed", &mut fields.seed),
        ("max_atlas", &mut fields.max_atlas),
        ("max_pages", &mut fields.max_pages),
    ] {
        if let Some(value) = bake_runner::u32_param(params, name)? {
            *slot = value;
        }
    }
    if !(1..=regia_asset::lightmap::MAX_LIGHTMAP_PAGES).contains(&fields.max_pages) {
        return Err(RpcError::invalid_params(format!(
            "`max_pages` must be 1..={}",
            regia_asset::lightmap::MAX_LIGHTMAP_PAGES
        )));
    }
    fields.direct_sun = bake_runner::bool_param(params, "direct_sun");
    fields.no_filter = bake_runner::bool_param(params, "no_filter");
    fields.gpu = bake_runner::bool_param(params, "gpu");
    fields.meshlets = bake_runner::bool_param(params, "meshlets");
    // On for new bakes: an absent switch keeps the default, `false` turns it off.
    for (name, slot) in [
        ("denoise", &mut fields.denoise),
        ("crease", &mut fields.crease),
        ("stitch", &mut fields.stitch),
        ("auto_density", &mut fields.auto_density),
    ] {
        *slot = match params.get(name) {
            None | Some(Value::Null) => true,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean")))?,
        };
    }
    fields.encoding = match params.get("encoding") {
        None | Some(Value::Null) => LightmapTexelEncoding::Rgb9e5,
        Some(v) => {
            let name = v.as_str().unwrap_or_default();
            LightmapTexelEncoding::from_name(name).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`encoding` must be `rgb9e5`, `bc6h` or `astc_hdr`, not {v}"
                ))
            })?
        }
    };
    fields.planar_charts = match str_param(params, "unwrap") {
        None | Some("atlas") => false,
        Some("charts") => true,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`unwrap` must be `atlas` or `charts`, not `{other}`"
            )));
        }
    };
    Ok(())
}

/// `editor.lightmap_bake`.
fn lightmap_bake_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let source = str_param(params, "source").unwrap_or("").trim();
    // Handed to the CLI as an argument: never as a flag (ADR-1816).
    if source.starts_with('-') {
        return Err(RpcError::invalid_params("`source` must not start with `-`"));
    }
    // The default output sits beside a description, so a source under the allowed roots keeps the
    // bake's file there too (ADR-1816). The open level and a built-in name no path of the agent's.
    if !source.is_empty() && !BUILTIN_SCENES.contains(&source) && source != QUAD_BOX {
        crate::agent_ext::document_request_path(world, "source", source)?;
    }
    let dry_run = bake_runner::bool_param(params, "dry_run");
    let wait = bake_runner::bool_param(params, "wait");
    if wait && !dry_run {
        return Err(bake_runner::bake_error(
            regia_agent::protocol::INVALID_PARAMS,
            &failure(
                "lightmap_wait_needs_dry_run",
                "`wait` blocks the editor, so only a dry run may wait",
            ),
            "start the bake without `wait` and poll editor.lightmap_status until running is false",
        ));
    }
    let out = match str_param(params, "out").filter(|out| !out.is_empty()) {
        // Under the agent's allowed roots only (ADR-1816).
        Some(out) => Some(crate::agent_ext::allowed_request_path(world, "out", out)?),
        None => None,
    };
    let project = crate::mesh_source::project_root(world);
    let open_level = open_level_path(world);
    let mut panel = world.remove_resource::<LightmapPanel>().unwrap_or_default();
    let mut fields = panel.fields.clone();
    // A refused call changes nothing the panel's next Bake would use.
    let before = (panel.source.clone(), panel.fields.clone());
    let reply = apply_fields(&mut fields, params).and_then(|()| {
        panel.source = source.to_string();
        panel.fields = fields;
        let mut request = panel
            .request(project.as_deref(), open_level.as_deref(), dry_run)
            .map_err(|e| {
                bake_runner::bake_error(
                    regia_agent::protocol::INVALID_PARAMS,
                    &e,
                    "name a `source`, or save the open level first",
                )
            })?;
        if let Some(out) = out
            && !dry_run
        {
            request.out = Some(out);
        }
        panel.start(&request, project.as_deref()).map_err(|e| {
            let (code, hint) = match e.code.as_str() {
                "lightmap_busy" => (
                    regia_agent::protocol::INVALID_REQUEST,
                    "poll editor.lightmap_status until running is false, or call editor.lightmap_cancel",
                ),
                SPAWN_FAILED => (
                    regia_agent::protocol::UNSUPPORTED,
                    "build the CLI (cargo build -p regia_cli) or set REGIA_CLI to its path",
                ),
                _ => (
                    regia_agent::protocol::INTERNAL_ERROR,
                    "check the output path and the editor log",
                ),
            };
            bake_runner::bake_error(code, &e, hint)
        })?;
        if wait {
            panel.wait();
        } else {
            panel.poll();
        }
        Ok(panel.status_json())
    });
    if reply.is_err() {
        (panel.source, panel.fields) = before;
    }
    world.insert_resource(panel);
    reply
}

/// `editor.lightmap_status`.
fn lightmap_status_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.get_resource_or_insert_with(LightmapPanel::default);
    panel.poll();
    let mut status = panel.status_json();
    if bake_runner::bool_param(params, "list") {
        status["builtins"] = json!(BUILTIN_SCENES.into_iter().chain([QUAD_BOX]).collect::<Vec<_>>());
        status["levels"] = json!(project.as_deref().map(find_levels).unwrap_or_default());
        status["descriptions"] = json!(
            project
                .as_deref()
                .map(bake_runner::find_bake_descriptions)
                .unwrap_or_default()
        );
    }
    Ok(status)
}

/// `editor.lightmap_cancel`.
fn lightmap_cancel_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.get_resource_or_insert_with(LightmapPanel::default);
    panel.poll();
    let cancelled = panel.cancel();
    let mut status = panel.status_json();
    status["cancelled"] = json!(cancelled);
    Ok(status)
}

/// `editor.lightmap_preview`.
fn lightmap_preview_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let preview = match str_param(params, "preview") {
        None => None,
        Some(name) => Some(AtlasPreview::parse(name).ok_or_else(|| {
            RpcError::invalid_params(format!("`preview` must be `density` or `seams`, not `{name}`"))
        })?),
    };
    let budget = f32_param(params, "seam_budget")?;
    if let Some(b) = budget
        && !(b.is_finite() && b >= 0.0)
    {
        return Err(RpcError::invalid_params("`seam_budget` must be a number >= 0"));
    }
    let overlay = match params.get("seam_overlay") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`seam_overlay` must be a boolean"))?,
        ),
    };
    let mut panel = world.get_resource_or_insert_with(LightmapPanel::default);
    if let Some(preview) = preview {
        panel.preview = preview;
    }
    if let Some(budget) = budget {
        panel.seam_budget = budget;
    }
    if let Some(overlay) = overlay {
        panel.seam_overlay = overlay;
    }
    panel.poll();
    Ok(panel.status_json())
}

const FIELD_PARAMS: [ParamSchema; 20] = [
    ParamSchema {
        name: "source",
        kind: "string",
        required: false,
        description: "A `.level.ron`, a `.bake.ron` (absolute or project-relative) or a built-in: `cornell`, `furnace`, `rolling-hills`, `quad-box`. Default: the open level.",
    },
    ParamSchema {
        name: "out",
        kind: "string",
        required: false,
        description: "The `.flmap` to write, under the allowed roots. Default: `assets/maps/<level>.flmap` for a level, beside a description, `.regia/bake/<name>.flmap` for a built-in.",
    },
    ParamSchema {
        name: "dry_run",
        kind: "boolean",
        required: false,
        description: "Lay the atlas out only (no tracing, nothing written). Default false.",
    },
    ParamSchema {
        name: "wait",
        kind: "boolean",
        required: false,
        description: "Block until the dry run finishes. Dry runs only. Default false.",
    },
    ParamSchema {
        name: "density",
        kind: "number",
        required: false,
        description: "Texels per metre (`--density`).",
    },
    ParamSchema {
        name: "samples",
        kind: "number",
        required: false,
        description: "Hemisphere samples per texel (`--samples`, 1..=65536).",
    },
    ParamSchema {
        name: "bounces",
        kind: "number",
        required: false,
        description: "Path vertices after the texel (`--bounces`, at most 64).",
    },
    ParamSchema {
        name: "seed",
        kind: "number",
        required: false,
        description: "Random seed (`--seed`).",
    },
    ParamSchema {
        name: "max_atlas",
        kind: "number",
        required: false,
        description: "Largest atlas side, texels (`--max-atlas`, 16..=8192).",
    },
    ParamSchema {
        name: "max_pages",
        kind: "number",
        required: false,
        description: "Atlas pages the layout may open before it lowers the density (`--max-pages`, 1..=64, default 4 when the frame binds texture arrays, else 1; ADR-3300).",
    },
    ParamSchema {
        name: "direct_sun",
        kind: "boolean",
        required: false,
        description: "Bake the sun seen directly (`--direct-sun`). Default false.",
    },
    ParamSchema {
        name: "no_filter",
        kind: "boolean",
        required: false,
        description: "Skip the edge-aware filter (`--no-filter`). Default false.",
    },
    ParamSchema {
        name: "gpu",
        kind: "boolean",
        required: false,
        description: "Bake on the GPU (`--gpu` in short tiles with a pause after each, `--gpu-tile-rays 1048576 --gpu-yield-ms 2`, so the editor keeps drawing; a CLI built with the `lightmap-gpu` feature, else `lightmap_gpu_unavailable`). Default false.",
    },
    ParamSchema {
        name: "meshlets",
        kind: "boolean",
        required: false,
        description: "Store clusters over each entry's unwrap for the visibility buffer (`--meshlets`). Default false.",
    },
    ParamSchema {
        name: "unwrap",
        kind: "string",
        required: false,
        description: "`atlas` (default) or `charts` (`--unwrap`).",
    },
    ParamSchema {
        name: "denoise",
        kind: "boolean",
        required: false,
        description: "The edge-avoiding à-trous denoiser (`--denoise on|off`, ADR-2314). Default true.",
    },
    ParamSchema {
        name: "encoding",
        kind: "string",
        required: false,
        description: "How the `.flmap` stores its texels (`--encoding`, ADR-2314): `rgb9e5` (default), `bc6h` or `astc_hdr`.",
    },
    ParamSchema {
        name: "crease",
        kind: "boolean",
        required: false,
        description: "Crease-aware texel placement (`--crease on|off`, ADR-2404): texels under a wall or slab sample the light at the joint. Default true.",
    },
    ParamSchema {
        name: "stitch",
        kind: "boolean",
        required: false,
        description: "Seam stitching (`--stitch on|off`, ADR-2404): both sides of every smooth chart seam read the same light. Default true.",
    },
    ParamSchema {
        name: "auto_density",
        kind: "boolean",
        required: false,
        description: "Automatic density (`--auto-density on|off`, ADR-2404): small curved receivers whose predicted seams pass the budget get a higher density; the result's `summary.density.warnings` lists `lightmap_density_low`. Default true.",
    },
];

/// The Lightmaps panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.lightmap_bake",
                tool: "lightmap_bake",
                description: "Start a lightmap bake in the Lightmaps panel: runs `regia bake-lightmaps` on a level \
                    (its GiMobility::Static meshes, entries keyed by entity), a .bake.ron or a built-in in a \
                    child process and returns the status (editor.lightmap_status's shape). `dry_run: true` lays \
                    the atlas out without tracing; only a dry run may `wait`. One bake at a time (data.bake_error \
                    `lightmap_busy`).",
                params: &FIELD_PARAMS,
                result: "{ job, state, running, source, dry_run, args, progress, summary, error, log, view, preview, seam_budget, seam_overlay, overlay_segments, checkpoint }",
                mutating: true,
                permission: Permission::Assets,
            },
            lightmap_bake_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.lightmap_status",
                tool: "lightmap_status",
                description: "The Lightmaps panel's current or last bake: state (`idle`, `running`, `succeeded`, \
                    `failed`, `cancelled`), progress {stage, done, total, fraction, remaining, eta_seconds}, the CLI's summary or the \
                    error {code, path, message}, and `view`, what the panel draws: the atlas with each entry's \
                    rect, texels_per_metre, density_ratio and seam figures, the worst seam and the entries \
                    over_budget. `list: true` adds the built-ins and the project's levels and descriptions.",
                params: &[ParamSchema {
                    name: "list",
                    kind: "boolean",
                    required: false,
                    description: "Also list `builtins`, `levels` and `descriptions` (project-relative). Default false.",
                }],
                result: "{ job, state, running, source, dry_run, args, progress, summary, error, log, view, preview, seam_budget, seam_overlay, overlay_segments, checkpoint, builtins?, levels?, descriptions? }",
                mutating: false,
                permission: Permission::Read,
            },
            lightmap_status_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.lightmap_cancel",
                tool: "lightmap_cancel",
                description: "Kill the running lightmap bake (the output file is not replaced). `cancelled` is false \
                    when no bake was running.",
                params: &[],
                result: "{ cancelled, job, state, ... }",
                mutating: true,
                permission: Permission::Assets,
            },
            lightmap_cancel_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.lightmap_preview",
                tool: "lightmap_preview",
                description: "Set what the Lightmaps panel shows: the atlas colored by `density` or `seams`, the budget \
                    the seams are held to, and whether the worst seams are drawn in the viewport. Absent values \
                    stay; every value is checked before any changes. Returns the status.",
                params: &[
                    ParamSchema {
                        name: "preview",
                        kind: "string",
                        required: false,
                        description: "`density` or `seams`.",
                    },
                    ParamSchema {
                        name: "seam_budget",
                        kind: "number",
                        required: false,
                        description: "The seam `p95` the preview and `view.over_budget` hold entries to (>= 0).",
                    },
                    ParamSchema {
                        name: "seam_overlay",
                        kind: "boolean",
                        required: false,
                        description: "Draw the worst seams in the viewport.",
                    },
                ],
                result: "{ job, state, ..., view, preview, seam_budget, seam_overlay, overlay_segments, checkpoint }",
                mutating: true,
                permission: Permission::Read,
            },
            lightmap_preview_method as Handler,
        ),
    ]
}

/// Puts the open level's lightmap into the edit world (ADR-2635): the framework's
/// `LightmapLevelPlugin` installs the level's `look.lightmap` and gives every keyed mesh part a
/// `LightmapReceiver`, as in a game; [`crate::mesh_source::resolve_mesh_sources`] loads those
/// entities in the cook's part order, which the bake keyed them by; the viewport hands receivers
/// to the renderer before extraction ([`apply_level_lightmap_receivers`]). An open level that names
/// no lightmap changes nothing.
pub fn install_level_lightmaps(app: &mut regia_app::prelude::App) {
    app.add_plugins(regia_framework::lightmap_level::LightmapLevelPlugin)
        .init_resource::<regia_render::lightmap::LightmapReceiverReport>();
}

/// Runs the renderer's receiver mapping (`regia_render::lightmap::apply_lightmap_receivers`) on
/// the edit world, as the runtime's `Render` schedule does before extraction: each receiver part
/// draws its entry's unwrap and the world's `RuntimeRenderer` gets the lightmap. Idle while no
/// lightmap is installed and no receiver exists.
pub fn apply_level_lightmap_receivers(world: &mut World) {
    let installed = world
        .get_resource::<regia_render::lightmap::InstalledLightmap>()
        .is_some_and(|i| i.lightmap.is_some());
    let had = world
        .get_resource::<regia_render::lightmap::LightmapReceiverReport>()
        .is_some_and(|r| r.receivers > 0);
    if !installed && !had {
        return;
    }
    let _span = tracing::info_span!(target: "regia_editor", "editor_lightmap_receivers").entered();
    if !world.contains_resource::<regia_render::lightmap::LightmapReceiverReport>() {
        world.init_resource::<regia_render::lightmap::LightmapReceiverReport>();
    }
    // One registered system for the world's life: the mapping's `Local` holds the baked clusters it
    // installed, and a system built anew each frame would re-install them every frame.
    let id = match world.get_resource::<LevelReceiverSystem>() {
        Some(system) => system.0,
        None => {
            let id = world.register_system(regia_render::lightmap::apply_lightmap_receivers);
            world.insert_resource(LevelReceiverSystem(id));
            id
        }
    };
    if let Err(err) = world.run_system(id) {
        tracing::debug!(target: "regia_editor", "lightmap receivers: {err}");
    }
}

/// The edit world's registered [`regia_render::lightmap::apply_lightmap_receivers`].
#[derive(Resource)]
struct LevelReceiverSystem(regia_ecs::ecs::system::SystemId);

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const SUMMARY: &str = r#"{
  "ok": true, "out": "maps/yard.flmap", "bytes": 4096, "seconds": 0.5,
  "atlas": { "width": 64, "height": 32, "fit_scale": 1.0 },
  "settings": { "density": 8.0 },
  "entries": [
    { "name": "entity_a_0", "label": "wall (a)", "rect": [0, 0, 32, 32], "texels_per_metre": 8.0 },
    { "name": "entity_b_0", "label": null, "rect": [32, 0, 16, 16], "texels_per_metre": 16.0 }
  ],
  "seams": {
    "entries": [
      { "name": "entity_a_0", "edges": 12, "p95": 0.1, "max": 0.2 },
      { "name": "entity_b_0", "edges": 4, "p95": 0.5, "max": 0.75 }
    ],
    "overlay": [
      { "entry": 1, "a": [0.5, 0.0], "b": [0.75, 0.25], "world_a": [1.0, 2.0, 3.0], "world_b": [4.0, 5.0, 6.0], "error": 0.75 }
    ]
  }
}"#;

    const DRY: &str = r#"{
  "ok": true, "dry_run": true,
  "atlas": { "width": 16, "height": 16, "fit_scale": 0.8 },
  "settings": { "density": 8.0 },
  "entries": [ { "name": "instance_0", "rect": [0, 0, 16, 16], "texels_per_metre": 6.4 } ]
}"#;

    const ERROR: &str =
        r#"{"ok": false, "code": "export_no_input", "path": null, "message": "no static mesh"}"#;

    fn fake_launcher(
        _program: &Path,
        request: &LightmapRequest,
        _cwd: Option<&Path>,
    ) -> Result<BakeJob, BakeFailure> {
        let (out, err) = if request.source.ends_with("bad.level.ron") {
            (ERROR, "")
        } else if request.dry_run {
            (DRY, "")
        } else {
            (SUMMARY, "  lightmap: 10/20 texels\n  lightmap: 20/20 texels\n")
        };
        Ok(BakeJob::from_readers(
            request.job_request(),
            Cursor::new(out.as_bytes().to_vec()),
            Cursor::new(err.as_bytes().to_vec()),
        ))
    }

    /// A job that never ends: its pipes stay open, so it stays running until it is cancelled.
    fn hanging_launcher(
        _program: &Path,
        request: &LightmapRequest,
        _cwd: Option<&Path>,
    ) -> Result<BakeJob, BakeFailure> {
        let (out, out_writer) = std::io::pipe().expect("pipe");
        let (err, err_writer) = std::io::pipe().expect("pipe");
        std::mem::forget((out_writer, err_writer));
        Ok(BakeJob::from_readers(request.job_request(), out, err))
    }

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(LightmapPanel {
            launcher: fake_launcher,
            ..LightmapPanel::default()
        });
        world
    }

    fn call(world: &mut World, name: &str, params: &Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(schema, _)| schema.name == name)
            .expect("registered");
        handler(world, params)
    }

    /// Runs the panel for one frame with no renderer and returns how many shapes it drew.
    fn draw_once(panel: &mut LightmapPanel) -> usize {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw(ui, panel, None, Some(Path::new("/p/levels/yard.level.ron")));
        });
        // No renderer here: the font atlas upload is dropped on purpose.
        output.textures_delta.clear();
        output.shapes.len()
    }

    /// Audit LOW (sprint-corr-editor-a L2): with no project open the panel passes no `--project`,
    /// and the CLI then finds the level's project itself (`find_project`), but `written()` answered
    /// `None`, so the checkpoint and the job's `out` were missing for a bake that wrote one.
    #[test]
    fn written_finds_the_project_as_the_cli_does_without_one() {
        let root = regia_test_utils::unique_temp_dir("editor_lightmap_no_project");
        std::fs::create_dir_all(root.join("assets/levels")).expect("dirs");
        let level = root.join("assets/levels/yard.level.ron");
        let request = LightmapRequest {
            source: level.display().to_string(),
            project: None,
            out: None,
            fields: LightmapFields::default(),
            dry_run: false,
        };
        // No manifest: the folder above `assets`.
        assert_eq!(request.written(), Some(root.join("assets/maps/yard.flmap")));
        // A manifest higher up than `assets`' parent wins, as in the CLI.
        let outer = regia_test_utils::unique_temp_dir("editor_lightmap_manifest");
        std::fs::write(outer.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
        let nested = outer.join("content/levels/yard.level.ron");
        std::fs::create_dir_all(nested.parent().expect("parent")).expect("dirs");
        let request = LightmapRequest {
            source: nested.display().to_string(),
            ..request
        };
        assert_eq!(request.written(), Some(outer.join("assets/maps/yard.flmap")));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outer);
    }

    /// ADR-3095: a bake from the panel checkpoints (`--checkpoint`, never on a dry run); the
    /// checkpoint lives beside the lightmap the CLI writes, a level's included; the panel reports a
    /// checkpoint there in one line, and its draw shows it.
    #[test]
    fn panel_bakes_checkpoint_and_a_found_checkpoint_shows_one_line() {
        let root = regia_test_utils::unique_temp_dir("editor_lightmap_checkpoint");
        let level = root.join("assets/levels/yard.level.ron");
        let mut request = LightmapRequest {
            source: level.display().to_string(),
            project: Some(root.clone()),
            out: None,
            fields: LightmapFields::default(),
            dry_run: false,
        };
        let has = |r: &LightmapRequest| r.args().iter().any(|a| a == "--checkpoint");
        assert!(has(&request));
        assert_eq!(request.written(), Some(root.join("assets/maps/yard.flmap")));
        request.dry_run = true;
        assert!(!has(&request), "a dry run bakes nothing");
        let panel = LightmapPanel::default();
        assert!(panel.pending_checkpoint(Some(&root), Some(&level)).is_none());
        let out = root.join("assets/maps/yard.flmap");
        regia_asset::bake_checkpoint::BakeCheckpoint {
            header: regia_asset::bake_checkpoint::BakeCheckpointHeader {
                kind: regia_asset::bake_checkpoint::BakeCheckpointKind::LightmapCpu,
                stage: 0,
                input_hash: [1; 32],
                units_total: 100,
                units_done: 25,
                batches_done: 1,
                work_seconds: 5.0,
            },
            payload: Vec::new(),
        }
        .write_atomic(&regia_asset::bake_checkpoint::default_checkpoint_path(&out))
        .expect("checkpoint");
        let status = panel
            .pending_checkpoint(Some(&root), Some(&level))
            .expect("found");
        assert_eq!(
            status.line().as_deref(),
            Some("resumable checkpoint: 25 % (75 texels left); the next bake continues it")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The checkpoint Discard as a command deletes the checkpoint the panel shows (ADR-7307).
    #[test]
    fn discard_checkpoint_deletes_the_pending_checkpoint() {
        let root = regia_test_utils::unique_temp_dir("editor_lightmap_discard_checkpoint");
        let level = root.join("assets/levels/yard.level.ron");
        let panel = LightmapPanel::default();
        assert_eq!(
            discard_checkpoint_of(&panel, Some(&root), Some(&level)),
            "No bake checkpoint to discard"
        );
        let path =
            regia_asset::bake_checkpoint::default_checkpoint_path(&root.join("assets/maps/yard.flmap"));
        regia_asset::bake_checkpoint::BakeCheckpoint {
            header: regia_asset::bake_checkpoint::BakeCheckpointHeader {
                kind: regia_asset::bake_checkpoint::BakeCheckpointKind::LightmapCpu,
                stage: 0,
                input_hash: [1; 32],
                units_total: 100,
                units_done: 25,
                batches_done: 1,
                work_seconds: 5.0,
            },
            payload: Vec::new(),
        }
        .write_atomic(&path)
        .expect("checkpoint");
        assert!(panel.pending_checkpoint(Some(&root), Some(&level)).is_some());
        let line = discard_checkpoint_of(&panel, Some(&root), Some(&level));
        assert!(line.starts_with("Discarded the checkpoint"), "{line}");
        assert!(!path.exists(), "the checkpoint file is gone");
        assert!(panel.pending_checkpoint(Some(&root), Some(&level)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A level bake leaves `out` empty (the CLI picks `assets/maps/<stem>.flmap`). Status must
    /// still report the checkpoint beside that file.
    #[test]
    fn a_level_bake_status_reports_the_checkpoint_beside_the_map() {
        let root = regia_test_utils::unique_temp_dir("editor_lightmap_status_checkpoint");
        let level = root.join("assets/levels/yard.level.ron");
        let out = root.join("assets/maps/yard.flmap");
        regia_asset::bake_checkpoint::BakeCheckpoint {
            header: regia_asset::bake_checkpoint::BakeCheckpointHeader {
                kind: regia_asset::bake_checkpoint::BakeCheckpointKind::LightmapCpu,
                stage: 0,
                input_hash: [1; 32],
                units_total: 100,
                units_done: 25,
                batches_done: 1,
                work_seconds: 5.0,
            },
            payload: Vec::new(),
        }
        .write_atomic(&regia_asset::bake_checkpoint::default_checkpoint_path(&out))
        .expect("checkpoint");
        let mut panel = LightmapPanel {
            launcher: fake_launcher,
            ..LightmapPanel::default()
        };
        let request = LightmapRequest {
            source: level.display().to_string(),
            project: Some(root.clone()),
            out: None,
            fields: LightmapFields::default(),
            dry_run: false,
        };
        panel.start(&request, Some(&root)).expect("started");
        let status = panel.status_json();
        assert_eq!(
            status["checkpoint"]["exists"],
            json!(true),
            "a level bake reported {status}"
        );
        assert_eq!(status["checkpoint"]["units_done"], json!(25));
        assert_eq!(status["checkpoint"]["resumable"], json!(true));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_level_is_baked_with_scene_and_a_description_by_position() {
        let mut request = LightmapRequest {
            source: "/p/assets/levels/yard.level.ron".into(),
            project: Some(PathBuf::from("/p")),
            out: None,
            fields: LightmapFields::default(),
            dry_run: false,
        };
        let args: Vec<String> = request
            .args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[0..5],
            [
                "bake-lightmaps",
                "--scene",
                "/p/assets/levels/yard.level.ron",
                "--project",
                "/p"
            ]
        );
        request.source = "maps/bore.bake.ron".into();
        request.dry_run = true;
        request.fields.direct_sun = true;
        request.fields.planar_charts = true;
        request.fields.meshlets = true;
        let args: Vec<String> = request
            .args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0..2], ["bake-lightmaps", "maps/bore.bake.ron"]);
        for flag in [
            "--direct-sun",
            "--dry-run",
            "--unwrap",
            "charts",
            "--density",
            "--meshlets",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag}: {args:?}");
        }
    }

    /// The denoiser, the texel encoding and the ADR-2404 switches pass through to the CLI: on (and
    /// RGB9E5) by default, as `regia bake-lightmaps` bakes new maps; an agent call sets each one,
    /// and a bad value is refused before anything starts.
    #[test]
    fn denoise_encoding_and_the_crease_switches_reach_the_cli() {
        let strings = |r: &LightmapRequest| -> Vec<String> {
            r.args()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let value = |args: &[String], flag: &str| -> String {
            let at = args.iter().position(|a| a == flag).expect(flag);
            args[at + 1].clone()
        };
        let mut request = LightmapRequest {
            source: QUAD_BOX.into(),
            project: None,
            out: None,
            fields: LightmapFields::default(),
            dry_run: false,
        };
        let args = strings(&request);
        for (flag, want) in [
            ("--denoise", "on"),
            ("--encoding", "rgb9e5"),
            ("--crease", "on"),
            ("--stitch", "on"),
            ("--auto-density", "on"),
        ] {
            assert_eq!(value(&args, flag), want, "{args:?}");
        }
        let mut fields = LightmapFields::default();
        apply_fields(
            &mut fields,
            &json!({ "denoise": false, "encoding": "astc_hdr", "crease": false, "stitch": false, "auto_density": false }),
        )
        .expect("fields");
        request.fields = fields;
        let args = strings(&request);
        for (flag, want) in [
            ("--denoise", "off"),
            ("--encoding", "astc-hdr"),
            ("--crease", "off"),
            ("--stitch", "off"),
            ("--auto-density", "off"),
        ] {
            assert_eq!(value(&args, flag), want, "{args:?}");
        }
        let mut fields = LightmapFields::default();
        apply_fields(&mut fields, &json!({ "encoding": "bc6h" })).expect("fields");
        assert_eq!(fields.encoding, LightmapTexelEncoding::Bc6h);
        assert!(fields.denoise && fields.crease && fields.stitch && fields.auto_density);
        let mut world = World::new();
        for bad in [
            json!({ "source": "quad-box", "encoding": "dxt5" }),
            json!({ "source": "quad-box", "encoding": 2 }),
            json!({ "source": "quad-box", "denoise": "yes" }),
            json!({ "source": "quad-box", "stitch": 1 }),
        ] {
            let e = call(&mut world, "editor.lightmap_bake", &bad).expect_err("refused");
            assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{bad}");
        }
        let panel = world.get_resource::<LightmapPanel>().expect("panel");
        assert_eq!(
            panel.fields,
            LightmapFields::default(),
            "a refused call changes nothing"
        );
    }

    /// CLI's bounds (4096..=67108864 rays, at most 1000 ms); without `gpu` neither flag is there.
    #[test]
    fn a_gpu_bake_from_the_panel_shares_the_gpu() {
        let mut request = LightmapRequest {
            source: QUAD_BOX.into(),
            project: None,
            out: None,
            fields: LightmapFields::default(),
            dry_run: false,
        };
        let strings = |r: &LightmapRequest| -> Vec<String> {
            r.args()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert!(!strings(&request).iter().any(|a| a.starts_with("--gpu")));
        request.fields.gpu = true;
        let args = strings(&request);
        let value = |flag: &str| -> u64 {
            let at = args.iter().position(|a| a == flag).expect(flag);
            args[at + 1].parse().expect("a number")
        };
        assert!(args.iter().any(|a| a == "--gpu"));
        assert!((4096..=(1 << 26)).contains(&value("--gpu-tile-rays")));
        assert!((1..=1000).contains(&value("--gpu-yield-ms")));
    }

    #[test]
    fn default_outputs_follow_the_cli_s_rules() {
        let root = Path::new("/p");
        assert_eq!(default_out("/p/levels/yard.level.ron", Some(root)), None);
        assert_eq!(
            default_out("/p/maps/bore.bake.ron", Some(root)),
            Some(PathBuf::from("/p/maps/bore.flmap"))
        );
        let built_in = default_out(QUAD_BOX, Some(root)).expect("under the project");
        assert!(built_in.ends_with("bake/quad-box.flmap"), "{built_in:?}");
        assert_eq!(default_out("cornell", None), None);
        assert_eq!(resolve_source(QUAD_BOX, Some(root)), QUAD_BOX);
        // Compared as paths: the joined source uses the host separator (ADR-3150).
        assert_eq!(
            Path::new(&resolve_source("levels/a.level.ron", Some(root))),
            Path::new("/p/levels/a.level.ron")
        );
    }

    /// The panel's whole picture is read from the CLI's JSON.
    #[test]
    fn the_view_reads_density_and_seams_from_the_summary() {
        let v: Value = serde_json::from_str(SUMMARY).expect("json");
        let view = AtlasView::from_summary(&v).expect("view");
        assert_eq!((view.width, view.height, view.entries.len()), (64, 32, 2));
        assert_eq!(view.entries[0].density_ratio, 1.0);
        assert_eq!(view.entries[1].density_ratio, 2.0);
        assert_eq!(view.entries[0].label.as_deref(), Some("wall (a)"));
        assert_eq!(view.entries[1].seam_edges, 4);
        assert_eq!(view.entries[1].seam_p95, Some(0.5));
        assert_eq!(view.over_budget(0.25), ["entity_b_0"]);
        assert!(view.over_budget(0.5).is_empty());
        assert_eq!(view.seams[0].world_b, [4.0, 5.0, 6.0]);
        let json = view.to_json(0.25);
        assert_eq!(json["seam_segments"], 1);
        assert_eq!(json["worst_seam"], 0.75);
        // A dry run has no seams, and a summary of another shape gives no view.
        let dry: Value = serde_json::from_str(DRY).expect("json");
        let dry_view = AtlasView::from_summary(&dry).expect("view");
        assert!(dry_view.dry_run && dry_view.seams.is_empty());
        assert_eq!(dry_view.entries[0].seam_p95, None);
        assert!((dry_view.entries[0].density_ratio - 0.8).abs() < 1e-6);
        assert_eq!(AtlasView::from_summary(&json!({"ok": true})), None);
    }

    #[test]
    fn colors_move_with_the_density_and_the_seam_step() {
        assert_eq!(density_color(1.0), egui::Color32::from_rgb(90, 190, 110));
        assert_eq!(density_color(0.25), egui::Color32::from_rgb(60, 90, 220));
        assert_eq!(density_color(4.0), egui::Color32::from_rgb(240, 150, 60));
        assert_eq!(
            density_color(f32::NAN),
            density_color(0.25),
            "bad input reads as too low"
        );
        assert_eq!(seam_color(0.0, 0.25), egui::Color32::from_rgb(110, 200, 90));
        assert_eq!(seam_color(0.25, 0.25), egui::Color32::from_rgb(235, 80, 90));
        assert_eq!(seam_color(9.0, 0.25), seam_color(0.25, 0.25));
        assert_eq!(
            seam_color(0.0, 0.0),
            seam_color(0.25, 0.25),
            "a zero budget has no room"
        );
    }

    /// The preview draws one rectangle per entry over the background, plus the worst seams when
    /// that is what it shows.
    #[test]
    fn the_atlas_preview_draws_entries_and_seams() {
        let v: Value = serde_json::from_str(SUMMARY).expect("json");
        let view = AtlasView::from_summary(&v).expect("view");
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(64.0, 32.0));
        let density = atlas_shapes(&view, rect, AtlasPreview::Density, 0.25);
        assert_eq!(density.len(), 3);
        let seams = atlas_shapes(&view, rect, AtlasPreview::Seams, 0.25);
        assert_eq!(seams.len(), 4);
        let egui::Shape::LineSegment { points, .. } = &seams[3] else {
            panic!("the seam is a line: {:?}", seams[3]);
        };
        assert_eq!(points[0], egui::pos2(42.0, 20.0));
        assert_eq!(points[1], egui::pos2(58.0, 28.0));
    }

    /// The headless smoke test: every state draws, from idle through a dry run to a finished bake
    /// and its seam preview, and from a bad level to its error.
    #[test]
    fn the_panel_draws_every_state_headless() {
        let mut panel = LightmapPanel {
            launcher: fake_launcher,
            ..LightmapPanel::default()
        };
        assert!(draw_once(&mut panel) > 0);
        assert_eq!(panel.state().name(), "idle");
        assert_eq!(panel.listed.as_deref(), Some(&[][..]), "no project lists nothing");

        let level = Some(Path::new("/p/levels/yard.level.ron"));
        let dry = panel.request(None, level, true).expect("request");
        assert_eq!(dry.out, None);
        panel.start(&dry, None).expect("started");
        panel.wait();
        assert!(panel.view().expect("dry view").dry_run);
        draw_once(&mut panel);

        let bake = panel.request(None, level, false).expect("request");
        panel.start(&bake, None).expect("started");
        panel.wait();
        let view = panel.view().expect("bake view");
        assert!(!view.dry_run);
        panel.preview = AtlasPreview::Seams;
        draw_once(&mut panel);

        panel.source = "bad.level.ron".into();
        let bad = panel.request(None, level, false).expect("request");
        panel.start(&bad, None).expect("started");
        panel.wait();
        assert_eq!(panel.state().name(), "failed");
        assert!(panel.view().is_none(), "the failed bake drew no atlas");
        draw_once(&mut panel);

        panel.source = String::new();
        let e = panel.request(None, None, false).expect_err("no source");
        assert_eq!(e.code, "lightmap_no_source");
    }

    /// Real egui input: a click on Bake starts a job.
    #[test]
    fn the_bake_button_starts_a_bake() {
        let mut panel = LightmapPanel {
            launcher: fake_launcher,
            ..LightmapPanel::default()
        };
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 800.0));
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                draw(ui, &mut panel, None, Some(Path::new("/p/levels/yard.level.ron")));
            });
            output.textures_delta.clear();
            panel.bake_button
        };
        // Two frames lay the panel out, then a press and a release over the button.
        frame(Vec::new());
        let at = frame(Vec::new()).expect("the Bake button has a place");
        let press = |pressed: bool| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(at)]);
        frame(vec![press(true)]);
        frame(vec![press(false)]);
        assert_eq!(panel.started, 1, "the click started one bake");
    }

    #[test]
    fn agent_methods_start_report_and_cancel() {
        let mut world = world();
        let idle = call(&mut world, "editor.lightmap_status", &json!({})).expect("status");
        assert_eq!(idle["state"], "idle");
        assert_eq!(idle["view"], Value::Null);

        // A dry run may wait; a bake may not.
        let dry = call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "quad-box", "dry_run": true, "wait": true, "density": 4.0 }),
        )
        .expect("dry run");
        assert_eq!(dry["state"], "succeeded");
        assert_eq!(dry["view"]["dry_run"], true);
        assert!(
            dry["args"]
                .as_array()
                .expect("args")
                .iter()
                .any(|a| a == "--dry-run")
        );
        let e = call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "quad-box", "wait": true }),
        )
        .expect_err("wait");
        assert_eq!(
            e.data.as_ref().expect("data")["bake_error"],
            "lightmap_wait_needs_dry_run"
        );

        // A flag is not a source, and bad types change nothing.
        let e = call(&mut world, "editor.lightmap_bake", &json!({ "source": "--help" })).expect_err("flag");
        assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS);
        call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "quad-box", "unwrap": "nope" }),
        )
        .expect_err("unwrap");
        call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "quad-box", "samples": "many" }),
        )
        .expect_err("samples");

        // A bake runs to its summary; the overlay reads its world-space seams.
        call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "quad-box" }),
        )
        .expect("bake");
        world.resource_mut::<LightmapPanel>().wait();
        let done = call(&mut world, "editor.lightmap_status", &json!({})).expect("status");
        assert_eq!(done["state"], "succeeded");
        assert_eq!(done["progress"]["done"], 20);
        assert_eq!(done["progress"]["stage"], "lightmap");
        assert_eq!(done["view"]["over_budget"], json!(["entity_b_0"]));
        assert_eq!(done["overlay_segments"], 0, "the overlay is off by default");
        assert!(lightmap_gizmo_segments(&world).is_empty());

        let on = call(
            &mut world,
            "editor.lightmap_preview",
            &json!({ "preview": "seams", "seam_overlay": true, "seam_budget": 1.0 }),
        )
        .expect("preview");
        assert_eq!(on["overlay_segments"], 1);
        assert_eq!(on["view"]["over_budget"], json!([]));
        let segments = lightmap_gizmo_segments(&world);
        assert_eq!(segments[0].0, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(segments[0].1, Vec3::new(4.0, 5.0, 6.0));
        call(
            &mut world,
            "editor.lightmap_preview",
            &json!({ "preview": "nope" }),
        )
        .expect_err("bad name");
        call(
            &mut world,
            "editor.lightmap_preview",
            &json!({ "seam_budget": -1.0 }),
        )
        .expect_err("bad budget");
        call(
            &mut world,
            "editor.lightmap_preview",
            &json!({ "seam_overlay": 3 }),
        )
        .expect_err("bad flag");
        assert_eq!(
            world.resource::<LightmapPanel>().seam_budget,
            1.0,
            "refused calls keep the value"
        );

        let finished = call(&mut world, "editor.lightmap_cancel", &json!({})).expect("cancel");
        assert_eq!(finished["cancelled"], false, "nothing was running");

        // Errors of the bake come back with the CLI's code.
        let bad = call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "bad.level.ron", "dry_run": true, "wait": true }),
        )
        .expect("the call worked, the bake failed");
        assert_eq!(bad["state"], "failed");
        assert_eq!(bad["error"]["code"], "export_no_input");
        assert_eq!(bad["view"], Value::Null);
    }

    #[test]
    fn levels_and_descriptions_are_listed() {
        let dir = regia_test_utils::unique_temp_dir("lightmap_panel_listing");
        let levels = dir.join("assets/levels");
        std::fs::create_dir_all(&levels).expect("dirs");
        regia_test_utils::write_atomic(&levels.join("yard.level.ron"), b"()").expect("level");
        regia_test_utils::write_atomic(&dir.join("bore.bake.ron"), b"()").expect("desc");
        assert_eq!(find_levels(&dir), ["assets/levels/yard.level.ron"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every method's schema names itself and points at the shared status shape.
    /// A refused call leaves what the panel's next Bake would use: no half-applied settings.
    #[test]
    fn a_refused_call_changes_nothing() {
        let mut world = world();
        let before = {
            let panel = world.resource::<LightmapPanel>();
            (panel.source.clone(), panel.fields.clone())
        };
        let e = call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "samples": 1, "direct_sun": true }),
        )
        .expect_err("no source, no open level");
        assert_eq!(e.data.as_ref().expect("data")["bake_error"], "lightmap_no_source");
        let panel = world.resource::<LightmapPanel>();
        assert_eq!((panel.source.clone(), panel.fields.clone()), before);

        // The same while a bake runs.
        world.resource_mut::<LightmapPanel>().launcher = hanging_launcher;
        call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "quad-box" }),
        )
        .expect("starts");
        let running = {
            let panel = world.resource::<LightmapPanel>();
            (panel.source.clone(), panel.fields.clone())
        };
        let e = call(
            &mut world,
            "editor.lightmap_bake",
            &json!({ "source": "cornell", "samples": 1 }),
        )
        .expect_err("busy");
        assert_eq!(e.data.as_ref().expect("data")["bake_error"], "lightmap_busy");
        let panel = world.resource::<LightmapPanel>();
        assert_eq!((panel.source.clone(), panel.fields.clone()), running);
        call(&mut world, "editor.lightmap_cancel", &json!({})).expect("cancel");
    }

    /// Before anything starts: a flag in disguise, a density the CLI's parser would refuse with no
    /// JSON, and a source whose default output would land outside the allowed roots.
    #[test]
    fn bad_sources_and_densities_are_refused() {
        let mut world = world();
        for source in [" --help", "\t-x"] {
            let e = call(&mut world, "editor.lightmap_bake", &json!({ "source": source })).expect_err(source);
            assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{source}");
        }
        for density in [-1.0, 0.0, 1e300] {
            let e = call(
                &mut world,
                "editor.lightmap_bake",
                &json!({ "source": "quad-box", "density": density }),
            )
            .expect_err("density");
            assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{density}");
        }
        for source in [
            "/regia-no-such-root/x.bake.ron",
            "../../x.bake.ron",
            "/regia-no-such-root/x.level.ron",
        ] {
            let e = call(&mut world, "editor.lightmap_bake", &json!({ "source": source })).expect_err(source);
            assert_eq!(
                e.data.as_ref().expect("data")["reason"],
                "path_outside_project",
                "{source}"
            );
        }
        assert!(!world.resource::<LightmapPanel>().is_running(), "nothing started");
    }

    /// The directory form of a level goes to the CLI as `--scene`, and its output is the CLI's.
    #[test]
    fn a_split_level_is_a_level() {
        for source in ["assets/levels/yard.level.d", "assets/levels/yard.level.d/"] {
            assert!(is_level(source), "{source}");
            assert_eq!(default_out(source, None), None, "{source}");
            let request = LightmapRequest {
                source: source.into(),
                project: None,
                out: None,
                fields: LightmapFields::default(),
                dry_run: true,
            };
            let args = request.args();
            assert_eq!(args[1], "--scene", "{source}");
            assert_eq!(args[2], source);
        }
        assert!(!is_level("assets/levels/yard.bake.ron"));
    }

    #[test]
    fn the_schema_lists_four_methods() {
        let names: Vec<&str> = methods().iter().map(|(s, _)| s.name).collect();
        assert_eq!(
            names,
            [
                "editor.lightmap_bake",
                "editor.lightmap_status",
                "editor.lightmap_cancel",
                "editor.lightmap_preview",
            ]
        );
    }

    /// A glTF of one mesh with two primitives whose material order is the reverse of their own:
    /// primitive 0 a triangle drawn with material 1, primitive 1 a quad drawn with material 0. The
    /// editor's material grouping puts the quad first; the cook (and so the bake's part keys) puts
    /// the triangle first.
    fn write_two_part_gltf(path: &Path) {
        let mut bin: Vec<u8> = Vec::new();
        let mut views: Vec<Value> = Vec::new();
        let mut accessors: Vec<Value> = Vec::new();
        let mut push = |bytes: Vec<u8>, accessor: Value, bin: &mut Vec<u8>| {
            while !bin.len().is_multiple_of(4) {
                bin.push(0);
            }
            views.push(json!({ "buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len() }));
            bin.extend_from_slice(&bytes);
            let mut a = accessor;
            a["bufferView"] = json!(views.len() - 1);
            accessors.push(a);
            accessors.len() - 1
        };
        let f32s = |v: &[[f32; 3]]| {
            v.iter()
                .flatten()
                .flat_map(|x| x.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        let f32s2 = |v: &[[f32; 2]]| {
            v.iter()
                .flatten()
                .flat_map(|x| x.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        let u16s = |v: &[u16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let mut primitive = |pos: &[[f32; 3]], idx: &[u16], material: usize, bin: &mut Vec<u8>| {
            let n = pos.len();
            let (lo, hi) = pos.iter().fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
                (
                    [lo[0].min(p[0]), lo[1].min(p[1]), lo[2].min(p[2])],
                    [hi[0].max(p[0]), hi[1].max(p[1]), hi[2].max(p[2])],
                )
            });
            let p = push(
                f32s(pos),
                json!({ "componentType": 5126, "count": n, "type": "VEC3", "min": lo, "max": hi }),
                bin,
            );
            let nrm = push(
                f32s(&vec![[0.0, 1.0, 0.0]; n]),
                json!({ "componentType": 5126, "count": n, "type": "VEC3" }),
                bin,
            );
            let uv: Vec<[f32; 2]> = pos.iter().map(|p| [p[0], p[2]]).collect();
            let t = push(
                f32s2(&uv),
                json!({ "componentType": 5126, "count": n, "type": "VEC2" }),
                bin,
            );
            let i = push(
                u16s(idx),
                json!({ "componentType": 5123, "count": idx.len(), "type": "SCALAR" }),
                bin,
            );
            json!({ "attributes": { "POSITION": p, "NORMAL": nrm, "TEXCOORD_0": t }, "indices": i, "material": material })
        };
        let tri = primitive(
            &[[0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
            &[0, 1, 2],
            1,
            &mut bin,
        );
        let quad = primitive(
            &[[2.0, 0.0, 0.0], [2.0, 0.0, 1.0], [3.0, 0.0, 1.0], [3.0, 0.0, 0.0]],
            &[0, 1, 2, 0, 2, 3],
            0,
            &mut bin,
        );
        let gltf = json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "name": "floor", "mesh": 0 }],
            "meshes": [{ "primitives": [tri, quad] }],
            "materials": [
                { "name": "red", "pbrMetallicRoughness": { "baseColorFactor": [1.0, 0.0, 0.0, 1.0] } },
                { "name": "blue", "pbrMetallicRoughness": { "baseColorFactor": [0.0, 0.0, 1.0, 1.0] } }
            ],
            "buffers": [{ "uri": "two.bin", "byteLength": bin.len() }],
            "bufferViews": views,
            "accessors": accessors,
        });
        std::fs::write(path.with_file_name("two.bin"), &bin).expect("write bin");
        std::fs::write(path, gltf.to_string()).expect("write gltf");
    }

    /// The edit world installs the open level's lightmap (ADR-2635) and every part of a keyed
    /// multi-part mesh receives it: the parts load in the cook's order, so a lightmap keyed the
    /// way `regia bake-lightmaps --scene` keys it (from the cooked model's LOD 0 parts) applies to
    /// both with no `lightmap_layout`. The editor's material grouping would have refused part 0
    /// (the assertion on `load_gltf_model` shows the two orders differ for this file).
    #[test]
    fn an_edited_level_installs_its_lightmap_on_every_cooked_part() {
        use regia_asset::lightmap::{
            Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapEntryKey, LightmapLayout, MeshUnwrap,
        };
        use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
        use regia_scene::prelude::EntityGuid;
        use std::sync::Arc;

        use regia_app::prelude::App;

        let root = regia_test_utils::unique_temp_dir("editor-lightmap-level");
        std::fs::create_dir_all(root.join("assets/props")).expect("props dir");
        std::fs::create_dir_all(root.join("assets/maps")).expect("maps dir");
        let gltf = root.join("assets/props/two.gltf");
        write_two_part_gltf(&gltf);

        // The bake's view of the file: the cooked model's LOD 0 parts, in its order.
        let import = regia_asset_pipeline::import_gltf_path(
            &gltf,
            regia_asset::AssetId::new_random(),
            regia_asset_pipeline::Forward::Keep,
            regia_asset_pipeline::host_platform(),
            None,
            regia_asset_pipeline::TextureCookQuality::Low,
        )
        .expect("import");
        let mut meshes = std::collections::HashMap::new();
        let mut model = None;
        for sub in &import.sub_assets {
            match sub.type_path.as_str() {
                "regia_asset::CookedMesh" => {
                    meshes.insert(
                        sub.id,
                        regia_asset::CookedMesh::from_postcard(&sub.payload).expect("mesh"),
                    );
                }
                "regia_asset::CookedModel" => {
                    model = Some(regia_asset::CookedModel::from_postcard(&sub.payload).expect("model"));
                }
                _ => {}
            }
        }
        let cooked: Vec<regia_asset::CpuMesh> = model
            .expect("a model")
            .parts
            .iter()
            .map(|p| regia_render::visbuffer::cooked_mesh_lod0(&meshes[&p.mesh]).expect("lod 0"))
            .collect();
        assert_eq!(cooked.len(), 2);
        let editor_order =
            crate::mesh_source::load_gltf_model(&gltf, &Arc::new(regia_render::LabMaterial::default()))
                .expect("editor load");
        assert_ne!(
            editor_order.parts[0].mesh.vertices.len(),
            cooked[0].positions.len(),
            "the material grouping and the cook disagree on part 0 for this file"
        );

        let guid: regia_core::id::Guid = "00000000-0000-0000-0000-0000000000c3".parse().expect("guid");
        let n = cooked.len() as u32;
        let entries = cooked
            .iter()
            .enumerate()
            .map(|(k, mesh)| LightmapEntry {
                name: format!("entity_{guid}_{k}"),
                source_vertices: mesh.positions.len() as u32,
                rect: [2 * k as u32, 0, 2, 2],
                scale_offset: [1.0 / n as f32, 1.0, k as f32 / n as f32, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 1.0,
                texels_per_metre: 1.0,
                key: Some(LightmapEntryKey {
                    entity: guid,
                    part: k as u32,
                }),
                seams: None,
                page: 0,
            })
            .collect();
        let unwraps = cooked
            .iter()
            .map(|mesh| MeshUnwrap {
                xref: (0..mesh.positions.len() as u32).collect(),
                uv: vec![[0.5, 0.5]; mesh.positions.len()],
                indices: mesh.indices.clone(),
            })
            .collect();
        let texels = (2 * n * 2) as usize;
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 2 * n,
                height: 2,
                density: 1.0,
                padding: 0,
                entries,
                unwraps,
            },
            &vec![[100.0; 3]; texels],
            vec![Coverage::BAKED; texels],
            LightmapBakeInfo::default(),
        )
        .expect("valid lightmap")
        .write(&root.join("assets/maps/two.flmap"))
        .expect("write flmap");

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        regia_render::register_render_types(&mut app);
        install_level_lightmaps(&mut app);
        app.insert_resource(
            regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".to_string(),
                content_dir: None,
            })
            .expect("asset server"),
        );
        app.insert_resource(regia_render::RuntimeRenderer::default());
        app.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let id = LevelId(regia_core::id::Guid::new_random());
        let text = format!(
            r#"SceneFile(
    format_version: 11,
    entities: [ (guid: "{guid}", name: "floor", components: {{
        "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
    }}) ],
    resources: {{
        "regia_scene::level::LevelSettings": (look: (lightmap: Some((file: "maps/two.flmap")))),
    }},
)"#
        );
        let level = Level::from_ron_str(id, &text).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        let owner = {
            let mut q = app.world_mut().query::<(Entity, &EntityGuid)>();
            q.iter(app.world())
                .find(|(_, g)| g.0 == guid)
                .map(|(e, _)| e)
                .expect("spawned")
        };
        app.world_mut()
            .entity_mut(owner)
            .insert(regia_render::MeshSource::new("props/two.gltf", ""));
        // The editor's frame: the world's systems, the mesh resolve, the viewport's receiver step.
        for _ in 0..3 {
            app.update();
            crate::mesh_source::resolve_mesh_sources(app.world_mut());
            apply_level_lightmap_receivers(app.world_mut());
        }
        let resolved = app
            .world()
            .get::<crate::mesh_source::ResolvedMesh>(owner)
            .expect("the mesh source resolved");
        assert_eq!(resolved.error, None);
        assert_eq!(resolved.parts.len(), 2, "one part per cooked primitive");
        let part_of: Vec<Option<regia_render::lightmap::MeshPartOf>> = resolved
            .parts
            .iter()
            .map(|p| app.world().get::<regia_render::lightmap::MeshPartOf>(*p).copied())
            .collect();
        assert_eq!(
            part_of,
            vec![
                Some(regia_render::lightmap::MeshPartOf { owner, part: 0 }),
                Some(regia_render::lightmap::MeshPartOf { owner, part: 1 }),
            ]
        );
        let state = app
            .world()
            .resource::<regia_framework::lightmap_level::LevelLightmapState>();
        assert!(state.installed, "{:?}", state.error);
        assert_eq!(
            state.attached, 2,
            "both parts got their entry: {:?}",
            state.unmatched
        );
        let report = app
            .world()
            .resource::<regia_render::lightmap::LightmapReceiverReport>();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!((report.receivers, report.applied), (2, 2));
        assert!(
            app.world()
                .resource::<regia_render::RuntimeRenderer>()
                .0
                .warm_lightmaps(),
            "the level asked the viewport's renderer to warm the lightmap variants"
        );
        // The cooked stream keeps only the bitangent sign: the parts' tangents are generated, not
        // the placeholder +x. A quarter turn about +y moves the surface's U axis off +x.
        let mut document: Value =
            serde_json::from_slice(&std::fs::read(&gltf).expect("read gltf")).expect("gltf json");
        let half = std::f32::consts::FRAC_PI_4;
        document["nodes"][0]["rotation"] = json!([0.0, half.sin(), 0.0, half.cos()]);
        std::fs::write(&gltf, serde_json::to_vec(&document).expect("gltf json")).expect("write gltf");
        let turned = crate::mesh_source::load_gltf_model_cooked_order(
            &gltf,
            &Arc::new(regia_render::LabMaterial::default()),
        )
        .expect("cooked order");
        for part in &turned.parts {
            for v in &part.mesh.vertices {
                assert!(v.tangent[2].abs() > 0.9, "the U axis is along z: {:?}", v.tangent);
                assert!(v.tangent[3].abs() > 0.9, "a bitangent sign: {:?}", v.tangent);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One quad, a lightmap with its baked clusters, one receiver on a visibility-buffer world.
    fn clustered_receiver_world() -> (World, regia_asset::lightmap::Lightmap, Entity) {
        use regia_asset::lightmap::{
            Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap,
        };
        use regia_asset_pipeline::lightmap_uv::{SourceAttributes, meshlets_for_unwrap};
        use regia_render::{CpuMesh, LabMaterial, LabVertex};
        use std::sync::Arc;

        let n = [0.0, 0.0, 1.0];
        let corners = [
            ([-1.0, -1.0, 0.0], [0.0, 0.0]),
            ([1.0, -1.0, 0.0], [1.0, 0.0]),
            ([1.0, 1.0, 0.0], [1.0, 1.0]),
            ([-1.0, 1.0, 0.0], [0.0, 1.0]),
        ];
        let mesh = Arc::new(CpuMesh {
            vertices: corners
                .iter()
                .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, n, *uv))
                .collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
        });
        let unwrap = MeshUnwrap {
            xref: vec![0, 1, 2, 3],
            uv: corners.iter().map(|(_, uv)| *uv).collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        let positions: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| v.position).collect();
        let normals = vec![n; 4];
        let uvs: Vec<[f32; 2]> = mesh.vertices.iter().map(|v| v.uv).collect();
        let clusters = meshlets_for_unwrap(
            SourceAttributes {
                positions: &positions,
                normals: &normals,
                uvs: &uvs,
            },
            &unwrap,
        )
        .expect("clusters");
        let mut map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 4,
                density: 1.0,
                padding: 0,
                entries: vec![LightmapEntry {
                    name: "quad".into(),
                    source_vertices: 4,
                    rect: [0, 0, 4, 4],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 4.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![unwrap],
            },
            &[[100.0; 3]; 16],
            vec![Coverage::BAKED; 16],
            LightmapBakeInfo::default(),
        )
        .expect("valid lightmap");
        map.meshlets = vec![clusters.to_bytes()];

        let mut world = World::new();
        world.init_resource::<regia_render::RuntimeRenderer>();
        let mut settings = regia_render::RenderSettings::default();
        settings.profile.visibility_buffer = true;
        world.insert_resource(settings);
        world.insert_resource(regia_render::lightmap::LightmapSettings {
            enabled: true,
            ..regia_render::lightmap::LightmapSettings::default()
        });
        let mut installed = regia_render::lightmap::InstalledLightmap::default();
        installed.install("first", map.clone());
        world.insert_resource(installed);
        let receiver = world
            .spawn((
                regia_render::MeshRenderer::new(mesh, Arc::new(LabMaterial::default())),
                regia_render::lightmap::LightmapReceiver { entry: "quad".into() },
            ))
            .id();
        (world, map, receiver)
    }

    /// The viewport's receiver step keeps its state between frames: the clusters of a receiver
    /// mesh leave the renderer with that mesh (a system built anew per frame forgets them, and
    /// decodes them again every frame).
    #[test]
    fn the_viewport_receiver_step_remembers_the_clusters_it_installed() {
        let (mut world, map, receiver) = clustered_receiver_world();
        let drawn_key = |world: &World| {
            let mesh = &world
                .get::<regia_render::MeshRenderer>(receiver)
                .expect("receiver")
                .mesh;
            regia_render::mesh_key(mesh)
        };
        for _ in 0..2 {
            apply_level_lightmap_receivers(&mut world);
        }
        let report = world.resource::<regia_render::lightmap::LightmapReceiverReport>();
        assert_eq!((report.applied, report.clusters), (1, 1), "{:?}", report.errors);
        let first = drawn_key(&world);
        assert!(
            world
                .resource::<regia_render::RuntimeRenderer>()
                .0
                .has_meshlets(first)
        );

        // Another install of the lightmap: the receiver gets a new mesh and the old clusters go.
        world
            .resource_mut::<regia_render::lightmap::InstalledLightmap>()
            .install("second", map);
        apply_level_lightmap_receivers(&mut world);
        let second = drawn_key(&world);
        assert_ne!(first, second, "the receiver mesh was rebuilt");
        let renderer = &world.resource::<regia_render::RuntimeRenderer>().0;
        assert!(renderer.has_meshlets(second));
        assert!(
            !renderer.has_meshlets(first),
            "the old receiver mesh's clusters left"
        );
    }

    /// A paged bake's summary (ADR-3300): the view and `editor.lightmap_status` carry the page count
    /// and each entry's page; an older summary without pages reads as one page.
    #[test]
    fn the_view_carries_the_atlas_pages() {
        let paged: Value = serde_json::from_str(
            r#"{
  "ok": true, "pages": 3,
  "atlas": { "width": 256, "height": 256, "fit_scale": 1.0, "pages": 3 },
  "settings": { "density": 8.0 },
  "entries": [
    { "name": "a", "page": 0, "rect": [0, 0, 128, 128], "texels_per_metre": 8.0 },
    { "name": "b", "page": 2, "rect": [0, 0, 128, 128], "texels_per_metre": 8.0 }
  ]
}"#,
        )
        .expect("json");
        let view = AtlasView::from_summary(&paged).expect("view");
        assert_eq!(view.pages, 3);
        assert_eq!(view.entries[1].page, 2);
        let v = view.to_json(1.0);
        assert_eq!(v["pages"], 3);
        assert_eq!(v["entries"][1]["page"], 2);
        let old = AtlasView::from_summary(&serde_json::from_str(SUMMARY).expect("json")).expect("view");
        assert_eq!((old.pages, old.entries[0].page), (1, 0));
    }

    /// Corr editor-a finding L3: an entry on a page past the summary's page count used to be
    /// drawn below the preview rectangle.
    #[test]
    fn an_entry_past_the_last_page_draws_inside_the_preview() {
        let summary: Value = serde_json::from_str(
            r#"{
  "ok": true, "pages": 3,
  "atlas": { "width": 256, "height": 256, "fit_scale": 1.0, "pages": 3 },
  "settings": { "density": 8.0 },
  "entries": [
    { "name": "a", "page": 7, "rect": [0, 0, 128, 128], "texels_per_metre": 8.0 }
  ]
}"#,
        )
        .expect("json");
        let view = AtlasView::from_summary(&summary).expect("view");
        assert_eq!(
            view.entries[0].page, 7,
            "the summary's value is reported as given"
        );
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 300.0));
        for shape in atlas_shapes(&view, rect, AtlasPreview::Density, 1.0) {
            let bounds = shape.visual_bounding_rect();
            assert!(
                rect.expand(0.5).contains_rect(bounds),
                "{bounds:?} is outside the preview {rect:?}"
            );
        }
    }
}
