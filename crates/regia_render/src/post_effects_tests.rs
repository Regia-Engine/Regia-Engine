// SPDX-License-Identifier: MPL-2.0
//! Correctness tests of the post effects (ADR-1152): bloom energy and centroid, motion blur at high
//! velocity, depth of field edge cases. None needs a golden image: each checks a property the
//! effect must have on a synthetic `Rgba16Float` frame.
//!
//! The bloom tests run a CPU model of the chain (`post.wgsl` modes 1–3, the post sampler's
//! clamp-to-edge bilinear filter) in `f64`, then check that the GPU chain matches the model. The
//! GPU tests skip without an adapter (lavapipe on the merge gate).

use super::*;
use crate::dof::{DepthOfField, DepthOfFieldMode, DepthOfFieldPass};
use crate::motion_blur::{MotionBlur, MotionBlurPass};

/// One channel of a `w × h` image, row-major.
#[derive(Clone, Debug)]
struct Image {
    w: usize,
    h: usize,
    px: Vec<f64>,
}

impl Image {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0.0; w * h],
        }
    }

    fn from_f32(w: usize, h: usize, px: &[f32]) -> Self {
        Self {
            w,
            h,
            px: px.iter().map(|&v| f64::from(v)).collect(),
        }
    }

    /// The texel at `(x, y)`, clamped to the edge (the post sampler's address mode).
    fn clamped(&self, x: i64, y: i64) -> f64 {
        let x = x.clamp(0, self.w as i64 - 1) as usize;
        let y = y.clamp(0, self.h as i64 - 1) as usize;
        self.px[y * self.w + x]
    }

    /// Bilinear sample at texel-space `(x, y)` (texel `i` spans `[i, i + 1)`), clamp to edge.
    fn bilinear(&self, x: f64, y: f64) -> f64 {
        let (fx, fy) = (x - 0.5, y - 0.5);
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        self.clamped(x0, y0) * (1.0 - tx) * (1.0 - ty)
            + self.clamped(x0 + 1, y0) * tx * (1.0 - ty)
            + self.clamped(x0, y0 + 1) * (1.0 - tx) * ty
            + self.clamped(x0 + 1, y0 + 1) * tx * ty
    }

    fn map(w: usize, h: usize, f: impl Fn(f64, f64) -> f64) -> Self {
        let mut out = Self::new(w, h);
        for (i, v) in out.px.iter_mut().enumerate() {
            *v = f((i % w) as f64 + 0.5, (i / w) as f64 + 0.5);
        }
        out
    }

    fn sum(&self) -> f64 {
        self.px.iter().sum()
    }

    /// Intensity-weighted centre `(x, y)`, texel space.
    fn centroid(&self) -> (f64, f64) {
        let (mut sx, mut sy) = (0.0, 0.0);
        for (i, v) in self.px.iter().enumerate() {
            sx += v * ((i % self.w) as f64 + 0.5);
            sy += v * ((i / self.w) as f64 + 0.5);
        }
        let s = self.sum();
        (sx / s, sy / s)
    }
}

/// `post.wgsl` mode 1 (`bloom_down`) into a target of `(w, h)`.
fn model_down(src: &Image, (w, h): (u32, u32)) -> Image {
    Image::map(w as usize, h as usize, |x, y| {
        let (cx, cy) = (2.0 * x, 2.0 * y);
        let mut sum = 0.0;
        for (ox, oy) in [(-0.75, -0.75), (0.75, -0.75), (-0.75, 0.75), (0.75, 0.75)] {
            sum += src.bilinear(cx + ox, cy + oy);
        }
        sum * 0.25
    })
}

/// `post.wgsl` mode 2: half the lower level (read at half the pixel position), half this level.
fn model_up(low: &Image, high: &Image) -> Image {
    Image::map(high.w, high.h, |x, y| {
        0.5 * low.bilinear(0.5 * x, 0.5 * y) + 0.5 * high.clamped(x as i64, y as i64)
    })
}

