// SPDX-License-Identifier: MPL-2.0
//! The display stage (ADR-092): the frame draws into an intermediate when calibration is not
//! neutral or the surface is HDR, and one fullscreen pass (`display.wgsl`) writes the calibrated,
//! output-encoded image into the swapchain. The intermediate is also the neutral screenshot.

use crate::frame_work::counted::*;
use std::borrow::Cow;
use std::collections::HashMap;

use regia_app::prelude::*;

use crate::RenderError;
use crate::calibration::DisplayCalibration;
use crate::shader_lib::compose_source;

/// Format the frame draws into ahead of an SDR display stage (the canonical sRGB path the
/// scene blit and the UI already target).
pub(crate) const SDR_INTERMEDIATE: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// Format the frame draws into ahead of an HDR display stage: linear, paper-white units,
/// highlights above 1.
pub(crate) const HDR_INTERMEDIATE: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// How the display stage encodes its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageOutput {
    /// Display-encoded values into a non-sRGB 8-bit target.
    SdrEncoded,
    /// Linear values into an sRGB target (the hardware encodes).
    SdrLinear,
    /// HDR10: Rec.2020 primaries, SMPTE ST 2084.
    Pq,
    /// scRGB: linear Rec.709, 1.0 = 80 cd/m².
    ScRgb,
}

impl StageOutput {
    /// The output encoding for a swapchain format and colour space.
    pub(crate) fn for_surface(format: wgpu::TextureFormat, space: wgpu::SurfaceColorSpace) -> Self {
        match space {
            wgpu::SurfaceColorSpace::Bt2100Pq => Self::Pq,
            wgpu::SurfaceColorSpace::ExtendedSrgbLinear => Self::ScRgb,
            _ if format.is_srgb() => Self::SdrLinear,
            _ => Self::SdrEncoded,
        }
    }

    /// True for the HDR encodings.
    pub(crate) fn is_hdr(self) -> bool {
        matches!(self, Self::Pq | Self::ScRgb)
    }

    fn word(self) -> f32 {
        match self {
            Self::SdrEncoded => 0.0,
            Self::SdrLinear => 1.0,
            Self::Pq => 2.0,
            Self::ScRgb => 3.0,
        }
    }
}

/// The HDR peak test pattern the calibration screen asks the display stage to draw: a ring at
/// [`Self::logo_nits`] on a background at [`Self::background_nits`] (brighter than any display,
/// so it shows the display's real maximum). The player raises the peak until the ring vanishes.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct HdrTestPattern {
    /// Rectangle in normalised window coordinates (x, y, width, height).
    pub rect: [f32; 4],
    /// Ring luminance, cd/m² (the calibrated peak).
    pub logo_nits: f32,
    /// Background luminance, cd/m².
    pub background_nits: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    sdr: [f32; 4],
    hdr: [f32; 4],
    pattern_rect: [f32; 4],
    pattern: [f32; 4],
}

/// Uniform words for one stage pass.
fn params(
    calibration: &DisplayCalibration,
    input_linear: bool,
    output: StageOutput,
    pattern: Option<&HdrTestPattern>,
    size: (u32, u32),
) -> Params {
    let [sdr, hdr] = calibration.uniform_words();
    let (w, h) = (size.0 as f32, size.1 as f32);
    let (pattern_rect, pattern) = match pattern.filter(|_| output.is_hdr()) {
        Some(p) => {
            let rect = [
                p.rect[0] * w,
                p.rect[1] * h,
                (p.rect[0] + p.rect[2]) * w,
                (p.rect[1] + p.rect[3]) * h,
            ];
            let radius = 0.35 * (rect[2] - rect[0]).min(rect[3] - rect[1]);
            (rect, [1.0, p.background_nits, p.logo_nits, radius])
        }
        None => ([0.0; 4], [0.0; 4]),
    };
    Params {
        sdr: [
            sdr[0],
            if output.is_hdr() { 1.0 } else { sdr[1] },
            if input_linear { 1.0 } else { 0.0 },
            output.word(),
        ],
        hdr,
        pattern_rect,
        pattern,
    }
}

/// A texture the stage owns (the intermediate or the capture scratch).
pub(crate) struct StageTexture {
    pub(crate) texture: wgpu::Texture,
    pub(crate) view: wgpu::TextureView,
}

fn stage_texture(
    device: &wgpu::Device,
    label: &str,
    (width, height): (u32, u32),
    format: wgpu::TextureFormat,
) -> StageTexture {
    let texture = crate::TrackedDevice::observe(device).create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    StageTexture { texture, view }
}

/// Which uniform slot a pass uses (one buffer each, so two passes in one submit do not
/// overwrite each other's parameters).
#[derive(Debug, Clone, Copy)]
pub(crate) enum StageSlot {
    /// The pass into the swapchain.
    Present = 0,
    /// The pass into the capture scratch (screenshots of an HDR frame).
    Capture = 1,
}

