// SPDX-License-Identifier: MPL-2.0
//! A render job's `product` block (`.render_job.ron` v3, ADR-2757, book `agents/capture-hooks.md`): frames come
//! from the product viewer's raster renderer, through the same player as the web page and the phone, and go to the
//! job's takes, writers and manifest like any other frame. No path tracing and no world: a product job needs no
//! `RuntimeRenderer`, its own device comes from `regia_viewer`, and a frame depends only on the job.

use std::path::Path;

use regia_app::prelude::World;
use regia_framework::shots::ShotSequence;
use regia_viewer::{BakedEnv, ViewerError};
use regia_web::capture::{
    CaptureRefusal, CaptureRig, CaptureRigError, CaptureScript, CheckedScript, Ease, ScriptCamera,
    ScriptPick, ScriptViewKey, check_script, pose_json, view_at_size,
};
use regia_web::product::PackageProduct;
use regia_web::seams::CameraPose;
use serde_json::{Value, json};

use super::plan::Request;
use super::takes::{PlanError, Rate, Sources, frame_at, frame_seconds, marker_time, need_sequence};
use crate::protocol::{self, RpcError};
use crate::render_job::{CutAt, PickValue, ProductCapture};

/// The plan's warning: timed picks at or past the end of the timeline are dropped.
const WARN_OPTION_PAST_END: &str = "product_option_past_end";
/// The plan's warning: the build was saved against another recipe than the package's.
const WARN_BUILD_RECIPE: &str = "product_build_recipe_changed";
/// The plan's warning: the build was saved against another catalogue snapshot than the package's.
const WARN_BUILD_SNAPSHOT: &str = "product_build_snapshot_changed";
/// The plan's warning: the shot list has fades or depth of field, and a product frame draws neither.
const WARN_SHOT_EFFECTS: &str = "product_shot_effects_not_applied";
/// The plan's warning: a zone has no usable library entry and draws a neutral look (`problems` lists them).
const WARN_LOOK_PROBLEMS: &str = "product_look_problems";

/// The studio the builds of a product job name.
const STUDIO_ID: &str = "studio-default";

/// The frame an `at` of a product block falls on.
fn frame_of(at: &CutAt, sources: Sources<'_>, rate: Rate) -> Result<u64, PlanError> {
    Ok(match at {
        CutAt::Frame(frame) => *frame,
        CutAt::Seconds(t) => frame_at(*t, rate),
        CutAt::Marker(name) => {
            let sequence = need_sequence(sources, &format!("a product time at marker `{name}`"))?;
            frame_at(marker_time(sequence, name, "a product time")?, rate)
        }
    })
}

/// The frames of a product block's times.
pub(crate) struct ProductTimes {
    picks: Vec<u64>,
    views: Vec<u64>,
}

impl ProductTimes {
    /// `Whole` of a job with no shot list and no sequence: up to the last timed event.
    pub fn last_frame(&self) -> Option<u64> {
        self.picks.iter().chain(&self.views).copied().max()
    }
}

/// Resolves the times of `product` against the job's files (a `Marker` names the `.seq.ron`).
pub(crate) fn resolve_times(
    product: &ProductCapture,
    sources: Sources<'_>,
    rate: Rate,
) -> Result<ProductTimes, PlanError> {
    let picks = product
        .options
        .iter()
        .map(|change| frame_of(&change.at, sources, rate))
        .collect::<Result<Vec<_>, _>>()?;
    let views = product
        .view
        .iter()
        .map(|key| frame_of(&key.at, sources, rate))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ProductTimes { picks, views })
}

fn pick_value(value: &PickValue) -> Value {
    match value {
        PickValue::One(label) => json!(label),
        PickValue::Many(labels) => json!(labels),
    }
}

