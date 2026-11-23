// SPDX-License-Identifier: MPL-2.0
//! `regia bake-gi <scene>`: bakes a diffuse GI probe volume with the reference path tracer
//! (`regia_pt::probe`) and writes it as a `.fgiv` (`regia_asset::baked_gi`). `<scene>` is a
//! built-in scene (`cornell`, `furnace`, `rolling-hills`) or a bake description (`.bake.ron` v2,
//! `regia_pt::bake_desc::GiBakeDescription`, `docs/formats/bake-ron.md`). Prints a JSON summary
//! on stdout: probe counts per layer and brick, valid / invalid counts, the bounce budget, what
//! the clamps and firefly suppression removed, the noise estimate, time. `--dry-run`
//! validates the description and prints the layout without tracing. A description error prints
//! `{"ok": false, "code", "path", "message"}` (agent error codes, `book/agents/baked-gi-bake.md`).
//!
//! glTF mesh sources load here, through the asset pipeline's importer (`regia_pt` is below it).
//!
//! `--from-scene <level>` exports a level's bake input first ([`from_scene`], ADR-310).
//! `--cpu` bakes on the CPU probe tracer (`regia_pt::cpu_probe`, no GPU), and `--sample
//! <volume>` reads a baked volume back at given points ([`sample`], ADR-365).
//! `--incremental <state>` traces only the cells near what changed since the last bake
//! ([`incremental`], ADR-1094).

pub mod from_scene;
pub mod incremental;
pub mod sample;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args;
use regia_asset::baked_gi::{BakedGiVolume, ProbeFlags, ProbeMeta, VolumeLayout};
use regia_asset::{AssetId, CookedMesh, CookedModel, CookedStandardMaterial, MaterialAssetRon};
use regia_asset_pipeline::{Forward, TextureCookQuality, host_platform, import_gltf_path, parse_lod_name};
use regia_pt::PathTracer;
use regia_pt::bake_desc::{
    BUILTIN_SCENES, BakeDescError, GiBakeDescription, GiBakeJob, builtin, validate_bake_settings,
};
use regia_pt::bake_meshes::{
    BakeMeshLoader, BakeMeshPart, BakeMeshSource, CookedMeshLoader, pt_mesh_from_cpu,
};
use regia_pt::bake_quality::bounce_budget;
use regia_pt::cpu_probe::CpuProbeTracer;
use regia_pt::probe::{ProbeTracer, bake_gi_volume};
use serde_json::json;

/// Arguments of `regia bake-gi`.
#[derive(Debug, Clone, Args)]
pub struct BakeGiArgs {
    /// A built-in scene (`cornell`, `furnace`, `rolling-hills`) or a bake description `.ron`
    /// (not used with `--from-scene` / `--from-static-meshes` / `--sample`).
    #[arg(required_unless_present_any = ["from_scene", "from_static_meshes", "sample"])]
    pub scene: Option<String>,
    /// Output volume (default `<scene>.fgiv` in the current directory).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Rays per probe per sample (overrides the scene's).
    #[arg(long)]
    pub rays: Option<u32>,
    /// Samples per probe (overrides the scene's).
    #[arg(long)]
    pub samples: Option<u32>,
    /// Maximum path vertices (overrides the scene's).
    #[arg(long)]
    pub bounces: Option<u32>,
    /// Seed (overrides the scene's).
    #[arg(long)]
    pub seed: Option<u32>,
    /// Probes per GPU batch (overrides the scene's).
    #[arg(long)]
    pub probes_per_batch: Option<u32>,
    /// Path vertex from which Russian roulette may end a path (overrides the scene's).
    #[arg(long)]
    pub rr_start: Option<u32>,
    /// Luminance cap of light a probe ray sees without scattering, cd/m², 0 = off.
    #[arg(long)]
    pub clamp_direct: Option<f32>,
    /// Luminance cap of light after a scattering event, cd/m², 0 = off.
    #[arg(long)]
    pub clamp_indirect: Option<f32>,
    /// Sample groups (2+ print a noise estimate, 3+ allow firefly suppression).
    #[arg(long)]
    pub sample_groups: Option<u32>,
    /// Firefly suppression: scale a sample group above this many times the median group down
    /// to it (0 = off, else above 1; needs 3+ sample groups).
    #[arg(long)]
    pub firefly_threshold: Option<f32>,
    /// Skip the 3×3×3 visibility-weighted filter.
    #[arg(long)]
    pub no_filter: bool,
    /// Validate the description, load the meshes and print the layout without tracing (no GPU).
    #[arg(long)]
    pub dry_run: bool,
    /// Bake on the CPU probe tracer instead of the GPU (no GPU needed; for small scenes, CI and
    /// checks: diffuse surfaces, uniform sky; `regia_pt::cpu_probe`, ADR-365).
    #[arg(long)]
    pub cpu: bool,
    /// Read a baked volume (`.fgiv`) and print the irradiance the runtime lights each `--at`
    /// point with (lux, per component), instead of baking.
    #[arg(long, value_name = "FGIV", conflicts_with_all = ["from_scene", "from_static_meshes", "dry_run"])]
    pub sample: Option<PathBuf>,
    /// A point to sample, `x,y,z` (repeat for more; with `--sample`).
    #[arg(long, value_parser = sample::parse_vec3, allow_hyphen_values = true, requires = "sample")]
    pub at: Vec<[f32; 3]>,
    /// The surface normal at the sampled points, `x,y,z` (default `0,1,0`; with `--sample`).
    #[arg(long, value_parser = sample::parse_vec3, allow_hyphen_values = true, requires = "sample")]
    pub normal: Option<[f32; 3]>,
    /// Compare a baked volume with the description instead of baking (no GPU): prints whether it
    /// is stale and why, and fails with `bake_volume_stale` when it is (ADR-357; renamed from
    /// `--check` because `--from-scene --check` owns that name, ADR-417).
    #[arg(long, value_name = "FGIV", conflicts_with_all = ["from_scene", "from_static_meshes", "sample"])]
    pub check_volume: Option<PathBuf>,
    /// Export a level (or a DO27 bundle) to a `.bake.ron` first (ADR-310).
    #[command(flatten)]
    pub from_scene: from_scene::FromSceneArgs,
    /// Bake incrementally through a state file (ADR-1094).
    #[command(flatten)]
    pub inc: incremental::IncrementalArgs,
    /// `--checkpoint`, `--resume` and their cadence (ADR-3095).
    #[command(flatten)]
    pub checkpoint: super::bake_checkpoint::CheckpointArgs,
}