/// Persistent display-stage state of one window.
#[derive(Default)]
pub(crate) struct DisplayStage {
    module: Option<wgpu::ShaderModule>,
    bgl: Option<wgpu::BindGroupLayout>,
    layout: Option<wgpu::PipelineLayout>,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    uniforms: Vec<wgpu::Buffer>,
    intermediate: Option<(StageTexture, (u32, u32), wgpu::TextureFormat)>,
    capture: Option<(StageTexture, (u32, u32))>,
    /// The group of each [`StageSlot`] over the intermediate: built on its first pass, dropped
    /// when the intermediate is replaced, so a steady HDR window creates no group per frame
    /// (ADR-2585; it made one every frame on a raw device).
    groups: [Option<wgpu::BindGroup>; 2],
}

impl DisplayStage {
    fn ensure_base(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.module.is_some() {
            return Ok(());
        }
        let naga = compose_source(
            include_str!("../shaders/display.wgsl"),
            "display.wgsl",
            HashMap::new(),
        )?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("display stage"),
            source: wgpu::ShaderSource::Naga(Cow::Owned(naga)),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("display stage bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("display stage layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let tracked = crate::TrackedDevice::observe(device);
        self.uniforms = (0..2)
            .map(|_| {
                tracked.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("display stage params"),
                    size: std::mem::size_of::<Params>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .collect();
        self.module = Some(module);
        self.bgl = Some(bgl);
        self.layout = Some(layout);
        Ok(())
    }

    /// The intermediate the frame draws into (created or resized on demand).
    pub(crate) fn intermediate(
        &mut self,
        device: &wgpu::Device,
        size: (u32, u32),
        format: wgpu::TextureFormat,
    ) -> &StageTexture {
        let stale = self
            .intermediate
            .as_ref()
            .is_none_or(|(_, s, f)| *s != size || *f != format);
        if stale {
            self.groups = [None, None];
            self.intermediate = Some((
                stage_texture(device, "display stage intermediate", size, format),
                size,
                format,
            ));
        }
        &self.intermediate.as_ref().expect("intermediate just ensured").0
    }

    /// Drops the intermediate (the stage is off).
    pub(crate) fn release(&mut self) {
        self.intermediate = None;
        self.capture = None;
        self.groups = [None, None];
    }

    /// Records the stage from the intermediate into `out`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        out: &wgpu::TextureView,
        out_format: wgpu::TextureFormat,
        output: StageOutput,
        calibration: &DisplayCalibration,
        pattern: Option<&HdrTestPattern>,
        slot: StageSlot,
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("display_stage").entered();
        self.ensure_base(device)?;
        let Some((input, size, in_format)) = self.intermediate.as_ref() else {
            return Err(RenderError::Shader("display stage has no intermediate".into()));
        };
        let input_linear = in_format.is_srgb() || *in_format == HDR_INTERMEDIATE;
        let uniform = params(calibration, input_linear, output, pattern, *size);
        let buffer = &self.uniforms[slot as usize];
        queue.write_buffer(buffer, 0, bytemuck::bytes_of(&uniform));
        let bgl = self.bgl.as_ref().expect("display bgl");
        let bind = self.groups[slot as usize]
            .get_or_insert_with(|| {
                crate::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("display stage bg"),
                    layout: bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&input.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: buffer.as_entire_binding(),
                        },
                    ],
                })
            })
            .clone();
        let module = self.module.as_ref().expect("display module");
        let layout = self.layout.as_ref().expect("display layout");
        let pipeline = self
            .pipelines
            .entry(out_format)
            .or_insert_with(|| stage_pipeline(device, layout, module, out_format));
        let _crumb = crate::breadcrumbs::frame_pass("display_stage");
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("display stage"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: out,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            ..Default::default()
        });
        pass.counted_set_pipeline(pipeline);
        pass.counted_set_bind_group(0, &bind, &[]);
        pass.counted_draw(0..3, 0..1);
        Ok(())
    }

    /// Renders an SDR rendition of the intermediate (with `calibration`'s SDR curve) into the
    /// capture scratch and returns it: screenshots of an HDR frame.
    pub(crate) fn record_capture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        calibration: &DisplayCalibration,
    ) -> Result<&wgpu::Texture, RenderError> {
        let Some((_, size, _)) = self.intermediate.as_ref() else {
            return Err(RenderError::Shader("display stage has no intermediate".into()));
        };
        let size = *size;
        if self.capture.as_ref().is_none_or(|(_, s)| *s != size) {
            self.capture = Some((
                stage_texture(device, "display stage capture", size, SDR_INTERMEDIATE),
                size,
            ));
        }
        let view = self
            .capture
            .as_ref()
            .expect("capture just ensured")
            .0
            .view
            .clone();
        self.record(
            device,
            queue,
            encoder,
            &view,
            SDR_INTERMEDIATE,
            StageOutput::SdrLinear,
            calibration,
            None,
            StageSlot::Capture,
        )?;
        Ok(&self.capture.as_ref().expect("capture just ensured").0.texture)
    }

    /// The intermediate texture (the neutral image), when the stage is on.
    pub(crate) fn intermediate_texture(&self) -> Option<(&wgpu::Texture, wgpu::TextureFormat)> {
        self.intermediate.as_ref().map(|(t, _, f)| (&t.texture, *f))
    }
}

