// SPDX-License-Identifier: MPL-2.0
//! `regia bake-lightmaps <scene>`: bakes a lightmap atlas (`.flmap`, `regia_asset::lightmap`)
//! for the static meshes of a scene with the CPU baker (`regia_pt::lightmap`), after laying out
//! their second UV set (`regia_asset_pipeline::lightmap_uv`, the UV atlas generator by default).
//! `<scene>` is a built-in (`cornell`, `furnace`, `rolling-hills`, or `quad-box`, the lightmap
//! baker's reference scene) or a GI bake description (`.bake.ron`, the same file `regia bake-gi`
//! reads: its meshes, sun, sky and lights). `--scene <level>` bakes a level directly: its
//! `GiMobility::Static` meshes, keyed by entity (ADR-2024, [`level`]). `--instance` bakes only the
//! named instances (ADR-1101). ADR-860.
//!
//! Prints one JSON object on stdout: the atlas, every entry, the bake report, the chart seams and
//! the output.
//! New bakes run the à-trous denoiser (`--denoise on`, the default since ADR-2314; `--denoise off`
//! keeps the 3×3 filter of older bakes), and `--encoding bc6h|astc-hdr` stores the texels
//! block-compressed (`.flmap` version 5; the cook can do the same per platform).
//! New bakes also sample texels under geometry at the crease (`--crease on`), stitch smooth chart
//! seams (`--stitch on`) and raise the density of small curved receivers whose predicted seams
//! pass the budget (`--auto-density on`); ADR-2404. `--crease off --stitch off --auto-density off`
//! bakes as before.
//! `--dry-run` lays out the atlas without baking. `--inspect` reads an existing `.flmap` (the
//! positional argument) and prints its summary. Errors print `{"ok": false, "code", "path",
//! "message"}` with an agent error code (`book/agents/lightmaps.md`).

pub mod cells;
pub mod level;
mod meshlets;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use regia_agent::bake_export::SunOverride;
use regia_asset::lightmap::{
    DEFAULT_HARD_EDGE_DEG, Lightmap, LightmapContents, LightmapEntryKey, LightmapLayout, SeamGeometry,
    lightmap_version, seam_report,
};
use regia_asset_pipeline::lightmap_uv::{
    LightmapUvReport, LightmapUvSettings, UnwrapInput, build_atlas_layout, build_lightmap_layout,
};
use regia_pt::bake_desc::{BUILTIN_SCENES, GiBakeDescription, builtin};
use regia_pt::checkpoint::{BakeCheckpointKind, BakeCheckpointPlan, BakeCheckpointStats};
use regia_pt::lightmap::{
    LightmapBakeSettings, LightmapScene, bake_lightmap, bake_lightmap_checkpointed, quad_box_scene,
};

use super::bake_checkpoint::{self, CheckpointArgs};
use serde_json::{Value, json};

use super::bake_gi::GltfBakeLoader;

/// Which generator lays out the second UV set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UnwrapMode {
    /// The UV atlas generator (`regia_asset_pipeline::atlas`, `book/agents/atlas.md`): low-stretch
    /// charts packed by footprint. The default.
    Atlas,
    /// Planar charts packed as rectangles (`regia_asset_pipeline::lightmap_uv`, ADR-860).
    Charts,
}

impl UnwrapMode {
    /// The lowercase name printed in `settings.unwrap`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Atlas => "atlas",
            Self::Charts => "charts",
        }
    }
}

/// `--denoise`: the à-trous denoiser on (new bakes) or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DenoiseMode {
    /// Run the edge-aware à-trous denoiser (ADR-2314) instead of the 3×3 filter. The default.
    On,
    /// No denoiser: the 3×3 filter (unless `--no-filter`), as bakes before ADR-2314.
    Off,
}

/// The world reach the denoiser's default pass count aims at, metres (ADR-4108): the gallery's
/// noise of light found through small openings was left as patches 0.5 to 2 m across by a reach of
/// 1.75 m (3 passes at 8 texels/m); 30 texels (3.75 m at 8 texels/m) smoothed them, and a reach of
/// 7.5 m or more (4 passes at 4 or 2 texels/m) lost light at the lower tiers.
pub const DENOISE_REACH_M: f32 = 3.75;

/// The denoiser passes of a new bake at `density` texels per metre when `--denoise-iterations` is
/// not given: the count whose reach (`2 · (2^n − 1)` texels) is nearest [`DENOISE_REACH_M`], 1 to
/// `MAX_DENOISE_ITERATIONS`. 2 at 2 texels/m, 3 at 4, 4 at 8 (the CLI's default density), 5 at 16.
#[must_use]
pub fn default_denoise_iterations(density: f32) -> u32 {
    let texels = (DENOISE_REACH_M * density).max(0.0);
    let n = (texels / 2.0 + 1.0).log2().round();
    if n.is_finite() {
        (n as u32).clamp(1, regia_asset::lightmap::MAX_DENOISE_ITERATIONS)
    } else {
        1
    }
}

/// An on/off switch of a bake step (`--crease`, `--stitch`, `--auto-density`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Switch {
    /// The step runs (the default for new bakes).
    On,
    /// The step is skipped, as bakes before ADR-2404.
    Off,
}

/// `--encoding`: how the `.flmap` stores its texels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TexelEncodingArg {
    /// RGB9E5, 4 bytes a texel (the default; any GPU samples it).
    Rgb9e5,
    /// BC6H blocks, 1 byte a texel (desktop GPUs).
    Bc6h,
    /// ASTC 4×4 HDR blocks, 1 byte a texel (phones with ASTC HDR; others decode on the CPU).
    AstcHdr,
}

impl TexelEncodingArg {
    /// The file's encoding.
    #[must_use]
    pub fn encoding(self) -> regia_asset::lightmap::LightmapTexelEncoding {
        use regia_asset::lightmap::LightmapTexelEncoding as E;
        match self {
            Self::Rgb9e5 => E::Rgb9e5,
            Self::Bc6h => E::Bc6h,
            Self::AstcHdr => E::AstcHdr,
        }
    }
}

/// The default page budget of `--max-pages` (ADR-3300,
/// `regia_render::lightmap::DEFAULT_LIGHTMAP_MAX_PAGES`): 4 once the frame binds texture arrays
/// (four 4096² pages hold the ULTRA tier's lightmap budget, ADR-3001), 1 while it draws one 2-D
/// atlas, so a default bake never writes pages the frame cannot draw.
pub const DEFAULT_MAX_PAGES: u32 = regia_render::lightmap::DEFAULT_LIGHTMAP_MAX_PAGES;

/// Arguments of `regia bake-lightmaps`.
#[derive(Debug, Clone, Args)]
pub struct BakeLightmapsArgs {
    /// A built-in scene (`cornell`, `furnace`, `rolling-hills`, `quad-box`), a bake description
    /// `.ron`, or with `--inspect` a `.flmap` file.
    #[arg(required_unless_present = "level")]
    pub scene: Option<String>,
    /// Bake a level (`.level.ron` or `.level.d`) directly: its `GiMobility::Static` meshes, its sun and sky.
    /// Entries are keyed by entity. Default output `<project>/assets/maps/<level>.flmap`.
    #[arg(long = "scene", value_name = "LEVEL", conflicts_with_all = ["scene", "inspect"])]
    pub level: Option<PathBuf>,
    /// Project root of `--scene` (default: the nearest ancestor of the level with a project
    /// manifest, else the directory above its `assets/`).
    #[arg(long, value_name = "DIR", requires = "level")]
    pub project: Option<PathBuf>,
    /// A sun `elevation_deg,azimuth_deg,lux` replacing the level's for `--scene`.
    #[arg(long, value_parser = super::bake_gi::from_scene::parse_sun, allow_hyphen_values = true, requires = "level")]
    pub sun: Option<SunOverride>,
    /// Output lightmap (default `<scene>.flmap` in the current directory).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The second-UV generator.
    #[arg(long, value_enum, default_value_t = UnwrapMode::Atlas)]
    pub unwrap: UnwrapMode,
    /// Fail with `lightmap_seam_budget` when an entry's chart seams (`p95` step) pass this; the
    /// file is still written, so the seams can be inspected. Default: only report.
    #[arg(long, value_name = "F")]
    pub seam_budget: Option<f32>,
    /// Texels per metre of surface.
    #[arg(long, default_value_t = 8.0)]
    pub density: f32,
    /// Gutter texels around every chart.
    #[arg(long, default_value_t = 2)]
    pub padding: u32,
    /// Largest atlas side, texels (a level that does not fit one atlas opens pages of this side,
    /// up to `--max-pages`; past that the density scales down until the charts fit).
    #[arg(long, default_value_t = 4096)]
    pub max_atlas: u32,
    /// Most atlas pages before the density is lowered (ADR-3300): 1 keeps one atlas and scales the
    /// density down, as before pages existed. Each page is a texture array layer at run time. The
    /// default is 4 when this build's frame binds texture arrays, else 1.
    #[arg(long, default_value_t = DEFAULT_MAX_PAGES, value_parser = clap::value_parser!(u32).range(1..=i64::from(regia_asset::lightmap::MAX_LIGHTMAP_PAGES)))]
    pub max_pages: u32,
    /// Largest angle between a triangle and its chart, degrees (`--unwrap charts` only).
    #[arg(long, default_value_t = 60.0)]
    pub chart_angle: f32,
    /// Hemisphere samples per texel.
    #[arg(long, default_value_t = 256)]
    pub samples: u32,
    /// Path vertices after the texel (1 = direct light only).
    #[arg(long, default_value_t = 3)]
    pub bounces: u32,
    /// Random seed.
    #[arg(long, default_value_t = 1)]
    pub seed: u32,
    /// Also bake the sun seen directly (fully static lighting, e.g. for mobile).
    #[arg(long)]
    pub direct_sun: bool,
    /// Skip the edge-aware 3×3 filter (it only runs with `--denoise off`).
    #[arg(long)]
    pub no_filter: bool,
    /// The à-trous denoiser (ADR-2314): `on` for new bakes, `off` for the 3×3 filter of older ones.
    #[arg(long, value_enum, default_value_t = DenoiseMode::On)]
    pub denoise: DenoiseMode,
    /// Denoiser passes (1..=8; each doubles the reach). Default: by `--density`, a reach of about
    /// 3.75 m ([`default_denoise_iterations`]: 2 at 2 texels/m, 3 at 4, 4 at 8, 5 at 16).
    #[arg(long)]
    pub denoise_iterations: Option<u32>,
    /// How the file stores its texels (`.flmap` version 5 for the block encodings).
    #[arg(long, value_enum, default_value_t = TexelEncodingArg::Rgb9e5)]
    pub encoding: TexelEncodingArg,
    /// Crease-aware texel placement (ADR-2404): a texel whose centre is inside geometry (a floor
    /// under a wall) is sampled at the nearest point of its chart outside it.
    #[arg(long, value_enum, default_value_t = Switch::On)]
    pub crease: Switch,
    /// Seam stitching (ADR-2404): a least-squares solve makes the two sides of every smooth chart
    /// seam read the same light.
    #[arg(long, value_enum, default_value_t = Switch::On)]
    pub stitch: Switch,
    /// Automatic density (ADR-2404): a receiver whose predicted seam `p95` passes the seam budget
    /// (`--seam-budget`, else 0.25) gets a power-of-two density scale up to 8. A level entity's own
    /// `LightmapSurface.density_scale` (other than 1) is kept; either way a receiver still over
    /// the budget is warned about (`lightmap_density_low`).
    #[arg(long, value_enum, default_value_t = Switch::On)]
    pub auto_density: Switch,
    /// Bake on the GPU (`regia_pt::lightmap::bake_lightmap_gpu`, ADR-2024): the same light, up to
    /// float rounding. Needs a CLI built with the `lightmap-gpu` feature.
    #[arg(long)]
    pub gpu: bool,
    /// How `--gpu` cuts, paces and falls back (ADR-2125).
    #[command(flatten)]
    pub gpu_opts: GpuBakeArgs,
    /// After the bake, compare the new map with this `.flmap` of the same layout (PSNR and the
    /// relative difference; the `compare` object of the JSON).
    #[arg(long, value_name = "FLMAP")]
    pub compare: Option<PathBuf>,
    /// Do not bake the per-texel light direction (`.flmap` v2 direction section, ADR-1556).
    #[arg(long)]
    pub no_directional: bool,
    /// Adaptive sampling: the most samples a texel may take (0 = exactly `--samples`; at most
    /// 65536). Needs `--noise-target`.
    #[arg(long, default_value_t = 0)]
    pub max_samples: u32,
    /// Adaptive sampling: a texel stops once the relative standard error of its luminance is at
    /// or below this (0 = off; 0.02 = 2 %).
    #[arg(long, default_value_t = 0.0)]
    pub noise_target: f32,
    /// Worker threads (0 = all cores).
    #[arg(long, default_value_t = 0)]
    pub threads: usize,
    /// Skip meshes with less surface than this, m² (small props keep the probe lighting).
    #[arg(long, default_value_t = 0.0)]
    pub min_area: f32,
    /// Bake only this instance (repeat for more); the rest of the scene still lights and shadows
    /// it. Default: every receiving instance.
    #[arg(long = "instance", value_name = "INDEX")]
    pub instances: Vec<usize>,
    /// Lay out the atlas and print it without baking.
    #[arg(long)]
    pub dry_run: bool,
    /// Read the `.flmap` named by `<scene>` and print its summary.
    #[arg(long)]
    pub inspect: bool,
    /// Also build visibility-buffer clusters over each entry's unwrap and store them in the file
    /// (ADR-2107). Without them a receiver draws in the GPU-driven batches on the visibility path.
    #[arg(long)]
    pub meshlets: bool,
    /// Name the written lightmap in the level's `look.lightmap.file` (scene format v11, ADR-2313;
    /// an atomic rewrite through the scene writer, keeping the level's overrides), so the level
    /// installs it and attaches its receivers when it opens. Needs `--scene` with a `.level.ron`
    /// file and an output under the project's `assets/`.
    #[arg(long, requires = "level", conflicts_with_all = ["dry_run", "inspect"])]
    pub write_level: bool,
    /// `--checkpoint`, `--resume` and their cadence (ADR-3095).
    #[command(flatten)]
    pub checkpoint: CheckpointArgs,
    /// `--cells`, `--cell-ring` and `--cells-force`: per-cell bakes of a streamed level (ADR-3318).
    #[command(flatten)]
    pub cells: cells::CellArgs,
}

