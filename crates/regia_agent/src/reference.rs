// SPDX-License-Identifier: MPL-2.0
//! `seeing.reference_trace` and `seeing.compare` (`docs/specs/27-renderer-excellence.md` § 5).
//!
//! The reference path tracer (`regia_pt`) is an offline tool and never part of a shipped game,
//! so it is behind this crate's `reference-trace` feature; without it both methods answer
//! `F0013`. Traces run as background jobs: the first call returns a job id, later calls with
//! `job` poll progress, `cancel: true` stops early, `wait: true` blocks until done.
//!
//! `preset` (draft, preview, final, reference; ADR-2433) applies a named quality table with the
//! built-in denoiser, adaptive sampling and the caustics modes (`book/agents/pt-presets.md`).
//!
//! `denoise: true` (the `denoise` feature: Intel OIDN, desktop tools only) also writes an OIDN
//! preview of the trace. A preview is never a reference: `seeing.compare` refuses one as `a`.

use serde_json::Value;

use regia_app::prelude::*;

use crate::protocol::{self, RpcError};

#[cfg(not(feature = "reference-trace"))]
fn not_built() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0013",
        "this build has no reference path tracer",
        "build a desktop tool host with regia_agent's `reference-trace` feature (off by default; a shipped game never links the tracer)",
    )
}

/// `seeing.reference_trace`.
#[cfg(not(feature = "reference-trace"))]
pub(crate) fn reference_trace(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Err(not_built())
}

/// `seeing.compare`.
#[cfg(not(feature = "reference-trace"))]
pub(crate) fn compare(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Err(not_built())
}

#[cfg(feature = "reference-trace")]
pub(crate) use enabled::{compare, reference_trace};

#[cfg(feature = "reference-trace")]
mod enabled {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use regia_pt::{Aov, PathTracer, PresetSettings, PtCache, PtPreset, PtScene, PtSettings};
    use serde_json::json;

    use super::*;

    /// Running and finished trace jobs.
    #[derive(Resource, Default)]
    pub(crate) struct ReferenceJobs {
        next: u64,
        pub(super) jobs: HashMap<u64, Arc<Job>>,
        /// Bottom-level BVHs shared by successive traces (meshes matched by content), so a
        /// trace from a moved camera rebuilds only the top level.
        cache: Arc<Mutex<PtCache>>,
        /// Trace threads not joined yet (each holds a device clone until it returns).
        threads: Vec<std::thread::JoinHandle<()>>,
    }

    /// How long dropping [`ReferenceJobs`] waits for the cancelled trace threads in all.
    const JOIN_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

    /// Most traces running at once; a further start answers `too_many_pending` (each trace is a
    /// GPU thread with its own device clone; audit-concurrency-1103 agent H3).
    pub(super) const MAX_RUNNING: usize = 2;

    /// Most finished jobs kept for polling and `seeing.compare` (each holds its result, base64
    /// PNG included); the oldest finished ones are forgotten first.
    pub(super) const MAX_FINISHED: usize = 16;

    impl ReferenceJobs {
        /// Jobs still tracing (no result yet).
        fn running(&self) -> usize {
            self.jobs.values().filter(|job| !job.finished()).count()
        }

