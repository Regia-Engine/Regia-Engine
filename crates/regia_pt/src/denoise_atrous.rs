// SPDX-License-Identifier: MPL-2.0
//! The built-in preview denoiser: an edge-avoiding a-trous wavelet filter that needs no native
//! library (ADR-2433, `book/agents/pt-presets.md`).
//!
//! The algorithm is the public one: the a-trous (with holes) B3-spline wavelet filter whose taps
//! are weighted by the first-hit normal, depth and luminance, after Dammertz, Sewtz, Hanika and
//! Lensch, "Edge-Avoiding A-Trous Wavelet Transform for Fast Global Illumination Filtering"
//! (HPG 2010), with the variance-guided luminance weight and variance propagation of Schied et
//! al., "Spatiotemporal Variance-Guided Filtering" (HPG 2017); only SVGF's spatial part is used
//! (no temporal accumulation). [`variance_from_halves`] returns `(a − b)² / 4` for two equal-count
//! halves. The preset path averages that term, in the form `(a − b)² · h · (n − h) / n²`, with the
//! image's second-moment variance, then passes the average here. Before the first iteration the
//! variance is divided by the demodulation luminance squared and replaced by the mean of a 5×5
//! window of same-surface neighbors. Each iteration's luminance weight uses a 3×3 Gaussian of
//! that stored variance.
//!
//! The beauty is **demodulated** by the first-hit albedo before filtering (so texture detail and
//! albedo edges survive: they are multiplied back afterwards) and filtered five times with steps
//! 1, 2, 4, 8, 16. Two implementations share one definition: [`filter_cpu`] (the twin the tests
//! compare against) and [`filter_gpu`] (`shaders/atrous.wgsl`, one dispatch per iteration;
//! lavapipe runs it in CI). Preparing the guide buffers and remodulating are shared Rust.
//!
//! The result is **a preview**: it is marked with [`crate::output::DENOISER_KEY`] and [`crate::compare`] refuses
//! it as a reference. Only `beauty` is filtered, and only its lit part: light seen directly
//! (the `emissive` and `sky` AOVs) is subtracted before filtering and added back unchanged.

use std::borrow::Cow;
#[cfg(test)]
use std::cell::Cell;

use regia_render::RenderDevice;

#[cfg(test)]
thread_local! {
    /// How many times [`variance_from_image`] has built a full-frame variance plane on this thread.
    pub(crate) static VARIANCE_CONVERSIONS: Cell<u64> = const { Cell::new(0) };
}

use crate::PtError;
use crate::output::PtImage;
use crate::presets::{bad, finite_non_negative};

const SHADER: &str = include_str!("../shaders/atrous.wgsl");

/// The most iterations (steps `1, 2, 4, ...`) a filter may run.
pub const MAX_ITERATIONS: u32 = 8;

/// The value [`crate::output::DENOISER_KEY`] carries for this filter (the iteration count follows it).
pub const BUILTIN_NAME: &str = "regia a-trous svgf-variance";

/// Filter parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtrousSettings {
    /// Wavelet iterations, `1..=`[`MAX_ITERATIONS`]; the support grows as `4 * (2^n - 1) + 1` pixels.
    pub iterations: u32,
    /// Luminance edge-stopping strength. The shipped default is 3. The weight is
    /// `exp(-|dl| / (sigma_luma * sqrt(v) + 1e-4))`, where `v` is a 3×3 Gaussian of the stored
    /// variance. Larger smooths more.
    pub sigma_luma: f32,
    /// Normal edge-stopping exponent (SVGF `σ_n`, 128); larger keeps more creases.
    pub sigma_normal: f32,
    /// Depth edge-stopping strength. The shipped default is 4. The weight is
    /// `exp(-|dz| / (sigma_depth * plane + 1e-3 * max(z, 1)))`, where `dz` is the depth
    /// difference and `plane` is the depth gradient's predicted step. Larger smooths across
    /// more depth change.
    pub sigma_depth: f32,
}

impl Default for AtrousSettings {
    fn default() -> Self {
        Self {
            iterations: 5,
            sigma_luma: 3.0,
            sigma_normal: 128.0,
            sigma_depth: 4.0,
        }
    }
}

