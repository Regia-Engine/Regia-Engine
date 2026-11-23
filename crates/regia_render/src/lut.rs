// SPDX-License-Identifier: MPL-2.0
//! 3D colour lookup tables for the per-map grade (`docs/specs/13-renderer.md` § 5 step 12).
//!
//! A LUT maps the display-referred colour after the tonemap and the parametric grade to the
//! final display colour. It is applied by trilinear lookup in a 3D texture (identity by
//! default, so a view without one is unchanged). Two authoring forms load:
//!
//! - **`.cube`** (the Adobe/IRIDAS Cube LUT text format, public specification v1.0):
//!   `LUT_3D_SIZE N`, optional `DOMAIN_MIN` / `DOMAIN_MAX`, then N³ `r g b` lines with red
//!   varying fastest.
//! - **PNG strip**: N slices of N×N laid side by side (N² wide, N tall); x = r + b·N, y = g.

use std::sync::Arc;

/// A loaded 3D LUT with an identity key, cheap to compare and to print.
#[derive(Clone)]
pub struct ColorLut {
    size: u32,
    /// `size³` entries, red fastest, then green, then blue.
    data: Vec<[f32; 3]>,
    key: u64,
}

/// Why a LUT did not load.
#[derive(Debug, thiserror::Error)]
pub enum LutError {
    /// The `.cube` text is malformed.
    #[error("cube LUT: {0}")]
    Cube(String),
    /// The PNG strip is not N² × N RGBA.
    #[error("LUT strip: {0}")]
    Strip(String),
    /// Reading the file failed.
    #[error("LUT file {path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// The error.
        source: std::io::Error,
    },
}

impl std::fmt::Debug for ColorLut {
    // Short on purpose: post settings are compared and hashed through their Debug text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ColorLut({}³, {:016x})", self.size, self.key)
    }
}

impl PartialEq for ColorLut {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size && self.key == other.key
    }
}

impl ColorLut {
    /// The largest edge accepted (a 64³ cube is already finer than the 8-bit output).
    pub const MAX_SIZE: u32 = 64;

    /// Largest `.cube` or `.png` file [`Self::load`] reads, in bytes.
    pub const MAX_FILE_BYTES: u64 = 16 << 20;

