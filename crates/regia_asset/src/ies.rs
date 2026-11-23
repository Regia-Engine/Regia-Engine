// SPDX-License-Identifier: MPL-2.0
//! IES LM-63 photometric profiles (`docs/formats/ies-profile-v1.md`, ADR-2592).
//!
//! [`IesFile`] reads the public IESNA LM-63 text format (the 1986, 1991, 1995, 2002 and 2019 headers, `TILT=NONE` and
//! `TILT=INCLUDE`) and writes it back ([`IesFile::to_text`]). [`IesFile::candela`] looks up the file's luminous
//! intensity in a direction, unfolding the horizontal symmetry the file declares. [`CookedIesProfile`] is what the
//! renderer and the path tracer load: the profile resampled onto a fixed `θ × φ` grid of relative intensities, with
//! its peak and its integrated flux (`FIES` v1, [`IES_PROFILE_FORMAT_VERSION`]).
//!
//! Directions are in the luminaire's photometric frame: `-Z` is the nadir (vertical angle 0°), `+X` is the
//! horizontal angle 0° and `+Y` is 90°. A spot light's direction is the nadir.

use std::fmt::Write as _;

use thiserror::Error;

/// Version of the cooked profile layout (`FIES`, [`CookedIesProfile`]).
pub const IES_PROFILE_FORMAT_VERSION: u32 = 1;

/// Magic of a cooked profile payload.
pub const IES_PROFILE_MAGIC: [u8; 4] = *b"FIES";

/// Reflect type path of a cooked profile in a cooked file header (`CookedHeader::type_tag`).
pub const IES_PROFILE_TYPE_PATH: &str = "regia_asset::ies::CookedIesProfile";

/// Vertical samples of a cooked profile: `θ_i = i · 180° / (V − 1)`, nadir to zenith.
pub const COOKED_VERTICAL: usize = 256;

/// Horizontal samples of a cooked profile: `φ_j = j · 360° / H`, wrapping.
pub const COOKED_HORIZONTAL: usize = 32;

/// Largest vertical or horizontal angle count a file may declare.
pub const MAX_ANGLES: usize = 4096;

/// Largest candela table (`vertical × horizontal`) a file may declare.
pub const MAX_CANDELA_VALUES: usize = 1 << 20;

/// Bytes of the fixed header of a cooked profile.
const COOKED_HEADER_BYTES: usize = 4 + 4 + 4 + 4 + 4 * 6 + 4;

/// Why an IES file or a cooked profile could not be read.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum IesError {
    /// The text ended before a required value.
    #[error("ies: unexpected end of file while reading {0}")]
    Truncated(&'static str),
    /// A number could not be parsed.
    #[error("ies: `{token}` is not a number ({what})")]
    BadNumber {
        /// The offending token.
        token: String,
        /// What was being read.
        what: &'static str,
    },
    /// No `TILT=` line.
    #[error("ies: no TILT= line")]
    NoTilt,
    /// `TILT=<file>`: the tilt data lives in another file, which a single-file import cannot read.
    #[error("ies: TILT={0} names an external file; only TILT=NONE and TILT=INCLUDE are read")]
    ExternalTilt(String),
    /// A header value is out of its legal range.
    #[error("ies: {0}")]
    Invalid(String),
    /// A cooked payload is malformed.
    #[error("ies cooked profile: {0}")]
    Cooked(String),
}

/// Photometric type (LM-63 field 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotometricType {
    /// Type C: vertical angles from the nadir, horizontal angles around the vertical axis (architectural).
    C,
    /// Type B: horizontal and vertical angles about a horizontal axis (floodlights).
    B,
    /// Type A: as B, rotated (automotive).
    A,
}

impl PhotometricType {
    fn code(self) -> u32 {
        match self {
            Self::C => 1,
            Self::B => 2,
            Self::A => 3,
        }
    }
}

/// `TILT=INCLUDE` data (LM-63 § tilt): candela multipliers by lamp tilt angle. Every candela value of the file
/// takes the factor at the luminaire's installed tilt ([`IesFile::tilt_factor`], ADR-3247): the plain lookups
/// ([`IesFile::candela`], [`IesFile::candela_at_angles`], [`IesFile::peak_candela`]) at a tilt of 0° (the luminaire
/// hung as photometered), the `_tilted` ones at the angle given.
#[derive(Clone, Debug, PartialEq)]
pub struct IesTilt {
    /// Lamp-to-luminaire geometry code (1, 2 or 3).
    pub geometry: u32,
    /// Tilt angles, degrees.
    pub angles: Vec<f32>,
    /// One multiplying factor per angle.
    pub factors: Vec<f32>,
}

/// A parsed LM-63 file.
#[derive(Clone, Debug, PartialEq)]
pub struct IesFile {
    /// The first line when it names the standard (`IESNA:LM-63-2002`, `IESNA91`, ...); empty for a 1986 file.
    pub standard: String,
    /// `[KEYWORD] value` lines in file order (`[TEST]`, `[MANUFAC]`, ...), keyword without brackets.
    pub keywords: Vec<(String, String)>,
    /// `TILT=INCLUDE` data; `None` for `TILT=NONE`.
    pub tilt: Option<IesTilt>,
    /// Number of lamps.
    pub lamp_count: u32,
    /// Lumens per lamp; −1 for absolute photometry (the candela values are the luminaire's own).
    pub lumens_per_lamp: f32,
    /// Candela multiplier.
    pub candela_multiplier: f32,
    /// Photometric type.
    pub photometric_type: PhotometricType,
    /// Units of the luminous opening: 1 feet, 2 metres.
    pub units: u32,
    /// Luminous opening width, length, height in `units` (negative values name round shapes, LM-63 § 5.11).
    pub opening: [f32; 3],
    /// Ballast factor.
    pub ballast_factor: f32,
    /// Ballast-lamp photometric factor (LM-63-1995); "future use", normally 1, from 2002 on.
    pub ballast_lamp_factor: f32,
    /// Input watts.
    pub input_watts: f32,
    /// Vertical angles, degrees, ascending.
    pub vertical: Vec<f32>,
    /// Horizontal angles, degrees, ascending.
    pub horizontal: Vec<f32>,
    /// Candela values as written, `horizontal.len()` rows of `vertical.len()` values.
    pub candela: Vec<f32>,
}

/// Whitespace- and comma-separated numbers across lines.
struct Numbers<'a> {
    tokens: std::iter::Peekable<Box<dyn Iterator<Item = &'a str> + 'a>>,
}

