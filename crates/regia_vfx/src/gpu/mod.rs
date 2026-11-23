// SPDX-License-Identifier: MPL-2.0
//! GPU layer: buffers, pipelines and frame recording for particles, decals and trails.
//!
//! ## Integration hook (hosts: `regia_render`'s `TranslucentLayer`, tests, samples)
//!
//! `VfxGpu` owns every GPU resource this crate needs and is driven in two steps:
//!
//! 1. [`VfxGpu::set_depth_view`] (the scene depth) + [`VfxGpu::set_frame`] +
//!    [`VfxGpu::simulate`] once per frame, before the host's colour pass (it records compute
//!    passes onto the host's encoder; the scene depth view is bound for depth-collision and
//!    soft particles);
//! 2. [`VfxGpu::draw_decals`] + [`VfxGpu::draw_particles`] + [`VfxGpu::draw_trails`] inside the
//!    host's transparent render pass, after opaques, with [`VfxCamera`] as the camera contract.
//!
//! The render pass must have a colour attachment in any 8-bit (sRGB or Unorm) format —
//! pipelines are built lazily per format on first use ([`VfxGpu::formats`]), or ahead of the
//! first effect by [`VfxGpu::warm`] / [`vfx_warm_job`] — and a **read-only**
//! `Depth32Float` depth attachment (`depth_ops: None`), because the shaders also sample that
//! depth for soft particles, decal projection and depth collision. The M5 runtime path drives
//! this through [`regia_render::TranslucentLayer`] (see [`crate::runtime`]); the offscreen
//! reference consumer is [`crate::offscreen::VfxHarness`].

pub mod bounds;
pub mod events;
pub mod layout;
pub mod mesh_draw;
pub mod radix_sort;
pub mod shaders;
mod soft;
mod warm;

#[cfg(test)]
pub(crate) mod determinism_tests;

use regia_core::math::{Mat4, Vec3};
use regia_core::tasks::block_on;
use regia_render::{RenderDevice, TrackedDevice};

use shaders::{
    BILLBOARD, COMMON, CUBE_VERT, DECAL, LUT_FNS, MESH_PARTICLE, PARTICLE_RENDER_BINDINGS, SIM_BINDINGS,
    SIM_ENTRY, TRAIL,
};

pub use warm::{VfxWarmReport, vfx_warm_job, warm_shared};

pub use layout::{
    COUNTER_ADDITIVE, COUNTER_ALIVE, COUNTER_ALPHA, COUNTER_COUNT, COUNTER_MESH, DecalGpu, DecalTileGpu,
    EmitterGpu, FrameGpu, MAX_DECALS, MAX_EMITTERS, MAX_SPAWN_JOBS, SpawnJobGpu, TrailVertex,
};

use crate::curves::CurveLut;

/// Size in bytes of the GPU scalar-curve buffer.
const CURVE_SCALAR_BYTES: u64 = 64 * 1024;
/// Size in bytes of the GPU color-curve buffer.
const CURVE_COLOR_BYTES: u64 = 16 * 1024 * 16;

/// Whether a curve LUT of these byte sizes fits the GPU curve buffers (`write_buffer` panics
/// on an oversized slice, which aborts a shipping build).
fn curve_lut_fits(scalar_bytes: u64, color_bytes: u64) -> bool {
    scalar_bytes <= CURVE_SCALAR_BYTES && color_bytes <= CURVE_COLOR_BYTES
}
use crate::decal::{Decal, DecalSurface};
use crate::quality::EXPOSURE_FLOOR;
use crate::sim::SpawnJob;

/// Updates a queued spawn job may wait without a drawn frame before the queue is dropped
/// ([`VfxGpu::note_upload`]).
pub const STALE_UPLOADS: u32 = 8;

/// Frames a new context counts as live with nothing in it ([`VfxGpu::is_idle`]), so the host
/// frame builds the layer's targets, bind groups and passes on its first frames (the warm-up)
/// and the first effect spawned later creates none (`specs/27` § 2.4, ADR-2585): the frame
/// warm's two 64² frames and its window-size frame, then the first game frames, with room for a
/// probe capture or a loading frame before them. The runtime re-arms them when the window size
/// changes. 0 ([`VfxGpu::set_layer_warm_frames`]) leaves an empty layer without any target until
/// its first effect.
pub const LAYER_WARM_FRAMES: u32 = 8;

/// Seconds past the longest particle lifetime a spawn keeps the context live: the update
/// window's own margin ([`VfxGpu::is_idle`]).
const LIVE_MARGIN_S: f32 = 0.25;

/// WGSL modules the VFX renderer builds, in the same concatenation order as the pipelines.
///
/// `cargo xtask check-shaders` validates each entry. The harness backdrop is the
/// offscreen reference shader, not part of a particle pipeline.
#[must_use]
pub fn shader_sources() -> Vec<(String, String)> {
    vec![
        (
            "vfx sim".to_string(),
            format!("{COMMON}{SIM_BINDINGS}{LUT_FNS}{SIM_ENTRY}"),
        ),
        ("vfx sort".to_string(), shaders::SORT.to_string()),
        radix_sort::shader_sources()[0].clone(),
        radix_sort::shader_sources()[1].clone(),
        ("vfx prepare".to_string(), shaders::PREPARE.to_string()),
        ("vfx gather".to_string(), format!("{COMMON}{}", shaders::GATHER)),
        events::prepare_source(),
        events::emit_source(),
        (
            "vfx particle render".to_string(),
            format!("{COMMON}{PARTICLE_RENDER_BINDINGS}{LUT_FNS}{BILLBOARD}"),
        ),
        (
            "vfx mesh particle render".to_string(),
            format!("{COMMON}{PARTICLE_RENDER_BINDINGS}{LUT_FNS}{CUBE_VERT}{MESH_PARTICLE}"),
        ),
        ("vfx mesh bucket".to_string(), mesh_draw::MESH_BUCKET.to_string()),
        ("vfx decal".to_string(), format!("{COMMON}{CUBE_VERT}{DECAL}")),
        ("vfx trail".to_string(), format!("{COMMON}{TRAIL}")),
        (
            "vfx harness".to_string(),
            include_str!("harness.wgsl").to_string(),
        ),
        crate::precip::precip_shader_source(),
        crate::ambient::ambient_shader_source(),
        // The soft-particle bilateral upsample (`soft.rs`), so `cargo xtask check-shaders` runs
        // `spirv-val` on it too (ADR-869).
        (
            "vfx soft upsample".to_string(),
            include_str!("soft_half_res.wgsl").to_string(),
        ),
        // The opt-in particle bounds reduction (P-225, ADR-8355).
        bounds::bounds_source(),
    ]
}

/// Pool slots per workgroup of the ordered list gather (`shaders::GATHER`'s `GATHER_BLOCK`).
const GATHER_BLOCK: u32 = 256;

/// The ordered list gather (ADR-5991): the three kernels of `shaders::GATHER` and their bind
/// group (counters, lists, block offsets).
struct ListGather {
    count: wgpu::ComputePipeline,
    scan: wgpu::ComputePipeline,
    write: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    /// Kept alive with the bind group that references it.
    _blocks: wgpu::Buffer,
}

/// The per-target render pipelines of one color format.
struct RenderPipes {
    billboard_haze: wgpu::RenderPipeline,
    billboard_add: wgpu::RenderPipeline,
    billboard_alpha: wgpu::RenderPipeline,
    mesh: wgpu::RenderPipeline,
    /// Registered mesh assets (`.vfx.ron` v8 `render.mesh`), built with the rest so the warm
    /// covers it (ADR-7261).
    mesh_asset: wgpu::RenderPipeline,
    decal: wgpu::RenderPipeline,
    trail: wgpu::RenderPipeline,
}

/// The format-independent inputs of [`RenderPipes`] (shared handles): what compiling one
/// colour format's pipelines needs, so a warm job ([`warm::vfx_warm_job`]) compiles them
/// without holding the context.
#[derive(Clone)]
struct PipeSource {
    device: TrackedDevice,
    sort_n: u32,
    pl_particles: wgpu::PipelineLayout,
    pl_mesh: wgpu::PipelineLayout,
    pl_decal: wgpu::PipelineLayout,
    pl_trail: wgpu::PipelineLayout,
    particle_module: wgpu::ShaderModule,
    mesh_module: wgpu::ShaderModule,
    decal_module: wgpu::ShaderModule,
    trail_module: wgpu::ShaderModule,
}

impl PipeSource {
    /// Number of pipelines in one [`RenderPipes`].
    const PIPELINES: u32 = 7;

    /// Builds the render pipelines of `format`. With `cancellable` (a warm), returns `Ok(None)`
    /// between two compiles once the warm queue running it shuts down
    /// ([`regia_render::warm_cancelled`]).
    fn build(&self, format: wgpu::TextureFormat, cancellable: bool) -> Result<Option<RenderPipes>, VfxError> {
        let stop = || cancellable && regia_render::warm_cancelled();
        let d = &self.device;
        let error_scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let premult_alpha = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let depth = || wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        };
        let mk_billboard = |label: &str, list_base: u32, haze: u32, blend: wgpu::BlendState| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&self.pl_particles),
                vertex: wgpu::VertexState {
                    module: &self.particle_module,
                    entry_point: Some("vs_billboard"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &[("list_base", list_base as f64), ("haze_pass", haze as f64)],
                        ..Default::default()
                    },
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.particle_module,
                    entry_point: Some("fs_billboard"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    // Overrides are per stage: the fragment needs `haze_pass` too.
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &[("haze_pass", haze as f64)],
                        ..Default::default()
                    },
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(depth()),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
            })
        };
        let billboard_haze = mk_billboard("vfx billboard haze", 0, 1, premult_alpha);
        if stop() {
            return Ok(None);
        }
        let billboard_add = mk_billboard("vfx billboard additive", 0, 0, additive);
        if stop() {
            return Ok(None);
        }
        let billboard_alpha = mk_billboard("vfx billboard alpha", self.sort_n, 0, premult_alpha);
        if stop() {
            return Ok(None);
        }
        let mesh = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx mesh particles"),
            layout: Some(&self.pl_mesh),
            vertex: wgpu::VertexState {
                module: &self.mesh_module,
                entry_point: Some("vs_mesh"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.mesh_module,
                entry_point: Some("fs_mesh"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(premult_alpha),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(depth()),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
        });
        if stop() {
            return Ok(None);
        }
        let mesh_asset = mesh_draw::build_pipeline(d, &self.pl_mesh, &self.mesh_module, format);
        if stop() {
            return Ok(None);
        }
        let decal = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx decals"),
            layout: Some(&self.pl_decal),
            vertex: wgpu::VertexState {
                module: &self.decal_module,
                entry_point: Some("vs_decal"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.decal_module,
                entry_point: Some("fs_decal"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(premult_alpha),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(depth()),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
        });
        if stop() {
            return Ok(None);
        }
        let trail = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx trails"),
            layout: Some(&self.pl_trail),
            vertex: wgpu::VertexState {
                module: &self.trail_module,
                entry_point: Some("vs_trail"),
                buffers: &[Some(layout::TrailVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.trail_module,
                entry_point: Some("fs_trail"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(premult_alpha),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(depth()),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
        });
        let validation = block_on(error_scope.pop());
        if let Some(err) = validation {
            return Err(VfxError::Shader(err.to_string()));
        }
        Ok(Some(RenderPipes {
            billboard_haze,
            billboard_add,
            billboard_alpha,
            mesh,
            mesh_asset,
            decal,
            trail,
        }))
    }
}

/// How the alpha-blended particle list is put in back-to-front order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortAlgorithm {
    /// GPU radix sort of the live keys ([`radix_sort::GpuRadixSort`], ported from
    /// FidelityFX Parallel Sort): cost follows what is alive. Opt-in until the RTX 5090
    /// `vfx_lab --sort-bench` numbers are in (see the decisions log).
    Radix,
    /// Batcher bitonic merge over the whole pool width (next power of two of the
    /// capacity), whatever is alive. The default.
    #[default]
    Bitonic,
}

/// Errors from GPU setup.
#[derive(Debug, thiserror::Error)]
pub enum VfxError {
    /// A shader failed to compile (message from the driver).
    #[error("vfx shader validation failed: {0}")]
    Shader(String),
    /// The requested pool size is not usable.
    #[error("vfx pool capacity {0} is out of range (1..=1_000_000)")]
    Capacity(u32),
    /// A render target would be larger than the device allows (or has a zero side).
    #[error("vfx render target {width}x{height} is outside the device limit (1..={max} per side)")]
    TextureSize {
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
        /// The device's `max_texture_dimension_2d`.
        max: u32,
    },
}

/// Rejects a render-target size the device cannot create, before `create_texture` would
/// report it through the uncaptured-error hook (which may abort).
///
/// # Errors
///
/// [`VfxError::TextureSize`] when a side is 0 or above `max`.
pub fn check_texture_extent(width: u32, height: u32, max: u32) -> Result<(), VfxError> {
    if width == 0 || height == 0 || width > max || height > max {
        return Err(VfxError::TextureSize { width, height, max });
    }
    Ok(())
}

/// Key bits of the mobile emitter-order pass: an emitter rank below [`MAX_EMITTERS`].
const EMITTER_RANK_BITS: u32 = 8;
const _: () = assert!(MAX_EMITTERS <= 1 << EMITTER_RANK_BITS);

/// Quality profile: pool sizes, the alpha sort, and the smoke / screen-size budgets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VfxProfile {
    /// Live-particle pool capacity (the global budget).
    pub particle_capacity: u32,
    /// Decal pool capacity.
    pub decal_capacity: u32,
    /// Bitonic sort for alpha-blended particles (desktop). Without it (mobile) the alpha list
    /// draws emitter by emitter, back to front per view, ranked on the CPU (ADR-7210).
    pub sort: bool,
    /// Particle-light budget.
    pub light_budget: u32,
    /// Minimum on-screen size in pixels when an emitter's own floor is 0. Stops sub-pixel shimmer.
    pub screen_floor_px: f32,
    /// Live alpha-blended particles (smoke) the runtime plugin will schedule. The bench stress
    /// path does not use this cap; it fills [`Self::particle_capacity`].
    pub smoke_budget: u32,
    /// Resolution of the soft-particle layer: the soft, low-frequency alpha particles (dust,
    /// smoke, billows; [`crate::scene::is_soft_emitter`]) draw into an offscreen layer at this
    /// fraction of the frame and are composited with a depth-aware bilateral upsample. Sparks,
    /// tracers, muzzle cores and fire stay full resolution. Desktop High: half; Ultra: full;
    /// Mobile: quarter. Applies on HDR frames (the runtime) and in the offscreen harness.
    pub soft_particle_res: SoftParticleRes,
    /// Events one frame can fire before the rest are counted lost (`.vfx.ron` v6 event
    /// spawns), clamped to `16..=65536`. Every fired event is staged — the GPU gives each pool
    /// slot three staging ranks (`slot_ordinal * 3 + n`) — and `event_prepare` keeps the first
    /// `event_capacity` fired events in slot order, so the budget counts events, not staging
    /// ranks, and a sparse frame keeps every event it fires however late its slots sit in the
    /// window (Muse review of fix-vfx1-determinism, finding 1).
    pub event_capacity: u32,
    /// Slots of the child ring event spawns draw from, on top of
    /// [`Self::particle_capacity`], clamped to `16..=65536`. A live child is overwritten when the
    /// ring wraps; both losses are counted ([`VfxPoolStats`]).
    pub child_capacity: u32,
}

/// Resolution of the soft-particle layer ([`VfxProfile::soft_particle_res`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SoftParticleRes {
    /// Everything at full resolution (no offscreen layer).
    #[default]
    Full,
    /// The soft layer at half the frame size (a quarter of the pixels).
    Half,
    /// The soft layer at a quarter of the frame size (a sixteenth of the pixels).
    Quarter,
}

impl SoftParticleRes {
    /// The frame-size divisor (1, 2 or 4).
    #[must_use]
    pub fn divisor(self) -> u32 {
        match self {
            Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
        }
    }

    /// Lower-case name (`full`, `half`, `quarter`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Half => "half",
            Self::Quarter => "quarter",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Full, Self::Half, Self::Quarter]
            .into_iter()
            .find(|r| r.name() == name)
    }
}

impl VfxProfile {
    /// The desktop-High reference of `specs/26` § 6 (100k live particles, 256 decals).
    pub fn desktop_high() -> Self {
        Self {
            particle_capacity: 100_000,
            decal_capacity: 256,
            sort: true,
            light_budget: 32,
            screen_floor_px: 2.0,
            smoke_budget: 8_192,
            soft_particle_res: SoftParticleRes::Half,
            event_capacity: 8_192,
            child_capacity: 16_384,
        }
    }

    /// Desktop Ultra: the same particle budget as High, with the full decal cap.
    pub fn desktop_ultra() -> Self {
        Self {
            particle_capacity: 100_000,
            decal_capacity: MAX_DECALS,
            sort: true,
            light_budget: 64,
            screen_floor_px: 2.0,
            smoke_budget: 8_192,
            soft_particle_res: SoftParticleRes::Full,
            event_capacity: 8_192,
            child_capacity: 16_384,
        }
    }

    /// The mobile-High reference of `specs/26` § 6 (10k live particles, 64 decals).
    pub fn mobile_high() -> Self {
        Self {
            particle_capacity: 10_000,
            decal_capacity: 64,
            sort: false,
            light_budget: 16,
            screen_floor_px: 1.5,
            smoke_budget: 2_048,
            soft_particle_res: SoftParticleRes::Quarter,
            event_capacity: 1_024,
            child_capacity: 2_048,
        }
    }
}

/// Scene lighting copied onto particles. The harness default matches its sky disc.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VfxLighting {
    /// World-space direction from the scene toward the sun.
    pub sun_direction: [f32; 3],
    /// Linear sun colour.
    pub sun_color: [f32; 3],
    /// Scalar ambient in `0..=1` (display-referred).
    pub ambient: f32,
    /// Exposure multiplier the post pass will apply. Particle colour is divided by this when
    /// the draw target is the pre-exposure HDR buffer. Values below [`EXPOSURE_FLOOR`] are
    /// raised. Daylight EV 15 (about `2.5e-5`) passes through. The offscreen harness uses `1`.
    pub exposure: f32,
}

impl Default for VfxLighting {
    fn default() -> Self {
        let len = ((-0.45f32) * -0.45 + 0.62 * 0.62 + 0.42 * 0.42).sqrt();
        Self {
            sun_direction: [-0.45 / len, 0.62 / len, 0.42 / len],
            sun_color: [1.05, 0.98, 0.85],
            ambient: 0.5,
            exposure: 1.0,
        }
    }
}

impl Default for VfxProfile {
    fn default() -> Self {
        Self::desktop_high()
    }
}

/// The camera contract between the host renderer and the VFX draw calls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VfxCamera {
    /// Eye position.
    pub eye: Vec3,
    /// Looked-at point.
    pub target: Vec3,
    /// Vertical field of view (radians).
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
}

impl VfxCamera {
    /// A camera looking at `target` from `eye`.
    pub fn looking_at(eye: Vec3, target: Vec3) -> Self {
        Self {
            eye,
            target,
            fov_y: 60.0f32.to_radians(),
            near: 0.1,
            far: 250.0,
        }
    }