    /// Builds a LUT of edge `size` from `f`, evaluated at the grid points in `0..=1`.
    #[must_use]
    pub fn from_fn(size: u32, f: impl Fn([f32; 3]) -> [f32; 3]) -> Self {
        let size = size.clamp(2, Self::MAX_SIZE);
        let n = (size - 1) as f32;
        let mut data = Vec::with_capacity((size * size * size) as usize);
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    data.push(f([r as f32 / n, g as f32 / n, b as f32 / n]));
                }
            }
        }
        Self::from_data(size, data)
    }

    /// The identity LUT of edge `size`.
    #[must_use]
    pub fn identity(size: u32) -> Self {
        Self::from_fn(size, |c| c)
    }

    /// A LUT of edge `size` from its `size³` entries (red fastest, then green, then blue), `None`
    /// when the size is outside `2..=`[`Self::MAX_SIZE`] or the count does not match. The key
    /// is the content hash, so equal tables compare equal however they were built.
    #[must_use]
    pub fn from_entries(size: u32, data: Vec<[f32; 3]>) -> Option<Self> {
        let n = size as usize;
        ((2..=Self::MAX_SIZE).contains(&size) && data.len() == n * n * n).then(|| Self::from_data(size, data))
    }

    fn from_data(size: u32, data: Vec<[f32; 3]>) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        size.hash(&mut hasher);
        for c in &data {
            for v in c {
                v.to_bits().hash(&mut hasher);
            }
        }
        Self {
            size,
            data,
            key: hasher.finish(),
        }
    }

    /// Edge length.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Entries, red fastest.
    #[must_use]
    pub fn data(&self) -> &[[f32; 3]] {
        &self.data
    }

    /// Content hash.
    #[must_use]
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Parses Cube LUT text (3D only; a `LUT_1D_SIZE` file is rejected).
    ///
    /// # Errors
    ///
    /// [`LutError::Cube`] on a missing or out-of-range size, a bad line, or a wrong entry count.
    pub fn parse_cube(text: &str) -> Result<Self, LutError> {
        // A UTF-8 BOM (a Notepad save) is not whitespace: strip one before the line scan.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut size = 0u32;
        let mut min = [0.0f32; 3];
        let mut max = [1.0f32; 3];
        let mut data = Vec::new();
        let triple = |words: &[&str], line: usize| -> Result<[f32; 3], LutError> {
            if words.len() != 3 {
                return Err(LutError::Cube(format!("line {line}: expected three numbers")));
            }
            let mut out = [0.0; 3];
            for (o, w) in out.iter_mut().zip(words) {
                *o = w
                    .parse()
                    .map_err(|_| LutError::Cube(format!("line {line}: `{w}` is not a number")))?;
            }
            Ok(out)
        };
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let words: Vec<&str> = line.split_whitespace().collect();
            match words[0] {
                "TITLE" => {}
                "LUT_1D_SIZE" => return Err(LutError::Cube("1D LUTs are not supported".into())),
                "LUT_3D_SIZE" => {
                    size = words
                        .get(1)
                        .and_then(|w| w.parse().ok())
                        .filter(|s| (2..=Self::MAX_SIZE).contains(s))
                        .ok_or_else(|| LutError::Cube(format!("line {}: bad LUT_3D_SIZE", index + 1)))?;
                }
                "DOMAIN_MIN" => min = triple(&words[1..], index + 1)?,
                "DOMAIN_MAX" => max = triple(&words[1..], index + 1)?,
                _ => {
                    // The size line must come first; the table never grows past `size³`.
                    if size == 0 || data.len() >= (size * size * size) as usize {
                        return Err(LutError::Cube(format!(
                            "line {}: data before LUT_3D_SIZE or more than size³ entries",
                            index + 1
                        )));
                    }
                    data.push(triple(&words, index + 1)?);
                }
            }
        }
        if size == 0 {
            return Err(LutError::Cube("no LUT_3D_SIZE".into()));
        }
        if data.len() != (size * size * size) as usize {
            return Err(LutError::Cube(format!(
                "{} entries for a {size}³ LUT (expected {})",
                data.len(),
                size * size * size
            )));
        }
        // `DOMAIN_MIN` / `DOMAIN_MAX` bound the input lattice; the table lines are outputs and
        // stay as authored. The shader (and the post-volume blend, which mixes LUTs entry by
        // entry) indexes a 0..1 grid, so a non-unit domain is resampled onto it at the finest
        // edge: grid point `x` takes the authored table at `(x − min) / (max − min)`, clamped
        // to the domain's edge (an input outside the domain clamps).
        if (0..3).any(|k| !(min[k].is_finite() && max[k].is_finite() && min[k] < max[k])) {
            return Err(LutError::Cube(format!(
                "DOMAIN_MIN {min:?} must be below DOMAIN_MAX {max:?} on every channel"
            )));
        }
        if min == [0.0; 3] && max == [1.0; 3] {
            return Ok(Self::from_data(size, data));
        }
        let authored = Self::from_data(size, data);
        Ok(Self::from_fn(Self::MAX_SIZE, |x| {
            authored.sample(std::array::from_fn(|k| (x[k] - min[k]) / (max[k] - min[k])))
        }))
    }

    /// A strip of `n` slices of `n × n` RGBA8 (sRGB-coded display values as stored), laid side
    /// by side: `width = n²`, `height = n`, x = r + b·n, y = g.
    ///
    /// # Errors
    ///
    /// [`LutError::Strip`] when the shape is not `n² × n` or the bytes are short.
    pub fn from_strip_rgba8(width: u32, height: u32, rgba: &[u8]) -> Result<Self, LutError> {
        let n = height;
        if !(2..=Self::MAX_SIZE).contains(&n) || width != n * n {
            return Err(LutError::Strip(format!("{width}×{height} is not N²×N")));
        }
        if rgba.len() < (width * height * 4) as usize {
            return Err(LutError::Strip("short pixel data".into()));
        }
        let mut data = Vec::with_capacity((n * n * n) as usize);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let i = ((g * width + b * n + r) * 4) as usize;
                    data.push([
                        f32::from(rgba[i]) / 255.0,
                        f32::from(rgba[i + 1]) / 255.0,
                        f32::from(rgba[i + 2]) / 255.0,
                    ]);
                }
            }
        }
        Ok(Self::from_data(n, data))
    }

    /// Decodes a PNG strip (see [`Self::from_strip_rgba8`]).
    ///
    /// # Errors
    ///
    /// [`LutError::Strip`] when the PNG does not decode or has the wrong shape.
    pub fn from_png_strip(bytes: &[u8]) -> Result<Self, LutError> {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder
            .read_info()
            .map_err(|e| LutError::Strip(format!("png: {e}")))?;
        // The shape before the buffer (ADR-5869): the png crate holds only one row to its memory
        // limit, so a 70-byte file whose header claims millions of rows asked for terabytes here.
        let (w, h) = (reader.info().width, reader.info().height);
        if !(2..=Self::MAX_SIZE).contains(&h) || w != h * h {
            return Err(LutError::Strip(format!("{w}×{h} is not N²×N")));
        }
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| LutError::Strip("png too large".into()))?;
        let mut buf = vec![0; size];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| LutError::Strip(format!("png: {e}")))?;
        let (w, h) = (info.width, info.height);
        let rgba: Vec<u8> = match (info.color_type, info.bit_depth) {
            (png::ColorType::Rgba, png::BitDepth::Eight) => buf[..(w * h * 4) as usize].to_vec(),
            (png::ColorType::Rgb, png::BitDepth::Eight) => buf[..(w * h * 3) as usize]
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|c| [c[0], c[1], c[2], 255])
                .collect(),
            other => return Err(LutError::Strip(format!("unsupported PNG layout {other:?}"))),
        };
        Self::from_strip_rgba8(w, h, &rgba)
    }

    /// Loads `.cube` or `.png` by extension.
    ///
    /// # Errors
    ///
    /// [`LutError::Io`] when the file cannot be read; the parser's error otherwise.
    pub fn load(path: &std::path::Path) -> Result<Arc<Self>, LutError> {
        let io = |source| LutError::Io {
            path: path.display().to_string(),
            source,
        };
        let is_png = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("png"));
        // A 64³ cube is well under 16 MiB; a larger file (or a device node) is refused unread.
        use std::io::Read as _;
        let file = std::fs::File::open(path).map_err(io)?;
        let mut bytes = Vec::new();
        file.take(Self::MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 > Self::MAX_FILE_BYTES {
            return Err(LutError::Cube(format!(
                "{} is larger than {} bytes",
                path.display(),
                Self::MAX_FILE_BYTES
            )));
        }
        let lut = if is_png {
            Self::from_png_strip(&bytes)?
        } else {
            let text = String::from_utf8(bytes)
                .map_err(|_| LutError::Cube(format!("{} is not UTF-8 text", path.display())))?;
            Self::parse_cube(&text)?
        };
        Ok(Arc::new(lut))
    }

    /// Trilinear lookup of a display colour in `0..1` (the shader's lookup, on the CPU).
    #[must_use]
    pub fn sample(&self, c: [f32; 3]) -> [f32; 3] {
        let n = self.size as usize;
        let f = (n - 1) as f32;
        let p = c.map(|v| v.clamp(0.0, 1.0) * f);
        let i0 = p.map(|v| (v.floor() as usize).min(n - 1));
        let i1 = i0.map(|v| (v + 1).min(n - 1));
        let t = [p[0] - i0[0] as f32, p[1] - i0[1] as f32, p[2] - i0[2] as f32];
        let at = |r: usize, g: usize, b: usize| self.data[(b * n + g) * n + r];
        let mut out = [0.0; 3];
        for (k, o) in out.iter_mut().enumerate() {
            let c00 = at(i0[0], i0[1], i0[2])[k] * (1.0 - t[0]) + at(i1[0], i0[1], i0[2])[k] * t[0];
            let c10 = at(i0[0], i1[1], i0[2])[k] * (1.0 - t[0]) + at(i1[0], i1[1], i0[2])[k] * t[0];
            let c01 = at(i0[0], i0[1], i1[2])[k] * (1.0 - t[0]) + at(i1[0], i0[1], i1[2])[k] * t[0];
            let c11 = at(i0[0], i1[1], i1[2])[k] * (1.0 - t[0]) + at(i1[0], i1[1], i1[2])[k] * t[0];
            let c0 = c00 * (1.0 - t[1]) + c10 * t[1];
            let c1 = c01 * (1.0 - t[1]) + c11 * t[1];
            *o = c0 * (1.0 - t[2]) + c1 * t[2];
        }
        out
    }
}

