// SPDX-License-Identifier: MPL-2.0
//! Minimal OpenEXR 2 scanline writer/reader: uncompressed, `FLOAT` channels, one part.
//!
//! Written from the public OpenEXR file-layout document ("OpenEXR File Layout", Academy Software
//! Foundation): magic + version, a header of typed attributes ending in a null byte, a line
//! offset table, then one chunk per scanline (y, byte count, channel planes in alphabetical
//! channel order). The `image` crate is pipeline-only (`docs/06` § 7), and the reference output
//! needs 32-bit floats anyway: sun-disc radiance (~1e9 cd/m²) overflows half floats.
//!
//! The reader accepts what [`write()`] produces (and other uncompressed FLOAT/HALF scanline files);
//! compressed files are rejected with a clear error.

use std::io::Write;

/// One named channel plane, row-major, top row first.
#[derive(Clone, Debug, PartialEq)]
pub struct ExrChannel {
    /// Channel name, e.g. `beauty.R` (layers use `layer.channel`).
    pub name: String,
    /// `width × height` samples.
    pub data: Vec<f32>,
}

/// A decoded image.
#[derive(Clone, Debug, PartialEq)]
pub struct ExrImage {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Channels, sorted by name.
    pub channels: Vec<ExrChannel>,
    /// String attributes (metadata) found in the header.
    pub attributes: Vec<(String, String)>,
}

impl ExrImage {
    /// The plane called `name`.
    #[must_use]
    pub fn channel(&self, name: &str) -> Option<&[f32]> {
        self.channels
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.data.as_slice())
    }

    /// RGB of layer `layer` (`layer.R/G/B`, or plain `R/G/B` for an empty layer name).
    #[must_use]
    pub fn rgb(&self, layer: &str) -> Option<Vec<[f32; 3]>> {
        let name = |c: &str| {
            if layer.is_empty() {
                c.to_string()
            } else {
                format!("{layer}.{c}")
            }
        };
        let r = self.channel(&name("R"))?;
        let g = self.channel(&name("G"))?;
        let b = self.channel(&name("B"))?;
        Some(r.iter().zip(g).zip(b).map(|((r, g), b)| [*r, *g, *b]).collect())
    }

    /// Layer names (the part before the last `.` of each channel).
    #[must_use]
    pub fn layers(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .channels
            .iter()
            .map(|c| {
                c.name
                    .rsplit_once('.')
                    .map_or(String::new(), |(l, _)| l.to_string())
            })
            .collect();
        // Each layer once, in first-seen order: another layer's channels can sort between a
        // layer's own (`a.A`, `a.B.x`, `a.R`).
        let mut seen = std::collections::HashSet::new();
        out.retain(|l| seen.insert(l.clone()));
        out
    }
}

