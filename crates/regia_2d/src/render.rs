// SPDX-License-Identifier: MPL-2.0
//! GPU side of the sprite pass: one instanced draw per texture batch, straight into the window
//! target, or — in pixel-perfect mode — into a virtual-resolution target that is then scaled by
//! an integer factor into the letterbox rectangle with nearest filtering.
//!
//! Colours are exact: textures are uploaded as RGBA8 and sampled through an sRGB view when the
//! target is sRGB (decode on sample, encode on write: a texel round-trips unchanged), the sampler
//! is nearest, and the blit is a 1:scale nearest copy.

use std::collections::HashMap;

use regia_core::math::{Mat4, UVec2, Vec2, Vec3};
use regia_render::RenderDevice;
use wgpu::util::DeviceExt;

use crate::batch::{PreparedFrame, SpriteInstance};
use crate::camera::{OrthoCamera, PixelPerfect};

const SHADER: &str = include_str!("../shaders/sprite2d.wgsl");

/// What one draw renders: the prepared quads and how to view them.
#[derive(Clone, Debug, Default)]
pub struct DrawSettings {
    /// The camera.
    pub camera: OrthoCamera,
    /// Pixel-perfect mode.
    pub pixel_perfect: Option<PixelPerfect>,
    /// Snap the camera to whole (virtual) pixels.
    pub snap: bool,
    /// Clear colour (sRGB-encoded) for the 2D target, `None` = draw over what is there.
    pub clear: Option<[f32; 4]>,
}

impl DrawSettings {
    /// The camera actually used for an output of `size` pixels (snapped when asked).
    #[must_use]
    pub fn effective_camera(&self, size: UVec2) -> OrthoCamera {
        if self.snap {
            self.camera.snapped(size)
        } else {
            self.camera
        }
    }

    /// These settings with the pixel-perfect virtual size clamped to `max_dim` per axis (the device's
    /// 2D texture limit), so the texture, camera, letterbox and blit all use the same size.
    #[must_use]
    pub fn with_virtual_limit(&self, max_dim: u32) -> Self {
        let mut s = self.clone();
        if let Some(pp) = s.pixel_perfect.as_mut() {
            // Low end too: a struct-literal `PixelPerfect` skips `PixelPerfect::new`'s clamp, and a
            // 0-pixel texture is a wgpu validation error.
            pp.virtual_size = pp.virtual_size.clamp(UVec2::ONE, UVec2::splat(max_dim.max(1)));
        }
        s
    }

    /// The pixel size the sprites are rendered at for a window of `window` pixels: the virtual
    /// size in pixel-perfect mode, else the window.
    #[must_use]
    pub fn render_size(&self, window: UVec2) -> UVec2 {
        self.pixel_perfect.map_or(window, |pp| pp.virtual_size)
    }
}

/// sRGB-encoded → linear, per colour channel (alpha unchanged).
///
/// The power segment `((v + 0.055) / 1.055)^2.4` is evaluated without `powf`: libm
/// transcendentals are not required to agree across operating systems, and this conversion is
/// written into the instance buffer that goldens and agent readbacks compare (determinism audit
/// 11-08 L3). Instead `x^2.4` splits as `m^2.4 · 2^2.4e` with `x = m·2^e`, `m ∈ [0.5, 1)` (frexp
/// by bit manipulation) and `m^2.4` a fixed-coefficient polynomial: every operation is an IEEE
/// add/mul/div, exactly rounded on every platform, so the result is bit-identical everywhere.
/// Maximum absolute error over the segment is ≈ 5·10⁻⁷ (the fit is recorded in
/// `docs/provenance.md`). Encoded values above 1 stay on that curve (an over-bright tint). A
/// result that does not fit in `f32` saturates at `f32::MAX`. A non-finite channel becomes 0
/// so it cannot poison the instance buffer.
fn srgb_to_linear(c: [f32; 4]) -> [f32; 4] {
    let f = |v: f32| {
        if !v.is_finite() {
            0.0
        } else if v <= 0.040_45 {
            v / 12.92
        } else if v == 1.0 {
            1.0
        } else {
            let y = pow_2_4((f64::from(v) + 0.055) / 1.055) as f32;
            if y.is_finite() { y } else { f32::MAX }
        }
    };
    let alpha = if c[3].is_finite() { c[3] } else { 0.0 };
    [f(c[0]), f(c[1]), f(c[2]), alpha]
}