        /// Forgets the oldest finished jobs past [`MAX_FINISHED`], never `keep` (the job being
        /// polled or started) and never a running one, and joins the threads that have returned.
        fn prune(&mut self, keep: Option<u64>) {
            let mut finished: Vec<u64> = self
                .jobs
                .iter()
                .filter(|(id, job)| Some(**id) != keep && job.finished())
                .map(|(id, _)| *id)
                .collect();
            let limit = MAX_FINISHED.saturating_sub(usize::from(keep.is_some()));
            if finished.len() > limit {
                finished.sort_unstable();
                for id in &finished[..finished.len() - limit] {
                    self.jobs.remove(id);
                }
            }
            let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.threads)
                .into_iter()
                .partition(std::thread::JoinHandle::is_finished);
            self.threads = running;
            for handle in done {
                let _ = handle.join();
            }
        }
    }

    impl Drop for ReferenceJobs {
        /// The world is going away (quit): cancels every trace and joins the threads within
        /// [`JOIN_WAIT`], so no trace thread's device clone outlives the app into the process's
        /// exit (the GPU driver can crash tearing down a device another thread still uses).
        fn drop(&mut self) {
            for job in self.jobs.values() {
                job.cancel.store(true, Ordering::Relaxed);
            }
            let deadline = std::time::Instant::now() + JOIN_WAIT;
            for handle in self.threads.drain(..) {
                while !handle.is_finished() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                if handle.is_finished() {
                    let _ = handle.join();
                } else {
                    tracing::warn!(target: "regia_agent", "a reference trace thread is still running at quit");
                }
            }
        }
    }

    pub(super) struct Job {
        spp: u32,
        /// A preview file (`preview.exr`) is written: OIDN, or a preset that post-processes.
        denoise: bool,
        /// The preset's table when the trace runs one (ADR-2433).
        preset: Option<PresetSettings>,
        pub(super) done: AtomicU32,
        pub(super) cancel: AtomicBool,
        out_dir: PathBuf,
        /// What was traced (`scene_info` in every status).
        info: Value,
        result: Mutex<Option<Result<Value, String>>>,
    }

    fn invalid(msg: impl Into<String>) -> RpcError {
        RpcError::invalid_params(msg.into())
    }

    fn u32_param(params: &Value, key: &str, default: u32, max: u32) -> Result<u32, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_u64()
                .filter(|n| *n >= 1 && *n <= u64::from(max))
                .map(|n| n as u32)
                .ok_or_else(|| invalid(format!("`{key}` must be an integer in 1..={max}"))),
        }
    }

    impl Job {
        /// Whether the trace has its result (done, cancelled or failed).
        fn finished(&self) -> bool {
            self.result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        }
    }

    fn status(id: u64, job: &Job) -> Value {
        let result = job
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut out = json!({
            "job": id,
            "spp": job.spp,
            "spp_done": job.done.load(Ordering::Relaxed),
            "out_dir": job.out_dir.display().to_string(),
            "scene_info": job.info,
        });
        match result {
            None => out["status"] = json!("running"),
            Some(Ok(done)) => {
                out["status"] = json!(if job.cancel.load(Ordering::Relaxed) {
                    "cancelled"
                } else {
                    "done"
                });
                if let (Some(o), Some(d)) = (out.as_object_mut(), done.as_object()) {
                    o.extend(d.clone());
                }
            }
            Some(Err(e)) => {
                out["status"] = json!("failed");
                out["error"] = json!(e);
            }
        }
        out
    }

    fn no_frame(message: &str, hint: &str) -> RpcError {
        RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            message.to_string(),
            hint.to_string(),
        )
    }

    /// The renderer's export of the frame it last drew ([`regia_render::FrameExport`]) and the
    /// terrain its shared draw renders. The export is switched on at the first trace and stays
    /// on (it is incremental: mesh data is copied once per key), so later traces see the
    /// host's latest frame. When nothing was exported yet, the extracted frame is drawn once
    /// offscreen to record it.
    fn current_export(
        world: &mut World,
        width: u32,
        height: u32,
        require: bool,
    ) -> Result<
        Option<(
            regia_render::FrameExport,
            Option<regia_render::ReferenceHeightfield>,
        )>,
        RpcError,
    > {
        if !world.contains_resource::<regia_render::runtime::RuntimeRenderer>() {
            return Ok(None);
        }
        world.resource_scope(
            |world, mut runtime: Mut<regia_render::runtime::RuntimeRenderer>| {
                runtime.0.set_scene_export(true);
                if runtime.0.scene_export().is_none() {
                    let drawn = match (
                        world.get_resource::<regia_render::RenderDevice>(),
                        world.get_resource::<regia_render::runtime::ExtractedFrame>(),
                    ) {
                        (Some(device), Some(extracted)) if extracted.is_renderable() => {
                            let view = extracted.window_views().first().map_or(0, |(i, _)| *i);
                            runtime.0.set_outdoor(extracted.outdoor.clone());
                            runtime.0.set_valley_mist(extracted.valley_mist.clone());
                            runtime.0.set_interior_scene(extracted.enclosure.clone());
                            let frame = extracted.frame_scene(view, width, height);
                            runtime.0.render_offscreen(device, &frame).map_err(|e| {
                                RpcError::internal(format!("offscreen render for the export failed: {e}"))
                            })?;
                            true
                        }
                        _ => false,
                    };
                    if !drawn && require {
                        return Err(no_frame(
                            "the renderer has not drawn a frame since the scene export was switched on",
                            "step one frame (play.step) and trace again, or use source: \"extracted\"",
                        ));
                    }
                }
                Ok(runtime
                    .0
                    .scene_export()
                    .cloned()
                    .map(|export| (export, runtime.0.reference_heightfield())))
            },
        )
    }

    /// The `clouds` parameter: which traced cloud field a `current` trace holds (ADR-2127).
    /// `None` is `off`, the default: a frame's reference without clouds, as it has always been.
    fn clouds_param(params: &Value) -> Result<Option<regia_pt::PtCloudPhase>, RpcError> {
        match params.get("clouds") {
            None | Some(Value::Null) => Ok(None),
            Some(v) => match v.as_str() {
                Some("off") => Ok(None),
                Some("droplet") => Ok(Some(regia_pt::PtCloudPhase::DROPLET)),
                Some("raster") => Ok(Some(regia_pt::PtCloudPhase::RASTER_DUAL_LOBE)),
                _ => Err(invalid(format!(
                    "`clouds` must be off, droplet or raster, not {v}"
                ))),
            },
        }
    }

    /// The extraction options for scene `current`: time-reading graphs take the frame's material
    /// clock, position-reading graphs the frame's floating origin, wind-reading graphs the
    /// frame's foliage wind — all as the runtime renderer writes them (ADR-2725) — and
    /// subsurface on request. Without the runtime renderer the origin stays at zero and the
    /// clock and wind stay unset, as an extracted frame alone cannot know them.
    pub(crate) fn current_extract_options(
        world: &World,
        subsurface: bool,
    ) -> regia_pt::extract::ExtractOptions {
        let runtime = world.get_resource::<regia_render::runtime::RuntimeRenderer>();
        regia_pt::extract::ExtractOptions {
            material_clock: runtime.and_then(|r| r.0.material_clock()),
            foliage_wind: runtime.and_then(|r| r.0.foliage_wind()),
            origin_offset: runtime.map_or([0.0; 3], |r| r.0.origin_offset()),
            subsurface,
            ..regia_pt::extract::ExtractOptions::default()
        }
    }

    /// The frame's cloud layer on `scene`, with `phase` (and, for the raster's phase, its albedo).
    fn apply_clouds(
        scene: &mut PtScene,
        outdoor: &regia_render::OutdoorParams,
        phase: Option<regia_pt::PtCloudPhase>,
    ) {
        let Some(phase) = phase else {
            return;
        };
        scene.clouds = regia_pt::extract::clouds_from_outdoor(outdoor).map(|field| regia_pt::PtCloudField {
            phase,
            albedo: if phase == regia_pt::PtCloudPhase::RASTER_DUAL_LOBE {
                regia_render::CLOUD_MARCH_ALBEDO
            } else {
                field.albedo
            },
            ..field
        });
    }

    /// Adds the open mouths' bands and collars of every terrain with mesh layers to the export,
    /// beside the reference heightfield (spec 26 § 2a.6 "Path tracer", ADR-8537). Without layers
    /// the export is unchanged.
    fn add_mesh_layer_pieces(world: &mut World, export: &mut regia_render::FrameExport) -> usize {
        let offset = world
            .get_resource::<regia_scene::streaming::OriginOffset>()
            .map_or([0.0; 3], |o| o.0.map(|axis| axis as f32));
        let mut layers =
            world.query_filtered::<&regia_terrain::TerrainMeshLayers, With<regia_terrain::Terrain>>();
        layers
            .iter(world)
            .map(|l| l.add_reference_pieces(export, offset))
            .sum()
    }

    fn scene_for(
        world: &mut World,
        name: &str,
        source: &str,
        width: u32,
        height: u32,
        clouds: Option<regia_pt::PtCloudPhase>,
        subsurface: bool,
    ) -> Result<(PtScene, Value), RpcError> {
        if clouds.is_some() && name != "current" {
            return Err(invalid(
                "`clouds` traces the frame's cloud layer: it needs scene current",
            ));
        }
        let named = |scene: PtScene| (scene, json!({ "source": "named" }));
        match name {
            "cornell" => Ok(named(regia_pt::scenes::cornell_box(width, height, true))),
            "cornell_room" => Ok(named(regia_pt::scenes::cornell_box(width, height, false))),
            "furnace" => Ok(named(regia_pt::scenes::furnace_sphere(
                regia_pt::PtMaterial::lambert([1.0; 3]),
                width,
            ))),
            "current" => {
                if !matches!(source, "auto" | "export" | "extracted") {
                    return Err(invalid(format!(
                        "unknown source `{source}`; use auto, export or extracted"
                    )));
                }
                let options = current_extract_options(world, subsurface);
                if source != "extracted"
                    && let Some((mut export, terrain)) =
                        current_export(world, width, height, source == "export")?
                {
                    add_mesh_layer_pieces(world, &mut export);
                    // The drawn layer chunks trace at LOD 0 (ADR-8872).
                    let chunks = regia_terrain::reference_chunks_lod0(world, &mut export);
                    let mut scene = regia_pt::extract::from_export_with(&export, terrain.as_ref(), &options);
                    apply_clouds(&mut scene, &export.outdoor, clouds);
                    let mut info = scene_info("export", &scene, Some(export.frame_index), &options);
                    if chunks != regia_terrain::ReferenceChunks::default()
                        && let Some(map) = info.as_object_mut()
                    {
                        map.insert(
                            "mesh_layer_chunks".into(),
                            json!({
                                "lod0": chunks.swapped + chunks.at_lod0,
                                "swapped": chunks.swapped,
                                "failed": chunks.failed,
                            }),
                        );
                    }
                    return Ok((scene, info));
                }
                if source == "export" {
                    return Err(no_frame(
                        "no renderer to export the frame from",
                        "add regia_render::RenderPlugin, or use source: \"extracted\"",
                    ));
                }
                let Some(extracted) = world.get_resource::<regia_render::runtime::ExtractedFrame>() else {
                    return Err(no_frame(
                        "no extracted frame to trace",
                        "add regia_render::RenderPlugin and spawn a Camera; or trace a named scene (cornell, furnace)",
                    ));
                };
                if !extracted.is_renderable() {
                    return Err(no_frame(
                        "nothing to trace",
                        "spawn a Camera and a MeshRenderer, then step one frame",
                    ));
                }
                let view = extracted.window_views().first().map_or(0, |(i, _)| *i);
                let mut scene =
                    regia_pt::extract::from_extracted_with(extracted, view, width, height, &options);
                apply_clouds(&mut scene, &extracted.outdoor, clouds);
                let info = scene_info("extracted", &scene, None, &options);
                Ok((scene, info))
            }
            other => Err(invalid(format!(
                "unknown scene `{other}`; use current, cornell, cornell_room or furnace"
            ))),
        }
    }

    /// What the trace contains, for the caller to check before waiting on it. `options` is what
    /// the extraction took from the runtime renderer (or defaults), so a mismatch a comparator
    /// sees (a floating origin, a foliage wind) is named here.
    fn scene_info(
        source: &str,
        scene: &PtScene,
        frame: Option<u64>,
        options: &regia_pt::extract::ExtractOptions,
    ) -> Value {
        json!({
            "source": source,
            "frame_index": frame,
            "instances": scene.instances.len(),
            "meshes": scene.meshes.len(),
            "triangles": scene.meshes.iter().map(regia_pt::PtMesh::triangle_count).sum::<usize>(),
            "materials": scene.materials.len(),
            "terrain": scene.terrain.as_ref().map(|t| json!([t.width, t.depth])),
            "sun_lux": scene.sun.map(|s| s.illuminance),
            "sky": match scene.sky {
                regia_pt::PtSky::None => "none",
                regia_pt::PtSky::Uniform(_) => "uniform",
                regia_pt::PtSky::Table { .. } => "atmosphere_table",
                regia_pt::PtSky::Equirect { .. } => "equirect",
            },
            "sky_bodies": scene.sky_bodies.len(),
            "stars": scene.stars > 0.0,
            "local_lights": scene.lights.len(),
            "surface_graphs": scene.surface_graphs.len(),
            "subsurface_materials": scene.subsurface.len(),
            "brdf_energy": scene.brdf_energy.name(),
            "material_clock": options.material_clock.map(|(now, prev)| json!([now, prev])),
            "origin_offset": options.origin_offset,
            "foliage_wind": options.foliage_wind,
            "clouds": scene.clouds.as_ref().map(|c| json!({
                "coverage": c.settings.coverage,
                "base_m": c.settings.altitude_bottom,
                "top_m": c.settings.altitude_top,
                "albedo": c.albedo,
                "forward_g": c.phase.forward_g,
            })),
        })
    }

    fn run_job(
        device: regia_render::RenderDevice,
        scene: PtScene,
        settings: PtSettings,
        aovs: Vec<Aov>,
        ev100: f32,
        job: Arc<Job>,
        cache: Arc<Mutex<PtCache>>,
    ) -> Result<Value, String> {
        std::fs::create_dir_all(&job.out_dir).map_err(|e| format!("{}: {e}", job.out_dir.display()))?;
        let started = std::time::Instant::now();
        // Cancel (a poll's `cancel: true`, or quit) is checked between phases too, so a thread
        // cancelled before sampling returns instead of building a BVH nobody waits for.
        let cancelled = || job.cancel.load(Ordering::Relaxed);
        if cancelled() {
            return Err("cancelled before the trace started".into());
        }
        let tracer = PathTracer::new(&device).map_err(|e| e.to_string())?;
        if cancelled() {
            return Err("cancelled before the trace started".into());
        }
        let quality = job.preset.map(|p| p.quality).unwrap_or_default();
        let mut session = {
            // A trace thread that panicked while holding the cache left it half-updated: start
            // from an empty cache instead of failing every later trace.
            let mut cache = match cache.lock() {
                Ok(cache) => cache,
                Err(poisoned) => {
                    let mut cache = poisoned.into_inner();
                    *cache = PtCache::default();
                    cache
                }
            };
            tracer
                .begin_cached_with_quality(&scene, &settings, &mut cache, &quality)
                .map_err(|e| e.to_string())?
        };
        if cancelled() {
            return Err("cancelled before sampling".into());
        }
        let mut preset_report = None;
        let mut preset_raw = None;
        let image = if let Some(table) = &job.preset {
            let result = regia_pt::run_preset(&mut session, table, &mut |done| {
                job.done.store(done, Ordering::Relaxed);
                !job.cancel.load(Ordering::Relaxed)
            })
            .map_err(|e| e.to_string())?;
            preset_report = serde_json::to_value(&result.report).ok();
            preset_raw = result.raw;
            result.image
        } else {
            session
                .run_with(settings.spp, |done| {
                    job.done.store(done, Ordering::Relaxed);
                    !job.cancel.load(Ordering::Relaxed)
                })
                .map_err(|e| e.to_string())?;
            session.image().map_err(|e| e.to_string())?
        };
        // A preset that post-processed keeps its raw accumulation as `reference.*` and the
        // preview as `preview.*`; one that did not writes `reference.*` only.
        let (image, preview_of) = match preset_raw {
            Some(raw) => (raw, Some(image)),
            None => (image, None),
        };
        let exr = job.out_dir.join("reference.exr");
        let png = job.out_dir.join("reference_preview.png");
        let meta = job.out_dir.join("reference.json");
        image.write_exr(&exr, &aovs).map_err(|e| e.to_string())?;
        image
            .write_preview_png(&png, Aov::Beauty, ev100)
            .map_err(|e| e.to_string())?;
        image.write_metadata_json(&meta).map_err(|e| e.to_string())?;
        let raw = json!({
            "exr": exr.display().to_string(),
            "preview_png": png.display().to_string(),
            "metadata_json": meta.display().to_string(),
        });
        let (image, mut out) = if job.denoise {
            let preview = match preview_of {
                Some(p) => p,
                None => denoised(&image)?,
            };
            let exr = job.out_dir.join("preview.exr");
            let png = job.out_dir.join("preview.png");
            let meta = job.out_dir.join("preview.json");
            preview.write_exr(&exr, &aovs).map_err(|e| e.to_string())?;
            preview
                .write_preview_png(&png, Aov::Beauty, ev100)
                .map_err(|e| e.to_string())?;
            preview.write_metadata_json(&meta).map_err(|e| e.to_string())?;
            let out = json!({
                "exr": exr.display().to_string(),
                "preview_png": png.display().to_string(),
                "metadata_json": meta.display().to_string(),
                "denoised": true,
                "raw": raw,
            });
            (preview, out)
        } else {
            (image, raw)
        };
        let rgba = image.preview_rgba8(Aov::Beauty, ev100);
        let captured = regia_render::CapturedImage {
            width: image.width,
            height: image.height,
            rgba,
        };
        let b64 = crate::image::png_base64(&captured)?;
        if let Some(o) = out.as_object_mut() {
            o.insert(
                "metadata".into(),
                serde_json::to_value(&image.metadata).unwrap_or(Value::Null),
            );
            o.insert("width".into(), json!(image.width));
            o.insert("height".into(), json!(image.height));
            o.insert("total_seconds".into(), json!(started.elapsed().as_secs_f64()));
            o.insert("mime".into(), json!("image/png"));
            o.insert("png_base64".into(), json!(b64));
            if let Some(report) = preset_report {
                o.insert("preset".into(), report);
            }
        }
        Ok(out)
    }

    #[cfg(feature = "denoise")]
    fn denoised(image: &regia_pt::PtImage) -> Result<regia_pt::PtImage, String> {
        regia_pt::denoise::denoise(image, &regia_pt::denoise::DenoiseOptions::default())
            .map_err(|e| e.to_string())
    }

    #[cfg(not(feature = "denoise"))]
    fn denoised(_image: &regia_pt::PtImage) -> Result<regia_pt::PtImage, String> {
        // Refused before the job starts (`reference_trace`); kept for completeness.
        Err("this build has no denoiser".into())
    }

    /// Longest a `wait: true` call blocks the world thread. It stays under the socket's 30 s
    /// reply timeout; a longer trace answers `running` and the caller polls with `job`.
    const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

    /// Blocks until the job has a result or `limit` has passed.
    fn wait_for(job: &Job, limit: std::time::Duration) {
        let deadline = std::time::Instant::now() + limit;
        while job.result.lock().map(|r| r.is_none()).unwrap_or(false) && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn spp_given(params: &Value) -> bool {
        !matches!(params.get("spp"), None | Some(Value::Null))
    }

    fn max_bounces_given(params: &Value) -> bool {
        !matches!(params.get("max_bounces"), None | Some(Value::Null))
    }

    fn f32_param(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0 && *n <= f64::from(f32::MAX))
                .map(|n| Some(n as f32))
                .ok_or_else(|| invalid(format!("`{key}` must be a finite number, 0 or more"))),
        }
    }

    fn name_param<T>(
        params: &Value,
        key: &str,
        parse: impl Fn(&str) -> Option<T>,
        names: &str,
    ) -> Result<Option<T>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => parse(s)
                .map(Some)
                .ok_or_else(|| invalid(format!("unknown `{key}` {s:?}: {names}"))),
            Some(_) => Err(invalid(format!("`{key}` must be a string ({names})"))),
        }
    }

    /// The preset table the request asks for, with its overrides applied and checked.
    /// `spp`/`max_bounces` are the request's values (or defaults); the preset's own numbers win
    /// unless the request gave them, and `reference` always uses the request's.
    fn preset_param(
        params: &Value,
        spp_given: bool,
        bounces_given: bool,
        spp: u32,
        max_bounces: u32,
        seed: u32,
    ) -> Result<Option<PresetSettings>, RpcError> {
        let preset = name_param(
            params,
            "preset",
            PtPreset::from_name,
            "draft, preview, final or reference",
        )?;
        let caustics = name_param(
            params,
            "caustics",
            regia_pt::CausticsMode::from_name,
            "suppress, clamped or full",
        )?;
        let denoiser = name_param(
            params,
            "denoiser",
            regia_pt::DenoiserKind::from_name,
            "none, builtin or oidn",
        )?;
        let clamp = f32_param(params, "clamp_indirect")?;
        let firefly = f32_param(params, "firefly_threshold")?;
        let adaptive = match params.get("adaptive") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(invalid("`adaptive` must be a boolean")),
        };
        let Some(preset) = preset else {
            if caustics.is_some()
                || denoiser.is_some()
                || clamp.is_some()
                || firefly.is_some()
                || adaptive.is_some()
            {
                return Err(invalid(
                    "`caustics`, `clamp_indirect`, `firefly_threshold`, `adaptive` and `denoiser` need a `preset`",
                ));
            }
            return Ok(None);
        };
        let base = PtSettings {
            spp,
            max_bounces,
            seed,
            ..PtSettings::default()
        };
        let mut table = preset.settings_for(&base);
        if spp_given {
            table.spp = spp;
        }
        if bounces_given {
            table.max_bounces = max_bounces;
        }
        if let Some(c) = caustics {
            table.quality.caustics = c;
        }
        if let Some(v) = clamp {
            table.quality.clamp_indirect = v;
        }
        if let Some(v) = firefly {
            table.firefly_threshold = v;
        }
        if let Some(a) = adaptive {
            table.adaptive.enabled = a;
        }
        if let Some(d) = denoiser {
            table.denoiser = d;
        }
        if table.spp < 2 {
            // One sample has no variance to judge.
            table.adaptive.enabled = false;
        }
        if table.adaptive.enabled {
            table.adaptive.min_spp = table.adaptive.min_spp.min(table.spp);
        }
        if table.denoiser == regia_pt::DenoiserKind::Oidn && !cfg!(feature = "denoise") {
            return Err(RpcError::new(
                protocol::UNSUPPORTED,
                "F0013",
                "this build has no OIDN denoiser",
                "use `denoiser: \"builtin\"` (no native library) or build the host with regia_agent's `denoise` feature",
            ));
        }
        table.validate().map_err(|e| invalid(e.to_string()))?;
        Ok(Some(table))
    }

    #[cfg(test)]
    mod job_table_tests {
        use super::*;

        fn job(result: Option<Result<Value, String>>) -> Arc<Job> {
            Arc::new(Job {
                spp: 1,
                denoise: false,
                // The field ADR-2433 added; the helper never got it (lib tests did not compile).
                preset: None,
                done: AtomicU32::new(0),
                cancel: AtomicBool::new(false),
                out_dir: PathBuf::new(),
                info: Value::Null,
                result: Mutex::new(result),
            })
        }

        /// Every call used to start another GPU trace thread with no cap (audit-concurrency-1103
        /// agent H3): with traces already running, a new one is refused with `too_many_pending`
        /// before any device, scene or thread is made.
        #[test]
        fn a_new_trace_is_refused_while_traces_are_running() {
            let mut world = World::new();
            let mut jobs = ReferenceJobs::default();
            for id in 1..=8 {
                jobs.jobs.insert(id, job(None));
            }
            jobs.next = 8;
            world.insert_resource(jobs);
            let error = reference_trace(&mut world, &json!({ "width": 8, "height": 8, "spp": 1 }))
                .expect_err("refused while traces run");
            assert_eq!(
                error.data.as_ref().and_then(|d| d["code"].as_str()),
                Some("too_many_pending"),
                "{error:?}"
            );
            assert_eq!(
                world.resource::<ReferenceJobs>().jobs.len(),
                8,
                "no job was added"
            );
        }

        /// Finished jobs (each holding its result, base64 PNG included) used to stay in the table
        /// for the rest of the process (audit-concurrency-1103 agent H3): the oldest finished
        /// ones are evicted, never a running one or the one being polled.
        #[test]
        fn finished_jobs_are_evicted_oldest_first() {
            let mut world = World::new();
            let mut jobs = ReferenceJobs::default();
            jobs.jobs.insert(1, job(None)); // still running: never evicted
            for id in 2..=60 {
                jobs.jobs
                    .insert(id, job(Some(Ok(json!({ "png_base64": "x".repeat(1024) })))));
            }
            jobs.next = 60;
            world.insert_resource(jobs);
            let status = reference_trace(&mut world, &json!({ "job": 2 })).expect("polled");
            assert_eq!(status["status"], "done");
            let jobs = &world.resource::<ReferenceJobs>().jobs;
            assert!(jobs.len() <= 17, "finished jobs are bounded: {} kept", jobs.len());
            assert!(jobs.contains_key(&1), "a running job is kept");
            assert!(jobs.contains_key(&2), "the job just polled is kept");
            assert!(jobs.contains_key(&60), "the newest finished job is kept");
            assert!(!jobs.contains_key(&3), "the oldest finished job went first");
        }
    }

    /// `seeing.reference_trace`.
    pub(crate) fn reference_trace(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        if world.get_resource::<ReferenceJobs>().is_none() {
            world.insert_resource(ReferenceJobs::default());
        }
        if let Some(id) = params.get("job") {
            let id = id.as_u64().ok_or_else(|| invalid("`job` must be a job id"))?;
            let job = world
                .resource::<ReferenceJobs>()
                .jobs
                .get(&id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::not_found(format!("no trace job {id}"), "start one without `job`")
                })?;
            if params.get("cancel").and_then(Value::as_bool) == Some(true) {
                job.cancel.store(true, Ordering::Relaxed);
            }
            if params.get("wait").and_then(Value::as_bool) == Some(true) {
                wait_for(&job, WAIT_LIMIT);
            }
            world.resource_mut::<ReferenceJobs>().prune(Some(id));
            return Ok(status(id, &job));
        }
        if world.resource::<ReferenceJobs>().running() >= MAX_RUNNING {
            return Err(crate::seeing::too_many_pending(
                "seeing.reference_trace",
                MAX_RUNNING,
            ));
        }
        let width = u32_param(params, "width", 256, 4096)?;
        let height = u32_param(params, "height", 144, 4096)?;
        let spp = u32_param(params, "spp", 64, 1 << 20)?;
        let max_bounces = u32_param(params, "max_bounces", 12, regia_pt::MAX_PATH_VERTICES)?;
        let clouds = clouds_param(params)?;
        let seed = match params.get("seed") {
            None | Some(Value::Null) => 1,
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| invalid("`seed` must be an integer in 0..=4294967295"))?,
        };
        let scene_name = params.get("scene").and_then(Value::as_str).unwrap_or("current");
        let source = params.get("source").and_then(Value::as_str).unwrap_or("auto");
        let denoise = match params.get("denoise") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(invalid("`denoise` must be a boolean")),
        };
        if denoise && !cfg!(feature = "denoise") {
            return Err(RpcError::new(
                protocol::UNSUPPORTED,
                "F0013",
                "this build has no denoiser",
                "build the host with regia_agent's `denoise` feature (links Intel OIDN; desktop tools only), \
                 or trace without `denoise`",
            ));
        }
        let preset = preset_param(
            params,
            spp_given(params),
            max_bounces_given(params),
            spp,
            max_bounces,
            seed,
        )?;
        if preset.is_some() && denoise {
            return Err(invalid(
                "`denoise` (the OIDN extra preview) cannot be combined with `preset`: use `denoiser: \"oidn\"`",
            ));
        }
        let aovs = match params.get("aovs") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(list)) => list
                .iter()
                .map(|v| {
                    v.as_str()
                        .and_then(Aov::from_name)
                        .ok_or_else(|| invalid(format!("unknown AOV {v}; see specs/27 § 1.2")))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(invalid("`aovs` must be an array of AOV names")),
        };
        // Confine `out_dir` before anything is created or started (same roots as `render.out_start`).
        let requested_out_dir = match params.get("out_dir").and_then(Value::as_str) {
            Some(dir) => Some(crate::render_out::confine_path(world, "out_dir", Path::new(dir))?),
            None => None,
        };
        // The device first: a `current` trace may draw the frame once to export it.
        if world.get_resource::<regia_render::RenderDevice>().is_none() {
            // A host that opened no GPU on purpose keeps it that way (ADR-5180).
            crate::seeing::GpuDisabled::refuse(world)?;
            let device = crate::seeing::headless_render_device().map_err(|e| {
                RpcError::new(
                    protocol::UNSUPPORTED,
                    "F0011",
                    format!("no GPU for the reference trace: {e}"),
                    "install lavapipe (Vulkan software) or a GPU driver",
                )
            })?;
            world.insert_resource(device);
        }
        // ADR-2725: trace each material's subsurface scattering (Burley); off by default.
        let subsurface = match params.get("subsurface") {
            None | Some(Value::Null) => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| invalid("`subsurface` must be true or false"))?,
        };
        if subsurface && scene_name != "current" {
            return Err(invalid(
                "`subsurface` traces the frame's materials: it needs scene current",
            ));
        }
        let (mut scene, info) = scene_for(world, scene_name, source, width, height, clouds, subsurface)?;
        scene.camera.width = width;
        scene.camera.height = height;
        let ev100 = params
            .get("ev100")
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .unwrap_or_else(|| default_ev100(&scene));
        let device = world.resource::<regia_render::RenderDevice>().clone();
        let mut jobs = world.resource_mut::<ReferenceJobs>();
        jobs.next += 1;
        let cache = Arc::clone(&jobs.cache);
        let id = jobs.next;
        let out_dir = match requested_out_dir {
            Some(dir) => dir,
            None => std::env::temp_dir()
                .join("regia_pt")
                .join(format!("job-{}-{id}", std::process::id())),
        };
        let (spp, denoise) = match &preset {
            Some(t) => (
                t.spp,
                t.denoiser != regia_pt::DenoiserKind::None
                    || t.firefly_threshold > 0.0
                    || !t.quality.is_base(),
            ),
            None => (spp, denoise),
        };
        let job = Arc::new(Job {
            spp,
            denoise,
            preset,
            done: AtomicU32::new(0),
            cancel: AtomicBool::new(false),
            out_dir,
            info,
            result: Mutex::new(None),
        });
        jobs.jobs.insert(id, job.clone());
        let settings = match &job.preset {
            Some(t) => PtSettings {
                spp: t.spp,
                max_bounces: t.max_bounces,
                rr_start: t.rr_start,
                seed,
                ..PtSettings::default()
            },
            None => PtSettings {
                spp,
                max_bounces,
                seed,
                ..PtSettings::default()
            },
        };
        let worker = job.clone();
        // Forget old finished jobs and join the threads of finished traces as new ones start.
        jobs.prune(Some(id));
        let handle = std::thread::Builder::new()
            .name(format!("regia_pt job {id}"))
            .spawn(move || {
                // A panic still ends the job (`failed`), so pollers never see `running` for ever.
                let traced = worker.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    run_job(device, scene, settings, aovs, ev100, traced, cache)
                }))
                .unwrap_or_else(|_| Err("the trace thread panicked".to_string()));
                *worker
                    .result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            })
            .map_err(|e| RpcError::internal(format!("could not start the trace thread: {e}")))?;
        jobs.threads.push(handle);
        if params.get("wait").and_then(Value::as_bool) == Some(true) {
            wait_for(&job, WAIT_LIMIT);
        }
        Ok(status(id, &job))
    }

    /// Preview exposure when none is given: the scene's own sun-lit or lamp-lit level.
    fn default_ev100(scene: &PtScene) -> f32 {
        match scene.sun {
            Some(sun) if sun.illuminance > 1000.0 => {
                regia_render::ev100_from_luminance(sun.illuminance * 0.18 / std::f32::consts::PI)
            }
            _ => 0.0,
        }
    }

    fn resolve(world: &World, v: &Value, key: &str) -> Result<PathBuf, RpcError> {
        match v {
            Value::String(s) => crate::render_out::confine_path(world, key, Path::new(s)),
            Value::Object(o) => {
                let id = o
                    .get("job")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid(format!("`{key}` must be a path or {{\"job\": id}}")))?;
                let job = world
                    .get_resource::<ReferenceJobs>()
                    .and_then(|j| j.jobs.get(&id).cloned())
                    .ok_or_else(|| {
                        RpcError::not_found(
                            format!("no trace job {id}"),
                            "start one with seeing.reference_trace",
                        )
                    })?;
                let done = job
                    .result
                    .lock()
                    .map(|r| matches!(*r, Some(Ok(_))))
                    .unwrap_or(false);
                if !done {
                    return Err(invalid(format!("trace job {id} has not finished")));
                }
                // A denoised job resolves to its preview, which `compare` refuses as `a`.
                Ok(job.out_dir.join(if job.denoise {
                    "preview.exr"
                } else {
                    "reference.exr"
                }))
            }
            _ => Err(invalid(format!("`{key}` must be a path or {{\"job\": id}}"))),
        }
    }

    /// `seeing.compare`.
    pub(crate) fn compare(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        let a = resolve(world, params.get("a").unwrap_or(&Value::Null), "a")?;
        let b = resolve(world, params.get("b").unwrap_or(&Value::Null), "b")?;
        let aov = params.get("aov").and_then(Value::as_str).unwrap_or("beauty");
        let ev100 = params.get("ev100").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let ppd = params
            .get("ppd")
            .and_then(Value::as_f64)
            .map_or(regia_render::compare::DEFAULT_PPD, |v| v as f32);
        let mode = params.get("mode").and_then(Value::as_str).unwrap_or("all");
        if !matches!(mode, "flip" | "rmse" | "ssim" | "all") {
            return Err(invalid("`mode` must be flip, rmse, ssim or all"));
        }
        let map = match params.get("map_png").and_then(Value::as_str) {
            Some(path) => crate::render_out::confine_path(world, "map_png", Path::new(path))?,
            None => b.with_extension("flip.png"),
        };
        let ra = regia_pt::compare::load(&a, aov).map_err(|e| invalid(e.to_string()))?;
        let rb = regia_pt::compare::load(&b, aov).map_err(|e| invalid(e.to_string()))?;
        let (report, flip) =
            regia_pt::compare::compare_images(&ra, &rb, ev100, ppd).map_err(|e| match e {
                regia_pt::PtError::DenoisedReference(_) => RpcError::new(
                    protocol::INVALID_PARAMS,
                    "F0002",
                    format!("`a` ({}) is denoised: {e}", a.display()),
                    "pass a raw trace as `a` (seeing.reference_trace without `denoise`, or its `raw.exr`); \
                 a denoised preview may only be `b`",
                ),
                other => invalid(other.to_string()),
            })?;
        let heat = regia_render::compare::flip_heatmap_rgba8(&flip.map);
        write_map(&map, report.width, report.height, &heat)?;
        let mut out: Value =
            serde_json::from_str(&report.to_json()).map_err(|e| RpcError::internal(e.to_string()))?;
        // ADR-2315: PSNR on the display-encoded pair (peak 1), null when the images are equal.
        let psnr = regia_pt::compare::psnr_db(&ra, &rb, ev100);
        if let Some(o) = out.as_object_mut() {
            o.insert("psnr_db".into(), json!(psnr));
            match mode {
                "flip" => o.retain(|k, _| k.starts_with("flip") || k == "width" || k == "height"),
                "rmse" => o.retain(|k, _| {
                    k.contains("rmse")
                        || k.starts_with("mean")
                        || k == "psnr_db"
                        || k == "width"
                        || k == "height"
                }),
                "ssim" => o.retain(|k, _| k == "ssim" || k == "width" || k == "height"),
                _ => {}
            }
            o.insert("aov".into(), json!(aov));
            o.insert("flip_map_png".into(), json!(map.display().to_string()));
            let captured = regia_render::CapturedImage {
                width: report.width,
                height: report.height,
                rgba: heat,
            };
            o.insert("mime".into(), json!("image/png"));
            o.insert(
                "png_base64".into(),
                json!(crate::image::png_base64(&captured).map_err(RpcError::internal)?),
            );
        }
        Ok(out)
    }

    fn write_map(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), RpcError> {
        regia_pt::output::write_png(path, width, height, rgba).map_err(|e| RpcError::internal(e.to_string()))
    }

    #[cfg(test)]
    mod wait_tests {
        use super::*;

        #[test]
        fn wait_returns_at_the_limit_for_an_unfinished_job() {
            let job = Job {
                spp: 1,
                denoise: false,
                preset: None,
                done: AtomicU32::new(0),
                cancel: AtomicBool::new(false),
                out_dir: PathBuf::new(),
                info: Value::Null,
                result: Mutex::new(None),
            };
            // A zero limit must return at once instead of waiting for a result that never comes.
            wait_for(&job, std::time::Duration::ZERO);
            assert!(job.result.lock().expect("lock").is_none());
        }
    }
}

