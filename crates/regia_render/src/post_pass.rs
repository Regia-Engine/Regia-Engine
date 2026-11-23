// SPDX-License-Identifier: MPL-2.0
//! Fullscreen post stack: exposure, dual-filter bloom, AgX, FXAA or SMAA, single-frame TAA,
//! plus the final blit (or the SGSR upscale) that maps the internal LDR image onto a window
//! surface.
//!
//! The chain owns its pipelines, shader modules and intermediate textures
//! persistently. Parameter bind groups are cached and rebuilt only when a bound
//! view or buffer changes. The caller records this into the scene encoder and
//! submits once. The pass sequence is unchanged from the golden harness:
//! TAA → exposure → bloom (down/up/composite) → tonemap → FXAA → blit. With [`PostAa`]
//! SMAA ([`crate::smaa`]) replaces FXAA, and SGSR (`shaders/sgsr.wgsl`) replaces the blit:
//! the stack then runs at the internal size and the last pass upscales to the output.
//! That upscale reads the same uniform tail as the blit, including the HDR headroom, and
//! expands highlights the same way.
//!
//! Auto exposure reads a histogram from at least two frames ago ([`crate::readback`]).
//! The slot filled this frame is mapped after that single submit.

use crate::RenderError;
use crate::exposure_zones::{MeterReading, ZoneExposure};
use crate::frame_work::counted::*;
use crate::post::{PostSettings, exposure_from_ev100};
use crate::readback::{
    self, READBACK_LATENCY, READBACK_SLOTS, ST_FAILED, ST_IDLE, ST_INFLIGHT, average_log_luminance,
    slot_is_consumable, smooth_luminance,
};
use crate::shader_lib::compose_source;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

const MODE_EXPOSURE: f32 = 0.0;
const MODE_DOWN: f32 = 1.0;
const MODE_UP: f32 = 2.0;
const MODE_COMPOSITE: f32 = 3.0;
const MODE_TONEMAP: f32 = 4.0;
const MODE_FXAA: f32 = 5.0;
const MODE_TAA: f32 = 6.0;
/// Nearest 1:1 copy of the source, optionally decoding sRGB for linear targets.
const MODE_BLIT: f32 = 7.0;
/// Pass slots in the persistent parameter uniform pool.
const PARAM_SLOTS: usize = 16;

/// The non-temporal AA and upscale choices the frame hands the post stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PostAa {
    /// SMAA 1× after the tonemap instead of FXAA ([`crate::FrameQuality::smaa`]).
    pub(crate) smaa: bool,
    /// Upscale to this output view size with SGSR instead of the 1:1 blit; the stack runs
    /// at the size [`PostChain::record`] is given.
    pub(crate) sgsr_output: Option<(u32, u32)>,
}

/// Which shader module a fullscreen pass runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Program {
    /// `post.wgsl` (every mode).
    Post,
    /// `sgsr.wgsl`: the spatial upscale onto the output.
    Sgsr,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PostParams {
    params: [f32; 4],
    extra: [f32; 4],
    /// The view's grade (every pass carries it; the exposure pass reads the white
    /// balance, the tonemap pass lift / gamma / gain): white-balance gains (w unused),
    /// lift, gamma, gain.
    grade: [[f32; 4]; 4],
    /// Local exposure (the exposure pass reads it): shadow contrast, highlight contrast,
    /// detail strength, 1 when on; then the grid mapping
    /// ([`crate::local_exposure::LocalExposurePass::grid_row`]).
    local: [[f32; 4]; 2],
}

struct Tex {
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// A post pass waiting for [`PostChain::record_queued`].
struct QueuedPass {
    dest: wgpu::TextureView,
    src: wgpu::TextureView,
    add: wgpu::TextureView,
    params: [f32; 4],
    extra: [f32; 4],
    format: wgpu::TextureFormat,
    nearest: bool,
    viewport: [f32; 4],
    program: Program,
}

/// One step of the post stack, in recording order.
// Steps live in one reused Vec for the length of `record`; boxing each pass would add
// an allocation per pass per frame.
#[allow(clippy::large_enum_variant)]
enum QueuedStep {
    Pass(QueuedPass),
    /// The auto-exposure histogram of this view.
    Histogram(wgpu::TextureView),
    /// Local exposure's bilateral grid of `src`, exposed by `exposure` and balanced by `wb`
    /// as the exposure pass does.
    LocalExposure {
        src: wgpu::TextureView,
        exposure: f32,
        wb: [f32; 3],
    },
    /// SMAA's three passes from `src` into `dest` (the stack's size, `Rgba8Unorm`).
    Smaa {
        src: wgpu::TextureView,
        dest: wgpu::TextureView,
    },
}

/// Persistent intermediate state, rebuilt only when the size or mip count changes.
struct PostState {
    width: u32,
    height: u32,
    /// Bloom mip count the bloom targets were built for.
    mips: u32,
    /// HDR target shared by the TAA output and the bloom composite: the TAA output is dead
    /// once the exposure pass has read it, before the composite writes (one full-res
    /// `Rgba16Float` less than a target per pass; lane mem-stream).
    taa_composite: Tex,
    exposed: Tex,
    tonemapped: Tex,
    /// Created on the first frame with FXAA on (a TAA frame never needs it).
    fxaa: Option<Tex>,
    /// `Rgba16Float` twins of `tonemapped` and `fxaa` for a deep (float) output target, created on
    /// the first such frame (`crate::deep_readout`, ADR-1404). SMAA keeps the 8-bit `fxaa`.
    tonemapped_deep: Option<Tex>,
    fxaa_deep: Option<Tex>,
    /// `mips` downsample targets followed by `mips - 1` upsample targets.
    bloom: Vec<Tex>,
    /// Parameter uniform buffers, one per pass position in the stack (rewritten per
    /// frame; a pass keeps its buffer, so its bind group stays cached).
    params: Vec<wgpu::Buffer>,
}

/// One histogram readback slot. `bins` is the GPU storage image of the 64 bins;
/// `staging` is the mappable copy. Only an idle slot is written.
struct HistSlot {
    bins: wgpu::Buffer,
    staging: wgpu::Buffer,
    state: Arc<AtomicU8>,
    submitted_frame: u64,
}

/// Persistent histogram state for auto exposure.
///
/// Three slots, read two frames late. See [`crate::readback`].
struct Histogram {
    bgl: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
    slots: [HistSlot; READBACK_SLOTS],
}

/// The persistent post stack. Created once per renderer and reused every frame.
#[derive(Default)]
pub(crate) struct PostChain {
    module: Option<wgpu::ShaderModule>,
    /// `sgsr.wgsl`, on the post layout (it reads bindings 0–2 only).
    sgsr_module: Option<wgpu::ShaderModule>,
    bgl: Option<wgpu::BindGroupLayout>,
    layout: Option<wgpu::PipelineLayout>,
    sampler: Option<wgpu::Sampler>,
    nearest: Option<wgpu::Sampler>,
    pipelines: HashMap<(Program, wgpu::TextureFormat), wgpu::RenderPipeline>,
    /// SMAA's targets, tables and pipelines (built on first use).
    smaa: crate::smaa::SmaaPass,
    state: Option<PostState>,
    /// This frame's passes, recorded by [`Self::record_queued`].
    queued: Vec<QueuedStep>,
    hist: Option<Histogram>,
    /// Log-smoothed average luminance from a histogram at least two frames old.
    smoothed_luminance: Option<f32>,
    /// Smoothed bright-subject coverage of the same histograms (0 with the compensation off).
    smoothed_bright: f32,
    /// The newest histogram alone, `(luminance, bright coverage)`: what an exposure zone meters
    /// (`crate::exposure_zones::MeterReading::latest_ev`, ADR-322).
    latest_sample: Option<(f32, f32)>,
    /// Frames [`Self::record`] has been entered. Histogram slots are stamped with it.
    hist_frame: u64,
    /// Histograms stamped at or before this frame belong to the view before a camera cut
    /// and are dropped unread ([`Self::reset_exposure`]).
    hist_discard_through: u64,
    /// Wait for a histogram readback that is due instead of using it on a later frame
    /// (`FrameRenderer::set_wait_for_exposure_readback`, ADR-1402).
    wait_readbacks: bool,
    /// Fullscreen groups keyed by the two views, the sampler choice, the param buffer, the
    /// LUT and the local-exposure grid.
    bg_cache: crate::bind_cache::BindCache<(
        wgpu::TextureView,
        wgpu::TextureView,
        bool,
        wgpu::Buffer,
        wgpu::TextureView,
        wgpu::TextureView,
    )>,
    /// Histogram groups keyed by the HDR view and the bin buffer.
    hist_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::Buffer)>,
    /// Histogram slot to map after the caller submits the frame encoder.
    pending_hist: Option<usize>,
    /// The first pass that read the caller's HDR input last frame: its parameter slot, sampler
    /// choice and `add` view (`None` when it was the input too), so [`Self::prewarm_input`] can build its bind group for a view the
    /// chain has not been handed yet.
    head_pass: Option<(usize, bool, Option<wgpu::TextureView>)>,
    /// The histogram read the caller's HDR input directly last frame (no TAA pass before it).
    hist_reads_input: bool,
    /// Grade rows of the view being recorded (see [`PostParams::grade`]).
    grade: [[f32; 4]; 4],
    /// Local-exposure rows of the view being recorded (see [`PostParams::local`]).
    local: [[f32; 4]; 2],
    /// Local exposure's grid passes.
    local_exposure: crate::local_exposure::LocalExposurePass,
    /// Meter of the view being recorded; the delayed histogram is averaged with it.
    meter: crate::post::ExposureMeter,
    /// The bound 3D LUT (content key, texture view), or the 2³ identity.
    lut: Option<(u64, wgpu::TextureView)>,
    /// The texture behind `lut` and its edge: a new table of the same edge is uploaded into it,
    /// so a LUT that changes every frame (post volumes blending, ADR-2323) keeps one texture,
    /// one view and its cached bind groups.
    lut_texture: Option<(u32, wgpu::Texture)>,
    identity_lut: Option<wgpu::TextureView>,
    /// `(exposure, readback ms)` from [`Self::prepare_exposure`], consumed by the next
    /// [`Self::record`] so translucent layers divide by the value the post multiplies by.
    prepared: Option<(f32, f32)>,
    /// Pre-exposure of the HDR input ([`crate::post::pre_exposure_scale`]); 0 reads as 1.
    pre_exposure: f32,
    /// Dither the 8-bit display image (`FrameRenderer::set_output_dither`, ADR-2486).
    output_dither: bool,
}

