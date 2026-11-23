// SPDX-License-Identifier: MPL-2.0
// Source: Adobe Systems, "Cube LUT Specification", version 1.0, 2013 (grammar: keywords, size
// ranges, domain bounds, red-fastest row order); the behavior spec `lut_cube.md` (clean-room,
// written against Lumit commit fa959219, GPL-3.0); J. M. Kasson, S. I. Nin, W. Plouffe and
// J. L. Hafner, "Performing color space conversions with three-dimensional linear
// interpolation", Journal of Electronic Imaging 4(3), 1995 (tetrahedral cell decomposition).

//! `.cube` color lookup tables ([`CubeLut`]): parsing and CPU reference sampling of the Adobe
//! Cube LUT Specification 1.0 text format, loader side only (the renderer uploads the parsed
//! rows as a texture; caching and GPU filtering live elsewhere).
//!
//! # Format
//!
//! Keyword lines (`TITLE`, `DOMAIN_MIN`, `DOMAIN_MAX`, `LUT_3D_SIZE`, `LUT_1D_SIZE`, matched
//! case-insensitively; unknown keywords are skipped for forward compatibility) are followed by
//! `N` (1D) or `N³` (3D) data rows of three whitespace-separated floats. 3D rows are ordered
//! red-fastest: row `r + g·N + b·N²`. Everything from the first `#` on a line to its end is a
//! comment. Row values may leave 0..1 (HDR tables) and are never clamped on load.
//!
//! [`parse`] rejects malformed input with line-numbered errors, so an effect that fails to load
//! its table can become a labeled passthrough instead of crashing the render.
//!
//! # Sampling
//!
//! Each channel of the input is mapped through the declared domain onto grid coordinates and
//! clamped there (no extrapolation, no wrap). 3D tables blend the enclosing cell's corners
//! trilinearly (continuous; the reference the GPU path must match) or tetrahedrally (4 samples,
//! exact on the neutral axis; Kasson et al. 1995). 1D tables interpolate each channel linearly
//! and independently. [`CubeLut::sample`] takes straight (unpremultiplied) RGB; premultiply and
//! mix are the caller's compositing steps, not the loader's.
//!
//! # Gaps in the behavior reference, closed per its own recommendations
//!
//! - Non-finite numbers and `DOMAIN_MIN >= DOMAIN_MAX` are rejected instead of accepted.
//! - A data row starting with a non-finite word (`nan`, `inf`) is classified as data and
//!   rejected, not skipped as an unknown keyword.
//! - 1D tables are actually sampled (the reference's parser reads them but its render path
//!   passes them through unchanged).
//!
//! Adobe's maximum line length is not enforced: the published limit is a strict-conformance
//! option the behavior spec leaves open, and its numeric value is not reproduced here.

use std::path::Path;

use regia_core::glam::Vec3;
use thiserror::Error;

/// Smallest 3D table edge the format defines (Adobe range 2..=256).
pub const LUT_3D_MIN_SIZE: u32 = 2;
/// Largest 3D table edge accepted (Adobe range 2..=256; a 256³ table is about 200 MB of f32).
pub const LUT_3D_MAX_SIZE: u32 = 256;
/// Smallest 1D table length the format defines (Adobe range 2..=65,536).
pub const LUT_1D_MIN_SIZE: u32 = 2;
/// Largest 1D table length accepted (Adobe range 2..=65,536).
pub const LUT_1D_MAX_SIZE: u32 = 65_536;

/// Errors from parsing or loading a `.cube` file.
///
/// Every parse error carries the 1-based line it was found on ([`CubeLutError::line`]); a
/// render effect turns any of these into a labeled passthrough.
#[derive(Debug, Error)]
pub enum CubeLutError {
    /// `TITLE` is not followed by a double-quoted string.
    #[error("line {line}: TITLE must be followed by a double-quoted string")]
    Title {
        /// Line the keyword was on.
        line: u32,
    },
    /// `DOMAIN_MIN` or `DOMAIN_MAX` has fewer than the required three numbers (extra numbers
    /// after the three are ignored, per the format's tolerance).
    #[error("line {line}: {keyword} needs three numbers, found {found}")]
    DomainArity {
        /// Line the keyword was on.
        line: u32,
        /// The keyword as written in canonical upper case.
        keyword: &'static str,
        /// Number of values found after the keyword.
        found: usize,
    },
    /// A `DOMAIN_MIN` or `DOMAIN_MAX` value does not parse or is not finite.
    #[error("line {line}: {keyword} value '{token}' is not a finite number")]
    DomainValue {
        /// Line the keyword was on.
        line: u32,
        /// The keyword as written in canonical upper case.
        keyword: &'static str,
        /// The offending token, verbatim.
        token: String,
    },
    /// `DOMAIN_MIN` is not below `DOMAIN_MAX` on every channel (a zero or reversed span makes
    /// the domain mapping degenerate, so the file is malformed).
    #[error("line {line}: DOMAIN_MIN must be below DOMAIN_MAX on every channel")]
    DomainRange {
        /// Line of the last domain keyword declared (0 when only defaults are in play).
        line: u32,
    },
    /// A size keyword's value is not an integer.
    #[error("line {line}: {keyword} needs an integer size, found '{found}'")]
    SizeValue {
        /// Line the keyword was on.
        line: u32,
        /// The keyword as written in canonical upper case.
        keyword: &'static str,
        /// The offending token (empty when the keyword had no argument).
        found: String,
    },
    /// A size outside the format's range, refused before any table memory is allocated.
    #[error("line {line}: {keyword} {size} is outside the supported range {min}..={max}")]
    SizeLimit {
        /// Line the keyword was on.
        line: u32,
        /// The keyword as written in canonical upper case.
        keyword: &'static str,
        /// The size that was declared.
        size: u32,
        /// Smallest accepted size for this table kind.
        min: u32,
        /// Largest accepted size for this table kind.
        max: u32,
    },
    /// A second size keyword: both table kinds present, or one kind twice.
    #[error("line {line}: repeated size keyword {keyword}; a file declares exactly one table")]
    SizeRepeated {
        /// Line the repeated keyword was on.
        line: u32,
        /// The repeated keyword as written in canonical upper case.
        keyword: &'static str,
    },
    /// A data row appeared before the size keyword, so the expected row count is unknown.
    #[error("line {line}: data row before the size keyword")]
    DataBeforeSize {
        /// Line the row was on.
        line: u32,
    },
    /// No size keyword anywhere in the file (a file of only metadata, or empty).
    #[error("no LUT_1D_SIZE or LUT_3D_SIZE keyword in the file")]
    MissingSize,
    /// A data row without exactly three values.
    #[error("line {line}: data row needs exactly three values, found {count}")]
    RowValues {
        /// Line the row was on.
        line: u32,
        /// Number of whitespace-separated values on the line.
        count: usize,
    },
    /// A data row value that does not parse or is not finite (NaN, infinities).
    #[error("line {line}: data value '{token}' is not a finite number")]
    RowValue {
        /// Line the row was on.
        line: u32,
        /// The offending token, verbatim.
        token: String,
    },
    /// The table does not have the declared number of rows; a partial table is never returned.
    #[error("line {line}: table needs {expected} rows, found {found}")]
    RowCount {
        /// Line the mismatch was detected on (the extra row, or the last line of the file).
        line: u32,
        /// Row count the size keyword declared (`N` or `N³`).
        expected: usize,
        /// Rows actually present.
        found: usize,
    },
    /// The file could not be read (missing, unreadable, or not UTF-8).
    #[error("cube lut io: {0}")]
    Io(#[from] std::io::Error),
}

impl CubeLutError {
    /// The 1-based line of the file the error was found on, when the error is about the file's
    /// text; IO errors have no line.
    #[must_use]
    pub fn line(&self) -> Option<u32> {
        match self {
            Self::Title { line }
            | Self::DomainArity { line, .. }
            | Self::DomainValue { line, .. }
            | Self::DomainRange { line, .. }
            | Self::SizeValue { line, .. }
            | Self::SizeLimit { line, .. }
            | Self::SizeRepeated { line, .. }
            | Self::DataBeforeSize { line }
            | Self::RowValues { line, .. }
            | Self::RowValue { line, .. }
            | Self::RowCount { line, .. } => Some(*line),
            Self::MissingSize | Self::Io(_) => None,
        }
    }
}

/// Which table kind a `.cube` file declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CubeLutKind {
    /// `LUT_1D_SIZE N`: N rows, three independent per-channel curves.
    OneDimensional,
    /// `LUT_3D_SIZE N`: N³ rows, a full color transform (channels interact).
    ThreeDimensional,
}