    /// View, projection and view-projection for a viewport (`wgpu`-style RH 0..1 depth).
    pub fn matrices(&self, aspect: f32) -> (Mat4, Mat4, Mat4) {
        let view = regia_render::view_math::look_at_view(self.eye, self.target, Vec3::Y);
        // The renderer's clip planes (ADR-869): `far = ∞` or a zero near plane would be NaN here.
        let (fov_y, near, far) = regia_render::view_math::usable_projection(self.fov_y, self.near, self.far);
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov_y, aspect, near, far);
        let view_proj = proj * view;
        (view, proj, view_proj)
    }
}

fn next_pow2(n: u32) -> u32 {
    n.max(1).next_power_of_two()
}

/// Advances the session clock by `dt`; a non-finite `dt` leaves it unchanged so one bad frame
/// cannot poison the clock (NaN never compares equal, which also grows the spawn history).
fn advance_clock(time: f32, dt: f32) -> f32 {
    if dt.is_finite() { time + dt } else { time }
}

/// Adds `spawned` jobs at `time` to the spawn history. Frames that spawn nothing add no entry,
/// and frames at the same `time` (a paused game writes `dt = 0`) share one, so the history
/// stays bounded by distinct spawn times rather than growing one entry per paused frame.
fn record_spawn(history: &mut std::collections::VecDeque<(f32, u32)>, time: f32, spawned: u32) {
    if spawned == 0 {
        return;
    }
    match history.back_mut() {
        Some((t, n)) if *t == time => *n = n.saturating_add(spawned),
        _ => history.push_back((time, spawned)),
    }
}

/// What [`spawn_window`] reads of a pool: the ring counter after this frame's spawns, the clock,
/// the longest lifetime and the ring size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SpawnClock {
    /// The ring counter after this frame's spawns.
    pub(crate) cursor: u32,
    /// The pool's clock, seconds.
    pub(crate) time: f32,
    /// The longest particle lifetime (`None`: no window).
    pub(crate) max_lifetime: Option<f32>,
    /// Main-ring slots.
    pub(crate) capacity: u32,
}

/// A frame's update window `(first slot, slots)` after `spawned` jobs (`VfxGpu`'s ring and the CPU
/// probe share it, ADR-7431): the slots spawned within the longest lifetime plus a quarter second,
/// or the whole pool `(0, capacity)` without a lifetime, when the window fills the pool, or when
/// the 32-bit ring counter wrapped inside it (its slot sequence then jumps). `history` keeps the
/// spawn counts by time between frames.
pub(crate) fn spawn_window(
    history: &mut std::collections::VecDeque<(f32, u32)>,
    clock: SpawnClock,
    spawned: u32,
) -> (u32, u32) {
    let full = (0, clock.capacity);
    let Some(life) = clock.max_lifetime else {
        // No window without a lifetime: keep no history either (it would never be pruned).
        history.clear();
        return full;
    };
    record_spawn(history, clock.time, spawned);
    let horizon = clock.time - (life + 0.25);
    while history.front().is_some_and(|(t, _)| *t < horizon) {
        history.pop_front();
    }
    let window: u64 = history.iter().map(|(_, n)| u64::from(*n)).sum();
    if window >= u64::from(clock.capacity) {
        return full;
    }
    let window = window as u32;
    let first = clock.cursor.wrapping_sub(window);
    // The window's counter values are `first..cursor`; a wrap inside it shows as
    // `first > cursor`, and the slot sequence jumps there.
    if first > clock.cursor {
        return full;
    }
    (first % clock.capacity, window.max(1))
}

/// Smallest sort list size: one 64-thread workgroup. The bitonic sort and its bound
/// (`clamp(64, sort_n)`) work in whole groups, so a pool of fewer than 64 particles still gets a
/// 64-slot list (padded with sentinels) rather than an inverted clamp range.
const MIN_SORT_N: u32 = 64;

/// Sort list size for a pool of `capacity` particles: the next power of two, at least
/// `MIN_SORT_N`.
fn sort_list_size(capacity: u32) -> u32 {
    next_pow2(capacity).max(MIN_SORT_N)
}

/// Keys a bitonic sort of `live` particles covers: the next power of two above `live` with a
/// 25 % + 512 margin, within `64..=sort_n`.
fn sort_bound(live: u32, sort_n: u32) -> u32 {
    let padded = live.saturating_add(live / 4).saturating_add(512);
    padded
        .checked_next_power_of_two()
        .unwrap_or(sort_n)
        .clamp(64, sort_n)
}

struct SimBuffers {
    emitters: wgpu::Buffer,
    pos_age: wgpu::Buffer,
    vel_life: wgpu::Buffer,
    attrib: wgpu::Buffer,
    color0: wgpu::Buffer,
    spawn_jobs: wgpu::Buffer,
    counters: wgpu::Buffer,
    stats: wgpu::Buffer,
    lists: wgpu::Buffer,
    curves_scalars: wgpu::Buffer,
    curves_colors: wgpu::Buffer,
    /// The event buffer (`.vfx.ron` v6 event spawns). Every fired event is staged at the rank
    /// `slot_ordinal * EVENTS_PER_SLOT + n` (at most three events per pool slot), so the buffer
    /// spans all `3 * (capacity + child_capacity)` staging ranks; `event_prepare` compacts the
    /// first `event_capacity` fired events in slot order into the front of the same buffer for
    /// `child_emit`.
    events: wgpu::Buffer,
    /// The simulated step each staged `events` rank was written at (`event_prepare` skips stale
    /// ranks). Sized like `events`.
    event_epoch: wgpu::Buffer,
    /// One bit per pool slot, double-buffered by the event step's parity: `update` marks every
    /// slot that fired an event so `event_prepare` scans only fired slots; the prepare clears
    /// each word it consumes, in both parities, so the next scan never revisits them.
    event_mask: wgpu::Buffer,
}

/// [`VfxGpu::read_pool_stats`]: the particle pool after one simulated frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VfxPoolStats {
    /// Additive (and heat-haze) particles drawn this frame.
    pub additive: u32,
    /// Alpha-blended (sorted) particles drawn this frame.
    pub alpha: u32,
    /// Mesh particles drawn this frame.
    pub mesh: u32,
    /// Live particles this frame.
    pub alive: u32,
    /// Spawn jobs the emit kernel has run since the context was created (wrapping).
    pub spawned: u32,
    /// Particles lost to the ring since the context was created (wrapping): a live particle
    /// overwritten by a new spawn, or a job past the pool size in one frame. Zero while the
    /// CPU budgets keep the live count at or below the pool capacity.
    pub dropped: u32,
    /// Events fired since the context was created (wrapping), overflow included.
    pub events_fired: u32,
    /// Events lost to a full event buffer since the context was created (wrapping).
    pub events_overflow: u32,
    /// Child particles spawned since the context was created (wrapping).
    pub children_spawned: u32,
    /// Child particles lost since the context was created (wrapping): a live child overwritten
    /// when the child ring wraps, or children past one ring in one frame.
    pub children_dropped: u32,
}

/// [`VfxGpu::container_sizes`]: lengths of the context's per-frame CPU containers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VfxGpuSizes {
    /// Spawn jobs staged for the next drawn frame.
    pub spawn_jobs: usize,
    /// Capacity of the spawn-job staging vector.
    pub spawn_jobs_capacity: usize,
    /// `(time, jobs)` entries of the update-window history.
    pub spawn_history: usize,
    /// Staged trail vertices.
    pub trail_vertices: usize,
    /// Staged decals.
    pub pending_decals: usize,
    /// Render pipeline sets (one per colour format).
    pub render_pipes: usize,
    /// Free emitter-record slots.
    pub free_emitter_slots: usize,
}

/// The particle pool of a [`VfxGpu`] at one moment ([`VfxGpu::capture_pool`], ADR-2633): copies of
/// the four pool buffers and the CPU state that says where the simulation stands.
pub struct VfxPoolCapture {
    pools: [wgpu::Buffer; 4],
    time: f32,
    spawn_cursor: u32,
    spawn_history: std::collections::VecDeque<(f32, u32)>,
    /// The clock at the last spawn: whether a particle may still live is judged from it, so it
    /// goes back with the clock (ADR-2687).
    last_spawn: Option<f32>,
    /// The clock when the last spawned particle ends ([`VfxGpu::restart_idle_clock`]); goes back
    /// with the clock too (ADR-2814).
    spawned_life_end: Option<f32>,
    update_window: (u32, u32),
    emitter_records: Vec<EmitterGpu>,
    emitter_slots: EmitterAllocator,
    spawn_cpu: Vec<SpawnJobGpu>,
    uploads_since_frame: u32,
    pending_shift: Vec3,
}

/// Everything GPU-side for one VFX context. Created once per device + surface format.
pub struct VfxGpu {
    // Counted: frame-path bind groups and lazy pipelines show in the hitch counters.
    device: TrackedDevice,
    queue: wgpu::Queue,
    capacity: u32,
    sort_enabled: bool,
    sort_n: u32,

    frame_cpu: FrameGpu,
    frame_ub: wgpu::Buffer,
    /// Frame uniform with a half-resolution viewport (the opt-in soft-particle pass).
    frame_ub_half: wgpu::Buffer,
    sim: SimBuffers,
    indirect: wgpu::Buffer,
    decals: wgpu::Buffer,
    /// One [`DecalTileGpu`] per drawn decal (the tile-union quad).
    decal_tiles: wgpu::Buffer,
    trail_verts: wgpu::Buffer,
    trail_count: u32,
    /// Trail vertices staged by [`VfxGpu::write_trails`], uploaded when a frame is drawn
    /// ([`VfxGpu::set_frame`] or [`VfxGpu::draw_trails`]) and not before: a host that updates
    /// without rendering (hidden window) queues no buffer writes.
    trail_cpu: Vec<TrailVertex>,
    trails_dirty: bool,
    decal_count: u32,
    /// Pixel size of the largest tile-union quad from the last upload.
    clip_w: u32,
    clip_h: u32,
    decal_capacity: u32,
    pending_decals: Vec<Decal>,
    pending_surfaces: Vec<DecalSurface>,
    decal_atlas: wgpu::TextureView,
    decal_atlas_sampler: wgpu::Sampler,
    custom_decal_atlas: Option<std::sync::Arc<crate::decal_texture::DecalTextureAtlas>>,
    dummy_receiver: wgpu::TextureView,
    dummy_scene: wgpu::TextureView,
    /// Scene copy heat-haze cards sample (`None`: haze off, the dummy is bound).
    haze_view: Option<wgpu::TextureView>,
    /// Host switch for heat haze ([`Self::set_haze_look`]); `set_haze(true)` is ignored when off.
    haze_allowed: bool,
    receiver_view: Option<wgpu::TextureView>,
    scene_view: Option<wgpu::TextureView>,
    /// Last upload: pending count, culled count, tiles touched.
    cull_live: u32,
    cull_culled: u32,
    cull_tiles: u32,
    /// `(byte offset into the sort uniform buffer, k, j)` per Batcher merge step.
    sort_steps: Vec<(u32, u32, u32)>,
    /// `(byte offset, stage k)` per shared-memory stage (`k = 0`: the whole 512-key tile sort).
    sort_local_steps: Vec<(u32, u32)>,
    /// The shared-memory bitonic kernel (`sort_local`).
    pipe_sort_local: wgpu::ComputePipeline,
    /// Keys the bitonic sort covers this frame: a power of two at or above the live alpha
    /// count ([`VfxGpu::set_alpha_bound`]), at most `sort_n`.
    sort_live_n: u32,
    /// Byte offset of the mesh list's copy of the sort uniform blocks.
    mesh_sort_offset: u32,
    /// Keys the mesh-list sort covers this frame ([`VfxGpu::set_mesh_bound`]); 0 skips it.
    mesh_sort_n: u32,
    /// Soft particles are alive this frame ([`VfxGpu::set_soft_live`]).
    soft_live: bool,
    /// The ring cursor: spawn jobs pushed so far, wrapping. Each frame's jobs land from
    /// `spawn_cursor % capacity` on (`FrameGpu::window`), so the GPU follows it exactly.
    spawn_cursor: u32,
    /// `(time, spawn jobs)` per frame within the longest lifetime.
    spawn_history: std::collections::VecDeque<(f32, u32)>,
    /// The library's longest particle lifetime ([`VfxGpu::set_max_lifetime`]).
    max_lifetime: Option<f32>,
    /// Clock time of the last frame that spawned ([`VfxGpu::is_idle`]).
    last_spawn: Option<f32>,
    /// The clock when the longest-lived particle spawned so far ends (ADR-2814).
    spawned_life_end: Option<f32>,
    /// Frames left that count as live with nothing in them ([`LAYER_WARM_FRAMES`]).
    layer_warm_frames: u32,
    /// The warm frames waiting while a render-out job runs ([`VfxGpu::hold_layer_warm_frames`]).
    held_warm_frames: Option<u32>,
    /// The CPU scene has a live instance ([`VfxGpu::set_scene_live`]).
    scene_live: bool,
    /// `(first slot, count)` the update kernel covers this frame.
    update_window: (u32, u32),
    stats_read: wgpu::Buffer,

    atlas: wgpu::TextureView,
    atlas_sampler: wgpu::Sampler,
    fireball_sheet: wgpu::TextureView,
    fireball_sampler: wgpu::Sampler,
    depth_sampler: wgpu::Sampler,
    dummy_depth: wgpu::TextureView,
    bound_depth: Option<wgpu::TextureView>,

    // layouts kept for per-frame rebinds (depth view changes rebuild these bind groups)
    bgl_sim: wgpu::BindGroupLayout,
    bgl_particles: wgpu::BindGroupLayout,
    bgl_decal: wgpu::BindGroupLayout,
    bg_frame: wgpu::BindGroup,
    /// The sprite atlas for trails and ribbons (group 1 of the trail pipeline).
    bg_trail: wgpu::BindGroup,
    bg_frame_half: wgpu::BindGroup,
    bg_sim: wgpu::BindGroup,
    bg_sort_data: wgpu::BindGroup,
    bg_sort_params: wgpu::BindGroup,
    bg_prepare: wgpu::BindGroup,
    bg_particles: Option<wgpu::BindGroup>,
    /// [`Self::bg_particles`] with the bucketed mesh-asset list bound as `lists` (ADR-7261);
    /// built once a render mesh is registered.
    bg_mesh_assets: Option<wgpu::BindGroup>,
    bg_decal: Option<wgpu::BindGroup>,

    // render pipeline layouts (format-independent) + pipelines per colour format (lazily built)
    pl_particles: wgpu::PipelineLayout,
    pl_mesh: wgpu::PipelineLayout,
    pl_decal: wgpu::PipelineLayout,
    pl_trail: wgpu::PipelineLayout,
    particle_module: wgpu::ShaderModule,
    mesh_module: wgpu::ShaderModule,
    decal_module: wgpu::ShaderModule,
    trail_module: wgpu::ShaderModule,
    render_pipes: std::collections::HashMap<wgpu::TextureFormat, RenderPipes>,

    // pipelines
    pipe_emit: wgpu::ComputePipeline,
    pipe_update: wgpu::ComputePipeline,
    /// Moves the live pool on a floating-origin rebase frame.
    pipe_shift: wgpu::ComputePipeline,
    /// The rebase shift the next [`Self::set_frame`] hands to `shift_pool` (zero: none).
    pending_shift: Vec3,
    pipe_pad: wgpu::ComputePipeline,
    pipe_sort: wgpu::ComputePipeline,
    /// Radix sort of the alpha list (built when the profile sorts).
    radix: Option<radix_sort::GpuRadixSort>,
    /// Profiles without the alpha sort (mobile): an 8-bit radix pass that puts the alpha list
    /// in the per-view emitter order [`VfxGpu::set_frame`] ranks on the CPU (`specs/26` § 3,
    /// ADR-7210). Particles keep their gather order inside one emitter.
    emitter_order: Option<radix_sort::GpuRadixSort>,
    /// Scratch of [`VfxGpu::rank_emitters`]: `(view depth, slot)` per emitter slot.
    rank_scratch: Vec<(f32, usize)>,
    sort_algorithm: SortAlgorithm,
    /// Spherical billboards: camera-facing puffs fade through their own depth (see
    /// [`crate::quality::spherical_billboard_fade`]). Packed in `fire_color.w`.
    volumetric_billboards: bool,
    pipe_prepare: wgpu::ComputePipeline,
    /// The ordered list gather (ADR-5991).
    gather: ListGather,

    // CPU staging
    emitter_records: Vec<EmitterGpu>,
    emitter_dirty: Vec<bool>,
    emitter_slots: EmitterAllocator,
    spawn_cpu: Vec<SpawnJobGpu>,
    /// [`CurveLut::upload_key`] of the LUT on the GPU (`None` before the first upload).
    curves_key: Option<(u64, usize, usize)>,
    /// LUT uploads so far (stat, [`VfxGpu::curve_uploads`]).
    curve_uploads: u64,
    /// Emitter-record `write_buffer` calls so far (one per dirty run, not one per slot).
    emitter_writes: u64,
    /// [`VfxGpu::note_upload`] calls since the oldest queued spawn job was queued (0 after a
    /// [`VfxGpu::set_frame`]): uploads its jobs waited without a drawn frame.
    uploads_since_frame: u32,
    /// The host's game time did not advance this update ([`VfxGpu::set_time_held`]).
    time_held: bool,
    time: f32,
    /// Profile screen-size floor (pixels). An emitter floor above 0 replaces it.
    screen_floor_px: f32,
    /// Soft-particle layer resolution ([`VfxProfile::soft_particle_res`]).
    soft_res: SoftParticleRes,
    /// The host composites the soft layer this frame ([`VfxGpu::set_soft_pass`]).
    soft_pass: bool,
    /// The low-resolution soft layer and its composite pipelines (built on first use).
    soft: soft::SoftLayer,
    /// Sun, ambient and exposure for the next [`VfxGpu::set_frame`].
    lighting: VfxLighting,
    /// The strongest particle light's colour × intensity as given to
    /// [`VfxGpu::set_fire_light`]; [`VfxGpu::set_frame`] scales it by `fire_scale`.
    fire_raw: [f32; 3],
    /// Display brightness per unit of fire-light intensity at 1 m (see [`VfxGpu::set_sun`]).
    fire_scale: f32,
    /// The frame's sky light rows (`FrameGpu::sky_up`, `sky_down`), [`Self::set_sky_light`].
    sky_rows: [[f32; 4]; 2],
    /// Event spawns: the link and child-init tables, their pipelines and the step counter.
    event_gpu: events::EventPass,
    /// Mesh-asset particles (`.vfx.ron` v8 `render.mesh`): uploaded meshes, their indirect
    /// arguments and pipelines.
    render_meshes: mesh_draw::RenderMeshes,
    /// The particle bounds reduction, built by the first [`VfxGpu::read_particle_bounds`].
    bounds_reduce: Option<bounds::BoundsReduce>,
}

impl VfxGpu {
    /// Creates the VFX GPU context for `profile`.
    ///
    /// Render pipelines are built lazily per colour format on the first
    /// [`VfxGpu::draw_particles`]/[`VfxGpu::draw_decals`]/[`VfxGpu::draw_trails`]
    /// call with that format (see [`VfxGpu::formats`]).
    pub fn new(device: &RenderDevice, profile: VfxProfile) -> Result<Self, VfxError> {
        // `None` only on a warm worker whose queue is shutting down (never off the workers).
        Self::build(device, profile)?.ok_or_else(|| VfxError::Shader("canceled at shutdown".into()))
    }