impl<'a> Numbers<'a> {
    fn new(lines: &'a [&'a str]) -> Self {
        let it: Box<dyn Iterator<Item = &'a str> + 'a> = Box::new(
            lines
                .iter()
                .flat_map(|l| l.split(|c: char| c.is_whitespace() || c == ','))
                .filter(|t| !t.is_empty()),
        );
        Self {
            tokens: it.peekable(),
        }
    }

    fn f32(&mut self, what: &'static str) -> Result<f32, IesError> {
        let token = self.tokens.next().ok_or(IesError::Truncated(what))?;
        let value: f32 = token.parse().map_err(|_| IesError::BadNumber {
            token: token.chars().take(32).collect(),
            what,
        })?;
        if !value.is_finite() {
            return Err(IesError::BadNumber {
                token: token.chars().take(32).collect(),
                what,
            });
        }
        Ok(value)
    }

    /// An integer field; written as `1` or `1.0` in the wild.
    fn count(&mut self, what: &'static str, max: usize) -> Result<usize, IesError> {
        let v = self.f32(what)?;
        if v < 0.0 || v.fract() != 0.0 || v > max as f32 {
            return Err(IesError::Invalid(format!("{what} = {v} (0..={max}, whole)")));
        }
        Ok(v as usize)
    }

    fn list(&mut self, n: usize, what: &'static str) -> Result<Vec<f32>, IesError> {
        (0..n).map(|_| self.f32(what)).collect()
    }

    fn is_empty(&mut self) -> bool {
        self.tokens.peek().is_none()
    }
}

fn ascending(angles: &[f32], what: &str) -> Result<(), IesError> {
    if angles.windows(2).any(|w| w[1] <= w[0]) {
        return Err(IesError::Invalid(format!(
            "{what} angles are not strictly ascending"
        )));
    }
    Ok(())
}

