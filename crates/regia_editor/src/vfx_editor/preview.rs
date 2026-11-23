// SPDX-License-Identifier: MPL-2.0
//! The live preview: the open effect driven through `regia_vfx`'s offscreen harness
//! (`regia_vfx::offscreen::VfxHarness`: sky, lit ground, the full GPU simulate and draw) at a fixed
//! step, read back as RGBA8 for an egui texture or a PNG.
//!
//! A session is one harness, one scene and one spawned instance. Time only moves forward inside a
//! session: a later time steps on, an earlier time (a scrub back, a restart), a new seed, a new size
//! or an edit to the effect starts a fresh session, so a given `(effect, seed, time)` always shows the
//! frame the same number of fixed steps produced. Nothing in the renderer's frame code is used or
//! changed.

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::format::VfxSystemDef;
use regia_vfx::gpu::{VfxCamera, VfxProfile};
use regia_vfx::measure::measure_effect;
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::probe::{ProbeSettings, probe_effect};
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::timing::GpuTimer;

use super::sim::{FORWARD, ORIGIN, STEP, steps_for};

/// Smallest preview side in pixels.
pub const MIN_SIDE: u32 = 32;
/// Largest preview side in pixels.
pub const MAX_SIDE: u32 = 1024;
/// Most simulated seconds one preview may reach (each step is one GPU frame).
pub const MAX_TIME: f32 = 30.0;

/// Particle capacity of the preview pool (the harness's desktop High profile sizes the rest).
const PREVIEW_CAPACITY: u32 = 100_000;

/// One preview frame.
#[derive(Clone, Debug)]
pub struct PreviewFrame {
    /// Tightly packed sRGB RGBA8, top row first.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Simulated seconds (`steps * STEP`).
    pub time: f32,
    /// Fixed steps simulated in this session.
    pub steps: u32,
    /// The instance seed.
    pub seed: u64,
    /// Fraction of pixels the effect changed against the same backdrop without it.
    pub coverage: f32,
    /// GPU list counters after the frame: `[additive, alpha, mesh, alive]`.
    pub counters: [u32; 4],
    /// GPU time of the VFX simulate + draw passes of the last step in ms, when the device has
    /// timestamp queries.
    pub gpu_ms: Option<f32>,
}

/// Why a preview failed.
#[derive(Clone, Debug, PartialEq)]
pub enum PreviewError {
    /// Size or time out of range.
    BadValue(String),
    /// The harness or a frame failed on the GPU.
    Gpu(String),
}

/// A running preview session.
pub struct PreviewSession {
    harness: VfxHarness,
    scene: VfxScene,
    camera: VfxCamera,
    timer: Option<GpuTimer>,
    background: Vec<u8>,
    revision: u64,
    seed: u64,
    width: u32,
    height: u32,
    steps: u32,
    last: Option<PreviewFrame>,
    /// The previewed instance's id (its GPU emitter records, [`Self::gpu_bounds`]).
    instance: u64,
}

impl std::fmt::Debug for PreviewSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewSession")
            .field("revision", &self.revision)
            .field("seed", &self.seed)
            .field("size", &(self.width, self.height))
            .field("steps", &self.steps)
            .finish_non_exhaustive()
    }
}

/// A camera that frames `def` around [`ORIGIN`]: the CPU probe's envelope one second in.
#[must_use]
pub fn frame_camera(def: &VfxSystemDef, seed: u64) -> VfxCamera {
    let settings = ProbeSettings {
        dt: STEP,
        seed,
        ..ProbeSettings::default()
    };
    let snaps = probe_effect(def, ORIGIN, FORWARD, settings, &[1.0]);
    let mut reach: f32 = 1.0;
    let mut top: f32 = ORIGIN.y + 1.0;
    if let Some(snap) = snaps.first() {
        for e in snap.emitters.iter().filter(|e| e.live > 0) {
            let off = (e.centroid - ORIGIN).length();
            if (off + e.envelope_radius).is_finite() {
                reach = reach.max(off + e.envelope_radius);
            }
            if e.top.is_finite() {
                top = top.max(e.top);
            }
        }
    }
    let reach = reach.min(80.0);
    let target = Vec3::new(ORIGIN.x, (ORIGIN.y + top.min(ORIGIN.y + 80.0)) * 0.5, ORIGIN.z);
    let distance = (reach * 2.4).max(3.0);
    VfxCamera::looking_at(target + Vec3::new(0.0, distance * 0.35, distance), target)
}