/// The whole chain as `PostChain::record` queues it, at a blend of 1 (the composite is the bloom).
fn model_bloom(src: &Image, mips: u32) -> Image {
    let sizes = bloom_level_sizes(src.w as u32, src.h as u32, mips);
    let mut down = Vec::new();
    let mut level = src.clone();
    for &size in &sizes {
        level = model_down(&level, size);
        down.push(level.clone());
    }
    let mut up = level;
    for high in down.iter().rev().skip(1) {
        up = model_up(&up, high);
    }
    Image::map(src.w, src.h, |x, y| up.bilinear(0.5 * x, 0.5 * y))
}

/// A `w × h` frame, black but for `value` at `(x, y)`.
fn point(w: usize, h: usize, x: usize, y: usize, value: f64) -> Image {
    let mut img = Image::new(w, h);
    img.px[y * w + x] = value;
    img
}

#[test]
fn bloom_levels_halve_rounding_up() {
    assert_eq!(
        bloom_level_sizes(1920, 1080, 5),
        vec![(960, 540), (480, 270), (240, 135), (120, 68), (60, 34)]
    );
    assert_eq!(bloom_level_sizes(101, 75, 3), vec![(51, 38), (26, 19), (13, 10)]);
    assert_eq!(bloom_level_sizes(1, 1, 3), vec![(1, 1); 3]);
    assert_eq!(bloom_level_sizes(0, 0, 1), vec![(1, 1)]);
    assert!(bloom_level_sizes(64, 64, 0).is_empty());
}

#[test]
fn a_non_finite_bloom_blend_is_no_bloom() {
    assert_eq!(bloom_blend(f32::NAN), 0.0);
    assert_eq!(bloom_blend(f32::INFINITY), 0.0);
    assert_eq!(bloom_blend(2.0), 1.0);
    assert_eq!(bloom_blend(-1.0), 0.0);
    assert_eq!(
        bloom_blend(crate::post::DEFAULT_BLOOM_INTENSITY),
        crate::post::DEFAULT_BLOOM_INTENSITY
    );
}

/// A highlight away from the border keeps its energy and its centre through the chain, wherever it
/// sits, for even and odd frame sizes. Before ADR-1152 an odd level was rounded down and addressed
/// by its own uv: the energy varied by ±3 % with the highlight's position and the glow's centre
/// drifted by up to a pixel (2 px with the 2 × 2 box downsample even on even sizes).
#[test]
fn the_bloom_model_keeps_a_highlights_energy_and_centroid() {
    for (w, h, mips) in [(101, 75, 3), (128, 96, 3), (161, 97, 3), (201, 151, 4)] {
        for dx in 0..8 {
            for dy in 0..2 {
                let (x, y) = (w / 2 - 4 + dx, h / 2 + dy);
                let bloom = model_bloom(&point(w, h, x, y, 1000.0), mips);
                let energy = bloom.sum() / 1000.0;
                assert!(
                    (energy - 1.0).abs() < 1e-9,
                    "{w}×{h}, {mips} mips, point ({x}, {y}): energy ×{energy}"
                );
                let (cx, cy) = bloom.centroid();
                assert!(
                    (cx - (x as f64 + 0.5)).abs() < 1e-6 && (cy - (y as f64 + 0.5)).abs() < 1e-6,
                    "{w}×{h}, {mips} mips, point ({x}, {y}): centroid ({cx}, {cy})"
                );
            }
        }
    }
}

/// A flat field is unchanged up to the border, at odd sizes and down to one-texel levels.
#[test]
fn the_bloom_model_keeps_a_flat_field_flat() {
    for (w, h, mips) in [(37, 21, 5), (64, 48, 5), (3, 1, 5)] {
        let mut flat = Image::new(w, h);
        flat.px.fill(3.0);
        let bloom = model_bloom(&flat, mips);
        let worst = bloom.px.iter().map(|v| (v - 3.0).abs()).fold(0.0, f64::max);
        assert!(worst < 1e-9, "{w}×{h}, {mips} mips: off by {worst}");
    }
}

fn gpu(tag: &str) -> Option<crate::RenderDevice> {
    match crate::RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip {tag}: {err}");
            None
        }
    }
}

