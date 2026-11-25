// SPDX-License-Identifier: MPL-2.0
//! Test tooling for the vendor seam (ADR-2679), hidden from the docs and never reached unless a
//! test installs it with [`install_binding_factory`].
//!
//! - [`ScriptedVendor`]: a binding with no runtime. It answers any [`VendorRuntime`] state and any
//!   render range, can fail on cue, and upscales by a known filter (nearest render texel times a
//!   gain), so the frame's vendor path runs on any GPU, lavapipe included.
//! - [`Recorder`]: wraps any binding, the scripted one or a real one, and logs everything the seam
//!   hands it, optionally with a readback of the input images.
//! - [`check_eval`]: the conformance checklist ([`CHECKLIST`]) over one logged evaluate. A new vendor
//!   binding passes the same list: `crates/regia_render/tests/vendor_conformance.rs` runs it against
//!   the binding named by `REGIA_VENDOR_CONFORMANCE` (`book/agents/upscalers.md`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use wgpu::util::DeviceExt as _;

use super::dlss::GpuBinding;
use super::{
    FeatureDesc, FeatureFlags, FeatureHandle, OptimalSettings, VendorEvalParams, VendorEvaluate,
    VendorRuntimeApi, VulkanExtensionRequest,
};
use crate::upscaler::plugin::{
    GpuTarget, GpuUpscaleContext, InputContract, InputConventions, UpscaleError, UpscaleFrame, UpscaleParams,
    UpscaleTarget, VendorRuntime,
};
use crate::upscaler::{UpscaleQuality, VendorUpscaler};

const WGSL: &str = include_str!("scripted_vendor.wgsl");

/// Makes the binding the frame's slot uses for a vendor; `None` leaves the vendor to this build's.
pub type BindingFactory =
    Arc<dyn Fn(VendorUpscaler, &wgpu::Device) -> Option<Box<dyn GpuBinding>> + Send + Sync>;

static FACTORY: Mutex<Option<BindingFactory>> = Mutex::new(None);

/// Removes the factory when dropped, also when a test panics.
#[must_use = "dropping the guard removes the factory"]
#[derive(Debug)]
pub struct BindingFactoryGuard(());

impl Drop for BindingFactoryGuard {
    fn drop(&mut self) {
        *FACTORY.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Every frame slot of this process that starts a binding from now on asks `factory` first. The
/// slot of a running renderer keeps its binding until the selection changes. Tests only; a test
/// binary serializes its renderers, as it does for device creation.
pub fn install_binding_factory(
    factory: impl Fn(VendorUpscaler, &wgpu::Device) -> Option<Box<dyn GpuBinding>> + Send + Sync + 'static,
) -> BindingFactoryGuard {
    *FACTORY.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(factory));
    BindingFactoryGuard(())
}

/// The installed factory's binding for `vendor`, if any.
pub(super) fn binding_override(vendor: VendorUpscaler, device: &wgpu::Device) -> Option<Box<dyn GpuBinding>> {
    let factory = FACTORY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(Arc::clone)?;
    factory(vendor, device)
}

/// What a [`ScriptedVendor`] answers and does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScriptConfig {
    /// The vendor it stands in for.
    pub vendor: VendorUpscaler,
    /// The state it reports.
    pub runtime: VendorRuntime,
    /// Its contract.
    pub contract: InputContract,
    /// `(preferred, min, max)` render permille of the display per axis, whatever the mode; `None`
    /// answers ADR-324's rule ([`OptimalSettings::from_rule`]).
    pub range_permille: Option<(u32, u32, u32)>,
    /// The filter's gain: 1 is the nearest render texel, 0 a black image.
    pub gain: f32,
    /// Creates to refuse before the first success.
    pub fail_creates: u32,
    /// Evaluates to fail before the first success.
    pub fail_evaluates: u32,
}