#[cfg(all(test, feature = "reference-trace"))]
mod tests {
    use super::*;
    use serde_json::json;

    /// `current_extract_options` carries the runtime renderer's floating origin, foliage wind
    /// and material clock into the extraction (ADR-2725); without a renderer it defaults
    /// (origin zero, clock and wind unset) instead of inventing values.
    #[test]
    fn current_extract_options_takes_the_renderer_origin_wind_and_clock() {
        let world = World::new();
        let options = enabled::current_extract_options(&world, false);
        assert_eq!(options.origin_offset, [0.0; 3]);
        assert_eq!(options.foliage_wind, None);
        assert_eq!(options.material_clock, None);
        assert!(!options.subsurface);

        let mut world = World::new();
        let mut runtime = regia_render::runtime::RuntimeRenderer(regia_render::FrameRenderer::new());
        runtime.0.set_origin_offset([120.0, 0.0, -45.0]);
        runtime.0.set_foliage_wind(Some([1.5, -0.25]));
        runtime.0.set_material_clock(2.0, 1.5);
        world.insert_resource(runtime);
        let options = enabled::current_extract_options(&world, true);
        assert_eq!(options.origin_offset, [120.0, 0.0, -45.0]);
        assert_eq!(options.foliage_wind, Some([1.5, -0.25]));
        assert_eq!(options.material_clock, Some((2.0, 1.5)));
        assert!(options.subsurface);
    }