/// IEEE half-float bits of `v` (round to nearest even; NaN, ±Inf and overflow kept).
fn f16_bits(v: f32) -> u16 {
    if v.is_nan() {
        return 0x7e00;
    }
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let a = v.abs();
    if a >= 65520.0 {
        return sign | 0x7c00;
    }
    if a < 6.103_515_6e-5 {
        return sign | (a * 16_777_216.0).round() as u16;
    }
    let e = ((b >> 23) & 0xff) - 112;
    let m = b & 0x7f_ffff;
    let mut h = (e << 10) | (m >> 13);
    let rest = m & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && h & 1 == 1) {
        h += 1;
    }
    sign | h as u16
}

fn f16_value(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = i32::from((h >> 10) & 0x1f);
    let m = f32::from(h & 0x3ff);
    match e {
        0 => sign * m * 2f32.powi(-24),
        31 if m == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

/// A sampled `Rgba16Float` texture holding `texels` (`w × h`, RGBA as `f32`, stored as halves).
fn rgba16f_texture(device: &crate::RenderDevice, w: u32, h: u32, texels: &[[f32; 4]]) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("post effects test input"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let halves: Vec<u16> = texels.iter().flat_map(|t| t.map(f16_bits)).collect();
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&halves),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 8),
            rows_per_image: Some(h),
        },
        size,
    );
    texture
}

/// Grey texels (`[v, v, v, 1]`) of `lum`.
fn grey(lum: &[f32]) -> Vec<[f32; 4]> {
    lum.iter().map(|&v| [v, v, v, 1.0]).collect()
}

/// The texels of a `w × h` `Rgba16Float` texture (NaN and Inf kept).
fn read_rgba(device: &crate::RenderDevice, texture: &wgpu::Texture, w: u32, h: u32) -> Vec<[f32; 4]> {
    let d = &device.device;
    let row = w * 8;
    let padded = row.div_ceil(256) * 256;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("post effects test readback"),
        size: u64::from(padded * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().expect("map").expect("map ok");
    let mapped = staging.slice(..).get_mapped_range().expect("range");
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let start = (y * padded) as usize;
        let halves: &[u16] = bytemuck::cast_slice(&mapped[start..start + row as usize]);
        for p in halves.as_chunks::<4>().0 {
            out.push([f16_value(p[0]), f16_value(p[1]), f16_value(p[2]), f16_value(p[3])]);
        }
    }
    out
}