/// Coefficients of the degree-10 approximation of `m^2.4` on `m ∈ [0.5, 1]`, constant term
/// first (fitted offline by iterated weighted least squares at Chebyshev nodes; maximum
/// absolute error ≈ 1·10⁻⁷).
const POW_2_4_COEF: [f64; 11] = [
    0.005_716_457_255_547_916,
    -0.071_251_110_743_441_87,
    0.661_510_589_643_777_1,
    0.432_202_498_615_033_24,
    0.042_777_702_228_237_735,
    -0.086_430_438_549_909_87,
    -0.022_833_741_981_361_72,
    0.042_770_536_644_566_67,
    0.020_648_660_807_866_773,
    -0.036_197_338_794_256_69,
    0.011_086_209_647_898_117,
];
/// `2^2.4` (the scale factor of one frexp bin).
const POW_2_4_BIN: f64 = 5.278_031_643_091_577;

/// `x^2.4` for finite `x > 0`, with no libm calls: `x = m·2^e` (`m ∈ [0.5, 1)`) by exponent-bit
/// manipulation, `m^2.4` by Horner, then `e` multiply/divides by the `2^2.4` constant. `x > 1`
/// (an encoded tint above 1) uses the same split; the polynomial still sees only `m`.
fn pow_2_4(x: f64) -> f64 {
    debug_assert!(x.is_finite() && x > 0.0);
    let bits = x.to_bits();
    // Bias 1022 (2^-1) so the mantissa lands in [0.5, 1).
    let e = ((bits >> 52) & 0x7FF) as i64 - 1022;
    let m = f64::from_bits((bits & 0x800F_FFFF_FFFF_FFFF) | (1022 << 52));
    let mut acc = 0.0;
    for c in POW_2_4_COEF.iter().rev() {
        acc = acc * m + c;
    }
    if e >= 0 {
        for _ in 0..e {
            acc *= POW_2_4_BIN;
        }
    } else {
        for _ in 0..-e {
            acc /= POW_2_4_BIN;
        }
    }
    acc
}

/// A colour in the target's space: linear for sRGB targets (the hardware encodes on write).
fn target_color(c: [f32; 4], srgb: bool) -> [f32; 4] {
    if srgb { srgb_to_linear(c) } else { c }
}

/// Writes `src` into `dst` with sRGB tints converted to linear, one conversion per run of equal
/// colours. A tile layer is one tint, so a full layer converts once. Returns how many colours
/// were converted. `0` and `1` take the endpoint branches of [`srgb_to_linear`] (no polynomial).
fn recolor_runs(src: &[SpriteInstance], dst: &mut Vec<SpriteInstance>) -> u64 {
    dst.clear();
    if dst.capacity() < src.len() {
        // `reserve` is relative to len. `clear` left len at 0, so ask for the whole run.
        dst.reserve(src.len());
    }
    let mut conversions = 0u64;
    let mut i = 0;
    while i < src.len() {
        let color = src[i].color;
        let mut j = i + 1;
        while j < src.len() && src[j].color == color {
            j += 1;
        }
        let linear = srgb_to_linear(color);
        conversions += 1;
        for quad in &src[i..j] {
            let mut quad = *quad;
            quad.color = linear;
            dst.push(quad);
        }
        i = j;
    }
    conversions
}

fn clear_value(c: [f32; 4], srgb: bool) -> wgpu::Color {
    let c = target_color(c, srgb);
    wgpu::Color {
        r: f64::from(c[0]),
        g: f64::from(c[1]),
        b: f64::from(c[2]),
        a: f64::from(c[3]),
    }
}

struct CachedTexture {
    _texture: wgpu::Texture,
    /// Bind group through the unorm view and through the sRGB view.
    unorm: wgpu::BindGroup,
    srgb: wgpu::BindGroup,
    last_used: u64,
}