impl ScriptConfig {
    /// A ready runtime with the vendor contract, the rule's sizes and a gain of 1.
    #[must_use]
    pub const fn ready(vendor: VendorUpscaler) -> Self {
        Self {
            vendor,
            runtime: VendorRuntime::Ready,
            contract: InputContract::VENDOR,
            range_permille: None,
            gain: 1.0,
            fail_creates: 0,
            fail_evaluates: 0,
        }
    }
}

/// `value × permille ÷ 1000` rounded, halves up.
fn scaled(value: u32, permille: u32) -> u32 {
    let scaled = (u64::from(value) * u64::from(permille) * 2 + 1000) / 2000;
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

/// The compute module, shared by the filter and the capture.
fn module(device: &wgpu::Device) -> Result<wgpu::ShaderModule, UpscaleError> {
    let composed = crate::shader_lib::compose_source(WGSL, "scripted_vendor.wgsl", HashMap::new())
        .map_err(|error| UpscaleError::Backend(format!("scripted vendor shader: {error}")))?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("scripted vendor"),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(composed)),
    }))
}

fn pipeline(
    device: &wgpu::Device,
    module: &wgpu::ShaderModule,
    entry_point: &'static str,
) -> wgpu::ComputePipeline {
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry_point),
        layout: None,
        module,
        entry_point: Some(entry_point),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Sizes {
    render: [u32; 2],
    display: [u32; 2],
    gain: f32,
    has_reactive: u32,
    pad: [u32; 2],
}

fn sizes_buffer(device: &wgpu::Device, sizes: Sizes) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("scripted vendor sizes"),
        contents: bytemuck::bytes_of(&sizes),
        usage: wgpu::BufferUsages::UNIFORM,
    })
}

/// A vendor binding with no runtime behind it. See the module docs.
#[derive(Debug)]
pub struct ScriptedVendor {
    config: ScriptConfig,
    next: u64,
    filter: Option<wgpu::ComputePipeline>,
}

impl ScriptedVendor {
    /// A binding that follows `config`.
    #[must_use]
    pub const fn new(config: ScriptConfig) -> Self {
        Self {
            config,
            next: 1,
            filter: None,
        }
    }
}

impl VendorRuntimeApi for ScriptedVendor {
    fn vendor(&self) -> VendorUpscaler {
        self.config.vendor
    }

    fn runtime(&self) -> VendorRuntime {
        self.config.runtime
    }

    fn contract(&self) -> InputContract {
        self.config.contract
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        Ok(VulkanExtensionRequest::default())
    }

    fn optimal_settings(
        &self,
        display: (u32, u32),
        quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        if let Some(reason) = self.config.runtime.blocker() {
            return Err(UpscaleError::RuntimeUnavailable { reason });
        }
        let Some((preferred, min, max)) = self.config.range_permille else {
            return Ok(OptimalSettings::from_rule(display, quality));
        };
        let size = |permille: u32| (scaled(display.0, permille), scaled(display.1, permille));
        Ok(OptimalSettings {
            render_size: size(preferred),
            render_min: size(min),
            render_max: size(max),
            sharpness: 0.0,
        })
    }

    fn release_feature(&mut self, _feature: FeatureHandle) {}
}

impl VendorEvaluate<GpuTarget> for ScriptedVendor {
    fn create_feature(
        &mut self,
        _ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        _desc: &FeatureDesc,
        _settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        if self.config.fail_creates > 0 {
            self.config.fail_creates -= 1;
            return Err(UpscaleError::Backend("scripted create failure".into()));
        }
        let handle = FeatureHandle(self.next);
        self.next += 1;
        Ok(handle)
    }