    /// [`Self::new`]; `None` when it stopped early because its
    /// [`regia_render::PipelineWarmQueue`] is shutting down ([`regia_render::warm_cancelled`],
    /// checked between its compute pipeline batches).
    pub(crate) fn build(device: &RenderDevice, profile: VfxProfile) -> Result<Option<Self>, VfxError> {
        let d = &device.device;
        let q = &device.queue;
        if profile.particle_capacity == 0 || profile.particle_capacity > 1_000_000 {
            return Err(VfxError::Capacity(profile.particle_capacity));
        }
        let capacity = profile.particle_capacity;
        let event_capacity = profile.event_capacity.clamp(16, 65_536);
        let child_capacity = profile.child_capacity.clamp(16, 65_536);
        // Every fired event is staged at `slot_ordinal * 3 + n`, so the event buffers span all
        // `3 * (capacity + child_capacity)` ranks; the over-budget drop is counted in
        // `event_prepare`, by fired count, not by staging position.
        let staged_ranks = 3 * (u64::from(capacity) + u64::from(child_capacity));
        let event_mask_words = 2 * (u64::from(capacity) + u64::from(child_capacity)).div_ceil(32);
        // The draw lists hold every live particle, the child ring's included.
        let sort_n = sort_list_size(capacity + child_capacity);
        let decal_capacity = profile.decal_capacity.clamp(1, MAX_DECALS);

        let storage = |label: &str, size: u64| -> wgpu::Buffer {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        // The main ring, then the child ring behind it.
        let byte_len = (u64::from(capacity) + u64::from(child_capacity)) * 16;
        let sim = SimBuffers {
            emitters: storage(
                "vfx emitters",
                MAX_EMITTERS as u64 * size_of::<EmitterGpu>() as u64,
            ),
            pos_age: storage("vfx pos_age", byte_len),
            vel_life: storage("vfx vel_life", byte_len),
            attrib: storage("vfx attrib", byte_len),
            color0: storage("vfx color0", byte_len),
            spawn_jobs: storage(
                "vfx spawn jobs",
                MAX_SPAWN_JOBS as u64 * size_of::<SpawnJobGpu>() as u64,
            ),
            counters: d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vfx counters"),
                size: COUNTER_COUNT as u64 * 4,
                // STORAGE for the kernels, COPY_DST for clearing, COPY_SRC for the stats copy.
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            stats: storage("vfx stats", layout::STATS_COUNT as u64 * 4),
            // Additive, alpha and mesh lists, then the gather region (`shaders::GATHER`, ADR-5991).
            lists: storage("vfx lists", 4 * sort_n as u64 * 8),
            curves_scalars: storage("vfx curve scalars", CURVE_SCALAR_BYTES),
            curves_colors: storage("vfx curve colors", CURVE_COLOR_BYTES),
            events: storage("vfx events", staged_ranks * size_of::<layout::EventGpu>() as u64),
            event_epoch: storage("vfx event epoch", staged_ranks * 4),
            event_mask: storage("vfx event mask", event_mask_words * 4),
        };
        // Epoch 0 means never written. A zero-filled allocation would match `prev` on the
        // first advancing frame (step 1). Stamp every rank invalid before the first dispatch;
        // `event_prepare` also rejects epoch 0, so a later clear cannot reopen the hole.
        // The buffer holds one epoch per staged rank, not per budgeted event: `event_capacity`
        // counts fired events and can be larger than the staging span of a small pool (a write
        // of that many epochs ran past the buffer) or far smaller (most ranks stayed 0).
        let invalid_epoch = vec![0xFFFF_FFFFu32; staged_ranks as usize];
        q.write_buffer(&sim.event_epoch, 0, bytemuck::cast_slice(&invalid_epoch));
        let frame_ub = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx frame"),
            size: size_of::<FrameGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let frame_ub_half = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx frame half"),
            size: size_of::<FrameGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let indirect = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx indirect"),
            size: 3 * 16,
            // COPY_SRC: the mesh list's instance count feeds the mesh-asset draws (`mesh_draw`).
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::INDIRECT,
            mapped_at_creation: false,
        });
        let decals = storage("vfx decals", decal_capacity as u64 * size_of::<DecalGpu>() as u64);
        let decal_tiles = storage(
            "vfx decal tiles",
            decal_capacity as u64 * size_of::<DecalTileGpu>() as u64,
        );
        let trail_verts = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx trail verts"),
            size: 65_536 * size_of::<TrailVertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let stats_read = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx stats readback"),
            // The four list counters, then the ring and event stats.
            size: 16 + u64::from(layout::STATS_COUNT) * 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Sort uniform blocks (256-byte aligned for dynamic offsets). Block 0 pads the list
        // (`[0, counter index, region base, n]`); blocks 1.. are the Batcher merge steps.
        let mut steps: Vec<[u32; 4]> = vec![[0, COUNTER_ALPHA, sort_n, sort_n]];
        if profile.sort {
            let mut k = 2u32;
            while k <= sort_n {
                let mut j = k >> 1;
                while j >= 1 {
                    steps.push([k, j, sort_n, sort_n]);
                    j >>= 1;
                }
                k <<= 1;
            }
        }
        // Shared-memory stages: block "local full" ([0, 0, ..]) and one tail block per stage
        // k >= 1024 ([k, 0, ..]); their offsets follow the merge steps.
        let local_first = steps.len();
        if profile.sort {
            steps.push([0, 0, sort_n, sort_n]);
            let mut k = 1024u32;
            while k <= sort_n {
                steps.push([k, 0, sort_n, sort_n]);
                k <<= 1;
            }
        }
        let uniform_stride = 256u64;
        // The mesh list sorts back to front too (no depth writes: overlapping mesh particles
        // would draw in gather order). It reuses the same blocks with its own region base and
        // counter: a second copy, `mesh_sort_offset` bytes after the first.
        let alpha_blocks = steps.len();
        if profile.sort {
            let base = sort_n * 2;
            let mesh: Vec<[u32; 4]> = steps
                .iter()
                .enumerate()
                .map(|(i, step)| {
                    if i == 0 {
                        [0, COUNTER_MESH, base, sort_n]
                    } else {
                        [step[0], step[1], base, sort_n]
                    }
                })
                .collect();
            steps.extend(mesh);
        }
        let mesh_sort_offset = (alpha_blocks as u64 * uniform_stride) as u32;
        let sort_uniforms = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx sort uniforms"),
            size: uniform_stride * steps.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        for (i, step) in steps.iter().enumerate() {
            q.write_buffer(
                &sort_uniforms,
                i as u64 * uniform_stride,
                bytemuck::cast_slice(step),
            );
        }
        // (uniform offset, k, j) per global merge step; block 0 (the pad) is not a merge step.
        let sort_steps: Vec<(u32, u32, u32)> = steps
            .iter()
            .enumerate()
            .take(local_first)
            .skip(1)
            .map(|(i, step)| (i as u32 * uniform_stride as u32, step[0], step[1]))
            .collect();
        // (uniform offset, stage k; 0 = the whole tile sort) per shared-memory stage.
        let sort_local_steps: Vec<(u32, u32)> = steps
            .iter()
            .enumerate()
            .take(alpha_blocks)
            .skip(local_first)
            .map(|(i, step)| (i as u32 * uniform_stride as u32, step[0]))
            .collect();

