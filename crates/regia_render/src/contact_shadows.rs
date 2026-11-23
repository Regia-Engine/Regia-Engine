// SPDX-License-Identifier: MPL-2.0
//! Screen-space contact shadows: a short ray march toward the sun through the depth buffer,
//! multiplied into the sun's visibility only (`RenderProfile::contact_shadows`).
//!
//! Regia Engine's own design, from the public description of line-coherent screen-space shadows
//! (Bend Studio's *Days Gone* screen-space shadows, SIGGRAPH 2023 Advances talk and blog post)
//! and the general screen-space shadow literature; see `docs/provenance.md`. No third-party code.
//!
//! A ray from a surface toward the sun projects onto a screen line through the sun's screen
//! point (the vanishing point of the sun direction, [`LightPoint`]). [`plan`] splits the
//! screen into four sectors around that point and each sector into 64-pixel chunks of
//! major-axis distance, and lists the lines of every chunk. A workgroup of 64 threads takes 64
//! consecutive pixels of one line, loads the depths along it once into workgroup memory (plus
//! [`STEPS`] pixels further toward the light and one behind) and every thread marches through
//! that strip, so a pixel costs about two depth loads instead of one per step. The lines of a
//! chunk are one pixel apart at its far end and converge toward the light, so they cover every
//! pixel; the line closest to the sector's centre line among those landing on a pixel writes
//! it, so every pixel is written exactly once and the output is deterministic.
//!
//! A sample shadows the receiver only when it is in front of the ray (by more than the bias
//! and the strip's interpolation error, by less than the assumed thickness) *and* in front of
//! the receiver's own surface extrapolated along the line (1/z of a plane is affine along a
//! screen line; the slope comes from the strip pixel behind the receiver). A front-facing lit
//! plane is always behind its rays, but a lit surface facing away from the camera (an
//! inside-out mesh, smoothed normals past a silhouette) has its nearer part in front of every
//! ray leaving it: without the second test it darkens itself. The ray starts from the pixel's
//! own depth; the line's sub-pixel sample can be a neighbouring row (another surface at a
//! silhouette), and how far it is from the pixel's depth joins the receiver's uncertainty.
//!
//! Output: `Rgba8Unorm` storage (R = sun visibility, 1 = lit). R8 storage is not a core
//! WebGPU/Android format, so the pass uses the format SSAO already writes. Off binds the 1×1
//! white texture, so an off frame is unchanged.

use crate::frame_work::counted::*;
use std::collections::HashMap;

use crate::RenderError;
use crate::shader_lib::compose_source;

/// Pixels per line chunk and threads per workgroup.
pub const GROUP: i32 = 64;
/// Most pixels a ray marches toward the light (`contact_shadows.wgsl` `STEPS`), reached at
/// [`REFERENCE_HEIGHT`] and above. Below it the march shortens with the target height (see
/// [`steps_for_height`]), so the shadows cover the same fraction of the screen at every
/// resolution.
pub const STEPS: u32 = 48;
/// Target height at which the march is [`STEPS`] pixels long.
pub const REFERENCE_HEIGHT: u32 = 1440;
/// Fewest pixels marched on small targets (below 120 lines). A longer floor reaches across a
/// much larger share of a small screen than of a large one: small targets then shadow more
/// of each surface than the same view at full size does.
pub const MIN_STEPS: u32 = 4;
/// The ray counts a depth sample as an occluder when it is in front of the ray by more than
/// this fraction of the ray's view depth (depth-buffer and interpolation noise).
pub const BIAS: f32 = 0.0015;
/// Assumed occluder thickness as a fraction of view depth: a surface further in front of the
/// ray than this is a separate object the ray passes behind, not an occluder.
pub const THICKNESS: f32 = 0.04;
/// Minimum assumed occluder thickness, metres.
pub const THICKNESS_MIN_M: f32 = 0.12;
/// Maximum assumed occluder thickness, metres (ADR-1449). [`THICKNESS`] alone grows with
/// distance (10 m at 250 m), so far away every surface the ray passes behind within that depth
/// counted as solid: a tree's twig cards, 3-8 m in front of the ground seen through the crown,
/// hung dark curtains of false shadow under every crown of a landscape. Contact shadows are for
/// contact-scale occluders; thicker ones are the shadow maps' (a 1 m object spans several texels
/// of the farthest cascade that still holds it). Only view depths beyond
/// `THICKNESS_MAX_M / THICKNESS` (25 m) are affected.
pub const THICKNESS_MAX_M: f32 = 1.0;
/// Shadow strength (1 = an occluded ray blocks the sun completely).
pub const STRENGTH: f32 = 1.0;
/// View depth where the effect starts fading out, metres.
pub const FADE_START_M: f32 = 300.0;
/// View depth beyond which the effect is gone, metres.
pub const FADE_END_M: f32 = 600.0;
/// Two depth neighbours further apart than this fraction are an edge: the strip takes the
/// nearer one instead of interpolating across the silhouette.
pub const EDGE: f32 = 0.25;
/// Occluders within this fraction of the march shadow fully; later hits fade out toward its
/// end (a soft tail instead of a hard cut where the march ends).
pub const HARD_FRACTION: f32 = 0.2;
/// Fraction of the depth gap between the two rows an interpolated strip sample comes from
/// that the occlusion test tolerates (curved ground seen at grazing angles is not the plane
/// the interpolation assumes). Across an edge the whole gap is tolerated.
pub const INTERP_ERROR: f32 = 0.25;