/// Loads a LUT named the way a level look or a post volume names one (`docs/formats/scene-files.md`,
/// `docs/formats/post-volume-v3.md`): a project-relative `.cube` or `.png` path such as
/// `looks/hall.cube`, where `\` is accepted as a separator too (a level authored on Windows),
/// or an absolute path to a loose file (used as written, so a drive path stays absolute only
/// on the OS that has drives).
///
/// Resolution order for a relative path:
///
/// 1. The asset server's manifest (`assets/<path>` then `<path>`): a `.cube` cooked into a
///    pak or the dev cache, the only route a packaged Android client has (its content
///    directory holds paks, not loose files; ADR-4830). A loaded LUT is not hot-reloaded: no
///    LUT cache compares the asset generation, so an edited `.cube` is picked up when the
///    level or volume next names it (the live cooker re-cooks a saved `.cube`, and the
///    level/editor load path re-reads it), not per frame.
/// 2. A loose file under the asset server's project root (`<root>/<path>`, then
///    `<root>/assets/<path>`), the dev run and the desktop package with raw `assets/`.
///
/// A `.png` strip only loads loose: a cooked `.png` is a texture, not PNG bytes.
///
/// # Errors
///
/// [`LutError::Io`] when no candidate reads; the parser's error otherwise. A `.cube` the
/// manifest holds but whose cooked bytes do not parse is an error (a readable cooked entry is
/// authoritative); a missing manifest entry, or one whose cooked bytes cannot be read (a pruned
/// cache, an interrupted cook), falls through to the loose files.
pub fn load_named(
    path: &str,
    mut assets: Option<&mut regia_asset::AssetServer>,
) -> Result<Arc<ColorLut>, LutError> {
    let rel = canonical_lut_rel(path);
    if std::path::Path::new(&rel).is_absolute() {
        return ColorLut::load(std::path::Path::new(&rel));
    }
    let is_cube = std::path::Path::new(&rel)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cube"));
    if is_cube && let Some(server) = assets.as_deref_mut() {
        for key in [format!("assets/{rel}"), rel.clone()] {
            let Some(id) = server.id_for_path(&key) else {
                continue;
            };
            server.track_external(id);
            let (bytes, _) = match server.read_cooked(id) {
                Ok(read) => read,
                // A manifest hit whose bytes cannot be read (a pruned cache, an interrupted
                // cook) behaves like a missing entry: try the next key, then the loose
                // candidates. Errors about bytes that did read stay errors.
                Err(regia_asset::AssetLoadError::Io(_))
                | Err(regia_asset::AssetLoadError::Cooked(regia_asset::CookedIoError::Io(_))) => {
                    continue;
                }
                Err(e) => {
                    return Err(LutError::Io {
                        path: key.clone(),
                        source: std::io::Error::other(e.to_string()),
                    });
                }
            };
            let (_, payload) =
                regia_asset::decode_cooked_file(&bytes).map_err(|e| LutError::Cube(format!("{key}: {e}")))?;
            // The cap bounds the payload (the source `.cube` text), not the cooked envelope:
            // the loose load caps the same bytes, so a LUT exactly at the cap loads both ways.
            if payload.len() as u64 > ColorLut::MAX_FILE_BYTES {
                return Err(LutError::Cube(format!(
                    "{key} is larger than {} bytes",
                    ColorLut::MAX_FILE_BYTES
                )));
            }
            let text = std::str::from_utf8(payload)
                .map_err(|_| LutError::Cube(format!("{key} is not UTF-8 text")))?;
            return Ok(Arc::new(ColorLut::parse_cube(text)?));
        }
    }
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(server) = assets.as_deref() {
        candidates.push(server.project_root().join(&rel));
        // `assets/<rel>`, else the earliest overlay root that holds it (ADR-8845).
        candidates.push(server.source_path(&rel));
    } else {
        candidates.push(std::path::PathBuf::from(&rel));
    }
    let mut last = None;
    for candidate in &candidates {
        match ColorLut::load(candidate) {
            Ok(lut) => return Ok(lut),
            Err(err) => last = Some(err),
        }
    }
    Err(last.expect("at least one candidate path"))
}