impl AtrousSettings {
    /// Checks the numbers.
    ///
    /// # Errors
    ///
    /// Zero or more than [`MAX_ITERATIONS`] iterations, or a sigma that is NaN, infinite or not
    /// above zero.
    pub fn validate(&self) -> Result<(), PtError> {
        if self.iterations == 0 || self.iterations > MAX_ITERATIONS {
            return Err(bad(format!(
                "denoise iterations must be 1..={MAX_ITERATIONS}, got {}",
                self.iterations
            )));
        }
        for (name, v) in [
            ("sigma_luma", self.sigma_luma),
            ("sigma_normal", self.sigma_normal),
            ("sigma_depth", self.sigma_depth),
        ] {
            finite_non_negative(name, v)?;
            if v == 0.0 {
                return Err(bad(format!("{name} must be above 0")));
            }
        }
        Ok(())
    }
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Variance of the mean of two equal-count half buffers' mean, as luminance: `(a - b)² / 4`.
/// Non-finite pixels give 0.
///
/// # Errors
///
/// The buffers differ in length.
pub fn variance_from_halves(a: &[[f32; 3]], b: &[[f32; 3]]) -> Result<Vec<f32>, PtError> {
    if a.len() != b.len() {
        return Err(bad("half buffers differ in length"));
    }
    Ok(a.iter()
        .zip(b)
        .map(|(a, b)| {
            let d = luminance(*a) - luminance(*b);
            let v = 0.25 * d * d;
            if v.is_finite() { v } else { 0.0 }
        })
        .collect())
}

/// Luminance variance of the mean from the image's own `variance` AOV (the per-channel variance
/// of the mean, combined with the luminance weights squared).
#[must_use]
pub fn variance_from_image(image: &PtImage) -> Vec<f32> {
    #[cfg(test)]
    VARIANCE_CONVERSIONS.with(|n| n.set(n.get() + 1));
    image
        .variance
        .iter()
        .map(|v| {
            let s = 0.2126 * 0.2126 * v[0] + 0.7152 * 0.7152 * v[1] + 0.0722 * 0.0722 * v[2];
            if s.is_finite() { s.max(0.0) } else { 0.0 }
        })
        .collect()
}

/// Guide buffers and the demodulated signal, shared by the CPU and GPU filters.
struct Prepared {
    /// Demodulated beauty (rgb) and the variance of its luminance (w).
    signal: Vec<[f32; 4]>,
    /// First-hit normal (xyz) and depth (w; 0 = no surface).
    guide: Vec<[f32; 4]>,
    /// What to multiply the filtered signal by.
    factor: Vec<[f32; 3]>,
    /// Light seen directly (emitters, sky): added back unfiltered, so a bright source is not
    /// blurred into its surroundings.
    direct_view: Vec<[f32; 3]>,
}

/// Albedo luminance under which a pixel is not demodulated (emitters, sky, black surfaces).
const ALBEDO_FLOOR: f32 = 0.02;

fn prepare(image: &PtImage, variance: &[f32]) -> Result<Prepared, PtError> {
    let n = (image.width as usize) * (image.height as usize);
    if n == 0
        || image.beauty.len() != n
        || image.albedo.len() != n
        || image.normal.len() != n
        || image.depth.len() != n
        || variance.len() != n
    {
        return Err(PtError::Scene(format!(
            "cannot denoise a {}x{} image whose AOVs do not match (beauty {}, albedo {}, normal {}, depth {}, variance {})",
            image.width,
            image.height,
            image.beauty.len(),
            image.albedo.len(),
            image.normal.len(),
            image.depth.len(),
            variance.len()
        )));
    }
    let mut signal = Vec::with_capacity(n);
    let mut guide = Vec::with_capacity(n);
    let mut factor = Vec::with_capacity(n);
    let mut direct_view = Vec::with_capacity(n);
    let seen = |plane: &[[f32; 3]], i: usize| -> [f32; 3] {
        match plane.get(i) {
            Some(c) if plane.len() == n && c.iter().all(|v| v.is_finite()) => c.map(|v| v.max(0.0)),
            _ => [0.0; 3],
        }
    };
    for (i, &var_i) in variance.iter().enumerate() {
        let finite = |c: [f32; 3]| {
            if c.iter().all(|v| v.is_finite()) {
                c
            } else {
                [0.0; 3]
            }
        };
        let view = {
            let (e, k) = (seen(&image.emissive, i), seen(&image.sky, i));
            [e[0] + k[0], e[1] + k[1], e[2] + k[2]]
        };
        let beauty_all = finite(image.beauty[i]).map(|v| v.max(0.0));
        let beauty = [
            (beauty_all[0] - view[0]).max(0.0),
            (beauty_all[1] - view[1]).max(0.0),
            (beauty_all[2] - view[2]).max(0.0),
        ];
        direct_view.push(view);
        let albedo = finite(image.albedo[i]);
        let depth = if image.depth[i].is_finite() {
            image.depth[i].max(0.0)
        } else {
            0.0
        };
        let normal = finite(image.normal[i]);
        let f = if depth > 0.0 && luminance(albedo) >= ALBEDO_FLOOR {
            albedo.map(|a| a.max(ALBEDO_FLOOR))
        } else {
            [1.0; 3]
        };
        let lf = luminance(f).max(1e-6);
        let var = if var_i.is_finite() { var_i.max(0.0) } else { 0.0 } / (lf * lf);
        signal.push([beauty[0] / f[0], beauty[1] / f[1], beauty[2] / f[2], var]);
        guide.push([normal[0], normal[1], normal[2], depth]);
        factor.push(f);
    }
    smooth_variance(&mut signal, &guide, image.width as usize, image.height as usize);
    Ok(Prepared {
        signal,
        guide,
        factor,
        direct_view,
    })
}

/// A single `(a - b)² / 4` is a one-degree-of-freedom estimate: average it over a 5x5 window of
/// pixels on the same surface (similar normal and depth) before the filter uses it.
fn smooth_variance(signal: &mut [[f32; 4]], guide: &[[f32; 4]], w: usize, h: usize) {
    let src: Vec<f32> = signal.iter().map(|s| s[3]).collect();
    for y in 0..h {
        for x in 0..w {
            let p = y * w + x;
            let g = guide[p];
            let (mut sum, mut cnt) = (0.0f32, 0.0f32);
            for dy in -2i32..=2 {
                for dx in -2i32..=2 {
                    let (qx, qy) = (x as i32 + dx, y as i32 + dy);
                    if qx < 0 || qy < 0 || qx >= w as i32 || qy >= h as i32 {
                        continue;
                    }
                    let q = qy as usize * w + qx as usize;
                    let r = guide[q];
                    let same = (g[0] * r[0] + g[1] * r[1] + g[2] * r[2]) > 0.9
                        && (g[3] - r[3]).abs() <= 0.05 * g[3].max(r[3]).max(1e-6)
                        || (g[3] == 0.0 && r[3] == 0.0);
                    if same {
                        sum += src[q];
                        cnt += 1.0;
                    }
                }
            }
            signal[p][3] = if cnt > 0.0 { sum / cnt } else { src[p] };
        }
    }
}

fn kernel(i: i32) -> f32 {
    // B3 spline: 1/16, 1/4, 3/8, 1/4, 1/16.
    match i.abs() {
        0 => 3.0 / 8.0,
        1 => 1.0 / 4.0,
        _ => 1.0 / 16.0,
    }
}

/// One a-trous iteration on the CPU (the twin of `atrous` in `shaders/atrous.wgsl`).
fn iterate_cpu(
    src: &[[f32; 4]],
    guide: &[[f32; 4]],
    w: usize,
    h: usize,
    step: i32,
    s: &AtrousSettings,
) -> Vec<[f32; 4]> {
    let at = |x: i32, y: i32| -> usize {
        (y.clamp(0, h as i32 - 1) as usize) * w + x.clamp(0, w as i32 - 1) as usize
    };
    let mut dst = vec![[0.0f32; 4]; src.len()];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let p = at(x, y);
            let c = src[p];
            let g = guide[p];
            let lp = luminance([c[0], c[1], c[2]]);
            // SVGF: the 3x3 gaussian-filtered variance sets the luminance tolerance.
            let mut vg = 0.0f32;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let k = (if dx == 0 { 2.0 } else { 1.0 }) * (if dy == 0 { 2.0 } else { 1.0 }) / 16.0;
                    vg += k * src[at(x + dx, y + dy)][3];
                }
            }
            let sigma_l = s.sigma_luma * vg.max(0.0).sqrt() + 1e-4;
            // Depth gradient (central differences) for the plane-distance tolerance.
            let zx = 0.5 * (guide[at(x + 1, y)][3] - guide[at(x - 1, y)][3]);
            let zy = 0.5 * (guide[at(x, y + 1)][3] - guide[at(x, y - 1)][3]);
            let z_eps = 1e-3 * g[3].max(1.0);
            let mut acc = [0.0f32; 3];
            let (mut var, mut wsum) = (0.0f32, 0.0f32);
            for dy in -2..=2 {
                for dx in -2..=2 {
                    let (qx, qy) = (x + dx * step, y + dy * step);
                    if qx < 0 || qy < 0 || qx >= w as i32 || qy >= h as i32 {
                        continue;
                    }
                    let q = qy as usize * w + qx as usize;
                    let (cq, gq) = (src[q], guide[q]);
                    let wn = (g[0] * gq[0] + g[1] * gq[1] + g[2] * gq[2])
                        .max(0.0)
                        .powf(s.sigma_normal);
                    let plane = (zx * (dx * step) as f32 + zy * (dy * step) as f32).abs();
                    let wz = (-(g[3] - gq[3]).abs() / (s.sigma_depth * plane + z_eps)).exp();
                    let lq = luminance([cq[0], cq[1], cq[2]]);
                    let wl = (-(lp - lq).abs() / sigma_l).exp();
                    let wt = kernel(dx) * kernel(dy) * wn * wz * wl;
                    acc[0] += wt * cq[0];
                    acc[1] += wt * cq[1];
                    acc[2] += wt * cq[2];
                    var += wt * wt * cq[3];
                    wsum += wt;
                }
            }
            dst[p] = if wsum > 0.0 {
                [acc[0] / wsum, acc[1] / wsum, acc[2] / wsum, var / (wsum * wsum)]
            } else {
                c
            };
        }
    }
    dst
}

