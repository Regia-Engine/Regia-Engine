// SPDX-License-Identifier: MPL-2.0
//! Radiance RGBE (`.hdr`, `.pic`) reader: the common file type of HDRI sky maps.
//!
//! Written from Greg Ward's public description of the format ("Real Pixels", Graphics Gems II,
//! 1991, and the Radiance file-format notes): a text header that starts `#?RADIANCE` (or
//! `#?RGBE`) and ends with an empty line, a resolution line, then scanlines of 4-byte RGBE pixels,
//! flat, in the old run-length form (`1, 1, 1, n` repeats the previous pixel) or in the adaptive
//! form (`2, 2, hi, lo` then the four components one after another, each run-length coded).
//! A pixel decodes as `(c + 0.5) · 2^(e − 136)` per component, 0 when `e` is 0.
//!
//! Only `32-bit_rle_rgbe` is read (`32-bit_rle_xyze` is refused), in the standard `-Y h +X w`
//! orientation or bottom-up `+Y h +X w`. The `EXPOSURE` header is not applied: scale the map
//! instead (the bake description's `sky_hdri.scale` or `irradiance`).

/// A decoded RGBE image, linear RGB, row-major, top row first.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width × height` linear RGB values.
    pub rgb: Vec<[f32; 3]>,
}