impl IesFile {
    /// Parses LM-63 text.
    ///
    /// # Errors
    /// [`IesError`] for a missing `TILT=` line, an external tilt file, a malformed or out-of-range number, a
    /// truncated table, angles out of their legal ranges or not ascending, and trailing numbers.
    pub fn parse(text: &str) -> Result<Self, IesError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        let mut standard = String::new();
        if let Some(first) = lines.first() {
            let t = first.trim();
            if t.starts_with("IESNA") || t.starts_with("IES:") {
                standard = t.to_string();
                i = 1;
            }
        }
        let mut keywords = Vec::new();
        let tilt_line = loop {
            let Some(line) = lines.get(i) else {
                return Err(IesError::NoTilt);
            };
            i += 1;
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("TILT=").or_else(|| t.strip_prefix("TILT =")) {
                break rest.trim().to_string();
            }
            if let Some(rest) = t.strip_prefix('[')
                && let Some(end) = rest.find(']')
            {
                let (key, value) = (rest[..end].to_string(), rest[end + 1..].trim().to_string());
                // `[]` alone carries nothing and would not survive a rewrite.
                if !(key.is_empty() && value.is_empty()) {
                    keywords.push((key, value));
                }
            } else if !t.is_empty() {
                // A 1986 file's free-form label lines.
                keywords.push((String::new(), t.to_string()));
            }
        };
        let rest = &lines[i.min(lines.len())..];
        let mut n = Numbers::new(rest);
        let tilt = match tilt_line.as_str() {
            "NONE" => None,
            "INCLUDE" => {
                let geometry = n.count("tilt lamp-to-luminaire geometry", 3)? as u32;
                let count = n.count("tilt angle count", MAX_ANGLES)?;
                let angles = n.list(count, "tilt angles")?;
                let factors = n.list(count, "tilt factors")?;
                Some(IesTilt {
                    geometry,
                    angles,
                    factors,
                })
            }
            other => return Err(IesError::ExternalTilt(other.chars().take(64).collect())),
        };
        let lamp_count = n.count("lamp count", 1_000_000)? as u32;
        let lumens_per_lamp = n.f32("lumens per lamp")?;
        let candela_multiplier = n.f32("candela multiplier")?;
        let nv = n.count("vertical angle count", MAX_ANGLES)?;
        let nh = n.count("horizontal angle count", MAX_ANGLES)?;
        let photometric_type = match n.count("photometric type", 3)? {
            1 => PhotometricType::C,
            2 => PhotometricType::B,
            3 => PhotometricType::A,
            t => return Err(IesError::Invalid(format!("photometric type {t} (1 C, 2 B, 3 A)"))),
        };
        let units = n.count("units type", 2)? as u32;
        if units == 0 {
            return Err(IesError::Invalid("units type 0 (1 feet, 2 metres)".into()));
        }
        let opening = [n.f32("width")?, n.f32("length")?, n.f32("height")?];
        let ballast_factor = n.f32("ballast factor")?;
        let ballast_lamp_factor = n.f32("ballast-lamp photometric factor")?;
        let input_watts = n.f32("input watts")?;
        if nv == 0 || nh == 0 || nv * nh > MAX_CANDELA_VALUES {
            return Err(IesError::Invalid(format!(
                "{nv} × {nh} candela table (each ≥ 1, at most {MAX_CANDELA_VALUES} values)"
            )));
        }
        let vertical = n.list(nv, "vertical angles")?;
        let horizontal = n.list(nh, "horizontal angles")?;
        let candela = n.list(nv * nh, "candela values")?;
        if !n.is_empty() {
            return Err(IesError::Invalid("numbers after the candela table".into()));
        }
        let file = Self {
            standard,
            keywords,
            tilt,
            lamp_count,
            lumens_per_lamp,
            candela_multiplier,
            photometric_type,
            units,
            opening,
            ballast_factor,
            ballast_lamp_factor,
            input_watts,
            vertical,
            horizontal,
            candela,
        };
        file.validate()?;
        Ok(file)
    }

    /// Checks the angle ranges and the table shape.
    ///
    /// # Errors
    /// [`IesError::Invalid`] naming the first problem.
    pub fn validate(&self) -> Result<(), IesError> {
        ascending(&self.vertical, "vertical")?;
        ascending(&self.horizontal, "horizontal")?;
        if self.candela.len() != self.vertical.len() * self.horizontal.len() {
            return Err(IesError::Invalid("candela table size".into()));
        }
        if self.candela.iter().any(|c| !c.is_finite() || *c < 0.0) {
            return Err(IesError::Invalid(
                "a candela value is negative or not finite".into(),
            ));
        }
        if !(self.candela_multiplier.is_finite() && self.candela_multiplier >= 0.0) {
            return Err(IesError::Invalid("candela multiplier is negative".into()));
        }
        if let Some(tilt) = &self.tilt {
            if tilt.angles.len() != tilt.factors.len() {
                return Err(IesError::Invalid("tilt angle and factor counts differ".into()));
            }
            if tilt.angles.windows(2).any(|w| w[1] < w[0]) {
                return Err(IesError::Invalid("tilt angles are not ascending".into()));
            }
            if tilt.factors.iter().any(|f| !f.is_finite() || *f < 0.0) {
                return Err(IesError::Invalid(
                    "a tilt factor is negative or not finite".into(),
                ));
            }
        }
        let (v0, v1) = (self.vertical[0], self.vertical[self.vertical.len() - 1]);
        let (h0, h1) = (self.horizontal[0], self.horizontal[self.horizontal.len() - 1]);
        match self.photometric_type {
            PhotometricType::C => {
                if v0 < 0.0 || v1 > 180.0 {
                    return Err(IesError::Invalid(format!(
                        "type C vertical angles {v0}..{v1} (0..=180)"
                    )));
                }
                if h0 < 0.0 || h1 > 360.0 {
                    return Err(IesError::Invalid(format!(
                        "type C horizontal angles {h0}..{h1} (0..=360)"
                    )));
                }
            }
            PhotometricType::A | PhotometricType::B => {
                if v0 < -90.0 || v1 > 90.0 || h0 < -90.0 || h1 > 90.0 {
                    return Err(IesError::Invalid("type A/B angles outside -90..=90".into()));
                }
            }
        }
        Ok(())
    }

    /// The scale every written candela value takes: multiplier × ballast factor × ballast-lamp factor.
    #[must_use]
    pub fn candela_scale(&self) -> f32 {
        self.candela_multiplier * self.ballast_factor * self.ballast_lamp_factor
    }

    /// The `TILT=INCLUDE` multiplier for a luminaire installed `tilt_deg` degrees from the orientation it was
    /// photometered in (LM-63 § tilt; 1 for `TILT=NONE` or an empty table): linear between the table's angles,
    /// the end factors past them. A table that starts at 0° or above is symmetric in the sign of the tilt.
    #[must_use]
    pub fn tilt_factor(&self, tilt_deg: f32) -> f32 {
        let Some(tilt) = &self.tilt else { return 1.0 };
        let n = tilt.angles.len().min(tilt.factors.len());
        if n == 0 || !tilt_deg.is_finite() {
            return 1.0;
        }
        let (angles, factors) = (&tilt.angles[..n], &tilt.factors[..n]);
        let t = if angles[0] >= 0.0 {
            tilt_deg.abs()
        } else {
            tilt_deg
        };
        if t <= angles[0] {
            return factors[0];
        }
        if t >= angles[n - 1] {
            return factors[n - 1];
        }
        let i = angles.partition_point(|&a| a <= t).clamp(1, n - 1);
        let (a, b) = (angles[i - 1], angles[i]);
        let w = if b > a { (t - a) / (b - a) } else { 1.0 };
        factors[i - 1] + (factors[i] - factors[i - 1]) * w.clamp(0.0, 1.0)
    }

    /// The written value at horizontal index `h`, vertical index `v`, scaled ([`Self::candela_scale`]).
    #[must_use]
    pub fn value(&self, h: usize, v: usize) -> f32 {
        self.candela
            .get(h * self.vertical.len() + v)
            .copied()
            .unwrap_or(0.0)
            * self.candela_scale()
    }

    /// Writes LM-63-2002 text that [`Self::parse`] reads back to an equal file.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if !self.standard.is_empty() {
            out.push_str(&self.standard);
            out.push('\n');
        }
        for (k, v) in &self.keywords {
            if k.is_empty() {
                let _ = writeln!(out, "{v}");
            } else {
                let _ = writeln!(out, "[{k}] {v}");
            }
        }
        let list = |out: &mut String, values: &[f32]| {
            for chunk in values.chunks(10) {
                let line: Vec<String> = chunk.iter().map(|v| format!("{v:?}")).collect();
                let _ = writeln!(out, "{}", line.join(" "));
            }
        };
        match &self.tilt {
            None => out.push_str("TILT=NONE\n"),
            Some(t) => {
                out.push_str("TILT=INCLUDE\n");
                let _ = writeln!(out, "{}\n{}", t.geometry, t.angles.len());
                list(&mut out, &t.angles);
                list(&mut out, &t.factors);
            }
        }
        let _ = writeln!(
            out,
            "{} {:?} {:?} {} {} {} {} {:?} {:?} {:?}",
            self.lamp_count,
            self.lumens_per_lamp,
            self.candela_multiplier,
            self.vertical.len(),
            self.horizontal.len(),
            self.photometric_type.code(),
            self.units,
            self.opening[0],
            self.opening[1],
            self.opening[2]
        );
        let _ = writeln!(
            out,
            "{:?} {:?} {:?}",
            self.ballast_factor, self.ballast_lamp_factor, self.input_watts
        );
        list(&mut out, &self.vertical);
        list(&mut out, &self.horizontal);
        for row in self.candela.chunks(self.vertical.len().max(1)) {
            list(&mut out, row);
        }
        out
    }

    /// The file's horizontal angle for an unfolded angle `phi` in `[0, 360)` (LM-63 § 5.15 symmetries): one angle
    /// is rotational symmetry, a last angle of 90 quadrant symmetry, of 180 bilateral symmetry about the 0-180
    /// plane, a range 90..270 bilateral symmetry about the 90-270 plane, and 0..360 no symmetry.
    #[must_use]
    pub fn fold_horizontal(&self, phi: f32) -> f32 {
        let h0 = self.horizontal[0];
        let h1 = self.horizontal[self.horizontal.len() - 1];
        let p = phi.rem_euclid(360.0);
        if self.horizontal.len() == 1 {
            return h0;
        }
        if h0 >= 90.0 - 1e-3 && h1 <= 270.0 + 1e-3 && h1 - h0 > 1e-3 && (h1 - h0 - 180.0).abs() < 1e-3 {
            // 90..270: mirror about the 90-270 plane.
            return if p < 90.0 {
                180.0 - p
            } else if p > 270.0 {
                540.0 - p
            } else {
                p
            };
        }
        if (h1 - 90.0).abs() < 1e-3 {
            let q = if p > 180.0 { 360.0 - p } else { p };
            return if q > 90.0 { 180.0 - q } else { q };
        }
        if (h1 - 180.0).abs() < 1e-3 {
            return if p > 180.0 { 360.0 - p } else { p };
        }
        p
    }

    /// Luminous intensity (cd, scaled by [`Self::candela_scale`] and the tilt factor at 0°, [`Self::tilt_factor`])
    /// at vertical angle `theta` and horizontal angle `phi` (degrees, type C), interpolated linearly in both angles.
    /// 0 outside the vertical range.
    #[must_use]
    pub fn candela_at_angles(&self, theta: f32, phi: f32) -> f32 {
        self.candela_at_angles_tilted(theta, phi, 0.0)
    }

    /// [`Self::candela_at_angles`] for a luminaire installed at `tilt_deg` ([`Self::tilt_factor`]).
    #[must_use]
    pub fn candela_at_angles_tilted(&self, theta: f32, phi: f32, tilt_deg: f32) -> f32 {
        self.untilted_at_angles(theta, phi) * self.tilt_factor(tilt_deg)
    }

    fn untilted_at_angles(&self, theta: f32, phi: f32) -> f32 {
        let theta = theta.clamp(0.0, 180.0);
        let Some((v0, v1, tv)) = bracket(&self.vertical, theta, false) else {
            return 0.0;
        };
        let h = self.fold_horizontal(phi);
        let full = self.horizontal.len() > 1
            && (self.horizontal[self.horizontal.len() - 1] - self.horizontal[0] - 360.0).abs() < 1e-3;
        let (h0, h1, th) = if self.horizontal.len() == 1 {
            (0, 0, 0.0)
        } else {
            match bracket(&self.horizontal, h, full) {
                Some(b) => b,
                None => {
                    // A partial horizontal range the folding did not cover: nearest end.
                    let last = self.horizontal.len() - 1;
                    let i = if h < self.horizontal[0] { 0 } else { last };
                    (i, i, 0.0)
                }
            }
        };
        let at = |hh: usize| self.value(hh, v0) * (1.0 - tv) + self.value(hh, v1) * tv;
        at(h0) * (1.0 - th) + at(h1) * th
    }

    /// Luminous intensity (cd) toward `dir` in the photometric frame (`-Z` nadir, `+X` at 0°, `+Y` at 90°), with
    /// the tilt factor at 0° ([`Self::tilt_factor`]). Type A and B files read their angles about the frame's `X`
    /// axis (B) or `Y` axis (A) instead.
    #[must_use]
    pub fn candela(&self, dir: [f32; 3]) -> f32 {
        self.candela_tilted(dir, 0.0)
    }

    /// [`Self::candela`] for a luminaire installed at `tilt_deg` ([`Self::tilt_factor`]).
    #[must_use]
    pub fn candela_tilted(&self, dir: [f32; 3], tilt_deg: f32) -> f32 {
        self.untilted(dir) * self.tilt_factor(tilt_deg)
    }

    fn untilted(&self, dir: [f32; 3]) -> f32 {
        let (theta, phi) = match self.photometric_type {
            PhotometricType::C => angles_c(dir),
            PhotometricType::B | PhotometricType::A => {
                let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2])
                    .sqrt()
                    .max(1e-12);
                let d = [dir[0] / len, dir[1] / len, dir[2] / len];
                // Vertical: elevation above the horizontal plane; horizontal: angle about the vertical axis,
                // 0 along the nadir (the luminaire aims along -Z).
                let (lat, elev) = if self.photometric_type == PhotometricType::B {
                    (d[1].atan2(-d[2]), d[0].clamp(-1.0, 1.0).asin())
                } else {
                    (d[0].atan2(-d[2]), d[1].clamp(-1.0, 1.0).asin())
                };
                if lat.abs() > std::f32::consts::FRAC_PI_2 {
                    return 0.0;
                }
                return self.candela_ab(elev.to_degrees(), lat.to_degrees());
            }
        };
        self.untilted_at_angles(theta, phi)
    }

    fn candela_ab(&self, vertical: f32, horizontal: f32) -> f32 {
        let Some((v0, v1, tv)) = bracket(&self.vertical, vertical, false) else {
            return 0.0;
        };
        // Files with only 0..90 horizontal angles are symmetric about 0.
        let h = if self.horizontal[0] >= 0.0 {
            horizontal.abs()
        } else {
            horizontal
        };
        let (h0, h1, th) = if self.horizontal.len() == 1 {
            (0, 0, 0.0)
        } else {
            match bracket(&self.horizontal, h, false) {
                Some(b) => b,
                None => return 0.0,
            }
        };
        let at = |hh: usize| self.value(hh, v0) * (1.0 - tv) + self.value(hh, v1) * tv;
        at(h0) * (1.0 - th) + at(h1) * th
    }

    /// Largest scaled candela value in the table, with the tilt factor at 0° ([`Self::tilt_factor`]).
    #[must_use]
    pub fn peak_candela(&self) -> f32 {
        self.peak_candela_tilted(0.0)
    }

    /// [`Self::peak_candela`] for a luminaire installed at `tilt_deg`.
    #[must_use]
    pub fn peak_candela_tilted(&self, tilt_deg: f32) -> f32 {
        self.candela.iter().fold(0.0f32, |m, &c| m.max(c)) * self.candela_scale() * self.tilt_factor(tilt_deg)
    }

    /// The luminous opening in metres (absolute values; feet converted).
    #[must_use]
    pub fn opening_m(&self) -> [f32; 3] {
        let k = if self.units == 1 { 0.3048 } else { 1.0 };
        self.opening.map(|v| v.abs() * k)
    }
}