/// A camera pose from a shot-list sample. Numbers read as mm in the piece's frame: a shot list holds no unit of
/// its own, and the viewer's world is mm. The field of view spans the height there and the shorter side here.
fn pose_of_shot(sample: &regia_framework::shots::ShotSample, size: [u32; 2]) -> CameraPose {
    let forward = sample.rotation * regia_core::glam::Vec3::NEG_Z;
    let up = sample.rotation * regia_core::glam::Vec3::Y;
    let distance = (sample.target - sample.eye).length();
    let look_at = sample.eye + forward * distance;
    let aspect = size[0].max(1) as f32 / size[1].max(1) as f32;
    let half_y = sample.fov_y_degrees.to_radians() * 0.5;
    let fov_short = if aspect >= 1.0 {
        sample.fov_y_degrees
    } else {
        (2.0 * (half_y.tan() * aspect).atan()).to_degrees()
    };
    CameraPose {
        eye: sample.eye.to_array(),
        look_at: look_at.to_array(),
        up: up.to_array(),
        fov_short_deg: fov_short,
        near: (distance * 0.05).max(0.01),
        far: distance * 20.0,
    }
}

/// The product of a job, read and checked, ready to draw.
pub(crate) struct PreparedProduct {
    product: PackageProduct,
    script: CaptureScript,
    checked: CheckedScript,
    info: ProductInfo,
}

/// What the plan, the progress and the manifest say about the product. No client data, no prices.
#[derive(Clone, Debug)]
struct ProductInfo {
    /// The package's hash.
    package_hash: String,
    /// The start build's identity (`house`, `product_key`, ...).
    identity: Value,
    /// Package mesh names.
    meshes: Vec<String>,
    /// What fell back to a neutral look somewhere on the timeline.
    problems: Vec<String>,
    /// The timed picks on the timeline.
    picks: Value,
}

fn refusal_code(refusal: &CaptureRefusal) -> &'static str {
    match refusal.kind {
        "build" => "render_job_build_refused",
        "pick" => "render_job_pick_refused",
        _ => "render_job_outside_view_range",
    }
}

/// The timeline a product is checked against.
#[derive(Clone, Copy)]
pub(crate) struct Timeline<'a> {
    /// The job's shot list.
    pub shots: Option<&'a ShotSequence>,
    /// Frames rendered.
    pub frames: u64,
    /// Its frame rate.
    pub rate: Rate,
    /// The delivered size, pixels.
    pub size: [u32; 2],
}