fn filter_cpu_prepared(p: &Prepared, w: usize, h: usize, s: &AtrousSettings) -> Vec<[f32; 4]> {
    let mut cur = p.signal.clone();
    for i in 0..s.iterations {
        cur = iterate_cpu(&cur, &p.guide, w, h, 1 << i, s);
    }
    cur
}

fn finish(image: &PtImage, p: &Prepared, filtered: &[[f32; 4]], s: &AtrousSettings, seconds: f64) -> PtImage {
    let mut out = image.clone();
    for (i, b) in out.beauty.iter_mut().enumerate() {
        let f = filtered[i];
        let m = p.factor[i];
        let v = p.direct_view[i];
        *b = [
            (f[0] * m[0]).max(0.0) + v[0],
            (f[1] * m[1]).max(0.0) + v[1],
            (f[2] * m[2]).max(0.0) + v[2],
        ];
        for c in b.iter_mut() {
            if !c.is_finite() {
                *c = 0.0;
            }
        }
    }
    out.metadata.denoiser = format!("{BUILTIN_NAME} {}it", s.iterations);
    out.metadata.denoise_seconds = seconds;
    out
}

/// Denoises `image`'s beauty on the CPU with the per-pixel luminance `variance` (see
/// [`variance_from_halves`], [`variance_from_image`]) and returns the preview: every other AOV
/// unchanged, `metadata.denoiser` set (so [`crate::compare`] refuses it as a reference).
///
/// # Errors
///
/// Invalid settings, or an image whose AOVs do not match its size.
pub fn filter_cpu(image: &PtImage, variance: &[f32], settings: &AtrousSettings) -> Result<PtImage, PtError> {
    let _span = tracing::info_span!(
        "regia_pt::denoise_atrous",
        backend = "cpu",
        width = image.width,
        height = image.height
    )
    .entered();
    settings.validate()?;
    let started = std::time::Instant::now();
    let prepared = prepare(image, variance)?;
    let filtered = filter_cpu_prepared(&prepared, image.width as usize, image.height as usize, settings);
    Ok(finish(
        image,
        &prepared,
        &filtered,
        settings,
        started.elapsed().as_secs_f64(),
    ))
}