struct Pipelines {
    sprite: wgpu::RenderPipeline,
    blit: wgpu::RenderPipeline,
}

struct VirtualTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    size: UVec2,
    format: wgpu::TextureFormat,
}

/// The sprite pass's GPU state. Create once per device; [`SpriteRenderer::draw`] records a frame.
pub struct SpriteRenderer {
    device_id: u64,
    view_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    shader: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    pipelines: HashMap<wgpu::TextureFormat, Pipelines>,
    textures: HashMap<u64, CachedTexture>,
    virtual_target: Option<VirtualTarget>,
    frame: u64,
    /// The instances recolored for an sRGB target, kept so the allocation lasts.
    recolored: Vec<SpriteInstance>,
    /// Instance bytes (`VERTEX | COPY_DST`). Grown when a frame does not fit; never per draw.
    instance_buffer: Option<wgpu::Buffer>,
    /// View matrix for the sprite pass. The bind group is created once; the 64 bytes are rewritten.
    sprite_view: Option<RetainedView>,
    /// View matrix for the pixel-perfect blit. A second retained bind, not a second allocation per frame.
    blit_view: Option<RetainedView>,
    /// How many instance vertex buffers `ensure_instance_buffer` has created.
    instance_buffer_creates: u64,
    /// How many view uniform bind groups have been created (sprite view and blit view each count).
    view_bind_creates: u64,
}