/// Reads `product` under the job's roots and walks the whole timeline with no GPU: a job a player would refuse
/// part way through is refused before it starts. Adds the warnings to `warnings`.
pub(crate) fn prepare(
    product_block: &ProductCapture,
    times: &ProductTimes,
    timeline: Timeline<'_>,
    resolve: &dyn Fn(&str, &str) -> Result<std::path::PathBuf, RpcError>,
    warnings: &mut Vec<&'static str>,
) -> Result<PreparedProduct, RpcError> {
    let job_error = super::takes::job_error;
    let Timeline {
        shots,
        frames,
        rate,
        size,
    } = timeline;
    let folder = resolve("product.package", &product_block.package)?;
    let package = read_package(&folder).map_err(|e| {
        job_error(
            "render_job_bad_package",
            format!("`product.package` {}: {e}", product_block.package),
        )
    })?;
    let build =
        match &product_block.build {
            Some(path) => {
                let file = resolve("product.build", path)?;
                // Capped, regular files only (ADR-5597): a FIFO here blocked the world thread.
                let text = crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES)
                    .and_then(|bytes| {
                        String::from_utf8(bytes)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                    })
                    .map_err(|e| {
                        job_error(
                            "render_job_unreadable",
                            format!("cannot read `product.build` {path}: {e}"),
                        )
                    })?;
                Some(serde_json::from_str::<Value>(&text).map_err(|e| {
                    job_error("render_job_build_refused", format!("`product.build` {path}: {e}"))
                })?)
            }
            None => None,
        };
    let mut product = PackageProduct::open(package, STUDIO_ID)
        .map_err(|e| job_error("render_job_bad_package", format!("`product.package`: {e}")))?;
    let view = view_at_size(&mut product, size)
        .map_err(|e| job_error("render_job_bad_package", format!("`product.package`: {e}")))?;
    let mut picks = product_block
        .options
        .iter()
        .zip(&times.picks)
        .map(|(change, frame)| ScriptPick {
            frame: *frame,
            group: change.group.clone(),
            value: pick_value(&change.value),
            adjust: change.adjust,
        })
        .collect::<Vec<_>>();
    // A stable sort: picks of one frame keep the order the file gives them.
    picks.sort_by_key(|pick| pick.frame);
    let camera = if let Some(shots) = shots {
        let mut poses = Vec::with_capacity(usize::try_from(frames).unwrap_or(0));
        for frame in 0..frames {
            let sample = shots.sample(frame_seconds(frame, rate)).ok_or_else(|| {
                job_error(
                    "render_job_bad_source",
                    "`shots` has no shot to sample".to_string(),
                )
            })?;
            poses.push(pose_of_shot(&sample, size));
        }
        let effects = shots
            .shots
            .iter()
            .any(|shot| shot.fade_in > 0.0 || shot.fade_out > 0.0 || shot.camera.dof.is_some());
        if effects {
            warnings.push(WARN_SHOT_EFFECTS);
        }
        ScriptCamera::Poses(poses)
    } else if product_block.view.is_empty() {
        ScriptCamera::Hold
    } else {
        let mut keys: Vec<ScriptViewKey> = product_block
            .view
            .iter()
            .zip(&times.views)
            .map(|(key, frame)| ScriptViewKey {
                frame: *frame,
                tilt: key.tilt,
                spin: key.spin,
                pan: key.pan,
                zoom: key.zoom,
                ease: key.ease.as_deref().and_then(Ease::parse).unwrap_or_default(),
            })
            .collect();
        keys.sort_by_key(|key| key.frame);
        ScriptCamera::View(keys)
    };
    let script = CaptureScript {
        frames,
        picks,
        camera,
    };
    let checked = check_script(&mut product, &view, build.as_ref(), &script)
        .map_err(|refusal| job_error(refusal_code(&refusal), refusal.message))?;
    if checked.picks_past_end > 0 {
        warnings.push(WARN_OPTION_PAST_END);
    }
    if checked.recipe_changed {
        warnings.push(WARN_BUILD_RECIPE);
    }
    if checked.snapshot_changed {
        warnings.push(WARN_BUILD_SNAPSHOT);
    }
    if !checked.problems.is_empty() {
        warnings.push(WARN_LOOK_PROBLEMS);
    }
    let start = &checked.start_build;
    let info = ProductInfo {
        package_hash: product.package_hash().to_owned(),
        identity: json!({
            "house": start.get("house"),
            "tenant_id": start.get("tenant_id"),
            "product_key": start.get("product_key"),
            "design_key": start.get("design_key"),
            "geometry_key": start.get("geometry_key"),
            "snapshot_id": start.get("snapshot_id"),
            "recipe": start.get("recipe"),
            "start_build_hash": start.get("build_hash"),
            "start_picks": start.get("picks"),
            "start_view": checked.start_view,
        }),
        meshes: product.mesh_names(),
        problems: checked.problems.clone(),
        picks: checked
            .picks
            .iter()
            .map(|pick| {
                json!({
                    "frame": pick.frame,
                    "time_s": frame_seconds(pick.frame, rate),
                    "group": pick.group,
                    "value": pick.value,
                    "adjusted": pick.adjusted,
                    "build_hash": pick.build_hash,
                    "orderable": pick.orderable,
                })
            })
            .collect(),
    };
    Ok(PreparedProduct {
        product,
        script,
        checked,
        info,
    })
}