impl BakeGiArgs {
    /// The positional scene, `""` when none was given (`--from-scene` / `--from-static-meshes`).
    #[must_use]
    pub fn scene_name(&self) -> &str {
        self.scene.as_deref().unwrap_or("")
    }
}

/// Loads mesh sources for `regia bake-gi`: glTF (`.gltf`, `.glb`) through the asset pipeline's
/// importer (every primitive with its node transform, LOD0 only, `collision_*` and `socket_*`
/// nodes skipped, no axis conversion: the description's transforms are in Regia Engine's frame), and
/// cooked meshes through `regia_pt`'s [`CookedMeshLoader`].
#[derive(Clone, Copy, Debug, Default)]
pub struct GltfBakeLoader;

impl BakeMeshLoader for GltfBakeLoader {
    fn load(&mut self, path: &Path) -> Result<BakeMeshSource, String> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "fckd" => CookedMeshLoader.load(path),
            "gltf" | "glb" => {
                if !path.is_file() {
                    return Err(format!("{}: no such file", path.display()));
                }
                // The bake only needs geometry and material factors; Low quality keeps the
                // texture cook (which the importer always runs) short.
                let result = import_gltf_path(
                    path,
                    AssetId::new_random(),
                    Forward::Keep,
                    host_platform(),
                    None,
                    TextureCookQuality::Low,
                )
                .map_err(|e| format!("{}: {e}", path.display()))?;
                gltf_bake_source(&result.sub_assets).map_err(|e| format!("{}: {e}", path.display()))
            }
            _ => Err(format!(
                "{}: not a glTF (.gltf, .glb) or cooked mesh (.fckd)",
                path.display()
            )),
        }
    }
}

/// The bake parts of an imported glTF: every `model` part (LOD0 and non-LOD nodes) with its
/// cooked mesh and the factors of its material (white for glTF's default material).
///
/// # Errors
///
/// A sub-asset that does not decode, or no `model` sub-asset.
pub fn gltf_bake_source(sub_assets: &[regia_asset_pipeline::GltfSubCook]) -> Result<BakeMeshSource, String> {
    let mut meshes: HashMap<AssetId, CookedMesh> = HashMap::new();
    let mut materials: HashMap<AssetId, MaterialAssetRon> = HashMap::new();
    let mut model: Option<CookedModel> = None;
    for sub in sub_assets {
        match sub.type_path.as_str() {
            "regia_asset::CookedMesh" => {
                let mesh =
                    CookedMesh::from_postcard(&sub.payload).map_err(|e| format!("{}: {e}", sub.path))?;
                meshes.insert(sub.id, mesh);
            }
            "regia_asset::CookedStandardMaterial" => {
                let m = CookedStandardMaterial::from_postcard(&sub.payload)
                    .map_err(|e| format!("{}: {e}", sub.path))?;
                materials.insert(sub.id, m.authored);
            }
            "regia_asset::CookedModel" => {
                model =
                    Some(CookedModel::from_postcard(&sub.payload).map_err(|e| format!("{}: {e}", sub.path))?);
            }
            _ => {}
        }
    }
    let model = model.ok_or_else(|| "the import has no model".to_string())?;
    let mut parts = Vec::with_capacity(model.parts.len());
    for part in &model.parts {
        if parse_lod_name(&part.node).is_some_and(|(_, level)| level > 0) {
            continue;
        }
        let Some(mesh) = meshes.get(&part.mesh) else {
            continue;
        };
        let cpu = mesh.to_cpu_mesh().map_err(|e| format!("{}: {e}", part.node))?;
        let material = part.material.and_then(|id| materials.get(&id));
        let base = material.map(|m| &m.base);
        let strength = base.map_or(1.0, |b| b.emissive_strength);
        parts.push(BakeMeshPart {
            mesh: pt_mesh_from_cpu(&cpu),
            base_color: base.map_or([1.0; 4], |b| b.base_color),
            metallic: base.map_or(0.0, |b| b.metallic),
            roughness: base.map_or(1.0, |b| b.roughness),
            emissive: base.map_or([0.0; 3], |b| {
                [
                    b.emissive[0] * strength,
                    b.emissive[1] * strength,
                    b.emissive[2] * strength,
                ]
            }),
            textured: base.is_some_and(|b| b.base_color_map.is_some()),
        });
    }
    Ok(BakeMeshSource { parts })
}