    fn evaluate(
        &mut self,
        _feature: FeatureHandle,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        frame: &UpscaleFrame<'_, wgpu::TextureView>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        if self.config.fail_evaluates > 0 {
            self.config.fail_evaluates -= 1;
            return Err(UpscaleError::Backend("scripted evaluate failure".into()));
        }
        let filter = match self.filter.take() {
            Some(filter) => filter,
            None => pipeline(ctx.device, &module(ctx.device)?, "cs_upscale"),
        };
        let display = frame.params.display_size;
        let sizes = sizes_buffer(
            ctx.device,
            Sizes {
                render: [eval.render_size.0, eval.render_size.1],
                display: [display.0, display.1],
                gain: self.config.gain,
                has_reactive: 0,
                pad: [0; 2],
            },
        );
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scripted vendor filter"),
            layout: &filter.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: sizes.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(frame.color),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(ctx.output),
                },
            ],
        });
        {
            let mut pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("scripted vendor filter"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&filter);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(display.0.div_ceil(8), display.1.div_ceil(8), 1);
        }
        self.filter = Some(filter);
        Ok(())
    }
}

/// One image a binding was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedImage {
    /// The texture's size.
    pub size: (u32, u32),
    /// Its format.
    pub format: wgpu::TextureFormat,
    /// Its sample count.
    pub samples: u32,
}

impl RecordedImage {
    fn of(view: &wgpu::TextureView) -> Self {
        let texture = view.texture();
        Self {
            size: (texture.width(), texture.height()),
            format: texture.format(),
            samples: texture.sample_count(),
        }
    }
}

/// One feature a binding was asked to create.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordedFeature {
    /// The handle it answered.
    pub handle: FeatureHandle,
    /// What it was created for.
    pub desc: FeatureDesc,
    /// The settings it was created with.
    pub settings: OptimalSettings,
}

/// Everything one evaluate received.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordedEval {
    /// The feature evaluated.
    pub feature: FeatureHandle,
    /// The binding's contract at this call.
    pub contract: InputContract,
    /// How the feature's inputs are encoded.
    pub flags: FeatureFlags,
    /// The frame's mode for this feature.
    pub quality: UpscaleQuality,
    /// The frame's values, in Regia Engine's conventions.
    pub params: UpscaleParams,
    /// The same values converted for the vendor.
    pub eval: VendorEvalParams,
    /// The color input.
    pub color: RecordedImage,
    /// The depth input.
    pub depth: Option<RecordedImage>,
    /// The motion input.
    pub motion: Option<RecordedImage>,
    /// The reactive mask.
    pub reactive: Option<RecordedImage>,
    /// The transparency mask.
    pub transparency: Option<RecordedImage>,
    /// The output.
    pub output: RecordedImage,
    /// The binding's answer.
    pub ok: bool,
}

/// One render texel as an evaluate saw it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CapturedTexel {
    /// The color input.
    pub color: [f32; 4],
    /// The motion input, in the texture's own units.
    pub motion: [f32; 2],
    /// The depth input.
    pub depth: f32,
    /// The reactive mask (0 when the frame passed none).
    pub reactive: f32,
}

/// A readback of what one evaluate saw and wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedFrame {
    /// The render size.
    pub render: (u32, u32),
    /// The display size.
    pub display: (u32, u32),
    /// The inputs, row by row, `render.0 × render.1` of them.
    pub texels: Vec<CapturedTexel>,
    /// The output, row by row, `display.0 × display.1` of them; empty after a failed evaluate.
    pub output: Vec<[f32; 4]>,
}

impl CapturedFrame {
    /// The input texel at `(x, y)` of the render image.
    #[must_use]
    pub fn at(&self, x: u32, y: u32) -> CapturedTexel {
        self.texels[(y * self.render.0 + x) as usize]
    }
}

#[derive(Debug)]
struct CaptureJob {
    render: (u32, u32),
    display: (u32, u32),
    inputs: wgpu::Buffer,
    /// The output's staging buffer and its padded row.
    output: Option<(wgpu::Buffer, u32)>,
}

