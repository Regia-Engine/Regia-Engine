// SPDX-License-Identifier: MPL-2.0
//! A baked studio environment: prefiltered specular cube mips and diffuse spherical harmonics.
//!
//! The file form (`.renv`, `docs/formats/viewer-env.md`) is small, HDR and needs no float render target:
//! texels are RGB9E5, which WebGL2 can filter. Baking happens offline (`crate::bake`,
//! `regia_viewer_bake_env`); the player only parses and uploads.

/// The current `.renv` format version.
pub const ENV_FORMAT_VERSION: u32 = 1;
const MAGIC: &[u8; 4] = b"RENV";
const HEADER_BYTES: usize = 4 + 4 + 4 + 4 + 27 * 4;
const MIN_FACE: u32 = 4;
const MAX_FACE: u32 = 2048;

/// The environment shipped inside the crate: the procedural default studio
/// ([`StudioLights`](crate::bake::StudioLights)), 64 texels per face.
const DEFAULT_STUDIO: &[u8] = include_bytes!("../assets/studio_default.renv");

/// Why an environment file was refused. The reason strings are part of the agent protocol.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvError {
    /// The file does not start with the `.renv` magic.
    #[error("not a viewer environment file")]
    BadMagic,
    /// The file is newer than this build.
    #[error(
        "environment format version {found} is not supported (this build reads up to {ENV_FORMAT_VERSION})"
    )]
    UnsupportedVersion {
        /// The version in the file.
        found: u32,
    },
    /// The header holds an impossible size.
    #[error("bad environment size: {0}")]
    BadSize(String),
    /// The file is shorter or longer than its header says.
    #[error("environment file has {found} bytes, its header says {expected}")]
    BadLength {
        /// Bytes the header implies.
        expected: usize,
        /// Bytes present.
        found: usize,
    },
    /// A diffuse coefficient is not finite.
    #[error("environment holds a diffuse coefficient that is not finite")]
    BadCoefficient,
}

/// A baked environment, ready to upload.
#[derive(Debug, Clone, PartialEq)]
pub struct BakedEnv {
    pub(crate) face_size: u32,
    pub(crate) mip_count: u32,
    /// Diffuse: second-order spherical harmonics, already cosine convolved and divided by pi.
    pub(crate) sh: [[f32; 3]; 9],
    /// RGB9E5 texels: mip 0 faces +X, -X, +Y, -Y, +Z, -Z, then mip 1, and so on.
    pub(crate) texels: Vec<u32>,
}

/// Packs a linear color into RGB9E5, the layout of `EXT_texture_shared_exponent`.
#[must_use]
pub fn encode_rgb9e5(rgb: [f32; 3]) -> u32 {
    const N: i32 = 9;
    const B: i32 = 15;
    const E_MAX: i32 = 31;
    let max_value = ((1 << N) - 1) as f32 / (1 << N) as f32 * 2f32.powi(E_MAX - B);
    let clamp = |v: f32| {
        if v.is_finite() {
            v.clamp(0.0, max_value)
        } else {
            0.0
        }
    };
    let (r, g, b) = (clamp(rgb[0]), clamp(rgb[1]), clamp(rgb[2]));
    let largest = r.max(g).max(b);
    if largest <= 0.0 {
        return 0;
    }
    let mut exponent = (largest.log2().floor() as i32).max(-B - 1) + 1 + B;
    let mut scale = 2f32.powi(exponent - B - N);
    let top = (largest / scale + 0.5).floor() as i32;
    if top == 1 << N {
        exponent += 1;
        scale *= 2.0;
    }
    let pack = |v: f32| ((v / scale + 0.5).floor() as u32).min((1 << N) - 1);
    pack(r) | (pack(g) << 9) | (pack(b) << 18) | ((exponent as u32) << 27)
}

/// Unpacks an RGB9E5 texel.
#[must_use]
pub fn decode_rgb9e5(texel: u32) -> [f32; 3] {
    let exponent = (texel >> 27) as i32 - 15 - 9;
    let scale = 2f32.powi(exponent);
    [
        (texel & 0x1ff) as f32 * scale,
        ((texel >> 9) & 0x1ff) as f32 * scale,
        ((texel >> 18) & 0x1ff) as f32 * scale,
    ]
}

/// How many texels `mip_count` mips of six `face_size` faces hold.
fn texel_count(face_size: u32, mip_count: u32) -> usize {
    (0..mip_count)
        .map(|mip| {
            let s = (face_size >> mip).max(1) as usize;
            6 * s * s
        })
        .sum()
}

impl BakedEnv {
    /// Builds an environment from parts.
    ///
    /// # Errors
    /// [`EnvError::BadSize`] for an impossible size, [`EnvError::BadLength`] when `texels` does not hold
    /// every mip, [`EnvError::BadCoefficient`] for a coefficient that is not finite.
    pub fn from_parts(
        face_size: u32,
        mip_count: u32,
        sh: [[f32; 3]; 9],
        texels: Vec<u32>,
    ) -> Result<Self, EnvError> {
        check_size(face_size, mip_count)?;
        let expected = texel_count(face_size, mip_count);
        if texels.len() != expected {
            return Err(EnvError::BadLength {
                expected: expected * 4,
                found: texels.len() * 4,
            });
        }
        if sh.iter().flatten().any(|c| !c.is_finite()) {
            return Err(EnvError::BadCoefficient);
        }
        Ok(Self {
            face_size,
            mip_count,
            sh,
            texels,
        })
    }