/// A command error with its agent code (`book/agents/lightmaps.md`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct LightmapCommandError {
    /// Agent error code (lowercase ascii, digits and `_`).
    pub code: &'static str,
    /// Key path or file the error is about, when it has one.
    pub path: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl LightmapCommandError {
    pub(crate) fn new(code: &'static str, path: Option<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            path,
            message: message.into(),
        }
    }

    /// The `{"ok": false, ...}` object printed on stdout.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "ok": false, "code": self.code, "path": self.path, "message": self.message })
    }
}

/// The options of `--gpu` (ADR-2125). None of them changes the light baked.
#[derive(Debug, Clone, Default, PartialEq, Args)]
pub struct GpuBakeArgs {
    /// `--gpu`: rays one tile (one dispatch) traces, about: 0 = the baker's default (4194304),
    /// else 4096..=67108864. Smaller tiles hand the GPU back sooner (an editor on the same GPU).
    #[arg(long, default_value_t = 0)]
    pub gpu_tile_rays: u64,
    /// `--gpu`: most GPU memory the bake holds, MiB (0 = the device's limits only). A larger
    /// atlas bakes in chunks; a scene that alone passes it is `lightmap_gpu_limit`.
    #[arg(long, default_value_t = 0.0)]
    pub gpu_memory_mib: f64,
    /// `--gpu`: pause after every tile, milliseconds (at most 1000), leaving the GPU to other
    /// work.
    #[arg(long, default_value_t = 0)]
    pub gpu_yield_ms: u64,
    /// `--gpu`: when the GPU cannot bake (no GPU, a lost device, a scene past the device or the
    /// budget, a shader failure, or a CLI built without the GPU baker), bake on the CPU instead;
    /// `gpu.fallback` says why.
    #[arg(long)]
    pub gpu_fallback: bool,
}

/// The GPU error codes `--gpu-fallback` bakes on the CPU after.
pub const GPU_FALLBACK_CODES: [&str; 5] = [
    "lightmap_gpu_unavailable",
    "lightmap_gpu_device",
    "lightmap_gpu_device_lost",
    "lightmap_gpu_limit",
    "lightmap_gpu",
];

/// Checks the `--gpu-*` options' bounds (`lightmap_bake_settings`, the option in `path`).
///
/// # Errors
///
/// The first option out of range.
pub fn check_gpu_args(gpu: &GpuBakeArgs) -> Result<(), LightmapCommandError> {
    let bad = |field: &str, message: String| {
        Err(LightmapCommandError::new(
            "lightmap_bake_settings",
            Some(field.into()),
            message,
        ))
    };
    if gpu.gpu_tile_rays != 0 && !(4096..=1 << 26).contains(&gpu.gpu_tile_rays) {
        return bad(
            "gpu_tile_rays",
            format!(
                "--gpu-tile-rays {} is outside 4096..=67108864 (or 0)",
                gpu.gpu_tile_rays
            ),
        );
    }
    if !(gpu.gpu_memory_mib.is_finite() && gpu.gpu_memory_mib >= 0.0) {
        return bad(
            "gpu_memory_mib",
            format!("--gpu-memory-mib {} must be a number >= 0", gpu.gpu_memory_mib),
        );
    }
    if gpu.gpu_yield_ms > 1000 {
        return bad(
            "gpu_yield_ms",
            format!("--gpu-yield-ms {} is above 1000", gpu.gpu_yield_ms),
        );
    }
    Ok(())
}

/// The lightmap baker's own built-in scene: the floor quad and box of
/// `regia_pt::lightmap::quad_box_scene` (instance 0 the floor, 1 the box).
pub const QUAD_BOX_SCENE: &str = "quad-box";

