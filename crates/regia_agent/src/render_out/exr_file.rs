// SPDX-License-Identifier: MPL-2.0
//! OpenEXR files of a render-out (ADR-1521): the scene-linear frame (`format: "exr"`) and the AOVs
//! (`aov_format: "exr"`), written with the pure-Rust `exr` crate (BSD-3-Clause).
//!
//! One layer per file, scanline ZIP compression (lossless, 16 rows per block), channels stored as
//! given (`f32` or `u32`; no conversion to half floats, so depth and ids keep every bit). The
//! encoder is deterministic: the same planes give the same file bytes.

use std::path::Path;

use exr::prelude::{
    AnyChannel, AnyChannels, Encoding, FlatSamples, Image, Layer, LayerAttributes, SmallVec, WritableImage,
};

use super::aov::AovPlanes;

/// Writes `planes` (one per name in `names`, each `width * height` samples, row-major, top row
/// first) as a single-layer EXR file at `path`.
///
/// # Errors
///
/// The plane count or a plane's length does not match, or the file could not be written.
pub fn write_planes(
    path: &Path,
    width: u32,
    height: u32,
    names: &[&str],
    planes: &AovPlanes,
) -> Result<(), String> {
    let pixels = width as usize * height as usize;
    let mut channels: SmallVec<[AnyChannel<FlatSamples>; 4]> = SmallVec::new();
    match planes {
        AovPlanes::F32(list) => {
            check(names, list.iter().map(Vec::len), pixels)?;
            for (name, plane) in names.iter().zip(list) {
                channels.push(AnyChannel::new(*name, FlatSamples::F32(plane.clone())));
            }
        }
        AovPlanes::U32(list) => {
            check(names, list.iter().map(Vec::len), pixels)?;
            for (name, plane) in names.iter().zip(list) {
                channels.push(AnyChannel::new(*name, FlatSamples::U32(plane.clone())));
            }
        }
    }
    let layer = Layer::new(
        (width as usize, height as usize),
        LayerAttributes::default(),
        Encoding::SMALL_LOSSLESS,
        AnyChannels::sort(channels),
    );
    Image::from_layer(layer)
        .write()
        .to_file(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn check(names: &[&str], lens: impl ExactSizeIterator<Item = usize>, pixels: usize) -> Result<(), String> {
    if lens.len() != names.len() {
        return Err(format!("{} planes for {} channel names", lens.len(), names.len()));
    }
    for (name, len) in names.iter().zip(lens) {
        if len != pixels {
            return Err(format!("channel {name} has {len} samples, the image {pixels}"));
        }
    }
    Ok(())
}

/// The planes of interleaved linear RGB (`[r, g, b, r, g, b, ...]`), for an `R`, `G`, `B` file.
pub fn rgb_planes(rgb: &[f32]) -> AovPlanes {
    let (triples, _) = rgb.as_chunks::<3>();
    AovPlanes::F32((0..3).map(|c| triples.iter().map(|t| t[c]).collect()).collect())
}

/// Channel names of a scene-linear frame file.
pub const RGB_CHANNELS: [&str; 3] = ["R", "G", "B"];

/// Resamples a scene-linear capture (`source` row-major, `sw`×`sh`) to the delivered `width`×
/// `height`, interleaved RGB. A capture that is a whole multiple of the delivered size (the
/// supersampled render) is box-averaged in linear light, as the 8-bit path averages its blocks;
/// any other size (a scaled internal resolution) is sampled bilinearly at the pixel centers.
pub fn resample_rgb(source: &[[f32; 3]], sw: u32, sh: u32, width: u32, height: u32) -> Vec<f32> {
    let (sw, sh, w, h) = (
        sw.max(1) as usize,
        sh.max(1) as usize,
        width.max(1) as usize,
        height.max(1) as usize,
    );
    let at = |x: usize, y: usize| source.get(y * sw + x).copied().unwrap_or([0.0; 3]);
    let mut out = Vec::with_capacity(w * h * 3);
    if sw.is_multiple_of(w) && sh.is_multiple_of(h) && sw / w == sh / h {
        let f = sw / w;
        let inv = 1.0 / (f * f) as f32;
        for y in 0..h {
            for x in 0..w {
                let mut sum = [0.0f32; 3];
                for dy in 0..f {
                    for dx in 0..f {
                        let c = at(x * f + dx, y * f + dy);
                        for (s, v) in sum.iter_mut().zip(c) {
                            *s += v;
                        }
                    }
                }
                out.extend(sum.iter().map(|v| v * inv));
            }
        }
        return out;
    }
    for y in 0..h {
        let fy = ((y as f32 + 0.5) * sh as f32 / h as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
        let (y0, ty) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(sh - 1);
        for x in 0..w {
            let fx = ((x as f32 + 0.5) * sw as f32 / w as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let (x0, tx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(sw - 1);
            let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
            for (((a, b), c), d) in a.iter().zip(b).zip(c).zip(d) {
                let top = a + (b - a) * tx;
                let bottom = c + (d - c) * tx;
                out.push(top + (bottom - top) * ty);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use exr::prelude::read_first_flat_layer_from_file;

    fn read(path: &Path) -> Vec<(String, FlatSamples)> {
        let image = read_first_flat_layer_from_file(path).expect("read exr");
        image
            .layer_data
            .channel_data
            .list
            .into_iter()
            .map(|channel| (channel.name.to_string(), channel.sample_data))
            .collect()
    }

    #[test]
    fn float_and_integer_planes_read_back_bit_exact() {
        let dir = regia_test_utils::unique_temp_dir("render-out-exr");
        let depth = dir.join("depth.exr");
        let z = vec![0.125, 1.0e6, f32::INFINITY, 3.0e-7, 12.5, 0.0];
        write_planes(&depth, 3, 2, &["Z"], &AovPlanes::F32(vec![z.clone()])).expect("write depth");
        let back = read(&depth);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "Z");
        match &back[0].1 {
            FlatSamples::F32(values) => {
                let bits: Vec<u32> = values.iter().map(|v| v.to_bits()).collect();
                let want: Vec<u32> = z.iter().map(|v| v.to_bits()).collect();
                assert_eq!(bits, want, "every bit of every depth survives");
            }
            other => panic!("depth came back as {other:?}"),
        }
        let ids = dir.join("id.exr");
        let instance = vec![0, 1, 2, 70_000, u32::MAX, 3];
        let class = vec![0, 4, 4, 9, 1, 1];
        write_planes(
            &ids,
            3,
            2,
            &["id.instance", "id.class"],
            &AovPlanes::U32(vec![instance.clone(), class.clone()]),
        )
        .expect("write ids");
        let back = read(&ids);
        let channel = |name: &str| {
            back.iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| s.clone())
                .expect(name)
        };
        assert_eq!(channel("id.instance"), FlatSamples::U32(instance));
        assert_eq!(channel("id.class"), FlatSamples::U32(class));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_planes_give_the_same_file_bytes() {
        let dir = regia_test_utils::unique_temp_dir("render-out-exr-same");
        let planes = rgb_planes(&(0..4 * 4 * 3).map(|i| i as f32 * 0.37).collect::<Vec<_>>());
        let (a, b) = (dir.join("a.exr"), dir.join("b.exr"));
        write_planes(&a, 4, 4, &RGB_CHANNELS, &planes).expect("a");
        write_planes(&b, 4, 4, &RGB_CHANNELS, &planes).expect("b");
        assert_eq!(std::fs::read(&a).expect("a"), std::fs::read(&b).expect("b"));
        let back = read(&a);
        let names: Vec<&str> = back.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["B", "G", "R"], "EXR stores channels sorted by name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatched_planes_are_refused_before_anything_is_written() {
        let dir = regia_test_utils::unique_temp_dir("render-out-exr-bad");
        let path = dir.join("bad.exr");
        let short = AovPlanes::F32(vec![vec![0.0; 3]]);
        assert!(write_planes(&path, 2, 2, &["Z"], &short).is_err());
        let two = AovPlanes::F32(vec![vec![0.0; 4], vec![0.0; 4]]);
        assert!(write_planes(&path, 2, 2, &["Z"], &two).is_err());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn captures_resample_to_the_delivered_size() {
        // 4x2 at factor 2 -> 2x1: block averages in linear light.
        let src: Vec<[f32; 3]> = (0..8).map(|i| [i as f32, 1.0, 100.0 * i as f32]).collect();
        let out = resample_rgb(&src, 4, 2, 2, 1);
        assert_eq!(out, [2.5, 1.0, 250.0, 4.5, 1.0, 450.0]);
        // The same size is a copy.
        let same = resample_rgb(&src, 4, 2, 4, 2);
        assert_eq!(&same[..3], &[0.0, 1.0, 0.0]);
        assert_eq!(same.len(), 24);
        // A scaled internal size (3x1 -> 2x1) is bilinear and keeps a flat field flat.
        let flat = vec![[5.0, 5.0, 5.0]; 3];
        assert_eq!(resample_rgb(&flat, 3, 1, 2, 1), [5.0; 6]);
    }

    #[test]
    fn rgb_planes_split_interleaved_triples() {
        assert_eq!(
            rgb_planes(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            AovPlanes::F32(vec![vec![1.0, 4.0], vec![2.0, 5.0], vec![3.0, 6.0]])
        );
    }
}
