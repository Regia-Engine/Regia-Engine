// SPDX-License-Identifier: MPL-2.0
//! The persisted bake of a level's local reflection captures (`.fcap`, ADR-2860,
//! `docs/formats/fcap.md`).
//!
//! One file holds every capture a level bakes: its placement (id, box, weight, fade), its share
//! of directions that saw a surface, and its GGX-filtered equirect chain as the renderer uploads
//! it (`Rgba16Float` texels: premultiplied radiance in rgb, surface coverage in alpha, mip 0
//! first). A level opens with these maps installed and no capture face rendered; a damaged file
//! is refused by name and the level keeps rendering without it.
//!
//! Layout (little endian): the magic `RFCP`, `format_version: u32`, `payload_len: u64`,
//! `crc32: u32` (IEEE, over the payload), then the payload: `width: u32`, `mips: u32`,
//! `count: u32` and `count` records of `id: u64`, `center: [f32; 3]`, `half_extent: [f32; 3]`,
//! `weight: f32`, `fade: f32`, `coverage: f32` and the `mips` mip images, each a `u32` byte
//! length and the bytes.

use std::path::Path;

/// The four bytes every `.fcap` starts with.
pub const FCAP_MAGIC: [u8; 4] = *b"RFCP";
/// The version this build writes and reads.
pub const FCAP_FORMAT_VERSION: u32 = 1;
/// Most captures one file holds.
pub const FCAP_MAX_CAPTURES: usize = 64;
/// Widest map a file may hold, texels (the height is half of it).
pub const FCAP_MAX_WIDTH: u32 = 4096;
/// The header before the payload: magic, version, payload length, CRC.
const HEADER_LEN: usize = 4 + 4 + 8 + 4;

/// Why a `.fcap` could not be read or written. [`Self::code`] is the stable code the agent
/// protocol and the CLI report.
#[derive(Debug, thiserror::Error)]
pub enum FcapError {
    /// File IO failed.
    #[error("fcap_io: {0}")]
    Io(#[from] std::io::Error),
    /// The bytes do not start with `RFCP`.
    #[error("fcap_bad_magic: not a local capture bake (no RFCP magic)")]
    BadMagic,
    /// A version this build does not read.
    #[error("fcap_version: format_version {found}, this build reads {FCAP_FORMAT_VERSION}")]
    Version {
        /// The file's version.
        found: u32,
    },
    /// The file ends before the header or the payload it declares.
    #[error("fcap_truncated: {0}")]
    Truncated(String),
    /// The payload's checksum does not match: the file is damaged.
    #[error("fcap_crc: payload checksum {found:08x}, header says {expected:08x}")]
    Crc {
        /// The header's checksum.
        expected: u32,
        /// The payload's checksum.
        found: u32,
    },
    /// The payload's sizes do not fit together.
    #[error("fcap_layout: {0}")]
    Layout(String),
    /// A value is out of range.
    #[error("fcap_value: {0}")]
    Value(String),
}

impl FcapError {
    /// The stable code: `fcap_io`, `fcap_bad_magic`, `fcap_version`, `fcap_truncated`,
    /// `fcap_crc`, `fcap_layout` or `fcap_value`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "fcap_io",
            Self::BadMagic => "fcap_bad_magic",
            Self::Version { .. } => "fcap_version",
            Self::Truncated(_) => "fcap_truncated",
            Self::Crc { .. } => "fcap_crc",
            Self::Layout(_) => "fcap_layout",
            Self::Value(_) => "fcap_value",
        }
    }
}

/// One baked capture.
#[derive(Clone, Debug, PartialEq)]
pub struct FcapCapture {
    /// The capture's stable id (the `LocalCapture` component's).
    pub id: u64,
    /// World centre of the box, which is also the capture point.
    pub center: [f32; 3],
    /// Half extents of the parallax box, meters.
    pub half_extent: [f32; 3],
    /// Blend weight against the environment, 0..1.
    pub weight: f32,
    /// How far outside the box the capture fades out, meters.
    pub fade: f32,
    /// Share of directions that saw a surface, 0..1.
    pub coverage: f32,
    /// The filtered equirect chain, mip 0 first: `Rgba16Float` texels, 8 bytes each.
    pub mips: Vec<Vec<u8>>,
}

