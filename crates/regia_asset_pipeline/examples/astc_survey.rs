// SPDX-License-Identifier: MPL-2.0
//! ASTC block-size survey: picks the Android cook's block per texture usage by measurement
//! (`docs/specs/12-assets.md` § 7.1).
//!
//! `cargo run --release -p regia_asset_pipeline --example astc_survey -- [--half] <png>...`
//!
//! Each PNG's usage comes from its file name (`albedo` → sRGB colour, `normal` → normal map,
//! `orm`, `height`, `mask`, `ao` → linear data; the survey does not model the cook's one-channel
//! choice). Every image is encoded at ASTC
//! 4×4, 5×5, 6×6 and 8×8 (astcenc `fast` and `medium`) and at the desktop BC format of its
//! usage, decoded again, and compared with the source: PSNR (dB) over the used channels, and
//! for normal maps the mean and 99th-percentile angle error (degrees). `--half` measures at
//! half size, the level a mobile tier samples first (`layer_mip_skip_for`). Sizes that are not
//! a multiple of the block are cropped to whole blocks, so the numbers are encoder error only.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ctt_astcenc::Preset;
use regia_asset::bcn_decode::{BcnKind, decode_bcn};
use regia_asset_pipeline::astc::{AstcContent, compress_astc_rgba8_preset, decompress_astc_rgba8};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Usage {
    Colour,
    Linear,
    Normal,
    Single,
}

fn usage_of(path: &Path) -> Usage {
    let name = path.to_string_lossy().to_ascii_lowercase();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if stem.contains("normal") {
        Usage::Normal
    } else if stem.contains("orm") {
        Usage::Linear
    } else if stem.contains("height") || stem.contains("dlh") || stem.contains("mask") || name.contains("_ao")
    {
        Usage::Single
    } else {
        Usage::Colour
    }
}

struct Row {
    file: String,
    usage: Usage,
    codec: String,
    bpp: f32,
    score: f64,
    p99: f64,
    ms: f64,
}

fn crop(w: u32, h: u32, rgba: &[u8], cw: u32, ch: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in 0..ch {
        let at = (y * w * 4) as usize;
        out.extend_from_slice(&rgba[at..at + (cw * 4) as usize]);
    }
    let _ = h;
    out
}

fn halve(w: u32, h: u32, rgba: &[u8]) -> (u32, u32, Vec<u8>) {
    let (nw, nh) = (w / 2, h / 2);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4 {
                let s: u32 = [(0, 0), (1, 0), (0, 1), (1, 1)]
                    .iter()
                    .map(|(dx, dy)| u32::from(rgba[(((2 * y + dy) * w + 2 * x + dx) * 4 + c) as usize]))
                    .sum();
                out[((y * nw + x) * 4 + c) as usize] = ((s + 2) / 4) as u8;
            }
        }
    }
    (nw, nh, out)
}

/// PSNR over `channels` of two RGBA8 buffers.
fn psnr(a: &[u8], b: &[u8], channels: &[usize]) -> f64 {
    let mut se = 0f64;
    let mut n = 0f64;
    for (pa, pb) in a.chunks(4).zip(b.chunks(4)) {
        for &c in channels {
            let d = f64::from(pa[c]) - f64::from(pb[c]);
            se += d * d;
            n += 1.0;
        }
    }
    if se == 0.0 {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / (se / n)).log10()
}

/// Mean and 99th-percentile angle (degrees) between the source normals and decoded X/Y with Z
/// rebuilt; `xy` picks the decoded X and Y channels.
fn normal_error(src: &[u8], dec: &[u8], xy: (usize, usize)) -> (f64, f64) {
    let unit = |x: u8, y: u8| {
        let x = f64::from(x) / 127.5 - 1.0;
        let y = f64::from(y) / 127.5 - 1.0;
        let z = (1.0 - x * x - y * y).max(0.0).sqrt();
        let l = (x * x + y * y + z * z).sqrt().max(1e-9);
        (x / l, y / l, z / l)
    };
    let mut angles: Vec<f64> = src
        .chunks(4)
        .zip(dec.chunks(4))
        .map(|(s, d)| {
            let a = unit(s[0], s[1]);
            let b = unit(d[xy.0], d[xy.1]);
            (a.0 * b.0 + a.1 * b.1 + a.2 * b.2)
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees()
        })
        .collect();
    let mean = angles.iter().sum::<f64>() / angles.len().max(1) as f64;
    angles.sort_by(f64::total_cmp);
    let p99 = angles.get(angles.len() * 99 / 100).copied().unwrap_or(0.0);
    (mean, p99)
}

fn has_alpha(rgba: &[u8]) -> bool {
    rgba.chunks(4).any(|p| p[3] < 255)
}

fn score(usage: Usage, src: &[u8], dec: &[u8], alpha: bool, normal_xy: (usize, usize)) -> (f64, f64) {
    match usage {
        Usage::Normal => normal_error(src, dec, normal_xy),
        Usage::Single => (psnr(src, dec, &[0]), 0.0),
        Usage::Colour | Usage::Linear => {
            let ch: &[usize] = if alpha { &[0, 1, 2, 3] } else { &[0, 1, 2] };
            (psnr(src, dec, ch), 0.0)
        }
    }
}