/// Vertical and horizontal angle (degrees) of `dir` in the type C frame.
#[must_use]
pub fn angles_c(dir: [f32; 3]) -> (f32, f32) {
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2])
        .sqrt()
        .max(1e-12);
    let cos_t = (-dir[2] / len).clamp(-1.0, 1.0);
    let theta = cos_t.acos().to_degrees();
    let phi = dir[1].atan2(dir[0]).to_degrees().rem_euclid(360.0);
    (theta, phi)
}

/// The two samples of ascending `xs` around `x` and the weight of the second; `wrap` closes the range at 360°.
fn bracket(xs: &[f32], x: f32, wrap: bool) -> Option<(usize, usize, f32)> {
    let last = xs.len() - 1;
    if xs.len() == 1 {
        return ((x - xs[0]).abs() < 1e-3).then_some((0, 0, 0.0));
    }
    if x < xs[0] - 1e-4 || x > xs[last] + 1e-4 {
        if wrap {
            return Some((last, 0, 0.0));
        }
        return None;
    }
    let x = x.clamp(xs[0], xs[last]);
    let i = xs.partition_point(|&a| a <= x).clamp(1, last);
    let (a, b) = (xs[i - 1], xs[i]);
    let t = if b > a { (x - a) / (b - a) } else { 0.0 };
    Some((i - 1, i, t.clamp(0.0, 1.0)))
}