/// [`filter_cpu`] on the GPU (`shaders/atrous.wgsl`): the same filter, agreeing with the CPU twin
/// to float rounding.
///
/// # Errors
///
/// See [`filter_cpu`]; also a shader, buffer or readback failure.
pub fn filter_gpu(
    device: &RenderDevice,
    image: &PtImage,
    variance: &[f32],
    settings: &AtrousSettings,
) -> Result<PtImage, PtError> {
    let _span = tracing::info_span!(
        "regia_pt::denoise_atrous",
        backend = "gpu",
        width = image.width,
        height = image.height
    )
    .entered();
    settings.validate()?;
    let started = std::time::Instant::now();
    let prepared = prepare(image, variance)?;
    let filtered = run_gpu(device, &prepared, image.width, image.height, settings)?;
    Ok(finish(
        image,
        &prepared,
        &filtered,
        settings,
        started.elapsed().as_secs_f64(),
    ))
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    size: [u32; 2],
    step: u32,
    _pad: u32,
    sigmas: [f32; 4],
}

fn run_gpu(
    rd: &RenderDevice,
    p: &Prepared,
    width: u32,
    height: u32,
    s: &AtrousSettings,
) -> Result<Vec<[f32; 4]>, PtError> {
    use wgpu::util::DeviceExt;
    let d = &rd.device;
    let q = &rd.queue;
    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("regia_pt atrous"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
    });
    let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("regia_pt atrous"),
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
            storage(1, true),
            storage(2, true),
            storage(3, false),
        ],
    });
    let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("regia_pt atrous"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("regia_pt atrous"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("atrous"),
        compilation_options: Default::default(),
        cache: None,
    });
    let bytes = (p.signal.len() * 16) as u64;
    let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST;
    let ping = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("atrous ping"),
        contents: bytemuck::cast_slice(&p.signal),
        usage,
    });
    let pong = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("atrous pong"),
        size: bytes,
        usage,
        mapped_at_creation: false,
    });
    let guide = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("atrous guide"),
        contents: bytemuck::cast_slice(&p.guide),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let mut bind_groups = Vec::new();
    let mut _uniforms = Vec::new();
    for i in 0..s.iterations {
        let params = Params {
            size: [width, height],
            step: 1 << i,
            _pad: 0,
            sigmas: [s.sigma_luma, s.sigma_normal, s.sigma_depth, 0.0],
        };
        let ub = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("atrous params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let (src, dst) = if i % 2 == 0 {
            (&ping, &pong)
        } else {
            (&pong, &ping)
        };
        bind_groups.push(d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atrous"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ub.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: src.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: guide.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: dst.as_entire_binding(),
                },
            ],
        }));
        _uniforms.push(ub);
    }
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("regia_pt atrous"),
    });
    for bg in &bind_groups {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("atrous iteration"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, bg, &[]);
        pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
    }
    let result = if s.iterations % 2 == 1 { &pong } else { &ping };
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("atrous readback"),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(result, 0, &staging, 0, bytes);
    q.submit([encoder.finish()]);
    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
        return Err(PtError::Gpu(err.to_string()));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    rx.recv()
        .map_err(|_| PtError::Gpu("map callback dropped".into()))?
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let mapped = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let out: Vec<[f32; 4]> = bytemuck::cast_slice::<u8, [f32; 4]>(&mapped).to_vec();
    drop(mapped);
    staging.unmap();
    Ok(out)
}