fn bc_reference(file: &str, usage: Usage, w: u32, h: u32, rgba: &[u8]) -> Row {
    use intel_tex_2::{RSurface, RgSurface, RgbaSurface, bc4, bc5, bc7};
    let alpha = has_alpha(rgba);
    let t = Instant::now();
    let (kind, codec, bpp, blocks) = match usage {
        Usage::Normal => {
            let rg: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1]]).collect();
            let s = RgSurface {
                width: w,
                height: h,
                stride: w * 2,
                data: &rg,
            };
            (BcnKind::Bc5, "BC5", 8.0, bc5::compress_blocks(&s))
        }
        Usage::Single => {
            let r: Vec<u8> = rgba.chunks(4).map(|p| p[0]).collect();
            let s = RSurface {
                width: w,
                height: h,
                stride: w,
                data: &r,
            };
            (BcnKind::Bc4, "BC4", 4.0, bc4::compress_blocks(&s))
        }
        Usage::Colour | Usage::Linear => {
            let settings = if alpha {
                bc7::alpha_fast_settings()
            } else {
                bc7::opaque_fast_settings()
            };
            let s = RgbaSurface {
                width: w,
                height: h,
                stride: w * 4,
                data: rgba,
            };
            (BcnKind::Bc7, "BC7", 8.0, bc7::compress_blocks(&settings, &s))
        }
    };
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let dec = decode_bcn(kind, w, h, &blocks).unwrap_or_default();
    let (s, p99) = score(usage, rgba, &dec, alpha, (0, 1));
    Row {
        file: file.to_string(),
        usage,
        codec: codec.to_string(),
        bpp,
        score: s,
        p99,
        ms,
    }
}

fn survey(path: &Path, half: bool) -> Vec<Row> {
    let Ok(img) = image::open(path) else {
        eprintln!("skip {}: not an image", path.display());
        return Vec::new();
    };
    let img = img.to_rgba8();
    let (mut w, mut h) = img.dimensions();
    let mut rgba = img.into_raw();
    if half {
        (w, h, rgba) = halve(w, h, &rgba);
    }
    let usage = usage_of(path);
    let file = path
        .iter()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<PathBuf>()
        .display()
        .to_string();
    let alpha = has_alpha(&rgba);
    let mut rows = vec![bc_reference(&file, usage, w, h, &rgba)];
    let content = match usage {
        Usage::Normal => AstcContent::Normal,
        Usage::Single => AstcContent::Single,
        _ if alpha => AstcContent::Rgba,
        _ => AstcContent::Rgb,
    };
    let srgb = usage == Usage::Colour;
    for b in [4u32, 5, 6, 8] {
        let (cw, ch) = (w / b * b, h / b * b);
        let src = crop(w, h, &rgba, cw, ch);
        let variants: Vec<(&str, Preset, AstcContent)> = if usage == Usage::Normal {
            vec![
                ("medium", Preset::Medium, AstcContent::Normal),
                ("medium rg", Preset::Medium, AstcContent::Rg),
                ("thorough rg", Preset::Thorough, AstcContent::Rg),
            ]
        } else {
            vec![
                ("fast", Preset::Fast, content),
                ("medium", Preset::Medium, content),
            ]
        };
        for (pname, preset, content) in variants {
            let t = Instant::now();
            let Ok(blocks) = compress_astc_rgba8_preset(cw, ch, &src, (b, b), srgb, content, preset) else {
                eprintln!("{file}: astc {b}x{b} failed");
                continue;
            };
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let Ok(dec) = decompress_astc_rgba8(cw, ch, &blocks, (b, b), srgb) else {
                continue;
            };
            let xy = if content == AstcContent::Normal {
                (0, 3)
            } else {
                (0, 1)
            };
            let (s, p99) = score(usage, &src, &dec, alpha, xy);
            rows.push(Row {
                file: file.clone(),
                usage,
                codec: format!("ASTC {b}x{b} {pname}"),
                bpp: 128.0 / (b * b) as f32,
                score: s,
                p99,
                ms,
            });
        }
    }
    rows
}

fn main() {
    let mut half = false;
    let mut files = Vec::new();
    for a in std::env::args().skip(1) {
        if a == "--half" {
            half = true;
        } else {
            files.push(PathBuf::from(a));
        }
    }
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    let chunk = files.len().div_ceil(threads).max(1);
    let rows: Vec<Row> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|c| s.spawn(move || c.iter().flat_map(|f| survey(f, half)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    println!("| file | usage | codec | bpp | PSNR dB / mean angle deg | p99 angle deg | encode ms |");
    println!("|---|---|---|---|---|---|---|");
    for r in &rows {
        println!(
            "| {} | {:?} | {} | {:.2} | {:.2} | {:.2} | {:.0} |",
            r.file, r.usage, r.codec, r.bpp, r.score, r.p99, r.ms
        );
    }
    // Per usage and codec: the mean score (and the worst texture).
    println!("\n| usage | codec | bpp | mean | worst | mean p99 angle |");
    println!("|---|---|---|---|---|---|");
    let mut keys: Vec<(String, String)> = rows
        .iter()
        .map(|r| (format!("{:?}", r.usage), r.codec.clone()))
        .collect();
    keys.sort();
    keys.dedup();
    for (u, c) in keys {
        let sel: Vec<&Row> = rows
            .iter()
            .filter(|r| format!("{:?}", r.usage) == u && r.codec == c)
            .collect();
        let n = sel.len() as f64;
        let mean = sel.iter().map(|r| r.score).sum::<f64>() / n;
        let normal = sel.first().is_some_and(|r| r.usage == Usage::Normal);
        let worst = if normal {
            sel.iter().map(|r| r.score).fold(f64::MIN, f64::max)
        } else {
            sel.iter().map(|r| r.score).fold(f64::MAX, f64::min)
        };
        let p99 = sel.iter().map(|r| r.p99).sum::<f64>() / n;
        println!(
            "| {u} | {c} | {:.2} | {mean:.2} | {worst:.2} | {p99:.2} |",
            sel.first().map_or(0.0, |r| r.bpp)
        );
    }
}