/// The interpolation [`CubeLut::sample_with`] blends 3D table cells with. A 1D table ignores
/// the choice and always interpolates linearly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CubeLutInterpolation {
    /// Blend the 8 corners of the enclosing cell by the fractional position; continuous across
    /// cells, and the reference a GPU trilinear path must match.
    #[default]
    Trilinear,
    /// Split the cell into six tetrahedra along its main diagonal, pick the one containing the
    /// point by ordering the three fractional coordinates, and blend its 4 corners. Cheaper,
    /// and exactly preserves the neutral (gray) axis when the table is neutral on its
    /// diagonal. Kasson et al. (1995) compare the two schemes.
    Tetrahedral,
}

/// A parsed `.cube` color lookup table with its CPU reference sampler.
///
/// Build one with [`parse`] (text) or [`CubeLut::from_path`] (file); both validate the whole
/// table and never return a partial one. The invariants a [`CubeLut`] always holds: exactly
/// `N` (1D) or `N³` (3D) rows, every row finite, and `domain_min < domain_max` per channel.
#[derive(Clone, Debug, PartialEq)]
pub struct CubeLut {
    title: Option<String>,
    domain_min: [f32; 3],
    domain_max: [f32; 3],
    kind: CubeLutKind,
    size: u32,
    rows: Vec<[f32; 3]>,
}

impl CubeLut {
    /// Reads and parses the `.cube` file at `path`.
    ///
    /// # Errors
    /// [`CubeLutError::Io`] when the file cannot be read or is not UTF-8; any parse error the
    /// text contains.
    pub fn from_path(path: &Path) -> Result<Self, CubeLutError> {
        parse(&std::fs::read_to_string(path)?)
    }

    /// The `TITLE` metadata, when the file declared one. Metadata only; rendering ignores it.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Lower input bound per channel (R, G, B); `[0, 0, 0]` when the file declared none.
    #[must_use]
    pub fn domain_min(&self) -> [f32; 3] {
        self.domain_min
    }

    /// Upper input bound per channel; `[1, 1, 1]` when the file declared none.
    #[must_use]
    pub fn domain_max(&self) -> [f32; 3] {
        self.domain_max
    }

    /// The declared size: N rows for a 1D table, an N-edge cube (N³ rows) for a 3D one.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Which table kind the file declared.
    #[must_use]
    pub fn kind(&self) -> CubeLutKind {
        self.kind
    }

    /// The table rows, in file order: for a 3D table, row `r + g·N + b·N²` (red changes
    /// fastest, then green, then blue); for a 1D table, row `i` holds the three channel
    /// outputs for input position `i`. Values are not clamped to 0..1 (HDR tables load as
    /// authored); a renderer can upload the slice without copying.
    #[must_use]
    pub fn rows(&self) -> &[[f32; 3]] {
        &self.rows
    }

    /// Samples the table at straight (unpremultiplied) RGB `rgb` with the default method:
    /// trilinear for a 3D table, linear per channel for a 1D table.
    ///
    /// Each channel is mapped through the declared domain and clamped to the table's edge
    /// entries; the result is never clamped, so a table may produce values below 0 or above 1.
    #[must_use]
    pub fn sample(&self, rgb: Vec3) -> Vec3 {
        self.sample_with(rgb, CubeLutInterpolation::default())
    }

    /// Samples the table at straight RGB `rgb` with an explicit interpolation method. A 1D
    /// table always interpolates linearly, whatever method is asked for.
    #[must_use]
    pub fn sample_with(&self, rgb: Vec3, method: CubeLutInterpolation) -> Vec3 {
        match self.kind {
            CubeLutKind::OneDimensional => self.sample_1d(rgb),
            CubeLutKind::ThreeDimensional => match method {
                CubeLutInterpolation::Trilinear => self.sample_3d_trilinear(rgb),
                CubeLutInterpolation::Tetrahedral => self.sample_3d_tetrahedral(rgb),
            },
        }
    }

    /// 1D lookup: each channel independently maps through its own domain, then interpolates
    /// linearly between the two nearest entries of its column.
    fn sample_1d(&self, rgb: Vec3) -> Vec3 {
        let last = (self.size - 1) as usize;
        let channel = |value: f32, c: usize| -> f32 {
            let g = self.coord(value, c);
            let f = g.floor();
            // `as` saturates (and maps NaN to 0); the clamp keeps the index in range.
            let i0 = (f as usize).min(last);
            let i1 = (i0 + 1).min(last);
            let t = g - f;
            self.rows[i0][c] * (1.0 - t) + self.rows[i1][c] * t
        };
        Vec3::new(channel(rgb.x, 0), channel(rgb.y, 1), channel(rgb.z, 2))
    }

    /// Trilinear 3D lookup: blend the 8 corners of the enclosing cell along red, then green,
    /// then blue by the fractional position.
    fn sample_3d_trilinear(&self, rgb: Vec3) -> Vec3 {
        let last = (self.size - 1) as usize;
        let (r0, fr) = cell(self.coord(rgb.x, 0), last);
        let (g0, fg) = cell(self.coord(rgb.y, 1), last);
        let (b0, fb) = cell(self.coord(rgb.z, 2), last);
        let r1 = (r0 + 1).min(last);
        let g1 = (g0 + 1).min(last);
        let b1 = (b0 + 1).min(last);
        let c000 = self.row3(r0, g0, b0);
        let c100 = self.row3(r1, g0, b0);
        let c010 = self.row3(r0, g1, b0);
        let c110 = self.row3(r1, g1, b0);
        let c001 = self.row3(r0, g0, b1);
        let c101 = self.row3(r1, g0, b1);
        let c011 = self.row3(r0, g1, b1);
        let c111 = self.row3(r1, g1, b1);
        let c00 = lerp3(c000, c100, fr);
        let c10 = lerp3(c010, c110, fr);
        let c01 = lerp3(c001, c101, fr);
        let c11 = lerp3(c011, c111, fr);
        Vec3::from(lerp3(lerp3(c00, c10, fg), lerp3(c01, c11, fg), fb))
    }