/// RGBE errors.
#[derive(Debug, thiserror::Error)]
pub enum HdrError {
    /// File IO.
    #[error("hdr io: {0}")]
    Io(#[from] std::io::Error),
    /// Not a file this reader understands.
    #[error("hdr format: {0}")]
    Format(String),
}

/// Largest side the reader accepts (a sanity bound against a corrupt resolution line).
const MAX_SIDE: u32 = 1 << 15;
/// Largest pixel count the reader accepts (16384 × 16384, 4 GiB decoded). Old-style repeat runs
/// let a valid scanline be a dozen bytes, so a small crafted file could otherwise declare
/// 32768 × 32768 and decode into 16 GiB.
const MAX_PIXELS: u64 = 1 << 28;

fn bad(message: impl Into<String>) -> HdrError {
    HdrError::Format(message.into())
}

/// Decodes one RGBE pixel.
#[must_use]
pub fn rgbe_to_rgb(p: [u8; 4]) -> [f32; 3] {
    if p[3] == 0 {
        return [0.0; 3];
    }
    let f = (f32::from(p[3]) - 136.0).exp2();
    [
        (f32::from(p[0]) + 0.5) * f,
        (f32::from(p[1]) + 0.5) * f,
        (f32::from(p[2]) + 0.5) * f,
    ]
}

/// Encodes linear RGB as one RGBE pixel (the inverse of [`rgbe_to_rgb`] up to rounding).
#[must_use]
pub fn rgb_to_rgbe(c: [f32; 3]) -> [u8; 4] {
    let m = c[0].max(c[1]).max(c[2]);
    if m.is_nan() || m <= 1e-32 || m.is_infinite() {
        return [0; 4];
    }
    // m = f · 2^e with f in [0.5, 1): the mantissas are c / 2^e · 256.
    // Exponents above 127 do not fit the biased byte: the largest code saturates the mantissas
    // instead of encoding an octave low.
    let e = (m.log2().floor() as i32 + 1).min(127);
    let scale = (8 - e) as f32;
    let q = |v: f32| ((v.max(0.0) * scale.exp2()) as i32).clamp(0, 255) as u8;
    let biased = (e + 128).clamp(1, 255) as u8;
    [q(c[0]), q(c[1]), q(c[2]), biased]
}

/// Reads an RGBE file.
///
/// # Errors
///
/// The file cannot be read or is not an RGBE image this reader understands.
pub fn read(path: &std::path::Path) -> Result<HdrImage, HdrError> {
    let bytes = std::fs::read(path)?;
    decode(&bytes)
}

/// Decodes RGBE bytes.
///
/// # Errors
///
/// A missing magic line, an unsupported format or orientation, or truncated pixel data.
pub fn decode(bytes: &[u8]) -> Result<HdrImage, HdrError> {
    let mut at = 0usize;
    let line = |at: &mut usize| next_line(bytes, at);
    let magic = line(&mut at)?;
    if !(magic.starts_with(b"#?RADIANCE") || magic.starts_with(b"#?RGBE")) {
        return Err(bad("not a Radiance file (no #?RADIANCE line)"));
    }
    loop {
        let l = line(&mut at)?;
        if l.is_empty() || l == b"\r" {
            break;
        }
        if let Some(fmt) = l.strip_prefix(b"FORMAT=") {
            let fmt = String::from_utf8_lossy(fmt);
            if fmt.trim() != "32-bit_rle_rgbe" {
                return Err(bad(format!(
                    "format {} (only 32-bit_rle_rgbe is read)",
                    fmt.trim()
                )));
            }
        }
    }
    let res = String::from_utf8_lossy(line(&mut at)?).to_string();
    let parts: Vec<&str> = res.split_whitespace().collect();
    let (flip, height, width) = match parts.as_slice() {
        [y, h, x, w] if (*y == "-Y" || *y == "+Y") && *x == "+X" => {
            let h: u32 = h.parse().map_err(|_| bad(format!("resolution line `{res}`")))?;
            let w: u32 = w.parse().map_err(|_| bad(format!("resolution line `{res}`")))?;
            (*y == "+Y", h, w)
        }
        _ => {
            return Err(bad(format!(
                "resolution line `{}` (only -Y h +X w and +Y h +X w are read)",
                res.trim()
            )));
        }
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(bad(format!("{width} × {height} pixels")));
    }
    let (w, h) = (width as usize, height as usize);
    let data = &bytes[at..];
    // Every scanline starts with at least four bytes (a run-length header or a pixel), so a file
    // with fewer than `4 * h` bytes of pixel data is truncated. Past that the frame grows row by
    // row as scanlines decode instead of being reserved from the resolution line: old-style
    // repeat runs let a valid line be a dozen bytes, so only decoded rows prove the size.
    if data.len() / 4 < h {
        return Err(bad("the pixel data is truncated"));
    }
    let mut rgbe = Vec::new();
    let mut row = vec![[0u8; 4]; w];
    let mut pos = 0usize;
    for y in 0..h {
        pos = read_scanline(data, pos, &mut row)?;
        // Checked once the first scanline decoded (a corrupt first row reports its own error),
        // before the frame grows past one row.
        if y == 0 && u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(bad(format!(
                "{width} × {height} pixels is more than the {MAX_PIXELS}-pixel limit"
            )));
        }
        rgbe.extend_from_slice(&row);
    }
    if flip {
        let mut flipped = Vec::with_capacity(rgbe.len());
        for row in rgbe.chunks(w).rev() {
            flipped.extend_from_slice(row);
        }
        rgbe = flipped;
    }
    Ok(HdrImage {
        width,
        height,
        rgb: rgbe.into_iter().map(rgbe_to_rgb).collect(),
    })
}

/// The header line at `*at` (without its `\n`); moves `*at` past it.
fn next_line<'a>(bytes: &'a [u8], at: &mut usize) -> Result<&'a [u8], HdrError> {
    let start = (*at).min(bytes.len());
    let end = bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|k| start + k)
        .ok_or_else(|| bad("the header has no end"))?;
    *at = end + 1;
    Ok(&bytes[start..end])
}

fn take(data: &[u8], pos: usize, n: usize) -> Result<&[u8], HdrError> {
    data.get(pos..pos + n)
        .ok_or_else(|| bad("the pixel data is truncated"))
}