#[derive(Debug, Default)]
struct LogState {
    capture: bool,
    features: Vec<RecordedFeature>,
    released: Vec<FeatureHandle>,
    evals: Vec<RecordedEval>,
    captures: Vec<Option<CaptureJob>>,
}

/// The log a [`Recorder`] writes, shared with the test that reads it.
#[derive(Debug, Clone, Default)]
pub struct ScriptLog(Arc<Mutex<LogState>>);

impl ScriptLog {
    /// An empty log; with `capture`, each evaluate also reads its images back.
    #[must_use]
    pub fn new(capture: bool) -> Self {
        Self(Arc::new(Mutex::new(LogState {
            capture,
            ..LogState::default()
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LogState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every evaluate so far.
    #[must_use]
    pub fn evals(&self) -> Vec<RecordedEval> {
        self.state().evals.clone()
    }

    /// Every feature created so far.
    #[must_use]
    pub fn features(&self) -> Vec<RecordedFeature> {
        self.state().features.clone()
    }

    /// Every feature released so far.
    #[must_use]
    pub fn released(&self) -> Vec<FeatureHandle> {
        self.state().released.clone()
    }

    /// Reads back what evaluate `index` saw. The frame must have been submitted.
    ///
    /// # Errors
    ///
    /// No capture for that evaluate, or the readback failed.
    pub fn read_capture(&self, device: &wgpu::Device, index: usize) -> Result<CapturedFrame, String> {
        let state = self.state();
        let job = state
            .captures
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("no capture for evaluate {index}"))?;
        let texels: Vec<CapturedTexel> = bytemuck::pod_collect_to_vec(&read_buffer(device, &job.inputs)?);
        let mut output = Vec::new();
        if let Some((buffer, row)) = job.output.as_ref() {
            let bytes = read_buffer(device, buffer)?;
            let row_bytes = job.display.0 as usize * 8;
            for line in bytes.chunks_exact(*row as usize) {
                let (texels, _) = line[..row_bytes].as_chunks::<8>();
                output.extend(texels.iter().map(|t| {
                    let channel = |at: usize| half_to_f32(u16::from_le_bytes([t[at], t[at + 1]]));
                    [channel(0), channel(2), channel(4), channel(6)]
                }));
            }
        }
        Ok(CapturedFrame {
            render: job.render,
            display: job.display,
            texels,
            output,
        })
    }
}

fn read_buffer(device: &wgpu::Device, buffer: &wgpu::Buffer) -> Result<Vec<u8>, String> {
    let slice = buffer.slice(..);
    let (send, receive) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        // The receiver outlives the poll below; a failed send only means the test already gave up.
        let _ = send.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|error| format!("poll: {error}"))?;
    receive
        .recv()
        .map_err(|error| format!("readback: {error}"))?
        .map_err(|error| format!("map: {error}"))?;
    let bytes = slice
        .get_mapped_range()
        .map_err(|error| format!("mapped range: {error}"))?
        .to_vec();
    buffer.unmap();
    Ok(bytes)
}

/// One `Rgba16Float` channel as an `f32`.
fn half_to_f32(half: u16) -> f32 {
    let sign = if half & 0x8000 == 0 { 1.0f32 } else { -1.0 };
    let exponent = i32::from((half >> 10) & 0x1F);
    let fraction = f32::from(half & 0x03FF) / 1024.0;
    sign * match exponent {
        0 => fraction * 2.0f32.powi(-14),
        31 if fraction == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        _ => (1.0 + fraction) * 2.0f32.powi(exponent - 15),
    }
}

/// Wraps a binding, logs everything the seam hands it and forwards the call.
#[derive(Debug)]
pub struct Recorder<B> {
    inner: B,
    log: ScriptLog,
    capture: Option<wgpu::ComputePipeline>,
    no_reactive: Option<(wgpu::Texture, wgpu::TextureView)>,
}

impl<B> Recorder<B> {
    /// `inner`, logging into `log`.
    #[must_use]
    pub const fn new(inner: B, log: ScriptLog) -> Self {
        Self {
            inner,
            log,
            capture: None,
            no_reactive: None,
        }
    }
}

impl<B: VendorRuntimeApi> VendorRuntimeApi for Recorder<B> {
    fn vendor(&self) -> VendorUpscaler {
        self.inner.vendor()
    }

    fn runtime(&self) -> VendorRuntime {
        self.inner.runtime()
    }

    fn contract(&self) -> InputContract {
        self.inner.contract()
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        self.inner.vulkan_extensions()
    }

    fn optimal_settings(
        &self,
        display: (u32, u32),
        quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        self.inner.optimal_settings(display, quality)
    }

    fn release_feature(&mut self, feature: FeatureHandle) {
        self.log.state().released.push(feature);
        self.inner.release_feature(feature);
    }
}

impl<B: VendorEvaluate<GpuTarget>> Recorder<B> {
    /// Records the copy of `frame`'s inputs into a staging buffer, ahead of the binding's work.
    fn capture_inputs(
        &mut self,
        ctx: &mut GpuUpscaleContext<'_>,
        frame: &UpscaleFrame<'_, wgpu::TextureView>,
    ) -> Result<wgpu::Buffer, UpscaleError> {
        let (Some(depth), Some(motion)) = (frame.depth, frame.motion) else {
            return Err(UpscaleError::MissingInput {
                input: if frame.depth.is_none() { "depth" } else { "motion" },
            });
        };
        let capture = match self.capture.take() {
            Some(capture) => capture,
            None => pipeline(ctx.device, &module(ctx.device)?, "cs_capture"),
        };
        let (no_reactive_texture, no_reactive) = match self.no_reactive.take() {
            Some(pair) => pair,
            None => {
                let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("scripted vendor no reactive"),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                (texture, view)
            }
        };
        let render = frame.params.render_size;
        let display = frame.params.display_size;
        let sizes = sizes_buffer(
            ctx.device,
            Sizes {
                render: [render.0, render.1],
                display: [display.0, display.1],
                gain: 1.0,
                has_reactive: u32::from(frame.reactive.is_some()),
                pad: [0; 2],
            },
        );
        let bytes = u64::from(render.0) * u64::from(render.1) * std::mem::size_of::<CapturedTexel>() as u64;
        let storage = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scripted vendor capture"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scripted vendor capture staging"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scripted vendor capture"),
            layout: &capture.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: sizes.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(frame.color),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(depth),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(motion),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(frame.reactive.unwrap_or(&no_reactive)),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: storage.as_entire_binding(),
                },
            ],
        });
        {
            let mut pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("scripted vendor capture"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&capture);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(render.0.div_ceil(8), render.1.div_ceil(8), 1);
        }
        ctx.encoder.copy_buffer_to_buffer(&storage, 0, &staging, 0, bytes);
        self.capture = Some(capture);
        self.no_reactive = Some((no_reactive_texture, no_reactive));
        Ok(staging)
    }

    /// Records the copy of the output into a staging buffer, after the binding's work.
    fn capture_output(ctx: &mut GpuUpscaleContext<'_>, display: (u32, u32)) -> (wgpu::Buffer, u32) {
        let row = (display.0 * 8).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scripted vendor output staging"),
            size: u64::from(row) * u64::from(display.1),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        ctx.encoder.copy_texture_to_buffer(
            ctx.output.texture().as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(display.1),
                },
            },
            ctx.output.texture().size(),
        );
        (staging, row)
    }
}

