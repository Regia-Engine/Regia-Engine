// SPDX-License-Identifier: MPL-2.0
//! A fullscreen fade over the finished frame: the compositor layer that draws a shot sequence's
//! fade (`regia_framework::shots::ShotSample::fade`). It is an ordinary [`OverlayDraw`] layer
//! named [`FADE_LAYER`], drawn after the UI ([`FADE_ORDER`]), so a fade to black covers the HUD
//! too. Window frames, screenshots with `window_layers` and render-out all draw it.
//!
//! The layer is driven through a [`FadeHandle`]: the owner sets the amount and colour, the layer
//! reads them when it records. An amount of zero records nothing (no pass, no pipeline).

use crate::frame_work::counted::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};

use crate::RenderDevice;
use crate::overlay::{OverlayDraw, WindowOverlay};

/// Name of the fade layer in [`WindowOverlay::layer_names`].
pub const FADE_LAYER: &str = "fade";

/// Order of the fade layer: above the UI ([`WindowOverlay::UI_ORDER`]) and the other layers.
pub const FADE_ORDER: i32 = WindowOverlay::UI_ORDER + 100;

/// What the fade layer draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FadeState {
    /// Opacity of the overlay: 0 = picture only, 1 = fully `color`. Clamped to `[0, 1]` on use.
    pub amount: f32,
    /// Linear RGB of the overlay.
    pub color: [f32; 3],
}

impl FadeState {
    /// No fade.
    pub const NONE: Self = Self {
        amount: 0.0,
        color: [0.0; 3],
    };

    /// `true` when the layer would draw something.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.amount.is_finite() && self.amount > 0.0
    }
}

/// Shared control of an installed fade layer. Cloning shares the same state.
#[derive(Clone, Debug)]
pub struct FadeHandle(Arc<Mutex<FadeState>>);

impl FadeHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(FadeState::NONE)))
    }

    /// Sets the fade for the next frames.
    pub fn set(&self, state: FadeState) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = state;
        }
    }

    /// The current fade ([`FadeState::NONE`] if the lock was poisoned).
    #[must_use]
    pub fn get(&self) -> FadeState {
        self.0.lock().map_or(FadeState::NONE, |g| *g)
    }
}