/// The scene of `args.scene`: a built-in or a bake description.
///
/// # Errors
///
/// `bake_scene_unknown`, or the description's own codes (`bake_desc_*`, `bake_source_missing`,
/// `bake_mesh_load`, `bake_manifest_mismatch`).
pub fn resolve_scene(scene: &str) -> Result<regia_pt::PtScene, LightmapCommandError> {
    if scene == QUAD_BOX_SCENE {
        return Ok(quad_box_scene().scene);
    }
    if let Some(job) = builtin(scene) {
        return Ok(job.scene);
    }
    let path = Path::new(scene);
    if path.extension().and_then(|e| e.to_str()) != Some("ron") {
        return Err(LightmapCommandError::new(
            "bake_scene_unknown",
            None,
            format!(
                "unknown scene `{scene}`: use one of {}, {QUAD_BOX_SCENE} or a bake description (.ron)",
                BUILTIN_SCENES.join(", ")
            ),
        ));
    }
    let from_desc =
        |e: regia_pt::bake_desc::BakeDescError| LightmapCommandError::new(e.code, e.path, e.message);
    let text = std::fs::read_to_string(path).map_err(|e| {
        LightmapCommandError::new(
            "bake_source_missing",
            Some(scene.to_string()),
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    let (desc, _) = GiBakeDescription::parse_versioned(&text).map_err(from_desc)?;
    let job = desc
        .build_with(path.parent().unwrap_or(Path::new(".")), &mut GltfBakeLoader)
        .map_err(from_desc)?;
    Ok(job.scene)
}

/// The UV2 settings of `args`.
#[must_use]
pub fn uv_settings(args: &BakeLightmapsArgs) -> LightmapUvSettings {
    LightmapUvSettings {
        texels_per_metre: args.density,
        padding: args.padding,
        max_atlas: args.max_atlas,
        max_chart_angle_deg: args.chart_angle,
        max_pages: args.max_pages,
        ..LightmapUvSettings::default()
    }
}

/// The bake settings of `args`.
#[must_use]
pub fn bake_settings(args: &BakeLightmapsArgs) -> LightmapBakeSettings {
    LightmapBakeSettings {
        samples: args.samples,
        bounces: args.bounces,
        seed: args.seed,
        direct_sun: args.direct_sun,
        filter: !args.no_filter,
        threads: args.threads,
        directional: !args.no_directional,
        max_samples: args.max_samples,
        noise_target: args.noise_target,
        denoise: (args.denoise == DenoiseMode::On).then_some(regia_asset::lightmap::LightmapDenoiseInfo {
            iterations: args
                .denoise_iterations
                .unwrap_or_else(|| default_denoise_iterations(args.density)),
            ..regia_asset::lightmap::LightmapDenoiseInfo::default()
        }),
        crease_placement: args.crease == Switch::On,
        stitch_seams: args.stitch == Switch::On,
        ..LightmapBakeSettings::default()
    }
}

/// Checks the bake settings' bounds before anything is laid out
/// (`LightmapBakeSettings::validate`).
///
/// # Errors
///
/// `lightmap_bake_settings`, with `path` naming the setting (`samples`, `max_samples`,
/// `bounces`, `noise_target`, `denoise`).
pub fn check_bake_settings(bake: &LightmapBakeSettings) -> Result<(), LightmapCommandError> {
    bake.validate().map_err(|m| {
        let field = m.split_whitespace().next().unwrap_or("settings").to_string();
        LightmapCommandError::new("lightmap_bake_settings", Some(field), m)
    })
}

/// Checks command-level UV settings without loading or flattening a scene.
fn check_uv_args(args: &BakeLightmapsArgs) -> Result<(), LightmapCommandError> {
    let settings = uv_settings(args);
    // The public layout builder owns the authoritative UV bounds. An empty input reaches its
    // settings check before reporting that there is nothing to unwrap.
    let validation = build_lightmap_layout(&[], &settings);
    if let Err(error) = validation {
        if error.code != "lightmap_uv_settings" {
            return check_min_area(args);
        }
        let setting = error.message.split_whitespace().next().unwrap_or("settings");
        let field = match setting {
            "texels_per_metre" => "density",
            "max_chart_angle_deg" => "chart_angle",
            other => other,
        };
        return Err(LightmapCommandError::new(
            error.code,
            Some(field.into()),
            error.message,
        ));
    }
    check_min_area(args)
}

fn check_min_area(args: &BakeLightmapsArgs) -> Result<(), LightmapCommandError> {
    if !(args.min_area.is_finite() && args.min_area >= 0.0) {
        return Err(LightmapCommandError::new(
            "lightmap_uv_settings",
            Some("min_area".into()),
            format!("--min-area {} must be a number >= 0", args.min_area),
        ));
    }
    Ok(())
}

/// The default output path for `scene`.
#[must_use]
pub fn default_out(scene: &str) -> PathBuf {
    let stem = Path::new(scene)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("lightmap")
        .trim_end_matches(".bake");
    PathBuf::from(format!("{stem}.flmap"))
}

/// A scene flattened and laid out: the scene, the receiving instance of every entry, the layout
/// and what the UV2 pass did.
pub struct LaidOut {
    /// The flattened scene.
    pub scene: LightmapScene,
    /// Instance index per entry.
    pub receivers: Vec<usize>,
    /// The atlas layout.
    pub layout: LightmapLayout,
    /// The UV2 report.
    pub report: LightmapUvReport,
    /// Receiving instances left out by `--min-area`, or with no surface at all.
    pub skipped_small: Vec<usize>,
    /// Instances a level's `LightmapSurface { receive: false }` left to the probes.
    pub skipped_opted_out: Vec<usize>,
    /// What the density rule did for each entry (ADR-2404), in entry order.
    pub density: Vec<EntryDensity>,
}

/// What the automatic density rule (ADR-2404, `regia_pt::lightmap::density`) did for one entry.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct EntryDensity {
    /// The entry's name.
    pub name: String,
    /// The density scale the entry was laid out with (before the atlas fit).
    pub density_scale: f32,
    /// The scale came from the level (`LightmapSurface.density_scale` other than 1) and was kept.
    pub explicit: bool,
    /// Mean curvature of the receiver's smooth bent edges, 1/m.
    pub curvature: f32,
    /// Predicted seam `p95` at the scale used.
    pub predicted_p95: f32,
    /// `lightmap_density_low` when the prediction stays over the budget.
    pub warning: Option<&'static str>,
}

/// The density rule's JSON (`density` of the output): the switch, the budget, every entry's
/// decision and the warnings.
#[must_use]
pub fn density_json(args: &BakeLightmapsArgs, density: &[EntryDensity]) -> Value {
    let warnings: Vec<Value> = density
        .iter()
        .filter_map(|d| {
            d.warning.map(|code| {
                json!({
                    "code": code,
                    "entry": d.name,
                    "message": format!(
                        "entry `{}`: predicted seam p95 {:.3} at density scale {} passes the budget {}{}",
                        d.name,
                        d.predicted_p95,
                        d.density_scale,
                        seam_budget(args),
                        if d.explicit { " (the level's density_scale is kept)" } else { " (the largest automatic scale)" }
                    ),
                })
            })
        })
        .collect();
    json!({
        "auto": args.auto_density == Switch::On,
        "budget": seam_budget(args),
        "stitched": args.stitch == Switch::On,
        "entries": density,
        "warnings": warnings,
    })
}

/// The warning code of a layout whose charts did not fit `--max-atlas` at the asked density, so
/// every entry was scaled down by `atlas.fit_scale` (ADR-2955).
pub const ATLAS_SCALED_CODE: &str = "lightmap_atlas_scaled";

/// The `lightmap_atlas_scaled` warning of `density.warnings` when the atlas fit scaled the density
/// down (`fit_scale` below 1), else `None`.
#[must_use]
pub fn atlas_scaled_warning(args: &BakeLightmapsArgs, report: &LightmapUvReport) -> Option<Value> {
    (report.fit_scale < 1.0).then(|| {
        json!({
            "code": ATLAS_SCALED_CODE,
            "fit_scale": report.fit_scale,
            "message": atlas_scaled_message(args, report.fit_scale),
        })
    })
}

/// What fixes a layout scaled down to fit: a larger atlas while `--max-atlas` is below the format's
/// limit, more pages while `--max-pages` is (ADR-3300), else a smaller level (or a lower density,
/// which is what the fit did).
fn atlas_scaled_remedy(args: &BakeLightmapsArgs) -> String {
    let limit = regia_asset::lightmap::MAX_LIGHTMAP_SIZE;
    let pages = regia_asset::lightmap::MAX_LIGHTMAP_PAGES;
    match (args.max_atlas < limit, args.max_pages < pages) {
        (true, true) => format!(
            "raise --max-atlas (up to {limit}) or --max-pages (up to {pages}), or split the level into smaller levels"
        ),
        (true, false) => format!("raise --max-atlas (up to {limit}) or split the level into smaller levels"),
        (false, true) => {
            format!("raise --max-pages (up to {pages}) or split the level into smaller levels")
        }
        (false, false) => {
            "split the level into smaller levels (one atlas each) or lower --density".to_string()
        }
    }
}

fn atlas_scaled_message(args: &BakeLightmapsArgs, fit_scale: f32) -> String {
    format!(
        "the charts do not fit {5} page(s) of {0}x{0} at {1} texels per meter: every entry got {2:.3} of \
         its density ({3:.3} texels per meter at scale 1); {4}",
        args.max_atlas,
        args.density,
        fit_scale,
        args.density * fit_scale,
        atlas_scaled_remedy(args),
        args.max_pages,
    )
}

/// The `density_scaled` headline of the bake and dry-run JSON (ADR-3121): `null` when every entry
/// got the density it asked for, else `{ factor, requested, density, max_atlas, remedy, message }`
/// with `factor` the atlas fit's `fit_scale` (below 1) and `density` the texels per meter an entry of
/// scale 1 got.
#[must_use]
pub fn density_scaled_json(args: &BakeLightmapsArgs, report: &LightmapUvReport) -> Value {
    if report.fit_scale >= 1.0 {
        return Value::Null;
    }
    json!({
        "factor": report.fit_scale,
        "requested": args.density,
        "density": args.density * report.fit_scale,
        "max_atlas": args.max_atlas,
        "max_pages": args.max_pages,
        "remedy": atlas_scaled_remedy(args),
        "message": atlas_scaled_message(args, report.fit_scale),
    })
}

/// The seam budget the density rule aims at: `--seam-budget`, else the shipping default.
#[must_use]
pub fn seam_budget(args: &BakeLightmapsArgs) -> f32 {
    args.seam_budget
        .unwrap_or(regia_asset::lightmap::DEFAULT_SEAM_BUDGET)
}

/// The density scale of instance `inst` and what the rule decided (ADR-2404): `requested` is the
/// level's scale (1 when it set none).
fn entry_density(
    args: &BakeLightmapsArgs,
    name: &str,
    inst: &regia_pt::lightmap::InstanceGeometry,
    requested: f32,
) -> EntryDensity {
    use regia_pt::lightmap::density::{DENSITY_LOW_CODE, auto_density_scale, predicted_seam_p95};
    let stitched = args.stitch == Switch::On;
    let budget = seam_budget(args);
    let explicit = requested != 1.0;
    if args.auto_density == Switch::Off {
        return EntryDensity {
            name: name.to_string(),
            density_scale: requested,
            explicit,
            curvature: 0.0,
            predicted_p95: 0.0,
            warning: None,
        };
    }
    let auto = auto_density_scale(&inst.positions, &inst.indices, args.density, budget, stitched);
    let (scale, predicted) = if explicit {
        (
            requested,
            predicted_seam_p95(auto.curvature, args.density * requested, stitched),
        )
    } else {
        (auto.density_scale, auto.predicted_p95_scaled)
    };
    EntryDensity {
        name: name.to_string(),
        density_scale: scale,
        explicit,
        curvature: auto.curvature,
        predicted_p95: predicted,
        warning: (predicted > budget).then_some(DENSITY_LOW_CODE),
    }
}

/// What a level says about one scene instance (`--scene`; [`level`]).
#[derive(Clone, Debug, PartialEq)]
pub struct InstancePart {
    /// The instance in the flattened scene.
    pub instance: usize,
    /// The entity and part it is.
    pub key: LightmapEntryKey,
    /// `name (guid)` of the entity.
    pub label: String,
    /// The entity's `LightmapSurface::density_scale`.
    pub density_scale: f32,
    /// The entity's `LightmapSurface::receive`.
    pub receive: bool,
}

/// World-space surface area of a triangle list, m².
#[must_use]
pub fn surface_area(positions: &[[f32; 3]], indices: &[u32]) -> f64 {
    indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let p = |k: usize| {
                positions
                    .get(t[k] as usize)
                    .copied()
                    .unwrap_or([0.0; 3])
                    .map(f64::from)
            };
            let (a, b, c) = (p(0), p(1), p(2));
            let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            0.5 * (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt()
        })
        .sum()
}

/// The entry name of `part`: stable across re-bakes and the level's order, so the bake's noise is
/// too (the random streams are keyed by the name).
fn entity_entry_name(part: &InstancePart) -> String {
    format!("entity_{}_{}", part.key.entity, part.key.part)
}

/// Flattens `scene` and lays out one entry per receiving instance (`instance_<k>`, every instance
/// that is neither emissive nor unlit and has triangles), or only the instances `--instance`
/// names. With `parts` (a level) an entry is named and keyed after its entity, an instance with no
/// part does not receive, and the entity's density scale and opt-out apply.
///
/// # Errors
///
/// `lightmap_scene` for a scene the baker cannot flatten, `lightmap_instance` for an
/// `--instance` that is missing or cannot receive, and the UV2 codes (`lightmap_uv_settings`,
/// `lightmap_uv_invalid_mesh`, `lightmap_uv_empty`, `lightmap_uv_atlas_full`).
pub fn lay_out(
    scene: &regia_pt::PtScene,
    args: &BakeLightmapsArgs,
    parts: Option<&[InstancePart]>,
) -> Result<LaidOut, LightmapCommandError> {
    if !(args.min_area.is_finite() && args.min_area >= 0.0) {
        return Err(LightmapCommandError::new(
            "lightmap_uv_settings",
            Some("min_area".into()),
            format!("--min-area {} must be a number >= 0", args.min_area),
        ));
    }
    let flat = LightmapScene::from_pt_scene(scene)
        .map_err(|e| LightmapCommandError::new("lightmap_scene", None, e.to_string()))?;
    for &k in &args.instances {
        match flat.instances().get(k) {
            None => {
                return Err(LightmapCommandError::new(
                    "lightmap_instance",
                    Some("instance".into()),
                    format!(
                        "--instance {k}: the scene has {} instances",
                        flat.instances().len()
                    ),
                ));
            }
            Some(i) if !i.receiver || i.indices.is_empty() => {
                return Err(LightmapCommandError::new(
                    "lightmap_instance",
                    Some("instance".into()),
                    format!("--instance {k} is emissive, unlit, a light or has no triangles"),
                ));
            }
            Some(_) => {}
        }
    }
    let by_instance = parts_by_instance(parts);
    let part_of = |k: usize| by_instance.get(&k).copied();
    let mut receivers = Vec::new();
    let mut skipped_small = Vec::new();
    let mut skipped_opted_out = Vec::new();
    for (k, i) in flat.instances().iter().enumerate() {
        if !i.receiver || i.indices.is_empty() {
            continue;
        }
        if !args.instances.is_empty() && !args.instances.contains(&k) {
            continue;
        }
        if parts.is_some() {
            match part_of(k) {
                None => continue,
                Some(part) if !part.receive => {
                    skipped_opted_out.push(k);
                    continue;
                }
                Some(_) => {}
            }
        }
        let area = surface_area(&i.positions, &i.indices);
        if area < f64::from(args.min_area) || area <= 0.0 {
            skipped_small.push(k);
        } else {
            receivers.push(k);
        }
    }
    let names: Vec<String> = receivers
        .iter()
        .map(|&k| part_of(k).map_or_else(|| format!("instance_{k}"), entity_entry_name))
        .collect();
    let density: Vec<EntryDensity> = receivers
        .iter()
        .zip(&names)
        .map(|(&k, name)| {
            let requested = part_of(k).map_or(1.0, |part| part.density_scale);
            entry_density(args, name, &flat.instances()[k], requested)
        })
        .collect();
    let inputs: Vec<UnwrapInput<'_>> = receivers
        .iter()
        .zip(&names)
        .zip(&density)
        .map(|((&k, name), d)| {
            let inst = &flat.instances()[k];
            UnwrapInput {
                name,
                positions: &inst.positions,
                indices: &inst.indices,
                density_scale: d.density_scale,
            }
        })
        .collect();
    let settings = uv_settings(args);
    let (mut layout, report) = match args.unwrap {
        UnwrapMode::Atlas => build_atlas_layout(&inputs, &settings, args.threads),
        UnwrapMode::Charts => build_lightmap_layout(&inputs, &settings),
    }
    .map_err(|e| LightmapCommandError::new(e.code, None, e.message))?;
    for (entry, &k) in layout.entries.iter_mut().zip(&receivers) {
        entry.key = part_of(k).map(|part| part.key);
    }
    Ok(LaidOut {
        scene: flat,
        receivers,
        layout,
        report,
        skipped_small,
        skipped_opted_out,
        density,
    })
}

/// The hash of each receiver's source mesh, in entry order (`.flmap` version 6, ADR-2656): over the
/// mesh's own positions, the way a receiver hashes the mesh it draws (`render.get_gi_mode`
/// `lightmaps.stale_instances`). 0 for an instance with no mesh of its own.
pub fn source_hashes(scene: &regia_pt::PtScene, receivers: &[usize]) -> Vec<u64> {
    receivers
        .iter()
        .map(|&k| {
            scene
                .instances
                .get(k)
                .and_then(|i| scene.meshes.get(i.mesh))
                .map_or(0, |m| {
                    regia_asset::lightmap::lightmap_source_hash(m.positions.iter().copied(), &m.indices)
                })
        })
        .collect()
}

/// What a bake returns: the map, its report, and the `gpu` object of the JSON (`None` on the
/// CPU).
type Baked = (
    Lightmap,
    regia_pt::lightmap::LightmapBakeReport,
    Option<Value>,
    Option<BakeCheckpointStats>,
);

/// `true` when a `--gpu` rerun must stay on the CPU: `--gpu-fallback` and the existing
/// checkpoint was written by the CPU baker. The baker is part of the checkpoint identity, and
/// `bake_checkpoint_mismatch` is not a fallback code, so trying the GPU would fail the rerun
/// that the flag promises to keep baking (ADR-5786).
fn cpu_checkpoint_resumes_on_cpu(gpu_fallback: bool, plan: Option<&BakeCheckpointPlan>) -> bool {
    gpu_fallback
        && plan.is_some_and(|p| {
            regia_asset::bake_checkpoint::read_header(&p.path)
                .is_ok_and(|h| h.kind == BakeCheckpointKind::LightmapCpu)
        })
}

/// The GPU bake of `--gpu`, cut and paced by `gpu` (ADR-2125). Installs a device-lost callback on
/// its own device, so a driver reset mid-bake is `lightmap_gpu_device_lost`.
#[cfg(feature = "lightmap-gpu")]
fn bake_on_gpu(
    laid: &LaidOut,
    bake: &LightmapBakeSettings,
    gpu: &GpuBakeArgs,
    progress: &(impl Fn(usize, usize) + Sync),
    plan: Option<&BakeCheckpointPlan>,
) -> Result<Baked, LightmapCommandError> {
    use regia_pt::lightmap::{
        LightmapGpuControl, LightmapGpuOptions, bake_lightmap_gpu_checkpointed, bake_lightmap_gpu_with,
        lightmap_gpu_error_code,
    };
    let options = LightmapGpuOptions {
        tile_rays: if gpu.gpu_tile_rays == 0 {
            regia_pt::lightmap::DEFAULT_GPU_TILE_RAYS
        } else {
            gpu.gpu_tile_rays
        },
        memory_budget: (gpu.gpu_memory_mib * 1024.0 * 1024.0) as u64,
        yield_between_tiles: std::time::Duration::from_millis(gpu.gpu_yield_ms),
    };
    let device = regia_render::RenderDevice::headless().map_err(|e| {
        LightmapCommandError::new("lightmap_gpu_device", None, format!("no GPU to bake on: {e}"))
    })?;
    device.device.set_device_lost_callback(|reason, message| {
        regia_render::gpu_health::note_device_lost(reason, &message);
    });
    let mut total = 0usize;
    let control = |p: &regia_pt::lightmap::LightmapGpuProgress| {
        total = p.texels;
        progress(p.texels_done(), total);
        LightmapGpuControl::Continue
    };
    let layout = laid.layout.clone();
    let baked = match plan {
        Some(plan) => bake_lightmap_gpu_checkpointed(
            &device,
            &laid.scene,
            layout,
            &laid.receivers,
            bake,
            &options,
            plan,
            control,
        )
        .map(|(m, r, s, c)| (m, r, s, Some(c))),
        None => bake_lightmap_gpu_with(
            &device,
            &laid.scene,
            layout,
            &laid.receivers,
            bake,
            &options,
            control,
        )
        .map(|(m, r, s)| (m, r, s, None)),
    };
    let (map, report, stats, checkpoint) = baked.map_err(|e| {
        bake_checkpoint::bake_error(&e, plan, LightmapCommandError::new, |e| {
            LightmapCommandError::new(lightmap_gpu_error_code(e), None, e.to_string())
        })
    })?;
    progress(total, total);
    let info = device.adapter.get_info();
    let stats = json!({
        "adapter": info.name,
        "tiles": stats.tiles,
        "chunks": stats.chunks,
        "chunk_texels": stats.chunk_texels,
        "tile_texels": stats.tile_texels,
        "tile_samples": stats.tile_samples,
        "tile_rays": stats.tile_rays,
        "scene_bytes": stats.scene_bytes,
        "peak_bytes": stats.peak_bytes,
        "memory_budget_bytes": options.memory_budget,
        "readbacks": stats.readbacks,
        "yield_ms": gpu.gpu_yield_ms,
        "schedule_hash": format!("{:016x}", stats.schedule_hash),
        "fallback": Value::Null,
    });
    Ok((map, report, Some(stats), checkpoint))
}

/// `--gpu` in a CLI built without the baker: says how to get it.
#[cfg(not(feature = "lightmap-gpu"))]
fn bake_on_gpu(
    _laid: &LaidOut,
    _bake: &LightmapBakeSettings,
    _gpu: &GpuBakeArgs,
    _progress: &(impl Fn(usize, usize) + Sync),
    _plan: Option<&BakeCheckpointPlan>,
) -> Result<Baked, LightmapCommandError> {
    Err(LightmapCommandError::new(
        "lightmap_gpu_unavailable",
        Some("gpu".into()),
        "this CLI was built without the GPU baker: build it with `--features lightmap-gpu`, or leave out --gpu",
    ))
}

/// `--compare <reference>`: how close the new map is to the reference (`compare` in the JSON).
fn compare_json(reference: &Path, map: &Lightmap) -> Result<Value, LightmapCommandError> {
    let path = Some(reference.display().to_string());
    let old = Lightmap::read(reference)
        .map_err(|e| LightmapCommandError::new(e.code(), path.clone(), e.to_string()))?;
    let c = regia_pt::lightmap::compare_lightmaps(&old, map).map_err(|e| {
        let message = e.to_string();
        let code = if message.contains("lightmap_compare_size") {
            "lightmap_compare_size"
        } else {
            "lightmap_compare_empty"
        };
        LightmapCommandError::new(code, path.clone(), message)
    })?;
    let floor = regia_pt::lightmap::LIGHTMAP_GPU_PSNR_FLOOR_DB;
    Ok(json!({
        "reference": reference.display().to_string(),
        "texels": c.texels,
        "coverage_mismatch": c.coverage_mismatch,
        "identical": c.mse == 0.0 && c.coverage_mismatch == 0,
        "psnr_db": c.psnr_db,
        "mse": c.mse,
        "peak": c.peak,
        "mean_rel": c.mean_rel,
        "p99_rel": c.p99_rel,
        "max_rel": c.max_rel,
        "psnr_floor_db": floor,
        "within_floor": c.within(floor),
    }))
}

/// Most seam segments printed for the editor's overlay, over every entry.
pub const MAX_OVERLAY_SEGMENTS: usize = 1024;

/// Measures every entry's chart seams against the scene, stores the figures in the lightmap and
/// returns the `seams` object of the JSON (`book/agents/lightmaps.md`).
pub fn measure_seams(map: &mut Lightmap, scene: &LightmapScene, receivers: &[usize]) -> Value {
    let _span = tracing::info_span!("lightmap_measure_seams", entries = receivers.len()).entered();
    let mut entries = Vec::with_capacity(receivers.len());
    let mut overlay = Vec::new();
    let (mut edges, mut hard, mut worst_p95, mut worst_max) = (0u64, 0u64, 0.0f32, 0.0f32);
    for (e, &k) in receivers.iter().enumerate() {
        let Some(inst) = scene.instances().get(k) else {
            continue;
        };
        let geometry = SeamGeometry {
            positions: &inst.positions,
        };
        let report = seam_report(map, e, geometry, DEFAULT_HARD_EDGE_DEG);
        map.entries[e].seams = report.seams;
        hard += u64::from(report.hard_edges);
        if let Some(s) = report.seams {
            edges += u64::from(s.edges);
            worst_p95 = worst_p95.max(s.p95);
            worst_max = worst_max.max(s.max);
        }
        overlay.extend(report.worst.iter().map(|w| (e, *w)));
        entries.push(json!({
            "name": map.entries[e].name,
            "edges": report.seams.map_or(0, |s| s.edges),
            "mean": report.seams.map(|s| s.mean),
            "p95": report.seams.map(|s| s.p95),
            "max": report.seams.map(|s| s.max),
            "worst_chart": report.seams.map(|s| s.worst_chart),
            "hard_edges": report.hard_edges,
            "charts": report.charts,
        }));
    }
    overlay.sort_by(|a, b| b.1.error.total_cmp(&a.1.error));
    overlay.truncate(MAX_OVERLAY_SEGMENTS);
    json!({
        "edges": edges,
        "hard_edges": hard,
        "p95": worst_p95,
        "max": worst_max,
        "entries": entries,
        "overlay": overlay
            .iter()
            .map(|(e, w)| {
                json!({
                    "entry": e,
                    "a": w.a,
                    "b": w.b,
                    "world_a": w.world_a,
                    "world_b": w.world_b,
                    "error": w.error,
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// The level's parts by instance (ADR-2955, ADR-3121): a level of many props would make a search
/// per instance quadratic. The first part naming an instance wins, as a search in order would.
fn parts_by_instance(parts: Option<&[InstancePart]>) -> std::collections::HashMap<usize, &InstancePart> {
    let mut by_instance = std::collections::HashMap::new();
    for part in parts.unwrap_or_default() {
        by_instance.entry(part.instance).or_insert(part);
    }
    by_instance
}

fn entries_json(layout: &LightmapLayout, receivers: &[usize], parts: Option<&[InstancePart]>) -> Vec<Value> {
    let by_instance = parts_by_instance(parts);
    layout
        .entries
        .iter()
        .zip(&layout.unwraps)
        .zip(receivers)
        .map(|((e, u), r)| {
            let part = by_instance.get(r).copied();
            json!({
                "name": e.name,
                "instance": r,
                "entity": e.key.map(|k| k.entity.to_string()),
                "part": e.key.map(|k| k.part),
                "label": part.map(|p| p.label.clone()),
                "page": e.page,
                "rect": e.rect,
                "charts": e.charts,
                "triangles": u.triangle_count(),
                "vertices": u.xref.len(),
                "area_m2": e.area_m2,
                "texels_per_metre": e.texels_per_metre,
            })
        })
        .collect()
}

/// The summary of a lightmap file (`--inspect`).
///
/// # Errors
///
/// The file does not read or decode (`lightmap_io`, `lightmap_bad_magic`, `lightmap_version`,
/// `lightmap_header`, `lightmap_payload`, `lightmap_layout`).
pub fn inspect(path: &Path) -> Result<Value, LightmapCommandError> {
    let fail = |e: regia_asset::lightmap::LightmapError| {
        LightmapCommandError::new(e.code(), Some(path.display().to_string()), e.to_string())
    };
    let bytes = std::fs::read(path).map_err(|e| fail(e.into()))?;
    let map = Lightmap::from_bytes(&bytes).map_err(fail)?;
    let version = lightmap_version(&bytes).map_err(fail)?;
    let [empty, baked, dilated] = map.coverage_counts();
    let entries: Vec<Value> = map
        .entries
        .iter()
        .enumerate()
        .map(|(k, e)| {
            json!({
                "name": e.name,
                "entity": e.key.map(|k| k.entity.to_string()),
                "part": e.key.map(|k| k.part),
                "seams": e.seams,
                "page": e.page,
                "rect": e.rect,
                "scale_offset": e.scale_offset,
                "charts": e.charts,
                "triangles": e.index_count / 3,
                "vertices": e.vertex_count,
                "area_m2": e.area_m2,
                "texels_per_metre": e.texels_per_metre,
                "meshlet_bytes": map.entry_meshlets(k).map_or(0, <[u8]>::len),
                "source_hash": map.source_hash(k),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "inspect": true,
        "path": path.display().to_string(),
        "bytes": bytes.len(),
        "version": version,
        "width": map.width,
        "height": map.height,
        "pages": map.pages(),
        "exposure": map.exposure,
        "density": map.density,
        "padding": map.padding,
        "directional": map.is_directional(),
        "contents": LightmapContents::names(map.bake.contents),
        "bake": {
            "samples": map.bake.samples,
            "bounces": map.bake.bounces,
            "seed": map.bake.seed,
            "sun_toward": map.bake.sun_toward,
            "sun_illuminance": map.bake.sun_illuminance,
            "filtered": map.bake.filtered,
        },
        "texel_encoding": map.texel_encoding().name(),
        "denoise": map.denoise,
        "coverage": { "empty": empty, "baked": baked, "dilated": dilated },
        "utilization": map.utilization(),
        "meshlet_entries": map.meshlet_entries(),
        "entries": entries,
    }))
}

/// Wall time of each phase of a bake, for the `timings` object of the JSON (seconds, in order).
struct PhaseMarks {
    last: Instant,
    phases: Vec<(&'static str, f64)>,
}

impl PhaseMarks {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            phases: Vec::new(),
        }
    }

    /// Ends the phase `name`: the time since the previous mark.
    fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        self.phases
            .push((name, now.duration_since(self.last).as_secs_f64()));
        self.last = now;
    }

    fn json(&self) -> Value {
        Value::Object(
            self.phases
                .iter()
                .map(|&(n, s)| (n.to_string(), json!(s)))
                .collect(),
        )
    }
}

/// Runs the command and returns the JSON it prints (the bake, the dry run or the inspection).
///
/// # Errors
///
/// A [`LightmapCommandError`] with its code: the scene, the layout, the bake (`lightmap_bake`) or
/// the write (`lightmap_io`).
pub fn execute(args: &BakeLightmapsArgs) -> Result<Value, LightmapCommandError> {
    let label = args
        .level
        .as_deref()
        .map(|l| l.display().to_string())
        .or_else(|| args.scene.clone())
        .ok_or_else(|| {
            LightmapCommandError::new("bake_scene_unknown", None, "give a scene, or --scene <level>")
        })?;
    if args.inspect {
        return inspect(Path::new(&label));
    }
    if args.cells.cells {
        return cells::execute(args);
    }
    if args.write_level && (args.level.is_none() || args.dry_run) {
        return Err(LightmapCommandError::new(
            "lightmap_write_level",
            None,
            "--write-level needs --scene <level> and a bake (not --dry-run)",
        ));
    }
    let started = Instant::now();
    let bake = bake_settings(args);
    check_bake_settings(&bake)?;
    check_gpu_args(&args.gpu_opts)?;
    check_uv_args(args)?;
    if let Some(budget) = args.seam_budget
        && !(budget.is_finite() && budget >= 0.0)
    {
        return Err(LightmapCommandError::new(
            "lightmap_bake_settings",
            Some("seam_budget".into()),
            format!("--seam-budget {budget} must be a number >= 0"),
        ));
    }
    let mut marks = PhaseMarks::new();
    let from_level = match &args.level {
        Some(path) => Some(level::load(path, args)?),
        None => None,
    };
    marks.mark("load");
    let scene = match &from_level {
        Some(l) => &l.scene,
        None => &resolve_scene(&label)?,
    };
    let parts = from_level.as_ref().map(|l| l.parts.as_slice());
    let laid = lay_out(scene, args, parts)?;
    marks.mark("layout");
    let contents = LightmapContents::names(laid.scene.contents(&bake));
    let clusters = args
        .meshlets
        .then(|| meshlets::build(scene, &laid.receivers, &laid.layout, args.threads));
    let clusters_json = clusters.as_ref().map(meshlets::EntryMeshlets::describe);
    let entries = entries_json(&laid.layout, &laid.receivers, parts);
    let mut settings = json!({
        "unwrap": args.unwrap.name(),
        "backend": if args.gpu { "gpu" } else { "cpu" },
        "seam_budget": args.seam_budget,
        "density": args.density,
        "padding": args.padding,
        "max_atlas": args.max_atlas,
        "max_pages": args.max_pages,
        "chart_angle": args.chart_angle,
        "min_area": args.min_area,
        "instances": args.instances,
        "samples": bake.samples,
        "bounces": bake.bounces,
        "seed": bake.seed,
        "direct_sun": bake.direct_sun,
        "filter": bake.filter,
        "directional": bake.directional,
        "max_samples": bake.max_samples,
        "noise_target": bake.noise_target,
        "denoise": bake.denoise,
        "encoding": args.encoding.encoding().name(),
        "meshlets": args.meshlets,
        "crease_placement": bake.crease_placement,
        "stitch_seams": bake.stitch_seams,
        "auto_density": args.auto_density == Switch::On,
    });
    let mut density = density_json(args, &laid.density);
    let density_scaled = density_scaled_json(args, &laid.report);
    if let Some(w) = atlas_scaled_warning(args, &laid.report) {
        eprintln!("warning: {}", w["message"].as_str().unwrap_or_default());
        if let Some(list) = density["warnings"].as_array_mut() {
            list.push(w);
        }
    }
    if args.dry_run {
        let mut out = json!({
            "ok": true,
            "dry_run": true,
            "scene": label,
            "pages": laid.report.pages,
            "density_scaled": density_scaled,
            "atlas": laid.report,
            "contents": contents,
            "triangles": laid.scene.triangle_count(),
            "skipped_small": laid.skipped_small,
            "skipped_opted_out": laid.skipped_opted_out,
            "entries": entries,
            "density": density,
            "meshlets": clusters_json,
            "settings": settings,
            "timings": marks.json(),
            "seconds": started.elapsed().as_secs_f64(),
        });
        if let Some(l) = &from_level {
            l.describe(&mut out);
        }
        return Ok(out);
    }
    let out = match (&args.out, &from_level) {
        (Some(out), _) => out.clone(),
        (None, Some(l)) => l.default_out.clone(),
        (None, None) => default_out(&label),
    };
    if from_level.is_some()
        && let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(|e| {
            LightmapCommandError::new(
                "lightmap_io",
                Some(dir.display().to_string()),
                format!("cannot create {}: {e}", dir.display()),
            )
        })?;
    }
    let plan = args
        .checkpoint
        .plan(&out)
        .map_err(|(code, path, message)| LightmapCommandError::new(code, Some(path), message))?;
    let (_, prior_seconds) = plan
        .as_ref()
        .map_or((0, 0.0), |p| bake_checkpoint::announce(p, "lightmap"));
    let bake_started = Instant::now();
    let last_print = std::sync::Mutex::new(Instant::now());
    let progress = |done: usize, total: usize| {
        if let Ok(mut at) = last_print.lock()
            && (at.elapsed().as_secs_f32() > 2.0 || done == total)
        {
            // A checkpointed bake counts its earlier runs: the estimate resumes with it.
            let eta = if plan.is_some() {
                let seconds = prior_seconds + bake_started.elapsed().as_secs_f64();
                bake_checkpoint::eta_suffix(done as u64, total as u64, seconds)
            } else {
                String::new()
            };
            eprintln!("  lightmap: {done}/{total} texels{eta}");
            *at = Instant::now();
        }
    };
    // A checkpoint the CPU wrote cannot continue on the GPU (the baker is part of the checkpoint
    // identity, and `bake_checkpoint_mismatch` is not a fallback code): with `--gpu-fallback`
    // such a rerun stays on the CPU instead of failing (ADR-5786). Without the flag the GPU
    // attempt still refuses, as documented.
    let cpu_checkpoint = cpu_checkpoint_resumes_on_cpu(args.gpu_opts.gpu_fallback, plan.as_ref());
    let tried_gpu = if args.gpu && !cpu_checkpoint {
        match bake_on_gpu(&laid, &bake, &args.gpu_opts, &progress, plan.as_ref()) {
            Ok(baked) => Ok(baked),
            Err(e) if args.gpu_opts.gpu_fallback && GPU_FALLBACK_CODES.contains(&e.code) => Err(Some(e)),
            Err(e) => return Err(e),
        }
    } else {
        if cpu_checkpoint {
            eprintln!("  lightmap: the checkpoint is a CPU bake; continuing on the CPU (--gpu-fallback)");
        }
        Err(None)
    };
    let (mut map, report, gpu_json, mut checkpoint) = match tried_gpu {
        Ok(baked) => baked,
        Err(failed) => {
            // After a GPU fallback the CPU continues the checkpoint only when the CPU wrote it.
            let cpu_plan = plan.as_ref().map(|p| {
                if failed.is_some() {
                    bake_checkpoint::plan_for_kind(p, BakeCheckpointKind::LightmapCpu)
                } else {
                    p.clone()
                }
            });
            let fallback = failed.map(|e| {
                eprintln!("  lightmap: the GPU bake failed ({}); baking on the CPU", e.code);
                json!({ "fallback": { "code": e.code, "message": e.message } })
            });
            let baked = match &cpu_plan {
                Some(p) => {
                    bake_lightmap_checkpointed(&laid.scene, laid.layout, &laid.receivers, &bake, p, |e| {
                        progress(e.done as usize, e.total as usize);
                    })
                    .map(|(m, r, c)| (m, r, Some(c)))
                }
                None => bake_lightmap(&laid.scene, laid.layout, &laid.receivers, &bake, progress)
                    .map(|(m, r)| (m, r, None)),
            };
            let (map, report, checkpoint) = baked.map_err(|e| {
                bake_checkpoint::bake_error(&e, cpu_plan.as_ref(), LightmapCommandError::new, |e| {
                    LightmapCommandError::new("lightmap_bake", None, e.to_string())
                })
            })?;
            settings["backend"] = json!("cpu");
            (map, report, fallback, checkpoint)
        }
    };
    marks.mark("bake");
    let mut seams = measure_seams(&mut map, &laid.scene, &laid.receivers);
    marks.mark("seams");
    if let Some(clusters) = clusters {
        map.meshlets = clusters.blobs;
    }
    map.source_hashes = source_hashes(scene, &laid.receivers);
    // The seams above were measured on the baked texels; the encoding is the last step.
    let encode = map
        .encode_texels(args.encoding.encoding())
        .map_err(|e| LightmapCommandError::new(e.code(), None, e.to_string()))?;
    map.write(&out)
        .map_err(|e| LightmapCommandError::new(e.code(), Some(out.display().to_string()), e.to_string()))?;
    // Only now that the lightmap is on disk is the checkpoint dropped: a failed encode or write
    // keeps it, and the next run resumes instead of starting over.
    if let (Some(plan), Some(stats)) = (plan.as_ref(), checkpoint.as_mut()) {
        bake_checkpoint::finish_checkpoint(plan, stats);
    }
    marks.mark("encode_write");
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let [empty, baked, dilated] = map.coverage_counts();
    if let Some(budget) = args.seam_budget {
        let over = over_seam_budget(&map, budget);
        seams["budget"] = json!(budget);
        seams["over_budget"] = json!(over);
        if let Some(first) = over.first() {
            return Err(LightmapCommandError::new(
                SEAM_BUDGET_CODE,
                Some(first.clone()),
                format!(
                    "{} of {} entries pass the seam budget {budget}: {} (the file was written: {})",
                    over.len(),
                    map.entries.len(),
                    over.join(", "),
                    out.display()
                ),
            ));
        }
    }
    let compare = match &args.compare {
        Some(reference) => Some(compare_json(reference, &map)?),
        None => None,
    };
    let mut summary = json!({
        "ok": true,
        "out": out.display().to_string(),
        "bytes": bytes,
        "scene": label,
        "pages": map.pages(),
        "density_scaled": density_scaled,
        "atlas": laid.report,
        "report": report,
        "seams": seams,
        "skipped_small": laid.skipped_small,
        "skipped_opted_out": laid.skipped_opted_out,
        "coverage": { "empty": empty, "baked": baked, "dilated": dilated },
        "utilization": map.utilization(),
        "exposure": map.exposure,
        "encode": encode,
        "entries": entries,
        "density": density,
        "meshlets": clusters_json,
        "settings": settings,
        "timings": marks.json(),
        "seconds": started.elapsed().as_secs_f64(),
    });
    if let Some(gpu) = gpu_json {
        summary["gpu"] = gpu;
    }
    if let Some(checkpoint) = checkpoint {
        summary["checkpoint"] = json!(checkpoint);
    }
    if let Some(compare) = compare {
        summary["compare"] = compare;
    }
    if let Some(l) = &from_level {
        l.describe(&mut summary);
        if args.write_level
            && let Some(path) = &args.level
        {
            summary["level_written"] = json!(level::write_level(path, &l.project, &out)?);
        }
    }
    Ok(summary)
}

/// The agent code of an entry past the seam budget (the bake's `--seam-budget`, `ship_check`).
pub const SEAM_BUDGET_CODE: &str = "lightmap_seam_budget";

/// Names of the entries whose stored seam `p95` passes `budget`.
#[must_use]
pub fn over_seam_budget(map: &Lightmap, budget: f32) -> Vec<String> {
    map.entries
        .iter()
        .filter(|e| e.seams.is_some_and(|s| s.p95 > budget))
        .map(|e| e.name.clone())
        .collect()
}

/// Runs `regia bake-lightmaps`: prints the JSON of [`execute`] (or its error object) on stdout.
///
/// # Errors
///
/// Any [`LightmapCommandError`] (after printing it).
pub fn run(args: &BakeLightmapsArgs) -> Result<()> {
    match execute(args) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Err(e) => {
            println!("{}", serde_json::to_string_pretty(&e.to_json())?);
            Err(e).context("bake-lightmaps failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// corr-low cli (ADR-5786): `--gpu --gpu-fallback` over a CPU-written checkpoint stays on the
    /// CPU; a GPU-written checkpoint, or no fallback flag, still lets the GPU run (and refuse a
    /// foreign checkpoint, as documented).
    #[test]
    fn a_cpu_checkpoint_resumes_on_cpu_under_gpu_fallback() {
        let dir = regia_test_utils::unique_temp_dir("cli-bakeck-cpu-fallback");
        let path = dir.join("bake.bakeck");
        let write_header = |kind| {
            let checkpoint = regia_asset::bake_checkpoint::BakeCheckpoint {
                header: regia_asset::bake_checkpoint::BakeCheckpointHeader {
                    kind,
                    stage: 0,
                    input_hash: [7; 32],
                    units_total: 100,
                    units_done: 10,
                    batches_done: 1,
                    work_seconds: 1.0,
                },
                payload: Vec::new(),
            };
            std::fs::write(&path, checkpoint.to_bytes()).expect("checkpoint");
        };
        let plan = BakeCheckpointPlan::new(&path);
        write_header(BakeCheckpointKind::LightmapCpu);
        assert!(cpu_checkpoint_resumes_on_cpu(true, Some(&plan)));
        assert!(
            !cpu_checkpoint_resumes_on_cpu(false, Some(&plan)),
            "without the flag the GPU attempt still refuses, as documented"
        );
        write_header(BakeCheckpointKind::LightmapGpu);
        assert!(
            !cpu_checkpoint_resumes_on_cpu(true, Some(&plan)),
            "a GPU checkpoint does not skip the GPU"
        );
        assert!(!cpu_checkpoint_resumes_on_cpu(true, None));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn args(scene: &str) -> BakeLightmapsArgs {
        BakeLightmapsArgs {
            scene: Some(scene.into()),
            level: None,
            project: None,
            sun: None,
            unwrap: UnwrapMode::Atlas,
            seam_budget: None,
            out: None,
            density: 32.0,
            padding: 2,
            max_atlas: 1024,
            max_pages: DEFAULT_MAX_PAGES,
            chart_angle: 60.0,
            samples: 4,
            bounces: 2,
            seed: 5,
            direct_sun: false,
            gpu: false,
            gpu_opts: GpuBakeArgs::default(),
            compare: None,
            no_filter: false,
            denoise: DenoiseMode::On,
            denoise_iterations: None,
            encoding: TexelEncodingArg::Rgb9e5,
            crease: Switch::On,
            stitch: Switch::On,
            auto_density: Switch::On,
            no_directional: false,
            max_samples: 0,
            noise_target: 0.0,
            threads: 2,
            min_area: 0.0,
            instances: Vec::new(),
            dry_run: false,
            inspect: false,
            meshlets: false,
            write_level: false,
            checkpoint: CheckpointArgs::default(),
            cells: cells::CellArgs::default(),
        }
    }

    /// `quad-box` is a built-in; `--instance` keeps only the named receivers, and the rest of the
    /// scene still counts (the box's triangles are in the scene when only the floor is baked).
    #[test]
    fn instance_bakes_one_mesh_of_the_quad_box() {
        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        a.dry_run = true;
        let all = execute(&a).expect("dry run");
        let names: Vec<&str> = all["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| e["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["instance_0", "instance_1"]);
        a.instances = vec![0];
        let one = execute(&a).expect("dry run");
        let entries = one["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "instance_0");
        assert_eq!(entries[0]["instance"], 0);
        assert_eq!(one["triangles"], all["triangles"]);
        assert_eq!(one["settings"]["instances"], json!([0]));

        // A real single-mesh bake: the floor only, lit by the sky and the sun's bounce off the box.
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_instance");
        a.dry_run = false;
        a.out = Some(dir.join("floor.flmap"));
        let v = execute(&a).expect("bake");
        assert_eq!(v["report"]["entries"], 1);
        assert_eq!(v["report"]["contents"], json!(["sky", "sun_bounce"]));
        let map = Lightmap::read(&dir.join("floor.flmap")).expect("read back");
        assert_eq!(map.entries.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);

        a.dry_run = true;
        a.instances = vec![9];
        let e = execute(&a).expect_err("no such instance");
        assert_eq!(
            (e.code, e.path.as_deref()),
            ("lightmap_instance", Some("instance"))
        );
        // The Cornell lamp is emissive: it cannot hold a lightmap.
        let mut c = args("cornell");
        c.dry_run = true;
        let lamp = LightmapScene::from_pt_scene(&resolve_scene("cornell").expect("cornell"))
            .expect("flatten")
            .instances()
            .iter()
            .position(|i| !i.receiver)
            .expect("the Cornell box has a lamp");
        c.instances = vec![lamp];
        assert_eq!(execute(&c).expect_err("lamp").code, "lightmap_instance");
    }

    #[test]
    fn dry_run_lays_out_the_cornell_box() {
        let mut a = args("cornell");
        a.dry_run = true;
        let v = execute(&a).expect("dry run");
        assert_eq!(v["ok"], true);
        assert_eq!(v["dry_run"], true);
        let entries = v["entries"].as_array().expect("entries");
        assert!(!entries.is_empty());
        let width = v["atlas"]["width"].as_u64().expect("width");
        assert!(width.is_power_of_two());
        // The light is emissive: it is a light, not a receiver.
        assert!(
            v["contents"]
                .as_array()
                .expect("contents")
                .contains(&json!("emissive"))
        );
        for e in entries {
            assert!(e["name"].as_str().expect("name").starts_with("instance_"));
            assert!(e["charts"].as_u64().expect("charts") >= 1);
        }
    }

    /// A layout scaled down to fit `--max-atlas` says so in `density.warnings` (ADR-2955); one that
    /// fits has no such warning.
    #[test]
    fn a_layout_scaled_to_fit_the_atlas_is_warned_about() {
        let mut a = args("cornell");
        a.dry_run = true;
        let v = execute(&a).expect("dry run");
        assert_eq!(v["atlas"]["fit_scale"], 1.0);
        let codes = |v: &Value| -> Vec<Value> {
            v["density"]["warnings"]
                .as_array()
                .expect("warnings")
                .iter()
                .map(|w| w["code"].clone())
                .collect()
        };
        assert!(!codes(&v).contains(&json!(ATLAS_SCALED_CODE)), "{v:#}");
        a.max_atlas = 64;
        a.density = 64.0;
        let v = execute(&a).expect("dry run");
        let fit = v["atlas"]["fit_scale"].as_f64().expect("fit_scale");
        assert!(fit < 1.0, "{v:#}");
        assert!(codes(&v).contains(&json!(ATLAS_SCALED_CODE)), "{v:#}");
        let w = v["density"]["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .find(|w| w["code"] == ATLAS_SCALED_CODE)
            .expect("warning");
        assert_eq!(w["fit_scale"].as_f64(), Some(fit));
        assert!(w["message"].as_str().expect("message").contains("64x64"), "{w:#}");
    }

    /// A density scaled down to fit the atlas is a headline of the JSON, not only a warning in the
    /// density block (ADR-3121): `density_scaled` is `null` when the layout fits and names the
    /// factor, the density reached and the remedy when it did not, in a dry run and in a bake.
    #[test]
    fn a_scaled_density_is_a_headline_with_the_factor_and_the_remedy() {
        let mut a = args("cornell");
        a.dry_run = true;
        let v = execute(&a).expect("dry run");
        assert_eq!(v.get("density_scaled"), Some(&Value::Null), "{v:#}");

        a.max_atlas = 64;
        a.density = 64.0;
        for dry_run in [true, false] {
            a.dry_run = dry_run;
            a.samples = 1;
            a.bounces = 1;
            let dir = regia_test_utils::unique_temp_dir("lightmap_density_scaled");
            a.out = Some(dir.join("scaled.flmap"));
            let v = execute(&a).expect("runs");
            let fit = v["atlas"]["fit_scale"].as_f64().expect("fit_scale");
            assert!(fit < 1.0, "{v:#}");
            let s = &v["density_scaled"];
            assert_eq!(s["factor"].as_f64(), Some(fit), "dry run {dry_run}: {v:#}");
            assert_eq!(s["requested"], 64.0);
            assert_eq!(s["max_atlas"], 64);
            let reached = s["density"].as_f64().expect("density");
            assert!((reached - 64.0 * fit).abs() < 1e-3, "{s}");
            let remedy = s["remedy"].as_str().expect("remedy");
            assert!(
                remedy.contains("--max-atlas") && remedy.contains("split the level"),
                "{remedy}"
            );
            assert!(s["message"].as_str().expect("message").contains(remedy));
            let _ = std::fs::remove_dir_all(&dir);
        }

        // At the format's largest atlas only a smaller level (or a lower density) helps.
        let mut full = args("cornell");
        full.max_atlas = regia_asset::lightmap::MAX_LIGHTMAP_SIZE;
        let remedy = atlas_scaled_remedy(&full);
        assert!(
            !remedy.contains("--max-atlas") && remedy.contains("split the level"),
            "{remedy}"
        );
    }

    /// A dry run's JSON times its two phases (a bake's adds `bake`, `seams` and `encode_write`).
    #[test]
    fn the_json_times_each_phase() {
        let mut a = args("cornell");
        a.dry_run = true;
        let v = execute(&a).expect("dry run");
        let mut keys: Vec<&str> = v["timings"]
            .as_object()
            .expect("timings")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["layout", "load"]);
        for (_, s) in v["timings"].as_object().expect("timings") {
            assert!(s.as_f64().is_some_and(|s| s >= 0.0), "{s}");
        }
    }

    /// A closed box of one-sided walls facing in, under a bright sky and nothing else.
    fn sealed_room() -> regia_pt::PtScene {
        let mut scene = regia_pt::PtScene::new(regia_pt::PtCamera::pinhole(
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            0.9,
            8,
            8,
        ));
        // Each face: its inward normal `n` and two edges with `u × v = n`, so the winding agrees.
        let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let (mut positions, mut normals, mut indices) = (Vec::new(), Vec::new(), Vec::new());
        for (n, u, v) in faces {
            let base = positions.len() as u32;
            for (a, b) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                positions.push(std::array::from_fn(|k| 2.0 * (a * u[k] + b * v[k] - n[k])));
                normals.push(n);
            }
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        let mesh = scene.add_mesh(regia_pt::PtMesh {
            positions,
            normals,
            indices,
            ..regia_pt::PtMesh::default()
        });
        let material = scene.add_material(regia_pt::PtMaterial::lambert([0.5; 3]));
        scene.add_instance(mesh, material, regia_core::glam::Mat4::IDENTITY);
        scene.sky = regia_pt::PtSky::Uniform([1000.0; 3]);
        scene
    }

    /// No light gets into a sealed room, on any layout and density. A texel center on a mesh edge
    /// must not trace from the joint itself, where half its rays leave through the wall it touches.
    #[test]
    fn no_light_leaks_into_a_sealed_room() {
        let scene = sealed_room();
        for unwrap in [UnwrapMode::Atlas, UnwrapMode::Charts] {
            for density in [4.0, 8.0, 16.0] {
                let mut a = args("sealed");
                a.unwrap = unwrap;
                a.density = density;
                a.samples = 32;
                a.bounces = 2;
                a.no_filter = true;
                let laid = lay_out(&scene, &a, None).expect("lays out");
                let (map, report) = bake_lightmap(
                    &laid.scene,
                    laid.layout,
                    &laid.receivers,
                    &bake_settings(&a),
                    |_, _| {},
                )
                .expect("bakes");
                let mut lit = 0usize;
                let mut brightest = 0.0f32;
                for y in 0..map.height {
                    for x in 0..map.width {
                        let e = map.irradiance_at(x, y)[1];
                        if map.coverage_at(x, y) == regia_asset::lightmap::Coverage::BAKED && e > 0.0 {
                            lit += 1;
                            brightest = brightest.max(e);
                        }
                    }
                }
                assert_eq!(
                    (lit, report.texels_inside),
                    (0, 0),
                    "{unwrap:?} at density {density}: {lit} of {} texels lit, brightest {brightest} lux",
                    report.texels_baked
                );
            }
        }
    }

    /// The default pass count follows the density so the reach stays about 3.75 m (ADR-4108).
    #[test]
    fn the_default_denoiser_reach_follows_the_density() {
        let counts: Vec<u32> = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 1000.0]
            .iter()
            .map(|d| default_denoise_iterations(*d))
            .collect();
        assert_eq!(counts, [2, 2, 3, 4, 5, 6, 8]);
        assert_eq!(default_denoise_iterations(0.0), 1);
        assert_eq!(default_denoise_iterations(f32::NAN), 1);
    }

    /// New bakes run the denoiser and record it (`--denoise on` is the default); `--denoise off`
    /// gives the bake of before (3×3 filter, no record); `--encoding` stores blocks the inspector
    /// names; and a bad pass count is `lightmap_bake_settings` naming `denoise`.
    #[test]
    fn denoise_is_on_for_new_bakes_and_encoding_writes_blocks() {
        use regia_asset::lightmap::{LightmapDenoiseInfo, LightmapTexelEncoding};
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_denoise");
        let mut on = args(QUAD_BOX_SCENE);
        on.density = 4.0;
        on.out = Some(dir.join("on.flmap"));
        let v = execute(&on).expect("bake");
        assert_eq!(v["report"]["denoised"], true, "{v}");
        assert_eq!(
            v["settings"]["denoise"]["iterations"],
            default_denoise_iterations(4.0)
        );
        assert_eq!(v["encode"]["encoding"], "rgb9e5");
        let map = Lightmap::read(&dir.join("on.flmap")).expect("reads");
        assert_eq!(
            map.denoise,
            Some(LightmapDenoiseInfo {
                iterations: default_denoise_iterations(4.0),
                ..LightmapDenoiseInfo::default()
            })
        );
        assert!(!map.bake.filtered, "the denoiser replaces the 3x3 filter");
        // The denoiser lowers the noise it carries.
        let before = v["report"]["noise"]["mean"].as_f64().expect("noise");
        let after = v["report"]["denoised_noise"].as_f64().expect("denoised noise");
        assert!(after < before, "{before} -> {after}");

        let mut off = on.clone();
        off.denoise = DenoiseMode::Off;
        off.out = Some(dir.join("off.flmap"));
        let v = execute(&off).expect("bake");
        assert_eq!(v["report"]["denoised"], false);
        assert_eq!(v["settings"]["denoise"], Value::Null);
        let plain = Lightmap::read(&dir.join("off.flmap")).expect("reads");
        assert_eq!(plain.denoise, None);
        assert!(plain.bake.filtered);
        assert_ne!(plain.texels, map.texels);

        for (arg, encoding) in [
            (TexelEncodingArg::Bc6h, LightmapTexelEncoding::Bc6h),
            (TexelEncodingArg::AstcHdr, LightmapTexelEncoding::AstcHdr),
        ] {
            let mut enc = on.clone();
            enc.encoding = arg;
            let out = dir.join(format!("{}.flmap", encoding.name()));
            enc.out = Some(out.clone());
            let v = execute(&enc).expect("bake");
            assert_eq!(v["encode"]["encoding"], encoding.name());
            assert!(
                v["encode"]["bytes"].as_u64() < v["encode"]["rgb9e5_bytes"].as_u64(),
                "{v}"
            );
            let i = inspect(&out).expect("inspects");
            assert_eq!(i["texel_encoding"], encoding.name());
            assert_eq!(i["version"], regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION);
            assert_eq!(i["denoise"]["iterations"], default_denoise_iterations(4.0));
        }

        let mut bad = on.clone();
        bad.denoise_iterations = Some(0);
        let e = execute(&bad).expect_err("0 passes");
        assert_eq!(
            (e.code, e.path.as_deref()),
            ("lightmap_bake_settings", Some("denoise"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--meshlets` stores clusters that fit each entry's unwrap, a dry run reports them, the
    /// number of workers does not change them, `--inspect` shows them, and without the flag the
    /// file has none.
    #[test]
    fn meshlets_flag_stores_clusters_that_fit_each_unwrap() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_meshlets");
        let plain_out = dir.join("plain.flmap");
        let mut plain = args(QUAD_BOX_SCENE);
        plain.density = 4.0;
        plain.out = Some(plain_out.clone());
        let v = execute(&plain).expect("bake");
        assert_eq!(v["meshlets"], Value::Null);
        assert_eq!(v["settings"]["meshlets"], false);
        assert!(Lightmap::read(&plain_out).expect("reads").meshlets.is_empty());

        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        a.meshlets = true;
        a.dry_run = true;
        let dry = execute(&a).expect("dry run");
        let entries = dry["entries"].as_array().expect("entries").len();
        assert!(entries >= 2, "{dry}");
        assert_eq!(dry["meshlets"]["entries"], entries, "{dry}");
        assert_eq!(dry["meshlets"]["refused"], json!([]));
        assert_eq!(dry["settings"]["meshlets"], true);

        let out = dir.join("clusters.flmap");
        a.dry_run = false;
        a.out = Some(out.clone());
        let v = execute(&a).expect("bake");
        assert_eq!(
            v["meshlets"], dry["meshlets"],
            "the dry run reported what the bake stores"
        );
        let map = Lightmap::read(&out).expect("reads back");
        assert_eq!(map.meshlet_entries(), entries);
        for (k, unwrap) in map.unwraps.iter().enumerate() {
            let blob = map.entry_meshlets(k).expect("a blob");
            let mesh = regia_render::MeshletMesh::from_bytes(blob).expect("decodes");
            assert_eq!(mesh.source_vertex_count as usize, unwrap.xref.len());
            assert_eq!(mesh.source_index_count as usize, unwrap.indices.len());
        }
        // The same bake without them differs only in that section.
        let without = Lightmap::read(&plain_out).expect("reads");
        assert_eq!(map.texels, without.texels);
        assert_eq!(map.unwraps, without.unwraps);

        let mut more_workers = args(QUAD_BOX_SCENE);
        more_workers.density = 4.0;
        more_workers.meshlets = true;
        more_workers.threads = 1;
        let serial = dir.join("serial.flmap");
        more_workers.out = Some(serial.clone());
        execute(&more_workers).expect("bake");
        assert_eq!(
            Lightmap::read(&serial).expect("reads").meshlets,
            map.meshlets,
            "workers do not change the bytes"
        );

        let mut i = args(&out.display().to_string());
        i.inspect = true;
        let seen = execute(&i).expect("inspect");
        assert_eq!(seen["version"], regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION);
        assert_eq!(seen["meshlet_entries"], entries);
        assert!(seen["entries"][0]["meshlet_bytes"].as_u64().expect("bytes") > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unwrap the builder refuses (here an `xref` past the mesh) leaves its entry without clusters
    /// and says so; the rest keep theirs.
    #[test]
    fn an_entry_the_builder_refuses_is_reported_and_has_no_clusters() {
        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        let scene = resolve_scene(QUAD_BOX_SCENE).expect("scene");
        let laid = lay_out(&scene, &a, None).expect("lays out");
        assert!(laid.receivers.len() >= 2);
        let mut broken = laid.layout.clone();
        broken.unwraps[0].xref[0] = u32::MAX;
        let built = meshlets::build(&scene, &laid.receivers, &broken, 2);
        assert!(built.blobs[0].is_empty());
        assert!(!built.blobs[1].is_empty());
        let v = built.describe();
        assert_eq!(v["refused"][0]["entry"], json!(broken.entries[0].name));
        assert_eq!(v["refused"][0]["code"], "lightmap_meshlets");
        assert_eq!(v["entries"], laid.receivers.len() - 1);
    }

    /// `--checkpoint` (ADR-3095): a bake stopped by the test hook at batch 3 leaves its checkpoint
    /// beside the output; the same command again says it found it, continues it, writes the bytes
    /// of a bake that never stopped and removes it. Another seed is `bake_checkpoint_mismatch`, a
    /// damaged file `bake_checkpoint_damaged`.
    #[test]
    fn a_checkpointed_bake_stopped_at_batch_k_resumes_to_the_same_file() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_resume");
        let plain = dir.join("plain.flmap");
        let mut a = args("cornell");
        a.out = Some(plain.clone());
        execute(&a).expect("plain bake");
        let out = dir.join("cornell.flmap");
        let mut c = args("cornell");
        c.out = Some(out.clone());
        c.threads = 3;
        c.checkpoint = CheckpointArgs {
            checkpoint: true,
            checkpoint_seconds: 0.0,
            checkpoint_batch: 97,
            checkpoint_stop_after: Some(3),
            ..CheckpointArgs::default()
        };
        let e = execute(&c).expect_err("stopped");
        assert_eq!(e.code, "bake_interrupted", "{e}");
        let ck = regia_pt::checkpoint::default_checkpoint_path(&out);
        assert_eq!(e.path.as_deref(), Some(ck.display().to_string().as_str()));
        let status = regia_pt::checkpoint::checkpoint_status(&out);
        assert!(status.resumable, "{status:?}");
        assert_eq!(status.units_done, Some(3 * 97));
        assert!(!out.exists(), "nothing written before the end");
        // Another seed refuses the checkpoint and leaves it.
        let mut other = c.clone();
        other.seed = 2;
        other.checkpoint.checkpoint_stop_after = None;
        assert_eq!(
            execute(&other).expect_err("seed").code,
            "bake_checkpoint_mismatch"
        );
        assert!(ck.exists());
        c.checkpoint.checkpoint_stop_after = None;
        c.threads = 1;
        let v = execute(&c).expect("resumed");
        assert_eq!(v["checkpoint"]["resumed"], true, "{v}");
        assert_eq!(v["checkpoint"]["resumed_units"], 3 * 97);
        assert_eq!(v["checkpoint"]["removed"], true);
        assert!(!ck.exists());
        assert_eq!(
            std::fs::read(&out).expect("resumed file"),
            std::fs::read(&plain).expect("plain file"),
            "the resumed bake writes the same bytes"
        );
        // A damaged checkpoint named by --resume is refused with its code, never a panic.
        std::fs::write(&ck, b"RGBKCKPT\x01\x00\x00\x00").expect("damage");
        let mut r = args("cornell");
        r.out = Some(out.clone());
        r.checkpoint.resume = Some(ck.clone());
        assert_eq!(execute(&r).expect_err("damaged").code, "bake_checkpoint_damaged");
        r.checkpoint.resume = Some(dir.join("missing.bakeck"));
        assert_eq!(execute(&r).expect_err("missing").code, "bake_checkpoint_io");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The checkpoint goes only once the lightmap is on disk: a bake whose final write fails
    /// keeps it, and the next run resumes to the same bytes instead of starting over.
    #[test]
    fn audit_a_failed_output_write_keeps_the_checkpoint() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_keep_ck");
        let plain = dir.join("plain.flmap");
        let mut a = args("cornell");
        a.out = Some(plain.clone());
        execute(&a).expect("plain bake");
        let out = dir.join("cornell.flmap");
        let mut c = args("cornell");
        c.out = Some(out.clone());
        c.checkpoint = CheckpointArgs {
            checkpoint: true,
            checkpoint_seconds: 0.0,
            checkpoint_batch: 97,
            checkpoint_stop_after: Some(3),
            ..CheckpointArgs::default()
        };
        assert_eq!(execute(&c).expect_err("stopped").code, "bake_interrupted");
        let ck = regia_pt::checkpoint::default_checkpoint_path(&out);
        assert!(ck.is_file());
        // The output cannot be written: a directory stands where the file goes.
        std::fs::create_dir_all(out.join("blocker")).expect("blocking dir");
        c.checkpoint.checkpoint_stop_after = None;
        execute(&c).expect_err("the output write fails");
        assert!(ck.is_file(), "a failed output write keeps the checkpoint");
        std::fs::remove_dir_all(&out).expect("unblock");
        let v = execute(&c).expect("resumed");
        assert_eq!(v["checkpoint"]["resumed"], true, "{v}");
        assert_eq!(v["checkpoint"]["removed"], true, "{v}");
        assert!(!ck.exists());
        assert_eq!(
            std::fs::read(&out).expect("resumed file"),
            std::fs::read(&plain).expect("plain file")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bakes_writes_and_inspects_a_lightmap() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps");
        let out = dir.join("cornell.flmap");
        let mut a = args("cornell");
        a.out = Some(out.clone());
        let v = execute(&a).expect("bake");
        assert_eq!(v["ok"], true);
        assert!(v["report"]["texels_baked"].as_u64().expect("baked") > 100, "{v}");
        assert_eq!(
            v["report"]["entries"],
            v["entries"].as_array().expect("entries").len()
        );
        let map = Lightmap::read(&out).expect("the written file reads back");
        assert_eq!(map.entries.len(), v["entries"].as_array().expect("entries").len());
        // Every entry records the hash of its source mesh (ADR-2656).
        let scene = resolve_scene("cornell").expect("cornell");
        let laid = lay_out(&scene, &a, None).expect("lays out");
        assert_eq!(map.source_hashes, source_hashes(&scene, &laid.receivers));
        assert!(
            map.source_hashes.iter().all(|h| *h != 0),
            "{:?}",
            map.source_hashes
        );
        let timings = v["timings"].as_object().expect("timings");
        for phase in ["load", "layout", "bake", "seams", "encode_write"] {
            assert!(timings[phase].as_f64().is_some_and(|s| s >= 0.0), "{phase}: {v}");
        }
        assert_eq!(timings.len(), 5);
        // Lit by the ceiling lamp: some texel holds light.
        assert!((0..map.height).any(|y| (0..map.width).any(|x| map.irradiance_at(x, y)[0] > 0.0)));

        let mut i = args(&out.display().to_string());
        i.inspect = true;
        let s = execute(&i).expect("inspect");
        assert_eq!(s["ok"], true);
        assert_eq!(s["version"], regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION);
        assert_eq!(s["directional"], true, "directions are baked by default");
        assert_eq!(v["report"]["directional"], true);
        assert!(v["report"]["noise"]["mean"].as_f64().expect("noise") > 0.0, "{v}");
        assert_eq!(s["width"], map.width);
        assert_eq!(s["coverage"]["baked"], v["coverage"]["baked"]);

        // A corrupt copy is refused with its code and path.
        let mut bytes = std::fs::read(&out).expect("read");
        let k = bytes.len() - 20;
        bytes[k] ^= 0xff;
        let bad = dir.join("bad.flmap");
        let tmp = dir.join("bad.flmap.tmp");
        std::fs::write(&tmp, bytes).expect("write");
        std::fs::rename(&tmp, &bad).expect("rename");
        let mut i = args(&bad.display().to_string());
        i.inspect = true;
        let e = execute(&i).expect_err("corrupt");
        assert_eq!(e.code, "lightmap_payload");
        assert_eq!(e.to_json()["path"], bad.display().to_string());
        let mut i = args(&dir.join("missing.flmap").display().to_string());
        i.inspect = true;
        assert_eq!(execute(&i).expect_err("missing").code, "lightmap_io");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--min-area` leaves small receivers to the probes and reports them; past every mesh's
    /// area nothing is left to unwrap.
    #[test]
    fn min_area_skips_small_meshes() {
        let mut a = args("cornell");
        a.dry_run = true;
        let all = execute(&a).expect("dry run");
        let n = all["entries"].as_array().expect("entries").len();
        assert_eq!(all["skipped_small"].as_array().expect("skipped").len(), 0);
        a.min_area = 0.2;
        let some = execute(&a).expect("dry run");
        let kept = some["entries"].as_array().expect("entries").len();
        let skipped = some["skipped_small"].as_array().expect("skipped").len();
        assert_eq!(kept + skipped, n);
        a.min_area = 1.0e6;
        assert_eq!(execute(&a).expect_err("nothing left").code, "lightmap_uv_empty");
        a.min_area = -1.0;
        assert_eq!(execute(&a).expect_err("negative").code, "lightmap_uv_settings");
        // A unit right triangle is 0.5 m².
        assert!(
            (surface_area(&[[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], &[0, 1, 2]) - 0.5).abs() < 1e-12
        );
    }

    #[test]
    fn errors_carry_codes() {
        assert_eq!(
            execute(&args("no-such-scene")).expect_err("unknown").code,
            "bake_scene_unknown"
        );
        assert_eq!(
            execute(&args("/nonexistent/map.bake.ron"))
                .expect_err("missing")
                .code,
            "bake_source_missing"
        );
        // furnace is an empty sky: nothing to unwrap (rolling-hills is not: it carries a boulder
        // mesh beside its heightfield).
        assert_eq!(
            execute(&args("furnace")).expect_err("empty").code,
            "lightmap_uv_empty"
        );
        let mut a = args("cornell");
        a.padding = 0;
        a.dry_run = true;
        assert_eq!(execute(&a).expect_err("settings").code, "lightmap_uv_settings");
        let printed = LightmapCommandError::new("lightmap_uv_settings", None, "x").to_json();
        assert_eq!(printed["ok"], false);
        assert_eq!(default_out("maps/bore.bake.ron"), PathBuf::from("bore.flmap"));
        assert_eq!(default_out("cornell"), PathBuf::from("cornell.flmap"));
    }

    /// The seam metric on a real bake of a curved mesh (the boulder of `rolling-hills`): seams are
    /// found on it, per chart, and they shrink with more samples, because what they measure at this
    /// quality is noise; the figures reach the file; a budget below them fails after writing it.
    #[test]
    fn a_curved_mesh_has_measured_seams_that_fall_with_samples_and_a_budget_holds_them() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_seams");
        let mut a = args("rolling-hills");
        a.density = 8.0;
        a.no_filter = true;
        let bake = |samples: u32, name: &str, budget: Option<f32>| {
            let mut a = a.clone();
            a.samples = samples;
            a.seam_budget = budget;
            a.out = Some(dir.join(name));
            execute(&a)
        };
        let noisy = bake(8, "noisy.flmap", None).expect("bake");
        let clean = bake(128, "clean.flmap", None).expect("bake");
        let edges = noisy["seams"]["edges"].as_u64().expect("edges");
        assert!(
            edges > 0,
            "the boulder's charts meet on smooth edges: {}",
            noisy["seams"]
        );
        assert_eq!(
            noisy["seams"]["edges"], clean["seams"]["edges"],
            "the same layout"
        );
        let p95 = |v: &Value| v["seams"]["p95"].as_f64().expect("p95");
        assert!(p95(&noisy) > 0.0, "{}", noisy["seams"]);
        assert!(
            p95(&clean) < p95(&noisy),
            "128 samples {} against 8 samples {}",
            p95(&clean),
            p95(&noisy)
        );
        assert!(
            !noisy["seams"]["entries"][0]["charts"]
                .as_array()
                .expect("charts")
                .is_empty()
        );
        assert_eq!(
            noisy["seams"]["overlay"][0]["world_a"].as_array().map(Vec::len),
            Some(3)
        );
        let map = Lightmap::read(&dir.join("noisy.flmap")).expect("read back");
        let stored = map.entries[0].seams.expect("the figures are in the file");
        assert_eq!(u64::from(stored.edges), edges);

        // Under the budget it passes; over it, the code names the entry and the file is there.
        bake(8, "generous.flmap", Some(10.0)).expect("within the budget");
        let e = bake(8, "tight.flmap", Some(0.0)).expect_err("over the budget");
        assert_eq!(
            (e.code, e.path.as_deref()),
            (SEAM_BUDGET_CODE, Some("instance_0"))
        );
        assert!(
            dir.join("tight.flmap").is_file(),
            "the file was written to look at"
        );
        assert!(over_seam_budget(&map, 0.0).contains(&"instance_0".to_string()));
        let mut bad = a.clone();
        bad.seam_budget = Some(-1.0);
        assert_eq!(
            execute(&bad).expect_err("negative").code,
            "lightmap_bake_settings"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--gpu` is a clear refusal in a build without the baker, and a bake in one with it.
    #[test]
    fn gpu_flag_bakes_or_says_how_to_get_it() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_gpu");
        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        a.gpu = true;
        a.out = Some(dir.join("gpu.flmap"));
        if cfg!(feature = "lightmap-gpu") {
            let v = execute(&a).expect("bake");
            assert_eq!(v["settings"]["backend"], "gpu");
            assert!(v["report"]["texels_baked"].as_u64().expect("baked") > 0, "{v}");
            let read = Lightmap::read(&dir.join("gpu.flmap")).expect("reads back");
            assert!(read.is_directional());
        } else {
            let e = execute(&a).expect_err("not built");
            assert_eq!(
                (e.code, e.path.as_deref()),
                ("lightmap_gpu_unavailable", Some("gpu"))
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `--gpu-*` options are bounded and checked before the scene is read, the option in
    /// `path`.
    #[test]
    fn gpu_options_out_of_range_are_refused_with_the_option() {
        type Tweak = fn(&mut GpuBakeArgs);
        let cases: [(Tweak, &str); 5] = [
            (|g| g.gpu_tile_rays = 4095, "gpu_tile_rays"),
            (|g| g.gpu_tile_rays = (1 << 26) + 1, "gpu_tile_rays"),
            (|g| g.gpu_memory_mib = -1.0, "gpu_memory_mib"),
            (|g| g.gpu_memory_mib = f64::NAN, "gpu_memory_mib"),
            (|g| g.gpu_yield_ms = 1001, "gpu_yield_ms"),
        ];
        for (tweak, field) in cases {
            let mut a = args("no-such-scene.bake.ron");
            a.gpu = true;
            tweak(&mut a.gpu_opts);
            let e = execute(&a).expect_err(field);
            assert_eq!(
                (e.code, e.path.as_deref()),
                ("lightmap_bake_settings", Some(field))
            );
        }
        assert!(check_gpu_args(&GpuBakeArgs::default()).is_ok());
    }

    /// Invalid settings are refused before resolving a scene or touching an existing output.
    #[test]
    fn invalid_settings_leave_existing_output_unchanged_before_scene_load() {
        type Tweak = fn(&mut BakeLightmapsArgs);
        let cases: [(Tweak, &str, &str); 30] = [
            (|a| a.density = 0.0, "lightmap_uv_settings", "density"),
            (|a| a.density = -1.0, "lightmap_uv_settings", "density"),
            (|a| a.density = f32::NAN, "lightmap_uv_settings", "density"),
            (|a| a.density = f32::INFINITY, "lightmap_uv_settings", "density"),
            (
                |a| a.density = f32::NEG_INFINITY,
                "lightmap_uv_settings",
                "density",
            ),
            (|a| a.padding = 0, "lightmap_uv_settings", "padding"),
            (|a| a.padding = 17, "lightmap_uv_settings", "padding"),
            (|a| a.max_atlas = 15, "lightmap_uv_settings", "max_atlas"),
            (
                |a| a.max_atlas = regia_asset::lightmap::MAX_LIGHTMAP_SIZE + 1,
                "lightmap_uv_settings",
                "max_atlas",
            ),
            (|a| a.chart_angle = 0.0, "lightmap_uv_settings", "chart_angle"),
            (|a| a.chart_angle = 81.0, "lightmap_uv_settings", "chart_angle"),
            (
                |a| a.chart_angle = f32::NAN,
                "lightmap_uv_settings",
                "chart_angle",
            ),
            (|a| a.min_area = -1.0, "lightmap_uv_settings", "min_area"),
            (|a| a.min_area = f32::NAN, "lightmap_uv_settings", "min_area"),
            (|a| a.min_area = f32::INFINITY, "lightmap_uv_settings", "min_area"),
            (|a| a.samples = 0, "lightmap_bake_settings", "samples"),
            (|a| a.bounces = 65, "lightmap_bake_settings", "bounces"),
            (|a| a.max_samples = 65537, "lightmap_bake_settings", "max_samples"),
            (
                |a| a.noise_target = -0.1,
                "lightmap_bake_settings",
                "noise_target",
            ),
            (
                |a| a.noise_target = f32::INFINITY,
                "lightmap_bake_settings",
                "noise_target",
            ),
            (
                |a| a.denoise_iterations = Some(0),
                "lightmap_bake_settings",
                "denoise",
            ),
            (
                |a| a.seam_budget = Some(f32::NAN),
                "lightmap_bake_settings",
                "seam_budget",
            ),
            (
                |a| a.seam_budget = Some(-1.0),
                "lightmap_bake_settings",
                "seam_budget",
            ),
            (
                |a| a.seam_budget = Some(f32::INFINITY),
                "lightmap_bake_settings",
                "seam_budget",
            ),
            (
                |a| a.gpu_opts.gpu_tile_rays = 4095,
                "lightmap_bake_settings",
                "gpu_tile_rays",
            ),
            (
                |a| a.gpu_opts.gpu_tile_rays = (1 << 26) + 1,
                "lightmap_bake_settings",
                "gpu_tile_rays",
            ),
            (
                |a| a.gpu_opts.gpu_yield_ms = 1001,
                "lightmap_bake_settings",
                "gpu_yield_ms",
            ),
            (
                |a| a.gpu_opts.gpu_memory_mib = -1.0,
                "lightmap_bake_settings",
                "gpu_memory_mib",
            ),
            (
                |a| a.gpu_opts.gpu_memory_mib = f64::NAN,
                "lightmap_bake_settings",
                "gpu_memory_mib",
            ),
            (
                |a| a.gpu_opts.gpu_memory_mib = f64::INFINITY,
                "lightmap_bake_settings",
                "gpu_memory_mib",
            ),
        ];
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_invalid_settings");
        let out = dir.join("existing.flmap");
        let sentinel = b"preexisting lightmap output";
        std::fs::write(&out, sentinel).expect("write sentinel");
        let before: Vec<_> = std::fs::read_dir(&dir)
            .expect("list output directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        for (tweak, expected_code, field) in cases {
            let mut a = args("missing-scene.bake.ron");
            a.out = Some(out.clone());
            tweak(&mut a);
            let error = execute(&a).expect_err(field);
            assert_eq!((error.code, error.path.as_deref()), (expected_code, Some(field)));
            assert_eq!(std::fs::read(&out).expect("read sentinel"), sentinel);
            let after: Vec<_> = std::fs::read_dir(&dir)
                .expect("list output directory")
                .map(|entry| entry.expect("directory entry").file_name())
                .collect();
            assert_eq!(after, before, "{field} changed output directory");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Inclusive GPU endpoints and the documented zero auto settings pass validation.
    #[test]
    fn gpu_settings_valid_endpoints_pass_without_baking() {
        for rays in [0, 4096, 1 << 26] {
            assert!(
                check_gpu_args(&GpuBakeArgs {
                    gpu_tile_rays: rays,
                    ..GpuBakeArgs::default()
                })
                .is_ok()
            );
        }
        for yield_ms in [0, 1000] {
            assert!(
                check_gpu_args(&GpuBakeArgs {
                    gpu_yield_ms: yield_ms,
                    ..GpuBakeArgs::default()
                })
                .is_ok()
            );
        }
        assert!(
            check_gpu_args(&GpuBakeArgs {
                gpu_memory_mib: 0.0,
                ..GpuBakeArgs::default()
            })
            .is_ok()
        );
        let mut valid_bake = bake_settings(&args("unused"));
        valid_bake.samples = 1;
        valid_bake.bounces = 64;
        valid_bake.max_samples = 65536;
        valid_bake.noise_target = 0.0;
        if let Some(denoise) = &mut valid_bake.denoise {
            denoise.iterations = 8;
        }
        assert!(check_bake_settings(&valid_bake).is_ok());
        let mut valid_seam_budget = args("missing-scene.bake.ron");
        valid_seam_budget.seam_budget = Some(0.0);
        assert_eq!(
            execute(&valid_seam_budget)
                .expect_err("scene remains missing")
                .code,
            "bake_source_missing"
        );
        for (density, padding, max_atlas, chart_angle, min_area) in [
            (f32::MIN_POSITIVE, 1, 16, 1.0, 0.0),
            (f32::MAX, 16, 8192, 80.0, f32::MAX),
        ] {
            let mut valid_uv = args("unused");
            valid_uv.density = density;
            valid_uv.padding = padding;
            valid_uv.max_atlas = max_atlas;
            valid_uv.chart_angle = chart_angle;
            valid_uv.min_area = min_area;
            assert!(check_uv_args(&valid_uv).is_ok());
        }
    }

    /// `--gpu-fallback` bakes on the CPU when the GPU cannot (here a memory budget the scene
    /// alone passes, or a CLI without the baker) and says why; without it the GPU's error stands.
    #[test]
    fn gpu_fallback_bakes_on_the_cpu_and_says_why() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_gpu_fallback");
        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        a.gpu = true;
        a.gpu_opts.gpu_memory_mib = 1e-6;
        a.out = Some(dir.join("fallback.flmap"));
        let code = if cfg!(feature = "lightmap-gpu") {
            "lightmap_gpu_limit"
        } else {
            "lightmap_gpu_unavailable"
        };
        assert_eq!(execute(&a).expect_err("no fallback").code, code);
        a.gpu_opts.gpu_fallback = true;
        let v = execute(&a).expect("bakes on the CPU");
        assert_eq!(v["settings"]["backend"], "cpu", "{v}");
        assert_eq!(v["gpu"]["fallback"]["code"], code, "{v}");
        assert!(v["report"]["texels_baked"].as_u64().expect("baked") > 0);
        Lightmap::read(&dir.join("fallback.flmap")).expect("written");
        // A settings error is never a reason to fall back.
        a.samples = 0;
        assert_eq!(execute(&a).expect_err("settings").code, "lightmap_bake_settings");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--compare` measures the new map against a reference of the same layout: a CPU re-bake is
    /// identical, a GPU bake holds the PSNR floor and reports its tiles; a missing reference is the
    /// reader's code.
    #[test]
    fn compare_measures_a_bake_against_a_reference() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_compare");
        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        a.samples = 16;
        a.out = Some(dir.join("cpu.flmap"));
        execute(&a).expect("reference");
        a.compare = Some(dir.join("cpu.flmap"));
        a.out = Some(dir.join("again.flmap"));
        let v = execute(&a).expect("re-bake");
        assert_eq!(v["compare"]["identical"], true, "{v}");
        assert_eq!(v["compare"]["within_floor"], true, "{v}");
        assert!(v["compare"]["texels"].as_u64().expect("texels") > 0);
        if cfg!(feature = "lightmap-gpu") {
            a.gpu = true;
            a.gpu_opts.gpu_tile_rays = 4096;
            a.gpu_opts.gpu_yield_ms = 1;
            a.out = Some(dir.join("gpu.flmap"));
            let v = execute(&a).expect("gpu bake");
            assert_eq!(v["settings"]["backend"], "gpu");
            assert_eq!(v["compare"]["within_floor"], true, "{v}");
            assert!(v["gpu"]["tiles"].as_u64().expect("tiles") > 1, "{v}");
            assert_eq!(v["gpu"]["tile_rays"], 4096);
            assert_eq!(v["gpu"]["fallback"], Value::Null);
            assert_eq!(v["gpu"]["schedule_hash"].as_str().map(str::len), Some(16));
            a.gpu = false;
        }
        a.compare = Some(dir.join("missing.flmap"));
        assert_eq!(execute(&a).expect_err("no reference").code, "lightmap_io");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sample counts are bounded and checked before the scene is read; the field is in `path`.
    #[test]
    fn bake_settings_out_of_range_are_refused_with_the_field() {
        type Tweak = fn(&mut BakeLightmapsArgs);
        let cases: [(Tweak, &str); 7] = [
            (|a| a.samples = 0, "samples"),
            (|a| a.samples = 70_000, "samples"),
            (|a| a.max_samples = 2, "max_samples"),
            (|a| a.max_samples = 1 << 20, "max_samples"),
            (|a| a.bounces = 1000, "bounces"),
            (|a| a.noise_target = -0.5, "noise_target"),
            (|a| a.noise_target = f32::NAN, "noise_target"),
        ];
        for (tweak, field) in cases {
            // An unknown scene: the settings are refused first.
            let mut a = args("no-such-scene");
            tweak(&mut a);
            let e = execute(&a).expect_err("out of range");
            assert_eq!(
                (e.code, e.path.as_deref()),
                ("lightmap_bake_settings", Some(field)),
                "{e}"
            );
        }
    }

    /// Adaptive sampling on the quad-box floor: every texel takes at least `--samples` and at most
    /// `--max-samples`, the report says how many on average, and `--no-directional` drops the
    /// direction section.
    #[test]
    fn adaptive_sampling_stays_within_its_bounds_and_directions_can_be_left_out() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_lightmaps_adaptive");
        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        a.instances = vec![0];
        a.samples = 4;
        a.max_samples = 32;
        a.noise_target = 0.01;
        a.no_directional = true;
        a.out = Some(dir.join("floor.flmap"));
        let v = execute(&a).expect("bake");
        let noise = &v["report"]["noise"];
        let mean = noise["mean_samples"].as_f64().expect("mean samples");
        assert!((4.0..=32.0).contains(&mean), "{noise}");
        assert!(
            mean > 4.0,
            "a 1 % target needs more than 4 samples somewhere: {noise}"
        );
        assert_eq!(v["settings"]["max_samples"], 32);
        assert_eq!(v["report"]["directional"], false);
        let map = Lightmap::read(&dir.join("floor.flmap")).expect("read back");
        assert!(!map.is_directional());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 0.3 m UV sphere (24 × 12) at the origin over a 4 m floor, under a uniform sky.
    fn sphere_scene() -> regia_pt::PtScene {
        use regia_core::glam::Mat4;
        use regia_pt::{PtCamera, PtMaterial, PtMesh, PtScene, PtSky};
        let (r, seg, rings) = (0.3f32, 24u32, 12u32);
        let mut positions = Vec::new();
        for j in 0..=rings {
            let th = std::f32::consts::PI * j as f32 / rings as f32;
            for i in 0..=seg {
                let ph = std::f32::consts::TAU * (i % seg) as f32 / seg as f32;
                positions.push([
                    r * th.sin() * ph.cos(),
                    0.5 + r * th.cos(),
                    r * th.sin() * ph.sin(),
                ]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..rings {
            for i in 0..seg {
                let a = j * (seg + 1) + i;
                let b = a + seg + 1;
                indices.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
            }
        }
        let normals = positions
            .iter()
            .map(|p| {
                let v = [p[0], p[1] - 0.5, p[2]];
                let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
                v.map(|c| c / l)
            })
            .collect();
        let mut s = PtScene::new(PtCamera::pinhole([0.0, 2.0, 3.0], [0.0; 3], 1.0, 4, 4));
        let floor = s.add_mesh(PtMesh {
            positions: vec![
                [-2.0, 0.0, -2.0],
                [2.0, 0.0, -2.0],
                [2.0, 0.0, 2.0],
                [-2.0, 0.0, 2.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2],
            ..PtMesh::default()
        });
        let ball = s.add_mesh(PtMesh {
            positions,
            normals,
            indices,
            ..PtMesh::default()
        });
        let grey = s.add_material(PtMaterial::lambert([0.5; 3]));
        s.add_instance(floor, grey, Mat4::IDENTITY);
        s.add_instance(ball, grey, Mat4::IDENTITY);
        s.sky = PtSky::Uniform([1.0; 3]);
        s
    }

    /// The automatic density rule (ADR-2404): without the stitch the small sphere gets a
    /// power-of-two scale above 1 and the flat floor keeps 1; with it (the default) the sphere's
    /// predicted seams are under the budget at 1; a level's own scale is kept and warned about when
    /// it predicts seams over the budget; `--auto-density off` lays everything out as asked.
    #[test]
    fn the_density_rule_raises_a_small_sphere_and_keeps_explicit_scales() {
        let scene = sphere_scene();
        let mut a = args("unused");
        a.density = 8.0;
        let stitched = lay_out(&scene, &a, None).expect("lay out");
        assert_eq!(
            stitched.density[1].density_scale, 1.0,
            "{:?}",
            stitched.density[1]
        );
        assert!(stitched.density[1].curvature > 3.0, "{:?}", stitched.density[1]);
        a.stitch = Switch::Off;
        let laid = lay_out(&scene, &a, None).expect("lay out");
        assert_eq!(laid.density.len(), 2);
        let (floor, ball) = (&laid.density[0], &laid.density[1]);
        assert_eq!(
            (floor.density_scale, floor.explicit, floor.warning),
            (1.0, false, None),
            "{floor:?}"
        );
        assert!(
            ball.density_scale > 1.0 && ball.density_scale.log2().fract() == 0.0,
            "{ball:?}"
        );
        assert!(
            ball.predicted_p95 <= seam_budget(&a) && ball.warning.is_none(),
            "{ball:?}"
        );
        assert!(
            laid.layout.entries[1].texels_per_metre > 8.0 * 1.5,
            "the sphere's entry is laid out at the raised density: {:?}",
            laid.layout.entries[1]
        );
        let json = density_json(&a, &laid.density);
        assert_eq!(json["auto"], true);
        assert_eq!(json["entries"][1]["density_scale"], ball.density_scale);
        assert!(json["warnings"].as_array().is_some_and(Vec::is_empty));

        // An explicit level scale wins, and is warned about when it predicts seams over the budget.
        let part = |instance: usize, density_scale: f32| InstancePart {
            instance,
            key: LightmapEntryKey {
                entity: regia_core::id::Guid::from_u128(instance as u128 + 1),
                part: 0,
            },
            label: format!("e{instance}"),
            density_scale,
            receive: true,
        };
        let parts = [part(0, 1.0), part(1, 0.5)];
        let laid = lay_out(&scene, &a, Some(&parts)).expect("lay out");
        let ball = &laid.density[1];
        assert_eq!((ball.density_scale, ball.explicit), (0.5, true), "{ball:?}");
        assert_eq!(
            ball.warning,
            Some(regia_pt::lightmap::density::DENSITY_LOW_CODE),
            "{ball:?}"
        );
        let json = density_json(&a, &laid.density);
        assert_eq!(json["warnings"][0]["code"], "lightmap_density_low");
        assert!(
            json["warnings"][0]["message"]
                .as_str()
                .is_some_and(|m| m.contains("kept"))
        );

        // Off: as asked, no prediction, no warning.
        a.auto_density = Switch::Off;
        let laid = lay_out(&scene, &a, None).expect("lay out");
        assert!(
            laid.density
                .iter()
                .all(|d| d.density_scale == 1.0 && d.warning.is_none())
        );
        assert_eq!(density_json(&a, &laid.density)["auto"], false);
    }

    /// The bake settings follow the switches: on for new bakes, `off` bakes as before ADR-2404.
    #[test]
    fn the_crease_and_stitch_switches_reach_the_baker() {
        let mut a = args(QUAD_BOX_SCENE);
        let s = bake_settings(&a);
        assert!(s.crease_placement && s.stitch_seams);
        a.crease = Switch::Off;
        a.stitch = Switch::Off;
        let s = bake_settings(&a);
        assert!(!s.crease_placement && !s.stitch_seams);
        // A small bake reports what they did; a dry run reports the density rule.
        let mut a = args(QUAD_BOX_SCENE);
        a.density = 4.0;
        a.dry_run = true;
        let out = execute(&a).expect("dry run");
        assert_eq!(out["density"]["auto"], true);
        assert_eq!(out["settings"]["crease_placement"], true);
        assert_eq!(out["settings"]["stitch_seams"], true);
    }
}