/// A profile resampled for the renderer and the path tracer: [`COOKED_VERTICAL`] × [`COOKED_HORIZONTAL`] relative
/// intensities (peak 1) in the type C frame.
#[derive(Clone, Debug, PartialEq)]
pub struct CookedIesProfile {
    /// Peak luminous intensity of the file, cd (the value a relative 1.0 stands for).
    pub peak_candela: f32,
    /// Luminous flux of the resampled profile at its file intensities, lm (∫ I dΩ).
    pub flux_lm: f32,
    /// Luminous opening, metres (width, length, height).
    pub opening_m: [f32; 3],
    /// The vertical range of the file, degrees: outside it the profile is dark.
    pub vertical_range: [f32; 2],
    /// Relative intensities, `COOKED_HORIZONTAL` rows of `COOKED_VERTICAL` values in `[0, 1]`.
    pub values: Vec<f32>,
}

impl CookedIesProfile {
    /// Resamples `file`. A type A or B file is resampled through [`IesFile::candela`] in the type C frame. The
    /// luminaire hangs as photometered: a `TILT=INCLUDE` table scales the peak by its factor at 0°
    /// ([`Self::from_file_tilted`]).
    ///
    /// # Errors
    /// [`IesError::Invalid`] when the file has no light (every value 0).
    pub fn from_file(file: &IesFile) -> Result<Self, IesError> {
        Self::from_file_tilted(file, 0.0)
    }

    /// [`Self::from_file`] for a luminaire installed `tilt_deg` from its photometered orientation: the tilt factor
    /// ([`IesFile::tilt_factor`]) multiplies every candela value alike, so it scales the peak and the flux and
    /// leaves the relative values as they are.
    ///
    /// # Errors
    /// [`IesError::Invalid`] when the file has no light at that tilt.
    pub fn from_file_tilted(file: &IesFile, tilt_deg: f32) -> Result<Self, IesError> {
        let peak = file.peak_candela_tilted(tilt_deg);
        if !(peak.is_finite() && peak > 0.0) {
            return Err(IesError::Invalid("the profile emits no light (peak 0)".into()));
        }
        let mut values = vec![0.0f32; COOKED_VERTICAL * COOKED_HORIZONTAL];
        for j in 0..COOKED_HORIZONTAL {
            let phi = j as f32 * 360.0 / COOKED_HORIZONTAL as f32;
            for i in 0..COOKED_VERTICAL {
                let theta = i as f32 * 180.0 / (COOKED_VERTICAL - 1) as f32;
                let cd = if file.photometric_type == PhotometricType::C {
                    file.candela_at_angles_tilted(theta, phi, tilt_deg)
                } else {
                    let (st, ct) = theta.to_radians().sin_cos();
                    let (sp, cp) = phi.to_radians().sin_cos();
                    file.candela_tilted([st * cp, st * sp, -ct], tilt_deg)
                };
                values[j * COOKED_VERTICAL + i] = (cd / peak).clamp(0.0, 1.0);
            }
        }
        let vertical_range = match file.photometric_type {
            PhotometricType::C => [file.vertical[0], file.vertical[file.vertical.len() - 1]],
            _ => [0.0, 180.0],
        };
        let mut cooked = Self {
            peak_candela: peak,
            flux_lm: 0.0,
            opening_m: file.opening_m(),
            vertical_range,
            values,
        };
        cooked.flux_lm = cooked.relative_flux() * peak;
        Ok(cooked)
    }

    /// ∫ relative intensity dΩ (sr): the flux of the profile at a peak of 1 cd. Midpoint rule over the grid's
    /// `θ` bands, with the bilinear lookup the shader uses.
    #[must_use]
    pub fn relative_flux(&self) -> f32 {
        let bands = 512usize;
        let cols = 64usize;
        let mut sum = 0.0f64;
        for b in 0..bands {
            let t0 = std::f64::consts::PI * b as f64 / bands as f64;
            let t1 = std::f64::consts::PI * (b + 1) as f64 / bands as f64;
            let ring = 2.0 * std::f64::consts::PI * (t0.cos() - t1.cos()) / cols as f64;
            let theta = (0.5 * (t0 + t1)).to_degrees() as f32;
            for c in 0..cols {
                let phi = (c as f32 + 0.5) * 360.0 / cols as f32;
                sum += f64::from(self.relative_at_angles(theta, phi)) * ring;
            }
        }
        sum as f32
    }

    /// Relative intensity at vertical angle `theta` and horizontal angle `phi` (degrees): bilinear on the grid,
    /// wrapping in `φ`. The CPU mirror of the shader's `ies_relative`.
    #[must_use]
    pub fn relative_at_angles(&self, theta: f32, phi: f32) -> f32 {
        let x = (theta.clamp(0.0, 180.0) / 180.0) * (COOKED_VERTICAL - 1) as f32;
        let y = phi.rem_euclid(360.0) / 360.0 * COOKED_HORIZONTAL as f32;
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let xi0 = (x0 as usize).min(COOKED_VERTICAL - 1);
        let xi1 = (xi0 + 1).min(COOKED_VERTICAL - 1);
        let yi0 = (y0 as usize) % COOKED_HORIZONTAL;
        let yi1 = (yi0 + 1) % COOKED_HORIZONTAL;
        let v = |xi: usize, yi: usize| self.values.get(yi * COOKED_VERTICAL + xi).copied().unwrap_or(0.0);
        let a = v(xi0, yi0) * (1.0 - fx) + v(xi1, yi0) * fx;
        let b = v(xi0, yi1) * (1.0 - fx) + v(xi1, yi1) * fx;
        a * (1.0 - fy) + b * fy
    }

    /// Relative intensity toward `dir` in the photometric frame (`-Z` nadir).
    #[must_use]
    pub fn relative(&self, dir: [f32; 3]) -> f32 {
        let (theta, phi) = angles_c(dir);
        self.relative_at_angles(theta, phi)
    }