        // Atlas + samplers. The 64 px atlas is one level (100k smoke/spark particles sample
        // it); the 256 px fireball sheet has mips and 4× anisotropic sampling.
        let atlas_tex = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx atlas"),
            size: wgpu::Extent3d {
                width: crate::atlas::ATLAS_SIZE,
                height: crate::atlas::ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        q.write_texture(
            atlas_tex.as_image_copy(),
            crate::atlas::atlas_rgba(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(crate::atlas::ATLAS_SIZE * 4),
                rows_per_image: Some(crate::atlas::ATLAS_HEIGHT),
            },
            wgpu::Extent3d {
                width: crate::atlas::ATLAS_SIZE,
                height: crate::atlas::ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        let atlas = atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vfx atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let fireball_tex = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx fireball sheet"),
            size: wgpu::Extent3d {
                width: crate::atlas::FIREBALL_SHEET_WIDTH,
                height: crate::atlas::FIREBALL_SHEET_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: crate::atlas::FIREBALL_MIP_LEVELS,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, data) in crate::atlas::fireball_sheet_mips().iter().enumerate() {
            let w = crate::atlas::FIREBALL_SHEET_WIDTH >> level;
            let h = crate::atlas::FIREBALL_SHEET_HEIGHT >> level;
            q.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &fireball_tex,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
        let fireball_sheet = fireball_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let fireball_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vfx fireball sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 4,
            ..Default::default()
        });
        let depth_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vfx depth sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let dummy_depth = Self::dummy_depth_texture(d, q);
        let (decal_atlas, decal_atlas_sampler) = Self::decal_atlas_resources(d, q);
        let dummy_receiver = Self::dummy_color_texture(
            d,
            q,
            "vfx dummy receiver",
            wgpu::TextureFormat::Rgba8Unorm,
            [0, 128, 255, 128],
        );
        let dummy_scene = Self::dummy_color_texture(
            d,
            q,
            "vfx dummy scene",
            wgpu::TextureFormat::Rgba8UnormSrgb,
            [255, 255, 255, 255],
        );

        // Bind group layouts.
        let bgl_frame = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx frame bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT | wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let storage_entry =
            |binding: u32, visibility: wgpu::ShaderStages, rw: bool| wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: !rw },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            };
        let cs = wgpu::ShaderStages::COMPUTE;
        let sim_entries = [
            storage_entry(0, cs, false),
            storage_entry(1, cs, true),
            storage_entry(2, cs, true),
            storage_entry(3, cs, true),
            storage_entry(4, cs, true),
            storage_entry(5, cs, false),
            storage_entry(6, cs, true),
            storage_entry(7, cs, true),
            storage_entry(8, cs, true),
            storage_entry(9, cs, false),
            storage_entry(10, cs, false),
            wgpu::BindGroupLayoutEntry {
                binding: 11,
                visibility: cs,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 12,
                visibility: cs,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
            storage_entry(13, cs, true),
            storage_entry(14, cs, true),
            storage_entry(15, cs, true),
        ];
        let bgl_sim = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx sim bgl"),
            entries: &sim_entries,
        });
        let bgl_sort_data = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx sort data bgl"),
            entries: &[storage_entry(0, cs, true), storage_entry(1, cs, true)],
        });
        let bgl_sort_params = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx sort params bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: cs,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bgl_prepare = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx prepare bgl"),
            entries: &[storage_entry(0, cs, true), storage_entry(1, cs, true)],
        });

        let vf = wgpu::ShaderStages::VERTEX_FRAGMENT;
        let particle_entries = [
            storage_entry(0, vf, false),
            storage_entry(1, vf, false),
            storage_entry(2, vf, false),
            storage_entry(3, vf, false),
            storage_entry(4, vf, false),
            storage_entry(5, vf, false),
            storage_entry(6, vf, false),
            storage_entry(7, vf, false),
            wgpu::BindGroupLayoutEntry {
                binding: 8,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 9,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 10,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 11,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 12,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 13,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 14,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ];
        let bgl_particles = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx particle bgl"),
            entries: &particle_entries,
        });
        let float_tex = |binding: u32, dim: wgpu::TextureViewDimension| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: dim,
                multisampled: false,
            },
            count: None,
        };
        let bgl_decal = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx decal bgl"),
            entries: &[
                storage_entry(0, vf, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
                float_tex(3, wgpu::TextureViewDimension::D2),
                float_tex(4, wgpu::TextureViewDimension::D2),
                float_tex(5, wgpu::TextureViewDimension::D2Array),
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                storage_entry(7, wgpu::ShaderStages::VERTEX, false),
            ],
        });

        // Bind groups.
        fn entry(binding: u32, buf: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: buf.as_entire_binding(),
            }
        }

        let bg_frame = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx frame bg"),
            layout: &bgl_frame,
            entries: &[entry(0, &frame_ub)],
        });
        let bg_frame_half = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx frame half bg"),
            layout: &bgl_frame,
            entries: &[entry(0, &frame_ub_half)],
        });
        let bg_sim = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx sim bg"),
            layout: &bgl_sim,
            entries: &[
                entry(0, &sim.emitters),
                entry(1, &sim.pos_age),
                entry(2, &sim.vel_life),
                entry(3, &sim.attrib),
                entry(4, &sim.color0),
                entry(5, &sim.spawn_jobs),
                entry(6, &sim.counters),
                entry(7, &sim.stats),
                entry(8, &sim.lists),
                entry(9, &sim.curves_scalars),
                entry(10, &sim.curves_colors),
                wgpu::BindGroupEntry {
                    binding: 11,
                    resource: wgpu::BindingResource::TextureView(&dummy_depth),
                },
                wgpu::BindGroupEntry {
                    binding: 12,
                    resource: wgpu::BindingResource::Sampler(&depth_sampler),
                },
                entry(13, &sim.events),
                entry(14, &sim.event_epoch),
                entry(15, &sim.event_mask),
            ],
        });
        let bg_sort_data = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx sort data bg"),
            layout: &bgl_sort_data,
            entries: &[entry(0, &sim.lists), entry(1, &sim.counters)],
        });
        let bg_sort_params = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx sort params bg"),
            layout: &bgl_sort_params,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                // One stride-sized window at offset 0; dynamic offsets slide it over the
                // uniform blocks (an entire-buffer binding would forbid any offset).
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &sort_uniforms,
                    offset: 0,
                    size: Some(std::num::NonZeroU64::new(uniform_stride).expect("stride > 0")),
                }),
            }],
        });
        let bg_prepare = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx prepare bg"),
            layout: &bgl_prepare,
            entries: &[entry(0, &sim.counters), entry(1, &indirect)],
        });

        // Pipelines (validation errors are captured with an error scope).
        let error_scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = |label: &str, source: String| -> wgpu::ShaderModule {
            d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            })
        };
        let sim_module = module("vfx sim", format!("{COMMON}{SIM_BINDINGS}{LUT_FNS}{SIM_ENTRY}"));
        let sort_module = module("vfx sort", shaders::SORT.to_string());
        let prepare_module = module("vfx prepare", shaders::PREPARE.to_string());
        let particle_module = module(
            "vfx particle render",
            format!("{COMMON}{PARTICLE_RENDER_BINDINGS}{LUT_FNS}{BILLBOARD}"),
        );
        let mesh_module = module(
            "vfx mesh particle render",
            format!("{COMMON}{PARTICLE_RENDER_BINDINGS}{LUT_FNS}{CUBE_VERT}{MESH_PARTICLE}"),
        );
        let decal_module = module("vfx decal", format!("{COMMON}{CUBE_VERT}{DECAL}"));
        let trail_module = module("vfx trail", format!("{COMMON}{TRAIL}"));

        let compute_layout = |label: &str, bgl: &[&wgpu::BindGroupLayout]| -> wgpu::PipelineLayout {
            let refs: Vec<Option<&wgpu::BindGroupLayout>> = bgl.iter().map(|b| Some(*b)).collect();
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &refs,
                immediate_size: 0,
            })
        };
        let mk_compute =
            |label: &str, m: &wgpu::ShaderModule, entry_point: &str, layout: &wgpu::PipelineLayout| {
                d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(label),
                    layout: Some(layout),
                    module: m,
                    entry_point: Some(entry_point),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
                })
            };
        // A quit stops a build on a warm worker between its compile batches (shipping builds
        // never unwind out of a cancelled job).
        if regia_render::warm_cancelled() {
            return Ok(None);
        }
        let sim_pipeline_layout = compute_layout("vfx sim layout", &[&bgl_frame, &bgl_sim]);
        let pipe_emit = mk_compute("vfx emit", &sim_module, "emit", &sim_pipeline_layout);
        let pipe_update = mk_compute("vfx update", &sim_module, "update", &sim_pipeline_layout);
        let pipe_shift = mk_compute("vfx shift pool", &sim_module, "shift_pool", &sim_pipeline_layout);
        let sort_pipeline_layout = compute_layout("vfx sort layout", &[&bgl_sort_data, &bgl_sort_params]);
        let pipe_pad = mk_compute("vfx pad", &sort_module, "pad_alpha", &sort_pipeline_layout);
        let pipe_sort = mk_compute("vfx sort step", &sort_module, "sort_step", &sort_pipeline_layout);
        let pipe_sort_local = mk_compute(
            "vfx sort local",
            &sort_module,
            "sort_local",
            &sort_pipeline_layout,
        );
        if regia_render::warm_cancelled() {
            return Ok(None);
        }
        // The alpha list is `lists[sort_n..2 · sort_n]`, its count `counters[COUNTER_ALPHA]`.
        // Equal keys order by the slot bits the key does not hold (above its 12 index bits),
        // the same `(key, slot)` order as the bitonic sort.
        let radix = profile.sort.then(|| {
            let slot_bits = u32::BITS - (sort_n - 1).leading_zeros();
            radix_sort::GpuRadixSort::with_tie_break(
                d,
                &radix_sort::RadixSortTarget {
                    pairs: &sim.lists,
                    base: sort_n,
                    max_keys: sort_n,
                    counters: &sim.counters,
                    count_index: COUNTER_ALPHA,
                    key_bits: 32,
                },
                radix_sort::RadixTieBreak {
                    shift: crate::quality::SORT_INDEX_BITS,
                    bits: slot_bits.saturating_sub(crate::quality::SORT_INDEX_BITS),
                },
            )
        });
        // Mobile: the alpha keys are the emitters' CPU back-to-front ranks (`events.z`, below
        // `MAX_EMITTERS` = 256): two 4-bit passes, stable, costing what is alive.
        let emitter_order = (!profile.sort).then(|| {
            radix_sort::GpuRadixSort::new(
                d,
                &radix_sort::RadixSortTarget {
                    pairs: &sim.lists,
                    base: sort_n,
                    max_keys: sort_n,
                    counters: &sim.counters,
                    count_index: COUNTER_ALPHA,
                    key_bits: EMITTER_RANK_BITS,
                },
            )
        });
        if regia_render::warm_cancelled() {
            return Ok(None);
        }
        let event_gpu = events::EventPass::new(
            d,
            &bgl_frame,
            &events::EventBuffers {
                events: &sim.events,
                pos_age: &sim.pos_age,
                vel_life: &sim.vel_life,
                attrib: &sim.attrib,
                color0: &sim.color0,
                stats: &sim.stats,
                event_epoch: &sim.event_epoch,
                event_mask: &sim.event_mask,
            },
            event_capacity,
            child_capacity,
        );
        if regia_render::warm_cancelled() {
            return Ok(None);
        }
        let prepare_layout = compute_layout("vfx prepare layout", &[&bgl_prepare]);
        let pipe_prepare = mk_compute(
            "vfx prepare",
            &prepare_module,
            "prepare_indirect",
            &prepare_layout,
        );
        // The ordered list gather (ADR-5991).
        let gather = {
            let blocks = storage(
                "vfx gather blocks",
                (u64::from(sort_n.div_ceil(GATHER_BLOCK)) + 1) * 16,
            );
            let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("vfx gather bgl"),
                entries: &[
                    storage_entry(0, cs, true),
                    storage_entry(1, cs, true),
                    storage_entry(2, cs, true),
                ],
            });
            let bind_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vfx gather bg"),
                layout: &bgl,
                entries: &[entry(0, &sim.counters), entry(1, &sim.lists), entry(2, &blocks)],
            });
            let gather_module = module("vfx gather", format!("{COMMON}{}", shaders::GATHER));
            let layout = compute_layout("vfx gather layout", &[&bgl_frame, &bgl]);
            ListGather {
                count: mk_compute("vfx gather count", &gather_module, "gather_count", &layout),
                scan: mk_compute("vfx gather scan", &gather_module, "gather_scan", &layout),
                write: mk_compute("vfx gather write", &gather_module, "gather_write", &layout),
                bind_group,
                _blocks: blocks,
            }
        };

        // Render pipeline layouts and modules (pipelines are per colour format, built lazily).
        let pl_particles = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx particle render layout"),
            bind_group_layouts: &[Some(&bgl_frame), Some(&bgl_particles)],
            immediate_size: 0,
        });
        let pl_mesh = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx mesh render layout"),
            bind_group_layouts: &[Some(&bgl_frame), Some(&bgl_particles)],
            immediate_size: 0,
        });
        let pl_decal = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx decal render layout"),
            bind_group_layouts: &[Some(&bgl_frame), Some(&bgl_decal)],
            immediate_size: 0,
        });
        // Trails and ribbons sample the sprite atlas (ribbon textures, `.vfx.ron` v7).
        let bgl_trail = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx trail bgl"),
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
        let bg_trail = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx trail bg"),
            layout: &bgl_trail,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });
        let pl_trail = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx trail render layout"),
            bind_group_layouts: &[Some(&bgl_frame), Some(&bgl_trail)],
            immediate_size: 0,
        });

        let validation = block_on(error_scope.pop());
        if let Some(err) = validation {
            return Err(VfxError::Shader(err.to_string()));
        }

        // Zero-initialise the particle pool so nothing renders garbage on frame 1.
        let zeros = vec![0u8; byte_len as usize];
        for buf in [&sim.pos_age, &sim.vel_life, &sim.attrib, &sim.color0] {
            q.write_buffer(buf, 0, &zeros);
        }
        q.write_buffer(
            &sim.stats,
            0,
            bytemuck::cast_slice(&[0u32; layout::STATS_COUNT as usize]),
        );

        let mut gpu = Self {
            device: TrackedDevice::new(d.clone()),
            queue: q.clone(),
            capacity,
            sort_enabled: profile.sort,
            sort_n,
            frame_cpu: FrameGpu {
                view_proj: [[0.0; 4]; 4],
                inv_view_proj: [[0.0; 4]; 4],
                view: [[0.0; 4]; 4],
                cam_right_dt: [1.0, 0.0, 0.0, 1.0 / 60.0],
                cam_up_time: [0.0, 1.0, 0.0, 0.0],
                cam_pos_near: [0.0, 0.0, 0.0, 0.1],
                viewport: [1.0, 1.0, 1.0, 1.0],
                params: [250.0, if profile.sort { 1.0 } else { 0.0 }, 1.0, 0.0],
                sim: [capacity as f32, sort_n as f32, profile.screen_floor_px, 1.0],
                light0: [0.0, 1.0, 0.0, 0.5],
                light1: [1.0, 1.0, 1.0, 1.0],
                decal: [0.0, 0.0, 0.0, 1.0],
                fire_pos: [0.0; 4],
                fire_color: [0.0; 4],
                window: [0.0, capacity as f32, 0.0, 0.0],
                sky_up: [0.0; 4],
                sky_down: [0.0; 4],
                shift: [0.0; 4],
                events: [0, 0, event_capacity, child_capacity],
            },
            pending_shift: Vec3::ZERO,
            frame_ub,
            frame_ub_half,
            sim,
            indirect,
            decals,
            decal_tiles,
            trail_verts,
            trail_count: 0,
            trail_cpu: Vec::new(),
            trails_dirty: false,
            decal_count: 0,
            clip_w: 0,
            clip_h: 0,
            decal_capacity,
            pending_decals: Vec::new(),
            pending_surfaces: Vec::new(),
            decal_atlas,
            decal_atlas_sampler,
            custom_decal_atlas: None,
            dummy_receiver,
            dummy_scene,
            haze_view: None,
            haze_allowed: true,
            receiver_view: None,
            scene_view: None,
            cull_live: 0,
            cull_culled: 0,
            cull_tiles: 0,
            sort_steps,
            sort_local_steps,
            pipe_sort_local,
            sort_live_n: sort_n,
            mesh_sort_offset,
            mesh_sort_n: sort_n,
            soft_live: true,
            spawn_cursor: 0,
            spawn_history: std::collections::VecDeque::new(),
            max_lifetime: None,
            last_spawn: None,
            spawned_life_end: None,
            layer_warm_frames: LAYER_WARM_FRAMES,
            held_warm_frames: None,
            scene_live: false,
            update_window: (0, capacity),
            stats_read,
            atlas,
            atlas_sampler,
            fireball_sheet,
            fireball_sampler,
            depth_sampler,
            dummy_depth,
            bound_depth: None,
            bgl_sim,
            bgl_particles,
            bgl_decal,
            bg_frame,
            bg_trail,
            bg_frame_half,
            bg_sim,
            bg_sort_data,
            bg_sort_params,
            bg_prepare,
            bg_particles: None,
            bg_mesh_assets: None,
            bg_decal: None,
            pl_particles,
            pl_mesh,
            pl_decal,
            pl_trail,
            particle_module,
            mesh_module,
            decal_module,
            trail_module,
            render_pipes: std::collections::HashMap::new(),
            event_gpu,
            pipe_emit,
            pipe_shift,
            pipe_update,
            pipe_pad,
            pipe_sort,
            radix,
            emitter_order,
            rank_scratch: Vec::with_capacity(MAX_EMITTERS as usize),
            sort_algorithm: SortAlgorithm::default(),
            volumetric_billboards: true,
            pipe_prepare,
            gather,
            emitter_records: vec![EmitterGpu::default_emitter(); MAX_EMITTERS as usize],
            emitter_dirty: vec![false; MAX_EMITTERS as usize],
            emitter_slots: EmitterAllocator::new(MAX_EMITTERS),
            spawn_cpu: Vec::new(),
            curves_key: None,
            curve_uploads: 0,
            emitter_writes: 0,
            uploads_since_frame: 0,
            time_held: false,
            time: 0.0,
            screen_floor_px: profile.screen_floor_px.max(0.0),
            soft_res: profile.soft_particle_res,
            soft_pass: false,
            soft: soft::SoftLayer::default(),
            lighting: VfxLighting::default(),
            fire_raw: [0.0; 3],
            fire_scale: 1.0,
            sky_rows: [[0.0; 4]; 2],
            render_meshes: mesh_draw::RenderMeshes::new(&TrackedDevice::new(d.clone()), sort_n),
            bounds_reduce: None,
        };
        gpu.set_depth_view(None);
        Ok(Some(gpu))
    }

    fn decal_flags(&self) -> u32 {
        let mut flags = 0u32;
        if self.receiver_view.is_some() {
            flags |= 1;
        }
        if self.scene_view.is_some() {
            flags |= 2;
        }
        flags
    }

    /// Frustum- and distance-culls the staged decals, keeps the highest priorities that fit
    /// the budget, and uploads that list. Lower priority is drawn first.
    fn upload_visible_decals(&mut self, view_proj: Mat4, eye: Vec3, width: u32, height: u32) {
        let _span = tracing::info_span!("decal_cull").entered();
        if self.pending_decals.is_empty() {
            self.decal_count = 0;
            self.cull_live = 0;
            self.cull_culled = 0;
            self.cull_tiles = 0;
            self.clip_w = 0;
            self.clip_h = 0;
            return;
        }
        let bin = crate::decal_cull::bin_decals(
            &self.pending_decals,
            &self.pending_surfaces,
            view_proj,
            eye,
            width,
            height,
            crate::decal_cull::DECAL_TILE_PX,
        );
        let cap = self.decal_capacity as usize;
        let start = bin.visible.len().saturating_sub(cap);
        let chosen = &bin.visible[start..];
        self.cull_live = u32::try_from(self.pending_decals.len()).unwrap_or(u32::MAX);
        self.cull_culled = u32::try_from(bin.culled + bin.visible.len() - chosen.len()).unwrap_or(u32::MAX);
        self.cull_tiles = u32::try_from(bin.tiles_touched()).unwrap_or(u32::MAX);
        // Pack survivors in priority order. The tile quad's `slot` indexes this buffer,
        // not the pending-list index (a dropped low-priority decal must not be drawn).
        let mut slot_of = vec![u32::MAX; self.pending_decals.len()];
        let mut bytes = Vec::with_capacity(chosen.len() * size_of::<DecalGpu>());
        for index in chosen {
            let i = usize::from(*index);
            let Some(decal) = self.pending_decals.get(i) else {
                continue;
            };
            let surface = self
                .pending_surfaces
                .get(i)
                .copied()
                .unwrap_or_else(|| DecalSurface::from_decal(decal));
            slot_of[i] = u32::try_from(bytes.len() / size_of::<DecalGpu>()).unwrap_or(u32::MAX);
            bytes.extend_from_slice(bytemuck::bytes_of(&pack_decal(decal, &surface)));
        }
        let quads = crate::decal_cull::quads_from_tiles(&bin, chosen, width, height);
        let mut tile_bytes = Vec::with_capacity(quads.len() * size_of::<DecalTileGpu>());
        let mut clip_w = 0u32;
        let mut clip_h = 0u32;
        for quad in &quads {
            let Some(&slot) = slot_of.get(usize::from(quad.index)) else {
                continue;
            };
            if slot == u32::MAX {
                continue;
            }
            clip_w = clip_w.max(quad.rect[2].saturating_sub(quad.rect[0]));
            clip_h = clip_h.max(quad.rect[3].saturating_sub(quad.rect[1]));
            tile_bytes.extend_from_slice(bytemuck::bytes_of(&DecalTileGpu {
                rect: [
                    quad.rect[0] as f32,
                    quad.rect[1] as f32,
                    quad.rect[2] as f32,
                    quad.rect[3] as f32,
                ],
                slot,
                _pad: [0; 3],
            }));
        }
        self.decal_count = u32::try_from(tile_bytes.len() / size_of::<DecalTileGpu>()).unwrap_or(0);
        self.clip_w = clip_w;
        self.clip_h = clip_h;
        if !bytes.is_empty() {
            self.queue.write_buffer(&self.decals, 0, &bytes);
        }
        if !tile_bytes.is_empty() {
            self.queue.write_buffer(&self.decal_tiles, 0, &tile_bytes);
        }
    }

    /// Installs a library's matching custom atlas. Reusing the same immutable snapshot costs
    /// no texture upload or bind-group rebuild. Call before writing that library's decals.
    pub fn set_decal_texture_atlas(
        &mut self,
        atlas: std::sync::Arc<crate::decal_texture::DecalTextureAtlas>,
    ) {
        if self
            .custom_decal_atlas
            .as_ref()
            .is_some_and(|old| std::sync::Arc::ptr_eq(old, &atlas))
        {
            return;
        }
        let _span = tracing::debug_span!("upload_decal_texture_atlas").entered();
        let (view, sampler) =
            Self::make_decal_atlas_resources(&self.device, &self.queue, atlas.rgba(), atlas.layers());
        self.decal_atlas = view;
        self.decal_atlas_sampler = sampler;
        self.custom_decal_atlas = Some(atlas);
        self.rebuild_decal_bind_group();
    }

    fn decal_atlas_resources(d: &wgpu::Device, q: &wgpu::Queue) -> (wgpu::TextureView, wgpu::Sampler) {
        Self::make_decal_atlas_resources(
            d,
            q,
            &crate::decal_atlas::generate_decal_atlas(),
            crate::decal_atlas::DECAL_ATLAS_LAYERS,
        )
    }

    fn make_decal_atlas_resources(
        d: &wgpu::Device,
        q: &wgpu::Queue,
        data: &[u8],
        layers: u32,
    ) -> (wgpu::TextureView, wgpu::Sampler) {
        let size = crate::decal_atlas::DECAL_ATLAS_SIZE;
        let tex = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx decal atlas"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        q.write_texture(
            tex.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size * 4),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: layers,
            },
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor {
            label: Some("vfx decal atlas view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vfx decal atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        (view, sampler)
    }

    fn dummy_color_texture(
        d: &wgpu::Device,
        q: &wgpu::Queue,
        label: &str,
        format: wgpu::TextureFormat,
        px: [u8; 4],
    ) -> wgpu::TextureView {
        let tex = d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut row = [0u8; 256];
        row[..4].copy_from_slice(&px);
        q.write_texture(
            tex.as_image_copy(),
            &row,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }

    fn dummy_depth_texture(d: &wgpu::Device, q: &wgpu::Queue) -> wgpu::TextureView {
        let tex = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx dummy depth"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        // Depth formats cannot be written with Queue::write_texture (wgpu validation);
        // initialise the far-plane texel with a clear pass instead.
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vfx dummy depth clear"),
        });
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vfx dummy depth clear"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        drop(pass);
        q.submit([encoder.finish()]);
        view
    }

    /// Pool capacity.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Whether camera-facing billboards fade through their own spherical depth (default on).
    pub fn volumetric_billboards(&self) -> bool {
        self.volumetric_billboards
    }

    /// Turns spherical billboards on or off (off: the flat quad's authored soft fade only,
    /// the pre-2026-10-08 look). Takes effect at the next [`Self::set_frame`].
    pub fn set_volumetric_billboards(&mut self, on: bool) {
        self.volumetric_billboards = on;
    }

    /// The alpha sort in use (bitonic by default; radix is opt-in).
    pub fn sort_algorithm(&self) -> SortAlgorithm {
        self.sort_algorithm
    }

    /// Chooses the alpha sort (only matters when the profile sorts).
    pub fn set_sort_algorithm(&mut self, algorithm: SortAlgorithm) {
        self.sort_algorithm = algorithm;
    }

    /// Whether the sort path is enabled.
    pub fn sort_enabled(&self) -> bool {
        self.sort_enabled
    }

    /// Bounds the bitonic alpha sort to `live_alpha` keys (the CPU scene's live alpha count,
    /// an upper bound: GPU particles only die early). The sort covers the next power of two
    /// above it with a 25 % + 512 margin, so the order is the same as sorting the whole pool.
    /// [`crate::scene::VfxScene::upload`] sets it every frame; without it the whole pool sorts.
    pub fn set_alpha_bound(&mut self, live_alpha: u32) {
        self.sort_live_n = sort_bound(live_alpha, self.sort_n);
    }

    /// Keys the bitonic sort covers this frame.
    pub fn sort_live_keys(&self) -> u32 {
        self.sort_live_n
    }

    /// Bounds the back-to-front sort of the mesh-particle list to `live_mesh` keys (the CPU
    /// scene's live mesh-particle count), padded like [`Self::set_alpha_bound`]. `0` skips
    /// the mesh sort. [`crate::scene::VfxScene::upload`] sets it every frame; without it the
    /// whole mesh region sorts.
    pub fn set_mesh_bound(&mut self, live_mesh: u32) {
        self.mesh_sort_n = if live_mesh == 0 {
            0
        } else {
            sort_bound(live_mesh, self.sort_n)
        };
    }

    /// Keys the mesh-list sort covers this frame (0: skipped).
    pub fn mesh_sort_keys(&self) -> u32 {
        self.mesh_sort_n
    }

    /// The longest particle lifetime the library can spawn, in seconds. With it the update
    /// kernel covers only the ring-buffer slots spawned within that time (every older slot is
    /// dead), not the whole pool. [`crate::scene::VfxScene::upload`] sets it every frame.
    pub fn set_max_lifetime(&mut self, seconds: f32) {
        self.max_lifetime = (seconds.is_finite() && seconds > 0.0).then_some(seconds);
    }

    /// Queues a floating-origin rebase (spec 78 § 12.2): the next frame moves every live particle by
    /// `-shift` before it spawns. Shifts queued before one frame add up. Non-finite input is ignored.
    pub fn shift_origin(&mut self, shift: Vec3) {
        if shift.is_finite() {
            self.pending_shift += shift;
        }
    }

    /// The shift the next frame applies (zero: none).
    pub fn pending_shift(&self) -> Vec3 {
        self.pending_shift
    }

    /// `(first slot, slot count)` the update kernel covers this frame.
    pub fn update_window(&self) -> (u32, u32) {
        self.update_window
    }

    /// Spawn jobs staged for the next [`Self::set_frame`] (stat).
    pub fn queued_spawn_jobs(&self) -> usize {
        self.spawn_cpu.len()
    }

    /// Lengths of the containers the context keeps from frame to frame (stat: a long session
    /// must keep every one bounded; `frame_creep.rs` samples them).
    pub fn container_sizes(&self) -> VfxGpuSizes {
        VfxGpuSizes {
            spawn_jobs: self.spawn_cpu.len(),
            spawn_jobs_capacity: self.spawn_cpu.capacity(),
            spawn_history: self.spawn_history.len(),
            trail_vertices: self.trail_cpu.len(),
            pending_decals: self.pending_decals.len(),
            render_pipes: self.render_pipes.len(),
            free_emitter_slots: self.emitter_slots.free.len(),
        }
    }

    /// Advances the ring-counter mirror by this frame's `spawned` jobs and returns the update
    /// window: the slots spawned within the longest lifetime plus a quarter second, or the
    /// whole pool without a lifetime, when the window fills the pool, or when the 32-bit ring
    /// counter wrapped inside it (its slot sequence then jumps).
    fn advance_spawn_window(&mut self, spawned: u32) -> (u32, u32) {
        self.spawn_cursor = self.spawn_cursor.wrapping_add(spawned);
        spawn_window(
            &mut self.spawn_history,
            SpawnClock {
                cursor: self.spawn_cursor,
                time: self.time,
                max_lifetime: self.max_lifetime,
                capacity: self.capacity,
            },
            spawned,
        )
    }

    /// Whether soft particles are alive (the CPU scene's count); without them the soft layer
    /// and its full-screen composite are skipped.
    pub fn set_soft_live(&mut self, live: bool) {
        self.soft_live = live;
    }

    /// The profile's soft-particle layer resolution.
    pub fn soft_particle_res(&self) -> SoftParticleRes {
        self.soft_res
    }

    /// Sets the soft-particle layer resolution (a host setting; takes effect next frame).
    pub fn set_soft_particle_res(&mut self, res: SoftParticleRes) {
        self.soft_res = res;
    }

    /// Claims `n` emitter-record slots (stable for an instance's lifetime).
    ///
    /// Slots are a contiguous run `start..start+n` (the GPU record table is indexed
    /// `base + emitter_index`); see the private `EmitterAllocator` for the allocation
    /// strategy and its tests.
    pub fn allocate_emitters(&mut self, n: u32) -> Option<u32> {
        self.emitter_slots.allocate(n)
    }

    /// Returns emitter slots starting at `start` (`n` = as allocated).
    ///
    /// A run outside the table, or one with a slot that is not currently allocated (a double
    /// free), is ignored with a warning: freeing it would alias two instances on one record.
    pub fn free_emitters(&mut self, start: u32, n: u32) {
        if !self.emitter_slots.free(start, n) {
            tracing::warn!(target: "regia_vfx", "free_emitters({start}, {n}): not an allocated run; ignored");
            return;
        }
        for i in 0..n {
            let index = (start + i) as usize;
            let cleared = EmitterGpu::default_emitter();
            if bytemuck::bytes_of(&self.emitter_records[index]) != bytemuck::bytes_of(&cleared) {
                self.emitter_records[index] = cleared;
                self.emitter_dirty[index] = true;
            }
        }
        self.event_gpu.free(start, n);
    }

    /// `true` when a live emitter is a heat-haze card: the host then needs a scene copy
    /// taken after the decals. Without one the copy is skipped (a full-screen copy a frame).
    pub fn wants_haze(&self) -> bool {
        self.emitter_records.iter().any(|record| record.origin[3] > 0.0)
    }

    /// `true` when decals are uploaded for this frame (the host's scene-colour copy and
    /// receiver are only needed then).
    pub fn has_decals(&self) -> bool {
        self.decal_count > 0
    }

    /// `true` when this context has nothing to simulate or draw: its warm frames are done
    /// ([`LAYER_WARM_FRAMES`]), the CPU scene has no live instance ([`Self::set_scene_live`]; one
    /// held back by its LOD counts, so the layer keeps following the camera), no spawn job,
    /// decal or trail is staged or uploaded, and every particle spawned so far has outlived the
    /// longest lifetime ([`Self::set_max_lifetime`]; a context that spawned without one never
    /// goes idle again). The runtime layer then leaves the host frame alone
    /// ([`regia_render::TranslucentLayer::is_active`], ADR-2585).
    ///
    /// The clock advances only in [`Self::set_frame`], so an idle context's clock stops; its
    /// next spawn starts a new window, nothing alive is lost.
    pub fn is_idle(&self) -> bool {
        self.layer_warm_frames == 0
            && !self.scene_live
            && !self.particles_may_live()
            && self.spawn_cpu.is_empty()
            && self.pending_decals.is_empty()
            && self.decal_count == 0
            && self.trail_count == 0
    }

    /// Frames from now on that count as live with nothing in them ([`LAYER_WARM_FRAMES`] for a
    /// new context): the host frame builds the layer's targets and passes on them, so the first
    /// effect later creates none. 0 makes an empty layer free from its first frame (no target,
    /// no pass) at the price of creating them with the first effect. While they are held
    /// ([`Self::hold_layer_warm_frames`]) the larger count waits for the release.
    pub fn set_layer_warm_frames(&mut self, frames: u32) {
        match &mut self.held_warm_frames {
            Some(held) => *held = (*held).max(frames),
            None => self.layer_warm_frames = frames,
        }
    }

    /// Takes the warm frames off the layer until [`Self::release_layer_warm_frames`]: a render-out
    /// job's layer is live only for what the job draws, because each warm frame simulated moves
    /// the particle clock (ADR-2741). Warm frames armed meanwhile (a window resized during the
    /// job) wait for the release too (ADR-2808).
    pub fn hold_layer_warm_frames(&mut self) {
        if self.held_warm_frames.is_none() {
            self.held_warm_frames = Some(std::mem::take(&mut self.layer_warm_frames));
        }
    }

    /// Hands back what [`Self::hold_layer_warm_frames`] held.
    pub fn release_layer_warm_frames(&mut self) {
        if let Some(held) = self.held_warm_frames.take() {
            self.layer_warm_frames = held;
        }
    }

    /// Frames left that count as live with nothing in them ([`Self::set_layer_warm_frames`]). Each
    /// one the host simulates moves the particle clock ([`Self::sim_clock`]).
    #[must_use]
    pub fn layer_warm_frames(&self) -> u32 {
        self.layer_warm_frames
    }

    /// Whether the CPU scene has a live instance ([`crate::VfxScene::upload`] sets it every
    /// frame): the context is not idle while one lives, spawned or not ([`Self::is_idle`]).
    pub fn set_scene_live(&mut self, live: bool) {
        self.scene_live = live;
    }

    /// Queues an emitter record write (uploaded in [`VfxGpu::set_frame`]).
    ///
    /// A record whose bytes match what the slot already holds is left clean, so a stationary
    /// emitter does not stage another buffer.
    pub fn write_emitter(&mut self, slot: u32, record: EmitterGpu) {
        let index = slot as usize;
        if index >= self.emitter_records.len() {
            return;
        }
        // `events[2]` is the per-view emitter rank this context owns (mobile ordering).
        let mut record = record;
        record.events[2] = self.emitter_records[index].events[2];
        if bytemuck::bytes_of(&self.emitter_records[index]) == bytemuck::bytes_of(&record) {
            return;
        }
        self.emitter_records[index] = record;
        self.emitter_dirty[index] = true;
    }

    /// Ranks the allocated emitter slots back to front along the view (`events[2]`: 0 = farthest) for
    /// a profile without the alpha sort, marking the records whose rank changed. The alpha list
    /// then draws emitter by emitter in this order (`specs/26` § 3: CPU-ordered emitters on
    /// mobile). Ties (the emitters of one instance share an origin) keep slot order, so an
    /// effect's emitters draw in authored order.
    fn rank_emitters(&mut self, eye: Vec3, forward: Vec3) {
        let _span = tracing::trace_span!("vfx_emitter_rank").entered();
        let depth = |r: &EmitterGpu| (Vec3::new(r.origin[0], r.origin[1], r.origin[2]) - eye).dot(forward);
        // Only allocated slots (`allocate_emitters`) take part: a free slot keeps its record
        // (and its upload) untouched. A reused scratch list, a stack mask and an in-place sort:
        // no allocation per frame.
        let mut live = [true; MAX_EMITTERS as usize];
        for &slot in &self.emitter_slots.free {
            if let Some(flag) = live.get_mut(slot as usize) {
                *flag = false;
            }
        }
        let mut order = std::mem::take(&mut self.rank_scratch);
        order.clear();
        order.extend(
            self.emitter_records
                .iter()
                .enumerate()
                .filter(|(slot, _)| live.get(*slot).copied().unwrap_or(false))
                .map(|(slot, r)| (depth(r), slot)),
        );
        // Far first; equal depths by slot (a total order, so the unstable sort is deterministic).
        order.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        for (rank, &(_, slot)) in order.iter().enumerate() {
            let rank = rank as u32;
            if self.emitter_records[slot].events[2] != rank {
                self.emitter_records[slot].events[2] = rank;
                self.emitter_dirty[slot] = true;
            }
        }
        self.rank_scratch = order;
    }

    /// The per-view back-to-front rank of emitter slot `slot` (0 = farthest) on a profile
    /// without the alpha sort; `None` for an out-of-range slot. Desktop profiles keep 0.
    pub fn emitter_rank(&self, slot: u32) -> Option<u32> {
        self.emitter_records.get(slot as usize).map(|r| r.events[2])
    }

    /// Appends spawn jobs for `emitter_slot`.
    ///
    /// The slot indexes the [`MAX_EMITTERS`]-entry emitter storage in the update and draw
    /// kernels (`emitters[u32(at.w)]`); a batch for a slot at or past the table would be an
    /// out-of-bounds WGSL storage read, so it is dropped with a warning, like an overflowing
    /// batch (deep-zai-vfx finding 3).
    pub fn push_spawn_jobs(&mut self, emitter_slot: u32, jobs: &[SpawnJob]) {
        if emitter_slot >= MAX_EMITTERS {
            tracing::warn!(
                target: "regia_vfx",
                "spawn jobs for emitter slot {emitter_slot} >= {MAX_EMITTERS}; dropping {} jobs",
                jobs.len()
            );
            return;
        }
        if self.spawn_cpu.len() + jobs.len() > MAX_SPAWN_JOBS as usize {
            tracing::warn!(
                target: "regia_vfx",
                "spawn job overflow: {} + {} > {MAX_SPAWN_JOBS}; dropping",
                self.spawn_cpu.len(),
                jobs.len()
            );
            return;
        }
        for j in jobs {
            self.spawn_cpu.push(SpawnJobGpu {
                pos_life: [j.position[0], j.position[1], j.position[2], j.lifetime],
                vel_size: [j.velocity[0], j.velocity[1], j.velocity[2], j.size],
                rot_spin: [j.rotation, j.spin, emitter_slot as f32, j.phase.clamp(0.0, 0.999)],
                color: j.color,
            });
        }
    }

    /// Uploads baked curve LUTs when they differ from the last upload
    /// ([`CurveLut::upload_key`]); the same LUT again costs nothing.
    /// [`crate::scene::VfxScene::upload`] calls it every frame: re-sending an unchanged LUT was
    /// two `queue.write_buffer` staging buffers a frame, which pile up without bound while
    /// nothing is submitted (a hidden window).
    pub fn write_curves(&mut self, lut: &CurveLut) {
        if lut.scalars().is_empty() || lut.colors().is_empty() {
            return;
        }
        let key = lut.upload_key();
        if self.curves_key == Some(key) {
            return;
        }
        if !curve_lut_fits(
            std::mem::size_of_val(lut.scalars()) as u64,
            std::mem::size_of_val(lut.colors()) as u64,
        ) {
            tracing::error!(target: "regia_vfx", "vfx curve library exceeds the GPU curve buffers; keeping the previous curves");
            return;
        }
        self.queue
            .write_buffer(&self.sim.curves_scalars, 0, bytemuck::cast_slice(lut.scalars()));
        self.queue
            .write_buffer(&self.sim.curves_colors, 0, bytemuck::cast_slice(lut.colors()));
        self.curves_key = Some(key);
        self.curve_uploads += 1;
    }

    /// Curve LUT uploads so far (stat: one per LUT change, not one per frame).
    pub fn curve_uploads(&self) -> u64 {
        self.curve_uploads
    }

    /// Emitter-record uploads so far: one `write_buffer` per contiguous dirty run.
    ///
    /// Repeating an unchanged record does not add to the count.
    pub fn emitter_writes(&self) -> u64 {
        self.emitter_writes
    }

    /// Marks the start of a frame's CPU upload ([`crate::scene::VfxScene::upload`] calls it).
    ///
    /// Spawn jobs wait in a CPU queue for the next [`Self::set_frame`]. When the oldest queued
    /// job has waited [`STALE_UPLOADS`] uploads with no drawn frame (a hidden window: the host
    /// keeps updating, the renderer skips the frame), the queued jobs are dropped: they belong to
    /// frames nobody saw, and kept they filled the queue to its cap (then every push warned) and
    /// burst out at once on the next drawn frame. The CPU scene's live estimate still counts them
    /// until their lifetime ends, which only throttles spawns briefly after the window shows
    /// again.
    ///
    /// The count starts when a job is queued, not at the last drawn frame (ADR-3307): a host that
    /// draws only now and then (a headless app between agent screenshots) used to drop a spawn
    /// one update after it was queued, because the updates before it, with nothing queued, had
    /// already used up the count.
    pub fn note_upload(&mut self) {
        // Nothing queued: nothing can go stale, so the next job starts a new count. This also
        // covers an idle context (ADR-2585), whose queue is empty.
        if self.spawn_cpu.is_empty() {
            self.uploads_since_frame = 0;
        }
        // A held update (paused, frozen after a frame run, time scale 0) shows the same moment
        // again: its jobs are still the ones the next drawn frame shows, however fast the host
        // loop spins meanwhile (ADR-3287).
        if self.time_held {
            return;
        }
        self.uploads_since_frame = self.uploads_since_frame.saturating_add(1);
        if self.uploads_since_frame > STALE_UPLOADS && !self.spawn_cpu.is_empty() {
            tracing::debug!(
                target: "regia_vfx",
                "spawn jobs waited {STALE_UPLOADS} updates with no frame drawn: dropping them"
            );
            self.spawn_cpu.clear();
        }
    }

    /// Whether the host's game time is held this update (paused, frozen after a frame run, time
    /// scale 0). The runtime sets it from `Time::delta` before each upload. A held update does
    /// not count toward [`STALE_UPLOADS`]: no new frame happened that nobody saw, so the queued
    /// spawn jobs wait for the next drawn frame. Before ADR-3287 a paused headless host that
    /// spun more than [`STALE_UPLOADS`] updates between a blast and its screenshot (a fast
    /// machine) dropped the blast's fire and smoke.
    pub fn set_time_held(&mut self, held: bool) {
        self.time_held = held;
    }

    /// Stages decals with M14 tint surfaces. [`VfxGpu::set_frame`] culls and uploads them.
    pub fn write_decals(&mut self, decals: &[Decal]) {
        let surfaces: Vec<DecalSurface> = decals.iter().map(DecalSurface::from_decal).collect();
        self.write_decals_with(decals, &surfaces);
    }

    /// Stages decals and their parallel material records. A missing surface falls back to the
    /// tint-only default. The list is kept until the next write; [`VfxGpu::set_frame`] uploads
    /// the priority-sorted, tile-culled subset that fits the profile budget.
    pub fn write_decals_with(&mut self, decals: &[Decal], surfaces: &[DecalSurface]) {
        let n = decals.len().min(MAX_DECALS as usize);
        self.pending_decals.clear();
        self.pending_surfaces.clear();
        self.pending_decals.extend(decals.iter().take(n).copied());
        for (i, decal) in self.pending_decals.iter().enumerate() {
            let surface = surfaces
                .get(i)
                .copied()
                .unwrap_or_else(|| DecalSurface::from_decal(decal));
            self.pending_surfaces.push(surface);
        }
        if self.pending_decals.is_empty() {
            self.decal_count = 0;
            self.cull_live = 0;
            self.cull_culled = 0;
            self.cull_tiles = 0;
            self.clip_w = 0;
            self.clip_h = 0;
        }
    }

    /// Stages trail vertices (ribbons/trails) for the next drawn frame: they reach the GPU in
    /// [`Self::set_frame`] (or [`Self::draw_trails`] for a host that draws without it), so
    /// updates that are never drawn queue no buffer writes.
    pub fn write_trails(&mut self, vertices: &[TrailVertex]) {
        let n = vertices.len().min(65_536);
        self.trail_count = n as u32;
        self.trail_cpu.clear();
        self.trail_cpu.extend_from_slice(&vertices[..n]);
        self.trails_dirty = n > 0;
    }

    /// Uploads trail vertices staged since the last upload.
    fn flush_trails(&mut self) {
        if self.trails_dirty {
            self.queue
                .write_buffer(&self.trail_verts, 0, bytemuck::cast_slice(&self.trail_cpu));
            self.trails_dirty = false;
        }
    }

    /// Sets the scene depth view for depth collision, soft particles and decal projection.
    /// `None` binds a far-plane dummy (no collision, no softening).
    ///
    /// The bind groups that sample depth are rebuilt only when the view changes (a view is
    /// keyed by its id: the same cached view every frame costs nothing).
    pub fn set_depth_view(&mut self, view: Option<&wgpu::TextureView>) {
        let depth = view.cloned().unwrap_or_else(|| self.dummy_depth.clone());
        if self.bound_depth.as_ref() == Some(&depth) {
            return;
        }
        self.bound_depth = Some(depth.clone());
        fn entry(binding: u32, buf: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: buf.as_entire_binding(),
            }
        }

        self.rebuild_particle_bind_group();
        // The sim bind group also samples depth (collision); rebuild it with the real view.
        self.bg_sim = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx sim bg"),
            layout: &self.bgl_sim,
            entries: &[
                entry(0, &self.sim.emitters),
                entry(1, &self.sim.pos_age),
                entry(2, &self.sim.vel_life),
                entry(3, &self.sim.attrib),
                entry(4, &self.sim.color0),
                entry(5, &self.sim.spawn_jobs),
                entry(6, &self.sim.counters),
                entry(7, &self.sim.stats),
                entry(8, &self.sim.lists),
                entry(9, &self.sim.curves_scalars),
                entry(10, &self.sim.curves_colors),
                wgpu::BindGroupEntry {
                    binding: 11,
                    resource: wgpu::BindingResource::TextureView(&depth),
                },
                wgpu::BindGroupEntry {
                    binding: 12,
                    resource: wgpu::BindingResource::Sampler(&self.depth_sampler),
                },
                entry(13, &self.sim.events),
                entry(14, &self.sim.event_epoch),
                entry(15, &self.sim.event_mask),
            ],
        });
        self.rebuild_decal_bind_group();
    }

    /// Particle render bind group: pools, curves, atlas, the bound depth and the scene copy
    /// (heat haze). Rebuilt when the depth view or the scene copy changes.
    fn rebuild_particle_bind_group(&mut self) {
        let Some(depth) = self.bound_depth.clone() else {
            return;
        };
        let scene = self.haze_view.clone().unwrap_or_else(|| self.dummy_scene.clone());
        fn entry(binding: u32, buf: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: buf.as_entire_binding(),
            }
        }
        let group = |label: &str, lists: &wgpu::Buffer| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.bgl_particles,
                entries: &[
                    entry(0, &self.sim.pos_age),
                    entry(1, &self.sim.vel_life),
                    entry(2, &self.sim.attrib),
                    entry(3, &self.sim.color0),
                    entry(4, &self.sim.emitters),
                    entry(5, lists),
                    entry(6, &self.sim.curves_scalars),
                    entry(7, &self.sim.curves_colors),
                    wgpu::BindGroupEntry {
                        binding: 8,
                        resource: wgpu::BindingResource::TextureView(&self.atlas),
                    },
                    wgpu::BindGroupEntry {
                        binding: 9,
                        resource: wgpu::BindingResource::Sampler(&self.atlas_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 10,
                        resource: wgpu::BindingResource::TextureView(&depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 11,
                        resource: wgpu::BindingResource::Sampler(&self.depth_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 12,
                        resource: wgpu::BindingResource::TextureView(&scene),
                    },
                    wgpu::BindGroupEntry {
                        binding: 13,
                        resource: wgpu::BindingResource::TextureView(&self.fireball_sheet),
                    },
                    wgpu::BindGroupEntry {
                        binding: 14,
                        resource: wgpu::BindingResource::Sampler(&self.fireball_sampler),
                    },
                ],
            })
        };
        let particles = group("vfx particle render bg", &self.sim.lists);
        // The mesh-asset draw reads the bucketed list in place of the particle lists.
        let mesh_assets = self
            .render_meshes
            .bucket_buffer()
            .map(|buckets| group("vfx mesh-asset render bg", buckets));
        self.bg_particles = Some(particles);
        self.bg_mesh_assets = mesh_assets;
    }

    /// Binds the receiver (layer + world normal) and the scene-colour copy the material path
    /// composites against. `None` keeps the M14 tint path. Call this before [`VfxGpu::set_depth_view`]
    /// or any time after — the decal bind group is rebuilt immediately when either view changed.
    pub fn set_material_views(
        &mut self,
        receiver: Option<&wgpu::TextureView>,
        scene: Option<&wgpu::TextureView>,
    ) {
        if self.receiver_view.as_ref() == receiver && self.scene_view.as_ref() == scene {
            return;
        }
        self.receiver_view = receiver.cloned();
        self.scene_view = scene.cloned();
        if self.bound_depth.is_some() {
            self.rebuild_decal_bind_group();
        }
    }

    /// Heat haze samples the decal scene copy ([`Self::set_material_views`]) when `enabled`.
    ///
    /// Turn it on only when the host re-copies that texture after the decals and before the
    /// particles (the runtime HDR pass does). Off by default.
    pub fn set_haze(&mut self, enabled: bool) {
        let view = if enabled && self.haze_allowed {
            self.scene_view.clone()
        } else {
            None
        };
        self.set_haze_view(view);
    }

    /// Binds `view` (the scene after decals, before particles) as the heat-haze source, or
    /// `None` to turn haze off. Hosts without decals (the offscreen harness) call this
    /// directly.
    pub fn set_haze_source(&mut self, view: Option<&wgpu::TextureView>) {
        self.set_haze_view(view.cloned());
    }

    /// Host look for heat haze: `enabled` gates [`Self::set_haze`], `scale` multiplies every
    /// card's pixel offset (`frame.decal.w`). Takes effect at the next frame.
    pub fn set_haze_look(&mut self, enabled: bool, scale: f32) {
        self.haze_allowed = enabled;
        self.frame_cpu.decal[3] = scale.max(0.0);
    }

    fn set_haze_view(&mut self, view: Option<wgpu::TextureView>) {
        if view == self.haze_view {
            return;
        }
        self.frame_cpu.decal[2] = if view.is_some() { 1.0 } else { 0.0 };
        self.haze_view = view;
        self.queue
            .write_buffer(&self.frame_ub, 0, bytemuck::bytes_of(&self.frame_cpu));
        if self.bound_depth.is_some() {
            self.rebuild_particle_bind_group();
        }
    }

    /// The strongest particle light this frame, for lit puffs (smoke over a fire): world
    /// position, linear colour × intensity, radius. `None` turns it off. Takes effect at the
    /// next [`Self::set_frame`]; on an HDR frame its brightness follows the exposure given to
    /// [`Self::set_sun`].
    pub fn set_fire_light(&mut self, light: Option<(Vec3, [f32; 3], f32)>) {
        match light {
            Some((position, color, radius)) if radius > 0.0 => {
                self.frame_cpu.fire_pos = [position.x, position.y, position.z, radius];
                self.fire_raw = color;
            }
            _ => {
                self.frame_cpu.fire_pos = [0.0; 4];
                self.fire_raw = [0.0; 3];
            }
        }
    }

    /// Last decal upload: `(pending, culled, tiles touched, drawn)`.
    pub fn decal_budget(&self) -> (u32, u32, u32, u32) {
        (
            self.cull_live,
            self.cull_culled,
            self.cull_tiles,
            self.decal_count,
        )
    }

    fn rebuild_decal_bind_group(&mut self) {
        let Some(depth) = self.bound_depth.clone() else {
            return;
        };
        let receiver = self
            .receiver_view
            .clone()
            .unwrap_or_else(|| self.dummy_receiver.clone());
        let scene = self
            .scene_view
            .clone()
            .unwrap_or_else(|| self.dummy_scene.clone());
        self.bg_decal = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx decal bg"),
            layout: &self.bgl_decal,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.decals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&depth),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.depth_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&receiver),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&scene),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&self.decal_atlas),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::Sampler(&self.decal_atlas_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: self.decal_tiles.as_entire_binding(),
                },
            ],
        }));
    }

    /// Pixel size of the largest tile-union quad from the last upload.
    ///
    /// A decal that only covers the left of the view reports a width below the viewport.
    /// `(0, 0)` when nothing was drawn.
    pub fn clip_extent(&self) -> (u32, u32) {
        (self.clip_w, self.clip_h)
    }

    /// Sun, ambient and exposure used by the next [`VfxGpu::set_frame`].
    pub fn set_lighting(&mut self, lighting: VfxLighting) {
        self.lighting = lighting;
    }

    /// The lighting the next frame uses (after [`Self::set_sun`] / [`Self::set_exposure`]).
    #[must_use]
    pub fn lighting(&self) -> VfxLighting {
        self.lighting
    }

    /// Sets the sun from a frame's directional light: `toward` the sun (world, any length),
    /// its linear `color` and `illuminance` in lux. Particles and the decal glint work in
    /// display-referred light, so the colour is scaled to how bright a white surface under
    /// that sun shows: with `exposure` (the post multiplier of an HDR frame) that is
    /// `illuminance / π × exposure`, the same mapping the meshes go through, so a dim
    /// planet's auto exposure lifts its smoke with its ground; without it (a display-pass
    /// frame) `illuminance / 100 klx`, clamped to `0.25..=1.5`. No sun (zero illuminance or
    /// direction) zeroes the colour and keeps the previous direction, so nothing glints at night.
    ///
    /// With `exposure` the fire light ([`Self::set_fire_light`]) goes through the same mapping:
    /// one unit of intensity is [`crate::PARTICLE_LIGHT_LUMENS_PER_UNIT`] lumens, `/4π` candela,
    /// so its display brightness at distance `d` is `lm / 4π / d² / π × exposure` (the meshes
    /// see the same light through the renderer). Without it the colour is used as given.
    pub fn set_sun(&mut self, toward: [f32; 3], color: [f32; 3], illuminance: f32, exposure: Option<f32>) {
        self.fire_scale = match exposure {
            Some(m) if m > 0.0 && m.is_finite() => (crate::PARTICLE_LIGHT_LUMENS_PER_UNIT
                / (4.0 * std::f32::consts::PI * std::f32::consts::PI)
                * m)
                .clamp(0.0, 64.0),
            _ => 1.0,
        };
        let len = (toward[0] * toward[0] + toward[1] * toward[1] + toward[2] * toward[2]).sqrt();
        if !(illuminance > 0.0 && len > 1e-6 && len.is_finite()) {
            self.lighting.sun_color = [0.0; 3];
            return;
        }
        let scale = match exposure {
            Some(m) if m > 0.0 && m.is_finite() => (illuminance / std::f32::consts::PI * m).clamp(0.0, 4.0),
            _ => (illuminance / 100_000.0).clamp(0.25, 1.5),
        };
        self.lighting.sun_direction = toward.map(|c| c / len);
        self.lighting.sun_color = color.map(|c| c.max(0.0) * scale);
    }

    /// Sets the sky light lit puffs take (ADR-1425): `sky` is `[sky from above, ground below]` as
    /// radiance (`E / π`, cd/m², the renderer's hemisphere ambient), mapped to display units by
    /// `exposure` (the post multiplier of an HDR frame) the way [`Self::set_sun`] maps the sun,
    /// clamped to `0..=4`. With a sky, lit puffs are shaded as volumes: the sun through the
    /// puff's own depth (a bright rind, a dark core) plus the sky above and the ground below.
    /// `None` (no atmosphere, or no exposure: the display pass and the offscreen harness) keeps
    /// the scalar ambient of [`VfxLighting::ambient`].
    pub fn set_sky_light(&mut self, sky: Option<[[f32; 3]; 2]>, exposure: Option<f32>) {
        self.sky_rows = match (sky, exposure) {
            (Some([up, down]), Some(m)) if m > 0.0 && m.is_finite() => {
                let map = |c: f32| {
                    if c.is_finite() {
                        (c.max(0.0) * m).min(4.0)
                    } else {
                        0.0
                    }
                };
                [
                    [map(up[0]), map(up[1]), map(up[2]), 1.0],
                    [map(down[0]), map(down[1]), map(down[2]), 0.0],
                ]
            }
            _ => [[0.0; 4]; 2],
        };
    }

    /// How the target stores color: 0 display bytes, 1 linear, 2 pre-exposure HDR.
    #[must_use]
    pub fn target_mode(&self) -> f32 {
        self.frame_cpu.decal[1]
    }

    /// The sky light rows the next frame uploads (`[sky_up, sky_down]`; all zero when off).
    #[must_use]
    pub fn sky_light_rows(&self) -> [[f32; 4]; 2] {
        self.sky_rows
    }

    /// Overrides only the exposure multiplier (the render hook sets this from the frame's EV).
    ///
    /// The floor is [`EXPOSURE_FLOOR`]. A `1e-4` floor disagrees with the post multiply at
    /// daylight EV 15 and sends a dim, ratio-crushed colour into AgX.
    pub fn set_exposure(&mut self, exposure: f32) {
        self.lighting.exposure = exposure.max(EXPOSURE_FLOOR);
    }

    /// The pools the simulation writes, in one fixed order.
    fn pool_buffers(&self) -> [&wgpu::Buffer; 4] {
        [
            &self.sim.pos_age,
            &self.sim.vel_life,
            &self.sim.attrib,
            &self.sim.color0,
        ]
    }

    /// Copies the particle pool and what says where the simulation stands (ADR-2633). The copy is
    /// submitted here, so it runs after every frame submitted so far.
    #[must_use]
    pub fn capture_pool(&self) -> VfxPoolCapture {
        let _span = tracing::info_span!("vfx_capture_pool").entered();
        let size = u64::from(self.capacity) * 16;
        let copy = |label: &str| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let pools = [
            copy("vfx held pos_age"),
            copy("vfx held vel_life"),
            copy("vfx held attrib"),
            copy("vfx held color0"),
        ];
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vfx capture pool"),
            });
        for (from, to) in self.pool_buffers().into_iter().zip(&pools) {
            encoder.copy_buffer_to_buffer(from, 0, to, 0, size);
        }
        self.queue.submit([encoder.finish()]);
        VfxPoolCapture {
            pools,
            time: self.time,
            spawn_cursor: self.spawn_cursor,
            spawn_history: self.spawn_history.clone(),
            last_spawn: self.last_spawn,
            spawned_life_end: self.spawned_life_end,
            update_window: self.update_window,
            emitter_records: self.emitter_records.clone(),
            emitter_slots: self.emitter_slots.clone(),
            spawn_cpu: self.spawn_cpu.clone(),
            uploads_since_frame: self.uploads_since_frame,
            pending_shift: self.pending_shift,
        }
    }

    /// Puts back what [`Self::capture_pool`] copied. The copy back is submitted here, so the next
    /// frame simulates from the captured pool. Every emitter record is uploaded again with that
    /// frame.
    pub fn restore_pool(&mut self, capture: VfxPoolCapture) {
        let _span = tracing::info_span!("vfx_restore_pool").entered();
        let size = u64::from(self.capacity) * 16;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vfx restore pool"),
            });
        for (from, to) in capture.pools.iter().zip(self.pool_buffers()) {
            encoder.copy_buffer_to_buffer(from, 0, to, 0, size);
        }
        self.queue.submit([encoder.finish()]);
        self.time = capture.time;
        self.spawn_cursor = capture.spawn_cursor;
        self.spawn_history = capture.spawn_history;
        self.last_spawn = capture.last_spawn;
        self.spawned_life_end = capture.spawned_life_end;
        self.update_window = capture.update_window;
        self.emitter_records = capture.emitter_records;
        self.emitter_dirty.fill(true);
        self.emitter_slots = capture.emitter_slots;
        self.spawn_cpu = capture.spawn_cpu;
        self.uploads_since_frame = capture.uploads_since_frame;
        self.pending_shift = capture.pending_shift;
    }

    /// A particle spawned so far may still be alive: it has not outlived the longest lifetime
    /// ([`Self::set_max_lifetime`]; without one, any spawn may live).
    fn particles_may_live(&self) -> bool {
        match (self.last_spawn, self.max_lifetime) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(at), Some(life)) => self.time - at <= life + LIVE_MARGIN_S,
        }
    }

    /// A particle spawned so far may still be alive, judged by the lifetimes it was spawned with.
    /// Sharper than [`Self::particles_may_live`], which keeps the layer live for the library's
    /// longest lifetime: a short job with a short effect leaves nothing alive at the next start,
    /// whatever else the library holds (ADR-2814).
    fn spawned_particles_may_live(&self) -> bool {
        match (self.spawned_life_end, self.max_lifetime) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(end), Some(_)) => self.time - end <= LIVE_MARGIN_S,
        }
    }

    /// Notes when the particles staged for this frame end; the library's longest lifetime bounds
    /// them, as it bounds the update window.
    fn note_spawned_lives(&mut self) {
        let longest = self
            .spawn_cpu
            .iter()
            .map(|job| job.pos_life[3])
            .fold(0.0_f32, f32::max);
        let life = self.max_lifetime.map_or(longest, |cap| longest.min(cap));
        let end = self.time + life;
        self.spawned_life_end = Some(self.spawned_life_end.map_or(end, |earlier| earlier.max(end)));
    }

    /// The particle simulation's clock in seconds: the sum of the steps it simulated since the
    /// context was made or last restarted ([`Self::restart_idle_clock`]). Heat haze reads it.
    #[must_use]
    pub fn sim_clock(&self) -> f32 {
        self.time
    }

    /// Where the next spawn lands in the particle ring: the count of particles spawned since the
    /// context was made or last restarted, wrapping.
    #[must_use]
    pub fn spawn_cursor(&self) -> u32 {
        self.spawn_cursor
    }

    /// Starts the particle clock and the spawn ring over, as in a new context, when no particle
    /// can be alive: no live instance in the CPU scene ([`Self::set_scene_live`]), no spawn job
    /// staged, and every particle spawned so far has outlived its own lifetime. Returns whether it
    /// did (ADR-2687, ADR-2814).
    ///
    /// The clock and the ring are history, not world state: the clock sets the heat-haze phase
    /// and the ring the slots new particles take (the last tie-break of the blend sort). Left as
    /// they were, effects spawned in a render-out job looked different from the same effects in
    /// an earlier job. The pool's dead particles stay; nothing simulates or draws them. Emitter
    /// slots, decals and trails are untouched.
    pub fn restart_idle_clock(&mut self) -> bool {
        if self.scene_live || self.spawned_particles_may_live() || !self.spawn_cpu.is_empty() {
            return false;
        }
        self.time = 0.0;
        self.spawn_cursor = 0;
        self.spawn_history.clear();
        self.last_spawn = None;
        self.spawned_life_end = None;
        self.update_window = (0, self.capacity);
        true
    }

    /// Empties the particle pool and starts the simulation over, as a new context would. For a
    /// context that was built during a stretch whose start it never saw (ADR-2633): what it did
    /// there goes back to nothing.
    pub fn reset_pool(&mut self) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vfx reset pool"),
            });
        for buffer in self.pool_buffers() {
            encoder.clear_buffer(buffer, 0, None);
        }
        self.queue.submit([encoder.finish()]);
        self.time = 0.0;
        self.spawn_cursor = 0;
        self.spawn_history.clear();
        // Judged against the restarted clock, an old spawn time would keep the empty pool "alive"
        // (never idle) until the clock caught up with it (ADR-2687).
        self.last_spawn = None;
        self.spawned_life_end = None;
        self.update_window = (0, self.capacity);
        self.emitter_records.fill(EmitterGpu::default_emitter());
        self.emitter_dirty.fill(true);
        self.emitter_slots = EmitterAllocator::new(MAX_EMITTERS);
        self.spawn_cpu.clear();
        self.uploads_since_frame = 0;
        self.pending_shift = Vec3::ZERO;
    }

    /// Fills the frame uniform for `camera` and `dt`; flushes CPU staging to the GPU.
    pub fn set_frame(&mut self, camera: &VfxCamera, dt: f32, width: u32, height: u32) {
        self.time = advance_clock(self.time, dt);
        self.uploads_since_frame = 0;
        self.flush_trails();
        let aspect = width.max(1) as f32 / height.max(1) as f32;
        let (view, _proj, view_proj) = camera.matrices(aspect);
        let inv_view_proj = view_proj.inverse();
        let forward = (camera.target - camera.eye).normalize_or_zero();
        let right = forward.cross(Vec3::Y).normalize_or(Vec3::X);
        let up = right.cross(forward).normalize_or(Vec3::Y);
        let spawn_count = self.spawn_cpu.len() as u32;
        if spawn_count > 0 {
            self.last_spawn = Some(self.time);
            self.note_spawned_lives();
        }
        self.layer_warm_frames = self.layer_warm_frames.saturating_sub(1);
        // Ring slot of this frame's first spawn job: the emit kernel places job `j` at
        // `(spawn_base + j) % capacity`, the same slots on every run (exact in an f32: the
        // capacity is at most 1 000 000).
        let spawn_base = self.spawn_cursor % self.capacity;
        self.update_window = self.advance_spawn_window(spawn_count);
        let tan_half = (camera.fov_y * 0.5).tan().max(1e-6);
        let px_per_m = (height.max(1) as f32 * 0.5) / tan_half;
        let light = self.lighting;
        self.frame_cpu = FrameGpu {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: inv_view_proj.to_cols_array_2d(),
            view: view.to_cols_array_2d(),
            cam_right_dt: [right.x, right.y, right.z, dt.max(0.0)],
            cam_up_time: [up.x, up.y, up.z, self.time],
            cam_pos_near: [camera.eye.x, camera.eye.y, camera.eye.z, camera.near],
            viewport: [
                width as f32,
                height as f32,
                1.0 / width.max(1) as f32,
                1.0 / height.max(1) as f32,
            ],
            params: [
                camera.far,
                if self.sort_enabled { 1.0 } else { 0.0 },
                1.0,
                spawn_count as f32,
            ],
            sim: [
                self.capacity as f32,
                self.sort_n as f32,
                self.screen_floor_px,
                light.exposure.max(EXPOSURE_FLOOR),
            ],
            light0: [
                light.sun_direction[0],
                light.sun_direction[1],
                light.sun_direction[2],
                light.ambient.clamp(0.0, 1.0),
            ],
            light1: [
                light.sun_color[0],
                light.sun_color[1],
                light.sun_color[2],
                px_per_m,
            ],
            // The colour-format and haze flags persist from `set_colour_format` / `set_haze`.
            decal: [
                self.decal_flags() as f32,
                self.frame_cpu.decal[1],
                self.frame_cpu.decal[2],
                self.frame_cpu.decal[3],
            ],
            fire_pos: self.frame_cpu.fire_pos,
            fire_color: [
                self.fire_raw[0] * self.fire_scale,
                self.fire_raw[1] * self.fire_scale,
                self.fire_raw[2] * self.fire_scale,
                // Bit 0: spherical billboards; bits 1..: the soft-pass mode (1 = this pass
                // skips the soft emitters, which the low-resolution layer draws).
                (u32::from(self.volumetric_billboards) | (u32::from(self.soft_pass_active()) << 1)) as f32,
            ],
            window: [
                self.update_window.0 as f32,
                self.update_window.1 as f32,
                spawn_base as f32,
                0.0,
            ],
            sky_up: self.sky_rows[0],
            sky_down: self.sky_rows[1],
            // Taken once: the shift kernel runs on this frame only.
            shift: std::mem::take(&mut self.pending_shift).extend(0.0).to_array(),
            events: self.event_gpu.frame_row(dt),
        };
        self.queue
            .write_buffer(&self.frame_ub, 0, bytemuck::bytes_of(&self.frame_cpu));
        // The low-resolution soft layer renders the soft emitters at 1/2 or 1/4 of the viewport,
        // so the screen UV and the pixels-per-metre scale with it (the projection is unchanged),
        // and its mode draws only the soft emitters.
        let mut half = self.frame_cpu;
        let div = self.soft_res.divisor().max(2);
        let (hw, hh) = soft::layer_size(width, height, div);
        half.viewport = [hw as f32, hh as f32, 1.0 / hw as f32, 1.0 / hh as f32];
        half.light1[3] = px_per_m / div as f32;
        half.fire_color[3] = (u32::from(self.volumetric_billboards) | (2u32 << 1)) as f32;
        self.queue
            .write_buffer(&self.frame_ub_half, 0, bytemuck::bytes_of(&half));
        self.upload_visible_decals(view_proj, camera.eye, width, height);

        // Spawn jobs.
        if !self.spawn_cpu.is_empty() {
            self.queue
                .write_buffer(&self.sim.spawn_jobs, 0, bytemuck::cast_slice(&self.spawn_cpu));
            self.spawn_cpu.clear();
        }
        self.event_gpu.flush(&self.queue);
        if !self.sort_enabled {
            self.rank_emitters(camera.eye, forward);
        }
        // Dirty emitter records. One `write_buffer` per contiguous run.
        if self.emitter_dirty.iter().any(|dirty| *dirty) {
            let _span = tracing::trace_span!("vfx_emitter_flush").entered();
            let mut slot = 0;
            let slots = self.emitter_dirty.len();
            while slot < slots {
                if !self.emitter_dirty[slot] {
                    slot += 1;
                    continue;
                }
                let start = slot;
                while slot < slots && self.emitter_dirty[slot] {
                    self.emitter_dirty[slot] = false;
                    slot += 1;
                }
                self.queue.write_buffer(
                    &self.sim.emitters,
                    (start * size_of::<EmitterGpu>()) as u64,
                    bytemuck::cast_slice(&self.emitter_records[start..slot]),
                );
                self.emitter_writes = self.emitter_writes.saturating_add(1);
            }
        }
    }

    /// Records how the colour target stores colour.
    ///
    /// Call after [`Self::set_frame`] and before the decal pass. An `Rgba8UnormSrgb`
    /// attachment decodes on load, so the shader composites in linear and writes linear.
    /// `Rgba8Unorm` stores display bytes (the offscreen golden path); the shader converts.
    /// A float target (`Rgba16Float`, the runtime's HDR pass) holds pre-exposure radiance:
    /// the shader multiplies the scene copy by the exposure ([`Self::set_exposure`]),
    /// composites in exposed linear light and divides the result back.
    pub fn set_colour_format(&mut self, format: wgpu::TextureFormat) {
        let mode = match format {
            wgpu::TextureFormat::Rgba8UnormSrgb | wgpu::TextureFormat::Bgra8UnormSrgb => 1.0,
            wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rgba32Float => 2.0,
            _ => 0.0,
        };
        self.frame_cpu.decal[1] = mode;
        self.queue
            .write_buffer(&self.frame_ub, 0, bytemuck::bytes_of(&self.frame_cpu));
    }

    /// Records the simulation: emit → update → alpha sort (radix, or pad + Batcher passes)
    /// → indirect prep.
    /// Call once per frame before rendering; `set_frame` and `set_depth_view` first.
    pub fn simulate(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.simulate_timed(encoder, None);
    }

    /// [`VfxGpu::simulate`] with optional GPU pass timestamps (`specs/26` § 6 perf budget).
    pub fn simulate_timed(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        timestamps: Option<wgpu::ComputePassTimestampWrites<'_>>,
    ) {
        // Clear per-frame counters (list lengths + alive).
        encoder.clear_buffer(&self.sim.counters, 0, Some(u64::from(COUNTER_COUNT * 4)));

        let spawn_groups = (self.frame_cpu.params[3] as u32).div_ceil(64);
        // The window, then the child ring while a live instance has events.
        let cap_groups =
            (self.update_window.1.min(self.capacity).max(1) + self.frame_cpu.events[0]).div_ceil(64);
        {
            let _span = tracing::trace_span!("vfx_simulate").entered();
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vfx simulate"),
                timestamp_writes: timestamps,
            });
            // A rebase frame moves the live pool first; this frame's spawns are already shifted.
            if self.frame_cpu.shift[..3].iter().any(|v| *v != 0.0) {
                pass.set_pipeline(&self.pipe_shift);
                pass.set_bind_group(0, Some(&self.bg_frame), &[]);
                pass.set_bind_group(1, Some(&self.bg_sim), &[]);
                pass.dispatch_workgroups(cap_groups, 1, 1);
            }
            // Last frame's events become this frame's children, before this frame's events.
            if self.event_gpu.active {
                self.event_gpu.record(&mut pass, &self.bg_frame);
            }
            if spawn_groups > 0 {
                pass.set_pipeline(&self.pipe_emit);
                pass.set_bind_group(0, Some(&self.bg_frame), &[]);
                pass.set_bind_group(1, Some(&self.bg_sim), &[]);
                pass.dispatch_workgroups(spawn_groups, 1, 1);
            }
            pass.set_pipeline(&self.pipe_update);
            pass.set_bind_group(0, Some(&self.bg_frame), &[]);
            pass.set_bind_group(1, Some(&self.bg_sim), &[]);
            pass.dispatch_workgroups(cap_groups, 1, 1);
            {
                // The lists in slot-ordinal order (the same slots as the update), before the sorts.
                let gather = &self.gather;
                let slots = self.update_window.1.min(self.capacity).max(1) + self.frame_cpu.events[0];
                let blocks = slots.div_ceil(GATHER_BLOCK);
                pass.set_bind_group(0, Some(&self.bg_frame), &[]);
                pass.set_bind_group(1, Some(&gather.bind_group), &[]);
                pass.set_pipeline(&gather.count);
                pass.dispatch_workgroups(blocks, 1, 1);
                pass.set_pipeline(&gather.scan);
                pass.dispatch_workgroups(1, 1, 1);
                pass.set_pipeline(&gather.write);
                pass.dispatch_workgroups(blocks, 1, 1);
            }
            if self.sort_enabled
                && self.sort_algorithm == SortAlgorithm::Radix
                && let Some(radix) = self.radix.as_ref()
            {
                radix.record(&mut pass);
            } else if self.sort_enabled {
                self.record_bitonic(&mut pass);
            } else if let Some(order) = self.emitter_order.as_ref() {
                order.record(&mut pass);
            }
            if self.sort_enabled && self.mesh_sort_n > 0 {
                self.record_bitonic_region(&mut pass, self.mesh_sort_offset, self.mesh_sort_n);
            }
            pass.set_pipeline(&self.pipe_prepare);
            pass.set_bind_group(0, Some(&self.bg_prepare), &[]);
            pass.dispatch_workgroups(1, 1, 1);
            // Mesh-asset particles: bucket the mesh list by render mesh (ADR-7261).
            if !self.render_meshes.is_empty() {
                let _span = tracing::trace_span!("vfx_mesh_buckets").entered();
                self.render_meshes
                    .record_buckets(&self.queue, &mut pass, self.mesh_sort_n);
            }
        }
    }

    /// The render-mesh slot of the registered mesh `name` (`.vfx.ron` v8 `render.mesh`), for
    /// an emitter record's `events[1]`: uploads `mesh` on first use (and again when the library
    /// replaced it). A refusal (slots full, too many triangles) draws nothing; the caller
    /// reports it as `bad_mesh`.
    pub fn render_mesh_slot(
        &mut self,
        name: &str,
        mesh: &std::sync::Arc<crate::mesh_shape::VfxMesh>,
    ) -> Result<u32, mesh_draw::RenderMeshRefusal> {
        let slot = self.render_meshes.slot(&self.device, &self.queue, name, mesh)?;
        let inputs = mesh_draw::BucketInputs {
            counters: &self.sim.counters,
            lists: &self.sim.lists,
            attrib: &self.sim.attrib,
            emitters: &self.sim.emitters,
        };
        if self
            .render_meshes
            .ensure_buffers(&self.device, &self.queue, &inputs)
        {
            self.rebuild_particle_bind_group();
        }
        Ok(slot)
    }

    /// Every registered render mesh's draw on the last simulated frame: its indices and the
    /// instances the draw covered (that mesh's live particles only, ADR-7261). Reads the indirect
    /// arguments back (blocks; tests and the agent's `vfx.describe`). Empty with no render mesh.
    pub fn read_render_mesh_draws(&self) -> Vec<mesh_draw::RenderMeshDraw> {
        self.render_meshes.read_draws(&self.device, &self.queue)
    }

    /// Render meshes uploaded so far (`.vfx.ron` v8).
    pub fn render_mesh_count(&self) -> u32 {
        self.render_meshes.len()
    }

    /// Indices one instance of render-mesh slot `slot` draws (`None`: no such slot). The
    /// built-in cube draws 36 vertices.
    pub fn render_mesh_index_count(&self, slot: u32) -> Option<u32> {
        self.render_meshes.index_count(slot)
    }

    /// Draws the mesh-asset particles (one indexed indirect draw per registered mesh, over that
    /// mesh's bucketed instances), after the cube draw; nothing when no mesh is registered.
    /// `pipe` is the format's mesh-asset pipeline; group 1 is set back to `particles` after.
    fn draw_mesh_assets(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        pipe: &wgpu::RenderPipeline,
        particles: &wgpu::BindGroup,
    ) {
        if self.render_meshes.is_empty() {
            return;
        }
        let Some(bg) = self.bg_mesh_assets.as_ref() else {
            return;
        };
        pass.set_pipeline(pipe);
        pass.set_bind_group(1, Some(bg), &[]);
        self.render_meshes.draw(pass);
        pass.set_bind_group(1, Some(particles), &[]);
    }

    /// Records the bitonic alpha sort into `pass`: the pad, then the merge network on the live
    /// prefix only (a power of two at or above the alpha count: the same order as sorting the
    /// whole padded list), with every compare distance below 512 in shared memory.
    fn record_bitonic(&self, pass: &mut wgpu::ComputePass<'_>) {
        self.record_bitonic_region(pass, 0, self.sort_live_n);
    }

    /// [`Self::record_bitonic`] over one list region: `region` is the byte offset of that
    /// region's copy of the sort uniform blocks (0: the alpha list, `mesh_sort_offset`: the
    /// mesh list), `live_n` the keys to cover.
    fn record_bitonic_region(&self, pass: &mut wgpu::ComputePass<'_>, region: u32, live_n: u32) {
        let live_n = live_n.clamp(64, self.sort_n);
        let sort_groups = live_n.div_ceil(64);
        pass.set_bind_group(0, Some(&self.bg_sort_data), &[]);
        pass.set_bind_group(1, Some(&self.bg_sort_params), &[region]);
        pass.set_pipeline(&self.pipe_pad);
        pass.dispatch_workgroups(sort_groups, 1, 1);
        if live_n >= 512 && !self.sort_local_steps.is_empty() {
            // Shared-memory stages for every j < 512, global dispatches for j >= 512:
            // the same network in a handful of dispatches.
            let tiles = live_n / 512;
            let local = |k: u32| {
                self.sort_local_steps
                    .iter()
                    .find(|s| s.1 == k)
                    .map(|s| s.0 + region)
            };
            if let Some(offset) = local(0) {
                pass.set_pipeline(&self.pipe_sort_local);
                pass.set_bind_group(1, Some(&self.bg_sort_params), &[offset]);
                pass.dispatch_workgroups(tiles, 1, 1);
            }
            let mut k = 1024u32;
            while k <= live_n {
                pass.set_pipeline(&self.pipe_sort);
                for &(offset, sk, sj) in &self.sort_steps {
                    if sk == k && sj >= 512 {
                        pass.set_bind_group(1, Some(&self.bg_sort_params), &[offset + region]);
                        pass.dispatch_workgroups(sort_groups, 1, 1);
                    }
                }
                if let Some(offset) = local(k) {
                    pass.set_pipeline(&self.pipe_sort_local);
                    pass.set_bind_group(1, Some(&self.bg_sort_params), &[offset]);
                    pass.dispatch_workgroups(tiles, 1, 1);
                }
                k <<= 1;
            }
        } else {
            pass.set_pipeline(&self.pipe_sort);
            for &(offset, k, _) in &self.sort_steps {
                if k > live_n {
                    break;
                }
                pass.set_bind_group(1, Some(&self.bg_sort_params), &[offset + region]);
                pass.dispatch_workgroups(sort_groups, 1, 1);
            }
        }
    }

    /// The colour formats render pipelines have been built for so far.
    pub fn formats(&self) -> impl Iterator<Item = wgpu::TextureFormat> + '_ {
        self.render_pipes.keys().copied()
    }

    /// The shared handles [`PipeSource::build`] compiles a format's pipelines from.
    fn pipe_source(&self) -> PipeSource {
        PipeSource {
            device: self.device.clone(),
            sort_n: self.sort_n,
            pl_particles: self.pl_particles.clone(),
            pl_mesh: self.pl_mesh.clone(),
            pl_decal: self.pl_decal.clone(),
            pl_trail: self.pl_trail.clone(),
            particle_module: self.particle_module.clone(),
            mesh_module: self.mesh_module.clone(),
            decal_module: self.decal_module.clone(),
            trail_module: self.trail_module.clone(),
        }
    }

    /// Builds (once) and returns the render pipelines of `format`.
    fn pipes_for(&mut self, format: wgpu::TextureFormat) -> Result<&RenderPipes, VfxError> {
        if !self.render_pipes.contains_key(&format)
            && let Some(pipes) = self.pipe_source().build(format, false)?
        {
            self.render_pipes.insert(format, pipes);
        }
        self.render_pipes
            .get(&format)
            .ok_or_else(|| VfxError::Shader(format!("vfx render pipelines for {format:?} missing")))
    }

    /// Draws particles (additive billboards, sorted alpha billboards, mesh particles) into the
    /// host's transparent pass. The depth view must have been set via [`VfxGpu::set_depth_view`];
    /// the pass's colour attachment must be `format`.
    pub fn draw_particles(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        format: wgpu::TextureFormat,
    ) -> Result<(), VfxError> {
        let bg = self
            .bg_particles
            .as_ref()
            .ok_or_else(|| VfxError::Shader("draw_particles before set_depth_view".into()))?
            .clone();
        let (haze, add, alpha, mesh, mesh_asset, indirect) = {
            let pipes = self.pipes_for(format)?;
            (
                pipes.billboard_haze.clone(),
                pipes.billboard_add.clone(),
                pipes.billboard_alpha.clone(),
                pipes.mesh.clone(),
                pipes.mesh_asset.clone(),
                self.indirect.clone(),
            )
        };
        pass.set_bind_group(0, Some(&self.bg_frame), &[]);
        pass.set_bind_group(1, Some(&bg), &[]);
        // Heat haze first (it bends the scene behind the effect), from the additive list.
        pass.set_pipeline(&haze);
        pass.draw_indirect(&indirect, 0);
        pass.set_pipeline(&add);
        pass.draw_indirect(&indirect, 0);
        pass.set_pipeline(&alpha);
        pass.draw_indirect(&indirect, 16);
        pass.set_pipeline(&mesh);
        pass.draw_indirect(&indirect, 32);
        self.draw_mesh_assets(pass, &mesh_asset, &bg);
        Ok(())
    }

    /// Draws every particle list except the sorted alpha list (heat haze, additive and mesh).
    ///
    /// The opt-in half-resolution soft-particle pass draws the alpha list separately
    /// ([`VfxGpu::draw_alpha_half_res`]) and bilaterally upsamples it, so the host calls this
    /// into the full-resolution colour target instead of [`VfxGpu::draw_particles`].
    pub fn draw_particles_non_alpha(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        format: wgpu::TextureFormat,
    ) -> Result<(), VfxError> {
        let bg = self
            .bg_particles
            .as_ref()
            .ok_or_else(|| VfxError::Shader("draw_particles before set_depth_view".into()))?
            .clone();
        let (haze, add, mesh, mesh_asset, indirect) = {
            let pipes = self.pipes_for(format)?;
            (
                pipes.billboard_haze.clone(),
                pipes.billboard_add.clone(),
                pipes.mesh.clone(),
                pipes.mesh_asset.clone(),
                self.indirect.clone(),
            )
        };
        pass.set_bind_group(0, Some(&self.bg_frame), &[]);
        pass.set_bind_group(1, Some(&bg), &[]);
        pass.set_pipeline(&haze);
        pass.draw_indirect(&indirect, 0);
        pass.set_pipeline(&add);
        pass.draw_indirect(&indirect, 0);
        pass.set_pipeline(&mesh);
        pass.draw_indirect(&indirect, 32);
        self.draw_mesh_assets(pass, &mesh_asset, &bg);
        Ok(())
    }

    /// Draws the sorted alpha list (the soft particles) at half resolution into the host's
    /// half-resolution transparent pass. The frame uniform bound here uses the half-resolution
    /// viewport, so soft-particle fading and the screen floor stay in half-resolution pixel
    /// space; the depth sampled is still the full-resolution scene depth (the UV is correct).
    pub fn draw_alpha_half_res(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        format: wgpu::TextureFormat,
    ) -> Result<(), VfxError> {
        let bg = self
            .bg_particles
            .as_ref()
            .ok_or_else(|| VfxError::Shader("draw_particles before set_depth_view".into()))?
            .clone();
        let alpha = self.pipes_for(format)?.billboard_alpha.clone();
        let indirect = self.indirect.clone();
        pass.set_bind_group(0, Some(&self.bg_frame_half), &[]);
        pass.set_bind_group(1, Some(&bg), &[]);
        pass.set_pipeline(&alpha);
        pass.draw_indirect(&indirect, 16);
        Ok(())
    }

    /// Draws projected decals into the host's transparent pass (colour attachment = `format`).
    pub fn draw_decals(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        format: wgpu::TextureFormat,
    ) -> Result<(), VfxError> {
        if self.decal_count == 0 {
            return Ok(());
        }
        let _span = tracing::trace_span!("decal_draw").entered();
        let bg = self
            .bg_decal
            .as_ref()
            .ok_or_else(|| VfxError::Shader("draw_decals before set_depth_view".into()))?
            .clone();
        let decal = self.pipes_for(format)?.decal.clone();
        pass.set_bind_group(0, Some(&self.bg_frame), &[]);
        pass.set_bind_group(1, Some(&bg), &[]);
        pass.set_pipeline(&decal);
        pass.draw(0..6, 0..self.decal_count);
        Ok(())
    }

    /// Draws uploaded trail ribbons (colour attachment = `format`).
    pub fn draw_trails(&mut self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        if self.trail_count == 0 {
            return;
        }
        self.flush_trails();
        let (trail, verts) = match self.pipes_for(format) {
            Ok(pipes) => (pipes.trail.clone(), self.trail_verts.clone()),
            Err(_) => return,
        };
        pass.set_bind_group(0, Some(&self.bg_frame), &[]);
        pass.set_bind_group(1, Some(&self.bg_trail), &[]);
        pass.set_pipeline(&trail);
        pass.set_vertex_buffer(0, verts.slice(..));
        pass.draw(0..self.trail_count, 0..1);
    }

    /// Copies counters + stats into the readback buffer (call after [`VfxGpu::simulate`]).
    pub fn record_stats_copy(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_buffer_to_buffer(&self.sim.counters, 0, &self.stats_read, 0, 16);
        encoder.copy_buffer_to_buffer(
            &self.sim.stats,
            0,
            &self.stats_read,
            16,
            u64::from(layout::STATS_COUNT) * 4,
        );
    }

    /// Reads the counter words `[additive, alpha, mesh, alive]` after a submitted frame (blocks; tests).
    pub fn read_stats(&self) -> Option<[u32; 4]> {
        self.read_pool_stats()
            .map(|s| [s.additive, s.alpha, s.mesh, s.alive])
    }

    /// Reads the list counters and the ring totals copied by [`Self::record_stats_copy`] after
    /// a submitted frame (blocks; tests and the budget checks).
    pub fn read_pool_stats(&self) -> Option<VfxPoolStats> {
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = self.stats_read.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let out = (|| {
            self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
            rx.recv().ok()?.ok()?;
            let data = slice.get_mapped_range().ok()?;
            let words: &[u32] = bytemuck::cast_slice(&data[..]);
            match *words {
                [
                    additive,
                    alpha,
                    mesh,
                    alive,
                    spawned,
                    dropped,
                    _append,
                    _consume,
                    events_fired,
                    events_overflow,
                    _child_cursor,
                    children_spawned,
                    children_dropped,
                    ..,
                ] => Some(VfxPoolStats {
                    additive,
                    alpha,
                    mesh,
                    alive,
                    spawned,
                    dropped,
                    events_fired,
                    events_overflow,
                    children_spawned,
                    children_dropped,
                }),
                _ => None,
            }
        })();
        // Unmap on every path: a map left pending would panic the next `map_async`.
        self.stats_read.unmap();
        out
    }
}