impl<B: VendorEvaluate<GpuTarget>> VendorEvaluate<GpuTarget> for Recorder<B> {
    fn create_feature(
        &mut self,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        desc: &FeatureDesc,
        settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        let handle = self.inner.create_feature(ctx, desc, settings)?;
        self.log.state().features.push(RecordedFeature {
            handle,
            desc: *desc,
            settings: *settings,
        });
        Ok(handle)
    }

    fn evaluate(
        &mut self,
        feature: FeatureHandle,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        frame: &UpscaleFrame<'_, wgpu::TextureView>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        let capturing = self.log.state().capture;
        let inputs = if capturing {
            Some(self.capture_inputs(ctx, frame)?)
        } else {
            None
        };
        let result = self.inner.evaluate(feature, ctx, frame, eval);
        let display = frame.params.display_size;
        // A binding that ended the encoder left a fresh one here, after its own buffer.
        let output = (inputs.is_some() && result.is_ok()).then(|| Self::capture_output(ctx, display));
        let job = inputs.map(|inputs| CaptureJob {
            render: frame.params.render_size,
            display,
            inputs,
            output,
        });
        let contract = self.inner.contract();
        let mut log = self.log.state();
        let desc = log
            .features
            .iter()
            .rev()
            .find(|created| created.handle == feature)
            .map(|created| created.desc);
        log.evals.push(RecordedEval {
            feature,
            contract,
            flags: desc.map_or(FeatureFlags::REGIA, |d| d.flags),
            quality: desc.map_or(UpscaleQuality::Native, |d| d.quality),
            params: frame.params,
            eval: *eval,
            color: RecordedImage::of(frame.color),
            depth: frame.depth.map(RecordedImage::of),
            motion: frame.motion.map(RecordedImage::of),
            reactive: frame.reactive.map(RecordedImage::of),
            transparency: frame.transparency.map(RecordedImage::of),
            output: RecordedImage::of(ctx.output),
            ok: result.is_ok(),
        });
        log.captures.push(job);
        drop(log);
        result
    }
}

