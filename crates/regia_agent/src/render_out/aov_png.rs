// SPDX-License-Identifier: MPL-2.0
//! PNG files of the material AOVs (`aov_format: "png"` 8-bit, `"png16"` 16-bit, ADR-3256).
//!
//! Only the AOVs that are a 0..1 or unit-vector quantity can be a PNG (`Aov::png_capable`). Codes
//! are `round(value * max)` clamped to the range; normals are stored as `n * 0.5 + 0.5` except on
//! background pixels (a zero vector), which stay 0; `base_color` is sRGB-encoded (the display
//! transfer), roughness and metalness are stored as they are. The encoder is deterministic, so
//! the same planes give the same file bytes.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use super::aov::{Aov, AovPlanes};

/// The sRGB transfer of one linear channel value in 0..1.
fn srgb_encode(linear: f32) -> f32 {
    let v = linear.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// The 0..1 value PNG stores for sample `i` of `aov`'s plane `c` (`planes` holds every channel,
/// the normals' background test needs all three).
fn stored(aov: Aov, planes: &[Vec<f32>], c: usize, i: usize) -> f32 {
    let v = planes[c][i];
    match aov {
        Aov::Normal | Aov::NormalView => {
            let covered = planes.iter().any(|p| p[i] != 0.0);
            if covered { v * 0.5 + 0.5 } else { 0.0 }
        }
        Aov::BaseColor => srgb_encode(v),
        _ => v,
    }
}

/// The interleaved big-endian (or single-byte) sample bytes of `planes` for `aov`.
///
/// # Errors
///
/// `aov` is not PNG-capable, the planes are not `f32`, or their lengths differ from the image.
pub fn png_samples(aov: Aov, planes: &AovPlanes, pixels: usize, sixteen: bool) -> Result<Vec<u8>, String> {
    let AovPlanes::F32(list) = planes else {
        return Err(format!("the {} AOV is not a PNG quantity", aov.name()));
    };
    if !aov.png_capable() || list.len() != aov.channels().len() || list.iter().any(|p| p.len() != pixels) {
        return Err(format!("the {} AOV does not fit a PNG", aov.name()));
    }
    let mut out = Vec::with_capacity(pixels * list.len() * if sixteen { 2 } else { 1 });
    for i in 0..pixels {
        for c in 0..list.len() {
            let v = stored(aov, list, c, i).clamp(0.0, 1.0);
            if sixteen {
                out.extend_from_slice(&((v * 65535.0).round() as u16).to_be_bytes());
            } else {
                out.push((v * 255.0).round() as u8);
            }
        }
    }
    Ok(out)
}

/// Writes `planes` as a grayscale (one channel) or RGB (three channel) PNG at `path`.
///
/// # Errors
///
/// See [`png_samples`]; or the file could not be written.
pub fn write_png_aov(
    path: &Path,
    width: u32,
    height: u32,
    aov: Aov,
    planes: &AovPlanes,
    sixteen: bool,
) -> Result<(), String> {
    let bytes = png_samples(aov, planes, width as usize * height as usize, sixteen)?;
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(if aov.channels().len() == 1 {
        png::ColorType::Grayscale
    } else {
        png::ColorType::Rgb
    });
    encoder.set_depth(if sixteen {
        png::BitDepth::Sixteen
    } else {
        png::BitDepth::Eight
    });
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer
        .write_image_data(&bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer.finish().map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normals_are_biased_and_the_sky_stays_zero() {
        let planes = AovPlanes::F32(vec![vec![0.0, 1.0], vec![0.0, 0.0], vec![0.0, 0.0]]);
        let bytes = png_samples(Aov::Normal, &planes, 2, false).expect("bytes");
        assert_eq!(bytes, [0, 0, 0, 255, 128, 128], "sky 0; +x is (1, 0.5, 0.5)");
        let wide = png_samples(Aov::Normal, &planes, 2, true).expect("bytes");
        assert_eq!(&wide[6..8], &[0xff, 0xff]);
        assert_eq!(&wide[8..10], &[0x80, 0x00], "0.5 * 65535 rounds to 32768");
    }

    #[test]
    fn roughness_is_linear_and_base_color_is_srgb() {
        let rough = AovPlanes::F32(vec![vec![0.5]]);
        assert_eq!(png_samples(Aov::Roughness, &rough, 1, false).expect("r"), [128]);
        let color = AovPlanes::F32(vec![vec![0.5], vec![0.0], vec![1.0]]);
        // 0.5 linear is 0.7354 sRGB.
        assert_eq!(
            png_samples(Aov::BaseColor, &color, 1, false).expect("c"),
            [188, 0, 255]
        );
    }

    #[test]
    fn exact_quantities_are_refused() {
        let ids = AovPlanes::U32(vec![vec![1]]);
        assert!(png_samples(Aov::MaterialId, &ids, 1, false).is_err());
        let short = AovPlanes::F32(vec![vec![0.5]]);
        assert!(
            png_samples(Aov::Normal, &short, 1, false).is_err(),
            "one plane for three channels"
        );
        assert!(
            png_samples(Aov::Roughness, &short, 2, false).is_err(),
            "wrong length"
        );
    }

    #[test]
    fn a_written_png_decodes_to_the_samples_and_repeats_byte_for_byte() {
        let dir = regia_test_utils::unique_temp_dir("aov-png");
        let planes = AovPlanes::F32(vec![vec![0.0, 0.25, 0.5, 1.0]]);
        let (a, b) = (dir.join("a.png"), dir.join("b.png"));
        write_png_aov(&a, 2, 2, Aov::Metalness, &planes, true).expect("a");
        write_png_aov(&b, 2, 2, Aov::Metalness, &planes, true).expect("b");
        assert_eq!(std::fs::read(&a).expect("a"), std::fs::read(&b).expect("b"));
        let mut reader = png::Decoder::new(std::io::BufReader::new(File::open(&a).expect("open")))
            .read_info()
            .expect("header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut buf).expect("frame");
        assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
        assert_eq!(reader.info().color_type, png::ColorType::Grayscale);
        assert_eq!(
            buf[2..4],
            16384u16.to_be_bytes(),
            "0.25 * 65535 = 16383.75 rounds to 16384"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