/// Reads one scanline starting at `pos` into `row`; returns the position after it.
fn read_scanline(data: &[u8], pos: usize, row: &mut [[u8; 4]]) -> Result<usize, HdrError> {
    let w = row.len();
    let head = take(data, pos, 4)?;
    if (8..0x8000).contains(&w) && head[0] == 2 && head[1] == 2 && head[2] & 0x80 == 0 {
        // Adaptive run-length: the four components one after another.
        let len = (usize::from(head[2]) << 8) | usize::from(head[3]);
        if len != w {
            return Err(bad(format!("a scanline says {len} pixels, the image {w}")));
        }
        let mut p = pos + 4;
        for c in 0..4 {
            let mut x = 0usize;
            while x < w {
                let count = take(data, p, 1)?[0];
                p += 1;
                if count > 128 {
                    let n = usize::from(count - 128);
                    let v = take(data, p, 1)?[0];
                    p += 1;
                    if x + n > w {
                        return Err(bad("a run overruns its scanline"));
                    }
                    for px in &mut row[x..x + n] {
                        px[c] = v;
                    }
                    x += n;
                } else {
                    let n = usize::from(count);
                    if n == 0 || x + n > w {
                        return Err(bad("a bad literal run in a scanline"));
                    }
                    let lit = take(data, p, n)?;
                    p += n;
                    for (px, &v) in row[x..x + n].iter_mut().zip(lit) {
                        px[c] = v;
                    }
                    x += n;
                }
            }
        }
        return Ok(p);
    }
    // Flat pixels, with the old `1, 1, 1, n` repeat runs (shifted counts for consecutive runs).
    let mut p = pos;
    let mut x = 0usize;
    let mut shift = 0u32;
    while x < w {
        let px = take(data, p, 4)?;
        p += 4;
        if px[0] == 1 && px[1] == 1 && px[2] == 1 {
            let n = usize::from(px[3])
                .checked_shl(shift)
                .ok_or_else(|| bad("too many consecutive old-style runs"))?;
            if x == 0 || n > w - x {
                return Err(bad("an old-style run without a pixel to repeat"));
            }
            let prev = row[x - 1];
            for q in &mut row[x..x + n] {
                *q = prev;
            }
            x += n;
            shift += 8;
        } else {
            row[x] = [px[0], px[1], px[2], px[3]];
            x += 1;
            shift = 0;
        }
    }
    Ok(p)
}