impl PostChain {
    /// Turns the 8-bit output dither on or off for the next [`Self::record`] (ADR-2486).
    pub(crate) fn set_output_dither(&mut self, on: bool) {
        self.output_dither = on;
    }

    /// The HDR input of the next [`Self::record`] holds radiance × `scale`: the exposure pass
    /// and the meter divide it out.
    /// Moves `other`'s shader module, layouts, samplers and per-format pipelines into a chain
    /// that has none yet (`FrameRenderer::adopt_warm_pipelines`); no frame state moves.
    pub(crate) fn adopt_pipelines(&mut self, other: &mut PostChain) -> bool {
        if self.module.is_some() || other.module.is_none() {
            return false;
        }
        self.module = other.module.take();
        // Without it the first SGSR frame (Mobile High/Low, the Deck fallback) panics.
        self.sgsr_module = other.sgsr_module.take();
        self.bgl = other.bgl.take();
        self.layout = other.layout.take();
        self.sampler = other.sampler.take();
        self.nearest = other.nearest.take();
        self.pipelines = std::mem::take(&mut other.pipelines);
        true
    }

    pub(crate) fn set_pre_exposure(&mut self, scale: f32) {
        self.pre_exposure = scale;
    }

    /// The exposure [`Self::prepare_exposure`] chose for the frame being recorded (what
    /// [`Self::record`] will apply); `None` outside that window.
    pub(crate) fn prepared_exposure(&self) -> Option<f32> {
        self.prepared.map(|(exposure, _)| exposure)
    }

    fn pre_exposure(&self) -> f32 {
        if self.pre_exposure > 0.0 && self.pre_exposure.is_finite() {
            self.pre_exposure
        } else {
            1.0
        }
    }

    /// Forgets the metered exposure (camera cut): the next frames use the initial exposure
    /// until the new view's first histogram, which is adopted outright, as on a fresh chain.
    /// Histograms still in flight show the old view and are dropped.
    pub(crate) fn reset_exposure(&mut self) {
        self.smoothed_luminance = None;
        self.smoothed_bright = 0.0;
        self.latest_sample = None;
        self.prepared = None;
        self.hist_discard_through = self.hist_frame;
    }

    /// The auto-exposure meter's smoothed scene luminance (cd/m²), `None` until the first
    /// histogram readback.
    pub(crate) fn metered_luminance(&self) -> Option<f32> {
        self.smoothed_luminance
    }

    /// The meter's smoothed bright-subject coverage (`ExposureMeter::bright_threshold`).
    pub(crate) fn metered_bright_coverage(&self) -> f32 {
        self.smoothed_bright
    }

    /// Records the post stack into `encoder`. The caller submits once.
    ///
    /// Returns the CPU milliseconds spent pumping the delayed histogram.
    /// [`Self::map_after_submit`] maps the slot this frame filled; call it after
    /// that submit. The value applied this frame was captured at least
    /// [`READBACK_LATENCY`] frames ago.
    /// Pumps the delayed histogram and returns the exposure the next [`Self::record`]
    /// applies with `settings`.
    ///
    /// The frame renderer calls this before translucent layers simulate, so an effect drawn
    /// into the pre-exposure HDR target divides by the exact multiplier (auto exposure
    /// included). Calling it is optional: `record` prepares itself when it was not called.
    /// `zones` is the main view's zone-driven eye (`None` for texture views): see
    /// [`Self::exposure_for`].
    /// [`Self::wait_readbacks`]: wait for due histogram readbacks (deterministic auto exposure).
    pub(crate) fn set_wait_readbacks(&mut self, wait: bool) {
        self.wait_readbacks = wait;
    }

    pub(crate) fn prepare_exposure(
        &mut self,
        device: &wgpu::Device,
        settings: &PostSettings,
        zones: Option<&mut ZoneExposure>,
    ) -> f32 {
        let prepared = self.prepare_exposure_inner(device, settings, zones);
        self.prepared = Some(prepared);
        prepared.0
    }