/// The colour the layer writes for `state` on a target of `format`, and whether that colour was
/// encoded to sRGB: an 8-bit UNORM target holds display-encoded values, an sRGB or float target
/// takes linear ones. Alpha is the clamped amount. The upload leaves the shader's encode flag
/// clear, because this colour is already in the target's encoding.
#[must_use]
pub fn fade_output(state: FadeState, format: wgpu::TextureFormat) -> ([f32; 4], bool) {
    let amount = if state.amount.is_finite() {
        state.amount.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let linear = state
        .color
        .map(|c| if c.is_finite() { c.clamp(0.0, 1.0) } else { 0.0 });
    let encode = matches!(
        format,
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
    );
    let rgb = if encode { linear.map(srgb_encode) } else { linear };
    ([rgb[0], rgb[1], rgb[2], amount], encode)
}

/// The uniform [`FadeOverlay::draw`] writes for `state` on `format`, and the encode variant it
/// is stored under. [`fade_output`] already encoded the colour for an 8-bit UNORM target, so
/// the shader's encode flag stays 0: the colour is encoded once (it was encoded on both sides,
/// a linear 0.5 landing at about 0.873 instead of 0.735).
fn fade_uniform(state: FadeState, format: wgpu::TextureFormat) -> (FadeUniform, bool) {
    let (color, encode) = fade_output(state, format);
    let uniform = FadeUniform {
        color,
        flags: [0.0; 4],
    };
    (uniform, encode)
}

fn srgb_encode(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Installs the fade layer into `overlay` (replacing a previous one) and returns its handle.
pub fn install_fade(overlay: &mut WindowOverlay) -> FadeHandle {
    let handle = FadeHandle::new();
    overlay.insert(
        FADE_ORDER,
        FADE_LAYER,
        Box::new(FadeOverlay {
            handle: handle.clone(),
            gpu: None,
        }),
    );
    handle
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FadeUniform {
    color: [f32; 4],
    flags: [f32; 4],
}

struct FadeGpu {
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    bind_layout: wgpu::BindGroupLayout,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    /// The uniform and its group, one per sRGB-encode flag (a frame drawing onto an encoded and
    /// a linear target writes both): built once, rewritten in place, so a fade frame creates
    /// nothing (ADR-2585; it made a buffer and a group every frame on a raw device).
    uniforms: HashMap<bool, (wgpu::Buffer, wgpu::BindGroup)>,
}

struct FadeOverlay {
    handle: FadeHandle,
    gpu: Option<FadeGpu>,
}

impl FadeOverlay {
    fn gpu(&mut self, device: &RenderDevice) -> &mut FadeGpu {
        self.gpu.get_or_insert_with(|| {
            let d = &device.device;
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("fade overlay"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/fade_overlay.wgsl").into()),
            });
            let bind_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("fade overlay"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
            let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("fade overlay"),
                bind_group_layouts: &[Some(&bind_layout)],
                immediate_size: 0,
            });
            FadeGpu {
                module,
                layout,
                bind_layout,
                pipelines: HashMap::new(),
                uniforms: HashMap::new(),
            }
        })
    }

    fn pipeline(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        let gpu = self.gpu(device);
        if let Some(p) = gpu.pipelines.get(&format) {
            return p.clone();
        }
        let pipeline = crate::TrackedDevice::observe(&device.device).create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("fade overlay"),
                layout: Some(&gpu.layout),
                vertex: wgpu::VertexState {
                    module: &gpu.module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &gpu.module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(&device.device).as_deref(),
            },
        );
        gpu.pipelines.insert(format, pipeline.clone());
        pipeline
    }

    /// The uniform buffer and group of the `encode` variant, built on first use (counted).
    fn uniform(&mut self, device: &RenderDevice, encode: bool) -> (wgpu::Buffer, wgpu::BindGroup) {
        let gpu = self.gpu(device);
        if let Some((buffer, group)) = gpu.uniforms.get(&encode) {
            return (buffer.clone(), group.clone());
        }
        let d = crate::TrackedDevice::observe(&device.device);
        let buffer = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fade overlay"),
            size: size_of::<FadeUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fade overlay"),
            layout: &gpu.bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        });
        gpu.uniforms.insert(encode, (buffer.clone(), group.clone()));
        (buffer, group)
    }
}