/// Pixels marched on a target `height` pixels tall: [`STEPS`] × height / [`REFERENCE_HEIGHT`],
/// clamped to [`MIN_STEPS`]..=[`STEPS`].
#[must_use]
pub fn steps_for_height(height: u32) -> u32 {
    let scaled = (u64::from(STEPS) * u64::from(height) + u64::from(REFERENCE_HEIGHT) / 2)
        / u64::from(REFERENCE_HEIGHT);
    (scaled as u32).clamp(MIN_STEPS, STEPS)
}

/// The light point is clamped to this many screen spans (width + height) from the
/// viewport centre: rays stay within a fraction of a degree of the true direction and the
/// line arithmetic stays in range when the sun is almost parallel to the view plane.
const MAX_SPANS: f64 = 64.0;

/// The sun direction's vanishing point in depth-texture pixels, split into integer and
/// fractional parts the shader uses as is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightPoint {
    /// Integer part (x, y) of the point, in pixels (y down).
    pub int: [i32; 2],
    /// Fractional part (x, y) in `[0, 1)`.
    pub frac: [f32; 2],
    /// `true` when the sun is in front of the camera: rays march toward the point. `false`:
    /// the sun is behind the camera and rays march away from the (anti-)point.
    pub toward: bool,
}

impl LightPoint {
    /// The point for `clip` = `view_proj · (to_sun, 0)` (the sun direction in clip space),
    /// with `viewport` = `[x, y, width, height]` the pixel rectangle NDC maps to.
    #[must_use]
    pub fn from_clip(clip: [f32; 4], viewport: [f32; 4]) -> Self {
        let [cx, cy, _, cw] = clip.map(f64::from);
        let [vx, vy, vw, vh] = viewport.map(f64::from);
        let (vw, vh) = (vw.max(1.0), vh.max(1.0));
        let centre = (vx + vw * 0.5, vy + vh * 0.5);
        let reach = MAX_SPANS * (vw + vh);
        // Screen direction of the rays' vanishing point (pixel space, y down).
        let sign = if cw >= 0.0 { 1.0 } else { -1.0 };
        let dir = (cx * vw * 0.5 * sign, -cy * vh * 0.5 * sign);
        let mut point = if cw.abs() > 1e-12 {
            (vx + (cx / cw * 0.5 + 0.5) * vw, vy + (0.5 - cy / cw * 0.5) * vh)
        } else {
            (f64::INFINITY, f64::INFINITY)
        };
        let off = (point.0 - centre.0, point.1 - centre.1);
        let dist = off.0.hypot(off.1);
        if dist.is_nan() || dist > reach {
            let len = dir.0.hypot(dir.1);
            point = if len > 0.0 {
                (centre.0 + dir.0 / len * reach, centre.1 + dir.1 / len * reach)
            } else {
                centre
            };
        }
        let split = |v: f64| -> (i32, f32) {
            let i = v.floor();
            let mut int = i as i32;
            let mut frac = (v - i) as f32;
            if frac >= 1.0 {
                int += 1;
                frac = 0.0;
            }
            (int, frac.max(0.0))
        };
        let (xi, xf) = split(point.0);
        let (yi, yf) = split(point.1);
        Self {
            int: [xi, yi],
            frac: [xf, yf],
            toward: cw >= 0.0,
        }
    }
}