/// A level's capture bake.
#[derive(Clone, Debug, PartialEq)]
pub struct FcapFile {
    /// Width of mip 0, texels; the height is half of it.
    pub width: u32,
    /// The captures, in the order they were baked.
    pub captures: Vec<FcapCapture>,
}

/// Bytes of mip `mip` of a `width`-wide chain.
#[must_use]
pub fn fcap_mip_bytes(width: u32, mip: u32) -> usize {
    let w = (width >> mip).max(1) as usize;
    let h = ((width / 2) >> mip).max(1) as usize;
    w * h * 8
}

/// Mips of a full chain `width` wide (down to 1 × 1).
#[must_use]
pub fn fcap_chain_len(width: u32) -> u32 {
    32 - width.max(1).leading_zeros()
}

/// CRC-32 (IEEE 802.3, reflected, as zip and PNG use it).
#[must_use]
pub fn fcap_crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn finite(name: &str, values: &[f32]) -> Result<(), FcapError> {
    if values.iter().all(|v| v.is_finite()) {
        Ok(())
    } else {
        Err(FcapError::Value(format!("{name} must be finite")))
    }
}

impl FcapCapture {
    fn validate(&self, width: u32, mips: u32) -> Result<(), FcapError> {
        finite("center", &self.center)?;
        finite("half_extent", &self.half_extent)?;
        if !self.half_extent.iter().all(|h| *h > 0.0) {
            return Err(FcapError::Value(format!(
                "capture {}: half_extent must be > 0",
                self.id
            )));
        }
        if !(self.weight.is_finite() && (0.0..=1.0).contains(&self.weight)) {
            return Err(FcapError::Value(format!(
                "capture {}: weight must be within 0 to 1",
                self.id
            )));
        }
        if !(self.fade.is_finite() && self.fade >= 0.0) {
            return Err(FcapError::Value(format!(
                "capture {}: fade must be >= 0",
                self.id
            )));
        }
        if !(self.coverage.is_finite() && (0.0..=1.0).contains(&self.coverage)) {
            return Err(FcapError::Value(format!(
                "capture {}: coverage must be within 0 to 1",
                self.id
            )));
        }
        if self.mips.len() != mips as usize {
            return Err(FcapError::Layout(format!(
                "capture {}: {} mips, the file says {mips}",
                self.id,
                self.mips.len()
            )));
        }
        for (mip, bytes) in self.mips.iter().enumerate() {
            let want = fcap_mip_bytes(width, mip as u32);
            if bytes.len() != want {
                return Err(FcapError::Layout(format!(
                    "capture {} mip {mip}: {} bytes, a {width}-wide chain needs {want}",
                    self.id,
                    bytes.len()
                )));
            }
        }
        Ok(())
    }
}

impl FcapFile {
    /// Checks sizes and values: what [`Self::from_bytes`] refuses, [`Self::to_bytes`] refuses.
    ///
    /// # Errors
    /// [`FcapError::Layout`] or [`FcapError::Value`] naming the first bad field.
    pub fn validate(&self) -> Result<(), FcapError> {
        if !(2..=FCAP_MAX_WIDTH).contains(&self.width) || !self.width.is_power_of_two() {
            return Err(FcapError::Layout(format!(
                "width {} must be a power of two from 2 to {FCAP_MAX_WIDTH}",
                self.width
            )));
        }
        if self.captures.len() > FCAP_MAX_CAPTURES {
            return Err(FcapError::Layout(format!(
                "{} captures, at most {FCAP_MAX_CAPTURES}",
                self.captures.len()
            )));
        }
        let mips = self
            .captures
            .first()
            .map_or(fcap_chain_len(self.width), |c| c.mips.len() as u32);
        if mips == 0 || mips > fcap_chain_len(self.width) {
            return Err(FcapError::Layout(format!(
                "{mips} mips for a {}-wide chain",
                self.width
            )));
        }
        for (i, capture) in self.captures.iter().enumerate() {
            capture.validate(self.width, mips)?;
            if self.captures[..i].iter().any(|c| c.id == capture.id) {
                return Err(FcapError::Value(format!(
                    "capture id {} appears twice",
                    capture.id
                )));
            }
        }
        Ok(())
    }