    /// Tetrahedral 3D lookup: the six orderings of the fractional coordinates pick one of the
    /// six tetrahedra that split the cell along its main diagonal; the four corners are the
    /// cell's base corner, its far corner, and the two axis corners on the path between them.
    fn sample_3d_tetrahedral(&self, rgb: Vec3) -> Vec3 {
        let last = (self.size - 1) as usize;
        let (r0, fr) = cell(self.coord(rgb.x, 0), last);
        let (g0, fg) = cell(self.coord(rgb.y, 1), last);
        let (b0, fb) = cell(self.coord(rgb.z, 2), last);
        let r1 = (r0 + 1).min(last);
        let g1 = (g0 + 1).min(last);
        let b1 = (b0 + 1).min(last);
        let c000 = self.row3(r0, g0, b0);
        let c111 = self.row3(r1, g1, b1);
        let (w0, va, wa, vb, wb, w111);
        if fr >= fg && fg >= fb {
            w0 = 1.0 - fr;
            (va, wa) = (self.row3(r1, g0, b0), fr - fg);
            (vb, wb) = (self.row3(r1, g1, b0), fg - fb);
            w111 = fb;
        } else if fg > fr && fr >= fb {
            w0 = 1.0 - fg;
            (va, wa) = (self.row3(r0, g1, b0), fg - fr);
            (vb, wb) = (self.row3(r1, g1, b0), fr - fb);
            w111 = fb;
        } else if fg >= fb && fb > fr {
            w0 = 1.0 - fg;
            (va, wa) = (self.row3(r0, g1, b0), fg - fb);
            (vb, wb) = (self.row3(r0, g1, b1), fb - fr);
            w111 = fr;
        } else if fb > fg && fg >= fr {
            w0 = 1.0 - fb;
            (va, wa) = (self.row3(r0, g0, b1), fb - fg);
            (vb, wb) = (self.row3(r0, g1, b1), fg - fr);
            w111 = fr;
        } else if fr > fb && fb > fg {
            w0 = 1.0 - fr;
            (va, wa) = (self.row3(r1, g0, b0), fr - fb);
            (vb, wb) = (self.row3(r1, g0, b1), fb - fg);
            w111 = fg;
        } else {
            // fb >= fr > fg: the remaining ordering.
            w0 = 1.0 - fb;
            (va, wa) = (self.row3(r0, g0, b1), fb - fr);
            (vb, wb) = (self.row3(r1, g0, b1), fr - fg);
            w111 = fg;
        }
        let out = axpy3(axpy3(axpy3(scale3(c000, w0), va, wa), vb, wb), c111, w111);
        Vec3::from(out)
    }

    /// Domain-mapped, clamped grid coordinate of `value` on channel `c` (0 = R, 1 = G, 2 = B):
    /// `t = (value - min) / (max - min)`, then `t · (N - 1)` clamped to `0..=N - 1`. The span
    /// is validated positive, so the division cannot divide by zero.
    fn coord(&self, value: f32, c: usize) -> f32 {
        let last = (self.size - 1) as f32;
        let t = (value - self.domain_min[c]) / (self.domain_max[c] - self.domain_min[c]);
        let g = (t * last).clamp(0.0, last);
        // `clamp` passes NaN through; a NaN input channel reads the table's first entry instead
        // of turning the whole sample into NaN.
        if g.is_nan() { 0.0 } else { g }
    }

    /// Row of grid cell `(r, g, b)` (red fastest, then green, then blue).
    fn row3(&self, r: usize, g: usize, b: usize) -> [f32; 3] {
        let n = self.size as usize;
        self.rows[r + g * n + b * n * n]
    }
}

/// Parses the text of a `.cube` file (Adobe Cube LUT Specification 1.0) into a [`CubeLut`].
///
/// LF, CR LF and CR line ends are accepted; everything from the first `#` on a line to its end
/// is a comment; keywords match case-insensitively and unknown keywords are skipped. Numbers
/// use ordinary float syntax (`0.5`, `-2`, `1e-3`); non-finite values are rejected.
///
/// # Errors
/// A [`CubeLutError`] naming the offending 1-based line: a malformed `TITLE`, a domain line
/// that is short, unparsable, non-finite or not strictly below the other bound, a size keyword
/// that is unreadable, out of the Adobe range, or repeated, a data row before the size
/// keyword, a row without exactly three finite values, or a wrong row count. A file with no
/// size keyword at all is [`CubeLutError::MissingSize`].
pub fn parse(text: &str) -> Result<CubeLut, CubeLutError> {
    let mut state = ParseState::default();
    // Lines are split by hand so line numbers stay correct for LF, CR LF and lone CR endings:
    // a CR immediately followed by an LF ends exactly one line.
    let mut line_no: u32 = 0;
    let mut line = String::new();
    let mut last_was_cr = false;
    for ch in text.chars() {
        if ch == '\n' && last_was_cr {
            last_was_cr = false;
            continue;
        }
        if ch == '\n' || ch == '\r' {
            line_no += 1;
            state.line(&line, line_no)?;
            line.clear();
            last_was_cr = ch == '\r';
        } else {
            last_was_cr = false;
            line.push(ch);
        }
    }
    if !line.is_empty() {
        line_no += 1;
        state.line(&line, line_no)?;
    }
    state.finish(line_no)
}

/// Accumulates one file's keywords and rows, then checks them as a whole.
#[derive(Default)]
struct ParseState {
    title: Option<String>,
    domain_min: Option<[f32; 3]>,
    domain_max: Option<[f32; 3]>,
    domain_line: Option<u32>,
    size: Option<(CubeLutKind, u32)>,
    expected_rows: usize,
    rows: Vec<[f32; 3]>,
}

impl ParseState {
    /// Classifies and handles one line: comment-stripped, blank lines skipped, first token
    /// starting with a letter (and not itself a number) makes a keyword line, anything else a
    /// data row.
    fn line(&mut self, raw: &str, line_no: u32) -> Result<(), CubeLutError> {
        // Everything from the first '#' to the end of the line is ignored, so trailing
        // comments work and a '#' inside a quoted title truncates the title harmlessly.
        let body = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
        if body.is_empty() {
            return Ok(());
        }
        let first = body.split_whitespace().next().unwrap_or_default();
        // A non-finite word such as `nan` or `inf` starts with a letter but *is* a number:
        // treat it as data so it is rejected explicitly instead of being skipped as an
        // unknown keyword and surfacing later as a row-count error.
        let is_keyword =
            first.starts_with(|c: char| c.is_ascii_alphabetic()) && first.parse::<f32>().is_err();
        if is_keyword {
            self.keyword(first, body, line_no)
        } else {
            self.data(body, line_no)
        }
    }