/// The job for `args.scene` before the command-line overrides, as a [`BakeDescError`] on failure
/// (`bake_scene_unknown` for a name that is neither a built-in nor a `.ron`).
///
/// # Errors
///
/// An unknown built-in, an unreadable or invalid description.
pub fn resolve(args: &BakeGiArgs) -> Result<GiBakeJob, BakeDescError> {
    if let Some(job) = builtin(args.scene_name()) {
        return Ok(job);
    }
    let path = Path::new(args.scene_name());
    if path.extension().and_then(|e| e.to_str()) != Some("ron") {
        return Err(BakeDescError {
            code: "bake_scene_unknown",
            path: None,
            message: format!(
                "unknown scene `{}`: use one of {} or a bake description (.ron)",
                args.scene_name(),
                BUILTIN_SCENES.join(", ")
            ),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        BakeDescError::missing(args.scene_name(), format!("cannot read {}: {e}", path.display()))
    })?;
    let (desc, _) = GiBakeDescription::parse_versioned(&text)?;
    desc.build_with(path.parent().unwrap_or(Path::new(".")), &mut GltfBakeLoader)
}

/// The job for `args.scene` with the command-line overrides applied.
///
/// # Errors
///
/// An unknown built-in, an unreadable or invalid description.
#[cfg(test)]
pub fn job(args: &BakeGiArgs) -> Result<GiBakeJob> {
    let mut job = resolve(args)?;
    apply_overrides(&mut job, args);
    Ok(job)
}

/// Applies `--rays`, `--samples`, `--bounces`, `--seed`, `--probes-per-batch`, `--rr-start`,
/// `--clamp-direct`, `--clamp-indirect`, `--sample-groups`, `--firefly-threshold` and
/// `--no-filter` (checked afterwards by [`validate_bake_settings`]).
pub fn apply_overrides(job: &mut GiBakeJob, args: &BakeGiArgs) {
    let t = &mut job.settings.trace;
    if let Some(v) = args.rays {
        t.rays_per_probe = v;
    }
    if let Some(v) = args.samples {
        t.samples = v;
    }
    if let Some(v) = args.bounces {
        t.max_bounces = v;
    }
    if let Some(v) = args.seed {
        t.seed = v;
    }
    if let Some(v) = args.probes_per_batch {
        t.probes_per_batch = v;
    }
    if let Some(v) = args.rr_start {
        t.rr_start = v;
    }
    if let Some(v) = args.clamp_direct {
        t.clamp_direct = v;
    }
    if let Some(v) = args.clamp_indirect {
        t.clamp_indirect = v;
    }
    if let Some(v) = args.sample_groups {
        t.sample_groups = v;
    }
    if let Some(v) = args.firefly_threshold {
        t.firefly_threshold = v;
    }
    if args.no_filter {
        job.settings.filter = false;
    }
}

/// The default output path for `scene`.
#[must_use]
pub fn default_out(scene: &str) -> PathBuf {
    let stem = Path::new(scene)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("volume")
        .trim_end_matches(".bake");
    PathBuf::from(format!("{stem}.fgiv"))
}

/// Probe counts per layer and brick, with valid / invalid counts when `meta` (one per probe) is
/// given: `{"terrain": {...} | null, "bricks": [...], "probes", "valid", "invalid"}`.
#[must_use]
pub fn layer_summary(job: &GiBakeJob, meta: Option<&[ProbeMeta]>) -> serde_json::Value {
    let layout: &VolumeLayout = &job.layout;
    let counts = |first: usize, n: usize| -> serde_json::Value {
        match meta.and_then(|m| m.get(first..first + n)) {
            Some(slice) => {
                let valid = slice.iter().filter(|p| p.is_valid()).count();
                let flag = |f: u16| slice.iter().filter(|p| p.flags & f != 0).count();
                json!({
                    "valid": valid,
                    "invalid": n - valid,
                    "under_terrain": flag(ProbeFlags::UNDER_TERRAIN),
                    "inside_geometry": flag(ProbeFlags::INSIDE_GEOMETRY),
                    "relocated": flag(ProbeFlags::RELOCATED),
                })
            }
            None => serde_json::Value::Null,
        }
    };
    let terrain = layout.terrain.as_ref().map(|t| {
        json!({
            "origin": t.origin,
            "spacing": t.spacing,
            "counts": t.counts,
            "levels": t.levels,
            "probes": t.probe_count(),
            "validity": counts(0, t.probe_count()),
        })
    });
    let bricks: Vec<serde_json::Value> = layout
        .bricks
        .iter()
        .enumerate()
        .map(|(i, b)| {
            json!({
                "index": i,
                "first_probe": b.first_probe,
                "origin": b.origin,
                "spacing": b.spacing,
                "counts": b.counts,
                "probes": b.probe_count(),
                "plan": job.plan.bricks.get(i),
                "validity": counts(b.first_probe as usize, b.probe_count()),
            })
        })
        .collect();
    let total = layout.probe_count();
    let valid = meta.map(|m| m.iter().filter(|p| p.is_valid()).count());
    json!({
        "terrain": terrain,
        "bricks": bricks,
        "probes": total,
        "valid": valid,
        "invalid": valid.map(|v| total - v),
    })
}

/// The `settings` object of the summary: the trace and quality settings the bake runs with.
#[must_use]
pub fn settings_json(job: &GiBakeJob) -> serde_json::Value {
    let t = &job.settings.trace;
    json!({
        "rays_per_probe": t.rays_per_probe,
        "samples": t.samples,
        "max_bounces": t.max_bounces,
        "rr_start": t.rr_start,
        "seed": t.seed,
        "clamp_direct": t.clamp_direct,
        "clamp_indirect": t.clamp_indirect,
        "sample_groups": t.sample_groups,
        "firefly_threshold": t.firefly_threshold,
        "filter": job.settings.filter,
        "moment_resolution": job.layout.moment_resolution,
        "max_distance": job.layout.max_distance,
    })
}

fn error_json(e: &BakeDescError) -> serde_json::Value {
    json!({ "ok": false, "code": e.code, "path": e.path, "message": e.message })
}

/// The `--check-volume` summary of `volume` (read from `path`) against `job`: `{"ok", "check", "scene",
/// "stale", "reasons", "code", "volume", "description"}`. `reasons` lists what changed since the bake
/// ([`GiBakeJob::stale_reasons`]: `scene`, `layout`, `lights`, `settings`, `emitters`); `code` is
/// `bake_volume_stale` when any did, else null. `memory` is the description's bytes per tier
/// ([`regia_pt::bake_sizing::job_memory`]).
#[must_use]
pub fn check_summary(job: &GiBakeJob, scene: &str, path: &Path, volume: &BakedGiVolume) -> serde_json::Value {
    let reasons = job.stale_reasons(volume);
    let stale = !reasons.is_empty();
    json!({
        "ok": !stale,
        "check": path.display().to_string(),
        "scene": scene,
        "stale": stale,
        "reasons": reasons,
        "code": stale.then_some("bake_volume_stale"),
        "volume": {
            "probes": volume.probe_count(),
            "interior_bricks": volume.interior_brick_count(),
            "scene_hash": format!("{:016x}", volume.info.scene_hash),
        },
        "description": {
            "probes": job.layout.probe_count(),
            "interior_bricks": job.layout.bricks.iter().filter(|b| b.is_interior()).count(),
            "scene_hash": format!("{:016x}", job.scene.content_hash()),
        },
        // What the description would bake to against every tier's budget (ADR-3006).
        "memory": regia_pt::bake_sizing::job_memory(job),
    })
}

/// `--check-volume`: prints [`check_summary`]; an unreadable volume prints `bake_volume_unreadable`.
/// A split volume (ADR-2618) is read as its whole set from part 0 and joined back into the volume
/// the bake split, so it checks as the bake it is; a missing or foreign part is unreadable.
fn check(job: &GiBakeJob, scene: &str, path: &Path) -> Result<()> {
    let joined = BakedGiVolume::read_split_set(path)
        .and_then(|parts| BakedGiVolume::join_split_set(&parts).map(|whole| (whole, split_summary(&parts))));
    let (volume, split) = match joined {
        Ok(v) => v,
        Err(e) => {
            let err = BakeDescError {
                code: "bake_volume_unreadable",
                path: Some(path.display().to_string()),
                message: e.to_string(),
            };
            println!("{}", serde_json::to_string_pretty(&error_json(&err))?);
            return Err(err.into());
        }
    };
    let mut summary = check_summary(job, scene, path, &volume);
    summary["split"] = split;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if summary["stale"].as_bool() == Some(true) {
        return Err(BakeDescError {
            code: "bake_volume_stale",
            path: Some(path.display().to_string()),
            message: format!("the volume is stale: {}", summary["reasons"]),
        }
        .into());
    }
    Ok(())
}

/// The JSON `--dry-run` prints: the layout summary and plan, no GPU involved.
fn dry_run_summary(
    args: &BakeGiArgs,
    job: &GiBakeJob,
    settings: serde_json::Value,
    seconds: f64,
) -> serde_json::Value {
    json!({
        "ok": true,
        "dry_run": true,
        "scene": args.scene_name(),
        "layers": layer_summary(job, None),
        "plan": job.plan,
        "settings": settings,
        "bounces": bounce_budget(&job.scene, job.settings.trace.max_bounces),
        "seconds": seconds,
        // The bytes against every tier's budget, what is over and what fixes it (ADR-3006).
        "memory": regia_pt::bake_sizing::job_memory(job),
    })
}

/// Runs `regia bake-gi`.
///
/// # Errors
///
/// No GPU, a bad scene, a failed bake, or a failed write.
pub fn run(args: &BakeGiArgs) -> Result<()> {
    if args.sample.is_some() {
        return sample::run(args);
    }
    if args.from_scene.active() {
        if args.inc.active() && !args.from_scene.bake {
            // Only a bake reads and writes the state; an export alone would ignore it.
            anyhow::bail!("--incremental with --from-scene needs --bake");
        }
        return from_scene::run(args);
    }
    if args.from_scene.bake || args.from_scene.check {
        anyhow::bail!("--bake and --check need --from-scene or --from-static-meshes");
    }
    let started = Instant::now();
    let resolved = resolve(args).and_then(|mut job| {
        apply_overrides(&mut job, args);
        validate_bake_settings(&job.settings).map(|()| job)
    });
    let job = match resolved {
        Ok(job) => job,
        Err(e) => {
            println!("{}", serde_json::to_string_pretty(&error_json(&e))?);
            return Err(e.into());
        }
    };
    let built = started.elapsed().as_secs_f64();
    let settings = settings_json(&job);
    if let Some(path) = &args.check_volume {
        return check(&job, args.scene_name(), path);
    }
    if args.dry_run {
        let summary = dry_run_summary(args, &job, settings, built);
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }
    let out = args.out.clone().unwrap_or_else(|| default_out(args.scene_name()));
    let mut summary = match incremental::bake_for(args, &job, &out, args.scene_name(), started, built) {
        Ok(summary) => summary,
        Err(e) => {
            if let Some(failure) = e.downcast_ref::<CheckpointFailure>() {
                println!("{}", serde_json::to_string_pretty(&failure.to_json())?);
            }
            return Err(e);
        }
    };
    summary["settings"] = settings;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// A checkpoint error of a bake (`bake_checkpoint_*`, `bake_interrupted`) with its code and the
/// checkpoint's path. The bake returns it; the command that ran the bake prints it once, as its
/// own JSON (`{ok: false, code, path, error}` for `regia bake-gi`, the `--from-scene` error line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointFailure {
    /// The error code.
    pub code: &'static str,
    /// The checkpoint file.
    pub path: String,
    /// What went wrong.
    pub message: String,
}

impl CheckpointFailure {
    /// The failure JSON `regia bake-gi` prints.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        json!({ "ok": false, "code": self.code, "path": self.path, "error": self.message })
    }
}

