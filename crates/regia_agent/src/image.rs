// SPDX-License-Identifier: MPL-2.0
//! PNG + base64 encoding of captured images.

use regia_render::CapturedImage;

/// Encodes `image` as PNG and returns it base64-encoded (standard alphabet, padded; RFC 4648 § 4).
///
/// The PNG is streamed into a buffer of [`regia_render::screenshot::ENCODE_BUFFERS`] (given back
/// here) and the text is built in one of [`regia_render::screenshot::REPLY_BUFFERS`], which the
/// server takes back once the reply is sent (ADR-8848): same-size screenshots after the first
/// allocate nothing image-sized.
pub fn png_base64(image: &CapturedImage) -> Result<String, String> {
    use regia_render::screenshot::{ENCODE_BUFFERS, REPLY_BUFFERS};
    let _span = tracing::debug_span!("png_base64").entered();
    let mut png = ENCODE_BUFFERS.take(0);
    if let Err(error) = png_stream_into(image, &mut png) {
        ENCODE_BUFFERS.give(png);
        return Err(error);
    }
    let mut text = REPLY_BUFFERS.take(png.len().div_ceil(3) * 4);
    base64_into(&png, &mut text);
    ENCODE_BUFFERS.give(png);
    // The alphabet and padding are ASCII.
    String::from_utf8(text).map_err(|e| e.to_string())
}

/// The IDAT chunk size of streamed PNGs: the encoder's one buffer, independent of the image.
const PNG_STREAM_CHUNK: usize = 64 * 1024;

/// Appends `image` to `out` as an RGBA8 PNG, streaming the compressed rows in
/// [`PNG_STREAM_CHUNK`]-byte IDAT chunks (no image-sized intermediate).
fn png_stream_into(image: &CapturedImage, out: &mut Vec<u8>) -> Result<(), String> {
    let mut encoder = png::Encoder::new(&mut *out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    let mut stream = writer
        .stream_writer_with_size(PNG_STREAM_CHUNK)
        .map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut stream, &image.rgba).map_err(|e| e.to_string())?;
    stream.finish().map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

/// Encodes `image` as an RGBA8 PNG.
pub fn png_bytes(image: &CapturedImage) -> Result<Vec<u8>, String> {
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&image.rgba).map_err(|e| e.to_string())?;
    }
    Ok(png_bytes)
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    let mut out = Vec::new();
    base64_into(data, &mut out);
    // The alphabet and padding are ASCII, so the bytes are UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// Appends standard padded base64 of `data` to `out` (ASCII bytes).
fn base64_into(data: &[u8], out: &mut Vec<u8>) {
    out.reserve(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]);
            } else {
                out.push(b'=');
            }
        }
    }
}