    /// The payload bytes (`docs/formats/ies-profile-v1.md`).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(COOKED_HEADER_BYTES + self.values.len() * 4);
        out.extend_from_slice(&IES_PROFILE_MAGIC);
        out.extend_from_slice(&IES_PROFILE_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(COOKED_VERTICAL as u32).to_le_bytes());
        out.extend_from_slice(&(COOKED_HORIZONTAL as u32).to_le_bytes());
        for v in [
            self.peak_candela,
            self.flux_lm,
            self.opening_m[0],
            self.opening_m[1],
            self.opening_m[2],
            self.vertical_range[0],
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.vertical_range[1].to_le_bytes());
        for v in &self.values {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Reads a payload written by [`Self::to_bytes`].
    ///
    /// # Errors
    /// [`IesError::Cooked`] for a bad magic, another version or grid size, a truncated payload, trailing bytes,
    /// a non-finite or negative header value, and a value outside `[0, 1]`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IesError> {
        let bad = |m: &str| IesError::Cooked(m.to_string());
        if bytes.len() < COOKED_HEADER_BYTES {
            return Err(bad("truncated header"));
        }
        if bytes[..4] != IES_PROFILE_MAGIC {
            return Err(bad("bad magic"));
        }
        let word = |i: usize| -> [u8; 4] {
            let mut w = [0u8; 4];
            w.copy_from_slice(&bytes[i..i + 4]);
            w
        };
        let version = u32::from_le_bytes(word(4));
        if version != IES_PROFILE_FORMAT_VERSION {
            return Err(IesError::Cooked(format!(
                "version {version} (this build reads {IES_PROFILE_FORMAT_VERSION})"
            )));
        }
        let (nv, nh) = (u32::from_le_bytes(word(8)), u32::from_le_bytes(word(12)));
        if nv as usize != COOKED_VERTICAL || nh as usize != COOKED_HORIZONTAL {
            return Err(IesError::Cooked(format!(
                "grid {nv} × {nh} (v1 is {COOKED_VERTICAL} × {COOKED_HORIZONTAL})"
            )));
        }
        let f = |i: usize| f32::from_le_bytes(word(i));
        let header: Vec<f32> = (0..7).map(|k| f(16 + 4 * k)).collect();
        if header.iter().any(|v| !v.is_finite()) {
            return Err(bad("a header value is not finite"));
        }
        let expected = COOKED_HEADER_BYTES + COOKED_VERTICAL * COOKED_HORIZONTAL * 4;
        if bytes.len() != expected {
            return Err(IesError::Cooked(format!(
                "{} bytes, expected {expected}",
                bytes.len()
            )));
        }
        let (words, _) = bytes[COOKED_HEADER_BYTES..].as_chunks::<4>();
        let values: Vec<f32> = words.iter().map(|c| f32::from_le_bytes(*c)).collect();
        if values.iter().any(|v| !(0.0..=1.0).contains(v)) {
            return Err(bad("a relative intensity is outside [0, 1] or not finite"));
        }
        if header[0] <= 0.0 || header[1] < 0.0 || header[2..5].iter().any(|v| *v < 0.0) {
            return Err(bad("negative peak, flux or opening"));
        }
        Ok(Self {
            peak_candela: header[0],
            flux_lm: header[1],
            opening_m: [header[2], header[3], header[4]],
            vertical_range: [header[5], header[6]],
            values,
        })
    }
}

/// Fingerprint of the cooked layout for `cargo xtask check-formats` (the payload has no serde shape).
#[must_use]
pub fn ies_profile_schema_fingerprint() -> String {
    format!(
        "FIES;v{IES_PROFILE_FORMAT_VERSION};u32 vertical({COOKED_VERTICAL});u32 horizontal({COOKED_HORIZONTAL});\
         f32 peak_candela;f32 flux_lm;f32x3 opening_m;f32x2 vertical_range;f32[h][v] relative"
    )
}

/// The `from` versions of the cooked profile's migration steps (none: v1 is the first, and a cooked profile is
/// derived data the importer re-cooks from the `.ies` source).
#[must_use]
pub fn ies_profile_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The `FIES` version of a cooked file in the dev cache, `None` when the file holds another type
/// (`regia upgrade-project`).
///
/// # Errors
/// A cooked profile with a bad magic.
pub fn cooked_ies_profile_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let Ok((header, payload)) = crate::decode_cooked_file(bytes) else {
        return Ok(None);
    };
    if header.type_tag != regia_core::id::TypePathHash::of(IES_PROFILE_TYPE_PATH) {
        return Ok(None);
    }
    match payload {
        [m0, m1, m2, m3, v0, v1, v2, v3, ..] if [*m0, *m1, *m2, *m3] == IES_PROFILE_MAGIC => {
            Ok(Some(u32::from_le_bytes([*v0, *v1, *v2, *v3])))
        }
        _ => Err("cooked IES profile has a bad magic".to_string()),
    }
}