/// Runs the post stack on the grey frame `lum` (`w × h`) with `mips` bloom levels at a blend of 1
/// and returns the red channel of the exposed frame (the bloom's input) and of the composite.
fn gpu_bloom(device: &crate::RenderDevice, w: u32, h: u32, mips: u32, lum: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let d = &device.device;
    let hdr = rgba16f_texture(device, w, h, &grey(lum));
    let hdr_view = hdr.create_view(&wgpu::TextureViewDescriptor::default());
    let out = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("post effects test output"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let settings = PostSettings {
        bloom: true,
        bloom_mips: mips,
        bloom_intensity: 1.0,
        ..PostSettings::golden_deterministic()
    };
    let mut chain = PostChain::default();
    let mut belt = crate::staging::StagingBelt::default();
    belt.begin_frame();
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    chain
        .record(
            d,
            &mut belt,
            &mut encoder,
            &hdr_view,
            w,
            h,
            &settings,
            &out_view,
            wgpu::TextureFormat::Rgba8Unorm,
            [0.0; 4],
            None,
            PostAa::default(),
        )
        .expect("post stack");
    belt.upload(&device.tracked_queue());
    device.queue.submit([encoder.finish()]);
    chain.map_after_submit();
    let state = chain.state.as_ref().expect("post state");
    let red = |tex: &Tex| -> Vec<f32> {
        read_rgba(device, tex.view.texture(), w, h)
            .iter()
            .map(|p| p[0])
            .collect()
    };
    (red(&state.exposed), red(&state.taa_composite))
}

/// The GPU chain is the model: per pixel within half-float precision (and the hardware's
/// sub-texel weight precision), and a highlight keeps its energy and centre on an odd-sized frame
/// at every position tried.
#[test]
fn gpu_bloom_matches_the_model_and_keeps_energy() {
    let Some(device) = gpu("gpu bloom energy") else {
        return;
    };
    let (w, h, mips) = (101u32, 75u32, 3u32);
    for (x, y) in [(50usize, 37usize), (51, 38), (47, 36), (53, 39)] {
        let input = point(w as usize, h as usize, x, y, 1000.0);
        let lum: Vec<f32> = input.px.iter().map(|&v| v as f32).collect();
        let (exposed, composite) = gpu_bloom(&device, w, h, mips, &lum);
        let exposed = Image::from_f32(w as usize, h as usize, &exposed);
        let composite = Image::from_f32(w as usize, h as usize, &composite);
        assert!(composite.px.iter().all(|v| v.is_finite()), "non-finite bloom");
        let model = model_bloom(&exposed, mips);
        let peak = model.px.iter().copied().fold(0.0, f64::max);
        for (i, (g, m)) in composite.px.iter().zip(&model.px).enumerate() {
            assert!(
                (g - m).abs() <= 1e-2 * (peak + m.abs()),
                "point ({x}, {y}): pixel ({}, {}) is {g}, the model {m}",
                i % w as usize,
                i / w as usize
            );
        }
        let energy = composite.sum() / exposed.sum();
        assert!(
            (energy - 1.0).abs() < 3e-3,
            "point ({x}, {y}): the bloom has ×{energy} the energy"
        );
        let (cx, cy) = composite.centroid();
        assert!(
            (cx - (x as f64 + 0.5)).abs() < 0.05 && (cy - (y as f64 + 0.5)).abs() < 0.05,
            "point ({x}, {y}): centroid ({cx}, {cy})"
        );
    }
}

/// A flat field through the GPU chain stays flat to the border (odd size, five levels).
#[test]
fn gpu_bloom_keeps_a_flat_field_flat() {
    let Some(device) = gpu("gpu bloom flat field") else {
        return;
    };
    let (w, h) = (37u32, 21u32);
    let (exposed, composite) = gpu_bloom(&device, w, h, 5, &vec![2.0; (w * h) as usize]);
    let level = exposed[0];
    for (i, v) in composite.iter().enumerate() {
        assert!(
            (v - level).abs() <= 2e-3 * level,
            "pixel ({}, {}) is {v}, the field {level}",
            i % w as usize,
            i / w as usize
        );
    }
}

/// A `w × h` `Depth16Unorm` depth texture holding the standard-Z values `raw` (0 near, 1 far).
fn depth_texture(device: &crate::RenderDevice, w: u32, h: u32, raw: &[f32]) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("post effects test depth"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth16Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texels: Vec<u16> = raw
        .iter()
        .map(|&d| (d.clamp(0.0, 1.0) * 65535.0).round() as u16)
        .collect();
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::DepthOnly,
        },
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 2),
            rows_per_image: Some(h),
        },
        size,
    );
    texture
}