#[cfg(test)]
thread_local! {
    /// Product rigs (each a device and its pipelines) this thread started to open, so a unit test can
    /// show that a refused job opened none, whatever adapter the test host has.
    pub(crate) static RIGS_OPENED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A package folder: read and verified, with its hashes.
fn read_package(folder: &Path) -> Result<regia_viewer_pack::Package, String> {
    regia_viewer_pack::read_dir(folder).map_err(|e| e.to_string())
}

impl PreparedProduct {
    /// The plan's `product` block.
    pub fn plan_json(&self) -> Value {
        self.info.to_json(None)
    }

    /// The device, every pipeline and the open product. `F0003` when the machine has no adapter.
    pub fn into_run(self, request: &Request) -> Result<(ProductRun, ProductRig), RpcError> {
        #[cfg(test)]
        RIGS_OPENED.with(|opened| opened.set(opened.get() + 1));
        let rig = CaptureRig::new(
            self.product,
            [request.render_width(), request.render_height()],
            [request.width, request.height],
            &BakedEnv::studio_default(),
        )
        .map_err(|error| match error {
            CaptureRigError::Device(ViewerError::NoAdapter(message)) => RpcError::new(
                protocol::UNSUPPORTED,
                "F0003",
                format!("no GPU adapter: {message}"),
                "install the lavapipe driver on a headless machine",
            ),
            other => RpcError::internal(other.to_string()),
        })?;
        let (adapter, backend) = rig.adapter();
        let run = ProductRun {
            info: self.info,
            adapter,
            backend,
            camera: Vec::new(),
        };
        let rig = ProductRig {
            rig,
            script: self.script,
            checked: self.checked,
        };
        Ok((run, rig))
    }
}

impl ProductInfo {
    fn to_json(&self, camera: Option<&[Value]>) -> Value {
        let mut block = json!({
            "renderer_used": "regia_viewer",
            "quality": "raster",
            "package_hash": self.package_hash,
            "identity": self.identity,
            "meshes": self.meshes,
            "problems": self.problems,
            "option_changes": self.picks,
        });
        if let Some(camera) = camera {
            block["camera"] = json!(camera);
        }
        block
    }
}

/// The running product job's part that moves between threads with the job.
pub(crate) struct ProductRun {
    info: ProductInfo,
    adapter: String,
    backend: String,
    camera: Vec<Value>,
}

impl ProductRun {
    /// The manifest's `product` block, `delivered` frames in: `jewelry.render_out/1`'s extra fields
    /// (`interfaces.md` § 8; the id-AOV classes do not exist here, since a package is one house and its shared parts).
    pub fn manifest_json(&self, delivered: u64) -> Value {
        let kept = usize::try_from(delivered)
            .unwrap_or(usize::MAX)
            .min(self.camera.len());
        let mut block = self.info.to_json(Some(&self.camera[..kept]));
        block["adapter"] = json!(self.adapter);
        block["backend"] = json!(self.backend);
        block
    }

    /// The `product` line of `render.out_progress`.
    pub fn progress_json(&self) -> Value {
        json!({
            "renderer_used": "regia_viewer",
            "package_hash": self.info.package_hash,
            "adapter": self.adapter,
            "frames_drawn": self.camera.len(),
        })
    }
}

/// The player of a running product job. Not `Send` (the player's seams are not), so it lives on the app's main
/// thread as a non-send resource and the render-out's exclusive system uses it there.
pub(crate) struct ProductRig {
    rig: CaptureRig,
    script: CaptureScript,
    checked: CheckedScript,
}

/// Draws timeline frame `index` and returns its pixels at the render size.
pub(crate) fn draw_frame(
    world: &mut World,
    run: &mut ProductRun,
    index: u64,
    dt_ms: f64,
) -> Result<Vec<u8>, String> {
    let mut rig = world
        .get_non_send_mut::<ProductRig>()
        .ok_or("the product player is gone")?;
    let rig = &mut *rig;
    let frame = rig
        .rig
        .script_frame(&rig.script, &rig.checked, index, dt_ms)
        .map_err(|e| format!("frame {index}: {e}"))?;
    run.camera.push(pose_json(index, &frame.pose));
    Ok(frame.still.rgba)
}

/// Drops the job's player and its device.
pub(crate) fn end(world: &mut World) {
    world.remove_non_send::<ProductRig>();
}

#[cfg(test)]
mod tests {
    use regia_framework::shots::ShotSequence;

    use super::*;

    const SHOT: &str = r#"(version: 2, shots: [(name: "a", duration: 1.0, camera: (
        path: (keys: [(time: 0.0, position: (0.0, 0.0, 60.0))], look_at: Point((0.0, 0.0, 0.0))),
        fov: [(time: 0.0, value: 10.0)],
    ))])"#;

    fn sample() -> regia_framework::shots::ShotSample {
        ShotSequence::from_ron(SHOT)
            .expect("shots")
            .sample(0.0)
            .expect("a sample")
    }

    #[test]
    fn a_shot_sample_is_a_pose_in_mm_looking_at_its_target() {
        let pose = pose_of_shot(&sample(), [100, 100]);
        assert_eq!(pose.eye, [0.0, 0.0, 60.0]);
        assert!(pose.look_at.iter().all(|n| n.abs() < 1e-4), "{:?}", pose.look_at);
        assert!((pose.up[1] - 1.0).abs() < 1e-5, "{:?}", pose.up);
        assert!((pose.fov_short_deg - 10.0).abs() < 1e-5);
    }

    #[test]
    fn a_portrait_frame_narrows_the_short_side_a_landscape_one_keeps_the_height() {
        let wide = pose_of_shot(&sample(), [200, 100]);
        assert!((wide.fov_short_deg - 10.0).abs() < 1e-5, "{}", wide.fov_short_deg);
        let tall = pose_of_shot(&sample(), [100, 200]);
        let want = 2.0 * (5.0_f32.to_radians().tan() * 0.5).atan();
        assert!(
            (tall.fov_short_deg - want.to_degrees()).abs() < 1e-4,
            "{}",
            tall.fov_short_deg
        );
    }

    #[test]
    fn a_times_marker_needs_the_sequence() {
        let change = ProductCapture {
            package: "p".into(),
            build: None,
            options: vec![crate::render_job::OptionChange {
                at: CutAt::Marker("swap".into()),
                group: "g".into(),
                value: PickValue::One("v".into()),
                adjust: false,
            }],
            view: Vec::new(),
        };
        let rate = Rate { num: 30, den: 1 };
        let Err(error) = resolve_times(&change, Sources::default(), rate) else {
            panic!("no sequence to find the marker in");
        };
        assert_eq!(error.code, "render_job_no_source");
    }

    #[test]
    fn times_land_on_the_first_frame_at_or_after_them() {
        let product = ProductCapture {
            package: "p".into(),
            build: None,
            options: vec![crate::render_job::OptionChange {
                at: CutAt::Seconds(1.01),
                group: "g".into(),
                value: PickValue::Many(vec![]),
                adjust: false,
            }],
            view: vec![crate::render_job::ProductViewKey {
                at: CutAt::Frame(90),
                tilt: None,
                spin: None,
                pan: None,
                zoom: None,
                ease: None,
            }],
        };
        let times = resolve_times(&product, Sources::default(), Rate { num: 30, den: 1 })
            .expect("no marker to look up");
        assert_eq!(times.picks, vec![31]);
        assert_eq!(times.views, vec![90]);
        assert_eq!(times.last_frame(), Some(90));
    }

    #[test]
    fn a_pick_value_is_what_set_pick_takes() {
        assert_eq!(pick_value(&PickValue::One("a".into())), json!("a"));
        assert_eq!(pick_value(&PickValue::Many(vec!["a".into()])), json!(["a"]));
    }
}