/// What the frame should have handed a binding on one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpectedFrame {
    /// Render size.
    pub render: (u32, u32),
    /// Display size.
    pub display: (u32, u32),
    /// The projection jitter in NDC, as the frame moved its projection.
    pub jitter_ndc: [f32; 2],
    /// The exposure multiplier (post exposure ÷ pre-exposure).
    pub exposure: f32,
    /// The binding is told to drop its history.
    pub reset: bool,
    /// The frame passes a reactive mask.
    pub reactive: bool,
}

/// One broken item of [`CHECKLIST`].
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    /// The item of [`CHECKLIST`].
    pub item: &'static str,
    /// What was wrong.
    pub detail: String,
}

/// What a vendor binding is handed, one item each. `book/agents/upscalers.md` explains them.
pub const CHECKLIST: [&str; 10] = [
    "render_size",
    "display_size",
    "jitter_units",
    "jitter_sign",
    "motion_scale",
    "exposure",
    "reset",
    "depth_convention",
    "reactive_mask",
    "transparency_mask",
];

fn close(a: f32, b: f32, relative: f32) -> bool {
    (a - b).abs() <= relative * a.abs().max(b.abs()).max(1.0e-6)
}

/// Checks one logged evaluate against what the frame should have handed it, item by item.
#[must_use]
pub fn check_eval(recorded: &RecordedEval, expected: &ExpectedFrame) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut check = |item: &'static str, ok: bool, detail: String| {
        if !ok {
            out.push(Violation { item, detail });
        }
    };
    let conventions = recorded.contract.conventions;
    let (params, eval) = (&recorded.params, &recorded.eval);
    let render = expected.render;

    let holds_render =
        |image: Option<RecordedImage>| image.is_some_and(|i| i.size.0 >= render.0 && i.size.1 >= render.1);
    check(
        "render_size",
        params.render_size == render
            && eval.render_size == render
            && holds_render(Some(recorded.color))
            && holds_render(recorded.depth)
            && holds_render(recorded.motion),
        format!(
            "expected {render:?}; frame {:?}, eval {:?}, color {:?}",
            params.render_size, eval.render_size, recorded.color.size
        ),
    );
    check(
        "display_size",
        params.display_size == expected.display && recorded.output.size == expected.display,
        format!(
            "expected {:?}; frame {:?}, output {:?}",
            expected.display, params.display_size, recorded.output.size
        ),
    );

    // Units: exactly the contract's conversion of the frame's NDC jitter, and within half a pixel.
    let converted = conventions.jitter_from_regia(expected.jitter_ndc, render);
    let same = |a: [f32; 2], b: [f32; 2]| close(a[0], b[0], 1.0e-4) && close(a[1], b[1], 1.0e-4);
    check(
        "jitter_units",
        same(params.jitter, expected.jitter_ndc)
            && same(eval.jitter, converted)
            && eval
                .jitter
                .iter()
                .all(|axis| axis.abs() <= 0.5 + 1.0e-3 || conventions != InputConventions::RENDER_PIXELS),
        format!(
            "frame {:?} against NDC {:?}; eval {:?} against {converted:?}",
            params.jitter, expected.jitter_ndc, eval.jitter
        ),
    );
    // Sign, spelled out for the render-pixel convention the runtimes use: the projection's own
    // offset, x as it is and y flipped (pixel y is down, NDC y is up).
    let by_hand = [
        expected.jitter_ndc[0] * render.0 as f32 * 0.5,
        -expected.jitter_ndc[1] * render.1 as f32 * 0.5,
    ];
    check(
        "jitter_sign",
        conventions != InputConventions::RENDER_PIXELS || same(eval.jitter, by_hand),
        format!("eval {:?}, projection offset in pixels {by_hand:?}", eval.jitter),
    );

    let scale = conventions.motion_scale_from_regia(render);
    let by_hand = [render.0 as f32, render.1 as f32];
    check(
        "motion_scale",
        same(eval.motion_scale, scale)
            && (conventions != InputConventions::RENDER_PIXELS || same(eval.motion_scale, by_hand))
            && recorded.flags.motion_vectors_render_size
            && !recorded.flags.motion_vectors_jittered
            && recorded.motion.is_some(),
        format!(
            "scale {:?} against {scale:?}; flags {:?}; motion {:?}",
            eval.motion_scale, recorded.flags, recorded.motion
        ),
    );
    check(
        "exposure",
        close(eval.exposure, expected.exposure, 1.0e-4)
            && close(params.exposure, expected.exposure, 1.0e-4)
            && !recorded.flags.auto_exposure
            && recorded.flags.hdr,
        format!(
            "eval {}, frame {}, expected {}; flags {:?}",
            eval.exposure, params.exposure, expected.exposure, recorded.flags
        ),
    );
    check(
        "reset",
        eval.reset == expected.reset,
        format!("eval {}, expected {}", eval.reset, expected.reset),
    );
    check(
        "depth_convention",
        !recorded.flags.depth_inverted
            && recorded
                .depth
                .is_some_and(|d| d.format == wgpu::TextureFormat::Depth32Float && d.samples == 1),
        format!("flags {:?}; depth {:?}", recorded.flags, recorded.depth),
    );
    let reactive_ok = match recorded.reactive {
        None => !expected.reactive,
        Some(mask) => {
            expected.reactive
                && mask.format == wgpu::TextureFormat::R8Unorm
                && mask.samples == 1
                && mask.size == render
        }
    };
    check(
        "reactive_mask",
        reactive_ok,
        format!(
            "expected a mask {}: got {:?} (R8Unorm at {render:?})",
            expected.reactive, recorded.reactive
        ),
    );
    check(
        "transparency_mask",
        recorded.transparency.is_none(),
        format!("no frame passes one yet, got {:?}", recorded.transparency),
    );
    out
}

/// A logged evaluate without the identity of the feature it ran on: two slots that hand a binding
/// the same inputs compare equal.
#[must_use]
pub fn inputs_of(recorded: &RecordedEval) -> RecordedEval {
    RecordedEval {
        feature: FeatureHandle(0),
        ..*recorded
    }
}