    fn prepare_exposure_inner(
        &mut self,
        device: &wgpu::Device,
        settings: &PostSettings,
        zones: Option<&mut ZoneExposure>,
    ) -> (f32, f32) {
        self.hist_frame = self.hist_frame.saturating_add(1);
        self.meter = settings.meter;
        let pump_started = Instant::now();
        let _readback = tracing::info_span!("readbacks").entered();
        self.pump_histogram(device);
        let readback_ms = pump_started.elapsed().as_secs_f32() * 1000.0;
        drop(_readback);
        (self.exposure_for(settings, zones), readback_ms)
    }

    /// The exposure multiplier of a frame from the meter as it stands.
    ///
    /// Exposure uses a histogram from at least two frames ago. Until that sample exists, the
    /// initial EV100 keeps the first frames stable. With `zones` (the main view) auto exposure
    /// goes through [`ZoneExposure::apply`]: with no zone target and no eye to hand back it
    /// returns the plain EV unchanged, so the multiplier is bit for bit the one without zones
    /// (ADR-322). Manual exposure ignores zones.
    fn exposure_for(&self, settings: &PostSettings, zones: Option<&mut ZoneExposure>) -> f32 {
        if !settings.auto_exposure {
            let exposure = settings.exposure_scale();
            if let Some(zones) = zones {
                // EV100 of the fixed exposure, for the report and the log only.
                zones.apply_manual(-(1.2 * exposure).log2());
            }
            return exposure;
        }
        let ev = match self.smoothed_luminance {
            Some(luminance) => settings.auto_exposure_ev100_with_coverage(luminance, self.smoothed_bright),
            None => settings.auto_exposure_initial_ev100(),
        };
        let Some(zones) = zones else {
            return exposure_from_ev100(ev);
        };
        let latest_ev = self.latest_sample.map_or(ev, |(luminance, bright)| {
            settings.auto_exposure_ev100_with_coverage(luminance, bright)
        });
        exposure_from_ev100(zones.apply(MeterReading {
            ev,
            latest_ev,
            compensation: settings.exposure_compensation,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        hdr: &wgpu::TextureView,
        width: u32,
        height: u32,
        settings: &PostSettings,
        out: &wgpu::TextureView,
        out_format: wgpu::TextureFormat,
        viewport: [f32; 4],
        display_headroom: Option<f32>,
        aa: PostAa,
    ) -> Result<f32, RenderError> {
        let _crumb = crate::breadcrumbs::frame_pass("post");
        self.ensure_base(device)?;
        self.ensure_state(device, width, height, settings);
        let (exposure, readback_ms) = match self.prepared.take() {
            Some(prepared) => prepared,
            None => self.prepare_exposure_inner(device, settings, None),
        };
        let wb = settings.grade.white_balance_gains();
        let g = &settings.grade;
        self.grade = [
            [wb[0], wb[1], wb[2], 0.0],
            [g.lift[0], g.lift[1], g.lift[2], 0.0],
            [g.gamma[0], g.gamma[1], g.gamma[2], 0.0],
            [
                g.gain[0],
                g.gain[1],
                g.gain[2],
                if g.is_neutral() { 0.0 } else { 1.0 },
            ],
        ];
        self.meter = settings.meter;
        self.ensure_lut(device, belt, encoder, settings.lut.as_deref());
        if let Some(lut) = settings.lut.as_deref() {
            self.grade[1][3] = 1.0;
            self.grade[2][3] = lut.size() as f32;
        }
        let inv = [1.0 / width as f32, 1.0 / height as f32];
        let mips = if settings.bloom {
            settings.bloom_mips.clamp(1, 5)
        } else {
            0
        };

        // TAA shares the caller's encoder. The histogram samples its output.
        let src_after_taa: wgpu::TextureView = if settings.taa {
            let taa_view = self
                .state
                .as_ref()
                .expect("post state")
                .taa_composite
                .view
                .clone();
            self.queue_pass(
                &taa_view,
                hdr,
                hdr,
                [0.0, MODE_TAA, inv[0], inv[1]],
                [0.0; 4],
                wgpu::TextureFormat::Rgba16Float,
                false,
                [0.0; 4],
            );
            taa_view
        } else {
            hdr.clone()
        };

        // Histogram of this frame's HDR (after TAA). The copy lands in a free
        // staging slot; the CPU reads it on a later frame. No slot means the
        // ring is still mapped, so this frame keeps the smoothed exposure.
        self.pending_hist = None;
        if settings.auto_exposure {
            self.queued.push(QueuedStep::Histogram(src_after_taa.clone()));
        }

        // --- Local exposure (off by default; a level look turns it on) ---
        let local = settings
            .local_exposure
            .map(crate::post::LocalExposure::sanitized)
            .filter(|l| !l.is_identity());
        self.local = match local {
            Some(l) => [
                [l.shadow_contrast, l.highlight_contrast, l.detail_strength, 1.0],
                crate::local_exposure::LocalExposurePass::grid_row(width, height),
            ],
            None => [[1.0, 1.0, 1.0, 0.0], [0.0; 4]],
        };
        if local.is_some() {
            self.queued.push(QueuedStep::LocalExposure {
                src: src_after_taa.clone(),
                exposure: exposure / self.pre_exposure(),
                wb,
            });
        }

        // --- Exposure ---
        let exposed_view = self.state.as_ref().expect("post state").exposed.view.clone();
        self.queue_pass(
            &exposed_view,
            &src_after_taa,
            &src_after_taa,
            [exposure / self.pre_exposure(), MODE_EXPOSURE, inv[0], inv[1]],
            [0.0; 4],
            wgpu::TextureFormat::Rgba16Float,
            false,
            [0.0; 4],
        );

        // --- Bloom (dual filter down, up, composite) ---
        let composite_view = if mips > 0 {
            let mut src = exposed_view.clone();
            let mut src_size = (width, height);
            for level in 0..mips as usize {
                let dest = self.state.as_ref().expect("post state").bloom[level].view.clone();
                let (dw, dh) = (
                    self.state.as_ref().expect("post state").bloom[level].width,
                    self.state.as_ref().expect("post state").bloom[level].height,
                );
                self.queue_pass(
                    &dest,
                    &src,
                    &src,
                    [0.0, MODE_DOWN, 1.0 / dw as f32, 1.0 / dh as f32],
                    [1.0 / src_size.0 as f32, 1.0 / src_size.1 as f32, 0.0, 0.0],
                    wgpu::TextureFormat::Rgba16Float,
                    false,
                    [0.0; 4],
                );
                src = dest;
                src_size = (dw, dh);
            }
            // Every pass below reads the lower-resolution input at half its pixel position
            // (`extra.xy` = that input's texel size), so the levels stay pixel-aligned.
            if mips > 1 {
                for index in (0..mips as usize - 1).rev() {
                    let state = self.state.as_ref().expect("post state");
                    let dest = state.bloom[mips as usize + index].view.clone();
                    let add = state.bloom[index].view.clone();
                    let (dw, dh) = (
                        state.bloom[mips as usize + index].width,
                        state.bloom[mips as usize + index].height,
                    );
                    self.queue_pass(
                        &dest,
                        &src,
                        &add,
                        [0.0, MODE_UP, 1.0 / dw as f32, 1.0 / dh as f32],
                        [1.0 / src_size.0 as f32, 1.0 / src_size.1 as f32, 0.0, 0.0],
                        wgpu::TextureFormat::Rgba16Float,
                        false,
                        [0.0; 4],
                    );
                    src = dest;
                    src_size = (dw, dh);
                }
            }
            let state = self.state.as_ref().expect("post state");
            // The TAA output in this target was consumed by the exposure pass above.
            let composite = state.taa_composite.view.clone();
            self.queue_pass(
                &composite,
                &exposed_view,
                &src,
                [
                    bloom_blend(settings.bloom_intensity),
                    MODE_COMPOSITE,
                    1.0 / state.taa_composite.width as f32,
                    1.0 / state.taa_composite.height as f32,
                ],
                [1.0 / src_size.0 as f32, 1.0 / src_size.1 as f32, 0.0, 0.0],
                wgpu::TextureFormat::Rgba16Float,
                false,
                [0.0; 4],
            );
            composite
        } else {
            exposed_view
        };

        // --- Tonemap ---
        // A float output target (a deep readout) keeps the display image in half floats, so
        // nothing is quantized to 8 bits before the blit (ADR-1404).
        let display_format = crate::deep_readout::display_image_format(out_format, display_headroom);
        let deep = display_format != wgpu::TextureFormat::Rgba8Unorm;
        // `wb.w` (otherwise unused) is the output dither in code values; only the tonemap reads it.
        self.grade[0][3] = if self.output_dither && !deep { 1.0 } else { 0.0 };
        let mut output = if deep {
            let state = self.state.as_mut().expect("post state");
            let (w, h) = (state.width, state.height);
            state
                .tonemapped_deep
                .get_or_insert_with(|| color_target(device, w, h, display_format, "tonemap deep"))
                .view
                .clone()
        } else {
            self.state.as_ref().expect("post state").tonemapped.view.clone()
        };
        self.queue_pass(
            &output,
            &composite_view,
            &composite_view,
            [settings.contrast.max(0.05), MODE_TONEMAP, inv[0], inv[1]],
            [
                match settings.tonemap {
                    crate::post::TonemapOperator::AgX => 0.0,
                    crate::post::TonemapOperator::AcesFitted => 1.0,
                    crate::post::TonemapOperator::Neutral => 2.0,
                },
                settings.vignette,
                settings.film_grain,
                settings.saturation.max(0.0),
            ],
            display_format,
            false,
            [0.0; 4],
        );

        // --- SMAA (instead of FXAA) ---
        if aa.smaa {
            let state = self.state.as_mut().expect("post state");
            let (w, h) = (state.width, state.height);
            let dest = state
                .fxaa
                .get_or_insert_with(|| color_target(device, w, h, wgpu::TextureFormat::Rgba8Unorm, "fxaa"))
                .view
                .clone();
            self.queued.push(QueuedStep::Smaa {
                src: output.clone(),
                dest: dest.clone(),
            });
            output = dest;
        } else if settings.fxaa {
            // --- FXAA ---
            let state = self.state.as_mut().expect("post state");
            let (w, h) = (state.width, state.height);
            let slot = if deep {
                &mut state.fxaa_deep
            } else {
                &mut state.fxaa
            };
            let dest = slot
                .get_or_insert_with(|| color_target(device, w, h, display_format, "fxaa"))
                .view
                .clone();
            self.queue_pass(
                &dest,
                &output,
                &output,
                [0.0, MODE_FXAA, inv[0], inv[1]],
                [0.0; 4],
                display_format,
                false,
                [0.0; 4],
            );
            output = dest;
        }

        // --- Blit (or SGSR) to the output target (decode sRGB when the target encodes, or to
        // linear paper-white units with expanded highlights for the HDR display stage) ---
        let decode = out_format.is_srgb() || display_headroom.is_some();
        let extra = [
            if decode { 1.0 } else { 0.0 },
            if viewport == [0.0; 4] { 0.0 } else { viewport[0] },
            if viewport == [0.0; 4] { 0.0 } else { viewport[1] },
            display_headroom.unwrap_or(0.0),
        ];
        match aa.sgsr_output {
            // SGSR replaces the blit and must honour the same `extra`: sRGB decode and,
            // when `extra.w` > 1, the HDR highlight expansion (`shaders/sgsr.wgsl`).
            Some((out_w, out_h)) => {
                self.queue_pass(
                    out,
                    &output,
                    &output,
                    [0.0, 0.0, 1.0 / out_w.max(1) as f32, 1.0 / out_h.max(1) as f32],
                    extra,
                    out_format,
                    false,
                    viewport,
                );
                if let Some(QueuedStep::Pass(pass)) = self.queued.last_mut() {
                    pass.program = Program::Sgsr;
                }
            }
            None => self.queue_pass(
                out,
                &output,
                &output,
                [0.0, MODE_BLIT, 1.0 / width as f32, 1.0 / height as f32],
                extra,
                out_format,
                true,
                viewport,
            ),
        }
        self.note_input_readers(hdr);
        self.record_queued(device, belt, encoder, width, height)?;
        Ok(readback_ms)
    }

    /// Remembers which queued step reads `hdr` first, for [`Self::prewarm_input`].
    fn note_input_readers(&mut self, hdr: &wgpu::TextureView) {
        let mut slot = 0;
        self.head_pass = None;
        self.hist_reads_input = false;
        for step in &self.queued {
            match step {
                QueuedStep::Pass(pass) => {
                    if self.head_pass.is_none() && pass.src == *hdr {
                        let add = (pass.add != *hdr).then(|| pass.add.clone());
                        self.head_pass = Some((slot, pass.nearest, add));
                    }
                    slot += 1;
                }
                QueuedStep::Histogram(view) => self.hist_reads_input |= view == hdr,
                _ => {}
            }
        }
    }

    /// Builds the bind groups the chain would create the first time it is handed `hdr` as its
    /// input, so a swap to a view it has not seen (a held-view reuse frame, ADR-1361) creates
    /// nothing mid-session.
    ///
    /// Uses the layout of the last recorded frame; does nothing before the first one. Cheap
    /// when the groups already exist (hash lookups). Returns how many groups it created.
    pub(crate) fn prewarm_input(&mut self, device: &wgpu::Device, hdr: &wgpu::TextureView) -> u32 {
        let mut created = 0;
        let device = &crate::device::TrackedDevice::observe(device);
        if let Some((slot, nearest, add)) = self.head_pass.clone()
            && let (Some(bgl), Some(state)) = (self.bgl.clone(), self.state.as_ref())
            && let Some(param_buf) = state.params.get(slot).cloned()
            && let Some(lut_view) = self
                .lut
                .as_ref()
                .map(|(_, view)| view.clone())
                .or_else(|| self.identity_lut.clone())
        {
            let sampler = if nearest {
                self.nearest.clone()
            } else {
                self.sampler.clone()
            };
            if let Some(sampler) = sampler {
                let grid_view = self.local_exposure.view(device, self.local[0][3] > 0.5);
                let add = add.as_ref().unwrap_or(hdr);
                let key = (
                    hdr.clone(),
                    add.clone(),
                    nearest,
                    param_buf.clone(),
                    lut_view.clone(),
                    grid_view.clone(),
                );
                let mut made = false;
                self.bg_cache.get_or_insert_with(key, || {
                    made = true;
                    make_post_bg(
                        device, &bgl, hdr, add, &sampler, &param_buf, &lut_view, &grid_view,
                    )
                });
                created += u32::from(made);
            }
        }
        if self.hist_reads_input
            && let Some(hist) = self.hist.as_ref()
        {
            for slot in &hist.slots {
                let mut made = false;
                self.hist_cache
                    .get_or_insert_with((hdr.clone(), slot.bins.clone()), || {
                        made = true;
                        make_hist_bg(device, &hist.bgl, hdr, &slot.bins)
                    });
                created += u32::from(made);
            }
        }
        created
    }

    /// Every view the fullscreen and histogram bind groups hold now (both of a fullscreen
    /// group's input views): the views this chain keeps alive. An audit for the frame's pooled
    /// inputs (ADR-7877): a texture the view's pool dropped renews the bind epoch, so none of
    /// its views stays here past the next record.
    #[cfg(test)]
    pub(crate) fn bound_input_views(&self) -> Vec<wgpu::TextureView> {
        let mut views: Vec<wgpu::TextureView> = Vec::new();
        for (input, add, ..) in self.bg_cache.keys() {
            views.push(input.clone());
            views.push(add.clone());
        }
        views.extend(self.hist_cache.keys().map(|(view, _)| view.clone()));
        views
    }

    /// Records the passes [`Self::queue_pass`] collected, in order: first every pass's
    /// uniform (its own buffer per position in the stack, so the bind groups stay
    /// cached) as one block of copies, then the passes. wgpu opens a command buffer for
    /// each group of copies between passes; one block instead of a copy before each
    /// pass saves a dozen per frame.
    fn record_queued(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        let mut steps = std::mem::take(&mut self.queued);
        let param_bufs = self.state.as_ref().expect("post state").params.clone();
        let mut slot = 0;
        for step in &steps {
            if let QueuedStep::Pass(pass) = step {
                let uniform = PostParams {
                    params: pass.params,
                    extra: pass.extra,
                    grade: self.grade,
                    local: self.local,
                };
                let buf = param_bufs.get(slot).ok_or_else(|| {
                    RenderError::Shader(format!("post stack has more than {PARAM_SLOTS} passes"))
                })?;
                belt.write(device, encoder, buf, 0, bytemuck::bytes_of(&uniform));
                slot += 1;
            }
            if let QueuedStep::Smaa { .. } = step {
                self.smaa.prepare(device, belt, encoder, width, height)?;
                self.smaa.write_uniform(device, belt, encoder);
            }
        }
        let mut slot = 0;
        for step in steps.drain(..) {
            match step {
                QueuedStep::Pass(pass) => {
                    self.record_pass(device, encoder, &pass, &param_bufs[slot]);
                    slot += 1;
                }
                QueuedStep::Histogram(view) => {
                    self.pending_hist = self.record_histogram(device, belt, encoder, &view, width, height)?;
                }
                QueuedStep::Smaa { src, dest } => self.smaa.record(device, encoder, &src, &dest),
                QueuedStep::LocalExposure { src, exposure, wb } => {
                    self.local_exposure
                        .record(device, belt, encoder, &src, width, height, exposure, wb)?;
                }
            }
        }
        // Keep the allocation for the next frame.
        self.queued = steps;
        Ok(())
    }

    /// Maps the histogram slot recorded by [`Self::record`].
    ///
    /// Call this after the frame's `submit`. The CPU reads the slot only once
    /// it has been mapped for [`READBACK_LATENCY`] frames.
    pub(crate) fn map_after_submit(&mut self) {
        if let Some(slot) = self.pending_hist.take() {
            self.map_histogram_slot(slot);
        }
    }

    /// Uploads `lut` as an `Rgba16Float` 3D texture when its content changed (identity 2³
    /// when there is none).
    fn ensure_lut(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        lut: Option<&crate::lut::ColorLut>,
    ) {
        let make = |size: u32, label: &str| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: size,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let upload = |texture: &wgpu::Texture,
                      belt: &mut crate::staging::StagingBelt,
                      encoder: &mut wgpu::CommandEncoder,
                      lut: &crate::lut::ColorLut| {
            let n = lut.size();
            let row = n * 8;
            let aligned = row.div_ceil(256) * 256;
            let bytes = crate::ibl::rgb_to_rgba16f(lut.data());
            let mut padded = vec![0u8; (aligned * n * n) as usize];
            for slab in 0..(n * n) as usize {
                let src = slab * row as usize;
                let dst = slab * aligned as usize;
                padded[dst..dst + row as usize].copy_from_slice(&bytes[src..src + row as usize]);
            }
            let staging = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("post lut upload"),
                size: padded.len() as u64,
                usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            belt.write(device, encoder, &staging, 0, &padded);
            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(aligned),
                        rows_per_image: Some(n),
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: n,
                    height: n,
                    depth_or_array_layers: n,
                },
            );
        };
        if self.identity_lut.is_none() {
            let identity = crate::lut::ColorLut::identity(2);
            let texture = make(2, "post lut identity");
            upload(&texture, belt, encoder, &identity);
            self.identity_lut = Some(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        }
        match lut {
            None => self.lut = None,
            Some(lut) if self.lut.as_ref().is_some_and(|(key, _)| *key == lut.key()) => {}
            Some(lut) => {
                let reuse = self
                    .lut_texture
                    .as_ref()
                    .filter(|(size, _)| *size == lut.size())
                    .zip(self.lut.as_ref().map(|(_, view)| view.clone()));
                if let Some(((_, texture), view)) = reuse {
                    upload(texture, belt, encoder, lut);
                    self.lut = Some((lut.key(), view));
                } else {
                    let texture = make(lut.size(), "post lut");
                    upload(&texture, belt, encoder, lut);
                    self.lut = Some((
                        lut.key(),
                        texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    ));
                    self.lut_texture = Some((lut.size(), texture));
                }
            }
        }
    }

    fn ensure_base(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.module.is_some() {
            return Ok(());
        }
        let module = shader(device, "post", include_str!("../shaders/post.wgsl"), "post.wgsl")?;
        let sgsr_module = shader(device, "sgsr", include_str!("../shaders/sgsr.wgsl"), "sgsr.wgsl")?;
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post bgl"),
            entries: &[
                tex_entry(0),
                samp_entry(1),
                uniform_entry(2),
                tex_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
                // Local exposure's bilateral grid (`local_exposure.wgsl`; 1³ while off).
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("post layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("post sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("post blit sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        self.module = Some(module);
        self.sgsr_module = Some(sgsr_module);
        self.bgl = Some(bgl);
        self.layout = Some(layout);
        self.sampler = Some(sampler);
        self.nearest = Some(nearest);
        Ok(())
    }

    fn ensure_state(&mut self, device: &wgpu::Device, width: u32, height: u32, settings: &PostSettings) {
        let mips = if settings.bloom {
            settings.bloom_mips.clamp(1, 5)
        } else {
            0
        };
        if self
            .state
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height && s.mips == mips)
        {
            return;
        }
        let mut bloom = Vec::new();
        for (level, (w, h)) in bloom_level_sizes(width, height, mips).into_iter().enumerate() {
            bloom.push(color_target(
                device,
                w,
                h,
                wgpu::TextureFormat::Rgba16Float,
                &format!("bloom down {level}"),
            ));
        }
        for index in 0..mips.saturating_sub(1) as usize {
            let (bw, bh) = (bloom[index].width, bloom[index].height);
            bloom.push(color_target(
                device,
                bw,
                bh,
                wgpu::TextureFormat::Rgba16Float,
                &format!("bloom up {index}"),
            ));
        }
        let params = (0..PARAM_SLOTS)
            .map(|_| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("post params"),
                    size: size_of::<PostParams>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .collect();
        self.state = Some(PostState {
            width,
            height,
            mips,
            taa_composite: color_target(
                device,
                width,
                height,
                wgpu::TextureFormat::Rgba16Float,
                "taa hdr / bloom composite",
            ),
            exposed: color_target(device, width, height, wgpu::TextureFormat::Rgba16Float, "exposed"),
            tonemapped: color_target(device, width, height, wgpu::TextureFormat::Rgba8Unorm, "tonemap"),
            fxaa: None,
            tonemapped_deep: None,
            fxaa_deep: None,
            bloom,
            params,
        });
        self.bg_cache.clear();
    }

    /// Records one fullscreen pass into `encoder` (`src`/`add` = the two textures
    /// the bind group exposes; most modes read only `src`).
    ///
    /// `viewport` is the pixel rectangle to draw into; `[0.0; 4]` means the whole
    /// target with a clear (internal passes). A non-empty rectangle draws exactly
    /// that rect and *loads* the destination (multi-view frames must preserve
    /// what earlier views drew outside the rect).
    #[allow(clippy::too_many_arguments)]
    fn queue_pass(
        &mut self,
        dest_view: &wgpu::TextureView,
        src: &wgpu::TextureView,
        add: &wgpu::TextureView,
        params: [f32; 4],
        extra: [f32; 4],
        format: wgpu::TextureFormat,
        nearest: bool,
        viewport: [f32; 4],
    ) {
        self.queued.push(QueuedStep::Pass(QueuedPass {
            dest: dest_view.clone(),
            src: src.clone(),
            add: add.clone(),
            params,
            extra,
            format,
            nearest,
            viewport,
            program: Program::Post,
        }));
    }

    /// Records one queued pass; its uniform is already in `param_buf`.
    fn record_pass(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        pass: &QueuedPass,
        param_buf: &wgpu::Buffer,
    ) {
        let QueuedPass {
            dest: dest_view,
            src,
            add,
            format,
            nearest,
            viewport,
            program,
            ..
        } = pass;
        let (format, nearest, viewport, program) = (*format, *nearest, *viewport, *program);
        let device = &crate::device::TrackedDevice::observe(device);
        let module = match program {
            Program::Post => self.module.clone().expect("post module"),
            Program::Sgsr => self.sgsr_module.clone().expect("sgsr module"),
        };
        let bgl = self.bgl.clone().expect("post bgl");
        let layout = self.layout.clone().expect("post layout");
        let sampler = self.sampler.clone().expect("post sampler");
        let nearest_sampler = self.nearest.clone().expect("post blit sampler");
        let pipeline = self
            .pipelines
            .entry((program, format))
            .or_insert_with(|| pipeline(device, &layout, &module, format))
            .clone();
        let param_buf = param_buf.clone();
        let chosen = if nearest { nearest_sampler } else { sampler };
        let src_key = src.clone();
        let add_key = add.clone();
        let lut_view = self
            .lut
            .as_ref()
            .map(|(_, view)| view.clone())
            .or_else(|| self.identity_lut.clone())
            .expect("post lut identity");
        let grid_view = self.local_exposure.view(device, self.local[0][3] > 0.5);
        let bg = self.bg_cache.get_or_insert_with(
            (
                src_key,
                add_key,
                nearest,
                param_buf.clone(),
                lut_view.clone(),
                grid_view.clone(),
            ),
            || make_post_bg(device, &bgl, src, add, &chosen, &param_buf, &lut_view, &grid_view),
        );
        let mut render = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("post pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dest_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: if viewport == [0.0; 4] {
                        wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                    } else {
                        wgpu::LoadOp::Load
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            ..Default::default()
        });
        if viewport != [0.0; 4] {
            render.set_viewport(viewport[0], viewport[1], viewport[2], viewport[3], 0.0, 1.0);
            render.set_scissor_rect(
                viewport[0] as u32,
                viewport[1] as u32,
                viewport[2] as u32,
                viewport[3] as u32,
            );
        }
        render.counted_set_pipeline(&pipeline);
        render.counted_set_bind_group(0, &bg, &[]);
        render.counted_draw(0..3, 0..1);
    }

    fn ensure_hist(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.hist.is_some() {
            return Ok(());
        }
        // Metal allows 31 threadgroup arguments per entry point: the 64 scalar workgroup bins
        // (a SPIR-V workaround) become one array there.
        let mut defs = HashMap::new();
        if cfg!(target_os = "macos") {
            defs.insert(
                "HIST_WORKGROUP_ARRAY".to_string(),
                naga_oil::compose::ShaderDefValue::Bool(true),
            );
        }
        let module = shader_with_defs(
            device,
            "exposure hist",
            include_str!("../shaders/exposure_hist.wgsl"),
            "exposure_hist.wgsl",
            defs,
        )?;
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hist bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let device = &crate::device::TrackedDevice::observe(device);
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hist layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("hist"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("cs_hist"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        let slot = |index: usize| HistSlot {
            bins: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(match index {
                    0 => "hist bins 0",
                    1 => "hist bins 1",
                    _ => "hist bins 2",
                }),
                // 64 bins, the meter's centre weight (f32 bits) in slot 64, the HDR
                // pre-exposure (f32 bits) in slot 65.
                size: 66 * 4,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            staging: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(match index {
                    0 => "hist read 0",
                    1 => "hist read 1",
                    _ => "hist read 2",
                }),
                size: 256,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            state: Arc::new(AtomicU8::new(ST_IDLE)),
            submitted_frame: 0,
        };
        self.hist = Some(Histogram {
            bgl,
            pipeline,
            slots: [slot(0), slot(1), slot(2)],
        });
        Ok(())
    }

    /// Applies every histogram that has been mapped for at least [`READBACK_LATENCY`] frames.
    fn pump_histogram(&mut self, device: &wgpu::Device) {
        let device = &crate::device::TrackedDevice::observe(device);
        if device.poll(wgpu::PollType::Poll).is_err() {
            tracing::warn!(target: "regia_render", "histogram poll failed");
        }
        let frame = self.hist_frame;
        // Deterministic metering: a histogram due this frame (READBACK_LATENCY frames old) is waited for, so
        // every sample joins the meter on the same frame in every run instead of whenever its map lands.
        if self.wait_readbacks
            && let Some(hist) = self.hist.as_ref()
            && hist.slots.iter().any(|slot| {
                slot.state.load(Ordering::Acquire) == ST_INFLIGHT
                    && frame.saturating_sub(slot.submitted_frame) >= READBACK_LATENCY
            })
            && device.poll(wgpu::PollType::wait_indefinitely()).is_err()
        {
            tracing::warn!(target: "regia_render", "histogram wait failed");
        }
        let mut samples = Vec::new();
        if let Some(hist) = self.hist.as_mut() {
            let mut ready = Vec::new();
            for (index, slot) in hist.slots.iter().enumerate() {
                let state = slot.state.load(Ordering::Acquire);
                if state == ST_FAILED {
                    slot.state.store(ST_IDLE, Ordering::Release);
                    continue;
                }
                if slot_is_consumable(state, slot.submitted_frame, frame, READBACK_LATENCY) {
                    ready.push((slot.submitted_frame, index));
                }
            }
            ready.sort_by_key(|pair| pair.0);
            for (_, index) in ready {
                let slot = &hist.slots[index];
                let Ok(mapped) = slot.staging.slice(..).get_mapped_range() else {
                    slot.state.store(ST_IDLE, Ordering::Release);
                    continue;
                };
                let counts: [u32; 64] = bytemuck::pod_read_unaligned(&mapped);
                drop(mapped);
                slot.staging.unmap();
                slot.state.store(ST_IDLE, Ordering::Release);
                if slot.submitted_frame <= self.hist_discard_through {
                    continue;
                }
                samples.push((
                    average_log_luminance(
                        &counts,
                        f64::from(self.meter.low_percentile),
                        f64::from(self.meter.high_percentile),
                    ),
                    readback::bright_fraction(&counts, self.meter.bright_threshold),
                ));
            }
        }
        let mut smoothed = self.smoothed_luminance;
        if let Some(&newest) = samples.last() {
            self.latest_sample = Some(newest);
        }
        for (sample, bright) in samples {
            // The first sample adopts the coverage as it adopts the luminance.
            let alpha = if smoothed.is_some() {
                readback::LUMINANCE_SMOOTH_ALPHA
            } else {
                1.0
            };
            self.smoothed_bright += (bright - self.smoothed_bright) * alpha;
            smoothed = Some(smooth_luminance(
                smoothed,
                sample,
                readback::LUMINANCE_SMOOTH_ALPHA,
            ));
        }
        self.smoothed_luminance = smoothed;
    }

    /// Records the histogram dispatch and the copy into `encoder`.
    ///
    /// Returns the slot to map after the encoder is submitted. `None` means every
    /// slot is still in flight; exposure stays where the last sample left it.
    fn record_histogram(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        hdr: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Result<Option<usize>, RenderError> {
        self.ensure_hist(device)?;
        let frame = self.hist_frame;
        let device = &crate::device::TrackedDevice::observe(device);
        // Bind groups for every ring slot at once: which slot is idle depends on how far the
        // GPU lags, so building them lazily created one mid-session (a warm frame must not).
        if let Some(hist) = self.hist.as_ref() {
            for slot in &hist.slots {
                let layout = &hist.bgl;
                self.hist_cache
                    .get_or_insert_with((hdr.clone(), slot.bins.clone()), || {
                        make_hist_bg(device, layout, hdr, &slot.bins)
                    });
            }
        }
        let (index, slot_bins, slot_staging, layout, pipeline) = {
            let Some(hist) = self.hist.as_ref() else {
                return Ok(None);
            };
            let Some(index) = hist
                .slots
                .iter()
                .position(|slot| slot.state.load(Ordering::Acquire) == ST_IDLE)
            else {
                return Ok(None);
            };
            let slot = &hist.slots[index];
            (
                index,
                slot.bins.clone(),
                slot.staging.clone(),
                hist.bgl.clone(),
                hist.pipeline.clone(),
            )
        };
        let mut zeroed = [0u8; 264];
        zeroed[256..260].copy_from_slice(&self.meter.center_weight.clamp(0.0, 1.0).to_le_bytes());
        // Slot 65: the HDR input's pre-exposure; the meter bins radiance.
        zeroed[260..].copy_from_slice(&self.pre_exposure().to_le_bytes());
        belt.write(device, encoder, &slot_bins, 0, &zeroed);
        let hdr_key = hdr.clone();
        let bg = self
            .hist_cache
            .get_or_insert_with((hdr_key, slot_bins.clone()), || {
                make_hist_bg(device, &layout, hdr, &slot_bins)
            });
        {
            let mut compute = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("hist"),
                timestamp_writes: None,
            });
            compute.counted_set_pipeline(&pipeline);
            compute.counted_set_bind_group(0, &bg, &[]);
            compute.counted_dispatch(width.div_ceil(16), height.div_ceil(16), 1);
        }
        encoder.copy_buffer_to_buffer(&slot_bins, 0, &slot_staging, 0, 256);
        drop(bg);
        let hist = self
            .hist
            .as_mut()
            .ok_or_else(|| RenderError::Readback("histogram state missing after creation".into()))?;
        hist.slots[index].submitted_frame = frame;
        Ok(Some(index))
    }

    fn map_histogram_slot(&self, index: usize) {
        let Some(slot) = self.hist.as_ref().and_then(|hist| hist.slots.get(index)) else {
            return;
        };
        let state = Arc::clone(&slot.state);
        state.store(ST_INFLIGHT, Ordering::Release);
        crate::frame_stats::note_map_async();
        slot.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                readback::store_map_result(&state, result.is_ok());
            });
    }
}

/// Size of each bloom level of a `width × height` view: each is the one above halved, rounded
/// up, so texel `i` of a level covers texels `2i` and `2i + 1` of the level above whatever the
/// parity of its size (ADR-1152). Rounding down instead dropped the last row or column of an odd
/// level and stretched the rest over it, which made a highlight's glow gain or lose up to 3 % of
/// its energy, and drift by up to a pixel, depending on where it sat.
pub(crate) fn bloom_level_sizes(width: u32, height: u32, mips: u32) -> Vec<(u32, u32)> {
    let (mut w, mut h) = (width.max(1), height.max(1));
    (0..mips)
        .map(|_| {
            w = w.div_ceil(2);
            h = h.div_ceil(2);
            (w, h)
        })
        .collect()
}

/// The composite's blend toward the bloom: `intensity` clamped to 0–1, and 0 (no bloom) when it is
/// not finite (`f32::clamp` keeps a NaN, and a NaN blend turned every pixel of the frame NaN).
fn bloom_blend(intensity: f32) -> f32 {
    if intensity.is_finite() {
        intensity.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn color_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> Tex {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    crate::resource_inspector::track(label, &texture);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { view, width, height }
}

fn pipeline(
    device: &crate::device::TrackedDevice,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("post pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

fn shader(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    file: &str,
) -> Result<wgpu::ShaderModule, RenderError> {
    shader_with_defs(device, label, source, file, HashMap::new())
}

fn shader_with_defs(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    file: &str,
    defs: HashMap<String, naga_oil::compose::ShaderDefValue>,
) -> Result<wgpu::ShaderModule, RenderError> {
    let module = compose_source(source, file, defs)?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    }))
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn samp_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

#[cfg(test)]
#[path = "post_effects_tests.rs"]
mod post_effects_tests;
#[cfg(test)]
mod zone_exposure_tests {
    use super::*;
    use crate::exposure_zones::{EvRange, EvTarget, ZoneExposureMode};
    use crate::post::ev100_from_luminance;

    /// Scene luminance (cd/m²) the meter keys to `ev` EV100 (`ev100_from_luminance` inverted).
    fn luminance_for_ev(ev: f32) -> f32 {
        0.18 * 1.2 * ev.exp2()
    }

    /// Feeds one histogram sample the way `pump_histogram` does.
    fn feed(chain: &mut PostChain, sample: f32, bright: f32) {
        let alpha = if chain.smoothed_luminance.is_some() {
            readback::LUMINANCE_SMOOTH_ALPHA
        } else {
            1.0
        };
        chain.smoothed_bright += (bright - chain.smoothed_bright) * alpha;
        chain.latest_sample = Some((sample, bright));
        chain.smoothed_luminance = Some(smooth_luminance(
            chain.smoothed_luminance,
            sample,
            readback::LUMINANCE_SMOOTH_ALPHA,
        ));
    }

    /// The pre-ADR-322 auto exposure, written out.
    fn plain(chain: &PostChain, settings: &PostSettings) -> f32 {
        match chain.smoothed_luminance {
            Some(luminance) => exposure_from_ev100(
                settings.auto_exposure_ev100_with_coverage(luminance, chain.smoothed_bright),
            ),
            None => exposure_from_ev100(settings.auto_exposure_initial_ev100()),
        }
    }

    #[test]
    fn without_a_zone_target_the_exposure_sequence_is_unchanged_bit_for_bit() {
        let mut settings = PostSettings {
            exposure_compensation: 0.3,
            ..PostSettings::default()
        };
        settings.meter.bright_threshold = 400.0;
        settings.meter.bright_bias_ev = 1.0;
        let mut chain = PostChain::default();
        let mut zones = ZoneExposure::default();
        // Before the first histogram, then a sunlit drive into a dark bore and out again.
        let drive = [
            14.5_f32, 14.5, 14.2, 9.0, 5.0, 5.0, 5.0, 4.0, 12.0, 14.5, 16.0, -3.0, 14.5,
        ];
        let first = chain.exposure_for(&settings, Some(&mut zones));
        assert!(first.to_bits() == plain(&chain, &settings).to_bits());
        for (frame, ev) in drive.iter().enumerate() {
            feed(&mut chain, luminance_for_ev(*ev), (frame % 3) as f32 * 0.3);
            let want = plain(&chain, &settings);
            let with = chain.exposure_for(&settings, Some(&mut zones));
            let without = chain.exposure_for(&settings, None);
            assert!(
                with.to_bits() == want.to_bits(),
                "frame {frame}: {with} vs {want}"
            );
            assert!(without.to_bits() == want.to_bits(), "frame {frame}");
            let report = zones.report().expect("report");
            assert_eq!(report.mode, ZoneExposureMode::Meter);
            assert!(!report.adapting());
        }
    }

    /// ADR-7969: the exposure the frame applies (`exposure_for`, with and without the main
    /// view's zones) follows `PostSettings::band_bounds_final_exposure`: with the switch on, a
    /// sunlit-ice drive never leaves the band; with it off, the sequence is today's.
    #[test]
    fn the_applied_exposure_honors_the_band_switch() {
        let mut settings = PostSettings {
            auto_exposure_min_ev100: 10.5,
            auto_exposure_max_ev100: 11.5,
            exposure_compensation: 1.0,
            ..PostSettings::default()
        };
        settings.meter.bright_threshold = 250.0;
        settings.meter.bright_bias_ev = 1.0;
        let drive = [12.0_f32, 12.5, 13.0, 13.5, 13.5, 13.5, 13.5, 13.5];
        for switch in [false, true] {
            settings.band_bounds_final_exposure = switch;
            let mut chain = PostChain::default();
            let mut zones = ZoneExposure::default();
            let first = chain.exposure_for(&settings, Some(&mut zones));
            let first_ev = ev100_from_luminance(0.18 / first);
            for (frame, ev) in drive.iter().enumerate() {
                feed(&mut chain, luminance_for_ev(*ev), 1.0);
                let with = chain.exposure_for(&settings, Some(&mut zones));
                let without = chain.exposure_for(&settings, None);
                assert!(with.to_bits() == without.to_bits(), "frame {frame}");
                assert!(with.to_bits() == plain(&chain, &settings).to_bits());
                let applied = ev100_from_luminance(0.18 / with);
                if switch {
                    assert!(
                        (10.5 - 1e-3..=11.5 + 1e-3).contains(&applied),
                        "frame {frame}: EV100 {applied} left the band 10.5..11.5"
                    );
                } else {
                    assert!((applied - 9.5).abs() < 1e-3, "frame {frame}: {applied}");
                }
            }
            // Before the first histogram: the band ceiling less the compensation, in the band.
            assert!((first_ev - 10.5).abs() < 1e-3, "first frame {first_ev}");
        }
    }

    #[test]
    fn manual_exposure_ignores_the_zone_target() {
        let settings = PostSettings {
            auto_exposure: false,
            exposure_ev: 11.0,
            ..PostSettings::default()
        };
        let mut chain = PostChain::default();
        feed(&mut chain, luminance_for_ev(5.0), 0.0);
        let mut zones = ZoneExposure::default();
        zones.set_target(Some(EvTarget::from_range(EvRange::point(8.5))));
        zones.set_frame_dt(Some(1.0 / 30.0));
        let exposure = chain.exposure_for(&settings, Some(&mut zones));
        assert!(exposure.to_bits() == settings.exposure_scale().to_bits());
        let report = zones.report().expect("report");
        assert_eq!(report.mode, ZoneExposureMode::Manual);
        assert!((report.ev - 11.0).abs() < 1.0e-4, "{}", report.ev);
    }

    #[test]
    fn a_zone_clamps_the_newest_histogram_and_adapts_from_the_applied_ev() {
        let settings = PostSettings::default();
        let mut chain = PostChain::default();
        feed(&mut chain, luminance_for_ev(14.5), 0.0);
        // The bore: the newest histogram meters EV 5; the smoothed meter still says ~13.
        feed(&mut chain, luminance_for_ev(5.0), 0.0);
        let meter_ev =
            settings.auto_exposure_ev100_with_coverage(chain.smoothed_luminance.unwrap_or(0.0), 0.0);
        assert!(meter_ev > 12.0, "{meter_ev}");
        let mut zones = ZoneExposure::default();
        zones.set_target(Some(EvTarget::from_range(EvRange::point(8.5))));
        zones.set_frame_dt(Some(1.0 / 30.0));
        let exposure = chain.exposure_for(&settings, Some(&mut zones));
        let report = zones.report().expect("report");
        assert_eq!(report.mode, ZoneExposureMode::Zone);
        assert!(
            (report.goal - 8.5).abs() < 1.0e-5,
            "EV 5 is clamped up to the tube: {}",
            report.goal
        );
        assert!((report.meter_ev - meter_ev).abs() < 1.0e-5);
        assert!(
            report.ev < meter_ev && report.ev > 8.5,
            "one step from the applied EV: {}",
            report.ev
        );
        assert!((ev100_from_luminance(0.18 / exposure) - report.ev).abs() < 1.0e-3);
        assert!(report.adapting());
    }
}

/// The fullscreen post bind group for `src` and `add` (one group per source pair, parameter
/// buffer, LUT and local-exposure grid).
#[allow(clippy::too_many_arguments)]
fn make_post_bg(
    device: &crate::device::TrackedDevice,
    bgl: &wgpu::BindGroupLayout,
    src: &wgpu::TextureView,
    add: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
    param_buf: &wgpu::Buffer,
    lut_view: &wgpu::TextureView,
    grid_view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("post bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(src),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: param_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(add),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(lut_view),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::TextureView(grid_view),
            },
        ],
    })
}

/// The histogram compute bind group for the HDR view `hdr` and one ring slot's bin buffer.
fn make_hist_bg(
    device: &crate::device::TrackedDevice,
    layout: &wgpu::BindGroupLayout,
    hdr: &wgpu::TextureView,
    bins: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("hist bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(hdr),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: bins.as_entire_binding(),
            },
        ],
    })
}

#[cfg(test)]
#[path = "post_pass_prewarm_tests.rs"]
mod tests;