/// One sector of the screen around the light point (`contact_shadows.wgsl` `Sector`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sector {
    /// Major axis: 0 = x (columns are the march axis), 1 = y.
    pub axis: i32,
    /// +1 when the major index grows away from the light point along +axis, −1 along −axis.
    pub sign: i32,
    /// Major pixel index of j = 0 (the first pixel on this side of the light point).
    pub c0: i32,
    /// Major-axis distance of pixel j = 0's centre from the light point (`[0, 1]`).
    pub delta: f32,
    /// Integer part of the light point's minor coordinate.
    pub minor_int: i32,
    /// Fractional part of the light point's minor coordinate.
    pub minor_frac: f32,
}

/// A 64-pixel span of major-axis distance in one sector and the first of its lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Chunk {
    /// Sector index (0 = x+, 1 = x−, 2 = y+, 3 = y−).
    pub sector: u32,
    /// Chunk index: pixels j = 64k .. 64k + 63 from the light point.
    pub k: i32,
    /// First line (minor offset at the chunk's far end).
    pub e_lo: i32,
    /// First workgroup of this chunk in the flattened dispatch.
    pub start: u32,
}

/// The dispatch of one frame: sectors, chunk list and total workgroups.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Light point the sectors are built around.
    pub light: LightPoint,
    /// x+, x−, y+, y−.
    pub sectors: [Sector; 4],
    /// Chunks in dispatch order; `start` is the running workgroup count.
    pub chunks: Vec<Chunk>,
    /// Workgroups (lines × chunks) in the dispatch.
    pub workgroups: u32,
}

/// The sectors and line lists covering a `width × height` depth texture for `light`.
#[must_use]
pub fn plan(light: LightPoint, width: u32, height: u32) -> Plan {
    let mut sectors = [Sector {
        axis: 0,
        sign: 1,
        c0: 0,
        delta: 0.0,
        minor_int: 0,
        minor_frac: 0.0,
    }; 4];
    for axis in 0..2usize {
        let (li, lf) = (light.int[axis], light.frac[axis]);
        let (mi, mf) = (light.int[1 - axis], light.frac[1 - axis]);
        // Pixel p is on the + side when its centre is past the point: p + 0.5 > L.
        let first = li + i32::from(lf >= 0.5);
        sectors[axis * 2] = Sector {
            axis: axis as i32,
            sign: 1,
            c0: first,
            delta: (first - li) as f32 + 0.5 - lf,
            minor_int: mi,
            minor_frac: mf,
        };
        sectors[axis * 2 + 1] = Sector {
            axis: axis as i32,
            sign: -1,
            c0: first - 1,
            delta: lf - 0.5 - (first - 1 - li) as f32,
            minor_int: mi,
            minor_frac: mf,
        };
    }
    let mut chunks = Vec::new();
    let mut workgroups = 0u32;
    for (index, sector) in sectors.iter().enumerate() {
        let (major_size, minor_size) = if sector.axis == 0 {
            (width as i32, height as i32)
        } else {
            (height as i32, width as i32)
        };
        // On-screen major pixels p = c0 + sign·j.
        let (j_min, j_max) = if sector.sign > 0 {
            ((-sector.c0).max(0), major_size - 1 - sector.c0)
        } else {
            ((sector.c0 - (major_size - 1)).max(0), sector.c0)
        };
        if j_max < j_min || major_size <= 0 || minor_size <= 0 {
            continue;
        }
        let minor = f64::from(sector.minor_int) + f64::from(sector.minor_frac);
        let delta = f64::from(sector.delta);
        for k in j_min / GROUP..=j_max / GROUP {
            let span = f64::from(GROUP * k + 65) + delta;
            let m_first = (f64::from(GROUP * k) + delta).max(0.5);
            let m_last = f64::from(GROUP * k + 63) + delta;
            // Line e lands on row floor(L + e·m/M): the lines whose canonical pixel can be
            // on rows 0..minor_size, padded for rounding, and inside the sector's wedge.
            let lo = [m_first, m_last]
                .map(|m| -minor * span / m)
                .into_iter()
                .fold(f64::INFINITY, f64::min);
            let hi = [m_first, m_last]
                .map(|m| (f64::from(minor_size) - minor) * span / m)
                .into_iter()
                .fold(f64::NEG_INFINITY, f64::max);
            let wedge = span.floor() + 3.0;
            let e_lo = (lo.floor() - 2.0).max(-wedge);
            let e_hi = (hi.ceil() + 2.0).min(wedge);
            if e_hi < e_lo {
                continue;
            }
            let count = (e_hi - e_lo) as u32 + 1;
            chunks.push(Chunk {
                sector: index as u32,
                k,
                e_lo: e_lo as i32,
                start: workgroups,
            });
            workgroups += count;
        }
    }
    Plan {
        light,
        sectors,
        chunks,
        workgroups,
    }
}