fn pack_decal(decal: &Decal, surface: &DecalSurface) -> DecalGpu {
    let right = decal.rotation * Vec3::X;
    let up = decal.rotation * Vec3::Y;
    let forward = decal.rotation * Vec3::Z;
    let mask = if surface.receiver_mask == 0 || surface.receiver_mask == u32::MAX {
        0.0
    } else {
        // Layer masks we ship fit in the f32 mantissa. `u32::MAX` is rewritten to 0 above.
        surface.receiver_mask as f32
    };
    DecalGpu {
        right: [right.x, right.y, right.z, decal.half_size.x],
        up: [up.x, up.y, up.z, decal.half_size.y],
        forward: [forward.x, forward.y, forward.z, decal.half_size.z],
        center_fade: [decal.position.x, decal.position.y, decal.position.z, decal.fade()],
        albedo_opacity: [
            surface.albedo[0],
            surface.albedo[1],
            surface.albedo[2],
            surface.opacity,
        ],
        normal_rough: [
            surface.normal_xy[0],
            surface.normal_xy[1],
            surface.roughness,
            surface.normal_strength,
        ],
        emissive_angle0: {
            let e = crate::decal::cooled_emissive(surface.emissive, surface.cooling_s, decal.age);
            [e[0], e[1], e[2], surface.angle_fade[0]]
        },
        fade_atlas: [
            surface.angle_fade[1],
            surface.distance_fade[0],
            surface.distance_fade[1],
            surface.atlas_layer as f32,
        ],
        mask_channels: [
            mask,
            surface.channels as f32,
            surface.edge_softness,
            surface.blend,
        ],
        sort_pad: [f32::from(surface.priority), 0.0, 0.0, 0.0],
    }
}