/// Decodes standard base64 with optional padding (RFC 4648 § 4). Whitespace is ignored.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes: Vec<u8> = text
        .as_bytes()
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return Err("truncated base64".into());
        }
        let mut n: u32 = 0;
        for (i, c) in chunk.iter().enumerate() {
            let v = value(*c).ok_or_else(|| format!("invalid base64 character {}", *c as char))?;
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Largest image side the agent accepts from a caller (the same bound as offscreen screenshots).
pub const MAX_INPUT_SIDE: u32 = 4096;

/// Decodes a PNG into RGBA8 (palettes expanded, 16-bit reduced to 8-bit, gray and RGB converted with an opaque alpha),
/// but refuses an image whose header width or height is outside `1..=max_side` before any pixel buffer is allocated.
/// The returned buffer is always exactly `width * height * 4` bytes.
pub fn decode_png_rgba_within(bytes: &[u8], max_side: u32) -> Result<regia_render::CapturedImage, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || w > max_side || h > max_side {
        return Err(format!("png is {w}x{h}; each side must be 1..={max_side}"));
    }
    let size = reader.output_buffer_size().ok_or("png: no output buffer")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!("png: unsupported bit depth {:?}", info.bit_depth));
    }
    let samples = info.color_type.samples();
    let pixels = info.width as usize * info.height as usize;
    let src = buf
        .get(..info.buffer_size())
        .filter(|s| s.len() == pixels * samples)
        .ok_or("png: decoded buffer has an unexpected size")?;
    let mut rgba = Vec::with_capacity(pixels * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(src),
        png::ColorType::Rgb => {
            for p in src.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in src {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in src.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Indexed => return Err("png: palette was not expanded".into()),
    }
    Ok(regia_render::CapturedImage {
        width: info.width,
        height: info.height,
        rgba,
    })
}

/// Lays images side by side (top-aligned) into one strip, padding with opaque black.
pub fn compose_strip(images: &[regia_render::CapturedImage]) -> Result<regia_render::CapturedImage, String> {
    if images.is_empty() {
        return Err("no images".into());
    }
    let width = images
        .iter()
        .try_fold(0u32, |sum, i| sum.checked_add(i.width))
        .ok_or("the strip is too wide")?;
    let height = images.iter().map(|i| i.height).max().unwrap_or(0);
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("the strip is too large")?;
    if images.iter().any(|i| {
        i.rgba.len()
            < (i.width as usize)
                .saturating_mul(i.height as usize)
                .saturating_mul(4)
    }) {
        return Err("an image has fewer bytes than its size".into());
    }
    let mut rgba = vec![0u8; len];
    // Opaque black background.
    for px in rgba.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    let mut x = 0usize;
    for image in images {
        for y in 0..image.height as usize {
            let src = y * image.width as usize * 4;
            let dst = y * width as usize * 4 + x * 4;
            rgba[dst..dst + image.width as usize * 4]
                .copy_from_slice(&image.rgba[src..src + image.width as usize * 4]);
        }
        x += image.width as usize;
    }
    Ok(regia_render::CapturedImage { width, height, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_png_is_refused_before_allocation() {
        // The header check refuses a side over the bound before any pixel buffer exists.
        let ok = regia_render::CapturedImage {
            width: 2,
            height: 2,
            rgba: vec![255; 16],
        };
        let bytes = base64_decode(&png_base64(&ok).expect("encodes")).expect("decodes");
        assert!(decode_png_rgba_within(&bytes, MAX_INPUT_SIDE).is_ok());
        let err = decode_png_rgba_within(&bytes, 1).expect_err("2 > 1");
        assert!(err.contains("1..=1"), "{err}");
    }

    /// Encodes a PNG of the given color type and depth from raw sample bytes.
    fn encode(color: png::ColorType, depth: png::BitDepth, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(color);
            enc.set_depth(depth);
            let mut writer = enc.write_header().expect("header");
            writer.write_image_data(data).expect("data");
        }
        out
    }

    #[test]
    fn non_rgba_pngs_decode_to_a_full_rgba_buffer() {
        use png::{BitDepth, ColorType};
        let gray = encode(ColorType::Grayscale, BitDepth::Eight, 2, 1, &[10, 200]);
        let img = decode_png_rgba_within(&gray, MAX_INPUT_SIDE).expect("gray");
        assert_eq!(img.rgba, vec![10, 10, 10, 255, 200, 200, 200, 255]);
        let one = encode(ColorType::Grayscale, BitDepth::Eight, 1, 1, &[7]);
        assert_eq!(
            decode_png_rgba_within(&one, MAX_INPUT_SIDE)
                .expect("1x1")
                .rgba
                .len(),
            4
        );
        let rgb = encode(ColorType::Rgb, BitDepth::Eight, 2, 1, &[1, 2, 3, 4, 5, 6]);
        let img = decode_png_rgba_within(&rgb, MAX_INPUT_SIDE).expect("rgb");
        assert_eq!(img.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        let ga = encode(ColorType::GrayscaleAlpha, BitDepth::Eight, 1, 1, &[9, 128]);
        let img = decode_png_rgba_within(&ga, MAX_INPUT_SIDE).expect("ga");
        assert_eq!(img.rgba, vec![9, 9, 9, 128]);
        let sixteen = encode(
            ColorType::Rgba,
            BitDepth::Sixteen,
            1,
            1,
            &[255, 0, 0, 0, 0, 0, 255, 255],
        );
        let img = decode_png_rgba_within(&sixteen, MAX_INPUT_SIDE).expect("16-bit");
        assert_eq!(img.rgba.len(), 4);
        let one_bit = encode(ColorType::Grayscale, BitDepth::One, 8, 1, &[0b1010_0000]);
        let img = decode_png_rgba_within(&one_bit, MAX_INPUT_SIDE).expect("1-bit");
        assert_eq!(img.rgba.len(), 32);
    }

    #[test]
    fn non_rgba_pngs_decode_to_full_rgba() {
        let rgb: Vec<u8> = (0..12).collect();
        let bytes = encode(png::ColorType::Rgb, png::BitDepth::Eight, 2, 2, &rgb);
        let img = decode_png_rgba_within(&bytes, MAX_INPUT_SIDE).expect("rgb decodes");
        assert_eq!(img.rgba.len(), 16);
        assert_eq!(&img.rgba[..8], &[0, 1, 2, 255, 3, 4, 5, 255]);

        let gray = [10u8, 20, 30, 40];
        let bytes = encode(png::ColorType::Grayscale, png::BitDepth::Eight, 2, 2, &gray);
        let img = decode_png_rgba_within(&bytes, MAX_INPUT_SIDE).expect("gray decodes");
        assert_eq!(&img.rgba[4..8], &[20, 20, 20, 255]);

        let bytes = encode(
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            1,
            1,
            &[7, 9],
        );
        let img = decode_png_rgba_within(&bytes, MAX_INPUT_SIDE).expect("gray-alpha decodes");
        assert_eq!(img.rgba, vec![7, 7, 7, 9]);

        // 16-bit RGBA: 8 bytes per pixel in, 4 out.
        let wide = [0x12u8, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
        let bytes = encode(png::ColorType::Rgba, png::BitDepth::Sixteen, 1, 1, &wide);
        let img = decode_png_rgba_within(&bytes, MAX_INPUT_SIDE).expect("16-bit decodes");
        assert_eq!(img.rgba, vec![0x12, 0x56, 0x9a, 0xde]);
    }

    #[test]
    fn rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn base64_round_trips() {
        for size in [0usize, 1, 2, 3, 7, 31, 255] {
            let data: Vec<u8> = (0..size).map(|i| (i * 37 % 251) as u8).collect();
            let encoded = base64(&data);
            assert_eq!(base64_decode(&encoded).expect("decodes"), data, "size {size}");
        }
        // Whitespace and no padding decode too.
        assert_eq!(base64_decode("Zm9v\nYmFy").expect("decodes"), b"foobar");
        assert!(base64_decode("Zm9vY").is_err(), "truncated");
        assert!(base64_decode("!!!!").is_err(), "invalid characters");
    }

    #[test]
    fn png_has_signature() {
        let img = CapturedImage {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        let b64 = png_base64(&img).expect("encodes");
        assert!(b64.starts_with("iVBORw0KGgo"), "PNG signature in base64: {b64}");
    }

    #[test]
    fn png_round_trips_through_base64() {
        let img = CapturedImage {
            width: 3,
            height: 2,
            rgba: vec![
                10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255, 130, 140, 150, 255,
                160, 170, 180, 255,
            ],
        };
        let b64 = png_base64(&img).expect("encodes");
        let decoded =
            decode_png_rgba_within(&base64_decode(&b64).expect("decodes"), MAX_INPUT_SIDE).expect("decodes");
        assert_eq!(decoded, img);
    }

    #[test]
    fn strips_are_laid_out_horizontally() {
        let a = CapturedImage {
            width: 2,
            height: 2,
            rgba: [255, 0, 0, 255].repeat(4),
        };
        let b = CapturedImage {
            width: 1,
            height: 1,
            rgba: vec![0, 255, 0, 255],
        };
        let strip = compose_strip(&[a.clone(), b.clone()]).expect("composes");
        assert_eq!(strip.width, 3);
        assert_eq!(strip.height, 2);
        // (0,0) red, (2,0) green, (2,1) black padding.
        let at = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 3 + x) * 4;
            [
                strip.rgba[i],
                strip.rgba[i + 1],
                strip.rgba[i + 2],
                strip.rgba[i + 3],
            ]
        };
        assert_eq!(at(0, 0), [255, 0, 0, 255]);
        assert_eq!(at(2, 0), [0, 255, 0, 255]);
        assert_eq!(at(2, 1), [0, 0, 0, 255], "padded with opaque black");
        assert!(compose_strip(&[]).is_err());
    }

    #[test]
    fn rgb_and_gray_pngs_decode_to_rgba() {
        // A caller's PNG need not be RGBA; the result always is (callers index 4 bytes a pixel).
        for (color, samples, expected) in [
            (png::ColorType::Rgb, vec![10u8, 20, 30], [10u8, 20, 30, 255]),
            (png::ColorType::Grayscale, vec![77], [77, 77, 77, 255]),
            (png::ColorType::GrayscaleAlpha, vec![5, 9], [5, 5, 5, 9]),
        ] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
                encoder.set_color(color);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().expect("header");
                writer.write_image_data(&samples).expect("pixels");
            }
            let decoded = decode_png_rgba_within(&bytes, MAX_INPUT_SIDE).expect("decodes");
            assert_eq!(decoded.rgba, expected, "{color:?}");
        }
    }
}