    /// Handles one keyword line; unknown keywords are skipped for forward compatibility
    /// (vendor keywords included).
    fn keyword(&mut self, first: &str, body: &str, line_no: u32) -> Result<(), CubeLutError> {
        match first.to_ascii_uppercase().as_str() {
            "TITLE" => {
                self.title = Some(parse_title(first, body, line_no)?);
                Ok(())
            }
            "DOMAIN_MIN" => {
                self.domain_min = Some(parse_domain(body, "DOMAIN_MIN", line_no)?);
                self.domain_line = Some(line_no);
                Ok(())
            }
            "DOMAIN_MAX" => {
                self.domain_max = Some(parse_domain(body, "DOMAIN_MAX", line_no)?);
                self.domain_line = Some(line_no);
                Ok(())
            }
            "LUT_3D_SIZE" => self.size_line(CubeLutKind::ThreeDimensional, body, line_no),
            "LUT_1D_SIZE" => self.size_line(CubeLutKind::OneDimensional, body, line_no),
            _ => Ok(()),
        }
    }

    /// Handles one size keyword: the value must be an integer in the Adobe range (checked
    /// before any row memory is spent) and exactly one size keyword may appear.
    fn size_line(&mut self, kind: CubeLutKind, body: &str, line_no: u32) -> Result<(), CubeLutError> {
        let (keyword, min, max) = match kind {
            CubeLutKind::ThreeDimensional => ("LUT_3D_SIZE", LUT_3D_MIN_SIZE, LUT_3D_MAX_SIZE),
            CubeLutKind::OneDimensional => ("LUT_1D_SIZE", LUT_1D_MIN_SIZE, LUT_1D_MAX_SIZE),
        };
        let Some(token) = body.split_whitespace().nth(1) else {
            return Err(CubeLutError::SizeValue {
                line: line_no,
                keyword,
                found: String::new(),
            });
        };
        let size = token.parse::<u32>().map_err(|_| CubeLutError::SizeValue {
            line: line_no,
            keyword,
            found: token.to_owned(),
        })?;
        if !(min..=max).contains(&size) {
            return Err(CubeLutError::SizeLimit {
                line: line_no,
                keyword,
                size,
                min,
                max,
            });
        }
        if self.size.is_some() {
            return Err(CubeLutError::SizeRepeated {
                line: line_no,
                keyword,
            });
        }
        // Bounded by the range check far below usize::MAX on every target.
        self.expected_rows = match kind {
            CubeLutKind::ThreeDimensional => (size as u64).pow(3) as usize,
            CubeLutKind::OneDimensional => size as usize,
        };
        self.size = Some((kind, size));
        Ok(())
    }

    /// Handles one data row: exactly three finite floats, after the size keyword, and only up
    /// to the declared row count (extra rows are refused on the line they appear).
    fn data(&mut self, body: &str, line_no: u32) -> Result<(), CubeLutError> {
        if self.size.is_none() {
            return Err(CubeLutError::DataBeforeSize { line: line_no });
        }
        let count = body.split_whitespace().count();
        if count != 3 {
            return Err(CubeLutError::RowValues { line: line_no, count });
        }
        let mut row = [0.0f32; 3];
        for (slot, token) in row.iter_mut().zip(body.split_whitespace()) {
            let value = token.parse::<f32>().map_err(|_| CubeLutError::RowValue {
                line: line_no,
                token: token.to_owned(),
            })?;
            if !value.is_finite() {
                return Err(CubeLutError::RowValue {
                    line: line_no,
                    token: token.to_owned(),
                });
            }
            *slot = value;
        }
        if self.rows.len() >= self.expected_rows {
            return Err(CubeLutError::RowCount {
                line: line_no,
                expected: self.expected_rows,
                found: self.rows.len() + 1,
            });
        }
        self.rows.push(row);
        Ok(())
    }

    /// Validates the whole file (one size keyword, exact row count, strict domain) and builds
    /// the table. `last_line` is the number of the file's last line.
    fn finish(self, last_line: u32) -> Result<CubeLut, CubeLutError> {
        let Some((kind, size)) = self.size else {
            return Err(CubeLutError::MissingSize);
        };
        if self.rows.len() != self.expected_rows {
            return Err(CubeLutError::RowCount {
                line: last_line,
                expected: self.expected_rows,
                found: self.rows.len(),
            });
        }
        let domain_min = self.domain_min.unwrap_or([0.0; 3]);
        let domain_max = self.domain_max.unwrap_or([1.0; 3]);
        for (lo, hi) in domain_min.iter().zip(&domain_max) {
            if *lo >= *hi {
                return Err(CubeLutError::DomainRange {
                    line: self.domain_line.unwrap_or(0),
                });
            }
        }
        Ok(CubeLut {
            title: self.title,
            domain_min,
            domain_max,
            kind,
            size,
            rows: self.rows,
        })
    }
}

/// Parses a `TITLE` line: everything after the keyword must be a double-quoted string; extra
/// tokens after the closing quote are ignored. The comment strip may have removed a closing
/// quote when the title contained `#`; the remainder up to the end of the line is then the
/// title, which is the harmless truncation the format allows.
fn parse_title(keyword: &str, body: &str, line_no: u32) -> Result<String, CubeLutError> {
    let rest = body.strip_prefix(keyword).unwrap_or_default().trim_start();
    let Some(after_quote) = rest.strip_prefix('"') else {
        return Err(CubeLutError::Title { line: line_no });
    };
    Ok(after_quote
        .split_once('"')
        .map_or(after_quote, |(text, _)| text)
        .to_owned())
}

/// Parses a `DOMAIN_MIN` or `DOMAIN_MAX` line: at least three numbers, of which the first
/// three are taken (extras are ignored) and each must be a finite float.
fn parse_domain(body: &str, keyword: &'static str, line_no: u32) -> Result<[f32; 3], CubeLutError> {
    let found = body.split_whitespace().count().saturating_sub(1);
    if found < 3 {
        return Err(CubeLutError::DomainArity {
            line: line_no,
            keyword,
            found,
        });
    }
    let mut values = [0.0f32; 3];
    for (slot, token) in values.iter_mut().zip(body.split_whitespace().skip(1)) {
        let value = token.parse::<f32>().map_err(|_| CubeLutError::DomainValue {
            line: line_no,
            keyword,
            token: token.to_owned(),
        })?;
        if !value.is_finite() {
            return Err(CubeLutError::DomainValue {
                line: line_no,
                keyword,
                token: token.to_owned(),
            });
        }
        *slot = value;
    }
    Ok(values)
}

/// Floor cell index and fraction of a clamped grid coordinate; callers clamp `index + 1`.
fn cell(g: f32, last: usize) -> (usize, f32) {
    let f = g.floor();
    // `as` saturates (and maps NaN to 0); the clamp keeps the index in range.
    ((f as usize).min(last), g - f)
}

/// Component-wise linear blend: `a + t · (b - a)`.
fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + t * (b[0] - a[0]),
        a[1] + t * (b[1] - a[1]),
        a[2] + t * (b[2] - a[2]),
    ]
}

/// Accumulates `acc + v · w` component-wise.
fn axpy3(acc: [f32; 3], v: [f32; 3], w: f32) -> [f32; 3] {
    [acc[0] + v[0] * w, acc[1] + v[1] * w, acc[2] + v[2] * w]
}