/// Encodes `rgb` (`width × height`, top row first) as a flat RGBE file (tests and fixtures).
#[must_use]
pub fn encode_flat(width: u32, height: u32, rgb: &[[f32; 3]]) -> Vec<u8> {
    let mut out = format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y {height} +X {width}\n").into_bytes();
    for c in rgb {
        out.extend_from_slice(&rgb_to_rgbe(*c));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A crafted header may declare more pixels than any real HDRI: the reader refuses it before
    /// it decodes a row (corr-low-render, corr-pt finding 7).
    #[test]
    fn a_resolution_over_the_pixel_limit_is_refused_up_front() {
        // One compact 16-byte scanline, padded past the 4-bytes-per-row guard: a pixel, a run of
        // 255, a run of 63 shifted by 8 (16128), a pixel: 1 + 255 + 16128 + 1 = 16385.
        let mut file = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 16385 +X 16385\n".to_vec();
        let start = file.len();
        file.extend_from_slice(&[128, 64, 32, 129, 1, 1, 1, 255]);
        file.extend_from_slice(&[1, 1, 1, 63, 128, 64, 32, 129]);
        file.resize(start + 16385 * 4, 0);
        let e = decode(&file).expect_err("over the limit").to_string();
        assert!(e.contains("limit"), "{e}");
        // 16384 × 16384 is still read (here: refused only for its truncated data).
        let mut file = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 16384 +X 16384\n".to_vec();
        file.resize(file.len() + 16384 * 4, 0);
        let e = decode(&file).expect_err("truncated").to_string();
        assert!(!e.contains("limit"), "{e}");
    }

    /// Values in [2^127, 2^128) saturate the largest code instead of encoding an octave low
    /// (corr-low-render, corr-pt finding 13).
    #[test]
    fn the_largest_exponent_saturates_instead_of_dropping_an_octave() {
        for v in [2f32.powi(127), 1.5 * 2f32.powi(127), f32::MAX] {
            let back = rgbe_to_rgb(rgb_to_rgbe([v; 3]));
            let largest = 255.5 * 2f32.powi(119);
            assert!(back[0] >= 0.99 * v.min(largest), "{v} decoded as {}", back[0]);
        }
    }

    #[test]
    fn rgbe_round_trips_within_a_mantissa_step() {
        for c in [[1.0f32, 0.5, 0.25], [1234.5, 10.0, 0.001], [3.0e6, 2.0e6, 1.0e6]] {
            let back = rgbe_to_rgb(rgb_to_rgbe(c));
            let m = c[0].max(c[1]).max(c[2]);
            for k in 0..3 {
                assert!((back[k] - c[k]).abs() <= m / 128.0, "{c:?} → {back:?}");
            }
        }
        assert_eq!(rgbe_to_rgb([10, 20, 30, 0]), [0.0; 3]);
        assert_eq!(rgb_to_rgbe([0.0; 3]), [0; 4]);
    }

    #[test]
    fn flat_files_decode_top_row_first() {
        let rgb = [[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 4.0], [8.0, 8.0, 8.0]];
        let img = decode(&encode_flat(2, 2, &rgb)).expect("decode");
        assert_eq!((img.width, img.height), (2, 2));
        for (a, b) in img.rgb.iter().zip(&rgb) {
            let m = b[0].max(b[1]).max(b[2]);
            for k in 0..3 {
                assert!((a[k] - b[k]).abs() <= m / 64.0, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn adaptive_run_length_and_bottom_up_files_decode() {
        // Two 8-pixel scanlines, written bottom-up: a run of 8 per component on the first, eight
        // literals per component on the second.
        let mut bytes = b"#?RGBE\nFORMAT=32-bit_rle_rgbe\nEXPOSURE=1.0\n\n+Y 2 +X 8\n".to_vec();
        bytes.extend_from_slice(&[2, 2, 0, 8]);
        for v in [128u8, 64, 32, 129] {
            bytes.extend_from_slice(&[128 + 8, v]);
        }
        bytes.extend_from_slice(&[2, 2, 0, 8]);
        for c in 0..4u8 {
            bytes.push(8);
            for x in 0..8u8 {
                bytes.push(if c == 3 { 129 } else { 16 * x + c });
            }
        }
        let img = decode(&bytes).expect("decode");
        assert_eq!((img.width, img.height), (8, 2));
        // Bottom-up: the first scanline in the file is the bottom row.
        let bottom = &img.rgb[8..];
        assert!(bottom.iter().all(|p| (p[0] - 128.5 / 128.0).abs() < 1e-6));
        let top = &img.rgb[..8];
        assert!((top[3][0] - 48.5 / 128.0).abs() < 1e-6, "{:?}", top[3]);
    }

    #[test]
    fn old_style_runs_repeat_the_previous_pixel() {
        let mut bytes = b"#?RADIANCE\n\n-Y 1 +X 5\n".to_vec();
        bytes.extend_from_slice(&[128, 128, 128, 129]);
        bytes.extend_from_slice(&[1, 1, 1, 3]);
        bytes.extend_from_slice(&[64, 64, 64, 129]);
        let img = decode(&bytes).expect("decode");
        assert!(img.rgb[..4].iter().all(|p| (p[0] - 128.5 / 128.0).abs() < 1e-6));
        assert!((img.rgb[4][0] - 64.5 / 128.0).abs() < 1e-6);
    }

    #[test]
    fn bad_files_are_refused_with_a_reason() {
        assert!(decode(b"P6\n1 1\n255\n").is_err());
        assert!(decode(b"#?RADIANCE\nFORMAT=32-bit_rle_xyze\n\n-Y 1 +X 1\n\0\0\0\0").is_err());
        assert!(decode(b"#?RADIANCE\n\n-X 1 +Y 1\n\0\0\0\0").is_err());
        assert!(decode(b"#?RADIANCE\n\n-Y 2 +X 2\n\0\0\0\0").is_err());
    }

    // Independently assembled from Ward's Radiance File Formats, pp. 29–33, 39:
    // https://floyd.lbl.gov/radiance/refer/filefmts.pdf . No production encoder is used.
    const PIXELS: [[u8; 4]; 4] = [
        [128, 64, 32, 129],
        [32, 160, 96, 130],
        [192, 0, 255, 128],
        [250, 120, 10, 0],
    ];
    const LINEAR: [[f32; 3]; 4] = [
        [128.5 / 128.0, 64.5 / 128.0, 32.5 / 128.0],
        [32.5 / 64.0, 160.5 / 64.0, 96.5 / 64.0],
        [192.5 / 256.0, 0.5 / 256.0, 255.5 / 256.0],
        [0.0; 3],
    ];

    #[derive(Clone, Copy, Debug)]
    enum Encoding {
        Flat,
        Old,
        AdaptiveLiteral,
        AdaptiveRun,
    }

    fn header(width: usize, height: usize, bottom_up: bool) -> Vec<u8> {
        let y = if bottom_up { "+Y" } else { "-Y" };
        format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n{y} {height} +X {width}\n").into_bytes()
    }

    fn file(width: usize, height: usize, data: &[u8]) -> Vec<u8> {
        let mut bytes = header(width, height, false);
        bytes.extend_from_slice(data);
        bytes
    }

    fn encoded_row(row: &[[u8; 4]], encoding: Encoding) -> Vec<u8> {
        let mut out = Vec::new();
        match encoding {
            Encoding::Flat => out.extend(row.iter().flatten()),
            Encoding::Old => {
                let mut start = 0;
                while start < row.len() {
                    let pixel = row[start];
                    let len = row[start..].iter().take_while(|&&p| p == pixel).count();
                    out.extend_from_slice(&pixel);
                    let mut repeats = len - 1;
                    while repeats != 0 {
                        out.extend_from_slice(&[1, 1, 1, (repeats & 255) as u8]);
                        repeats >>= 8;
                    }
                    start += len;
                }
            }
            Encoding::AdaptiveLiteral | Encoding::AdaptiveRun => {
                assert!((8..32768).contains(&row.len()));
                out.extend_from_slice(&[2, 2, (row.len() >> 8) as u8, row.len() as u8]);
                for c in 0..4 {
                    match encoding {
                        Encoding::AdaptiveLiteral => {
                            for chunk in row.chunks(128) {
                                out.push(chunk.len() as u8);
                                out.extend(chunk.iter().map(|p| p[c]));
                            }
                        }
                        Encoding::AdaptiveRun => {
                            let mut start = 0;
                            while start < row.len() {
                                let v = row[start][c];
                                let len = row[start..].iter().take(127).take_while(|p| p[c] == v).count();
                                out.extend_from_slice(&[128 + len as u8, v]);
                                start += len;
                            }
                        }
                        _ => unreachable!("adaptive encoding"),
                    }
                }
            }
        }
        out
    }

    fn fixture(width: usize, bottom_up: bool, encoding: Encoding) -> (Vec<u8>, HdrImage) {
        let mut bytes = header(width, 3, bottom_up);
        let mut rgb = Vec::new();
        let rows: Vec<Vec<_>> = (0..3)
            .map(|y| {
                (0..width)
                    .map(|x| (y + x / (width / 2).max(1)) % PIXELS.len())
                    .collect()
            })
            .collect();
        for row in &rows {
            rgb.extend(row.iter().map(|&p| LINEAR[p]));
        }
        for y in 0..3 {
            let source_y = if bottom_up { 2 - y } else { y };
            let row: Vec<_> = rows[source_y].iter().map(|&p| PIXELS[p]).collect();
            bytes.extend(encoded_row(&row, encoding));
        }
        (
            bytes,
            HdrImage {
                width: width as u32,
                height: 3,
                rgb,
            },
        )
    }

    fn assert_format_error(result: Result<HdrImage, HdrError>, reason: &str) {
        match result {
            Err(HdrError::Format(message)) => {
                assert!(!message.trim().is_empty(), "empty HDR error reason");
                assert!(message.contains(reason), "{message:?} should contain {reason:?}");
            }
            other => panic!("expected HDR format error containing {reason:?}, got {other:?}"),
        }
    }

    #[test]
    fn equivalent_encodings_agree_exactly_at_supported_width_boundaries() {
        for width in [1, 2, 7, 8, 9, 127, 128, 129, 255, 256, 257, 32767, 32768] {
            for bottom_up in [false, true] {
                for encoding in [
                    Encoding::Flat,
                    Encoding::Old,
                    Encoding::AdaptiveLiteral,
                    Encoding::AdaptiveRun,
                ] {
                    if matches!(encoding, Encoding::AdaptiveLiteral | Encoding::AdaptiveRun)
                        && !(8..32768).contains(&width)
                    {
                        continue;
                    }
                    let (bytes, expected) = fixture(width, bottom_up, encoding);
                    assert_eq!(
                        decode(&bytes).expect("valid RGBE fixture"),
                        expected,
                        "width={width}, bottom_up={bottom_up}, encoding={encoding:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn adaptive_literal_and_repeat_counts_straddle_128_without_crossing_rows() {
        for (repeat, lengths) in [
            (false, &[1usize, 2, 3, 126, 127, 128][..]),
            (true, &[1usize, 2, 3, 126, 127][..]),
        ] {
            for &n in lengths {
                for ends_at_row in [false, true] {
                    let width = if ends_at_row { n.max(8) } else { n + 8 };
                    let mut data = vec![2, 2, (width >> 8) as u8, width as u8];
                    let start = if ends_at_row { width - n } else { 0 };
                    for v in PIXELS[0] {
                        if start != 0 {
                            data.push(start as u8);
                            data.extend(std::iter::repeat_n(v, start));
                        }
                        data.push(if repeat { 128 + n as u8 } else { n as u8 });
                        data.extend(std::iter::repeat_n(v, if repeat { 1 } else { n }));
                        let tail = width - start - n;
                        if tail != 0 {
                            data.push(tail as u8);
                            data.extend(std::iter::repeat_n(v, tail));
                        }
                    }
                    let boundary = data.len();
                    data.extend(encoded_row(&vec![PIXELS[1]; width], Encoding::AdaptiveRun));
                    let mut row = vec![[0; 4]; width];
                    let pos = read_scanline(&data, 0, &mut row).expect("first row");
                    assert_eq!(pos, boundary, "count={n}, repeat={repeat}");
                    assert_eq!(row, vec![PIXELS[0]; width]);
                    assert_eq!(
                        read_scanline(&data, pos, &mut row).expect("second row"),
                        data.len()
                    );
                    assert_eq!(row, vec![PIXELS[1]; width]);
                    let decoded = decode(&file(width, 2, &data)).expect("two bounded scanlines");
                    assert_eq!(decoded.rgb[..width], vec![LINEAR[0]; width]);
                    assert_eq!(decoded.rgb[width..], vec![LINEAR[1]; width]);
                }
            }
        }
    }

    #[test]
    fn adaptive_bad_tokens_are_rejected_in_every_component() {
        for c in 0..4 {
            for consumed in [0u8, 3] {
                for (token, reason) in [
                    (0, "literal"),
                    (9 - consumed, "literal"),
                    (137 - consumed, "overruns"),
                ] {
                    let mut data = vec![2, 2, 0, 8];
                    for v in &PIXELS[0][..c] {
                        data.extend_from_slice(&[136, *v]);
                    }
                    if consumed != 0 {
                        data.extend_from_slice(&[128 + consumed, 64]);
                    }
                    data.push(token);
                    // Sufficient bytes remain for a broken decoder to consume across components/rows.
                    data.extend_from_slice(&[64; 128]);
                    assert_format_error(decode(&file(8, 2, &data)), reason);
                }
                let mut data = vec![2, 2, 0, 8];
                for v in &PIXELS[0][..c] {
                    data.extend_from_slice(&[136, *v]);
                }
                if consumed != 0 {
                    data.extend_from_slice(&[128 + consumed, 64]);
                }
                data.push(136 - consumed); // Missing the value following a repeat token.
                assert_format_error(decode(&file(8, 1, &data)), "truncated");
            }
        }
    }

    #[test]
    fn adaptive_headers_require_matching_width_and_an_unset_high_bit() {
        for claimed in [0u16, 7, 9, 255, 256, 32767] {
            let [hi, lo] = claimed.to_be_bytes();
            assert_format_error(decode(&file(8, 1, &[2, 2, hi, lo, 136, 64])), "scanline says");
        }
        // These are flat pixels, not adaptive headers, including at the width 7/8 and
        // 32767/32768 dispatch boundaries. Keep the exponent zero so the first pixel is black.
        for width in [7, 8, 32767, 32768] {
            for first in [[2, 3, 0, 0], [3, 2, 0, 0], [2, 2, 128, 0]] {
                let mut data = first.to_vec();
                data.extend(encoded_row(&vec![PIXELS[0]; width - 1], Encoding::Flat));
                let img = decode(&file(width, 1, &data)).expect("flat dispatch");
                assert_eq!(img.rgb[0], [0.0; 3]);
                assert_eq!(img.rgb[1..], vec![LINEAR[0]; width - 1]);
            }
        }
    }

    #[test]
    fn old_runs_end_exactly_at_rows_and_reset_after_literals_and_scanlines() {
        // 1 + 0 + (1 << 8) = 257 A pixels, then a literal resets the shift before B's repeat.
        let mut data = PIXELS[0].to_vec();
        data.extend_from_slice(&[1, 1, 1, 0, 1, 1, 1, 1]);
        data.extend_from_slice(&PIXELS[1]);
        data.extend_from_slice(&[1, 1, 1, 2]); // First row ends exactly at 260.
        let boundary = data.len();
        data.extend_from_slice(&PIXELS[2]);
        data.extend_from_slice(&[1, 1, 1, 3]); // Must start at shift zero again.
        data.extend_from_slice(&PIXELS[3]);
        data.extend_from_slice(&[1, 1, 1, 255]);
        let img = decode(&file(260, 2, &data)).expect("old runs reset their count shift");
        let expected: Vec<_> = [(257, LINEAR[0]), (3, LINEAR[1]), (4, LINEAR[2]), (256, LINEAR[3])]
            .into_iter()
            .flat_map(|(n, p)| std::iter::repeat_n(p, n))
            .collect();
        assert_eq!(img.rgb, expected);
        let mut row = vec![[0; 4]; 260];
        assert_eq!(
            read_scanline(&data, 0, &mut row).expect("old row cursor"),
            boundary
        );
    }

    #[test]
    fn old_runs_cannot_overrun_or_repeat_pixels_from_the_previous_row() {
        for width in [1, 7, 8, 255] {
            let mut data = PIXELS[0].to_vec();
            data.extend_from_slice(&[1, 1, 1, width as u8]); // One pixel too many.
            data.extend_from_slice(&PIXELS[1]);
            assert_format_error(decode(&file(width, 2, &data)), "old-style run");
        }
        for previous_row in [false, true] {
            let mut data = Vec::new();
            if previous_row {
                data.extend(encoded_row(&[PIXELS[0]; 8], Encoding::Old));
            }
            data.extend_from_slice(&[1, 1, 1, 8]);
            assert_format_error(
                decode(&file(8, if previous_row { 2 } else { 1 }, &data)),
                "without a pixel",
            );
        }
    }

    #[test]
    fn old_run_shift_counts_cannot_discard_high_bits() {
        // On 32/64-bit hosts, respectively, 128 << 24/56 sets the top bit of usize.
        // Counts arrive in whole bytes, so this last in-range digit must stay large rather than
        // wrap to fit a 2-pixel row. The next byte's shift is checked separately below.
        for zero_digits in [3, 7] {
            let mut data = PIXELS[0].to_vec();
            for _ in 0..zero_digits {
                data.extend_from_slice(&[1, 1, 1, 0]);
            }
            data.extend_from_slice(&[1, 1, 1, 128]);
            data.extend_from_slice(&PIXELS[1]);
            assert_format_error(decode(&file(2, 1, &data)), "old-style run");
        }
    }

    #[test]
    fn old_run_shift_limit_returns_an_error_and_zero_digits_can_extend_counts() {
        let mut data = PIXELS[0].to_vec();
        for _ in 0..=usize::BITS / 8 {
            data.extend_from_slice(&[1, 1, 1, 0]);
        }
        data.extend_from_slice(&PIXELS[1]);
        assert_format_error(decode(&file(2, 1, &data)), "consecutive old-style runs");
        // A zero low byte is necessary to express exactly 256 repeats; it is not an adaptive zero token.
        let mut data = PIXELS[0].to_vec();
        data.extend_from_slice(&[1, 1, 1, 0, 1, 1, 1, 1]);
        let img = decode(&file(257, 1, &data)).expect("256 old repeats");
        assert_eq!(img.rgb, vec![LINEAR[0]; 257]);
    }

    #[test]
    fn every_truncated_prefix_of_bounded_fixtures_is_rejected() {
        let mut checked = 0;
        for encoding in [
            Encoding::Flat,
            Encoding::Old,
            Encoding::AdaptiveLiteral,
            Encoding::AdaptiveRun,
        ] {
            for bottom_up in [false, true] {
                for width in [8, 129, 257] {
                    let (bytes, expected) = fixture(width, bottom_up, encoding);
                    assert_eq!(decode(&bytes).expect("complete fixture"), expected);
                    for end in 0..bytes.len() {
                        assert_format_error(decode(&bytes[..end]), "");
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 10_000, "all bounded prefixes exercised: {checked}");
    }

    #[test]
    fn crlf_headers_and_exposure_metadata_preserve_pixel_values() {
        for encoding in [
            Encoding::Flat,
            Encoding::Old,
            Encoding::AdaptiveLiteral,
            Encoding::AdaptiveRun,
        ] {
            for bottom_up in [false, true] {
                let (bytes, expected) = fixture(8, bottom_up, encoding);
                let data_start = header(8, 3, bottom_up).len();
                for magic in ["#?RADIANCE", "#?RGBE"] {
                    for newline in ["\n", "\r\n"] {
                        for exposure in ["", "EXPOSURE=4.0\n", "EXPOSURE=0.25\nEXPOSURE=8\n"] {
                            let y = if bottom_up { "+Y" } else { "-Y" };
                            let text = format!("{magic}\nFORMAT=32-bit_rle_rgbe\n{exposure}\n{y} 3 +X 8\n");
                            let mut variant = text.replace('\n', newline).into_bytes();
                            variant.extend_from_slice(&bytes[data_start..]);
                            assert_eq!(decode(&variant).expect("header variant"), expected);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unsupported_headers_and_bounded_hostile_resolutions_have_reasons() {
        for res in [
            "-Y 1 -X 1",
            "+X 1 +Y 1",
            "-X 1 -Y 1",
            "-Y 1 +X 1 extra",
            "-Y nope +X 1",
            "-Y -1 +X 1",
            "-Y 1 +X 4294967296",
            "-Y 1",
            "",
        ] {
            let bytes = format!("#?RGBE\n\n{res}\n");
            assert_format_error(decode(bytes.as_bytes()), "resolution line");
        }
        for (w, h) in [(0, 1), (1, 0), (32769, 1), (1, 32769), (u32::MAX, u32::MAX)] {
            assert_format_error(decode(&header(w as usize, h as usize, false)), "pixels");
        }
        for format in ["32-bit_rle_xyze", "32-bit_rgbe", "unknown"] {
            let bytes = format!("#?RGBE\nFORMAT={format}\n\n-Y 1 +X 1\n");
            assert_format_error(decode(bytes.as_bytes()), "only 32-bit_rle_rgbe");
        }
        // Pre-existing row-wise allocation guard (8ba769d42): a legal maximum resolution with
        // no data or only one short row must not reserve width*height pixels (4 GiB of RGBE).
        assert_format_error(decode(&file(32768, 32768, &[])), "truncated");
        assert_format_error(decode(&file(32768, 32768, &[128; 12])), "truncated");
        // Pass the minimum row-byte guard, but fail decoding the first row. Even this must
        // allocate at most one bounded row, not the declared billion-pixel frame.
        let mut data = vec![0; 4 * 32768];
        data[..8].copy_from_slice(&[128, 64, 32, 129, 1, 1, 1, 255]);
        data[8..12].copy_from_slice(&[1, 1, 1, 255]);
        assert_format_error(decode(&file(32768, 32768, &data)), "old-style run");
        let res = format!("-Y {} +X 1", "9".repeat(1024));
        assert_format_error(decode(format!("#?RGBE\n\n{res}\n").as_bytes()), "resolution line");
    }
}