    /// `clouds` (ADR-2127): the three names, and only a `current` trace has a frame's layer to use.
    #[test]
    fn the_clouds_parameter_is_checked() {
        let mut world = World::new();
        let bad = reference_trace(&mut world, &json!({ "scene": "cornell", "clouds": "nope" }));
        assert!(
            bad.is_err_and(|e| format!("{e:?}").contains("off, droplet or raster")),
            "an unknown name is refused"
        );
        let number = reference_trace(&mut world, &json!({ "scene": "cornell", "clouds": 1 }));
        assert!(number.is_err(), "a number is refused");
        let named = reference_trace(&mut world, &json!({ "scene": "cornell", "clouds": "droplet" }));
        assert!(
            named.is_err_and(|e| format!("{e:?}").contains("scene current")),
            "a named scene has no cloud layer"
        );
    }

    /// Quit mid-trace: dropping the world cancels the running trace and joins its thread
    /// (holding a device clone) instead of leaving it to outlive the app.
    #[test]
    fn dropping_the_world_cancels_and_joins_a_running_trace() {
        let mut world = World::new();
        let dir = std::env::temp_dir().join(format!("regia_agent_pt_quit_{}", std::process::id()));
        let started = reference_trace(
            &mut world,
            &json!({ "scene": "cornell", "width": 64, "height": 64, "spp": 100000, "max_bounces": 4,
                     "out_dir": dir.display().to_string() }),
        )
        .expect("trace");
        let job = started["job"].as_u64().expect("job id");
        let job = std::sync::Arc::clone(&world.resource::<enabled::ReferenceJobs>().jobs[&job]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while job.done.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the trace never started sampling"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let t0 = std::time::Instant::now();
        drop(world);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "the trace was not canceled at quit ({:?})",
            t0.elapsed()
        );
        assert!(
            job.cancel.load(std::sync::atomic::Ordering::Relaxed),
            "the trace was canceled"
        );
        assert_eq!(
            std::sync::Arc::strong_count(&job),
            1,
            "the trace thread returned and was joined"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `out_dir`, `a`, `b` and `map_png` are confined like `render.out_start`'s `out`: `..` and
    /// paths outside every allowed root are refused before any file is touched or trace started.
    #[test]
    fn paths_outside_the_allowed_roots_are_refused() {
        let mut world = World::new();
        let outside = std::env::temp_dir().join("..").join("regia_escape");
        let outside = outside.display().to_string();
        let err = reference_trace(&mut world, &json!({ "scene": "cornell", "out_dir": outside }))
            .expect_err("`..` in out_dir");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert!(err.message.contains("`out_dir`"), "{}", err.message);
        assert!(
            !err.message.contains("regia_escape"),
            "no path echo: {}",
            err.message
        );
        // `reference_trace` holds the (empty) job table before it reads any parameter.
        assert!(
            world
                .get_resource::<enabled::ReferenceJobs>()
                .is_none_or(|jobs| jobs.jobs.is_empty()),
            "no job was started"
        );
        for params in [
            json!({ "a": "../secret.exr", "b": "x.exr" }),
            json!({ "a": "x.exr", "b": "../secret.exr" }),
            json!({ "a": "x.exr", "b": "y.exr", "map_png": "../map.png" }),
        ] {
            let err = compare(&mut world, &params).expect_err("confined");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{params}");
        }
    }

    #[test]
    fn both_methods_need_edit_scene() {
        for name in ["seeing.reference_trace", "seeing.compare"] {
            let method = crate::schema::method(name).expect(name);
            assert!(method.mutating, "{name}");
            assert_eq!(method.permission, crate::schema::Permission::EditScene, "{name}");
        }
    }

    #[test]
    fn traces_a_named_scene_and_compares_it_with_itself() {
        let mut world = World::new();
        let dir = std::env::temp_dir().join(format!("regia_agent_pt_{}", std::process::id()));
        let started = reference_trace(
            &mut world,
            &json!({ "scene": "cornell", "width": 32, "height": 32, "spp": 8, "max_bounces": 4,
                     "out_dir": dir.display().to_string(), "wait": true }),
        )
        .expect("trace");
        assert_eq!(started["status"], "done", "{started}");
        assert_eq!(started["spp_done"], 8);
        let exr = started["exr"].as_str().expect("exr path").to_string();
        assert!(std::path::Path::new(&exr).exists());
        assert!(started["png_base64"].as_str().is_some_and(|s| !s.is_empty()));
        let job = started["job"].as_u64().expect("job id");
        let polled = reference_trace(&mut world, &json!({ "job": job })).expect("poll");
        assert_eq!(polled["status"], "done");
        let cmp = compare(
            &mut world,
            &json!({ "a": { "job": job }, "b": exr, "aov": "beauty" }),
        )
        .expect("compare");
        assert!(cmp["flip_mean"].as_f64().is_some_and(|f| f < 1e-6), "{cmp}");
        assert!(cmp["flip_map_png"].as_str().is_some());
        let bad = reference_trace(&mut world, &json!({ "scene": "nope" }));
        assert!(bad.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2433: `preset` runs a named table; a preview is refused as the reference, `reference`
    /// is the plain trace, and bad values are invalid params.
    #[test]
    fn presets_run_and_previews_are_never_references() {
        let mut world = World::new();
        let dir = std::env::temp_dir().join(format!("regia_agent_pt_presets_{}", std::process::id()));
        let preview = reference_trace(
            &mut world,
            &json!({ "scene": "cornell", "width": 24, "height": 24, "spp": 8, "max_bounces": 3,
                     "preset": "draft", "out_dir": dir.join("draft").display().to_string(), "wait": true }),
        )
        .expect("draft trace");
        assert_eq!(preview["status"], "done", "{preview}");
        assert_eq!(preview["denoised"], true, "{preview}");
        assert_eq!(preview["preset"]["preset"], "draft", "{preview}");
        assert_eq!(preview["preset"]["denoiser"], "builtin");
        assert!(
            preview["metadata"]["denoiser"]
                .as_str()
                .is_some_and(|d| d.starts_with("regia a-trous")),
            "{preview}"
        );
        let preview_exr = preview["exr"].as_str().expect("exr").to_string();
        assert!(preview_exr.ends_with("preview.exr"), "{preview_exr}");
        let job = preview["job"].as_u64().expect("job");
        let err = compare(&mut world, &json!({ "a": { "job": job }, "b": preview_exr }))
            .expect_err("preview as reference");
        assert!(format!("{err:?}").contains("denoised"), "{err:?}");
        // The raw trace next to it is a fine reference.
        let raw = preview["raw"]["exr"].as_str().expect("raw exr").to_string();
        compare(&mut world, &json!({ "a": raw, "b": preview_exr })).expect("raw as reference");

        // `reference` is the plain trace: no preview file, no mark.
        let plain = reference_trace(
            &mut world,
            &json!({ "scene": "cornell", "width": 16, "height": 16, "spp": 4, "max_bounces": 3,
                     "preset": "reference", "out_dir": dir.join("ref").display().to_string(), "wait": true }),
        )
        .expect("reference preset");
        assert_eq!(plain["status"], "done", "{plain}");
        assert_eq!(plain["spp_done"], 4);
        assert!(
            plain["exr"]
                .as_str()
                .is_some_and(|p| p.ends_with("reference.exr")),
            "{plain}"
        );
        assert_eq!(plain["preset"]["base"], true);

        for bad in [
            json!({ "preset": "ultra" }),
            json!({ "preset": "draft", "caustics": "lots" }),
            json!({ "preset": "draft", "clamp_indirect": -1 }),
            json!({ "preset": "draft", "firefly_threshold": 0.5 }),
            json!({ "preset": "draft", "denoiser": 3 }),
            json!({ "caustics": "suppress" }),
            json!({ "preset": "draft", "denoise": true }),
        ] {
            assert!(reference_trace(&mut world, &bad).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `psnr_db` (ADR-2315) on two PNGs: grey 128 against grey 154 is a step of 26/255 on every
    /// channel, so `20 log10(255 / 26)` dB; an image against itself has none (`null`); `mode: "rmse"`
    /// keeps it and `mode: "flip"` drops it.
    #[test]
    fn compare_reports_the_psnr_of_the_display_pair() {
        let mut world = World::new();
        let dir = regia_test_utils::unique_temp_dir("regia_agent_psnr");
        let write = |name: &str, v: u8| {
            let path = dir.join(name);
            regia_pt::output::write_png(&path, 8, 4, &[v, v, v, 255].repeat(32)).expect("write png");
            path.display().to_string()
        };
        let (a, b) = (write("a.png", 128), write("b.png", 154));
        let map = dir.join("map.png").display().to_string();
        let cmp = compare(&mut world, &json!({ "a": a, "b": b, "map_png": map })).expect("compare");
        let expected = 20.0 * (255.0f64 / 26.0).log10();
        assert!(
            cmp["psnr_db"]
                .as_f64()
                .is_some_and(|p| (p - expected).abs() < 1e-3),
            "{cmp}"
        );
        let same = compare(&mut world, &json!({ "a": a, "b": a, "map_png": map })).expect("same");
        assert!(same["psnr_db"].is_null(), "{same}");
        let rmse = compare(
            &mut world,
            &json!({ "a": a, "b": b, "map_png": map, "mode": "rmse" }),
        )
        .expect("rmse");
        assert!(rmse["psnr_db"].is_number(), "{rmse}");
        let flip = compare(
            &mut world,
            &json!({ "a": a, "b": b, "map_png": map, "mode": "flip" }),
        )
        .expect("flip");
        assert!(flip.get("psnr_db").is_none(), "{flip}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// DO27's Bore Twelve report: an airless level traced black and without its meshes. An ECS
    /// world on an airless planet with Jupiter in the sky and two boxes on a slab: the
    /// `current` trace goes through the renderer's export (both boxes and the ground are in
    /// it), takes the level's sun, sky body and stars, and is lit.
    #[test]
    fn current_trace_of_an_airless_level_uses_the_export_and_is_lit() {
        use std::sync::Arc;

        use regia_core::math::{Quat, Vec3};
        use regia_render::{Camera, CpuMaterial, CpuMesh, DirectionalLight, Environment, MeshRenderer};
        use regia_scene::level::{AtmospherePreset, LevelAtmosphere, LevelSettings, SkyBody};
        use regia_scene::{GlobalTransform, Transform};

        let mut app = App::new();
        app.add_plugins((
            regia_app::MinimalPlugins,
            regia_render::RenderPlugin { headless: true },
        ));
        app.insert_resource(Environment {
            ibl: true,
            ambient_lux: 0.0,
            double_sided: false,
        });
        app.insert_resource(LevelSettings {
            atmosphere: LevelAtmosphere {
                preset: AtmospherePreset::Airless,
                sun_illuminance_lux: Some(18_000.0),
                sky_body: Some(SkyBody {
                    direction: [0.3, 0.35, -1.0],
                    fill_lux: Some(1_600.0),
                    ..SkyBody::jupiter_from_europa()
                }),
                ..LevelAtmosphere::default()
            },
            ..LevelSettings::default()
        });
        let spawn_box = |app: &mut App, half: [f32; 3], at: Vec3, rgb: [f32; 3]| {
            let (vertices, indices) = regia_render::box_mesh(half);
            let transform = Transform::from_translation(at);
            app.world_mut().spawn((
                MeshRenderer::new(
                    Arc::new(CpuMesh { vertices, indices }),
                    Arc::new(CpuMaterial {
                        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
                        roughness: 0.8,
                        ..CpuMaterial::default()
                    }),
                ),
                transform,
                GlobalTransform(transform.compute_affine()),
            ));
        };
        spawn_box(
            &mut app,
            [30.0, 0.5, 30.0],
            Vec3::new(0.0, -0.5, -10.0),
            [0.6, 0.6, 0.65],
        );
        spawn_box(
            &mut app,
            [2.0, 2.0, 2.0],
            Vec3::new(-2.0, 2.0, -12.0),
            [0.6, 0.1, 0.1],
        );
        spawn_box(
            &mut app,
            [1.5, 1.0, 3.0],
            Vec3::new(3.0, 1.0, -9.0),
            [0.1, 0.2, 0.5],
        );
        let eye = Vec3::new(0.0, 3.0, 4.0);
        let camera = Transform {
            translation: eye,
            rotation: Quat::from_rotation_x(-0.15),
            ..Transform::IDENTITY
        };
        app.world_mut().spawn((
            Camera {
                fov_y: 0.9,
                ..Camera::default()
            },
            camera,
            GlobalTransform(camera.compute_affine()),
        ));
        app.world_mut().spawn(DirectionalLight {
            direction: Vec3::new(0.4, -0.45, 0.8).normalize().to_array(),
            color: [1.0, 0.96, 0.92],
            illuminance: 18_000.0,
            cast_shadows: true,
            static_shadow: false,
        });
        app.finish_plugins();
        app.update();

        let dir = std::env::temp_dir().join(format!("regia_agent_pt_airless_{}", std::process::id()));
        let done = reference_trace(
            app.world_mut(),
            &json!({ "scene": "current", "width": 64, "height": 36, "spp": 8, "max_bounces": 3,
                     "out_dir": dir.display().to_string(), "wait": true }),
        )
        .expect("trace");
        assert_eq!(done["status"], "done", "{done}");
        let info = &done["scene_info"];
        assert_eq!(info["source"], "export", "{info}");
        assert_eq!(info["instances"], 3, "{info}");
        assert_eq!(info["sky_bodies"], 1, "{info}");
        assert_eq!(info["stars"], true, "{info}");
        assert_eq!(info["sky"], "atmosphere_table", "{info}");
        assert!(
            info["clouds"].is_null(),
            "no cloud field unless asked for: {info}"
        );
        assert!(info["sun_lux"].as_f64().is_some_and(|l| l > 1_000.0), "{info}");
        let exr = done["exr"].as_str().expect("exr");
        let beauty = regia_pt::compare::load(std::path::Path::new(exr), "beauty").expect("beauty");
        let mean = beauty
            .rgb
            .iter()
            .map(|c| f64::from(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]))
            .sum::<f64>()
            / beauty.rgb.len().max(1) as f64;
        assert!(mean > 10.0, "the trace is black: {mean} cd/m²");

        // The extracted-frame path resolves the same level light (no renderer export).
        let extracted = reference_trace(
            app.world_mut(),
            &json!({ "scene": "current", "source": "extracted", "width": 32, "height": 18, "spp": 2,
                     "max_bounces": 2, "out_dir": dir.display().to_string(), "wait": true }),
        )
        .expect("trace");
        assert_eq!(extracted["scene_info"]["source"], "extracted");
        assert_eq!(extracted["scene_info"]["sky_bodies"], 1);
        assert_eq!(extracted["scene_info"]["instances"], 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2725: a frame whose material is a graph traces through `seeing.reference_trace` with the
    /// graph composed into the tracer (its stats in `metadata.surface_graphs`), and `seeing.compare`
    /// takes that trace end to end.
    #[test]
    fn a_graph_material_frame_traces_and_compares_end_to_end() {
        use regia_render::runtime::{
            CpuMesh, ExtractedCamera, ExtractedFrame, ExtractedMesh, ExtractedTarget, ExtractedView,
        };
        let graph = regia_matgraph::MaterialGraph::from_ron(
            r#"(
                format_version: 4,
                name: "agent_pt_graph",
                params: [(name: "tint", value: Color((0.2, 0.6, 0.9)))],
                textures: [],
                nodes: {
                    "wp": WorldPosition,
                    "n": Noise(input: "wp", octaves: 2),
                    "tint": Param("tint"),
                    "base": Mul("tint", "n"),
                },
                output: (base_color: Some("base")),
            )"#,
        )
        .expect("graph");
        let compiled = regia_matgraph::compile(&graph).expect("compile");
        let material = regia_matgraph::lab_material(
            &graph,
            &compiled,
            regia_matgraph::default_params(&graph, 0.0),
            &std::collections::BTreeMap::new(),
        )
        .expect("material");
        let (vertices, indices) = regia_render::uv_sphere(1.0, 32, 16);
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye: [0.0, 0.0, 4.0],
                target: [0.0; 3],
                fov_y: 0.8,
                near: 0.1,
                far: 100.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: regia_render::PostSettings::default(),
        });
        frame.meshes.push(ExtractedMesh {
            is_static: false,
            entity: Entity::PLACEHOLDER,
            mesh: std::sync::Arc::new(CpuMesh { vertices, indices }),
            material: std::sync::Arc::new(material),
            layer: 0,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: Vec::new(),
            decal_layer: 0,
            uv1: None,
            shadow: regia_render::ShadowCast::All,
            draw_indices: None,
            lod_key: 0,
            object_key_xor: 0,
            gi_proxy: false,
            lod_dither: 0.0,
        });
        frame.directional = Some(regia_render::DirectionalLightUniform::new(
            [-0.3, -0.5, -0.8],
            [1.0; 3],
            20_000.0,
        ));
        let mut world = World::new();
        world.insert_resource(frame);
        let dir = regia_test_utils::unique_temp_dir("regia_agent_pt_graph");
        let started = reference_trace(
            &mut world,
            &json!({ "scene": "current", "source": "extracted", "width": 32, "height": 32, "spp": 4,
                     "max_bounces": 2, "out_dir": dir.display().to_string(), "wait": true }),
        )
        .expect("trace");
        assert_eq!(started["scene_info"]["surface_graphs"], 1, "{started}");
        // The first trace of a graph builds its kernel set (minutes on lavapipe), longer than one
        // `wait`: poll, each call waiting up to its limit.
        let job = started["job"].as_u64().expect("job id");
        let mut done = started;
        for _ in 0..90 {
            if done["status"] != "running" {
                break;
            }
            done = reference_trace(&mut world, &json!({ "job": job, "wait": true })).expect("poll");
        }
        assert_eq!(done["status"], "done", "{done}");
        assert_eq!(done["metadata"]["surface_graphs"]["graphs"], 1, "{done}");
        assert_eq!(done["metadata"]["surface_graphs"]["materials"], 1, "{done}");
        assert_eq!(
            done["metadata"]["surface_graphs"]["refusals"],
            json!([]),
            "{done}"
        );
        let exr = done["exr"].as_str().expect("exr path").to_string();
        let cmp = compare(
            &mut world,
            &json!({ "a": { "job": job }, "b": exr, "aov": "beauty" }),
        )
        .expect("compare");
        assert!(cmp["flip_mean"].as_f64().is_some_and(|f| f < 1e-6), "{cmp}");
        // `subsurface` is a boolean and needs the frame's materials.
        let named = reference_trace(&mut world, &json!({ "scene": "cornell", "subsurface": true }));
        assert!(
            named.is_err_and(|e| format!("{e:?}").contains("scene current")),
            "a named scene has no frame materials"
        );
        let number = reference_trace(&mut world, &json!({ "scene": "current", "subsurface": 1 }));
        assert!(number.is_err(), "a number is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-5180: a host that opened no GPU on purpose refuses a trace and opens no device.
    #[test]
    fn a_host_without_a_gpu_refuses_a_trace_and_opens_no_device() {
        let mut world = World::new();
        world.insert_resource(crate::seeing::GpuDisabled {
            code: "editor_headless".into(),
            reason: "the headless editor has no GPU adapter (started with --no-gpu)".into(),
            hint: "start it without --no-gpu".into(),
        });
        let error = reference_trace(
            &mut world,
            &json!({ "scene": "cornell", "width": 16, "height": 16, "spp": 1, "max_bounces": 1 }),
        )
        .expect_err("refused");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("editor_headless"),
            "{error:?}"
        );
        assert!(
            world.get_resource::<regia_render::RenderDevice>().is_none(),
            "no device"
        );
    }
}

#[cfg(all(test, feature = "reference-trace", not(feature = "denoise")))]
mod no_denoiser_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn denoise_without_the_feature_is_f0013() {
        let mut world = World::new();
        let err = reference_trace(
            &mut world,
            &json!({ "scene": "cornell", "width": 8, "height": 8, "spp": 1, "denoise": true }),
        )
        .expect_err("no denoiser in this build");
        assert!(format!("{err:?}").contains("F0013"), "{err:?}");
        assert!(reference_trace(&mut world, &json!({ "denoise": "yes" })).is_err());
    }
}

#[cfg(all(test, feature = "denoise"))]
mod denoise_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_denoised_preview_is_never_the_reference() {
        let mut world = World::new();
        let dir = std::env::temp_dir().join(format!("regia_agent_oidn_{}", std::process::id()));
        let done = reference_trace(
            &mut world,
            &json!({ "scene": "cornell", "width": 48, "height": 48, "spp": 8, "max_bounces": 4,
                     "denoise": true, "out_dir": dir.display().to_string(), "wait": true }),
        )
        .expect("trace");
        assert_eq!(done["status"], "done", "{done}");
        assert_eq!(done["denoised"], true);
        let preview = done["exr"].as_str().expect("preview exr").to_string();
        let raw = done["raw"]["exr"].as_str().expect("raw exr").to_string();
        assert!(preview.ends_with("preview.exr") && raw.ends_with("reference.exr"));
        assert!(
            done["metadata"]["denoiser"]
                .as_str()
                .is_some_and(|d| d.starts_with("oidn")),
            "{done}"
        );
        let job = done["job"].as_u64().expect("job id");
        // As the reference (`a`): refused, by job and by path, EXR and PNG.
        for a in [
            json!({ "job": job }),
            json!(preview),
            json!(done["preview_png"].as_str().expect("png")),
        ] {
            let err = compare(&mut world, &json!({ "a": a, "b": raw })).expect_err("denoised reference");
            assert!(format!("{err:?}").contains("denoised"), "{err:?}");
        }
        // As the image under test (`b`): fine.
        let cmp = compare(&mut world, &json!({ "a": raw, "b": { "job": job } })).expect("compare");
        assert!(
            cmp["flip_mean"].as_f64().is_some_and(|f| f > 0.0 && f < 0.5),
            "{cmp}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