impl EmitterGpu {
    /// A neutral record for unclaimed slots (zero gravity, kill-below disabled).
    pub fn default_emitter() -> Self {
        Self {
            gravity_drag: [0.0; 4],
            curl: [0.0; 4],
            vortex: [0.0; 4],
            attractor: [0.0; 4],
            kill: [f32::MIN, 0.0, 0.0, 0.0],
            curve_ids: [0; 4],
            render: [0.0, 0.0, 1.0, 1.0],
            flip: [1.0, 1.0, 0.0, 0.0],
            stretch: [0.05, 0.02, 0.0, 0.0],
            origin: [0.0; 4],
            quality: [1.0, 0.0, 0.65, 0.0],
            wind: [0.0; 4],
            events: [0; 4],
            kill_volume: [0.0; 4],
            kill_axes: [[0.0; 4]; 3],
        }
    }
}

/// Free-list of GPU emitter-record slots. Allocations must yield a **contiguous** run
/// `start..start+n` because the GPU record table is indexed `base + emitter_index`.
///
/// Frees push runs back in arbitrary interleaving, so [`allocate`](Self::allocate) sorts the
/// list and scans for a contiguous run. (The previous "pop n, report `free[len-n]`" claimed
/// `start..start+n` while popping a *different* set — off by one for `n > 1` — leaking one
/// slot and double-allocating another. Two instances then shared one record slot and one
/// system's particles rendered with the other's blend mode and sprite: the window-lab
/// "streaked zenith", where the tracer's additive streaks were alpha-blended with the muzzle
/// smoke's record.)
#[derive(Clone, Debug)]
struct EmitterAllocator {
    free: Vec<u32>,
    /// Table size; slots `0..max` exist.
    max: u32,
}