    /// The file's bytes.
    ///
    /// # Errors
    /// What [`Self::validate`] refuses.
    pub fn to_bytes(&self) -> Result<Vec<u8>, FcapError> {
        self.validate()?;
        // An empty bake records the full chain length (the reader refuses 0 mips).
        let mips = self
            .captures
            .first()
            .map_or(fcap_chain_len(self.width), |c| c.mips.len() as u32);
        let mut payload = Vec::new();
        payload.extend_from_slice(&self.width.to_le_bytes());
        payload.extend_from_slice(&mips.to_le_bytes());
        payload.extend_from_slice(&(self.captures.len() as u32).to_le_bytes());
        for c in &self.captures {
            payload.extend_from_slice(&c.id.to_le_bytes());
            for v in c
                .center
                .iter()
                .chain(&c.half_extent)
                .chain(&[c.weight, c.fade, c.coverage])
            {
                payload.extend_from_slice(&v.to_le_bytes());
            }
            for mip in &c.mips {
                payload.extend_from_slice(&(mip.len() as u32).to_le_bytes());
                payload.extend_from_slice(mip);
            }
        }
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&FCAP_MAGIC);
        out.extend_from_slice(&FCAP_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&fcap_crc32(&payload).to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Reads a file's bytes: the magic, the version, the payload length and its CRC first, then
    /// every size and value.
    ///
    /// # Errors
    /// [`FcapError`] naming what is wrong ([`FcapError::code`]).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FcapError> {
        let version = fcap_version(bytes)?;
        if version != FCAP_FORMAT_VERSION {
            return Err(FcapError::Version { found: version });
        }
        if bytes.len() < HEADER_LEN {
            return Err(FcapError::Truncated(format!(
                "{} bytes, the header needs {HEADER_LEN}",
                bytes.len()
            )));
        }
        let mut r = Reader { bytes, at: 8 };
        let payload_len = r.u64()?;
        let expected = r.u32()?;
        let rest = bytes.len() - HEADER_LEN;
        if payload_len != rest as u64 {
            return Err(FcapError::Truncated(format!(
                "the header declares a {payload_len}-byte payload, the file holds {rest}"
            )));
        }
        let payload = &bytes[HEADER_LEN..];
        let found = fcap_crc32(payload);
        if found != expected {
            return Err(FcapError::Crc { expected, found });
        }
        let mut r = Reader {
            bytes: payload,
            at: 0,
        };
        let width = r.u32()?;
        let mips = r.u32()?;
        let count = r.u32()? as usize;
        if count > FCAP_MAX_CAPTURES {
            return Err(FcapError::Layout(format!(
                "{count} captures, at most {FCAP_MAX_CAPTURES}"
            )));
        }
        if !(2..=FCAP_MAX_WIDTH).contains(&width) || !width.is_power_of_two() {
            return Err(FcapError::Layout(format!(
                "width {width} must be a power of two from 2 to {FCAP_MAX_WIDTH}"
            )));
        }
        if mips == 0 || mips > fcap_chain_len(width) {
            return Err(FcapError::Layout(format!("{mips} mips for a {width}-wide chain")));
        }
        let mut captures = Vec::with_capacity(count);
        for _ in 0..count {
            let id = r.u64()?;
            let mut f = [0.0f32; 9];
            for v in &mut f {
                *v = r.f32()?;
            }
            let mut chain = Vec::with_capacity(mips as usize);
            for mip in 0..mips {
                let len = r.u32()? as usize;
                let want = fcap_mip_bytes(width, mip);
                if len != want {
                    return Err(FcapError::Layout(format!(
                        "capture {id} mip {mip}: {len} bytes, a {width}-wide chain needs {want}"
                    )));
                }
                chain.push(r.take(len)?.to_vec());
            }
            captures.push(FcapCapture {
                id,
                center: [f[0], f[1], f[2]],
                half_extent: [f[3], f[4], f[5]],
                weight: f[6],
                fade: f[7],
                coverage: f[8],
                mips: chain,
            });
        }
        if r.at != payload.len() {
            return Err(FcapError::Layout(format!(
                "{} bytes after the last capture",
                payload.len() - r.at
            )));
        }
        let file = Self { width, captures };
        file.validate()?;
        Ok(file)
    }