/// Row (minor index) line `e` of chunk `k` covers at slot `lp` (`line_row` in the shader).
fn line_row(sector: &Sector, k: i32, e: i32, lp: i32) -> i32 {
    let span = (GROUP * k + 65) as f32 + sector.delta;
    let t = sector.minor_frac - e as f32 * (65 - lp) as f32 / span;
    sector.minor_int + e + t.floor() as i32
}

/// The sector of pixel `p` (`classify` in the shader).
fn classify(light: &LightPoint, p: [i32; 2]) -> u32 {
    let dx = (p[0] - light.int[0]) as f32 + (0.5 - light.frac[0]);
    let dy = (p[1] - light.int[1]) as f32 + (0.5 - light.frac[1]);
    if dy.abs() <= dx.abs() {
        if dx > 0.0 { 0 } else { 1 }
    } else if dy > 0.0 {
        2
    } else {
        3
    }
}

impl Plan {
    /// The pixel workgroup `wid`'s thread `local` writes, if any: the shader's ownership test
    /// on the CPU (on screen, in its own sector, the canonical line of that pixel).
    #[must_use]
    pub fn owner_pixel(&self, wid: u32, local: i32, width: u32, height: u32) -> Option<[u32; 2]> {
        let index = self.chunks.partition_point(|c| c.start <= wid).checked_sub(1)?;
        let chunk = self.chunks[index];
        if wid >= self.workgroups {
            return None;
        }
        let sector = &self.sectors[chunk.sector as usize];
        let e = chunk.e_lo + (wid - chunk.start) as i32;
        let j = GROUP * chunk.k + local;
        let major = sector.c0 + sector.sign * j;
        let row = line_row(sector, chunk.k, e, local);
        let p = if sector.axis == 0 {
            [major, row]
        } else {
            [row, major]
        };
        if p[0] < 0 || p[1] < 0 || p[0] >= width as i32 || p[1] >= height as i32 {
            return None;
        }
        if classify(&self.light, p) != chunk.sector {
            return None;
        }
        if e > 0 && line_row(sector, chunk.k, e - 1, local) == row {
            return None;
        }
        if e < 0 && line_row(sector, chunk.k, e + 1, local) == row {
            return None;
        }
        Some([p[0] as u32, p[1] as u32])
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SectorGpu {
    ints: [i32; 4],
    floats: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    screen: [f32; 4],
    dispatch: [u32; 4],
    light_i: [i32; 4],
    light_f: [f32; 4],
    tune: [f32; 4],
    tune2: [f32; 4],
    sectors: [SectorGpu; 4],
}

/// Workgroups per grid row (the per-dimension dispatch limit is 65535).
const GRID_X: u32 = 32_768;

/// The contact-shadow target and pipeline. Rebuilt when the resolution changes.
#[derive(Default)]
pub(crate) struct ContactShadowPass {
    pipeline: Option<wgpu::ComputePipeline>,
    bgl: Option<wgpu::BindGroupLayout>,
    params: Option<wgpu::Buffer>,
    chunks: Option<wgpu::Buffer>,
    chunk_capacity: usize,
    texture: Option<wgpu::Texture>,
    view: Option<wgpu::TextureView>,
    storage: Option<wgpu::TextureView>,
    size: (u32, u32),
    param_bytes: Vec<u8>,
    chunk_bytes: Vec<u8>,
    bg_cache:
        crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::TextureView, wgpu::Buffer, wgpu::TextureView)>,
}

impl ContactShadowPass {
    /// Ensures the pipeline and the `width × height` target exist and returns the view the
    /// forward pass samples. [`Self::dispatch`] writes it.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<wgpu::TextureView, RenderError> {
        self.ensure(device, width.max(1), height.max(1))?;
        self.view
            .clone()
            .ok_or_else(|| RenderError::Shader("contact shadow view missing".into()))
    }