impl PreviewSession {
    /// Camera used by the cached preview, also used to project diagnostic bounds.
    pub(crate) fn camera(&self) -> VfxCamera {
        self.camera
    }

    /// Starts a session: a fresh harness, the backdrop frame without the effect, then the effect
    /// spawned at [`ORIGIN`] with `seed`.
    ///
    /// # Errors
    ///
    /// [`PreviewError::Gpu`] when the harness cannot be built or the first frame fails.
    pub fn start(
        device: &RenderDevice,
        def: &VfxSystemDef,
        revision: u64,
        seed: u64,
        width: u32,
        height: u32,
    ) -> Result<Self, PreviewError> {
        let _span = tracing::info_span!("vfx_editor.preview_start", width, height).entered();
        let gpu = |e: regia_vfx::VfxError| PreviewError::Gpu(e.to_string());
        let mut harness = VfxHarness::new(device, VfxProfile::desktop_high()).map_err(gpu)?;
        let camera = frame_camera(def, seed);
        let mut library = VfxLibrary::new();
        library
            .add(def.clone())
            .map_err(|e| PreviewError::Gpu(format!("the effect does not load: {e}")))?;
        let mut scene = VfxScene::new(library);
        // The backdrop with no instance yet: the coverage reference.
        scene.update(0.0, &lod(&camera), PREVIEW_CAPACITY);
        scene.upload(harness.gpu());
        let background = harness
            .render(device, &camera, width, height, STEP)
            .map_err(gpu)?
            .pixels;
        let Some(instance) = scene.spawn(&def.name, ORIGIN, FORWARD, seed) else {
            return Err(PreviewError::Gpu(format!("cannot spawn `{}`", def.name)));
        };
        Ok(Self {
            harness,
            scene,
            camera,
            timer: GpuTimer::new(&device.device, 4),
            background,
            revision,
            seed,
            width,
            height,
            steps: 0,
            last: None,
            instance,
        })
    }

    /// The GPU particle bounds of the previewed effect, one per emitter (`None`: nothing alive),
    /// from the pool's bounds reduction ([`regia_vfx::VfxGpu::read_particle_bounds`]). Blocks on
    /// the device: the panel calls it at a bounded rate and only when opted in (P-225). `None`
    /// before the first frame or when the read fails.
    pub fn gpu_bounds(&mut self) -> Option<Vec<Option<regia_core::math::Aabb>>> {
        let _span = tracing::info_span!("vfx_editor.gpu_bounds").entered();
        let (base, count) = self.scene.gpu_emitter_run(self.instance)?;
        let records = self.harness.gpu().read_particle_bounds()?;
        Some(
            (0..count)
                .map(|i| records.get((base + i) as usize).copied().flatten())
                .collect(),
        )
    }

    /// Whether this session can reach `steps` for the given effect revision, seed and size.
    #[must_use]
    pub fn reaches(&self, revision: u64, seed: u64, width: u32, height: u32, steps: u32) -> bool {
        self.revision == revision
            && self.seed == seed
            && self.width == width
            && self.height == height
            && self.steps <= steps
    }

    /// Steps simulated so far.
    #[must_use]
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// The last frame read back.
    #[must_use]
    pub fn last(&self) -> Option<&PreviewFrame> {
        self.last.as_ref()
    }