    /// Reads `path`.
    ///
    /// # Errors
    /// File IO failed ([`FcapError::Io`]) or see [`Self::from_bytes`].
    pub fn read(path: &Path) -> Result<Self, FcapError> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    /// Writes `path` atomically (a temporary file beside it, then a rename).
    ///
    /// # Errors
    /// What [`Self::to_bytes`] refuses, or file IO failed.
    pub fn write(&self, path: &Path) -> Result<(), FcapError> {
        let bytes = self.to_bytes()?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp{}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, &bytes)?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// The capture with this id.
    #[must_use]
    pub fn capture(&self, id: u64) -> Option<&FcapCapture> {
        self.captures.iter().find(|c| c.id == id)
    }
}

/// The `.fcap` side file of a level: `<dir>/<name>.level.ron` → `<dir>/<name>.fcap` (any other
/// file name → the same name with the `.fcap` extension).
#[must_use]
pub fn fcap_path_for_level(level: &Path) -> std::path::PathBuf {
    let name = level.file_name().and_then(|n| n.to_str()).unwrap_or("level");
    let stem = name
        .strip_suffix(".level.ron")
        .or_else(|| name.strip_suffix(".ron"))
        .unwrap_or(name);
    level.with_file_name(format!("{stem}.fcap"))
}

/// The layout fingerprint `cargo xtask check-formats` hashes: the header and record fields in
/// file order. A change here is a format change (bump [`FCAP_FORMAT_VERSION`], add a migrator).
#[must_use]
pub fn fcap_schema_fingerprint() -> String {
    [
        "header:magic[4],format_version:u32,payload_len:u64,crc32:u32",
        "payload:width:u32,mips:u32,count:u32",
        "capture:id:u64,center:[f32;3],half_extent:[f32;3],weight:f32,fade:f32,coverage:f32,mips:[len:u32,bytes]",
        "texel:rgba16float",
    ]
    .join(";")
}