impl std::fmt::Display for CheckpointFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.starts_with(self.code) {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for CheckpointFailure {}

/// Where the probe traces run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BakeBackend {
    /// The GPU path tracer (`regia_pt::PathTracer`, a headless device).
    #[default]
    Gpu,
    /// The CPU probe tracer (`regia_pt::cpu_probe::CpuProbeTracer`, every core).
    Cpu,
}

impl BakeBackend {
    /// `gpu` or `cpu`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        }
    }
}

impl BakeGiArgs {
    /// The backend `--cpu` selects.
    #[must_use]
    pub fn backend(&self) -> BakeBackend {
        if self.cpu {
            BakeBackend::Cpu
        } else {
            BakeBackend::Gpu
        }
    }
}

/// Bakes `job` on `backend` and writes the volume to `out`; returns the bake summary (`ok`,
/// `out`, `bytes`, `scene`, `backend`, `device`, `seconds`, `build_seconds`, `report`, `layers`,
/// `plan`). `started` is when the command started, `built` the seconds the job took to build.
///
/// # Errors
///
/// No GPU (for [`BakeBackend::Gpu`]), a scene the CPU tracer does not take, a failed bake, or a
/// failed write.
pub fn bake_job(
    backend: BakeBackend,
    job: &GiBakeJob,
    out: &Path,
    scene: &str,
    started: Instant,
    built: f64,
) -> Result<serde_json::Value> {
    bake_job_checkpointed(backend, job, out, scene, started, built, None)
}