/// EXR errors.
#[derive(Debug, thiserror::Error)]
pub enum ExrError {
    /// File IO.
    #[error("exr io: {0}")]
    Io(#[from] std::io::Error),
    /// Not a file this reader understands.
    #[error("exr format: {0}")]
    Format(String),
}

const MAGIC: u32 = 20_000_630;
const PIXEL_HALF: u32 = 1;
const PIXEL_FLOAT: u32 = 2;

fn attr(out: &mut Vec<u8>, name: &str, kind: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(kind.as_bytes());
    out.push(0);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
}

/// Encodes an image. Channels are sorted by name (the file format requires it); `attributes`
/// become `string` header attributes (metadata).
///
/// # Errors
///
/// A channel's sample count is not `width × height`.
pub fn encode(
    width: u32,
    height: u32,
    channels: &[ExrChannel],
    attributes: &[(String, String)],
) -> Result<Vec<u8>, ExrError> {
    if width == 0 || height == 0 {
        return Err(ExrError::Format("zero-sized image".into()));
    }
    if i32::try_from(width).is_err() || i32::try_from(height).is_err() {
        return Err(ExrError::Format("image is too large".into()));
    }
    let n = (width as usize) * (height as usize);
    // The scanline size is a u32 field of the file.
    let line_bytes_wide = (channels.len() as u64) * u64::from(width) * 4;
    if u32::try_from(line_bytes_wide).is_err() {
        return Err(ExrError::Format("a scanline is too large".into()));
    }
    let mut sorted: Vec<&ExrChannel> = channels.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for c in &sorted {
        if c.data.len() != n {
            return Err(ExrError::Format(format!(
                "channel {} has {} samples",
                c.name,
                c.data.len()
            )));
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    let mut chlist = Vec::new();
    for c in &sorted {
        chlist.extend_from_slice(c.name.as_bytes());
        chlist.push(0);
        chlist.extend_from_slice(&PIXEL_FLOAT.to_le_bytes());
        chlist.extend_from_slice(&[0, 0, 0, 0]); // pLinear + reserved
        chlist.extend_from_slice(&1i32.to_le_bytes());
        chlist.extend_from_slice(&1i32.to_le_bytes());
    }
    chlist.push(0);
    attr(&mut out, "channels", "chlist", &chlist);
    attr(&mut out, "compression", "compression", &[0]);
    let mut window = Vec::new();
    for v in [0i32, 0, width as i32 - 1, height as i32 - 1] {
        window.extend_from_slice(&v.to_le_bytes());
    }
    attr(&mut out, "dataWindow", "box2i", &window);
    attr(&mut out, "displayWindow", "box2i", &window);
    attr(&mut out, "lineOrder", "lineOrder", &[0]);
    attr(&mut out, "pixelAspectRatio", "float", &1.0f32.to_le_bytes());
    attr(&mut out, "screenWindowCenter", "v2f", &[0; 8]);
    attr(&mut out, "screenWindowWidth", "float", &1.0f32.to_le_bytes());
    for (k, v) in attributes {
        attr(&mut out, k, "string", v.as_bytes());
    }
    out.push(0);
    let line_bytes = sorted.len() * width as usize * 4;
    let table_start = out.len();
    let first_chunk = table_start + 8 * height as usize;
    for y in 0..height as usize {
        let offset = (first_chunk + y * (8 + line_bytes)) as u64;
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for y in 0..height as usize {
        out.extend_from_slice(&(y as i32).to_le_bytes());
        out.extend_from_slice(&(line_bytes as u32).to_le_bytes());
        for c in &sorted {
            for x in 0..width as usize {
                out.extend_from_slice(&c.data[y * width as usize + x].to_le_bytes());
            }
        }
    }
    Ok(out)
}

/// Writes an image to `path` (see [`encode`]).
///
/// # Errors
///
/// Encoding or file IO failed.
pub fn write(
    path: &std::path::Path,
    width: u32,
    height: u32,
    channels: &[ExrChannel],
    attributes: &[(String, String)],
) -> Result<(), ExrError> {
    let bytes = encode(width, height, channels, attributes)?;
    write_file_atomic(path, &bytes)?;
    Ok(())
}

/// Writes `bytes` to a sibling temporary file and renames it over `path`, so a failed write
/// leaves the previous file intact. The temporary file is removed on failure.
pub(crate) fn write_file_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = std::path::PathBuf::from(tmp);
    let result = std::fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.flush()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ExrError> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
        let end = end.ok_or_else(|| ExrError::Format("truncated file".into()))?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, ExrError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn cstr(&mut self) -> Result<String, ExrError> {
        let rest = &self.bytes[self.at..];
        let len = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| ExrError::Format("unterminated string".into()))?;
        let s = String::from_utf8_lossy(&rest[..len]).into_owned();
        self.at += len + 1;
        Ok(s)
    }
}

fn half_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let mant = u32::from(h & 0x3ff);
    let bits = match (exp, mant) {
        (0, 0) => sign,
        (0, m) => {
            // Subnormal: normalise.
            let mut e = 127 - 15 + 1;
            let mut m = m;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | ((e as u32) << 23) | ((m & 0x3ff) << 13)
        }
        (31, 0) => sign | 0x7f80_0000,
        (31, m) => sign | 0x7f80_0000 | (m << 13),
        (e, m) => sign | ((e + 127 - 15) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

/// Largest decoded image, in pixels per channel (8192 × 8192 at 2^26).
const MAX_PIXELS: u64 = 1 << 26;

/// Decodes an uncompressed scanline EXR.
///
/// # Errors
///
/// The bytes are not an uncompressed single-part scanline EXR with HALF/FLOAT channels.
pub fn decode(bytes: &[u8]) -> Result<ExrImage, ExrError> {
    let mut c = Cursor { bytes, at: 0 };
    if c.u32()? != MAGIC {
        return Err(ExrError::Format("not an OpenEXR file".into()));
    }
    let version = c.u32()?;
    if version & 0x200 != 0 || version & 0x1000 != 0 {
        return Err(ExrError::Format(
            "tiled or multi-part EXR is not supported".into(),
        ));
    }
    let mut channels: Vec<(String, u32)> = Vec::new();
    let mut window = None;
    let mut attributes = Vec::new();
    loop {
        let name = c.cstr()?;
        if name.is_empty() {
            break;
        }
        let kind = c.cstr()?;
        let size = c.u32()? as usize;
        let value = c.take(size)?;
        match (name.as_str(), kind.as_str()) {
            ("channels", "chlist") => {
                let mut v = Cursor { bytes: value, at: 0 };
                loop {
                    let cname = v.cstr()?;
                    if cname.is_empty() {
                        break;
                    }
                    let ptype = v.u32()?;
                    v.take(12)?;
                    channels.push((cname, ptype));
                }
            }
            ("compression", _) => {
                if value.first().copied() != Some(0) {
                    return Err(ExrError::Format("compressed EXR is not supported".into()));
                }
            }
            ("dataWindow", "box2i") if value.len() == 16 => {
                let i = |k: usize| i32::from_le_bytes([value[k], value[k + 1], value[k + 2], value[k + 3]]);
                window = Some((i(0), i(4), i(8), i(12)));
            }
            (_, "string") => attributes.push((name, String::from_utf8_lossy(value).into_owned())),
            _ => {}
        }
    }
    let (x0, y0, x1, y1) = window.ok_or_else(|| ExrError::Format("no dataWindow".into()))?;
    let extent = |lo: i32, hi: i32| {
        i64::from(hi)
            .checked_sub(i64::from(lo))
            .map_or(0, |d| d + 1)
            .clamp(0, i64::from(u32::MAX)) as u32
    };
    let width = extent(x0, x1);
    let height = extent(y0, y1);
    if width == 0 || height == 0 {
        return Err(ExrError::Format("empty data window".into()));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        return Err(ExrError::Format("data window is too large".into()));
    }
    // A scanline offset table needs 8 bytes per row: refuse before reserving anything.
    if (bytes.len().saturating_sub(c.at) as u64) / 8 < u64::from(height) {
        return Err(ExrError::Format("truncated file".into()));
    }
    if channels
        .iter()
        .any(|(_, t)| *t != PIXEL_FLOAT && *t != PIXEL_HALF)
    {
        return Err(ExrError::Format(
            "only HALF and FLOAT channels are supported".into(),
        ));
    }
    let mut table = Vec::with_capacity(height as usize);
    for _ in 0..height {
        let b = c.take(8)?;
        table.push(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as usize);
    }
    // Uncompressed data holds every sample in the file: a file shorter than that is truncated, and
    // refusing it here keeps a small header from reserving gigabytes.
    let sample_bytes: u64 = channels
        .iter()
        .map(|(_, t)| if *t == PIXEL_FLOAT { 4u64 } else { 2 })
        .sum();
    if pixels.saturating_mul(sample_bytes) > bytes.len() as u64 {
        return Err(ExrError::Format("truncated file".into()));
    }
    let mut planes: Vec<Vec<f32>> = vec![vec![0.0; pixels as usize]; channels.len()];
    // The table has one entry per row: a row named twice means another is missing (it would
    // decode as zeros), so each row must come exactly once.
    let mut seen = vec![false; height as usize];
    for offset in table {
        let mut line = Cursor { bytes, at: offset };
        let y = i64::from(line.u32()? as i32) - i64::from(y0);
        let _size = line.u32()?;
        if y < 0 || y >= i64::from(height) {
            return Err(ExrError::Format("scanline outside the data window".into()));
        }
        if std::mem::replace(&mut seen[y as usize], true) {
            return Err(ExrError::Format(format!(
                "scanline {} appears twice",
                y + i64::from(y0)
            )));
        }
        for (ci, (_, ptype)) in channels.iter().enumerate() {
            for x in 0..width as usize {
                let v = if *ptype == PIXEL_FLOAT {
                    let b = line.take(4)?;
                    f32::from_le_bytes([b[0], b[1], b[2], b[3]])
                } else {
                    let b = line.take(2)?;
                    half_to_f32(u16::from_le_bytes([b[0], b[1]]))
                };
                planes[ci][y as usize * width as usize + x] = v;
            }
        }
    }
    let channels = channels
        .into_iter()
        .zip(planes)
        .map(|((name, _), data)| ExrChannel { name, data })
        .collect();
    Ok(ExrImage {
        width,
        height,
        channels,
        attributes,
    })
}

/// Reads an EXR file (see [`decode`]).
///
/// # Errors
///
/// File IO or decoding failed.
pub fn read(path: &std::path::Path) -> Result<ExrImage, ExrError> {
    decode(&std::fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_layers_and_metadata() {
        let (w, h) = (5u32, 3u32);
        let plane = |k: f32| {
            (0..w * h)
                .map(|i| i as f32 * k + 1e9 * (k - 1.0))
                .collect::<Vec<_>>()
        };
        let channels = vec![
            ExrChannel {
                name: "beauty.R".into(),
                data: plane(1.0),
            },
            ExrChannel {
                name: "beauty.G".into(),
                data: plane(2.0),
            },
            ExrChannel {
                name: "beauty.B".into(),
                data: plane(3.0),
            },
            ExrChannel {
                name: "depth.Z".into(),
                data: plane(0.5),
            },
        ];
        let meta = vec![("regia.seed".to_string(), "7".to_string())];
        let bytes = encode(w, h, &channels, &meta).expect("encode");
        let img = decode(&bytes).expect("decode");
        assert_eq!((img.width, img.height), (w, h));
        assert_eq!(img.channel("beauty.G"), Some(channels[1].data.as_slice()));
        assert_eq!(img.channel("depth.Z"), Some(channels[3].data.as_slice()));
        assert_eq!(img.attributes, meta);
        assert_eq!(img.layers(), vec!["beauty".to_string(), "depth".to_string()]);
        let rgb = img.rgb("beauty").expect("rgb");
        assert_eq!(rgb[4], [4.0, 1e9 + 8.0, 2e9 + 12.0]);
    }

    /// audit-v034b-formats L4: an offset table that names one scanline twice decoded as a valid
    /// image with a black row where the missing scanline should be.
    #[test]
    fn a_scanline_named_twice_is_an_error() {
        let (w, h) = (2u32, 3u32);
        let channels = vec![ExrChannel {
            name: "Y".into(),
            data: vec![1.0; (w * h) as usize],
        }];
        let mut bytes = encode(w, h, &channels, &[]).expect("encode");
        let u64_at = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
        // The table is where the first entry points just past the table itself.
        let table = (0..bytes.len() - 8)
            .find(|&i| u64_at(&bytes, i) == (i + 8 * h as usize) as u64)
            .expect("offset table");
        decode(&bytes).expect("the intact file decodes");
        let first = bytes[table..table + 8].to_vec();
        bytes[table + 16..table + 24].copy_from_slice(&first);
        let err = decode(&bytes).expect_err("row 0 twice, row 2 never");
        assert!(err.to_string().contains("scanline"), "{err}");
    }

    /// A minimal header with the given `dataWindow` and no scanlines.
    fn header_with_window(win: [i32; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        let mut chlist = b"Y\0".to_vec();
        chlist.extend_from_slice(&PIXEL_FLOAT.to_le_bytes());
        chlist.extend_from_slice(&[0; 4]);
        chlist.extend_from_slice(&1i32.to_le_bytes());
        chlist.extend_from_slice(&1i32.to_le_bytes());
        chlist.push(0);
        attr(&mut out, "channels", "chlist", &chlist);
        attr(&mut out, "compression", "compression", &[0]);
        let mut window = Vec::new();
        for v in win {
            window.extend_from_slice(&v.to_le_bytes());
        }
        attr(&mut out, "dataWindow", "box2i", &window);
        out.push(0);
        out
    }

    /// A header declaring `n` FLOAT channels over `win`, no scanlines.
    fn header_with_channels(n: usize, win: [i32; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        let mut chlist = Vec::new();
        for i in 0..n {
            chlist.extend_from_slice(format!("c{i:05}\0").as_bytes());
            chlist.extend_from_slice(&PIXEL_FLOAT.to_le_bytes());
            chlist.extend_from_slice(&[0; 4]);
            chlist.extend_from_slice(&1i32.to_le_bytes());
            chlist.extend_from_slice(&1i32.to_le_bytes());
        }
        chlist.push(0);
        attr(&mut out, "channels", "chlist", &chlist);
        attr(&mut out, "compression", "compression", &[0]);
        let mut window = Vec::new();
        for v in win {
            window.extend_from_slice(&v.to_le_bytes());
        }
        attr(&mut out, "dataWindow", "box2i", &window);
        out.push(0);
        // An offset table that fits, so only the plane budget can refuse the file.
        out.extend(std::iter::repeat_n(0u8, 8 * 8192 + 64));
        out
    }

    #[test]
    fn channel_count_and_plane_size_are_bounded_before_allocating() {
        // 4000 channels over 8192 x 8192 would reserve terabytes.
        let many = header_with_channels(4000, [0, 0, 8191, 8191]);
        assert!(matches!(decode(&many), Err(ExrError::Format(_))));
        // Few channels but far more samples than the file could hold.
        let big = header_with_channels(8, [0, 0, 8191, 8191]);
        assert!(matches!(decode(&big), Err(ExrError::Format(_))));
    }

    #[test]
    fn encode_refuses_a_scanline_that_overflows_its_u32_size() {
        let c = ExrChannel {
            name: "Y".into(),
            data: Vec::new(),
        };
        let err = encode(1 << 30, 1, &[c], &[]).expect_err("2^32 byte scanline");
        assert!(matches!(err, ExrError::Format(m) if m.contains("scanline")));
    }

    #[test]
    fn hostile_data_windows_are_errors() {
        for win in [
            [0, 0, 65535, 65535],
            [0, 0, 46339, 46339],
            [0, 0, i32::MAX, i32::MAX],
            [i32::MIN, i32::MIN, i32::MAX, i32::MAX],
            [0, 0, 3, i32::MAX],
        ] {
            assert!(decode(&header_with_window(win)).is_err(), "{win:?}");
        }
        // A sane window with no offset table is truncated, not a panic.
        assert!(decode(&header_with_window([0, 0, 3, 3])).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"not an exr").is_err());
        assert!(encode(0, 1, &[], &[]).is_err());
    }

    #[test]
    fn half_decoding() {
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x7bff), 65504.0);
        assert!((half_to_f32(0x0001) - 5.960_464_5e-8).abs() < 1e-12);
    }

    #[test]
    fn many_channels_with_missing_pixels_are_an_error_not_an_allocation() {
        // Header: a 4096 x 4096 window with 256 FLOAT channels, then a full offset table but no
        // pixel data. The decoder must refuse before reserving 256 planes.
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        let mut chlist = Vec::new();
        for k in 0..256 {
            chlist.extend_from_slice(format!("c{k:03}\0").as_bytes());
            chlist.extend_from_slice(&PIXEL_FLOAT.to_le_bytes());
            chlist.extend_from_slice(&[0; 4]);
            chlist.extend_from_slice(&1i32.to_le_bytes());
            chlist.extend_from_slice(&1i32.to_le_bytes());
        }
        chlist.push(0);
        attr(&mut out, "channels", "chlist", &chlist);
        attr(&mut out, "compression", "compression", &[0]);
        let mut window = Vec::new();
        for v in [0i32, 0, 4095, 4095] {
            window.extend_from_slice(&v.to_le_bytes());
        }
        attr(&mut out, "dataWindow", "box2i", &window);
        out.push(0);
        out.extend_from_slice(&vec![0u8; 4096 * 8]);
        assert!(matches!(decode(&out), Err(ExrError::Format(m)) if m.contains("truncated")));
    }
}