/// Pulls fireflies down: a pixel whose luminance exceeds `threshold` times the median luminance
/// of its 3x3 neighbors (never below 5 % of the image's mean luminance, so an isolated light on
/// black is not erased) is scaled to that bound, keeping its color. `threshold` 0 does nothing.
/// Biased by design: it removes energy, so only previews use it.
///
/// # Errors
///
/// `threshold` is NaN, infinite, negative or in `(0, 1]`, or the buffer does not match the size.
pub fn suppress_fireflies(
    beauty: &mut [[f32; 3]],
    width: u32,
    height: u32,
    threshold: f32,
) -> Result<(), PtError> {
    finite_non_negative("firefly_threshold", threshold)?;
    if threshold == 0.0 {
        return Ok(());
    }
    if threshold <= 1.0 {
        return Err(bad("firefly_threshold must be 0 (off) or above 1"));
    }
    let (w, h) = (width as usize, height as usize);
    if beauty.len() != w * h {
        return Err(bad("firefly buffer does not match the image size"));
    }
    let lum: Vec<f32> = beauty
        .iter()
        .map(|c| {
            let l = luminance(*c);
            if l.is_finite() { l.max(0.0) } else { 0.0 }
        })
        .collect();
    let mean = lum.iter().sum::<f32>() / lum.len().max(1) as f32;
    let floor = 0.05 * mean;
    for y in 0..h {
        for x in 0..w {
            let mut around = Vec::with_capacity(8);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (qx, qy) = (x as i32 + dx, y as i32 + dy);
                    if (dx, dy) != (0, 0) && qx >= 0 && qy >= 0 && qx < w as i32 && qy < h as i32 {
                        around.push(lum[qy as usize * w + qx as usize]);
                    }
                }
            }
            around.sort_by(f32::total_cmp);
            let median = around.get(around.len() / 2).copied().unwrap_or(0.0);
            let bound = threshold * median.max(floor);
            let l = lum[y * w + x];
            if l > bound && l > 0.0 {
                let k = bound / l;
                for c in &mut beauty[y * w + x] {
                    *c *= k;
                }
            }
        }
    }
    Ok(())
}