/// Multiplies every component by `w`.
fn scale3(v: [f32; 3], w: f32) -> [f32; 3] {
    [v[0] * w, v[1] * w, v[2] * w]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when every component of `a` and `b` is within `tol`.
    fn close(a: Vec3, b: Vec3, tol: f32) -> bool {
        (a - b).abs().max_element() <= tol
    }

    /// A 3D `.cube` text of edge `n` filled by `f` over normalized grid coordinates, plus the
    /// authored rows in file order (red fastest).
    fn cube_text_and_rows(n: u32, f: impl Fn(f32, f32, f32) -> [f32; 3]) -> (String, Vec<[f32; 3]>) {
        let mut text = String::from("TITLE \"generated\"\nLUT_3D_SIZE ");
        text.push_str(&n.to_string());
        text.push('\n');
        let mut rows = Vec::new();
        let last = (n - 1) as f32;
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let v = f(r as f32 / last, g as f32 / last, b as f32 / last);
                    rows.push(v);
                    text.push_str(&format!("{} {} {}\n", v[0], v[1], v[2]));
                }
            }
        }
        (text, rows)
    }

    /// The known-value 2³ table, per axis (R = 0.25·r, G = 0.10·g, B = 0.40·b): affine, so
    /// both interpolations are exact.
    const AFFINE_2_CUBE: &str = "TITLE \"affine\"\nLUT_3D_SIZE 2\n\
        0.00 0.00 0.00\n0.25 0.00 0.00\n0.00 0.10 0.00\n0.25 0.10 0.00\n\
        0.00 0.00 0.40\n0.25 0.00 0.40\n0.00 0.10 0.40\n0.25 0.10 0.40\n";

    /// The arbitrary 2³ table (non-affine values, one out of 0..1).
    const WILD_2_CUBE: &str = "LUT_3D_SIZE 2\n\
        0.1 0.2 0.3\n0.9 -0.1 0.0\n0.3 0.5 0.6\n0.7 0.7 0.1\n\
        0.0 1.0 0.2\n0.5 0.25 0.75\n0.33 0.66 0.11\n2.0 0.0 -0.5\n";

    #[test]
    fn parse_reports_title_domain_size_kind_and_rows() {
        let text = "TITLE \"demo\"\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\nLUT_3D_SIZE 2\n\
            0.00 0.10 0.20\n1.00 0.10 0.20\n0.00 0.60 0.20\n1.00 0.60 0.20\n\
            0.00 0.10 0.90\n1.00 0.10 0.90\n0.00 0.60 0.90\n1.00 0.60 0.90\n";
        let lut = parse(text).expect("valid table");
        assert_eq!(lut.title(), Some("demo"));
        assert_eq!(lut.domain_min(), [0.0, 0.0, 0.0]);
        assert_eq!(lut.domain_max(), [1.0, 1.0, 1.0]);
        assert_eq!(lut.size(), 2);
        assert_eq!(lut.kind(), CubeLutKind::ThreeDimensional);
        assert_eq!(lut.rows().len(), 8);
        // Red changes fastest: bumping r moves to the next row, bumping g the row after that.
        assert_eq!(lut.rows()[0], [0.0, 0.1, 0.2]);
        assert_eq!(lut.rows()[1], [1.0, 0.1, 0.2]);
        assert_eq!(lut.rows()[2], [0.0, 0.6, 0.2]);
        assert_eq!(lut.rows()[4], [0.0, 0.1, 0.9]);
    }

    #[test]
    fn identity_3d_returns_input_to_1e_6() {
        let (text, _) = cube_text_and_rows(33, |r, g, b| [r, g, b]);
        let lut = parse(&text).expect("identity table");
        for i in 0..=64 {
            let a = i as f32 / 64.0;
            let input = Vec3::new(a, 1.0 - a, (i % 13) as f32 / 13.0);
            assert!(close(lut.sample(input), input, 1e-6), "{input}");
        }
    }

    #[test]
    fn identity_3d_tetrahedral_returns_input_to_1e_6() {
        let (text, _) = cube_text_and_rows(33, |r, g, b| [r, g, b]);
        let lut = parse(&text).expect("identity table");
        for i in 0..=64 {
            let a = i as f32 / 64.0;
            let input = Vec3::new(a, (i % 7) as f32 / 7.0, 1.0 - a);
            let out = lut.sample_with(input, CubeLutInterpolation::Tetrahedral);
            assert!(close(out, input, 1e-6), "{input} -> {out}");
        }
    }

    #[test]
    fn identity_1d_returns_input_to_1e_6() {
        let mut text = String::from("LUT_1D_SIZE 17\n");
        for i in 0..17 {
            let v = i as f32 / 16.0;
            text.push_str(&format!("{v} {v} {v}\n"));
        }
        let lut = parse(&text).expect("identity 1D table");
        assert_eq!(lut.kind(), CubeLutKind::OneDimensional);
        for i in 0..=64 {
            let a = i as f32 / 64.0;
            let input = Vec3::new(a, 1.0 - a, (i % 11) as f32 / 11.0);
            assert!(close(lut.sample(input), input, 1e-6), "{input}");
        }
    }

    #[test]
    fn trilinear_known_corner_and_edge_values() {
        let lut = parse(AFFINE_2_CUBE).expect("affine table");
        // Corners and edges return their rows exactly.
        assert!(close(lut.sample(Vec3::ZERO), Vec3::ZERO, 0.0));
        assert!(close(lut.sample(Vec3::ONE), Vec3::new(0.25, 0.10, 0.40), 0.0));
        assert!(close(
            lut.sample(Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(0.25, 0.0, 0.0),
            0.0
        ));
        assert!(close(
            lut.sample(Vec3::new(0.0, 1.0, 0.0)),
            Vec3::new(0.0, 0.10, 0.0),
            0.0
        ));
        // Edge midpoint and a general point: the table is affine, so both methods are exact.
        let mid = lut.sample(Vec3::new(1.0, 0.5, 0.0));
        assert!(close(mid, Vec3::new(0.25, 0.05, 0.0), 1e-7), "{mid}");
        let p = Vec3::new(0.5, 0.25, 0.75);
        let expected = Vec3::new(0.125, 0.025, 0.30);
        assert!(close(lut.sample(p), expected, 1e-7));
        assert!(close(
            lut.sample_with(p, CubeLutInterpolation::Tetrahedral),
            expected,
            1e-7
        ));
    }

    #[test]
    fn trilinear_cell_center_is_corner_average() {
        let lut = parse(WILD_2_CUBE).expect("wild table");
        let out = lut.sample(Vec3::new(0.5, 0.5, 0.5));
        // Equal fractional weights on every corner.
        let expected = Vec3::new(4.83 / 8.0, 3.21 / 8.0, 1.56 / 8.0);
        assert!(close(out, expected, 1e-6), "{out} vs {expected}");
    }

    #[test]
    fn tetrahedral_known_values() {
        let lut = parse(WILD_2_CUBE).expect("wild table");
        // (fr, fg, fb) = (0.5, 0.25, 0.75): fb >= fr > fg, so the tetrahedron is
        // c000, c001, c101, c111 with equal weights of 0.25 each.
        let out = lut.sample_with(Vec3::new(0.5, 0.25, 0.75), CubeLutInterpolation::Tetrahedral);
        let expected = Vec3::new(
            0.25 * (0.1 + 0.0 + 0.5 + 2.0),
            0.25 * (0.2 + 1.0 + 0.25 + 0.0),
            0.25 * (0.3 + 0.2 + 0.75 - 0.5),
        );
        assert!(close(out, expected, 1e-6), "{out} vs {expected}");
        // Grid nodes still return their rows exactly.
        let node = lut.sample_with(Vec3::new(0.0, 0.0, 1.0), CubeLutInterpolation::Tetrahedral);
        assert!(close(node, Vec3::new(0.0, 1.0, 0.2), 0.0));
    }

    #[test]
    fn one_d_known_values_and_channel_independence() {
        let text = "LUT_1D_SIZE 3\n0.1 0.2 0.3\n0.3 0.2 0.1\n0.9 0.4 0.0\n";
        let lut = parse(text).expect("1D table");
        // t = 0.5 lands exactly on row 1.
        assert!(close(lut.sample(Vec3::splat(0.5)), Vec3::new(0.3, 0.2, 0.1), 0.0));
        // Each channel interpolates its own column at its own position.
        let out = lut.sample(Vec3::new(0.25, 0.75, 0.0));
        assert!(close(out, Vec3::new(0.2, 0.3, 0.3), 1e-7), "{out}");
    }

    #[test]
    fn one_d_tetrahedral_falls_back_to_linear() {
        let text = "LUT_1D_SIZE 3\n0.1 0.2 0.3\n0.3 0.2 0.1\n0.9 0.4 0.0\n";
        let lut = parse(text).expect("1D table");
        let p = Vec3::new(0.3, 0.6, 0.9);
        assert_eq!(
            lut.sample(p),
            lut.sample_with(p, CubeLutInterpolation::Tetrahedral)
        );
    }

    #[test]
    fn domain_remap_size_two_grid_equals_normalized_input() {
        let text = "DOMAIN_MIN 0 0 0\nDOMAIN_MAX 2 2 2\nLUT_3D_SIZE 2\n\
            0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        let lut = parse(text).expect("remapped table");
        // With N = 2 the grid coordinate is the domain-normalized input.
        let out = lut.sample(Vec3::new(0.5, 1.0, 1.5));
        assert!(close(out, Vec3::new(0.25, 0.5, 0.75), 1e-7), "{out}");
        // Outside the domain clamps to the edge entries on each channel independently.
        let clamped = lut.sample(Vec3::new(-1.0, 99.0, 2.0));
        assert!(close(clamped, Vec3::new(0.0, 1.0, 1.0), 0.0), "{clamped}");
    }

    #[test]
    fn per_channel_domains() {
        let text = "DOMAIN_MIN 0 -1 0\nDOMAIN_MAX 1 0 2\nLUT_3D_SIZE 2\n\
            0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        let lut = parse(text).expect("per-channel domain");
        assert_eq!(lut.domain_min(), [0.0, -1.0, 0.0]);
        assert_eq!(lut.domain_max(), [1.0, 0.0, 2.0]);
        let out = lut.sample(Vec3::new(0.5, -0.5, 1.0));
        assert!(close(out, Vec3::splat(0.5), 1e-7), "{out}");
    }

    #[test]
    fn sample_clamps_outside_the_default_domain() {
        let (text, _) = cube_text_and_rows(33, |r, g, b| [r, g, b]);
        let lut = parse(&text).expect("identity table");
        let out = lut.sample(Vec3::new(2.0, -1.0, 0.5));
        assert!(close(out, Vec3::new(1.0, 0.0, 0.5), 1e-6), "{out}");
    }

    #[test]
    fn output_values_are_not_clamped() {
        let text = "LUT_1D_SIZE 2\n2.5 -0.5 0.0\n3.0 0.5 0.0\n";
        let lut = parse(text).expect("HDR table loads unclamped");
        let out = lut.sample(Vec3::new(0.25, 0.75, 0.5));
        assert!(close(out, Vec3::new(2.625, 0.25, 0.0), 1e-6), "{out}");
    }

    #[test]
    fn cube_33_round_trip() {
        let f = |r: f32, g: f32, b: f32| [r * r, g * b, (r + g + b) / 3.0];
        let (text, rows) = cube_text_and_rows(33, f);
        let lut = parse(&text).expect("33^3 table");
        assert_eq!(lut.title(), Some("generated"));
        assert_eq!(lut.kind(), CubeLutKind::ThreeDimensional);
        assert_eq!(lut.rows().len(), 33usize * 33 * 33);
        // Every authored value survives the text round trip exactly.
        for (i, authored) in rows.iter().enumerate() {
            assert_eq!(&lut.rows()[i], authored, "row {i}");
        }
        // Grid nodes sample their rows exactly (the middle of the table is a node).
        let center = lut.sample(Vec3::splat(0.5));
        assert!(close(center, Vec3::from(f(0.5, 0.5, 0.5)), 1e-6), "{center}");
        // A non-node point: r^2 interpolated on a 33-grid stays close to the exact quadratic.
        let p = lut.sample(Vec3::new(1.0 / 3.0, 0.0, 2.0 / 3.0));
        assert!((p.x - 1.0 / 9.0).abs() < 1e-3, "{}", p.x);
    }

    #[test]
    fn tetrahedral_and_trilinear_agree_on_affine_tables() {
        let (text, _) = cube_text_and_rows(9, |r, g, b| [0.3 * r + 0.1, g, 2.0 * b - 0.5]);
        let lut = parse(&text).expect("affine table");
        for i in 0..8 {
            for j in 0..8 {
                let p = Vec3::new(i as f32 / 7.0, j as f32 / 7.0, (7 - i - j).max(0) as f32 / 7.0);
                let tri = lut.sample(p);
                let tet = lut.sample_with(p, CubeLutInterpolation::Tetrahedral);
                assert!(close(tri, tet, 1e-6), "{p}: {tri} vs {tet}");
            }
        }
    }

    #[test]
    fn tetrahedral_and_trilinear_agree_on_smooth_tables() {
        // Mixed terms (g·b, r·g) are where the two schemes differ; on a fine grid of a smooth
        // table the difference stays well below a thousandth.
        let (text, _) = cube_text_and_rows(65, |r, g, b| [r * r, g * b, r * g + 0.25 * b]);
        let lut = parse(&text).expect("smooth table");
        let mut max_diff = 0.0f32;
        for i in 0..7 {
            for j in 0..7 {
                for k in 0..7 {
                    let p = Vec3::new(i as f32 / 6.0, j as f32 / 6.0, k as f32 / 6.0);
                    let d = (lut.sample(p) - lut.sample_with(p, CubeLutInterpolation::Tetrahedral))
                        .abs()
                        .max_element();
                    max_diff = max_diff.max(d);
                }
            }
        }
        assert!(max_diff < 1.5e-3, "max difference {max_diff}");
    }

    #[test]
    fn tetrahedral_preserves_the_neutral_axis() {
        // Gray on the whole diagonal, warm off it; the warm terms are quadratic so trilinear
        // cannot reproduce them exactly either (the terms are strong enough that the final check
        // sees trilinear drift well above 1e-3 on a 17-node table).
        let f = |r: f32, g: f32, b: f32| {
            let m = (r + g + b) / 3.0;
            [
                m + 0.2 * (r - g) + 1.5 * (r - g) * (r - g),
                m,
                m + 0.15 * (b - m) + 2.0 * (b - m) * (b - m),
            ]
        };
        let (text, _) = cube_text_and_rows(17, f);
        let lut = parse(&text).expect("warm table");
        for i in 1..16 {
            let x = i as f32 / 19.0; // Never a grid node.
            let out = lut.sample_with(Vec3::splat(x), CubeLutInterpolation::Tetrahedral);
            assert!(
                (out.x - out.y).abs() < 1e-7 && (out.z - out.y).abs() < 1e-7,
                "{x}: {out}"
            );
        }
        // And the test is meaningful: trilinear on the same input breaks gray.
        let tri = lut.sample(Vec3::splat(5.0 / 19.0));
        assert!((tri.x - tri.y).abs() > 1e-3, "{tri}");
    }

    #[test]
    fn from_path_reads_a_file() {
        let dir = regia_test_utils::unique_temp_dir("lut-cube-read");
        let path = dir.join("table.cube");
        std::fs::write(&path, AFFINE_2_CUBE).expect("write fixture");
        let lut = CubeLut::from_path(&path).expect("read back");
        assert_eq!(lut.size(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- parsing: comments, line ends, keyword tolerance -------------------------------

    #[test]
    fn comments_blank_lines_and_trailing_comments() {
        let text = "TITLE \"t\" # the title\n# full-line comment\n\n\
            LUT_1D_SIZE 4 # four rows\n \
            0 0 0 # black\n0.5 0 0\n0 0.5 0\n0.5 0.5 0.5\n";
        let lut = parse(text).expect("commented file");
        assert_eq!(lut.title(), Some("t"));
        assert_eq!(lut.rows()[0], [0.0, 0.0, 0.0]);
        assert_eq!(lut.rows()[3], [0.5, 0.5, 0.5]);
    }

    #[test]
    fn title_hash_inside_quotes_truncates_harmlessly() {
        let text = "TITLE \"warm #grade\"\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let lut = parse(text).expect("title truncation is harmless");
        assert_eq!(lut.title(), Some("warm"));
    }

    #[test]
    fn title_keeps_spaces_and_empty_strings() {
        let text = "TITLE \"a b c\"\nTITLE \"\"\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let lut = parse(text).expect("titles");
        assert_eq!(lut.title(), Some("")); // The later title wins.
    }

    #[test]
    fn keywords_are_case_insensitive_and_unknown_ones_are_skipped() {
        let text = "title \"x\"\ndomain_min 0 0 0\nlut_1d_size 2\nMIN_RANGE 0 0 0\n\
            0 0 0\nBEST_VIEWED_ON 42\n0.1 0.2 0.3\n";
        let lut = parse(text).expect("lowercase and vendor keywords");
        assert_eq!(lut.title(), Some("x"));
        assert_eq!(lut.kind(), CubeLutKind::OneDimensional);
        assert_eq!(lut.rows().len(), 2);
    }

    #[test]
    fn signed_and_scientific_number_forms_parse() {
        let text = concat!("LUT_1D_SIZE 2\n", "+0.5 .25 1e-1\n", "-0.125 2 1E1\n");
        let lut = parse(text).expect("number forms");
        assert_eq!(lut.rows()[0], [0.5, 0.25, 0.1]);
        assert_eq!(lut.rows()[1], [-0.125, 2.0, 10.0]);
    }

    #[test]
    fn crlf_and_cr_line_endings_parse_like_lf() {
        let lf = "LUT_1D_SIZE 2\n0 0 0\n0.5 0.5 0.5\n";
        let crlf = "LUT_1D_SIZE 2\r\n0 0 0\r\n0.5 0.5 0.5\r\n";
        let cr = "LUT_1D_SIZE 2\r0 0 0\r0.5 0.5 0.5";
        let expected = parse(lf).expect("lf");
        assert_eq!(parse(crlf).expect("crlf"), expected);
        assert_eq!(parse(cr).expect("cr"), expected);
    }

    #[test]
    fn error_line_numbers_are_correct_with_crlf_and_cr() {
        let crlf = "TITLE \"ok\"\r\nLUT_3D_SIZE 2\r\nDOMAIN_MIN 0 0\r\n";
        let err = parse(crlf).expect_err("short domain");
        assert!(matches!(
            err,
            CubeLutError::DomainArity {
                line: 3,
                found: 2,
                ..
            }
        ));
        let cr = "LUT_3D_SIZE 2\r0 0 0\r0 z 0\r0 0 0\r0 0 0\r0 0 0\r0 0 0\r0 0 0\r0 0 0";
        let err = parse(cr).expect_err("bad row");
        assert!(matches!(err, CubeLutError::RowValue { line: 3, .. }));
        assert_eq!(err.line(), Some(3));
    }

    // ---- parsing: every error path ------------------------------------------------------

    #[test]
    fn error_title_without_quotes() {
        let err = parse("TITLE unquoted\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n").expect_err("unquoted");
        assert!(matches!(err, CubeLutError::Title { line: 1 }));
    }

    #[test]
    fn error_title_without_argument() {
        let err = parse("TITLE\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n").expect_err("bare title");
        assert!(matches!(err, CubeLutError::Title { line: 1 }));
        assert_eq!(err.line(), Some(1));
    }

    #[test]
    fn error_domain_min_too_few_numbers() {
        let text = "DOMAIN_MIN 0 0\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let err = parse(text).expect_err("short domain min");
        assert!(matches!(
            err,
            CubeLutError::DomainArity {
                line: 1,
                keyword: "DOMAIN_MIN",
                found: 2
            }
        ));
    }

    #[test]
    fn error_domain_max_without_numbers() {
        let text = "LUT_1D_SIZE 2\nDOMAIN_MAX\n0 0 0\n1 1 1\n";
        let err = parse(text).expect_err("bare domain max");
        assert!(matches!(
            err,
            CubeLutError::DomainArity {
                line: 2,
                keyword: "DOMAIN_MAX",
                found: 0
            }
        ));
    }

    #[test]
    fn error_domain_unparsable_number() {
        let text = "DOMAIN_MIN 0 zzz 0\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let err = parse(text).expect_err("unparsable domain");
        assert!(matches!(err, CubeLutError::DomainValue { line: 1, token, .. } if token == "zzz"));
    }

    #[test]
    fn error_domain_non_finite_number() {
        let text = "DOMAIN_MIN inf 0 0\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let err = parse(text).expect_err("infinite domain");
        assert!(matches!(err, CubeLutError::DomainValue { line: 1, token, .. } if token == "inf"));
    }

    #[test]
    fn domain_extra_numbers_are_ignored() {
        let text = "DOMAIN_MIN 0 0 0 99\nDOMAIN_MAX 1 1 1 99\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let lut = parse(text).expect("extra domain numbers ignored");
        assert_eq!(lut.domain_min(), [0.0, 0.0, 0.0]);
        assert_eq!(lut.domain_max(), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn error_domain_zero_span() {
        let text = "DOMAIN_MIN 0.5 0 0\nDOMAIN_MAX 0.5 1 1\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let err = parse(text).expect_err("zero span");
        assert!(matches!(err, CubeLutError::DomainRange { line: 2 }));
    }

    #[test]
    fn error_domain_reversed() {
        let text = "DOMAIN_MIN 1 1 1\nDOMAIN_MAX 0 0 0\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let err = parse(text).expect_err("reversed domain");
        assert!(matches!(err, CubeLutError::DomainRange { line: 2 }));
    }

    #[test]
    fn error_domain_min_at_default_max() {
        // Only DOMAIN_MIN declared, at or above the default max of 1.
        let text = "DOMAIN_MIN 0 0 1\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
        let err = parse(text).expect_err("span equals the default max");
        assert!(matches!(err, CubeLutError::DomainRange { line: 1 }));
    }

    #[test]
    fn error_size_not_an_integer() {
        for bad in ["thirty", "33.5", "-1", ""] {
            let text = format!(
                "LUT_3D_SIZE {bad}\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n\
                0 0 0\n0 0 0\n"
            );
            let err = parse(&text).expect_err("bad size value");
            assert!(
                matches!(
                    err,
                    CubeLutError::SizeValue {
                        line: 1,
                        keyword: "LUT_3D_SIZE",
                        ..
                    }
                ),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn error_size_below_two() {
        let err = parse("LUT_3D_SIZE 1\n").expect_err("tiny 3D");
        assert!(matches!(
            err,
            CubeLutError::SizeLimit {
                line: 1,
                keyword: "LUT_3D_SIZE",
                size: 1,
                min: LUT_3D_MIN_SIZE,
                max: LUT_3D_MAX_SIZE
            }
        ));
        let err = parse("LUT_1D_SIZE 1\n").expect_err("tiny 1D");
        assert!(matches!(err, CubeLutError::SizeLimit { size: 1, .. }));
    }

    #[test]
    fn error_size_above_max_is_refused_before_allocation() {
        let err = parse("LUT_3D_SIZE 257\n").expect_err("huge 3D");
        assert!(matches!(
            err,
            CubeLutError::SizeLimit {
                line: 1,
                keyword: "LUT_3D_SIZE",
                size: 257,
                min: LUT_3D_MIN_SIZE,
                max: LUT_3D_MAX_SIZE
            }
        ));
        let err = parse("LUT_1D_SIZE 65537\n").expect_err("huge 1D");
        assert!(matches!(err, CubeLutError::SizeLimit { size: 65_537, .. }));
    }

    #[test]
    fn max_sizes_are_accepted_and_only_fail_on_row_count() {
        // The Adobe maxima are inclusive: with no rows the failure is the row count, proving
        // the size itself was accepted (and nothing was allocated for it).
        let err = parse("LUT_1D_SIZE 65536\n").expect_err("no rows");
        assert!(matches!(
            err,
            CubeLutError::RowCount {
                expected: 65_536,
                found: 0,
                ..
            }
        ));
        let err = parse("LUT_3D_SIZE 256\n").expect_err("no rows");
        assert!(matches!(
            err,
            CubeLutError::RowCount {
                expected: 16_777_216,
                found: 0,
                ..
            }
        ));
    }

    #[test]
    fn error_size_repeated_same_kind() {
        let err = parse("LUT_3D_SIZE 2\nLUT_3D_SIZE 2\n").expect_err("twice");
        assert!(matches!(
            err,
            CubeLutError::SizeRepeated {
                line: 2,
                keyword: "LUT_3D_SIZE"
            }
        ));
    }

    #[test]
    fn error_both_size_kinds() {
        let err = parse("LUT_3D_SIZE 2\nLUT_1D_SIZE 2\n").expect_err("both kinds");
        assert!(matches!(
            err,
            CubeLutError::SizeRepeated {
                line: 2,
                keyword: "LUT_1D_SIZE"
            }
        ));
    }

    #[test]
    fn error_data_before_size() {
        let err = parse("0.0 0.0 0.0\nLUT_3D_SIZE 2\n").expect_err("data first");
        assert!(matches!(err, CubeLutError::DataBeforeSize { line: 1 }));
    }

    #[test]
    fn error_metadata_only_file_has_no_size() {
        assert!(matches!(
            parse("TITLE \"empty\"\n"),
            Err(CubeLutError::MissingSize)
        ));
    }

    #[test]
    fn error_empty_and_comment_only_files_have_no_size() {
        assert!(matches!(parse(""), Err(CubeLutError::MissingSize)));
        assert!(matches!(
            parse("# nothing\n\n# but comments\n"),
            Err(CubeLutError::MissingSize)
        ));
    }

    #[test]
    fn error_row_with_wrong_value_count() {
        let two = "LUT_1D_SIZE 2\n0 0\n1 1 1\n";
        let err = parse(two).expect_err("two values");
        assert!(matches!(err, CubeLutError::RowValues { line: 2, count: 2 }));
        let four = "LUT_1D_SIZE 2\n0 0 0 0\n1 1 1\n";
        let err = parse(four).expect_err("four values");
        assert!(matches!(err, CubeLutError::RowValues { line: 2, count: 4 }));
    }

    #[test]
    fn error_row_unparsable_value() {
        let err = parse("LUT_1D_SIZE 2\n0.1 zzz 0.2\n1 1 1\n").expect_err("unparsable row");
        assert!(matches!(err, CubeLutError::RowValue { line: 2, token, .. } if token == "zzz"));
    }

    #[test]
    fn error_row_non_finite_values() {
        // A leading non-finite word is data, not an unknown keyword.
        for (i, row) in ["nan 0.0 0.0", "0.0 inf 0.0", "0.0 0.0 1e999", "0.0 -inf 0.0"]
            .into_iter()
            .enumerate()
        {
            let text = format!("LUT_1D_SIZE 2\n{row}\n1 1 1\n");
            let err = parse(&text).expect_err("non-finite row");
            assert!(
                matches!(err, CubeLutError::RowValue { line: 2, .. }),
                "{i}: {err}"
            );
        }
    }

    #[test]
    fn error_too_many_rows() {
        let text = "LUT_1D_SIZE 2\n0 0 0\n1 1 1\n2 2 2\n";
        let err = parse(text).expect_err("extra row");
        assert!(matches!(
            err,
            CubeLutError::RowCount {
                line: 4,
                expected: 2,
                found: 3
            }
        ));
    }

    #[test]
    fn error_too_few_rows() {
        let mut text = String::from("LUT_3D_SIZE 3\n");
        for _ in 0..26 {
            text.push_str("0 0 0\n");
        }
        let err = parse(&text).expect_err("short table");
        assert!(matches!(
            err,
            CubeLutError::RowCount {
                expected: 27,
                found: 26,
                ..
            }
        ));
    }

    #[test]
    fn error_unreadable_file() {
        let dir = regia_test_utils::unique_temp_dir("lut-cube-io");
        let missing = CubeLut::from_path(&dir.join("missing.cube"));
        assert!(matches!(missing, Err(CubeLutError::Io(_))));
        assert_eq!(missing.expect_err("missing file").line(), None);
        // A directory is not a readable file either.
        let directory = CubeLut::from_path(&dir);
        assert!(matches!(directory, Err(CubeLutError::Io(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