/// One uniform buffer and the bind group that reads it.
struct RetainedView {
    buffer: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

/// Unused textures are dropped after this many draws.
const TEXTURE_TTL: u64 = 240;

impl SpriteRenderer {
    /// Creates the shared GPU state for `device`.
    #[must_use]
    pub fn new(device: &RenderDevice) -> Self {
        let d = &device.device;
        let view_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite2d view"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(64),
                },
                count: None,
            }],
        });
        let texture_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite2d texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite2d"),
            bind_group_layouts: &[Some(&view_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sprite2d.wgsl"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sprite2d nearest"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        Self {
            device_id: device.device_id(),
            view_layout,
            texture_layout,
            pipeline_layout,
            shader,
            sampler,
            pipelines: HashMap::new(),
            textures: HashMap::new(),
            virtual_target: None,
            frame: 0,
            recolored: Vec::new(),
            instance_buffer: None,
            sprite_view: None,
            blit_view: None,
            instance_buffer_creates: 0,
            view_bind_creates: 0,
        }
    }

    /// How many instance vertex buffers this renderer has created.
    ///
    /// A second draw of the same frame does not increment this. A later frame with more quads
    /// may increment it once, when the retained buffer is too small.
    #[must_use]
    pub fn instance_buffer_creates(&self) -> u64 {
        self.instance_buffer_creates
    }

    /// How many view-uniform bind groups this renderer has created.
    ///
    /// The sprite view is one. Pixel-perfect mode adds the blit view. Further draws reuse both.
    #[must_use]
    pub fn view_bind_creates(&self) -> u64 {
        self.view_bind_creates
    }

    /// The device this renderer was created for.
    #[must_use]
    pub fn device_id(&self) -> u64 {
        self.device_id
    }

    /// Number of textures resident on the GPU.
    #[must_use]
    pub fn resident_textures(&self) -> usize {
        self.textures.len()
    }

    fn pipelines(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> &Pipelines {
        let (layout, shader) = (&self.pipeline_layout, &self.shader);
        self.pipelines.entry(format).or_insert_with(|| {
            let attributes = wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x2, 4 => Float32x2,
                5 => Float32x4, 6 => Uint32
            ];
            let buffers = [Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<SpriteInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &attributes,
            })];
            let make = |label, vs, fs, buffers: &[Option<wgpu::VertexBufferLayout<'_>>], blend| {
                let target = [Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })];
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(layout),
                    vertex: wgpu::VertexState {
                        module: shader,
                        entry_point: Some(vs),
                        compilation_options: Default::default(),
                        buffers,
                    },
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: shader,
                        entry_point: Some(fs),
                        compilation_options: Default::default(),
                        targets: &target,
                    }),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&device).as_deref(),
                })
            };
            Pipelines {
                sprite: make(
                    "sprite2d",
                    "vs_sprite",
                    "fs_sprite",
                    &buffers,
                    wgpu::BlendState::ALPHA_BLENDING,
                ),
                // Straight-alpha sprites have already multiplied RGB by alpha in the virtual target.
                blit: make(
                    "sprite2d blit",
                    "vs_blit",
                    "fs_blit",
                    &[],
                    wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
                ),
            }
        })
    }

    fn texture_bind(&self, device: &wgpu::Device, view: &wgpu::TextureView, label: &str) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    /// Uploads every image the frame uses that is not resident yet and drops long-unused ones.
    fn upload(&mut self, device: &RenderDevice, frame: &PreparedFrame) {
        // Mark the resident images this frame uses, then free the expired ones before any new
        // page is allocated, so the frees are available to the allocation.
        for (image, _) in &frame.batches {
            if let Some(cached) = self.textures.get_mut(&image.key) {
                cached.last_used = self.frame;
            }
        }
        self.expire_textures();
        for (image, _) in &frame.batches {
            if self.textures.contains_key(&image.key) {
                continue;
            }
            let img = &image.image;
            let limit = device.device.limits().max_texture_dimension_2d;
            if img.validate().is_err() || img.width > limit || img.height > limit {
                tracing::warn!(target: "regia_2d", key = image.key, "sprite image has a bad size; skipped");
                continue;
            }
            let texture = device.device.create_texture_with_data(
                &device.queue,
                &wgpu::TextureDescriptor {
                    label: Some("sprite2d image"),
                    size: wgpu::Extent3d {
                        width: img.width,
                        height: img.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &img.rgba8,
            );
            let unorm_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let srgb_view = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::Rgba8UnormSrgb),
                ..Default::default()
            });
            let unorm = self.texture_bind(&device.device, &unorm_view, "sprite2d image (unorm)");
            let srgb = self.texture_bind(&device.device, &srgb_view, "sprite2d image (srgb)");
            self.textures.insert(
                image.key,
                CachedTexture {
                    _texture: texture,
                    unorm,
                    srgb,
                    last_used: self.frame,
                },
            );
        }
    }

    /// Drops cached textures unused for more than `TEXTURE_TTL` draws.
    fn expire_textures(&mut self) {
        let frame_no = self.frame;
        self.textures
            .retain(|_, t| frame_no.saturating_sub(t.last_used) <= TEXTURE_TTL);
    }

    /// Grows the instance buffer until it holds `bytes` (a multiple of 4). The buffer is
    /// `VERTEX | COPY_DST`; callers `write_buffer` the used prefix.
    fn ensure_instance_buffer(&mut self, device: &wgpu::Device, bytes: usize) {
        let need = bytes.max(4);
        if self
            .instance_buffer
            .as_ref()
            .is_some_and(|buffer| buffer.size() >= need as u64)
        {
            return;
        }
        // Next power of two so a map that grows by a few tiles does not allocate every frame.
        let cap = need.checked_next_power_of_two().unwrap_or(need).max(256);
        self.instance_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sprite2d instances"),
            size: cap as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.instance_buffer_creates += 1;
    }

    /// Creates the sprite or blit view bind the first time. Later calls only rewrite the matrix.
    fn write_view(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        matrix: Mat4,
        blit: bool,
    ) -> Option<wgpu::BindGroup> {
        let present = if blit {
            self.blit_view.is_some()
        } else {
            self.sprite_view.is_some()
        };
        if !present {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(if blit {
                    "sprite2d blit view"
                } else {
                    "sprite2d view"
                }),
                size: 64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(if blit {
                    "sprite2d blit view"
                } else {
                    "sprite2d view"
                }),
                layout: &self.view_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            self.view_bind_creates += 1;
            let retained = RetainedView { buffer, bind };
            if blit {
                self.blit_view = Some(retained);
            } else {
                self.sprite_view = Some(retained);
            }
        }
        let view = if blit {
            self.blit_view.as_ref()
        } else {
            self.sprite_view.as_ref()
        }?;
        queue.write_buffer(&view.buffer, 0, bytemuck::cast_slice(&matrix.to_cols_array()));
        Some(view.bind.clone())
    }

    fn ensure_virtual_target(&mut self, device: &wgpu::Device, size: UVec2, format: wgpu::TextureFormat) {
        // A scene-authored virtual size beyond the device limit would be a validation error.
        let size = size.clamp(
            UVec2::ONE,
            UVec2::splat(device.limits().max_texture_dimension_2d.max(1)),
        );
        if self
            .virtual_target
            .as_ref()
            .is_some_and(|t| t.size == size && t.format == format)
        {
            return;
        }
        // Free the old target first so a resize never holds two screens at once.
        self.virtual_target = None;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sprite2d virtual screen"),
            size: wgpu::Extent3d {
                width: size.x,
                height: size.y,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.texture_bind(device, &view, "sprite2d virtual screen");
        self.virtual_target = Some(VirtualTarget {
            _texture: texture,
            view,
            bind,
            size,
            format,
        });
    }

    /// Records `frame` into `encoder`, drawing into `target` (`format`, `width`×`height`).
    ///
    /// Direct mode draws over the target (or clears it to `settings.clear`). Pixel-perfect mode
    /// renders into the virtual screen, clears the target to the bar colour and blits the virtual
    /// screen into the letterbox rectangle.
    ///
    /// The instance buffer and the view uniforms are retained and rewritten with
    /// `Queue::write_buffer`, which lands before the next submit runs. Submit `encoder` before
    /// calling `draw` again with another frame or camera: two draws recorded into one submission
    /// both read the second draw's quads and matrix.
    #[allow(clippy::too_many_arguments)] // mirrors regia_render::OverlayDraw::draw
    #[tracing::instrument(skip_all, name = "sprite2d")]
    pub fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        frame: &PreparedFrame,
        settings: &DrawSettings,
    ) {
        // Idle draws (minimized window, nothing to show) still advance the clock so unused
        // textures expire, and a left pixel-perfect mode frees its virtual screen.
        let idle = width == 0
            || height == 0
            || (frame.instances.is_empty() && settings.clear.is_none() && settings.pixel_perfect.is_none());
        if settings.pixel_perfect.is_none() {
            self.virtual_target = None;
        }
        self.frame += 1;
        if idle {
            self.expire_textures();
            return;
        }
        let d = &device.device;
        let limited = settings.with_virtual_limit(d.limits().max_texture_dimension_2d);
        let settings = &limited;
        let srgb = format.is_srgb();
        self.upload(device, frame);
        let window = UVec2::new(width, height);
        let render_size = settings.render_size(window);
        let camera = settings.effective_camera(render_size);
        let matrix = camera
            .view(Vec2::new(render_size.x as f32, render_size.y as f32))
            .matrix();
        let view_bind = self.write_view(d, &device.queue, matrix, false);
        if srgb {
            let _converted = recolor_runs(&frame.instances, &mut self.recolored);
        }
        let byte_len = std::mem::size_of::<SpriteInstance>()
            * if srgb {
                self.recolored.len()
            } else {
                frame.instances.len()
            };
        let instance_buffer = if byte_len > 0 {
            debug_assert!(byte_len.is_multiple_of(4));
            self.ensure_instance_buffer(d, byte_len);
            self.instance_buffer.clone()
        } else {
            None
        };
        if let Some(buffer) = instance_buffer.as_ref() {
            let data = if srgb {
                self.recolored.as_slice()
            } else {
                frame.instances.as_slice()
            };
            device.queue.write_buffer(buffer, 0, bytemuck::cast_slice(data));
        }
        if let Some(pp) = settings.pixel_perfect {
            self.ensure_virtual_target(d, pp.virtual_size, format);
        }
        self.pipelines(d, format);
        // Clone the handles so the later blit can rewrite its view uniform without holding
        // the pipeline map borrow.
        let (sprite_pipeline, blit_pipeline) = {
            let Some(pipelines) = self.pipelines.get(&format) else {
                return;
            };
            (pipelines.sprite.clone(), pipelines.blit.clone())
        };
        let virtual_screen = if settings.pixel_perfect.is_some() {
            self.virtual_target
                .as_ref()
                .map(|vt| (vt.view.clone(), vt.bind.clone()))
        } else {
            None
        };
        let sprite_is_virtual = settings.pixel_perfect.is_some();
        if sprite_is_virtual && virtual_screen.is_none() {
            return;
        }
        let sprite_load = if sprite_is_virtual {
            let mut clear = clear_value(settings.clear.unwrap_or([0.0; 4]), srgb);
            // Premultiply after the sRGB decode, in the space the blend operates in.
            clear.r *= clear.a;
            clear.g *= clear.a;
            clear.b *= clear.a;
            wgpu::LoadOp::Clear(clear)
        } else {
            settings
                .clear
                .map_or(wgpu::LoadOp::Load, |c| wgpu::LoadOp::Clear(clear_value(c, srgb)))
        };
        let sprite_target: &wgpu::TextureView = if sprite_is_virtual {
            let Some((view, _)) = virtual_screen.as_ref() else {
                return;
            };
            view
        } else {
            target
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sprite2d"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: sprite_target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: sprite_load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let (Some(buffer), Some(view_bind)) = (&instance_buffer, &view_bind) {
                pass.set_pipeline(&sprite_pipeline);
                pass.set_bind_group(0, view_bind, &[]);
                pass.set_vertex_buffer(0, buffer.slice(..byte_len as u64));
                for (image, range) in &frame.batches {
                    let Some(cached) = self.textures.get(&image.key) else {
                        continue;
                    };
                    pass.set_bind_group(1, if srgb { &cached.srgb } else { &cached.unorm }, &[]);
                    pass.draw(0..6, range.clone());
                }
            }
        }

        if let Some(pp) = settings.pixel_perfect {
            let Some((_, screen_bind)) = virtual_screen.as_ref() else {
                return;
            };
            let lb = pp.letterbox(window);
            // A window smaller than the virtual screen shows its centre: crop the UVs.
            let shown = Vec2::new(lb.width as f32, lb.height as f32) / lb.scale as f32;
            let full = Vec2::new(pp.virtual_size.x as f32, pp.virtual_size.y as f32);
            let uv_scale = shown / full;
            let uv_offset = (full - shown) * 0.5 / full;
            let uv_matrix = Mat4::from_translation(Vec3::new(uv_offset.x, uv_offset.y, 0.0))
                * Mat4::from_scale(Vec3::new(uv_scale.x, uv_scale.y, 1.0));
            let Some(blit_view) = self.write_view(d, &device.queue, uv_matrix, true) else {
                return;
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sprite2d letterbox"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_value(pp.bar_color, srgb)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_viewport(
                lb.x as f32,
                lb.y as f32,
                lb.width as f32,
                lb.height as f32,
                0.0,
                1.0,
            );
            pass.set_pipeline(&blit_pipeline);
            pass.set_bind_group(0, &blit_view, &[]);
            pass.set_bind_group(1, screen_bind, &[]);
            pass.draw(0..6, 0..1);
        }
    }
}