impl EmitterAllocator {
    /// All slots `0..max` free.
    fn new(max: u32) -> Self {
        Self {
            free: (0..max).collect(),
            max,
        }
    }

    /// Claims `n` contiguous slots, returning the run's start. `None` when no contiguous
    /// run of that length remains.
    fn allocate(&mut self, n: u32) -> Option<u32> {
        if n == 0 || self.free.len() < n as usize {
            return None;
        }
        self.free.sort_unstable();
        let len = self.free.len();
        let n = n as usize;
        // Scan from the top so re-freed runs are reused before virgin slots.
        for i in (0..=len - n).rev() {
            if self.free[i + n - 1] - self.free[i] == (n as u32 - 1) {
                let start = self.free[i];
                self.free.drain(i..i + n);
                return Some(start);
            }
        }
        None
    }

    /// Returns the run `start..start+n` to the pool (must be as allocated). `false` (and
    /// no change) when the run leaves the table or any slot in it is already free.
    fn free(&mut self, start: u32, n: u32) -> bool {
        let Some(end) = start.checked_add(n) else {
            return false;
        };
        if end > self.max || (start..end).any(|slot| self.free.contains(&slot)) {
            return false;
        }
        self.free.extend(start..end);
        true
    }
}

#[cfg(test)]
mod sort_tests {
    use super::{VfxGpu, VfxProfile};