/// The versions a built-in migration step leaves: none, v1 is the first.
#[must_use]
pub fn fcap_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The format version in a `.fcap` header (no other validation).
///
/// # Errors
/// [`FcapError::BadMagic`] when the bytes are not a capture bake.
pub fn fcap_version(bytes: &[u8]) -> Result<u32, FcapError> {
    if bytes.len() < 8 || bytes[0..4] != FCAP_MAGIC {
        return Err(FcapError::BadMagic);
    }
    Ok(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FcapError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| FcapError::Truncated(format!("{n} bytes wanted at offset {}", self.at)))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, FcapError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, FcapError> {
        let lo = u64::from(self.u32()?);
        let hi = u64::from(self.u32()?);
        Ok(lo | (hi << 32))
    }

    fn f32(&mut self) -> Result<f32, FcapError> {
        Ok(f32::from_bits(self.u32()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(width: u32, ids: &[u64]) -> FcapFile {
        let mips = fcap_chain_len(width);
        FcapFile {
            width,
            captures: ids
                .iter()
                .map(|&id| FcapCapture {
                    id,
                    center: [1.0, 2.0, id as f32],
                    half_extent: [2.0, 1.5, 3.0],
                    weight: 1.0,
                    fade: 0.75,
                    coverage: 0.5,
                    mips: (0..mips)
                        .map(|m| {
                            (0..fcap_mip_bytes(width, m))
                                .map(|i| (i as u64 * 31 + id + u64::from(m)) as u8)
                                .collect()
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_bake_round_trips_byte_for_byte() {
        let file = sample(16, &[7, 3]);
        let bytes = file.to_bytes().expect("writes");
        assert_eq!(&bytes[0..4], b"RFCP");
        assert_eq!(fcap_version(&bytes).expect("version"), FCAP_FORMAT_VERSION);
        let back = FcapFile::from_bytes(&bytes).expect("reads");
        assert_eq!(back, file);
        assert_eq!(back.to_bytes().expect("writes again"), bytes);
        assert_eq!(back.capture(3).map(|c| c.center), Some([1.0, 2.0, 3.0]));
    }

    #[test]
    fn the_crc_matches_the_ieee_check_value() {
        assert_eq!(fcap_crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_damaged_file_is_refused_by_name() {
        let bytes = sample(8, &[1]).to_bytes().expect("writes");
        // One flipped bit in a texel.
        let mut flipped = bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x10;
        assert_eq!(
            FcapFile::from_bytes(&flipped).expect_err("crc").code(),
            "fcap_crc"
        );
        // Cut short.
        let cut = &bytes[..bytes.len() - 3];
        assert_eq!(
            FcapFile::from_bytes(cut).expect_err("cut").code(),
            "fcap_truncated"
        );
        assert_eq!(
            FcapFile::from_bytes(&bytes[..10]).expect_err("header").code(),
            "fcap_truncated"
        );
        // Not a bake.
        assert_eq!(
            FcapFile::from_bytes(b"PNG\0abcdefgh").expect_err("magic").code(),
            "fcap_bad_magic"
        );
        // A newer version.
        let mut newer = bytes.clone();
        newer[4] = 9;
        let err = FcapFile::from_bytes(&newer).expect_err("version");
        assert_eq!(err.code(), "fcap_version");
        assert!(err.to_string().starts_with("fcap_version"), "{err}");
    }

    #[test]
    fn bad_values_and_sizes_are_refused_on_write_and_read() {
        let mut file = sample(8, &[1]);
        file.captures[0].weight = 2.0;
        assert_eq!(file.to_bytes().expect_err("weight").code(), "fcap_value");
        let mut file = sample(8, &[1, 1]);
        assert_eq!(file.to_bytes().expect_err("dup").code(), "fcap_value");
        file.captures.pop();
        file.captures[0].mips[1].pop();
        assert_eq!(file.to_bytes().expect_err("mip size").code(), "fcap_layout");
        let file = FcapFile {
            width: 12,
            captures: vec![],
        };
        assert_eq!(file.to_bytes().expect_err("width").code(), "fcap_layout");
        // A NaN smuggled past the writer with a valid CRC is refused on read.
        let mut bytes = sample(8, &[1]).to_bytes().expect("writes");
        let at = HEADER_LEN + 12 + 8; // the first centre component
        bytes[at..at + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        let crc = fcap_crc32(&bytes[HEADER_LEN..]);
        bytes[16..20].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            FcapFile::from_bytes(&bytes).expect_err("nan").code(),
            "fcap_value"
        );
    }

    #[test]
    fn an_empty_bake_is_valid() {
        let file = FcapFile {
            width: 512,
            captures: vec![],
        };
        let bytes = file.to_bytes().expect("writes");
        assert_eq!(FcapFile::from_bytes(&bytes).expect("reads"), file);
    }

    #[test]
    fn a_level_names_its_bake_beside_it() {
        assert_eq!(
            fcap_path_for_level(Path::new("assets/levels/hall.level.ron")),
            Path::new("assets/levels/hall.fcap")
        );
        assert_eq!(fcap_path_for_level(Path::new("a/b.ron")), Path::new("a/b.fcap"));
    }

    #[test]
    fn files_write_atomically_and_read_back() {
        let dir = regia_test_utils::unique_temp_dir("fcap");
        let path = dir.join("room.fcap");
        let file = sample(4, &[5]);
        file.write(&path).expect("writes");
        assert_eq!(FcapFile::read(&path).expect("reads"), file);
        let missing = FcapFile::read(&dir.join("none.fcap")).expect_err("missing");
        assert_eq!(missing.code(), "fcap_io");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