    /// Records the march over `depth` (single-sample, DirectX-convention perspective,
    /// `width × height` = the target [`Self::prepare`] made). `clip_sun` is
    /// `view_proj · (to_sun, 0)` with the projection that drew `depth` (jitter included),
    /// `viewport` the pixel rectangle NDC maps to, and `near`/`far` the projection's planes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
        clip_sun: [f32; 4],
        viewport: [f32; 4],
        near: f32,
        far: f32,
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("contact_shadows").entered();
        let _crumb = crate::breadcrumbs::frame_pass("contact_shadows");
        self.ensure(device, width.max(1), height.max(1))?;
        let plan = plan(LightPoint::from_clip(clip_sun, viewport), width, height);
        if plan.workgroups == 0 || plan.chunks.is_empty() {
            return Ok(());
        }
        let device = &crate::device::TrackedDevice::observe(device);
        if plan.chunks.len() > self.chunk_capacity {
            self.chunk_capacity = plan.chunks.len().next_power_of_two();
            self.chunks = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("contact shadow chunks"),
                size: (self.chunk_capacity * std::mem::size_of::<Chunk>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.chunk_bytes.clear();
            self.bg_cache.clear();
        }
        let steps = steps_for_height(height);
        let grid_x = plan.workgroups.min(GRID_X);
        let grid_y = plan.workgroups.div_ceil(grid_x);
        let sector = |s: &Sector| SectorGpu {
            ints: [s.axis, s.sign, s.c0, s.minor_int],
            floats: [s.delta, s.minor_frac, 0.0, 0.0],
        };
        let params = Params {
            screen: [width as f32, height as f32, near, far],
            dispatch: [
                u32::from(plan.light.toward),
                plan.workgroups,
                plan.chunks.len() as u32,
                grid_x,
            ],
            light_i: [plan.light.int[0], plan.light.int[1], steps as i32, 0],
            light_f: [
                plan.light.frac[0],
                plan.light.frac[1],
                INTERP_ERROR,
                THICKNESS_MAX_M,
            ],
            tune: [BIAS, THICKNESS, THICKNESS_MIN_M, STRENGTH],
            tune2: [
                FADE_START_M,
                FADE_END_M,
                EDGE,
                (steps as f32 * HARD_FRACTION).max(1.0),
            ],
            sectors: plan.sectors.map(|s| sector(&s)),
        };
        let params_buf = self.params.clone().expect("contact shadow params");
        let chunk_buf = self.chunks.clone().expect("contact shadow chunks");
        belt.write_if_changed(
            device,
            encoder,
            &params_buf,
            bytemuck::bytes_of(&params),
            &mut self.param_bytes,
        );
        belt.write_if_changed(
            device,
            encoder,
            &chunk_buf,
            bytemuck::cast_slice(&plan.chunks),
            &mut self.chunk_bytes,
        );
        let storage = self.storage.clone().expect("contact shadow storage");
        let layout = self.bgl.clone().expect("contact shadow bgl");
        let bg = self.bg_cache.get_or_insert_with(
            (
                params_buf.clone(),
                depth.clone(),
                chunk_buf.clone(),
                storage.clone(),
            ),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("contact shadows"),
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: params_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(depth),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: chunk_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&storage),
                        },
                    ],
                })
            },
        );
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("contact shadows"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(self.pipeline.as_ref().expect("contact shadow pipeline"));
        pass.counted_set_bind_group(0, &bg, &[]);
        pass.counted_dispatch(grid_x, grid_y, 1);
        Ok(())
    }

    fn ensure(&mut self, device: &wgpu::Device, width: u32, height: u32) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        if self.pipeline.is_none() {
            let module = compose_source(
                include_str!("../shaders/contact_shadows.wgsl"),
                "contact_shadows.wgsl",
                HashMap::new(),
            )?;
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("contact shadows"),
                source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
            });
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("contact shadow bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::StorageTexture {
                            access: wgpu::StorageTextureAccess::WriteOnly,
                            format: wgpu::TextureFormat::Rgba8Unorm,
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                        count: None,
                    },
                ],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("contact shadow layout"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            self.pipeline = Some(device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("contact shadows"),
                layout: Some(&layout),
                module: &module,
                entry_point: Some("cs_main"),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            }));
            self.bgl = Some(bgl);
            self.params = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("contact shadow params"),
                size: std::mem::size_of::<Params>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if self.size != (width, height) || self.texture.is_none() {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("contact shadows"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = |usage: wgpu::TextureUsages| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("contact shadows"),
                    format: Some(wgpu::TextureFormat::Rgba8Unorm),
                    usage: Some(usage),
                    ..Default::default()
                })
            };
            self.storage = Some(view(wgpu::TextureUsages::STORAGE_BINDING));
            self.view = Some(view(wgpu::TextureUsages::TEXTURE_BINDING));
            self.texture = Some(texture);
            self.size = (width, height);
            self.bg_cache.clear();
            // The chunk count bound grows with the larger side; reserve it now so the first
            // frames after a resize do not reallocate.
            let bound = (4 * (width.max(height) as usize / GROUP as usize + 2)).next_power_of_two();
            if bound > self.chunk_capacity {
                self.chunk_capacity = bound;
                self.chunks = Some(device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("contact shadow chunks"),
                    size: (bound * std::mem::size_of::<Chunk>()) as u64,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
                self.chunk_bytes.clear();
            }
        }
        Ok(())
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "contact_shadows.wgsl",
            "Chunk",
            Chunk {
                sector,
                k,
                e_lo,
                start
            }
        ),
        crate::contract_row!(
            "contact_shadows.wgsl",
            "Params",
            Params {
                screen,
                dispatch,
                light_i,
                light_f,
                tune,
                tune2,
                sectors
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pixel written exactly once, for `clip` on a `w × h` target.
    fn assert_exactly_once(clip: [f32; 4], w: u32, h: u32) {
        let light = LightPoint::from_clip(clip, [0.0, 0.0, w as f32, h as f32]);
        let plan = plan(light, w, h);
        let mut writes = vec![0u32; (w * h) as usize];
        for wid in 0..plan.workgroups {
            for local in 0..GROUP {
                if let Some([x, y]) = plan.owner_pixel(wid, local, w, h) {
                    writes[(y * w + x) as usize] += 1;
                }
            }
        }
        let bad: Vec<(u32, u32, u32)> = writes
            .iter()
            .enumerate()
            .filter(|(_, n)| **n != 1)
            .take(8)
            .map(|(i, n)| (i as u32 % w, i as u32 / w, *n))
            .collect();
        assert!(
            bad.is_empty(),
            "clip {clip:?} light {light:?}: pixels not written exactly once (x, y, writes): {bad:?}"
        );
        // The overlap of converging lines stays small.
        let threads = u64::from(plan.workgroups) * GROUP as u64;
        assert!(
            threads <= u64::from(w * h) * 4 + 64 * 64 * 16,
            "{threads} threads for {} pixels",
            w * h
        );
    }

    #[test]
    fn light_on_screen_covers_every_pixel_once() {
        // NDC (0.1, -0.2), w > 0: the point is inside the frame.
        assert_exactly_once([0.1, -0.2, 0.3, 1.0], 203, 117);
        assert_exactly_once([0.0, 0.0, 0.5, 1.0], 160, 90);
        // Exactly on a pixel centre and on a pixel corner.
        assert_exactly_once([0.0125, 0.0, 0.5, 1.0], 80, 64);
        assert_exactly_once([0.0, 0.0, 0.5, 1.0], 80, 64);
    }

    #[test]
    fn light_off_screen_covers_every_pixel_once() {
        assert_exactly_once([3.0, 0.4, 0.3, 1.0], 160, 90);
        assert_exactly_once([-0.5, 2.5, 0.3, 1.0], 160, 90);
        assert_exactly_once([7.0, -9.0, 0.3, 1.0], 131, 77);
    }

    #[test]
    fn sun_behind_the_camera_covers_every_pixel_once() {
        let light = LightPoint::from_clip([0.2, 0.3, -0.5, -1.0], [0.0, 0.0, 160.0, 90.0]);
        assert!(!light.toward, "w < 0 marches away from the anti-point");
        assert_exactly_once([0.2, 0.3, -0.5, -1.0], 160, 90);
        assert_exactly_once([4.0, 0.3, -0.5, -0.5], 160, 90);
    }

    #[test]
    fn sun_parallel_to_the_view_plane_covers_every_pixel_once() {
        // w ≈ 0: the point is clamped far away along the rays' screen direction.
        assert_exactly_once([0.7, 0.2, 0.0, 0.0], 160, 90);
        assert_exactly_once([0.7, 0.2, 0.0, 1e-9], 160, 90);
        assert_exactly_once([-0.1, 0.9, 0.0, -1e-7], 120, 96);
        let light = LightPoint::from_clip([0.7, 0.2, 0.0, 0.0], [0.0, 0.0, 160.0, 90.0]);
        assert!(light.int[0] > 1000, "clamped far to the right: {light:?}");
    }

    #[test]
    fn large_target_stays_within_dispatch_limits() {
        let light = LightPoint::from_clip([0.0, 0.0, 0.5, 1.0], [0.0, 0.0, 2560.0, 1440.0]);
        let plan = plan(light, 2560, 1440);
        let pixels = 2560u64 * 1440;
        let threads = u64::from(plan.workgroups) * GROUP as u64;
        assert!(threads < pixels * 3 / 2, "{threads} threads for {pixels} pixels");
        assert!(plan.chunks.len() <= 4 * (2560 / GROUP as usize + 2));
        assert!(plan.workgroups.div_ceil(GRID_X) < 65_535);
    }

    #[test]
    fn march_length_follows_the_target_height() {
        assert_eq!(steps_for_height(1440), STEPS);
        assert_eq!(steps_for_height(2160), STEPS);
        assert_eq!(steps_for_height(1080), 36);
        assert_eq!(steps_for_height(540), 18);
        assert_eq!(steps_for_height(180), 6);
        // A 160×90 golden marches 4 pixels, not a quarter of a 1440-line march's screen share
        // times four.
        assert_eq!(steps_for_height(90), MIN_STEPS);
        assert_eq!(steps_for_height(0), MIN_STEPS);
    }

    #[test]
    fn steps_match_the_shader() {
        let src = include_str!("../shaders/contact_shadows.wgsl");
        assert!(src.contains(&format!("const STEPS: u32 = {STEPS}u;")));
        assert!(src.contains(&format!("const SLOTS: u32 = {}u;", GROUP as u32 + STEPS + 1)));
    }
}