    /// Steps on to `steps` (at least one step when none was read back yet) and reads the last frame.
    ///
    /// # Errors
    ///
    /// [`PreviewError::Gpu`] when a frame fails.
    pub fn advance_to(&mut self, device: &RenderDevice, steps: u32) -> Result<&PreviewFrame, PreviewError> {
        let _span = tracing::info_span!("vfx_editor.preview_advance", steps).entered();
        let gpu = |e: regia_vfx::VfxError| PreviewError::Gpu(e.to_string());
        let target = steps.max(self.steps).max(1);
        if target == self.steps
            && let Some(last) = self.last.as_ref()
            && last.steps == target
        {
            return self
                .last
                .as_ref()
                .ok_or_else(|| PreviewError::Gpu("no frame".into()));
        }
        let lod = lod(&self.camera);
        let (w, h) = (self.width, self.height);
        let mut frame = None;
        let mut gpu_ms = None;
        while self.steps < target {
            self.scene.update(STEP, &lod, PREVIEW_CAPACITY);
            self.scene.upload(self.harness.gpu());
            self.steps += 1;
            let last_step = self.steps == target;
            if last_step && let Some(timer) = self.timer.as_ref() {
                // The last step is timed, then a near-zero step reads its pixels back.
                self.harness
                    .render_timed(device, &self.camera, w, h, STEP, timer)
                    .map_err(gpu)?;
                gpu_ms = timer
                    .read_ms(&device.device, &device.queue)
                    .map(|ms| ms.iter().take(2).sum::<f32>());
                frame = Some(
                    self.harness
                        .render(device, &self.camera, w, h, 1.0e-3)
                        .map_err(gpu)?,
                );
            } else if last_step {
                frame = Some(
                    self.harness
                        .render(device, &self.camera, w, h, STEP)
                        .map_err(gpu)?,
                );
            } else {
                self.harness
                    .render(device, &self.camera, w, h, STEP)
                    .map_err(gpu)?;
            }
        }
        let frame = frame.ok_or_else(|| PreviewError::Gpu("no frame was read back".into()))?;
        let coverage = if frame.pixels.len() == self.background.len() {
            measure_effect(&frame.pixels, &self.background, w).coverage
        } else {
            0.0
        };
        self.last = Some(PreviewFrame {
            pixels: frame.pixels,
            width: w,
            height: h,
            time: self.steps as f32 * STEP,
            steps: self.steps,
            seed: self.seed,
            coverage,
            counters: frame.counters,
            gpu_ms,
        });
        self.last
            .as_ref()
            .ok_or_else(|| PreviewError::Gpu("no frame".into()))
    }
}

fn lod(camera: &VfxCamera) -> LodInput {
    LodInput {
        camera_pos: camera.eye,
    }
}

/// Checks a preview request: sides in [`MIN_SIDE`]..=[`MAX_SIDE`], time in `0..=`[`MAX_TIME`].
/// Returns the steps the time needs.
///
/// # Errors
///
/// [`PreviewError::BadValue`].
pub fn check_request(width: u32, height: u32, time: f32) -> Result<u32, PreviewError> {
    if !(MIN_SIDE..=MAX_SIDE).contains(&width) || !(MIN_SIDE..=MAX_SIDE).contains(&height) {
        return Err(PreviewError::BadValue(format!(
            "preview size must be {MIN_SIDE}..={MAX_SIDE} per side, got {width}x{height}"
        )));
    }
    if !time.is_finite() || !(0.0..=MAX_TIME).contains(&time) {
        return Err(PreviewError::BadValue(format!(
            "`time` must be in 0..={MAX_TIME} seconds, got {time}"
        )));
    }
    Ok(steps_for(time))
}

/// What a preview shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewRequest {
    /// The document revision the effect belongs to (a new revision restarts the session).
    pub revision: u64,
    /// The instance seed.
    pub seed: u64,
    /// Seconds into the effect.
    pub time: f32,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// Renders the frame `request` names, reusing `session` when it can reach that time.
///
/// # Errors
///
/// See [`PreviewError`].
pub fn render_at<'a>(
    session: &'a mut Option<PreviewSession>,
    device: &RenderDevice,
    def: &VfxSystemDef,
    request: &PreviewRequest,
) -> Result<&'a PreviewFrame, PreviewError> {
    let PreviewRequest {
        revision,
        seed,
        time,
        width,
        height,
    } = *request;
    let steps = check_request(width, height, time)?;
    let reusable = session
        .as_ref()
        .is_some_and(|s| s.reaches(revision, seed, width, height, steps.max(1)));
    if !reusable {
        *session = Some(PreviewSession::start(device, def, revision, seed, width, height)?);
    }
    match session.as_mut() {
        Some(s) => s.advance_to(device, steps),
        None => Err(PreviewError::Gpu("no preview session".into())),
    }
}

/// Encodes RGBA8 pixels as a PNG.
///
/// # Errors
///
/// The encoder's message.
pub fn png_bytes(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(pixels).map_err(|e| e.to_string())?;
    }
    Ok(out)
}