/// True when `metadata.denoiser` marks an image this module made.
#[must_use]
pub fn is_builtin_mark(denoiser: &str) -> bool {
    denoiser.starts_with(BUILTIN_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PtMetadata;

    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0 as f32 / u32::MAX as f32
        }
    }

    /// A scene of two flat panels with known clean irradiance, two noisy half buffers and their
    /// mean. `split` decides what differs across the left/right boundary: albedo, normal, or
    /// nothing.
    struct Panels {
        image: PtImage,
        halves: (Vec<[f32; 3]>, Vec<[f32; 3]>),
        clean: Vec<[f32; 3]>,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Split {
        None,
        Albedo,
        Normal,
    }

    fn panels(w: u32, h: u32, split: Split, noise: f32, seed: u32) -> Panels {
        let n = (w * h) as usize;
        let mut rng = Rng(seed);
        let (mut clean, mut a, mut b, mut albedo, mut normal) = (vec![], vec![], vec![], vec![], vec![]);
        for i in 0..n {
            let right = (i as u32 % w) >= w / 2;
            let alb = if split == Split::Albedo && right { 0.7 } else { 0.2 };
            let nrm = if split == Split::Normal && right {
                [0.6, 0.0, 0.8]
            } else {
                [0.0, 0.0, 1.0]
            };
            // Irradiance 100 on the left; the normal split lights the right panel differently.
            let irr = if split == Split::Normal && right {
                40.0
            } else {
                100.0
            };
            let c = [irr * alb; 3];
            clean.push(c);
            a.push(c.map(|v| v * (1.0 + noise * (2.0 * rng.next() - 1.0))));
            b.push(c.map(|v| v * (1.0 + noise * (2.0 * rng.next() - 1.0))));
            albedo.push([alb; 3]);
            normal.push(nrm);
        }
        let beauty: Vec<[f32; 3]> = a
            .iter()
            .zip(&b)
            .map(|(x, y)| [0, 1, 2].map(|k| 0.5 * (x[k] + y[k])))
            .collect();
        let image = PtImage {
            width: w,
            height: h,
            beauty,
            albedo,
            normal,
            depth: vec![5.0; n],
            direct: vec![[0.0; 3]; n],
            indirect_diffuse: vec![[0.0; 3]; n],
            indirect_specular: vec![[0.0; 3]; n],
            emissive: vec![[0.0; 3]; n],
            volumetric: vec![[0.0; 3]; n],
            sky: vec![[0.0; 3]; n],
            shadow: vec![1.0; n],
            roughness: vec![0.5; n],
            variance: vec![[0.0; 3]; n],
            spp: 8,
            spp_map: Vec::new(),
            metadata: PtMetadata {
                spp: 8,
                ..PtMetadata::default()
            },
        };
        Panels {
            image,
            halves: (a, b),
            clean,
        }
    }

    fn rmse(a: &[[f32; 3]], b: &[[f32; 3]]) -> f64 {
        let mut e = 0.0f64;
        for (p, q) in a.iter().zip(b) {
            for k in 0..3 {
                e += f64::from(p[k] - q[k]).powi(2);
            }
        }
        (e / (a.len() * 3) as f64).sqrt()
    }

    fn denoise(p: &Panels) -> PtImage {
        let var = variance_from_halves(&p.halves.0, &p.halves.1).expect("variance");
        filter_cpu(&p.image, &var, &AtrousSettings::default()).expect("filter")
    }

    #[test]
    fn noise_drops_and_the_preview_is_marked() {
        let p = panels(64, 48, Split::None, 0.5, 11);
        let out = denoise(&p);
        let before = rmse(&p.image.beauty, &p.clean);
        let after = rmse(&out.beauty, &p.clean);
        assert!(after < 0.5 * before, "rmse {before:.3} -> {after:.3}");
        assert!(
            is_builtin_mark(&out.metadata.denoiser),
            "{}",
            out.metadata.denoiser
        );
        assert_eq!(out.albedo, p.image.albedo, "aux AOVs stay raw");
    }

    #[test]
    fn a_flat_image_is_unchanged() {
        let mut p = panels(32, 32, Split::None, 0.0, 3);
        p.image.beauty.fill([20.0, 20.0, 20.0]);
        let out = filter_cpu(&p.image, &vec![0.0; 32 * 32], &AtrousSettings::default()).expect("filter");
        for (a, b) in out.beauty.iter().zip(&p.image.beauty) {
            for k in 0..3 {
                assert!((a[k] - b[k]).abs() <= 1e-4 * b[k], "{a:?} vs {b:?}");
            }
        }
    }

    /// Contrast across the boundary: mean of the right panel over the mean of the left, a few
    /// columns away from the seam so noise on the seam itself is not counted.
    fn contrast(beauty: &[[f32; 3]], w: u32, h: u32) -> f64 {
        let side = |x0: u32, x1: u32| {
            let (mut s, mut n) = (0.0f64, 0.0f64);
            for y in 0..h {
                for x in x0..x1 {
                    s += f64::from(beauty[(y * w + x) as usize][1]);
                    n += 1.0;
                }
            }
            s / n
        };
        let left = side(w / 2 - 6, w / 2 - 1);
        let right = side(w / 2 + 1, w / 2 + 6);
        right / left
    }

    #[test]
    fn an_albedo_edge_keeps_its_contrast() {
        let p = panels(64, 48, Split::Albedo, 0.4, 5);
        let out = denoise(&p);
        let truth = contrast(&p.clean, 64, 48);
        let got = contrast(&out.beauty, 64, 48);
        assert!(
            (got / truth - 1.0).abs() < 0.10,
            "contrast {got:.3} vs {truth:.3}"
        );
    }

    #[test]
    fn a_normal_edge_keeps_its_contrast() {
        let p = panels(64, 48, Split::Normal, 0.4, 9);
        let out = denoise(&p);
        let truth = contrast(&p.clean, 64, 48);
        let got = contrast(&out.beauty, 64, 48);
        assert!(
            (got / truth - 1.0).abs() < 0.10,
            "contrast {got:.3} vs {truth:.3}"
        );
    }

    #[test]
    fn hostile_input_is_an_error_or_finite() {
        let p = panels(8, 8, Split::None, 0.1, 1);
        let var = vec![0.0; 64];
        for bad_sigma in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            let s = AtrousSettings {
                sigma_luma: bad_sigma,
                ..AtrousSettings::default()
            };
            assert!(filter_cpu(&p.image, &var, &s).is_err());
        }
        for iterations in [0, MAX_ITERATIONS + 1, u32::MAX] {
            let s = AtrousSettings {
                iterations,
                ..AtrousSettings::default()
            };
            assert!(filter_cpu(&p.image, &var, &s).is_err());
        }
        assert!(filter_cpu(&p.image, &var[..10], &AtrousSettings::default()).is_err());
        let mut nan = p.image.clone();
        nan.beauty[5] = [f32::NAN, f32::INFINITY, -1.0];
        nan.depth[6] = f32::NAN;
        let mut v = var.clone();
        v[7] = f32::NAN;
        let out = filter_cpu(&nan, &v, &AtrousSettings::default()).expect("non-finite pixels are cleaned");
        assert!(out.beauty.iter().flatten().all(|c| c.is_finite() && *c >= 0.0));
        let mut empty = p.image.clone();
        empty.width = 0;
        assert!(filter_cpu(&empty, &[], &AtrousSettings::default()).is_err());
    }

    #[test]
    fn fireflies_are_capped_and_hostile_thresholds_error() {
        let mut img = vec![[1.0f32; 3]; 25];
        img[12] = [500.0; 3];
        suppress_fireflies(&mut img, 5, 5, 8.0).expect("filter");
        assert!(img[12][0] <= 9.0 && img[12][0] > 1.0, "{:?}", img[12]);
        assert_eq!(img[0], [1.0; 3]);
        let mut same = vec![[1.0f32; 3]; 25];
        suppress_fireflies(&mut same, 5, 5, 0.0).expect("off");
        assert_eq!(same, vec![[1.0f32; 3]; 25]);
        for t in [f32::NAN, f32::INFINITY, -2.0, 0.5, 1.0] {
            assert!(suppress_fireflies(&mut img, 5, 5, t).is_err(), "{t}");
        }
        assert!(suppress_fireflies(&mut img, 4, 4, 8.0).is_err());
    }

    /// The shipped defaults are sigma_luma 3 and sigma_depth 4. One iteration of a two-pixel
    /// step whose variance is 1 weights the neighbor by `exp(-|dl| / (3 * sqrt(v) + 1e-4))`.
    #[test]
    fn shipped_defaults_weight_luminance_with_sigma_three() {
        let settings = AtrousSettings {
            iterations: 1,
            ..AtrousSettings::default()
        };
        assert_eq!(settings.sigma_luma, 3.0);
        assert_eq!(settings.sigma_depth, 4.0);
        assert_eq!(settings.sigma_normal, 128.0);
        let n = 2;
        let image = PtImage {
            width: 2,
            height: 1,
            beauty: vec![[0.0; 3], [1.0; 3]],
            albedo: vec![[1.0; 3]; n],
            normal: vec![[0.0, 0.0, 1.0]; n],
            depth: vec![1.0; n],
            direct: vec![[0.0; 3]; n],
            indirect_diffuse: vec![[0.0; 3]; n],
            indirect_specular: vec![[0.0; 3]; n],
            emissive: vec![[0.0; 3]; n],
            volumetric: vec![[0.0; 3]; n],
            sky: vec![[0.0; 3]; n],
            shadow: vec![1.0; n],
            roughness: vec![0.5; n],
            variance: vec![[0.0; 3]; n],
            spp: 1,
            spp_map: Vec::new(),
            metadata: PtMetadata::default(),
        };
        let out = filter_cpu(&image, &[1.0, 1.0], &settings).expect("filter");
        let sigma_l = 3.0 * 1.0f32.sqrt() + 1e-4;
        let wl = (-1.0f32 / sigma_l).exp();
        let w_self = 3.0 / 8.0;
        let w_other = 0.25 * wl;
        let sum = w_self + w_other;
        assert!(
            (out.beauty[0][0] - w_other / sum).abs() < 1e-4,
            "pixel 0 {}",
            out.beauty[0][0]
        );
        assert!(
            (out.beauty[1][0] - w_self / sum).abs() < 1e-4,
            "pixel 1 {}",
            out.beauty[1][0]
        );
    }

    #[test]
    fn variance_from_halves_is_a_quarter_of_the_squared_difference() {
        let v = variance_from_halves(&[[2.0; 3]], &[[4.0; 3]]).expect("variance");
        assert!((v[0] - 1.0).abs() < 1e-6);
        assert!(variance_from_halves(&[[0.0; 3]], &[]).is_err());
    }
}