/// A small type C test profile: a downlight whose intensity falls as `cos²θ` to 0 at 60°, brighter toward
/// `+X` (horizontal 0°) than `-X` (180°), bilateral symmetry (horizontal 0..180). Used by tests and fuzz seeds.
#[must_use]
pub fn example_text() -> String {
    let mut v = Vec::new();
    for k in 0..=18 {
        v.push(k as f32 * 5.0);
    }
    let h = [0.0f32, 45.0, 90.0, 135.0, 180.0];
    let mut text = String::from(
        "IESNA:LM-63-2002\n[TEST] REGIA-EXAMPLE-1\n[MANUFAC] Regia Engine test data\n[LUMCAT] DL-60\n\
         [LUMINAIRE] Downlight, 60 degree cutoff\nTILT=NONE\n",
    );
    let _ = writeln!(text, "1 -1 1.0 {} {} 1 2 0.1 0.1 0.0", v.len(), h.len());
    text.push_str("1.0 1.0 12.0\n");
    let join = |xs: &[f32]| xs.iter().map(|x| format!("{x}")).collect::<Vec<_>>().join(" ");
    let _ = writeln!(text, "{}", join(&v));
    let _ = writeln!(text, "{}", join(&h));
    for &phi in &h {
        let side = 1.0 - 0.4 * (phi / 180.0);
        let row: Vec<f32> = v
            .iter()
            .map(|&t| {
                if t >= 60.0 {
                    0.0
                } else {
                    let c = (t.to_radians() * 1.5).cos();
                    (1000.0 * side * c * c).round()
                }
            })
            .collect();
        let _ = writeln!(text, "{}", join(&row));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_parses_and_round_trips_through_text() {
        let file = IesFile::parse(&example_text()).expect("parse");
        assert_eq!(file.standard, "IESNA:LM-63-2002");
        assert_eq!(file.vertical.len(), 19);
        assert_eq!(file.horizontal, vec![0.0, 45.0, 90.0, 135.0, 180.0]);
        assert_eq!(file.photometric_type, PhotometricType::C);
        assert_eq!(file.keywords[0], ("TEST".into(), "REGIA-EXAMPLE-1".into()));
        let again = IesFile::parse(&file.to_text()).expect("re-parse");
        assert_eq!(again, file);
    }

    #[test]
    fn values_split_across_lines_and_commas_parse() {
        let text =
            "IESNA91\n[TEST] x\nTILT=NONE\n1 1000 1\n3, 1 1 1\n0 0 0\n1 1 50\n0 45\n90\n0\n100, 50 0\n";
        let file = IesFile::parse(text).expect("parse");
        assert_eq!(file.vertical, vec![0.0, 45.0, 90.0]);
        assert_eq!(file.candela, vec![100.0, 50.0, 0.0]);
        assert!((file.candela_at_angles(22.5, 123.0) - 75.0).abs() < 1e-4);
    }

    /// The 2019 header (`IES:LM-63-2019`) and a 1986 file (no standard line, free-form labels) both read.
    #[test]
    fn the_2019_and_1986_headers_read() {
        let lm2019 = "IES:LM-63-2019\n[TEST] T1\n[TESTLAB] lab\n[ISSUEDATE] 2026-01-01\n[MANUFAC] m\nTILT=NONE\n\
                      1 -1 1 2 1 1 2 0.1 0.1 0\n1 1 10\n0 90\n0\n50 0\n";
        let f = IesFile::parse(lm2019).expect("2019");
        assert_eq!(f.standard, "IES:LM-63-2019");
        assert_eq!(f.keywords.len(), 4);
        assert_eq!(IesFile::parse(&f.to_text()).expect("round trip"), f);
        let lm1986 =
            "A 1986 luminaire\nNo keywords here\nTILT=NONE\n1 1000 1 2 1 1 1 1 1 0\n1 1 10\n0 90\n0\n50 0\n";
        let g = IesFile::parse(lm1986).expect("1986");
        assert!(g.standard.is_empty());
        assert_eq!(g.keywords[0], (String::new(), "A 1986 luminaire".to_string()));
        assert_eq!(g.units, 1);
        assert!((g.opening_m()[0] - 0.3048).abs() < 1e-6, "feet");
        assert_eq!(IesFile::parse(&g.to_text()).expect("round trip"), g);
    }

    #[test]
    fn tilt_include_round_trips() {
        let text = "IESNA:LM-63-1995\nTILT=INCLUDE\n1\n3\n0 45 90\n1 0.9 0.8\n1 -1 2 2 1 1 2 0 0 0\n1 1 10\n0 90\n0\n10 5\n";
        let file = IesFile::parse(text).expect("parse");
        let tilt = file.tilt.as_ref().expect("tilt");
        assert_eq!(tilt.factors, vec![1.0, 0.9, 0.8]);
        assert_eq!(file.value(0, 0), 20.0, "candela multiplier 2");
        assert_eq!(IesFile::parse(&file.to_text()).expect("re-parse"), file);
    }

    /// ADR-3247: the `TILT=INCLUDE` table scales the candela lookup. The fixture's factor at 0° is 0.95, so even the
    /// plain lookup (the luminaire hung as photometered) differs from the written values; a tilted installation
    /// takes the interpolated factor.
    #[test]
    fn tilt_include_scales_the_candela_lookup() {
        let file =
            IesFile::parse(include_str!("../tests/fixtures/ies-tilt-include.ies")).expect("fixture parses");
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4 * b.abs().max(1.0);
        assert!(close(file.tilt_factor(0.0), 0.95));
        assert!(close(file.tilt_factor(15.0), 0.975), "{}", file.tilt_factor(15.0));
        assert!(close(file.tilt_factor(45.0), 0.95));
        assert!(
            close(file.tilt_factor(-60.0), 0.9),
            "a 0..90 table is symmetric in the sign"
        );
        assert!(
            close(file.tilt_factor(120.0), 0.7),
            "the end factor past the table"
        );
        // The written values at the file's angles, times the factor at the tilt.
        for (theta, cd) in [(0.0f32, 100.0f32), (45.0, 80.0), (90.0, 0.0)] {
            assert!(close(file.candela_at_angles(theta, 33.0), cd * 0.95), "θ {theta}");
            assert!(
                close(file.candela_at_angles_tilted(theta, 33.0, 75.0), cd * 0.8),
                "θ {theta} tilted"
            );
            let (s, c) = theta.to_radians().sin_cos();
            assert!(close(file.candela([s, 0.0, -c]), cd * 0.95), "dir θ {theta}");
            assert!(
                close(file.candela_tilted([s, 0.0, -c], 30.0), cd),
                "dir θ {theta} at 30°"
            );
        }
        assert!(close(file.peak_candela(), 95.0));
        assert!(close(file.peak_candela_tilted(90.0), 70.0));
        // The cooked profile keeps the shape and carries the factor in its peak and flux.
        let flat = IesFile {
            tilt: None,
            ..file.clone()
        };
        let plain = CookedIesProfile::from_file(&flat).expect("cook");
        let cooked = CookedIesProfile::from_file(&file).expect("cook");
        let tilted = CookedIesProfile::from_file_tilted(&file, 90.0).expect("cook");
        // The same shape: a scalar factor cancels in cd / peak (to a rounding step).
        let same =
            |a: &[f32], b: &[f32]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        assert!(same(&cooked.values, &plain.values));
        assert!(same(&tilted.values, &plain.values));
        assert!(close(cooked.peak_candela, plain.peak_candela * 0.95));
        assert!(close(tilted.peak_candela, plain.peak_candela * 0.7));
        assert!(close(tilted.flux_lm, plain.flux_lm * 0.7));
        // A file without a table is unchanged at every tilt.
        assert_eq!(flat.tilt_factor(40.0), 1.0);
        assert_eq!(flat.candela_at_angles_tilted(45.0, 0.0, 40.0), 80.0);
        // The fixture still round-trips through text.
        assert_eq!(IesFile::parse(&file.to_text()).expect("re-parse"), file);
    }

    #[test]
    fn a_bad_tilt_table_is_an_error() {
        let bad = "IESNA:LM-63-1995\nTILT=INCLUDE\n1\n2\n0 90\n1 -0.5\n1 -1 1 2 1 1 2 0 0 0\n1 1 10\n0 90\n0\n10 5\n";
        assert!(matches!(IesFile::parse(bad), Err(IesError::Invalid(_))));
        let descending =
            "IESNA:LM-63-1995\nTILT=INCLUDE\n1\n2\n90 0\n1 1\n1 -1 1 2 1 1 2 0 0 0\n1 1 10\n0 90\n0\n10 5\n";
        assert!(matches!(IesFile::parse(descending), Err(IesError::Invalid(_))));
    }

    #[test]
    fn malformed_files_are_errors_not_panics() {
        assert_eq!(
            IesFile::parse("IESNA:LM-63-2002\n[TEST] x\n"),
            Err(IesError::NoTilt)
        );
        assert!(matches!(
            IesFile::parse("TILT=lamp.tlt\n"),
            Err(IesError::ExternalTilt(_))
        ));
        let truncated = "TILT=NONE\n1 -1 1 3 1 1 2 0 0 0\n1 1 1\n0 45 90\n0\n1 2\n";
        assert!(matches!(IesFile::parse(truncated), Err(IesError::Truncated(_))));
        let descending = "TILT=NONE\n1 -1 1 2 1 1 2 0 0 0\n1 1 1\n45 0\n0\n1 2\n";
        assert!(matches!(IesFile::parse(descending), Err(IesError::Invalid(_))));
        let huge = "TILT=NONE\n1 -1 1 99999 1 1 2 0 0 0\n1 1 1\n";
        assert!(IesFile::parse(huge).is_err());
        assert!(IesFile::parse("TILT=NONE\n1 -1 1 1 1 1 2 0 0 0\n1 1 1\n0\n0\n5 7\n").is_err());
        assert!(IesFile::parse("TILT=NONE\n1 -1 nan 1 1 1 2 0 0 0\n1 1 1\n0\n0\n5\n").is_err());
    }

    /// Every table value comes back from the lookup at its own angles, and the symmetry unfolding mirrors them.
    #[test]
    fn the_lookup_returns_the_file_values_at_the_file_angles() {
        let file = IesFile::parse(&example_text()).expect("parse");
        for (hi, &h) in file.horizontal.iter().enumerate() {
            for (vi, &v) in file.vertical.iter().enumerate() {
                let want = file.value(hi, vi);
                assert!((file.candela_at_angles(v, h) - want).abs() < 1e-3, "θ {v} φ {h}");
                // Bilateral symmetry about the 0-180 plane: φ and 360 − φ agree.
                assert!(
                    (file.candela_at_angles(v, 360.0 - h) - want).abs() < 1e-3,
                    "mirror θ {v} φ {h}"
                );
            }
        }
        // The nadir looks straight down -Z; +X is φ 0 and -X is φ 180.
        assert!((file.candela([0.0, 0.0, -1.0]) - 1000.0).abs() < 1e-3);
        let t = 30.0f32.to_radians();
        let toward_px = file.candela([t.sin(), 0.0, -t.cos()]);
        let toward_nx = file.candela([-t.sin(), 0.0, -t.cos()]);
        assert!((toward_px - file.candela_at_angles(30.0, 0.0)).abs() < 1e-2);
        assert!((toward_nx - file.candela_at_angles(30.0, 180.0)).abs() < 1e-2);
        assert!(toward_px > toward_nx);
        assert_eq!(file.candela([0.0, 0.0, 1.0]), 0.0, "nothing upward");
    }

    #[test]
    fn quadrant_and_90_270_symmetries_fold() {
        let quad = "TILT=NONE\n1 -1 1 1 2 1 2 0 0 0\n1 1 1\n0\n0 90\n10 20\n";
        let f = IesFile::parse(quad).expect("parse");
        assert!((f.candela_at_angles(0.0, 90.0) - 20.0).abs() < 1e-4);
        assert!((f.candela_at_angles(0.0, 270.0) - 20.0).abs() < 1e-4);
        assert!((f.candela_at_angles(0.0, 180.0) - 10.0).abs() < 1e-4);
        assert!((f.candela_at_angles(0.0, 135.0) - 15.0).abs() < 1e-4);
        let side = "TILT=NONE\n1 -1 1 1 2 1 2 0 0 0\n1 1 1\n0\n90 270\n10 20\n";
        let g = IesFile::parse(side).expect("parse");
        assert!(
            (g.candela_at_angles(0.0, 0.0) - 15.0).abs() < 1e-4,
            "0° mirrors 180°"
        );
        assert!((g.candela_at_angles(0.0, 45.0) - g.candela_at_angles(0.0, 135.0)).abs() < 1e-4);
    }

    #[test]
    fn the_cooked_profile_matches_the_file_and_round_trips() {
        let file = IesFile::parse(&example_text()).expect("parse");
        let cooked = CookedIesProfile::from_file(&file).expect("cook");
        assert_eq!(cooked.peak_candela, 1000.0);
        for &(theta, phi) in &[
            (0.0f32, 0.0f32),
            (20.0, 30.0),
            (35.0, 100.0),
            (50.0, 200.0),
            (59.0, 300.0),
        ] {
            let want = file.candela_at_angles(theta, phi) / file.peak_candela();
            let got = cooked.relative_at_angles(theta, phi);
            assert!((got - want).abs() < 0.02, "θ {theta} φ {phi}: {got} vs {want}");
        }
        let bytes = cooked.to_bytes();
        assert_eq!(CookedIesProfile::from_bytes(&bytes).expect("read"), cooked);
        // The flux of an isotropic 1 cd source is 4π lm.
        let iso = IesFile::parse("TILT=NONE\n1 -1 1 2 1 1 2 0 0 0\n1 1 1\n0 180\n0\n1 1\n").expect("parse");
        let iso = CookedIesProfile::from_file(&iso).expect("cook");
        assert!(
            (iso.flux_lm - 4.0 * std::f32::consts::PI).abs() < 1e-3,
            "{}",
            iso.flux_lm
        );
    }

    #[test]
    fn corrupt_cooked_payloads_are_rejected() {
        let file = IesFile::parse(&example_text()).expect("parse");
        let bytes = CookedIesProfile::from_file(&file).expect("cook").to_bytes();
        assert!(CookedIesProfile::from_bytes(&bytes[..bytes.len() - 1]).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(CookedIesProfile::from_bytes(&extra).is_err());
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert!(CookedIesProfile::from_bytes(&magic).is_err());
        let mut version = bytes.clone();
        version[4] = 2;
        assert!(CookedIesProfile::from_bytes(&version).is_err());
        let mut value = bytes.clone();
        let at = COOKED_HEADER_BYTES;
        value[at..at + 4].copy_from_slice(&2.0f32.to_le_bytes());
        assert!(CookedIesProfile::from_bytes(&value).is_err());
        let mut nan = bytes;
        nan[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(CookedIesProfile::from_bytes(&nan).is_err());
    }

    #[test]
    fn a_dark_profile_does_not_cook() {
        let dark = IesFile::parse("TILT=NONE\n1 -1 1 1 1 1 2 0 0 0\n1 1 1\n0\n0\n0\n").expect("parse");
        assert!(CookedIesProfile::from_file(&dark).is_err());
    }

    #[test]
    fn type_b_files_cook_through_the_direction_lookup() {
        let text = "TILT=NONE\n1 -1 1 3 3 2 2 0 0 0\n1 1 1\n-90 0 90\n-90 0 90\n0 0 0\n0 100 0\n0 0 0\n";
        let f = IesFile::parse(text).expect("parse");
        assert!(
            (f.candela([0.0, 0.0, -1.0]) - 100.0).abs() < 1e-3,
            "aimed along the nadir"
        );
        let cooked = CookedIesProfile::from_file(&f).expect("cook");
        assert!((cooked.relative([0.0, 0.0, -1.0]) - 1.0).abs() < 1e-3);
    }
}