/// [`bake_job`] checkpointed by `plan` (ADR-3095, `--checkpoint` / `--resume`): the summary gains
/// `checkpoint` (where, what was resumed, how many were written and the largest's size). A
/// checkpoint error prints `{ok: false, code, path, error}` (`bake_checkpoint_mismatch`,
/// `bake_checkpoint_damaged`, `bake_checkpoint_version`, `bake_checkpoint_io`, `bake_interrupted`)
/// before it fails.
///
/// # Errors
///
/// As [`bake_job`], and the checkpoint's errors.
pub fn bake_job_checkpointed(
    backend: BakeBackend,
    job: &GiBakeJob,
    out: &Path,
    scene: &str,
    started: Instant,
    built: f64,
    plan: Option<&regia_pt::checkpoint::BakeCheckpointPlan>,
) -> Result<serde_json::Value> {
    let (device, tracer): (String, Box<dyn ProbeTracer>) = match backend {
        BakeBackend::Gpu => {
            let device = regia_render::RenderDevice::headless().context("no GPU adapter for the bake")?;
            let tracer = PathTracer::new(&device)?;
            let info = device.adapter.get_info();
            (format!("{} ({:?})", info.name, info.backend), Box::new(tracer))
        }
        BakeBackend::Cpu => {
            let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
            (
                format!("cpu ({threads} threads)"),
                Box::new(CpuProbeTracer::new()),
            )
        }
    };
    let mut last = Instant::now();
    let (_, prior_seconds) = plan.map_or((0, 0.0), |p| super::bake_checkpoint::announce(p, "bake"));
    let bake_started = Instant::now();
    let progress = |stage: &str, done: usize, total: usize| {
        if last.elapsed().as_secs_f32() > 2.0 || done == total {
            // A checkpointed bake's main trace counts its earlier runs: the estimate resumes.
            let eta = if plan.is_some() && stage == "bake" {
                let seconds = prior_seconds + bake_started.elapsed().as_secs_f64();
                super::bake_checkpoint::eta_suffix(done as u64, total as u64, seconds)
            } else {
                String::new()
            };
            eprintln!("  {stage}: {done}/{total} probes{eta}");
            last = Instant::now();
        }
        true
    };
    let (bake, mut checkpoint) = match plan {
        None => (
            bake_gi_volume(tracer.as_ref(), &job.scene, &job.layout, &job.settings, progress)?,
            None,
        ),
        Some(plan) => {
            let baked = regia_pt::probe::bake_gi_volume_checkpointed(
                tracer.as_ref(),
                &job.scene,
                &job.layout,
                &job.settings,
                None,
                plan,
                progress,
            );
            match baked {
                Ok((bake, stats)) => (bake, Some(stats)),
                Err(e) => {
                    if let Some(code) = regia_pt::checkpoint::bake_checkpoint_error_code(&e) {
                        return Err(CheckpointFailure {
                            code,
                            path: plan.path.display().to_string(),
                            message: e.to_string(),
                        }
                        .into());
                    }
                    return Err(e.into());
                }
            }
        }
    };
    let parts = incremental::write_volume(&bake.volume, out)?;
    // Only now that the volume is on disk is the checkpoint dropped: a failed write keeps it.
    if let (Some(plan), Some(stats)) = (plan, checkpoint.as_mut()) {
        super::bake_checkpoint::finish_checkpoint(plan, stats);
    }
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    let meta: Vec<ProbeMeta> = bake.raw.iter().map(|p| p.meta).collect();
    Ok(json!({
        "ok": true,
        "out": out.display().to_string(),
        "bytes": bytes,
        "scene": scene,
        "backend": backend.name(),
        "device": device,
        "seconds": started.elapsed().as_secs_f64(),
        "build_seconds": built,
        "report": bake.report,
        "layers": layer_summary(job, Some(&meta)),
        "plan": job.plan,
        "parts": part_summary(&parts),
        "checkpoint": checkpoint,
    }))
}

/// `--check-volume`'s `split`: `null` for one volume, else per part (index order) its runtime
/// bytes, its bricks and how many of them are crops of a brick that crosses a cut (ADR-3237).
pub(crate) fn split_summary(parts: &[BakedGiVolume]) -> serde_json::Value {
    if parts.len() <= 1 {
        return serde_json::Value::Null;
    }
    json!(
        parts
            .iter()
            .map(|p| {
                let layout = p.layout();
                json!({
                    "bytes": layout.runtime_buffer_bytes(),
                    "bricks": layout.bricks.len(),
                    "cropped_bricks": p.split.as_ref().map_or(0, |s| s.cropped_bricks(&layout)),
                })
            })
            .collect::<Vec<_>>()
    )
}

