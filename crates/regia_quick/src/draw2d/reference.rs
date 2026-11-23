// SPDX-License-Identifier: MPL-2.0
//! A CPU reference rasterization of a [`Draw2dList`]: the same per-pixel coverage as
//! `draw2d.wgsl`, evaluated at pixel centres, blended premultiplied-over into an RGBA8 image the
//! way the GPU blends into an 8-bit target (decode, blend, encode and round once per instance).
//!
//! It is the oracle of the layer's tests and a software fallback for tools (thumbnails, golden
//! review). It is not fast: a few million pixel evaluations per HUD tile.

use regia_render::{ImmediateImage, ImmediateImageFormat};
use regia_ui::atlas::PAGE_SIZE;

use super::{Draw2dList, IMAGE_SLOTS, kind};

/// sRGB-encoded → linear (IEC 61966-2-1).
fn decode(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear → sRGB-encoded (IEC 61966-2-1).
fn encode(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

fn quantize(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A texture the reference samples: RGBA8 or R8 texels, `srgb` when its colour channels are
/// sRGB-encoded (decoded before filtering, as an `*Srgb` texture view does).
struct Texels<'a> {
    data: &'a [u8],
    width: u32,
    height: u32,
    channels: usize,
    srgb: bool,
}

impl Texels<'_> {
    fn texel(&self, x: i64, y: i64) -> [f32; 4] {
        let x = x.clamp(0, i64::from(self.width) - 1) as usize;
        let y = y.clamp(0, i64::from(self.height) - 1) as usize;
        let i = (y * self.width as usize + x) * self.channels;
        let v = |c: usize| f32::from(self.data.get(i + c).copied().unwrap_or(0)) / 255.0;
        if self.channels == 1 {
            let r = v(0);
            [r, 0.0, 0.0, 1.0]
        } else if self.srgb {
            [decode(v(0)), decode(v(1)), decode(v(2)), v(3)]
        } else {
            [v(0), v(1), v(2), v(3)]
        }
    }

    /// Bilinear sample at normalized `(u, v)`, clamped to the edge (the layer's sampler).
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        let tx = u * self.width as f32 - 0.5;
        let ty = v * self.height as f32 - 0.5;
        let (x0, y0) = (tx.floor(), ty.floor());
        let (fx, fy) = (tx - x0, ty - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let a = self.texel(x0, y0);
        let b = self.texel(x0 + 1, y0);
        let c = self.texel(x0, y0 + 1);
        let d = self.texel(x0 + 1, y0 + 1);
        std::array::from_fn(|k| {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bottom = c[k] + (d[k] - c[k]) * fx;
            top + (bottom - top) * fy
        })
    }
}

/// Signed distance from `p` to a box `geo` with corners rounded by `radius` (negative inside).
fn box_distance(p: [f32; 2], geo: [f32; 4], radius: f32) -> f32 {
    let centre = [(geo[0] + geo[2]) * 0.5, (geo[1] + geo[3]) * 0.5];
    let half = [(geo[2] - geo[0]).abs() * 0.5, (geo[3] - geo[1]).abs() * 0.5];
    let r = radius.min(half[0].min(half[1]));
    let q = [
        (p[0] - centre[0]).abs() - half[0] + r,
        (p[1] - centre[1]).abs() - half[1] + r,
    ];
    let outside = (q[0].max(0.0).powi(2) + q[1].max(0.0).powi(2)).sqrt();
    outside + q[0].max(q[1]).min(0.0) - r
}

/// Pixel bounds `[x0, y0, x1, y1)` an instance's quad can touch.
fn bounds(geo: [f32; 4], params: [f32; 4]) -> [f32; 4] {
    let k = params[0];
    if k == kind::SEGMENT {
        let m = params[1] + 1.0;
        [
            geo[0].min(geo[2]) - m,
            geo[1].min(geo[3]) - m,
            geo[0].max(geo[2]) + m,
            geo[1].max(geo[3]) + m,
        ]
    } else if k == kind::BOX || k == kind::OUTLINE {
        let m = if k == kind::BOX {
            params[2].max(1.0) * 0.5 + 1.0
        } else {
            1.0
        };
        [geo[0] - m, geo[1] - m, geo[2] + m, geo[3] + m]
    } else {
        geo
    }
}

/// Rasterizes `list` over `target` (`width` × `height` RGBA8, row-major, top row first), which
/// holds what the frame showed before. `images[slot]` are the images the list's image instances
/// sample (a missing slot samples transparent texels). `srgb_target`: the target stores
/// sRGB-encoded colour (an `*Srgb` format, blended in linear); otherwise it is a plain unorm
/// target receiving sRGB-encoded premultiplied colour (blended as stored).
pub fn rasterize(
    list: &Draw2dList,
    images: &[Option<&ImmediateImage>],
    target: &mut [u8],
    width: u32,
    height: u32,
    srgb_target: bool,
) {
    let atlas = list.atlas();
    let page = |kind: u8, page: u32| {
        atlas.page_data(kind, page).map(|(data, bpp)| Texels {
            data,
            width: PAGE_SIZE,
            height: PAGE_SIZE,
            channels: bpp,
            srgb: bpp == 4,
        })
    };
    let image = |slot: usize| {
        images.get(slot).copied().flatten().map(|img| Texels {
            data: &img.data,
            width: img.width.max(1),
            height: img.height.max(1),
            channels: if img.format == ImmediateImageFormat::R8 {
                1
            } else {
                4
            },
            srgb: img.format != ImmediateImageFormat::R8,
        })
    };
    for inst in list.instances() {
        let [geo, uv, color, clip, params] = [inst.geo, inst.uv, inst.color, inst.clip, inst.params];
        let k = params[0];
        let b = bounds(geo, params);
        let x0 = b[0].floor().max(0.0) as u32;
        let y0 = b[1].floor().max(0.0) as u32;
        let x1 = (b[2].ceil().max(0.0) as u32).min(width);
        let y1 = (b[3].ceil().max(0.0) as u32).min(height);
        let textured = k == kind::GLYPH || k == kind::COLOR_GLYPH || k == kind::IMAGE;
        let tex = if k == kind::GLYPH {
            page(0, params[2] as u32)
        } else if k == kind::COLOR_GLYPH {
            page(1, params[2] as u32)
        } else if k == kind::IMAGE {
            let slot = params[3] as usize;
            if slot < IMAGE_SLOTS { image(slot) } else { None }
        } else {
            None
        };
        for y in y0..y1 {
            for x in x0..x1 {
                let p = [x as f32 + 0.5, y as f32 + 0.5];
                if p[0] < clip[0] || p[1] < clip[1] || p[0] >= clip[2] || p[1] >= clip[3] {
                    continue;
                }
                // Textured quads cover the pixels whose centres lie inside them.
                if textured && (p[0] < geo[0] || p[1] < geo[1] || p[0] >= geo[2] || p[1] >= geo[3]) {
                    continue;
                }
                let src = shade(k, p, geo, uv, color, params, tex.as_ref());
                if src[3] <= 0.0 && src[0] <= 0.0 && src[1] <= 0.0 && src[2] <= 0.0 {
                    continue;
                }
                let i = ((y * width + x) * 4) as usize;
                let Some(dst) = target.get_mut(i..i + 4) else {
                    continue;
                };
                blend(dst, src, srgb_target);
            }
        }
    }
}

/// Premultiplied linear source colour of instance kind `k` at pixel centre `p`.
fn shade(
    k: f32,
    p: [f32; 2],
    geo: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    params: [f32; 4],
    tex: Option<&Texels<'_>>,
) -> [f32; 4] {
    let scale = |c: f32| color.map(|v| v * c);
    if k == kind::BOX {
        let cov = (0.5 - box_distance(p, geo, params[1]) / params[2].max(1.0)).clamp(0.0, 1.0);
        if params[3] > 0.5 {
            let t = if params[3] > 1.5 {
                (p[0] - geo[0]) / (geo[2] - geo[0]).max(0.0001)
            } else {
                (p[1] - geo[1]) / (geo[3] - geo[1]).max(0.0001)
            }
            .clamp(0.0, 1.0);
            return std::array::from_fn(|c| (color[c] + (uv[c] - color[c]) * t) * cov);
        }
        return scale(cov);
    }
    if k == kind::OUTLINE {
        let d = box_distance(p, geo, params[1]);
        return scale((0.5 - d).clamp(0.0, 1.0) - (0.5 - (d + params[2])).clamp(0.0, 1.0));
    }
    if k == kind::SEGMENT {
        let (a, b) = ([geo[0], geo[1]], [geo[2], geo[3]]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = (dx * dx + dy * dy).sqrt().max(0.0001);
        let dir = [dx / len, dy / len];
        let rel = [p[0] - a[0], p[1] - a[1]];
        let along = rel[0] * dir[0] + rel[1] * dir[1];
        let across = rel[0] * -dir[1] + rel[1] * dir[0];
        let hw = params[1];
        let cov = if params[2] > 0.5 {
            let t = along.clamp(0.0, len);
            (hw - ((along - t).powi(2) + across * across).sqrt() + 0.5).clamp(0.0, 1.0)
        } else {
            let side = (hw - across.abs() + 0.5).clamp(0.0, 1.0);
            let ends = (along + 0.5).clamp(0.0, 1.0) * (len - along + 0.5).clamp(0.0, 1.0);
            side * ends
        };
        return scale(cov);
    }
    // Textured: the uv interpolated linearly across the quad.
    let fx = (p[0] - geo[0]) / (geo[2] - geo[0]).max(1e-6);
    let fy = (p[1] - geo[1]) / (geo[3] - geo[1]).max(1e-6);
    let u = uv[0] + (uv[2] - uv[0]) * fx;
    let v = uv[1] + (uv[3] - uv[1]) * fy;
    let t = tex.map_or([0.0; 4], |t| t.sample(u, v));
    if k == kind::GLYPH {
        scale(t[0])
    } else if k == kind::COLOR_GLYPH {
        t.map(|c| c * color[3])
    } else {
        [t[0] * color[0], t[1] * color[1], t[2] * color[2], t[3] * color[3]]
    }
}

/// Blends premultiplied linear `src` over the stored pixel `dst`.
fn blend(dst: &mut [u8], src: [f32; 4], srgb_target: bool) {
    let d = |c: usize| f32::from(dst[c]) / 255.0;
    let inv = 1.0 - src[3];
    if srgb_target {
        let out: [f32; 4] = std::array::from_fn(|c| {
            if c == 3 {
                src[3] + d(3) * inv
            } else {
                encode(src[c] + decode(d(c)) * inv)
            }
        });
        for (stored, value) in dst.iter_mut().zip(out) {
            *stored = quantize(value);
        }
    } else {
        // The shader writes sRGB-encoded premultiplied colour to a unorm target.
        let enc = if src[3] > 0.0 {
            let a = src[3];
            [
                encode((src[0] / a).clamp(0.0, 1.0)) * a,
                encode((src[1] / a).clamp(0.0, 1.0)) * a,
                encode((src[2] / a).clamp(0.0, 1.0)) * a,
                a,
            ]
        } else {
            src
        };
        let out: [f32; 4] = std::array::from_fn(|c| enc[c] + d(c) * inv);
        for (stored, value) in dst.iter_mut().zip(out) {
            *stored = quantize(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw2d::PxRect;
    use regia_core::math::{Color, Vec2};

    #[test]
    fn an_aligned_opaque_box_is_crisp_and_a_rounded_corner_is_soft() {
        let mut list = Draw2dList::new();
        list.begin_frame();
        list.push_box(PxRect::new(2.0, 2.0, 8.0, 8.0), Color::WHITE, 0.0);
        list.push_box(PxRect::new(12.0, 2.0, 8.0, 8.0), Color::WHITE, 4.0);
        list.end_frame();
        let (w, h) = (24, 12);
        let mut px = vec![0u8; w * h * 4];
        rasterize(&list, &[], &mut px, w as u32, h as u32, false);
        let a = |x: usize, y: usize| px[(y * w + x) * 4 + 3];
        assert_eq!(a(2, 2), 255);
        assert_eq!(a(9, 9), 255);
        assert_eq!(a(1, 5), 0);
        assert_eq!(a(10, 5), 0);
        // The disc's corner pixel is partly covered.
        let corner = a(12, 2);
        assert!(corner < 255, "{corner}");
        assert_eq!(a(16, 6), 255);
    }

    #[test]
    fn a_soft_box_fades_and_a_gradient_runs_edge_to_edge() {
        let mut list = Draw2dList::new();
        list.begin_frame();
        list.push_soft_box(PxRect::new(10.0, 10.0, 20.0, 20.0), Color::WHITE, 0.0, 8.0);
        list.push_gradient_box(
            PxRect::new(40.0, 0.0, 10.0, 40.0),
            Color::BLACK,
            Color::WHITE,
            0.0,
            false,
        );
        list.end_frame();
        let (w, h) = (64, 40);
        let mut px = vec![0u8; w * h * 4];
        rasterize(&list, &[], &mut px, w as u32, h as u32, false);
        let a = |x: usize, y: usize| px[(y * w + x) * 4 + 3];
        // Feather 8 centred on the edge at x = 10: pixel centre 10.5 is 0.5 px inside, 0.5625.
        assert_eq!(a(10, 20), 143);
        assert_eq!(a(20, 20), 255);
        assert!(a(7, 20) > 0 && a(7, 20) < a(10, 20));
        assert_eq!(a(4, 20), 0);
        // The gradient is black at the top and white at the bottom.
        let r = |y: usize| px[(y * w + 45) * 4];
        assert!(
            r(0) < 40 && r(39) > 240 && r(20) > r(10),
            "{} {} {}",
            r(0),
            r(10),
            r(39)
        );
    }

    #[test]
    fn segment_coverage_falls_off_across_the_edge() {
        let mut list = Draw2dList::new();
        list.begin_frame();
        list.push_segment(Vec2::new(0.0, 5.25), Vec2::new(20.0, 5.25), 2.0, Color::WHITE);
        list.end_frame();
        let (w, h) = (20, 10);
        let mut px = vec![0u8; w * h * 4];
        rasterize(&list, &[], &mut px, w as u32, h as u32, false);
        let column: Vec<u8> = (0..h).map(|y| px[(y * w + 10) * 4 + 3]).collect();
        // Centre line 5.25, half width 1: pixel centres 4.5, 5.5 and 6.5 are 0.75, 0.25 and 1.25
        // away, so coverage 0.75, 1 and 0.25.
        assert_eq!(column[3], 0);
        assert_eq!(column[4], 191);
        assert_eq!(column[5], 255);
        assert_eq!(column[6], 64);
        assert_eq!(column[7], 0);
    }
}