    /// The environment shipped inside the crate.
    ///
    /// # Panics
    /// Never in a released build: a test parses the shipped bytes.
    #[must_use]
    pub fn studio_default() -> Self {
        match Self::parse(DEFAULT_STUDIO) {
            Ok(env) => env,
            // The asset is compiled in, and `the_shipped_studio_parses` keeps it valid.
            Err(e) => unreachable!("the shipped studio is valid: {e}"),
        }
    }

    /// A uniform environment of one radiance: every direction, every roughness.
    #[must_use]
    pub fn uniform(radiance: [f32; 3]) -> Self {
        // A non-finite channel would skip `from_parts` and store a NaN harmonic. Texels already clamp.
        let radiance = radiance.map(|c| if c.is_finite() { c } else { 0.0 });
        let mut sh = [[0.0; 3]; 9];
        // The DC coefficient of a constant L, convolved and divided by pi, stays L * sqrt(4 pi).
        let dc = (4.0 * std::f32::consts::PI).sqrt();
        sh[0] = [radiance[0] * dc, radiance[1] * dc, radiance[2] * dc];
        let texel = encode_rgb9e5(radiance);
        let count = texel_count(MIN_FACE, 1);
        Self {
            face_size: MIN_FACE,
            mip_count: 1,
            sh,
            texels: vec![texel; count],
        }
    }

    /// Texels per face at mip 0.
    #[must_use]
    pub fn face_size(&self) -> u32 {
        self.face_size
    }

    /// Mip levels per face.
    #[must_use]
    pub fn mip_count(&self) -> u32 {
        self.mip_count
    }

    /// The diffuse harmonics, red, green, blue per coefficient.
    #[must_use]
    pub fn sh(&self) -> &[[f32; 3]; 9] {
        &self.sh
    }

    /// One mip's texels: faces +X, -X, +Y, -Y, +Z, -Z.
    #[must_use]
    pub fn mip_texels(&self, mip: u32) -> &[u32] {
        let start = texel_count(self.face_size, mip.min(self.mip_count));
        let end = texel_count(self.face_size, mip.saturating_add(1).min(self.mip_count));
        &self.texels[start..end]
    }

    /// Parses a `.renv` file.
    ///
    /// # Errors
    /// [`EnvError`] with the reason. Nothing past a refusal is read, and nothing can panic on bad data.
    pub fn parse(bytes: &[u8]) -> Result<Self, EnvError> {
        if bytes.len() < 8 || &bytes[..4] != MAGIC {
            return Err(EnvError::BadMagic);
        }
        let word = |at: usize| {
            bytes
                .get(at..at + 4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map(u32::from_le_bytes)
                .ok_or(EnvError::BadLength {
                    expected: HEADER_BYTES,
                    found: bytes.len(),
                })
        };
        let version = word(4)?;
        if version != ENV_FORMAT_VERSION {
            return Err(EnvError::UnsupportedVersion { found: version });
        }
        let face_size = word(8)?;
        let mip_count = word(12)?;
        check_size(face_size, mip_count)?;
        let mut sh = [[0.0; 3]; 9];
        let mut at = 16;
        for coefficient in &mut sh {
            for channel in coefficient.iter_mut() {
                *channel = f32::from_bits(word(at)?);
                at += 4;
            }
        }
        let count = texel_count(face_size, mip_count);
        let expected = HEADER_BYTES + count * 4;
        if bytes.len() != expected {
            return Err(EnvError::BadLength {
                expected,
                found: bytes.len(),
            });
        }
        let (rows, _) = bytes[HEADER_BYTES..].as_chunks::<4>();
        let texels = rows.iter().map(|b| u32::from_le_bytes(*b)).collect();
        Self::from_parts(face_size, mip_count, sh, texels)
    }

    /// The `.renv` file form.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_BYTES + self.texels.len() * 4);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&ENV_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.face_size.to_le_bytes());
        out.extend_from_slice(&self.mip_count.to_le_bytes());
        for channel in self.sh.iter().flatten() {
            out.extend_from_slice(&channel.to_bits().to_le_bytes());
        }
        for texel in &self.texels {
            out.extend_from_slice(&texel.to_le_bytes());
        }
        out
    }
}

pub(crate) fn check_size(face_size: u32, mip_count: u32) -> Result<(), EnvError> {
    if !(MIN_FACE..=MAX_FACE).contains(&face_size) || !face_size.is_power_of_two() {
        return Err(EnvError::BadSize(format!(
            "{face_size} texels per face (a power of two from {MIN_FACE} to {MAX_FACE})"
        )));
    }
    let max_mips = face_size.trailing_zeros() + 1;
    if mip_count == 0 || mip_count > max_mips {
        return Err(EnvError::BadSize(format!(
            "{mip_count} mips of a {face_size} face"
        )));
    }
    Ok(())
}