/// The bake summary's `parts`: `null` for one volume, else every part file written (ADR-2618;
/// the level names part 0, the runtime reads the rest beside it).
pub(crate) fn part_summary(parts: &[PathBuf]) -> serde_json::Value {
    if parts.len() <= 1 {
        return serde_json::Value::Null;
    }
    json!(parts.iter().map(|p| p.display().to_string()).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn args(scene: &str) -> BakeGiArgs {
        BakeGiArgs {
            scene: Some(scene.into()),
            out: None,
            rays: Some(16),
            samples: None,
            bounces: Some(2),
            seed: Some(4),
            probes_per_batch: None,
            rr_start: None,
            clamp_direct: None,
            clamp_indirect: None,
            sample_groups: None,
            firefly_threshold: None,
            no_filter: true,
            dry_run: false,
            cpu: false,
            sample: None,
            at: Vec::new(),
            normal: None,
            check_volume: None,
            from_scene: from_scene::FromSceneArgs::default(),
            inc: incremental::IncrementalArgs::default(),
            checkpoint: crate::commands::bake_checkpoint::CheckpointArgs::default(),
        }
    }

    /// The quality flags reach the settings and the summary; a bad one fails before any trace
    /// with its `bake.*` key, through the dry run too.
    #[test]
    fn quality_flags_override_and_are_checked() {
        let mut a = args("cornell");
        a.samples = Some(12);
        a.rr_start = Some(6);
        a.clamp_direct = Some(5000.0);
        a.clamp_indirect = Some(800.0);
        a.sample_groups = Some(6);
        a.firefly_threshold = Some(4.0);
        let j = job(&a).expect("cornell");
        let t = j.settings.trace;
        assert_eq!((t.rr_start, t.sample_groups, t.firefly_threshold), (6, 6, 4.0));
        assert_eq!((t.clamp_direct, t.clamp_indirect), (5000.0, 800.0));
        validate_bake_settings(&j.settings).expect("valid");
        let s = settings_json(&j);
        assert_eq!(s["sample_groups"], 6);
        assert_eq!(s["clamp_indirect"], 800.0);
        assert_eq!(s["max_bounces"], 2);
        let budget = bounce_budget(&j.scene, t.max_bounces);
        assert!(
            budget.max_albedo > 0.5 && budget.missing_bound > 0.25,
            "{budget:?}"
        );
        a.dry_run = true;
        run(&a).expect("dry run");
        for (bad, key) in [
            (
                BakeGiArgs {
                    firefly_threshold: Some(0.5),
                    ..a.clone()
                },
                "bake.firefly_threshold",
            ),
            (
                BakeGiArgs {
                    sample_groups: Some(2),
                    ..a.clone()
                },
                "bake.sample_groups",
            ),
            (
                BakeGiArgs {
                    sample_groups: Some(13),
                    firefly_threshold: None,
                    ..a.clone()
                },
                "bake.sample_groups",
            ),
            (
                BakeGiArgs {
                    bounces: Some(0),
                    ..a.clone()
                },
                "bake.max_bounces",
            ),
            (
                BakeGiArgs {
                    clamp_indirect: Some(-1.0),
                    ..a.clone()
                },
                "bake.clamp_indirect",
            ),
        ] {
            let mut j = resolve(&bad).expect("resolve");
            apply_overrides(&mut j, &bad);
            let err = validate_bake_settings(&j.settings).expect_err(key);
            assert_eq!((err.code, err.path.as_deref()), ("bake_desc_invalid", Some(key)));
            let message = format!("{:#}", run(&bad).expect_err(key));
            assert!(message.contains(key), "{message}");
        }
    }

    #[test]
    fn builtins_resolve_with_overrides() {
        let j = job(&args("furnace")).expect("furnace");
        assert_eq!(j.settings.trace.rays_per_probe, 16);
        assert_eq!(j.settings.trace.max_bounces, 2);
        assert_eq!(j.settings.trace.seed, 4);
        assert!(!j.settings.filter);
        assert!(job(&args("no-such-scene")).is_err());
        assert!(job(&args("/nonexistent/map.ron")).is_err());
        assert_eq!(default_out("cornell"), PathBuf::from("cornell.fgiv"));
        assert_eq!(default_out("maps/bore.bake.ron"), PathBuf::from("bore.fgiv"));
        assert_eq!(
            resolve(&args("no-such-scene")).expect_err("unknown").code,
            "bake_scene_unknown"
        );
        assert_eq!(
            resolve(&args("/nonexistent/map.ron")).expect_err("missing").code,
            "bake_source_missing"
        );
    }

    /// `--cpu` bakes without a GPU: the furnace (empty, uniform sky of radiance 1) reads
    /// irradiance / π = 1 on every normal through `--sample`'s lookup, every probe is valid and
    /// the summary names the backend (ADR-365).
    #[test]
    fn the_furnace_bakes_on_the_cpu_and_samples_back() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_cpu");
        let out = dir.join("furnace.fgiv");
        let mut a = args("furnace");
        a.cpu = true;
        assert_eq!(a.backend(), BakeBackend::Cpu);
        let j = job(&a).expect("furnace");
        let summary = bake_job(a.backend(), &j, &out, "furnace", Instant::now(), 0.0).expect("cpu bake");
        assert_eq!(summary["ok"], true);
        assert_eq!(summary["backend"], "cpu");
        assert_eq!(summary["report"]["probes"], 27);
        assert_eq!(summary["report"]["valid"], 27);
        let volume = sample::read_volume(&out).expect("read back");
        for n in [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]] {
            let s = sample::sample_at(&volume, [0.0; 3], n);
            assert_eq!(s.coverage, 1.0);
            for (c, v) in s.irradiance_over_pi.iter().enumerate() {
                assert!((v - 1.0).abs() < 0.03, "channel {c} for {n:?}: {v}");
            }
            assert!(s.components[0].iter().all(|v| *v == 0.0), "no sun");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkpointed bake whose volume write fails keeps its checkpoint; the rerun resumes it.
    #[test]
    fn audit_a_failed_volume_write_keeps_the_checkpoint() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_gi_keep_ck");
        let mut a = args("furnace");
        a.cpu = true;
        let j = job(&a).expect("furnace");
        let out = dir.join("furnace.fgiv");
        a.checkpoint = crate::commands::bake_checkpoint::CheckpointArgs {
            checkpoint: true,
            checkpoint_seconds: 0.0,
            checkpoint_batch: 4,
            checkpoint_stop_after: Some(3),
            ..crate::commands::bake_checkpoint::CheckpointArgs::default()
        };
        let ck = regia_pt::checkpoint::default_checkpoint_path(&out);
        incremental::bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect_err("stopped");
        assert!(ck.is_file());
        std::fs::create_dir_all(out.join("blocker")).expect("blocking dir");
        a.checkpoint.checkpoint_stop_after = None;
        incremental::bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect_err("write fails");
        assert!(ck.is_file(), "a failed volume write keeps the checkpoint");
        std::fs::remove_dir_all(&out).expect("unblock");
        let v = incremental::bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect("resumed");
        assert_eq!(v["checkpoint"]["resumed"], true, "{v}");
        assert_eq!(v["checkpoint"]["removed"], true, "{v}");
        assert!(!ck.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--checkpoint` on the CPU (ADR-3095): stopped after the relocation (batch 1) and again
    /// after two main batches, the bake continues each time and writes the volume of a bake that
    /// never stopped, byte for byte; the checkpoint is gone afterwards.
    #[test]
    fn a_checkpointed_cpu_bake_resumes_to_the_same_volume() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_gi_resume");
        let mut a = args("furnace");
        a.cpu = true;
        let j = job(&a).expect("furnace");
        let plain = dir.join("plain.fgiv");
        bake_job(a.backend(), &j, &plain, "furnace", Instant::now(), 0.0).expect("plain");
        let out = dir.join("furnace.fgiv");
        a.checkpoint = crate::commands::bake_checkpoint::CheckpointArgs {
            checkpoint: true,
            checkpoint_seconds: 0.0,
            checkpoint_batch: 4,
            ..crate::commands::bake_checkpoint::CheckpointArgs::default()
        };
        let ck = regia_pt::checkpoint::default_checkpoint_path(&out);
        for stop in [1, 3] {
            a.checkpoint.checkpoint_stop_after = Some(stop);
            let e = incremental::bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect_err("stopped");
            assert!(e.to_string().contains("bake_interrupted"), "{e}");
            let h = regia_asset::bake_checkpoint::read_header(&ck).expect("checkpoint");
            assert_eq!((h.batches_done, h.units_done), (stop, 4 * (stop - 1)));
        }
        a.checkpoint.checkpoint_stop_after = None;
        let v = incremental::bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).expect("resumed");
        assert_eq!(v["checkpoint"]["resumed"], true, "{v}");
        assert_eq!(v["checkpoint"]["resumed_units"], 8);
        assert!(!ck.exists());
        assert_eq!(
            std::fs::read(&out).expect("resumed"),
            std::fs::read(&plain).expect("plain")
        );
        // --checkpoint does not combine with --incremental.
        a.inc.incremental = Some(dir.join("state.fgis"));
        assert!(incremental::bake_for(&a, &j, &out, "furnace", Instant::now(), 0.0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A v2 description's bake controls reach the job, the flags override them, and a v1 file of
    /// the same scene resolves to the v1 defaults (no clamp, one group).
    #[test]
    fn a_v2_description_carries_its_controls_and_flags_override_them() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_v2_controls");
        let write = |name: &str, text: &str| {
            let file = dir.join(name);
            let tmp = dir.join(format!("{name}.tmp"));
            std::fs::write(&tmp, text).expect("write");
            std::fs::rename(&tmp, &file).expect("rename");
            file.display().to_string()
        };
        let body = "sky: Some((1.0, 1.0, 1.0)), layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 0.5)])";
        let v2 = write(
            "room.bake.ron",
            &format!(
                "(format_version: 2, {body}, bake: (samples: 16, max_bounces: 12, rr_start: 4, \
                 clamp_indirect: 300.0, sample_groups: 8, firefly_threshold: 5.0))"
            ),
        );
        let v1 = write("old.bake.ron", &format!("(format_version: 1, {body})"));
        let plain = |scene: &str| BakeGiArgs {
            rays: None,
            bounces: None,
            seed: None,
            no_filter: false,
            dry_run: true,
            ..args(scene)
        };
        let t = job(&plain(&v2)).expect("v2").settings.trace;
        assert_eq!((t.max_bounces, t.rr_start, t.sample_groups), (12, 4, 8));
        assert_eq!(
            (t.clamp_indirect, t.firefly_threshold, t.clamp_direct),
            (300.0, 5.0, 0.0)
        );
        let flags = BakeGiArgs {
            bounces: Some(20),
            clamp_indirect: Some(0.0),
            sample_groups: Some(16),
            ..plain(&v2)
        };
        let t = job(&flags).expect("flags").settings.trace;
        assert_eq!((t.max_bounces, t.clamp_indirect, t.sample_groups), (20, 0.0, 16));
        run(&flags).expect("dry run");
        let old = job(&plain(&v1)).expect("v1").settings.trace;
        assert_eq!(
            (old.sample_groups, old.clamp_indirect, old.firefly_threshold),
            (1, 0.0, 0.0)
        );
        assert_eq!(
            resolve(&plain(&v1)).expect("v1").plan.format_version,
            regia_pt::bake_desc::BAKE_DESC_FORMAT_VERSION
        );
        // A bad control in the file fails with its key, before any GPU is needed.
        let bad = write(
            "bad.bake.ron",
            &format!(
                "(format_version: 2, {body}, bake: (samples: 4, sample_groups: 2, firefly_threshold: 3.0))"
            ),
        );
        let err = resolve(&plain(&bad)).expect_err("two groups cannot suppress");
        assert_eq!(
            (err.code, err.path.as_deref()),
            ("bake_desc_invalid", Some("bake.sample_groups"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--dry-run` runs without a GPU: a built-in's layout summary lists its brick and no
    /// validity; an invalid description fails with its key path and code.
    #[test]
    fn dry_run_prints_the_layout_or_the_key_path() {
        let mut a = args("furnace");
        a.dry_run = true;
        run(&a).expect("dry run");
        let j = job(&a).expect("furnace");
        let summary = dry_run_summary(&a, &j, json!({}), 0.0);
        assert_eq!(summary["ok"], true);
        assert_eq!(summary["dry_run"], true);
        assert_eq!(summary["layers"]["probes"], 27);
        assert_eq!(summary["layers"]["bricks"][0]["counts"], json!([3, 3, 3]));
        // The memory answer: a small built-in fits every tier, one part, nothing to fix (ADR-3006).
        let memory = &summary["memory"];
        assert_eq!(memory["parts"], json!([j.layout.runtime_buffer_bytes()]));
        assert_eq!(memory["binding_limit"], 128u64 << 20);
        let tiers = memory["tiers"].as_array().expect("tiers");
        let names: Vec<&str> = tiers.iter().filter_map(|t| t["tier"].as_str()).collect();
        assert_eq!(names, ["minimum", "deck", "desktop_high", "integrated_low"]);
        assert!(
            tiers.iter().all(|t| t["fits"] == true && t["over"] == json!([])),
            "{memory}"
        );
        let layers = layer_summary(&j, None);
        assert_eq!(layers["probes"], 27);
        assert_eq!(layers["bricks"][0]["counts"], json!([3, 3, 3]));
        assert!(layers["valid"].is_null());
        let mut meta = vec![
            ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            };
            27
        ];
        meta[3].flags = ProbeFlags::INSIDE_GEOMETRY;
        let layers = layer_summary(&j, Some(&meta));
        assert_eq!(layers["invalid"], 1);
        assert_eq!(layers["bricks"][0]["validity"]["inside_geometry"], 1);

        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_dry_run");
        let file = dir.join("bad.bake.ron");
        let tmp = dir.join("bad.bake.ron.tmp");
        std::fs::write(
            &tmp,
            "(format_version: 1, sky: Some((1.0, 1.0, 1.0)), layout: (auto_bricks: Some((spacing: 0.0))))",
        )
        .expect("write");
        std::fs::rename(&tmp, &file).expect("rename");
        let mut a = args(&file.display().to_string());
        a.dry_run = true;
        let err = resolve(&a).expect_err("invalid");
        assert_eq!(err.code, "bake_desc_invalid");
        assert_eq!(err.path.as_deref(), Some("layout.auto_bricks.spacing"));
        let printed = error_json(&err);
        assert_eq!(printed["path"], "layout.auto_bricks.spacing");
        assert_eq!(printed["ok"], false);
        let message = format!("{:#}", run(&a).expect_err("dry run fails"));
        assert!(message.contains("layout.auto_bricks.spacing"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review baked-light #6: `--check` passes a volume baked from the same job, fails a stale
    /// one with its reasons (here the seed), and fails an unreadable file; no GPU.
    #[test]
    fn check_reports_a_stale_volume() {
        use regia_asset::baked_gi::{GI_COMPONENTS, ProbeData};
        use regia_pt::probe::{bake_info, bake_lights};
        let j = job(&args("furnace")).expect("furnace");
        let l = &j.layout;
        let far = l.max_distance;
        let probes: Vec<ProbeData> = (0..l.probe_count())
            .map(|_| ProbeData {
                sh: [[[0.0; 4]; 3]; GI_COMPONENTS],
                moments: vec![[far, far * far]; (l.moment_resolution * l.moment_resolution) as usize],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let volume = BakedGiVolume::from_probes(
            l,
            bake_lights(&j.scene),
            bake_info(&j.scene, &j.settings),
            &probes,
        )
        .expect("volume");
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_check");
        let file = dir.join("furnace.fgiv");
        let tmp = dir.join("furnace.fgiv.tmp");
        volume.write(&tmp).expect("write");
        std::fs::rename(&tmp, &file).expect("rename");

        let mut a = args("furnace");
        a.check_volume = Some(file.clone());
        run(&a).expect("fresh volume");
        let fresh = check_summary(&j, "furnace", &file, &volume);
        assert_eq!(fresh["ok"], true);
        assert_eq!(fresh["memory"]["tiers"][1]["tier"], "deck", "{fresh}");
        assert_eq!(fresh["memory"]["tiers"][1]["fits"], true, "{fresh}");
        assert_eq!(fresh["reasons"], json!([]));
        assert!(fresh["code"].is_null());

        a.seed = Some(5);
        let message = format!("{:#}", run(&a).expect_err("stale"));
        assert!(message.contains("bake_volume_stale"), "{message}");
        let reseeded = job(&a).expect("furnace");
        let stale = check_summary(&reseeded, "furnace", &file, &volume);
        assert_eq!(stale["stale"], true);
        assert_eq!(stale["reasons"], json!(["settings"]));
        assert_eq!(stale["code"], "bake_volume_stale");

        a.check_volume = Some(dir.join("missing.fgiv"));
        let message = format!("{:#}", run(&a).expect_err("missing"));
        assert!(message.contains("bake_volume_unreadable"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-3237: `--check-volume`'s `split` names each part's bytes, bricks and cropped bricks
    /// (`null` for one volume).
    #[test]
    fn the_check_split_summary_counts_cropped_bricks() {
        use regia_asset::baked_gi::{
            BakeInfo, BakeLights, DetailBrick, GI_COMPONENTS, ProbeData, ProbeFlags, TerrainLayer,
            VolumeLayout,
        };
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [24, 4],
                levels: vec![1.0],
                ground: vec![0.0; 96],
            }),
            bricks: vec![DetailBrick {
                origin: [2.0, 0.0, 2.0],
                spacing: [1.0; 3],
                counts: [85, 6, 6],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [[[0.5, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        assert!(split_summary(std::slice::from_ref(&whole)).is_null());
        let parts = whole
            .split_parts(layout.bricks[0].probe_count() as u64 * 136 * 7 / 10)
            .expect("cropped split");
        let got = split_summary(&parts);
        let rows = got.as_array().expect("one row a part");
        assert_eq!(rows.len(), parts.len());
        for (row, part) in rows.iter().zip(&parts) {
            assert_eq!(row["bytes"], part.layout().runtime_buffer_bytes());
            assert_eq!(row["bricks"], 1);
            assert_eq!(row["cropped_bricks"], 1, "{got}");
        }
    }
}