fn stage_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    crate::TrackedDevice::observe(device).create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("display stage pipeline"),
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

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "display.wgsl",
        "DisplayParams",
        Params {
            sdr,
            hdr,
            pattern_rect,
            pattern
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_follows_the_surface() {
        assert_eq!(
            StageOutput::for_surface(
                wgpu::TextureFormat::Rgb10a2Unorm,
                wgpu::SurfaceColorSpace::Bt2100Pq
            ),
            StageOutput::Pq
        );
        assert_eq!(
            StageOutput::for_surface(
                wgpu::TextureFormat::Rgba16Float,
                wgpu::SurfaceColorSpace::ExtendedSrgbLinear
            ),
            StageOutput::ScRgb
        );
        assert_eq!(
            StageOutput::for_surface(wgpu::TextureFormat::Bgra8UnormSrgb, wgpu::SurfaceColorSpace::Auto),
            StageOutput::SdrLinear
        );
        assert_eq!(
            StageOutput::for_surface(wgpu::TextureFormat::Bgra8Unorm, wgpu::SurfaceColorSpace::Srgb),
            StageOutput::SdrEncoded
        );
    }

    /// Runs the stage over a 256-wide ramp (every 8-bit code value, grey) and reads it back.
    fn run_ramp(
        calibration: &DisplayCalibration,
        in_format: wgpu::TextureFormat,
        out_format: wgpu::TextureFormat,
        output: StageOutput,
    ) -> Vec<u8> {
        let device = crate::test_device();
        let mut stage = DisplayStage::default();
        let size = (256u32, 1u32);
        let ramp: Vec<u8> = (0..=255u8).flat_map(|v| [v, v, v, 255]).collect();
        let input = stage.intermediate(&device.device, size, in_format);
        device.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &input.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &ramp,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256 * 4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 256,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let out = stage_texture(&device.device, "stage test out", size, out_format);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        stage
            .record(
                &device.device,
                &device.queue,
                &mut encoder,
                &out.view,
                out_format,
                output,
                calibration,
                None,
                StageSlot::Present,
            )
            .expect("stage records");
        let readback = crate::screenshot::encode_copy(&device, &mut encoder, &out.texture, out_format);
        device.queue.submit([encoder.finish()]);
        crate::screenshot::finish(&device, readback)
            .expect("readback")
            .rgba
    }

    /// The stage's buffers, intermediate, pipeline and group are counted when built (the hitch
    /// gate sees them), and a steady HDR window's later frames create nothing: the group is kept
    /// per slot until the intermediate is replaced (ADR-2585; one group every frame before, on a
    /// raw device).
    #[test]
    fn a_steady_stage_frame_creates_nothing_and_the_first_is_counted() {
        use crate::frame_stats::FramePathKind;
        let device = crate::test_device();
        let mut stage = DisplayStage::default();
        let size = (16u32, 8u32);
        let calibration = DisplayCalibration::default();
        let out = stage_texture(&device.device, "stage test out", size, SDR_INTERMEDIATE);
        let traced = |stage: &mut DisplayStage| {
            crate::frame_stats::trace(true);
            let _ = stage.intermediate(&device.device, size, HDR_INTERMEDIATE);
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            stage
                .record(
                    &device.device,
                    &device.queue,
                    &mut encoder,
                    &out.view,
                    SDR_INTERMEDIATE,
                    StageOutput::SdrLinear,
                    &calibration,
                    None,
                    StageSlot::Present,
                )
                .expect("stage records");
            device.queue.submit([encoder.finish()]);
            let events = crate::frame_stats::take_events();
            crate::frame_stats::trace(false);
            events.into_iter().map(|e| (e.kind, e.label)).collect::<Vec<_>>()
        };
        let first = traced(&mut stage);
        for (kind, label) in [
            (FramePathKind::Texture, "display stage intermediate"),
            (FramePathKind::Buffer, "display stage params"),
            (FramePathKind::Pipeline, "display stage pipeline"),
            (FramePathKind::BindGroup, "display stage bg"),
        ] {
            assert!(
                first.iter().any(|(k, l)| *k == kind && l == label),
                "the first stage frame's {kind:?} {label:?} is counted: {first:?}"
            );
        }
        for _ in 0..3 {
            let later = traced(&mut stage);
            assert!(later.is_empty(), "a steady stage frame created {later:?}");
        }
        // A new intermediate (a resize) builds its group again, once.
        let _ = stage.intermediate(&device.device, (20, 8), HDR_INTERMEDIATE);
        let _ = stage.intermediate(&device.device, size, HDR_INTERMEDIATE);
        let resized = traced(&mut stage);
        assert_eq!(
            resized
                .iter()
                .filter(|(k, _)| *k == FramePathKind::BindGroup)
                .count(),
            1,
            "a replaced intermediate rebuilds the group once: {resized:?}"
        );
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    }

    #[test]
    fn neutral_stage_is_byte_identical() {
        // The calibration-neutral guarantee on the GPU: every code value through the stage at
        // NEUTRAL comes back unchanged (diff 0), on both SDR output encodings.
        let ramp: Vec<u8> = (0..=255u8).flat_map(|v| [v, v, v, 255]).collect();
        for (in_format, out_format, output) in [
            (
                SDR_INTERMEDIATE,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                StageOutput::SdrLinear,
            ),
            (
                SDR_INTERMEDIATE,
                wgpu::TextureFormat::Rgba8Unorm,
                StageOutput::SdrEncoded,
            ),
            (
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureFormat::Rgba8Unorm,
                StageOutput::SdrEncoded,
            ),
        ] {
            let got = run_ramp(&DisplayCalibration::NEUTRAL, in_format, out_format, output);
            let diff = got.iter().zip(&ramp).filter(|(a, b)| a != b).count();
            assert_eq!(diff, 0, "{in_format:?} -> {out_format:?}: {diff} bytes differ");
        }
    }

    #[test]
    fn gpu_stage_matches_the_cpu_curve() {
        let cal = DisplayCalibration {
            black_level: 0.6,
            gamma: 2.4,
            ..DisplayCalibration::NEUTRAL
        };
        let got = run_ramp(
            &cal,
            SDR_INTERMEDIATE,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            StageOutput::SdrLinear,
        );
        let mut want: Vec<u8> = (0..=255u8).flat_map(|v| [v, v, v, 255]).collect();
        cal.apply_sdr_rgba8(&mut want);
        let worst = got
            .iter()
            .zip(&want)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        assert!(worst <= 1, "GPU vs CPU calibration differ by {worst} code values");
        assert!(got[4 * 10] > 10, "shadows lifted: code 10 -> {}", got[4 * 10]);
    }

    #[test]
    fn hdr10_stage_encodes_pq_of_the_cpu_nits() {
        let cal = DisplayCalibration {
            black_level: 0.3,
            min_nits: 0.05,
            ..DisplayCalibration::NEUTRAL
        };
        // An sRGB intermediate reads back linear: the HDR path's paper-white-unit input.
        let got = run_ramp(
            &cal,
            SDR_INTERMEDIATE,
            wgpu::TextureFormat::Rgba8Unorm,
            StageOutput::Pq,
        );
        for code in [0u8, 8, 40, 128, 255] {
            let linear = crate::calibration::decode_srgb(f32::from(code) / 255.0);
            let nits = cal.hdr_nits([linear; 3]);
            let rgb2020 = crate::calibration::rec709_to_rec2020(nits);
            let want = crate::output::encode_pq_st2084(rgb2020[1]);
            let got_g = f32::from(got[usize::from(code) * 4 + 1]) / 255.0;
            assert!(
                (got_g - want).abs() <= 1.5 / 255.0,
                "code {code}: PQ {got_g} vs {want}"
            );
        }
    }

    #[test]
    fn hdr_ignores_the_sdr_gamma_and_draws_the_pattern_only_on_hdr() {
        let cal = DisplayCalibration {
            gamma: 2.6,
            ..DisplayCalibration::NEUTRAL
        };
        let pattern = HdrTestPattern {
            rect: [0.25, 0.25, 0.5, 0.5],
            logo_nits: 800.0,
            background_nits: 10_000.0,
        };
        let hdr = params(&cal, true, StageOutput::Pq, Some(&pattern), (100, 100));
        assert_eq!(hdr.sdr[1], 1.0);
        assert_eq!(hdr.pattern[0], 1.0);
        assert_eq!(hdr.pattern_rect, [25.0, 25.0, 75.0, 75.0]);
        let sdr = params(&cal, false, StageOutput::SdrEncoded, Some(&pattern), (100, 100));
        assert!((sdr.sdr[1] - 2.6 / 2.2).abs() < 1.0e-6);
        assert_eq!(sdr.pattern[0], 0.0);
    }
}