/// The fingerprint of the `.renv` layout for `cargo xtask check-formats`: a layout change without a
/// version bump fails that check.
#[must_use]
pub fn env_schema_fingerprint() -> String {
    "renv: magic[4] version:u32 face_size:u32 mip_count:u32 sh:f32[27] texels:rgb9e5_u32[mips,faces(+x,-x,+y,-y,+z,-z)]"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mip_past_the_last_is_empty_even_at_the_largest_number() {
        let env = BakedEnv::uniform([1.0; 3]);
        assert!(!env.mip_texels(0).is_empty());
        assert!(env.mip_texels(env.mip_count()).is_empty());
        assert!(env.mip_texels(u32::MAX).is_empty());
    }

    #[test]
    fn the_shipped_studio_parses() {
        let env = BakedEnv::parse(DEFAULT_STUDIO).expect("shipped studio");
        assert_eq!(env.face_size(), 64);
        assert_eq!(env.mip_count(), 5);
        assert_eq!(env.to_bytes(), DEFAULT_STUDIO);
    }

    #[test]
    fn rgb9e5_round_trips_within_its_precision() {
        for rgb in [
            [1.0, 1.0, 1.0],
            [0.25, 0.5, 0.125],
            [20.0, 3.0, 0.5],
            [0.001, 0.002, 0.003],
            [1000.0, 0.0, 0.0],
        ] {
            let back = decode_rgb9e5(encode_rgb9e5(rgb));
            let largest = rgb[0].max(rgb[1]).max(rgb[2]);
            for i in 0..3 {
                assert!((back[i] - rgb[i]).abs() <= largest / 200.0, "{rgb:?} -> {back:?}");
            }
        }
        assert_eq!(encode_rgb9e5([0.0; 3]), 0);
        assert_eq!(encode_rgb9e5([f32::NAN, -1.0, f32::NEG_INFINITY]), 0);
        // The largest value keeps its exponent in range instead of overflowing it.
        let top = decode_rgb9e5(encode_rgb9e5([1e30, 1e30, 1e30]));
        assert!(top[0] > 60000.0 && top[0].is_finite());
    }

    #[test]
    fn a_non_finite_uniform_environment_stays_finite() {
        let env = BakedEnv::uniform([f32::NAN, f32::INFINITY, 1.0]);
        assert!(env.sh().iter().flatten().all(|c| c.is_finite()), "{:?}", env.sh());
        assert_eq!(env.sh()[0][0], 0.0);
        assert_eq!(env.sh()[0][1], 0.0);
        let dc = (4.0 * std::f32::consts::PI).sqrt();
        assert!((env.sh()[0][2] - dc).abs() < 1e-5, "{:?}", env.sh()[0]);
        let back = BakedEnv::parse(&env.to_bytes()).expect("finite uniform round-trips");
        assert_eq!(back.sh()[0][0], 0.0);
    }

    #[test]
    fn a_file_round_trips() {
        let env = BakedEnv::uniform([0.5, 0.25, 1.0]);
        assert_eq!(BakedEnv::parse(&env.to_bytes()), Ok(env));
    }

    #[test]
    fn bad_files_are_refused_with_the_reason() {
        let good = BakedEnv::uniform([1.0; 3]).to_bytes();
        assert_eq!(BakedEnv::parse(b"nope"), Err(EnvError::BadMagic));
        assert_eq!(BakedEnv::parse(b"RENV"), Err(EnvError::BadMagic));

        let mut newer = good.clone();
        newer[4] = 2;
        assert_eq!(
            BakedEnv::parse(&newer),
            Err(EnvError::UnsupportedVersion { found: 2 })
        );

        let mut big = good.clone();
        big[8..12].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(BakedEnv::parse(&big), Err(EnvError::BadSize(_))));

        let mut mips = good.clone();
        mips[12..16].copy_from_slice(&100u32.to_le_bytes());
        assert!(matches!(BakedEnv::parse(&mips), Err(EnvError::BadSize(_))));

        let short = &good[..good.len() - 1];
        assert!(matches!(BakedEnv::parse(short), Err(EnvError::BadLength { .. })));
        assert!(matches!(
            BakedEnv::parse(&good[..20]),
            Err(EnvError::BadLength { .. })
        ));

        let mut nan = good.clone();
        nan[16..20].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
        assert_eq!(BakedEnv::parse(&nan), Err(EnvError::BadCoefficient));
    }

    #[test]
    fn mip_texels_address_each_level() {
        let env = BakedEnv::studio_default();
        assert_eq!(env.mip_texels(0).len(), 6 * 64 * 64);
        assert_eq!(env.mip_texels(1).len(), 6 * 32 * 32);
        assert_eq!(env.mip_texels(4).len(), 6 * 4 * 4);
        assert!(env.mip_texels(5).is_empty());
    }
}