/// The project-relative key [`load_named`] looks up. Separators are normalised, leading
/// `assets://`, `./` and `assets/` prefixes are removed, and `.` segments are dropped the
/// same way `regia_asset::normalise_asset_path` drops them, so the cooked manifest key matches.
fn canonical_lut_rel(path: &str) -> String {
    let mut rel = path.trim().replace('\\', "/");
    loop {
        let next = ["assets://", "./", "assets/"]
            .into_iter()
            .find_map(|prefix| rel.strip_prefix(prefix).map(str::to_string));
        match next {
            Some(rest) => rel = rest,
            None => break,
        }
    }
    regia_asset::normalise_asset_path(std::path::Path::new(&rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG of `width × height` RGBA8 whose image data is one empty stored zlib block: enough for
    /// the header to be read, nothing to decode.
    fn png_claiming(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &b in bytes {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32(&body).to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, deflate, adaptive, no interlace
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(
            &mut out,
            b"IDAT",
            &[0x78, 0x01, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01],
        );
        chunk(&mut out, b"IEND", &[]);
        out
    }

    /// sec-review-assets-agent (ADR-5869): `from_png_strip` allocated the whole image from the
    /// header before checking its shape, and the png crate holds only one row to its limit: a
    /// 4,000,000 × 1,000,000 header asked for 16 TB (an allocation-failure abort).
    #[test]
    fn a_png_header_claiming_a_huge_image_is_refused_before_allocating() {
        let bytes = png_claiming(4_000_000, 1_000_000);
        assert!(bytes.len() < 100);
        match ColorLut::from_png_strip(&bytes) {
            Err(LutError::Strip(msg)) => assert!(msg.contains("is not N²×N"), "{msg}"),
            other => panic!("expected the shape refusal, got {:?}", other.map(|_| ())),
        }
    }

    /// A `.` segment is not part of the cooked manifest key (`normalise_asset_path` drops it).
    /// A packaged client has no loose file, so `looks/./hall.cube` must still find
    /// `assets/looks/hall.cube`.
    #[test]
    fn a_dot_segment_resolves_to_the_manifest_key() {
        assert_eq!(canonical_lut_rel("looks/./hall.cube"), "looks/hall.cube");
        assert_eq!(canonical_lut_rel("././looks/hall.cube"), "looks/hall.cube");
        assert_eq!(canonical_lut_rel(r".\looks\hall.cube"), "looks/hall.cube");
    }

    #[test]
    fn identity_round_trips_and_cube_parses() {
        let id = ColorLut::identity(32);
        for c in [[0.0, 0.0, 0.0], [0.3, 0.6, 0.9], [1.0, 0.5, 0.25]] {
            let s = id.sample(c);
            for k in 0..3 {
                assert!((s[k] - c[k]).abs() < 1.0e-5, "{c:?} -> {s:?}");
            }
        }
        let mut text = String::from("TITLE \"swap\"\n# comment\nLUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    // Swap red and blue.
                    text.push_str(&format!("{b} {g} {r}\n"));
                }
            }
        }
        let swap = ColorLut::parse_cube(&text).expect("parses");
        let s = swap.sample([1.0, 0.0, 0.0]);
        assert!((s[2] - 1.0).abs() < 1.0e-5 && s[0].abs() < 1.0e-5, "{s:?}");
        assert!(
            ColorLut::parse_cube("LUT_3D_SIZE 2\n0 0 0\n").is_err(),
            "short data"
        );
        assert!(ColorLut::parse_cube("LUT_1D_SIZE 4\n").is_err());
        assert_ne!(swap, ColorLut::identity(2));
        assert_eq!(ColorLut::identity(2), ColorLut::identity(2));
    }

    /// `DOMAIN_MIN` / `DOMAIN_MAX` bound the input lattice; the table lines are outputs and
    /// stay as authored (Cube LUT spec v1.0). An input outside the domain clamps to its edge
    /// (audit render10 #2).
    #[test]
    fn the_cube_domain_maps_inputs_and_leaves_outputs_as_authored() {
        let cube = |domain: &str| {
            let mut text = format!("LUT_3D_SIZE 2\n{domain}");
            for b in 0..2 {
                for g in 0..2 {
                    for r in 0..2 {
                        // Identity outputs, except the blue corners are 0.5.
                        text.push_str(&format!("{r} {g} {}\n", f32::from(u8::from(b == 1)) * 0.5));
                    }
                }
            }
            text
        };
        let lut = ColorLut::parse_cube(&cube("DOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 0.5\n")).expect("parses");
        // Blue input 1 lies past the domain's 0.5: clamped to the last lattice point, output 0.5.
        let s = lut.sample([0.0, 0.0, 1.0]);
        assert!(
            (s[2] - 0.5).abs() < 1.0e-5,
            "outputs are not divided by the domain: {s:?}"
        );
        // Blue input 0.25 is half way along the domain: half way between the outputs 0 and 0.5.
        let s = lut.sample([0.0, 0.0, 0.25]);
        assert!(
            (s[2] - 0.25).abs() < 1.0e-5,
            "inputs are mapped through the domain: {s:?}"
        );
        let s = lut.sample([1.0, 1.0, 0.0]);
        assert!(
            (s[0] - 1.0).abs() < 1.0e-5 && (s[1] - 1.0).abs() < 1.0e-5,
            "{s:?}"
        );
        // A unit domain written out is the same table as no domain line.
        let unit = ColorLut::parse_cube(&cube("DOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n")).expect("parses");
        assert_eq!(unit, ColorLut::parse_cube(&cube("")).expect("parses"));
        // An empty or inverted domain has no lattice: refused, not turned into huge outputs.
        for bad in [
            "DOMAIN_MIN 0 0 1\nDOMAIN_MAX 1 1 0\n",
            "DOMAIN_MIN 0 0 0.5\nDOMAIN_MAX 1 1 0.5\n",
        ] {
            assert!(ColorLut::parse_cube(&cube(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_strip_maps_slices_to_blue() {
        let n = 4u32;
        let mut rgba = vec![0u8; (n * n * n * 4) as usize];
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let i = ((g * n * n + b * n + r) * 4) as usize;
                    rgba[i] = (r * 85) as u8;
                    rgba[i + 1] = (g * 85) as u8;
                    rgba[i + 2] = (b * 85) as u8;
                    rgba[i + 3] = 255;
                }
            }
        }
        let lut = ColorLut::from_strip_rgba8(n * n, n, &rgba).expect("strip");
        let s = lut.sample([0.0, 0.0, 1.0]);
        assert!((s[2] - 1.0).abs() < 1.0e-3, "{s:?}");
        assert!(ColorLut::from_strip_rgba8(10, 4, &rgba).is_err());
    }

    #[test]
    fn a_utf8_bom_before_lut_3d_size_is_stripped() {
        // A `.cube` saved by Windows Notepad as UTF-8 starts with U+FEFF; `trim()` does not
        // remove it (U+FEFF is not Unicode White_Space), so the size token must be stripped
        // before the line scan.
        let mut text = String::from("\u{feff}LUT_3D_SIZE 2\n");
        text.push_str(&"0 0 0\n".repeat(8));
        let lut = ColorLut::parse_cube(&text).expect("parses with the BOM");
        assert_eq!(lut.size(), 2);
    }

    #[test]
    fn a_cube_stops_at_size_cubed_and_needs_the_size_first() {
        let flood = "0 0 0\n".repeat(9);
        assert!(ColorLut::parse_cube(&format!("{flood}LUT_3D_SIZE 2\n")).is_err());
        assert!(ColorLut::parse_cube(&format!("LUT_3D_SIZE 2\n{flood}")).is_err());
        let exact = format!("LUT_3D_SIZE 2\n{}", "0 0 0\n".repeat(8));
        assert!(ColorLut::parse_cube(&exact).is_ok());
    }

    #[test]
    fn load_refuses_a_file_over_the_cap() {
        let dir = regia_test_utils::unique_temp_dir("lut-cap");
        let path = dir.join("big.cube");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(ColorLut::MAX_FILE_BYTES + 1).expect("sparse");
        assert!(matches!(ColorLut::load(&path), Err(LutError::Cube(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A manifest hit whose cooked file cannot be read (a pruned cache, an interrupted cook)
    /// behaves like a missing entry and falls through to the loose candidates; a readable cooked
    /// entry stays authoritative.
    #[test]
    fn a_manifest_hit_with_an_unreadable_cooked_file_falls_back_to_the_loose_file() {
        let root = regia_test_utils::unique_temp_dir("lut-manifest-unreadable");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let id = regia_asset::AssetId::new_random();
        let manifest = regia_asset::AssetManifest {
            entries: vec![regia_asset::ManifestEntry {
                path: "assets/looks/hall.cube".to_string(),
                id,
                type_path: "regia_render::lut::ColorLut".to_string(),
            }],
        };
        manifest.write_to_cache(&cache).expect("manifest");
        let looks = root.join("looks");
        std::fs::create_dir_all(&looks).expect("looks dir");
        std::fs::write(
            looks.join("hall.cube"),
            format!("LUT_3D_SIZE 2\n{}", "0 0 0\n".repeat(8)),
        )
        .expect("write loose");
        let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("server");
        let lut = load_named("looks/hall.cube", Some(&mut server)).expect("the loose file loads");
        assert_eq!(lut.size(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The size cap bounds the decoded payload (the source `.cube` text), not the cooked
    /// envelope: a payload of exactly `MAX_FILE_BYTES` (a small cube with comment padding)
    /// loads from the manifest, as it does loose.
    #[test]
    fn a_cooked_cube_at_the_payload_cap_loads() {
        let root = regia_test_utils::unique_temp_dir("lut-cooked-at-cap");
        let cache = regia_asset::cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let id = regia_asset::AssetId::new_random();
        let mut text = format!("LUT_3D_SIZE 2\n{}", "0 0 0\n".repeat(8));
        let pad = ColorLut::MAX_FILE_BYTES as usize - text.len() - 2; // '#' + '\n'
        text.push('#');
        text.extend(std::iter::repeat_n('x', pad));
        text.push('\n');
        assert_eq!(text.len() as u64, ColorLut::MAX_FILE_BYTES);
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of("regia_render::lut::ColorLut"),
            asset_id: id,
            importer_version: regia_asset::pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: text.len() as u64,
        };
        let file = regia_asset::encode_cooked_file(&header, text.as_bytes()).expect("encode");
        assert!(
            file.len() as u64 > ColorLut::MAX_FILE_BYTES,
            "the envelope adds a header"
        );
        std::fs::write(cache.join(regia_asset::cooked_file_name(id)), &file).expect("cooked");
        let manifest = regia_asset::AssetManifest {
            entries: vec![regia_asset::ManifestEntry {
                path: "assets/looks/hall.cube".to_string(),
                id,
                type_path: "regia_render::lut::ColorLut".to_string(),
            }],
        };
        manifest.write_to_cache(&cache).expect("manifest");
        let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("server");
        let lut = load_named("looks/hall.cube", Some(&mut server)).expect("a payload at the cap loads");
        assert_eq!(lut.size(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    /// An identity table over `-0.1..1.1` (outputs equal the input colour) in cube text.
    fn domain_cube(min: &str, max: &str) -> String {
        let mut text = format!("LUT_3D_SIZE 2\nDOMAIN_MIN {min}\nDOMAIN_MAX {max}\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    // Corner outputs 0 and 1 are the display extremes, whatever the domain.
                    text += &format!("{r} {g} {b}\n");
                }
            }
        }
        text
    }

    #[test]
    fn a_domain_remaps_inputs_and_keeps_stored_outputs() {
        let lut = ColorLut::parse_cube(&domain_cube("-0.1 -0.1 -0.1", "1.1 1.1 1.1")).expect("parse");
        // Display white (input 1.0) is inside the domain, so it must not hit output 1.0 only
        // by accident: u = 1.1/1.2 of the way along the table.
        let w = lut.sample([1.0; 3]);
        let u = 1.1 / 1.2;
        assert!((w[0] - u).abs() < 1.0e-5, "{w:?}");
        // Input 0 is at u = 0.1/1.2 ≈ 0.083, not at table entry 0.
        let b = lut.sample([0.0; 3]);
        assert!((b[0] - 0.1 / 1.2).abs() < 1.0e-5, "{b:?}");
        // Outputs are never rescaled: a corner stays at the value the file stored.
        assert!(w.iter().all(|v| *v <= 1.0));
    }

    #[test]
    fn a_degenerate_or_inverted_domain_is_rejected() {
        assert!(ColorLut::parse_cube(&domain_cube("0.5 0 0", "0.5 1 1")).is_err());
        assert!(ColorLut::parse_cube(&domain_cube("1 0 0", "0 1 1")).is_err());
        assert!(ColorLut::parse_cube(&domain_cube("nan 0 0", "1 1 1")).is_err());
    }
}