    /// The bitonic sort (global and shared-memory stages on the live prefix) leaves the alpha
    /// list sorted by key and a permutation of its input, at counts below one tile, one tile,
    /// several tiles and a non-power-of-two count.
    #[test]
    fn bitonic_sorts_the_live_prefix() {
        let device = match regia_render::RenderDevice::shared_headless() {
            Ok(d) => d,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let profile = VfxProfile {
            particle_capacity: 20_000,
            ..VfxProfile::desktop_high()
        };
        let mut gpu = VfxGpu::new(&device, profile).expect("gpu");
        let sort_n = gpu.sort_n;
        for count in [300u32, 512, 3000, 9001] {
            let mut s = 0x9e37_79b9u32 ^ count;
            let mut pairs: Vec<[u32; 2]> = (0..count)
                .map(|i| {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    [s >> 8, i]
                })
                .collect();
            gpu.queue.write_buffer(
                &gpu.sim.lists,
                u64::from(sort_n) * 8,
                bytemuck::cast_slice(&pairs),
            );
            gpu.queue
                .write_buffer(&gpu.sim.counters, 4, bytemuck::cast_slice(&[count]));
            gpu.set_alpha_bound(count);
            let d = &device.device;
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                gpu.record_bitonic(&mut pass);
            }
            let bytes = u64::from(count) * 8;
            let staging = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bitonic readback"),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            encoder.copy_buffer_to_buffer(&gpu.sim.lists, u64::from(sort_n) * 8, &staging, 0, bytes);
            device.queue.submit([encoder.finish()]);
            let (tx, rx) = std::sync::mpsc::channel();
            staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
            d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
            rx.recv().expect("map").expect("map ok");
            let out: Vec<[u32; 2]> =
                bytemuck::cast_slice(&staging.slice(..).get_mapped_range().expect("range")).to_vec();
            staging.unmap();
            assert!(out.windows(2).all(|w| w[0][0] <= w[1][0]), "{count}: not sorted");
            let mut got = out.clone();
            got.sort_unstable();
            pairs.sort_unstable();
            assert_eq!(got, pairs, "{count}: not a permutation of the input");
        }
    }
}

#[cfg(test)]
mod mesh_bucket_tests {
    use std::sync::Arc;

    use super::layout::EmitterGpu;
    use super::mesh_draw::{BUCKET_HEAD, RENDER_MESH_NONE};
    use super::{VfxGpu, VfxProfile};
    use crate::mesh_shape::VfxMesh;

    fn read(device: &regia_render::RenderDevice, buffer: &wgpu::Buffer, bytes: u64) -> Vec<u32> {
        let d = &device.device;
        let staging = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bucket readback"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, bytes);
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        rx.recv().expect("map").expect("map ok");
        let out = bytemuck::cast_slice(&staging.slice(..).get_mapped_range().expect("range")).to_vec();
        staging.unmap();
        out
    }

    /// The bucketing (ADR-7261) turns the mesh list into, per render-mesh slot, exactly that
    /// slot's entries in their list order (stable), across several 256-entry chunks, skips the
    /// cube (slot 0) and unresolved meshes (`RENDER_MESH_NONE`), and writes each slot's count into
    /// its draw's instance count. Synthetic lists: no simulation involved.
    #[test]
    fn bucketing_keeps_each_meshs_entries_in_list_order() {
        let device = match regia_render::RenderDevice::shared_headless() {
            Ok(d) => d,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let profile = VfxProfile {
            particle_capacity: 4096,
            ..VfxProfile::desktop_high()
        };
        let mut gpu = VfxGpu::new(&device, profile).expect("gpu");
        let tri = |s: f32| {
            Arc::new(
                VfxMesh::from_triangles(&[[0.0, 0.0, 0.0], [s, 0.0, 0.0], [0.0, s, 0.0]], &[0, 1, 2])
                    .expect("mesh"),
            )
        };
        assert_eq!(gpu.render_mesh_slot("a", &tri(1.0)), Ok(1));
        assert_eq!(gpu.render_mesh_slot("b", &tri(2.0)), Ok(2));
        let buckets = gpu.render_meshes.bucket_buffer().expect("bucket buffer").clone();
        let sort_n = gpu.sort_n;
        // Emitter e draws slot SLOTS[e].
        const SLOTS: [u32; 4] = [0, 1, 2, RENDER_MESH_NONE];
        let mut records = vec![<EmitterGpu as bytemuck::Zeroable>::zeroed(); SLOTS.len()];
        for (record, slot) in records.iter_mut().zip(SLOTS) {
            record.events[1] = slot;
        }
        gpu.queue
            .write_buffer(&gpu.sim.emitters, 0, bytemuck::cast_slice(&records));
        for count in [5u32, 256, 700, 1500] {
            let mut s = 0x2545_f491u32 ^ count;
            let mut next = || {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s
            };
            // Particle i belongs to emitter `emitter[i]`; the list holds them in a shuffled order.
            let emitter: Vec<u32> = (0..count).map(|_| next() % SLOTS.len() as u32).collect();
            let attrib: Vec<[f32; 4]> = emitter.iter().map(|&e| [0.0, 0.0, 0.0, e as f32]).collect();
            let list: Vec<[u32; 2]> = (0..count).map(|i| [next(), (i * 7919) % count]).collect();
            gpu.queue
                .write_buffer(&gpu.sim.attrib, 0, bytemuck::cast_slice(&attrib));
            gpu.queue.write_buffer(
                &gpu.sim.lists,
                u64::from(sort_n) * 16,
                bytemuck::cast_slice(&list),
            );
            gpu.queue
                .write_buffer(&gpu.sim.counters, 8, bytemuck::cast_slice(&[count]));
            gpu.set_mesh_bound(count);
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
                gpu.render_meshes
                    .record_buckets(&gpu.queue, &mut pass, gpu.mesh_sort_n);
            }
            device.queue.submit([encoder.finish()]);
            let words = read(&device, &buckets, (u64::from(BUCKET_HEAD) + u64::from(count)) * 8);
            let head = |slot: usize| (words[slot * 2], words[slot * 2 + 1]);
            let draws = gpu.read_render_mesh_draws();
            let mut start = 0;
            for slot in 1..=2u32 {
                let want: Vec<[u32; 2]> = list
                    .iter()
                    .copied()
                    .filter(|e| SLOTS[emitter[e[1] as usize] as usize] == slot)
                    .collect();
                let (got_start, got_count) = head(slot as usize);
                assert_eq!(
                    (got_start, got_count),
                    (start, want.len() as u32),
                    "{count}: slot {slot} head"
                );
                let base = (BUCKET_HEAD as usize + start as usize) * 2;
                let got: Vec<[u32; 2]> = words[base..base + want.len() * 2].as_chunks::<2>().0.to_vec();
                assert_eq!(got, want, "{count}: slot {slot} entries, in list order");
                assert_eq!(
                    draws[slot as usize - 1].instances,
                    want.len() as u32,
                    "{count}: slot {slot} draw"
                );
                start += want.len() as u32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn event_prepare_targets_dx12_without_dynamic_vector_stores() {
        let (name, source) = super::events::prepare_source();
        let problems = regia_render::shader_hlsl::wgsl_dx12_problems(&[(&name, &source)]);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn advance_clock_ignores_non_finite_dt() {
        assert_eq!(super::advance_clock(1.0, 0.5), 1.5);
        assert_eq!(super::advance_clock(1.0, f32::NAN), 1.0);
        assert_eq!(super::advance_clock(1.0, f32::INFINITY), 1.0);
    }

    use super::{
        CURVE_COLOR_BYTES, CURVE_SCALAR_BYTES, EmitterAllocator, MIN_SORT_N, SoftParticleRes, VfxProfile,
        curve_lut_fits, record_spawn, sort_list_size,
    };

    #[test]
    fn curve_lut_size_guard() {
        assert!(curve_lut_fits(CURVE_SCALAR_BYTES, CURVE_COLOR_BYTES));
        assert!(!curve_lut_fits(CURVE_SCALAR_BYTES + 4, 0));
        assert!(!curve_lut_fits(0, CURVE_COLOR_BYTES + 16));
    }

    /// Paused frames (`dt = 0`) do not grow the spawn history.
    #[test]
    fn spawn_history_is_bounded_while_paused() {
        let mut h = std::collections::VecDeque::new();
        record_spawn(&mut h, 1.0, 3);
        for _ in 0..1000 {
            record_spawn(&mut h, 1.0, 0);
            record_spawn(&mut h, 1.0, 2);
        }
        assert_eq!(h.len(), 1);
        assert_eq!(h[0], (1.0, 2003));
        record_spawn(&mut h, 1.5, 4);
        assert_eq!(h.len(), 2);
    }

    /// ADR-5991: the ordered gather module and the sim module that feeds it validate and translate
    /// to MSL and to DX12 HLSL (no device: the naga front end, validator and back ends), and the
    /// gather is listed for `cargo xtask check-shaders`.
    #[test]
    fn gather_and_sim_modules_validate_for_metal_and_dx12() {
        let sources = super::shader_sources();
        let picked: Vec<(&str, &str)> = sources
            .iter()
            .filter(|(name, _)| name == "vfx gather" || name == "vfx sim")
            .map(|(name, source)| (name.as_str(), source.as_str()))
            .collect();
        assert_eq!(picked.len(), 2, "vfx gather and vfx sim are listed");
        let problems = regia_render::shader_msl_check::wgsl_msl_problems(&picked);
        assert!(problems.is_empty(), "{problems:?}");
        let problems = regia_render::shader_hlsl::wgsl_dx12_problems(&picked);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// A tiny pool still gets a sort list of at least one workgroup, so the sort-bound clamps
    /// (`clamp(64, sort_n)`) never see an inverted range.
    #[test]
    fn sort_list_size_is_at_least_one_workgroup() {
        for capacity in [1u32, 2, 17, 32, 33, 64, 65, 1000, 1_000_000] {
            let n = sort_list_size(capacity);
            assert!(
                n >= MIN_SORT_N && n >= capacity && n.is_power_of_two(),
                "capacity {capacity}"
            );
            // The exact expression the bound uses must not panic.
            let _ = 1u32.clamp(64, n);
        }
        assert_eq!(sort_list_size(1000), 1024);
    }

    /// The soft-particle layer per tier: Desktop High half, Ultra full, Mobile quarter.
    #[test]
    fn soft_particle_res_per_tier() {
        assert_eq!(
            VfxProfile::desktop_high().soft_particle_res,
            SoftParticleRes::Half
        );
        assert_eq!(
            VfxProfile::desktop_ultra().soft_particle_res,
            SoftParticleRes::Full
        );
        assert_eq!(
            VfxProfile::mobile_high().soft_particle_res,
            SoftParticleRes::Quarter
        );
        assert_eq!(SoftParticleRes::Quarter.divisor(), 4);
        assert_eq!(SoftParticleRes::from_name("half"), Some(SoftParticleRes::Half));
    }

    /// The window-lab defect: a 2-emitter system followed by a 1-emitter system must get
    /// disjoint contiguous slots. The old pop-n allocator handed both the muzzle's second
    /// slot and the tracer's only slot the same record.
    #[test]
    fn multi_emitter_allocations_never_share_slots() {
        let mut a = EmitterAllocator::new(256);
        let muzzle = a.allocate(2).expect("muzzle run");
        let tracer = a.allocate(1).expect("tracer slot");
        let muzzle_run = muzzle..muzzle + 2;
        assert!(!muzzle_run.contains(&tracer), "tracer shares the muzzle's run");
        assert_eq!(a.free.len(), 253, "exactly the claimed slots leave the pool");
    }

    /// Every allocation is a contiguous ascending run (the GPU indexes `base + index`),
    /// and `n = 0` claims nothing.
    #[test]
    fn allocations_are_contiguous_runs() {
        let mut a = EmitterAllocator::new(8);
        assert_eq!(a.allocate(0), None, "n = 0 claims nothing");
        let mut claimed: Vec<std::ops::Range<u32>> = Vec::new();
        while let Some(s) = a.allocate(3) {
            assert!(s + 3 <= 8, "run {s}..{} escapes the table", s + 3);
            claimed.push(s..s + 3);
        }
        assert_eq!(claimed.len(), 2, "two full runs fit, remainder stays free");
        assert_eq!(a.free.len(), 8 % 3);
        for (i, x) in claimed.iter().enumerate() {
            for (j, y) in claimed.iter().enumerate() {
                if i < j {
                    assert!(x.start >= y.end || y.start >= x.end, "runs {x:?} {y:?} overlap");
                }
            }
        }
    }

    /// A double free or an out-of-table run is rejected and leaves the pool untouched
    /// (a duplicated free index would hand one record to two instances).
    #[test]
    fn bad_frees_are_rejected() {
        let mut a = EmitterAllocator::new(8);
        let run = a.allocate(2).expect("run");
        assert!(!a.free(6, 4), "run past the table end");
        assert!(!a.free(u32::MAX, 2), "overflowing run");
        assert!(!a.free(run + 2, 1), "slot that was never allocated");
        assert_eq!(a.free.len(), 6, "rejected frees change nothing");
        assert!(a.free(run, 2));
        assert!(!a.free(run, 2), "double free");
        assert_eq!(a.free.len(), 8);
    }

    /// Freed runs are reusable and never double-allocate after interleaved churn.
    #[test]
    fn freed_runs_reuse_without_collision() {
        let mut a = EmitterAllocator::new(12);
        let first = a.allocate(4).expect("first");
        let second = a.allocate(4).expect("second");
        a.free(first, 4);
        let third = a.allocate(4).expect("reuses the freed run");
        assert_eq!(third, first, "the freed contiguous run is preferred");
        let mut all: Vec<std::ops::Range<u32>> = vec![second..second + 4, third..third + 4];
        while let Some(s) = a.allocate(1) {
            all.push(s..s + 1);
        }
        for (i, x) in all.iter().enumerate() {
            for (j, y) in all.iter().enumerate() {
                if i < j {
                    assert!(
                        x.start >= y.end || y.start >= x.end,
                        "claims {x:?} and {y:?} overlap"
                    );
                }
            }
        }
        assert_eq!(
            all.iter().map(|r| r.len()).sum::<usize>(),
            12,
            "all slots used exactly once"
        );
    }
    #[test]
    fn texture_extent_check() {
        use super::{VfxError, check_texture_extent};
        assert!(check_texture_extent(16384, 1, 16384).is_ok());
        assert!(matches!(
            check_texture_extent(16385, 1, 16384),
            Err(VfxError::TextureSize { width: 16385, .. })
        ));
        assert!(check_texture_extent(0, 4, 16384).is_err());
    }
}