/// A `w × h` target in the lens format (`Rgba16Float`), sampled, rendered and read back.
fn lens_target(device: &crate::RenderDevice, w: u32, h: u32) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("post effects test target"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn view_of(texture: &wgpu::Texture) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// Motion blur of `colour` (`w × h`) on a flat depth, each pixel moving `motion` pixels a frame.
fn gpu_motion_blur(
    device: &crate::RenderDevice,
    (w, h): (u32, u32),
    colour: &[[f32; 4]],
    motion: &[[f32; 2]],
    blur: MotionBlur,
    frame: u64,
) -> Vec<[f32; 4]> {
    let src = rgba16f_texture(device, w, h, colour);
    // The velocity target holds uv offsets.
    let uv: Vec<[f32; 4]> = motion
        .iter()
        .map(|m| [m[0] / w as f32, m[1] / h as f32, 0.0, 0.0])
        .collect();
    let velocity = rgba16f_texture(device, w, h, &uv);
    let depth = depth_texture(device, w, h, &vec![0.5; (w * h) as usize]);
    let dest = lens_target(device, w, h);
    let tracked = device.tracked();
    let mut pass = MotionBlurPass::default();
    let mut belt = crate::staging::StagingBelt::default();
    belt.begin_frame();
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    pass.record(
        &tracked,
        &mut belt,
        &mut encoder,
        &view_of(&src),
        &view_of(&velocity),
        &view_of(&depth),
        &view_of(&dest),
        &blur,
        frame,
    )
    .expect("motion blur");
    belt.upload(&device.tracked_queue());
    device.queue.submit([encoder.finish()]);
    read_rgba(device, &dest, w, h)
}

/// Red channel of row `y` of a `w`-wide image.
fn row(px: &[[f32; 4]], w: u32, y: u32) -> Vec<f32> {
    px[(y * w) as usize..((y + 1) * w) as usize]
        .iter()
        .map(|p| p[0])
        .collect()
}

/// Grey texels of a black `w × h` frame with every column in `bar` at `value`.
fn vertical_bar(w: u32, h: u32, bar: std::ops::Range<u32>, value: f32) -> Vec<[f32; 4]> {
    (0..w * h)
        .map(|i| {
            let v = if bar.contains(&(i % w)) { value } else { 0.0 };
            [v, v, v, 1.0]
        })
        .collect()
}

/// An object 25 frame widths faster than the frame (a teleport, a hitch frame) blurs over the
/// capped length around itself. Before ADR-1152 its taps all fell off screen or on unrelated pixels:
/// some rows kept the bar sharp, others smeared it over 60 px.
#[test]
fn motion_blur_caps_the_blur_of_a_very_fast_object() {
    let Some(device) = gpu("motion blur cap") else {
        return;
    };
    let (w, h) = (256u32, 128u32);
    let colour = vertical_bar(w, h, 126..130, 100.0);
    let motion = vec![[6400.0, 0.0]; (w * h) as usize];
    let blur = MotionBlur {
        shutter_angle: 0.5,
        samples: 1,
    };
    // The bar's centre is x = 128; the blur spans the cap (16 px) plus the bar and a filter texel.
    let half = 0.5 * MotionBlur::MAX_BLUR_FRACTION * h as f32 + 4.0;
    for frame in [0, 5] {
        let out = gpu_motion_blur(&device, (w, h), &colour, &motion, blur, frame);
        for y in [0, 64, 127] {
            let r = row(&out, w, y);
            let lit: Vec<usize> = (0..w as usize).filter(|&x| r[x] > 1e-3).collect();
            let (first, last) = (
                *lit.first().expect("the bar is visible") as f32,
                *lit.last().expect("the bar is visible") as f32,
            );
            assert!(
                first >= 128.0 - half && last <= 128.0 + half,
                "frame {frame} row {y}: lit from {first} to {last}"
            );
            assert!(
                last - first >= 11.0,
                "frame {frame} row {y}: not blurred ({first}..{last})"
            );
            let energy = r.iter().sum::<f32>() / 400.0;
            assert!(
                (0.8..1.2).contains(&energy),
                "frame {frame} row {y}: ×{energy} of the bar's energy"
            );
        }
    }
}

/// A 40 px blur asked for with one sample per direction is sampled every few pixels: the trail is
/// continuous, not two or three ghost copies of the bar.
#[test]
fn a_long_blur_is_sampled_densely() {
    let Some(device) = gpu("motion blur density") else {
        return;
    };
    let (w, h) = (384u32, 384u32);
    let colour = vertical_bar(w, h, 190..194, 100.0);
    let motion = vec![[80.0, 0.0]; (w * h) as usize];
    let blur = MotionBlur {
        shutter_angle: 0.5,
        samples: 1,
    };
    for frame in [0, 1, 7, 63] {
        let out = gpu_motion_blur(&device, (w, h), &colour, &motion, blur, frame);
        for y in [0, 37, 200, 383] {
            let r = row(&out, w, y);
            let covered = r[176..208].iter().filter(|&&v| v > 1e-3).count();
            assert!(
                covered >= 29,
                "frame {frame} row {y}: {covered} of the 32 trail pixels lit"
            );
            let energy = r.iter().sum::<f32>() / 400.0;
            assert!(
                (0.9..1.1).contains(&energy),
                "frame {frame} row {y}: ×{energy} of the bar's energy"
            );
        }
    }
}

/// A NaN or infinite colour texel is skipped by the taps that reach it, and a NaN motion vector
/// reads as no motion: nothing but the broken texels themselves can be off.
#[test]
fn non_finite_motion_and_colour_stay_local() {
    let Some(device) = gpu("motion blur non-finite") else {
        return;
    };
    let (w, h) = (64u32, 32u32);
    let mut colour = grey(&vec![1.0; (w * h) as usize]);
    let nan_at = (16 * w + 20) as usize;
    let inf_at = (16 * w + 40) as usize;
    colour[nan_at] = [f32::NAN, f32::NAN, f32::NAN, 1.0];
    colour[inf_at] = [f32::INFINITY, f32::INFINITY, f32::INFINITY, 1.0];
    let mut motion = vec![[16.0, 0.0]; (w * h) as usize];
    motion[(8 * w + 30) as usize] = [f32::NAN, f32::NAN];
    let blur = MotionBlur {
        shutter_angle: 0.5,
        samples: 2,
    };
    let out = gpu_motion_blur(&device, (w, h), &colour, &motion, blur, 0);
    for (i, p) in out.iter().enumerate() {
        if i == nan_at || i == inf_at {
            continue;
        }
        assert!(
            (p[0] - 1.0).abs() <= 1e-2,
            "pixel ({}, {}) is {}",
            i % w as usize,
            i / w as usize,
            p[0]
        );
    }
}

/// Standard-Z depth of a surface `z` metres away (D3D-style perspective; `far` may be infinite).
fn raw_depth(z: f32, near: f32, far: f32) -> f32 {
    let near_over_far = if far.is_finite() { near / far } else { 0.0 };
    (1.0 - near / z) / (1.0 - near_over_far)
}

/// Depth of field of `colour` (`w × h`) over the standard-Z depth `raw`, for a camera of vertical
/// field of view, near and far `projection`.
fn gpu_dof(
    device: &crate::RenderDevice,
    (w, h): (u32, u32),
    colour: &[[f32; 4]],
    raw: &[f32],
    dof: &DepthOfField,
    projection: (f32, f32, f32),
) -> Vec<[f32; 4]> {
    let src = rgba16f_texture(device, w, h, colour);
    let depth = depth_texture(device, w, h, raw);
    let scratch = [lens_target(device, w, h), lens_target(device, w, h)];
    let dest = lens_target(device, w, h);
    let scratch_views = [view_of(&scratch[0]), view_of(&scratch[1])];
    let tracked = device.tracked();
    let mut pass = DepthOfFieldPass::default();
    let mut belt = crate::staging::StagingBelt::default();
    belt.begin_frame();
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    pass.record(
        &tracked,
        &mut belt,
        &mut encoder,
        &view_of(&src),
        (w, h),
        &view_of(&depth),
        [&scratch_views[0], &scratch_views[1]],
        &view_of(&dest),
        dof,
        projection,
    )
    .expect("depth of field");
    belt.upload(&device.tracked_queue());
    device.queue.submit([encoder.finish()]);
    read_rgba(device, &dest, w, h)
}

/// A lens for the small test frames: a very wide aperture, so a 48–64 px tall frame blurs by tens
/// of pixels.
fn wide_lens(
    mode: DepthOfFieldMode,
    focal_distance: f32,
    aperture_f_stops: f32,
    max_coc: f32,
) -> DepthOfField {
    DepthOfField {
        mode,
        focal_distance,
        sensor_height: 0.024,
        aperture_f_stops,
        max_circle_of_confusion_diameter: max_coc,
        max_depth: f32::INFINITY,
    }
}

/// A blurred foreground with a 64 px circle spreads its full radius over the sharp background
/// behind its silhouette. The tile max was dilated by one 16 px tile, so background pixels 17–24 px
/// from a silhouette that ends a tile, inside the foreground's reach, gathered nothing.
#[test]
fn large_circles_of_confusion_reach_their_full_radius() {
    let Some(device) = gpu("dof tile reach") else {
        return;
    };
    let (w, h) = (128u32, 64u32);
    let (near, far) = (0.1, 100.0);
    // Foreground (bright, 1 m) over columns 0–47, the end of tile 2; background (black, 10 m, in
    // focus) behind.
    let colour = vertical_bar(w, h, 0..48, 100.0);
    let raw: Vec<f32> = (0..w * h)
        .map(|i| raw_depth(if i % w < 48 { 1.0 } else { 10.0 }, near, far))
        .collect();
    for mode in [DepthOfFieldMode::Gaussian, DepthOfFieldMode::Bokeh] {
        let dof = wide_lens(mode, 10.0, 0.05, 64.0);
        let out = gpu_dof(&device, (w, h), &colour, &raw, &dof, (0.6, near, far));
        let mut band = 0.0;
        for y in 0..h {
            band += row(&out, w, y)[64..72].iter().sum::<f32>();
        }
        band /= (8 * h) as f32;
        assert!(
            band > 1.0,
            "{mode:?}: background 17–24 px from the silhouette gathered {band}"
        );
        // Far from the silhouette the background stays sharp (and black).
        for y in 0..h {
            let r = row(&out, w, y);
            assert!(
                r[100..].iter().all(|&v| v.abs() < 1e-6),
                "{mode:?}: row {y} far background changed"
            );
        }
    }
}

/// A NaN or infinite texel in an out-of-focus region is skipped by every tap that reaches it,
/// instead of turning its whole circle into NaN (which the exposure pass prints black).
#[test]
fn a_non_finite_texel_does_not_spread_over_its_circle() {
    let Some(device) = gpu("dof non-finite") else {
        return;
    };
    let (w, h) = (64u32, 48u32);
    let (near, far) = (0.1, 100.0);
    let mut colour = grey(&vec![1.0; (w * h) as usize]);
    colour[(24 * w + 32) as usize] = [f32::NAN, f32::NAN, f32::NAN, 1.0];
    colour[(10 * w + 10) as usize] = [f32::INFINITY, f32::INFINITY, f32::INFINITY, 1.0];
    let raw = vec![raw_depth(1.0, near, far); (w * h) as usize];
    for mode in [DepthOfFieldMode::Gaussian, DepthOfFieldMode::Bokeh] {
        let dof = wide_lens(mode, 10.0, 0.05, 16.0);
        let out = gpu_dof(&device, (w, h), &colour, &raw, &dof, (0.6, near, far));
        for (i, p) in out.iter().enumerate() {
            assert!(
                (p[0] - 1.0).abs() <= 1e-2,
                "{mode:?}: pixel ({}, {}) is {}",
                i % w as usize,
                i / w as usize,
                p[0]
            );
        }
    }
}

/// With an infinite far plane (`Camera::far`), depth reads as `near / (1 − raw)`: a surface 2 m
/// away is sharp when focused at 2 m and blurred when focused at 20 m. The shader divided
/// infinity by infinity and put every surface at the carried-depth cap.
#[test]
fn an_infinite_far_plane_keeps_the_focus() {
    let Some(device) = gpu("dof infinite far") else {
        return;
    };
    let (w, h) = (64u32, 48u32);
    let near = 0.1;
    let stripes: Vec<f32> = (0..w * h).map(|i| (i % w % 2) as f32).collect();
    let colour = grey(&stripes);
    let raw = vec![raw_depth(2.0, near, f32::INFINITY); (w * h) as usize];
    for mode in [DepthOfFieldMode::Gaussian, DepthOfFieldMode::Bokeh] {
        let projection = (0.6, near, f32::INFINITY);
        let sharp = gpu_dof(
            &device,
            (w, h),
            &colour,
            &raw,
            &wide_lens(mode, 2.0, 0.1, 16.0),
            projection,
        );
        for (i, (p, s)) in sharp.iter().zip(&stripes).enumerate() {
            assert!(
                (p[0] - s).abs() <= 1e-3,
                "{mode:?} focused at 2 m: pixel ({}, {}) is {}, was {s}",
                i % w as usize,
                i / w as usize,
                p[0]
            );
        }
        let soft = gpu_dof(
            &device,
            (w, h),
            &colour,
            &raw,
            &wide_lens(mode, 20.0, 0.1, 16.0),
            projection,
        );
        let mut inner = Vec::new();
        for y in 8..h - 8 {
            inner.extend_from_slice(&row(&soft, w, y)[8..(w - 8) as usize]);
        }
        let mean = inner.iter().sum::<f32>() / inner.len() as f32;
        let deviation = (inner.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / inner.len() as f32).sqrt();
        assert!(
            deviation < 0.15,
            "{mode:?} focused at 20 m: stripes kept a deviation of {deviation}"
        );
    }
}