impl OverlayDraw for FadeOverlay {
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        _width: u32,
        _height: u32,
    ) {
        let state = self.handle.get();
        if !state.is_visible() {
            return;
        }
        let _span = tracing::trace_span!("fade_overlay").entered();
        let (uniform, encode) = fade_uniform(state, format);
        let pipeline = self.pipeline(device, format);
        let (buffer, bind_group) = self.uniform(device, encode);
        device
            .queue
            .write_buffer(&buffer, 0, bytemuck::bytes_of(&uniform));
        let _crumb = crate::breadcrumbs::frame_pass("fade_overlay");
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("fade overlay"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.counted_set_pipeline(&pipeline);
        pass.counted_set_bind_group(0, &bind_group, &[]);
        pass.counted_draw(0..3, 0..1);
    }

    fn warm(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) {
        let _ = self.pipeline(device, format);
        let (_, encode) = fade_output(FadeState::NONE, format);
        let _ = self.uniform(device, encode);
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "fade_overlay.wgsl",
        "Fade",
        FadeUniform { color, flags }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layer_sits_above_the_ui_and_installs_once() {
        let mut overlay = WindowOverlay::default();
        struct Noop;
        impl OverlayDraw for Noop {
            fn draw(
                &mut self,
                _: &RenderDevice,
                _: &mut wgpu::CommandEncoder,
                _: &wgpu::TextureView,
                _: wgpu::TextureFormat,
                _: u32,
                _: u32,
            ) {
            }
        }
        overlay.set(Box::new(Noop));
        let first = install_fade(&mut overlay);
        let second = install_fade(&mut overlay);
        assert_eq!(
            overlay.layer_names(),
            ["ui", FADE_LAYER],
            "the fade draws after the UI"
        );
        first.set(FadeState {
            amount: 1.0,
            color: [1.0; 3],
        });
        assert_eq!(
            second.get(),
            FadeState::NONE,
            "a reinstall replaces the layer and its state"
        );
    }

    #[test]
    fn handles_share_state() {
        let mut overlay = WindowOverlay::default();
        let a = install_fade(&mut overlay);
        let b = a.clone();
        let state = FadeState {
            amount: 0.25,
            color: [0.1, 0.2, 0.3],
        };
        a.set(state);
        assert_eq!(b.get(), state);
        assert!(b.get().is_visible());
        assert!(!FadeState::NONE.is_visible());
        assert!(
            !FadeState {
                amount: f32::NAN,
                color: [0.0; 3]
            }
            .is_visible()
        );
    }

    /// Every counted create by `(kind, label)` while `run` runs on this thread.
    fn traced(run: impl FnOnce()) -> Vec<(crate::frame_stats::FramePathKind, String)> {
        crate::frame_stats::trace(true);
        run();
        let events = crate::frame_stats::take_events();
        crate::frame_stats::trace(false);
        events.into_iter().map(|e| (e.kind, e.label)).collect()
    }

    /// The fade's uniform and group are counted when built and built once: a fading frame after
    /// the first (or after the warm) creates no buffer and no bind group (ADR-2585; they were made
    /// every frame on a raw device, so the hitch gate never saw them).
    #[test]
    fn a_fade_frame_after_the_first_creates_nothing_and_the_first_is_counted() {
        use crate::frame_stats::FramePathKind;
        let device = crate::device::test_device();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let target = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fade test target"),
            size: wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let handle = FadeHandle::new();
        handle.set(FadeState {
            amount: 0.5,
            color: [0.0; 3],
        });
        let mut layer = FadeOverlay {
            handle: handle.clone(),
            gpu: None,
        };
        let frame = |layer: &mut FadeOverlay| {
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("fade test"),
                });
            layer.draw(&device, &mut encoder, &view, format, 8, 8);
            device.queue.submit([encoder.finish()]);
        };
        let first = traced(|| frame(&mut layer));
        for kind in [
            FramePathKind::Pipeline,
            FramePathKind::Buffer,
            FramePathKind::BindGroup,
        ] {
            assert!(
                first
                    .iter()
                    .any(|(k, label)| *k == kind && label == "fade overlay"),
                "the first fade frame's {kind:?} is counted: {first:?}"
            );
        }
        for _ in 0..3 {
            handle.set(FadeState {
                amount: 0.75,
                color: [1.0, 0.5, 0.25],
            });
            let later = traced(|| frame(&mut layer));
            assert!(later.is_empty(), "a later fade frame created {later:?}");
        }
        // A warmed layer's first fade creates nothing either.
        let mut warmed = FadeOverlay { handle, gpu: None };
        warmed.warm(&device, format);
        let after_warm = traced(|| frame(&mut warmed));
        assert!(
            after_warm.is_empty(),
            "the first fade after the warm created {after_warm:?}"
        );
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    }

    #[test]
    fn output_is_clamped_and_encoded_only_for_unorm_targets() {
        let state = FadeState {
            amount: 2.0,
            color: [0.5, -1.0, f32::NAN],
        };
        let (linear, encoded) = fade_output(state, wgpu::TextureFormat::Rgba8UnormSrgb);
        assert!(!encoded);
        assert_eq!(linear, [0.5, 0.0, 0.0, 1.0]);
        let (float, encoded) = fade_output(state, wgpu::TextureFormat::Rgba16Float);
        assert!(!encoded);
        assert_eq!(float, linear);
        let (unorm, encoded) = fade_output(state, wgpu::TextureFormat::Rgba8Unorm);
        assert!(encoded);
        assert!(
            (unorm[0] - 0.7354).abs() < 1.0e-3,
            "0.5 linear is 0.735 encoded: {}",
            unorm[0]
        );
        assert_eq!(unorm[3], 1.0);
        let (none, _) = fade_output(
            FadeState {
                amount: f32::INFINITY,
                color: [1.0; 3],
            },
            wgpu::TextureFormat::Rgba8Unorm,
        );
        assert_eq!(none[3], 0.0, "a non-finite amount draws nothing");
    }

    /// An 8-bit UNORM target stores one sRGB encode. The upload leaves the shader flag clear
    /// so that encode is not applied again.
    #[test]
    fn an_8_bit_unorm_fade_is_encoded_once() {
        let state = FadeState {
            amount: 1.0,
            color: [0.5, 0.5, 0.5],
        };
        let (color, encode) = fade_output(state, wgpu::TextureFormat::Rgba8Unorm);
        assert!(encode);
        let once = srgb_encode(0.5);
        assert!((color[0] - once).abs() < 1.0e-4, "{}", color[0]);
        assert!((color[1] - once).abs() < 1.0e-4);
        assert!((color[2] - once).abs() < 1.0e-4);
        let twice = srgb_encode(color[0]);
        assert!(
            (twice - once).abs() > 0.05,
            "a second encode would store {twice}, not {once}"
        );
        let uploaded = FadeUniform {
            color,
            flags: [0.0; 4],
        };
        assert_eq!(uploaded.flags[0], 0.0);
        let (linear, encode) = fade_output(state, wgpu::TextureFormat::Rgba8UnormSrgb);
        assert!(!encode);
        assert_eq!(linear[0].to_bits(), 0.5_f32.to_bits());
    }

    /// What `fade_overlay.wgsl` `fs_main` returns for a uniform: `flags.x` set encodes the colour.
    fn shader_output(u: &FadeUniform) -> [f32; 4] {
        let rgb = [u.color[0], u.color[1], u.color[2]];
        let rgb = if u.flags[0] > 0.5 {
            rgb.map(srgb_encode)
        } else {
            rgb
        };
        [rgb[0], rgb[1], rgb[2], u.color[3]]
    }

    /// A linear mid grey on an 8-bit UNORM target is sRGB-encoded exactly once between the CPU
    /// and the shader (it was encoded on both, about 0.873; audit render10 #4). An sRGB target
    /// takes the linear value and the hardware encodes it.
    #[test]
    fn the_fade_colour_is_encoded_once_on_every_target() {
        let grey = FadeState {
            amount: 1.0,
            color: [0.5; 3],
        };
        for format in [wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureFormat::Bgra8Unorm] {
            let (uniform, _) = fade_uniform(grey, format);
            let out = shader_output(&uniform);
            assert!((out[0] - 0.7354).abs() < 1.0e-3, "{format:?}: {out:?}");
            assert_eq!(out[3], 1.0);
        }
        for format in [
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba16Float,
        ] {
            let (uniform, _) = fade_uniform(grey, format);
            assert_eq!(shader_output(&uniform), [0.5, 0.5, 0.5, 1.0], "{format:?}");
        }
    }

    /// The same on the GPU: a full fade of linear 0.5 over a UNORM target reads back as the
    /// one-encode byte 188 (0.735 · 255), not the double-encoded 223.
    #[test]
    fn a_unorm_target_reads_back_the_once_encoded_fade() {
        let device = crate::device::test_device();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fade unorm target"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let handle = FadeHandle::new();
        handle.set(FadeState {
            amount: 1.0,
            color: [0.5; 3],
        });
        let mut layer = FadeOverlay { handle, gpu: None };
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fade unorm test"),
            });
        layer.draw(&device, &mut encoder, &view, format, 4, 4);
        device.queue.submit([encoder.finish()]);
        let pixels = crate::read_rgba8(&device.device, &device.queue, &target, 4, 4).expect("readback");
        for px in pixels.as_chunks::<4>().0 {
            for &c in &px[..3] {
                assert!(
                    (i32::from(c) - 188).abs() <= 1,
                    "one sRGB encode of 0.5 is 188: {px:?}"
                );
            }
        }
    }
}