/// Renders a prepared frame offscreen and reads it back (tight RGBA8 rows): clears to `clear`
/// (as written to the target), then runs the sprite pass as a `WindowOverlay` layer — the same
/// path a window frame takes. For goldens, tools and agent captures.
///
/// # Errors
///
/// GPU readback failures.
pub fn render_offscreen(
    device: &RenderDevice,
    frame: PreparedFrame,
    settings: DrawSettings,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    clear: [f64; 4],
) -> Result<Vec<u8>, regia_render::RenderError> {
    let pass = crate::SpritePass::default();
    if let Ok(mut state) = pass.0.lock() {
        state.prepared = frame;
        state.settings = settings;
    }
    let mut overlay = regia_render::WindowOverlay::default();
    overlay.insert_shared(
        crate::SPRITE_OVERLAY_ORDER,
        "sprites2d",
        crate::sprite_overlay(&pass),
    );
    regia_render::clear_to_pixels_format(device, clear, width, height, format, Some(&overlay))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn srgb_conversion_hits_the_endpoints() {
        assert_eq!(srgb_to_linear([0.0, 1.0, 0.0, 0.25]), [0.0, 1.0, 0.0, 0.25]);
        let mid = srgb_to_linear([0.5; 4]);
        assert!((mid[0] - 0.214).abs() < 1e-3);
        assert_eq!(target_color([0.5; 4], false), [0.5; 4]);
    }

    fn white_quad(color: [f32; 4]) -> SpriteInstance {
        let mut quad = SpriteInstance::zeroed();
        quad.color = color;
        quad
    }

    /// A layer of identical tints converts once. Two tints convert twice. White stays `[1, 1, 1, 1]`
    /// through the endpoint branch (no `pow_2_4`). The old path converted every quad: 4096 calls
    /// for a white layer, 4 calls for two pairs.
    #[test]
    fn a_run_of_equal_tints_converts_once() {
        let white = [1.0, 1.0, 1.0, 1.0];
        let n = 4096;
        let layer = vec![white_quad(white); n];
        let mut dst = Vec::new();
        let conversions = recolor_runs(&layer, &mut dst);
        assert_eq!(
            conversions, 1,
            "a white layer of {n} quads used to convert {n} times"
        );
        assert_eq!(dst.len(), n);
        assert_eq!(dst[0].color, white);
        assert!(dst.iter().all(|quad| quad.color == white));

        let grey = [0.5, 0.25, 0.75, 0.8];
        let mixed = vec![
            white_quad(white),
            white_quad(white),
            white_quad(grey),
            white_quad(grey),
        ];
        let conversions = recolor_runs(&mixed, &mut dst);
        assert_eq!(conversions, 2, "two tints used to convert 4 times");
        assert_eq!(dst[0].color, white);
        assert_eq!(dst[2].color, srgb_to_linear(grey));
        assert_eq!(dst[3].color, srgb_to_linear(grey));
        // A second pass reuses `dst`'s allocation.
        let kept = dst.capacity();
        let _ = recolor_runs(&layer, &mut dst);
        assert!(dst.capacity() >= kept);
    }

    /// The sRGB→linear conversion must be bit-identical on every platform: the polynomial in
    /// `pow_2_4` replaces a libm `powf`, whose result is not required to match across operating
    /// systems (determinism audit 11-08 L3). These are the exact `f32` bit patterns the
    /// conversion produces; they change only if the algorithm changes, never per machine.
    #[test]
    fn srgb_midpoint_bits_are_fixed() {
        let cases: [([f32; 4], [u32; 3]); 6] = [
            ([0.5, 0.0, 0.0, 1.0], [0x3E5B_2D99, 0x0000_0000, 0x0000_0000]),
            ([0.25, 0.0, 0.0, 1.0], [0x3D50_6373, 0x0000_0000, 0x0000_0000]),
            ([0.0, 0.75, 0.0, 1.0], [0x0000_0000, 0x3F05_C3F9, 0x0000_0000]),
            ([0.1, 0.0, 0.0, 1.0], [0x3C24_36C8, 0x0000_0000, 0x0000_0000]),
            ([0.0, 0.0, 0.9, 1.0], [0x0000_0000, 0x0000_0000, 0x3F49_93D9]),
            ([1.0, 1.0, 1.0, 1.0], [0x3F80_0000, 0x3F80_0000, 0x3F80_0000]),
        ];
        for (input, bits) in cases {
            let out = srgb_to_linear(input);
            for channel in 0..3 {
                assert_eq!(
                    out[channel].to_bits(),
                    bits[channel],
                    "srgb_to_linear({input:?})[{channel}]"
                );
            }
            assert_eq!(out[3], input[3], "alpha is untouched");
        }
        // The linear segment is a plain divide; its boundary value stays on the linear side.
        let boundary = srgb_to_linear([0.040_45, 0.0, 0.0, 1.0]);
        assert_eq!(boundary[0], 0.040_45_f32 / 12.92);
    }

    /// A tint above 1 is an sRGB-encoded value on the same power curve (`1` = unchanged), not a
    /// clamp. Encoded 2 decodes to about 4.95. Non-finite channels must not reach the instance
    /// buffer: a NaN tint used to fall through into `pow_2_4` (a debug assert, then a NaN write).
    #[test]
    fn srgb_above_one_follows_the_curve_and_non_finite_is_zero() {
        let over = srgb_to_linear([2.0, 1.25, 0.0, 0.5]);
        assert_eq!(over[0].to_bits(), 0x409e_85e7, "encoded 2, got {over:?}");
        assert_eq!(over[1].to_bits(), 0x3fd5_3d85, "encoded 1.25, got {over:?}");
        assert_eq!(over[2], 0.0);
        assert_eq!(over[3], 0.5, "finite alpha is kept");
        assert!(over[0] > 1.0 && over[0].is_finite());
        // A finite encoded channel can still overflow f32 on the power curve. That must saturate,
        // not become +inf in the instance buffer.
        let huge = srgb_to_linear([1.0e20, 0.0, 0.0, 1.0]);
        assert_eq!(huge[0], f32::MAX, "encoded 1e20, got {huge:?}");
        assert!(huge[0].is_finite());
        let bad = srgb_to_linear([f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN]);
        assert_eq!(bad, [0.0, 0.0, 0.0, 0.0]);
    }

    /// A struct-literal `PixelPerfect` with a zero size bypasses `PixelPerfect::new`'s clamp; the
    /// draw path must still never ask for a 0×0 texture (a wgpu validation panic; audit 11-03 L3).
    #[test]
    fn a_zero_virtual_size_is_clamped_to_one_pixel() {
        let settings = DrawSettings {
            pixel_perfect: Some(PixelPerfect {
                virtual_size: UVec2::new(0, 180),
                bar_color: [0.0; 4],
            }),
            ..DrawSettings::default()
        };
        let limited = settings.with_virtual_limit(8192);
        assert_eq!(limited.render_size(UVec2::new(1280, 720)), UVec2::new(1, 180));
    }

    #[test]
    fn render_size_is_the_virtual_size_in_pixel_perfect_mode() {
        let mut s = DrawSettings::default();
        assert_eq!(s.render_size(UVec2::new(800, 600)), UVec2::new(800, 600));
        s.pixel_perfect = Some(PixelPerfect::new(320, 180));
        assert_eq!(s.render_size(UVec2::new(800, 600)), UVec2::new(320, 180));
    }

    #[test]
    fn virtual_limit_clamps_the_size_every_consumer_sees() {
        let s = DrawSettings {
            pixel_perfect: Some(PixelPerfect::new(16384, 100)),
            ..DrawSettings::default()
        };
        let c = s.with_virtual_limit(8192);
        assert_eq!(c.render_size(UVec2::new(1920, 1080)), UVec2::new(8192, 100));
        let lb = c.pixel_perfect.expect("pp").letterbox(UVec2::new(1920, 1080));
        assert_eq!(lb.scale, 1);
        // Within the limit nothing changes; no pixel-perfect mode stays none.
        assert_eq!(
            s.with_virtual_limit(32768).render_size(UVec2::ONE),
            UVec2::new(16384, 100)
        );
        assert!(
            DrawSettings::default()
                .with_virtual_limit(8)
                .pixel_perfect
                .is_none()
        );
    }

    /// ADR-026: no Function-storage array with an explicit layout (the old indexed corner table).
    #[test]
    fn shader_has_no_function_layout_errors() {
        let found =
            regia_render::wgsl_function_layout_errors("sprite2d.wgsl", SHADER).expect("sprite2d emits");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn shader_declares_every_entry_point() {
        // The device validates the whole module in the GPU tests (tests/sprite_pass.rs); this only
        // guards the entry-point names the pipelines use.
        for entry in ["vs_sprite", "fs_sprite", "vs_blit", "fs_blit"] {
            assert!(SHADER.contains(&format!("fn {entry}(")), "{entry} missing");
        }
    }
}
