// SPDX-License-Identifier: MPL-2.0
// Source: clean-room behavior spec `radial_blur.md` ("Radial blur (Spin and Zoom): behavior
// spec", written 2026-10-24 from the Lumit repository at commit fa959219, GPL-3.0; the spec is
// behavior-only and carries no source identifiers); Gonzalez & Woods, "Digital Image
// Processing" (spatial filtering, motion-blur box kernels, bilinear interpolation); the WebGPU
// and OpenGL texture-addressing specifications (clamp and mirror edge modes).

//! CPU reference implementation of radial blur in its two modes, **spin** and **zoom**, plus the
//! premultiplied-linear RGBA buffer it operates on. This is the test oracle for the GPU port
//! that follows: no GPU code, no shaders, only the exact arithmetic a WGSL pass has to reproduce.
//!
//! Both modes average a box of bilinear samples along a segment of length `L` centered on each
//! pixel: zoom samples along the ray through the center, spin along the tangent perpendicular to
//! it (the first-order approximation of the arc). The length grows linearly with the pixel's
//! distance from the center and is zero at the center, so no singularity guard exists or is
//! needed: the per-sample offset is `direction * amount * frac / half_diagonal`, and at the
//! center the direction vector itself is zero.
//!
//! # Documented decisions for the spec's flagged gaps
//!
//! * **Half-diagonal rule (spec § 4.1):** the reference keeps `L = amount * |d| / (D / 2)` as
//!   written. With an off-center center the far corners sit farther than half a diagonal and get
//!   more than `amount` (up to about twice as much with the center in a corner). The
//!   normalize-by-farthest-corner alternative is not taken; the two agree when centered.
//! * **Sample count (spec § 4.3):** `N = ceil(amount)` clamped to `1..=511`, the same for every
//!   pixel. Sizing `N` from the true maximum length is not taken.
//! * **Mirror edges (spec § 4.4):** one reflection about the edge (without repeating the edge
//!   texel), then clamp — coordinates more than one frame width outside fall back to the edge
//!   texel. Periodic reflection is not taken.
//! * **"Smooth zoom" preset (spec § 7):** not present in the source. [`SMOOTH_ZOOM`] is a named
//!   Regia preset constant: zoom mode, repeat edges, and `amount` linear in a strength control
//!   ([`smooth_zoom`]). The scale-about-the-center half and the ease curve over time belong to
//!   the transform and animation layers, not to the blur.
//! * **Blend tail (spec § 3):** the 26-mode blend is the common effect tail shared by every
//!   effect; it is out of scope here. [`apply`] ends at the mix step and its output feeds that
//!   tail.
//! * **Matte (spec § 4.5):** the per-pixel strength is the chosen channel of the matte texel as
//!   stored (Rec. 709 luma of RGB for the luminance choice), inverted when asked, clamped to
//!   `0..=1`. Unpremultiplying the matte is the matte preparer's job. The strength scales the
//!   blur length only; the generic "fade the result by the matte" step does not run.
//! * **Non-finite parameters (spec § 8):** a non-finite `amount` or `mix` behaves as 0; a
//!   non-finite center component falls back to the frame center. A non-finite sample coordinate
//!   reads the origin texel. Together these keep every output finite.
//!
//! Texel `(x, y)` is sampled at continuous coordinates `(x, y)` (texel centers on integers), so
//! the default center `(width / 2, height / 2)` lies exactly half a diagonal from the corner
//! texel `(0, 0)`, as spec § 4.1 describes.

/// Upper cap on samples per pixel (spec § 4.3). Above it the sample spacing exceeds one pixel
/// and fine detail can step; the reference accepts that, as the source does.
pub const MAX_SAMPLES: u32 = 511;

/// Default blur length in pixels at half the frame diagonal (spec § 3).
pub const DEFAULT_AMOUNT: f32 = 150.0;

/// Rec. 709 luma coefficients, the matte strength definition of spec § 4.5.
pub const MATTE_LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Amount per unit strength of the "smooth zoom" preset. A Regia design decision (the preset is
/// absent from the source, spec § 7): one strength control drives zoom-mode amount linearly.
pub const SMOOTH_ZOOM_AMOUNT_PER_STRENGTH: f32 = 120.0;

/// The named "smooth zoom" preset at strength 1: zoom mode, repeat edges, full mix, amount
/// [`SMOOTH_ZOOM_AMOUNT_PER_STRENGTH`], center on the composition. Not derived from the source
/// (spec § 7): the scale-about-the-center and the ease curve over time live in the transform and
/// animation layers; this constant is the radial-blur half of the look.
pub const SMOOTH_ZOOM: RadialBlurParams = RadialBlurParams {
    mode: RadialBlurMode::Zoom,
    amount: SMOOTH_ZOOM_AMOUNT_PER_STRENGTH,
    center: None,
    edges: EdgeMode::Repeat,
    mix: 1.0,
    matte_channel: MatteChannel::Luminance,
    invert_matte: false,
};

/// Errors returned by the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RadialBlurError {
    /// A texel buffer length does not match its declared size.
    #[error("texel buffer has {len} texels, but {width}x{height} needs {expected}")]
    TexelCountMismatch {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
        /// Buffer length received.
        len: usize,
        /// Buffer length expected.
        expected: usize,
    },
    /// The matte layer does not match the picture's size.
    #[error("matte is {matte_width}x{matte_height}, but the picture is {width}x{height}")]
    MatteSizeMismatch {
        /// Picture width.
        width: u32,
        /// Picture height.
        height: u32,
        /// Matte width.
        matte_width: u32,
        /// Matte height.
        matte_height: u32,
    },
}

/// A scene-linear, premultiplied RGBA picture in 32-bit float, row-major. Color is premultiplied
/// by alpha; the blur treats all four channels the same way (spec § 2).
#[derive(Clone, Debug, PartialEq)]
pub struct RgbaImage {
    width: u32,
    height: u32,
    texels: Vec<[f32; 4]>,
}

impl RgbaImage {
    /// A zero-filled (fully transparent) picture.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            texels: vec![[0.0; 4]; width as usize * height as usize],
        }
    }

    /// A picture filled with one value.
    #[must_use]
    pub fn filled(width: u32, height: u32, value: [f32; 4]) -> Self {
        Self {
            width,
            height,
            texels: vec![value; width as usize * height as usize],
        }
    }

    /// Builds a picture from a row-major texel buffer, checking that its length matches
    /// `width * height`.
    pub fn from_texels(width: u32, height: u32, texels: Vec<[f32; 4]>) -> Result<Self, RadialBlurError> {
        let expected = width as usize * height as usize;
        if texels.len() != expected {
            return Err(RadialBlurError::TexelCountMismatch {
                width,
                height,
                len: texels.len(),
                expected,
            });
        }
        Ok(Self {
            width,
            height,
            texels,
        })
    }

    /// Width in texels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in texels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The whole row-major texel buffer.
    #[must_use]
    pub fn texels(&self) -> &[[f32; 4]] {
        &self.texels
    }

    /// One texel, or `None` outside the frame.
    #[must_use]
    pub fn get(&self, x: u32, y: u32) -> Option<[f32; 4]> {
        if x < self.width && y < self.height {
            Some(self.texels[y as usize * self.width as usize + x as usize])
        } else {
            None
        }
    }

    /// Writes one texel. Out-of-range writes are ignored.
    pub fn set(&mut self, x: u32, y: u32, value: [f32; 4]) {
        if x < self.width && y < self.height {
            self.texels[y as usize * self.width as usize + x as usize] = value;
        }
    }
}

/// The two smear directions of the effect (spec § 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RadialBlurMode {
    /// Average along the tangent perpendicular to the ray through the center: the first-order
    /// approximation of an arc around the center. The default.
    #[default]
    Spin,
    /// Average along the straight ray through the center and the pixel: the picture smears
    /// outward and inward from the center.
    Zoom,
}

/// What a bilinear read past the frame resolves to, per axis (spec § 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeMode {
    /// Out-of-frame texels read transparent black but keep their bilinear weight, so the blur
    /// fades toward transparent near the borders.
    Transparent,
    /// Out-of-frame coordinates clamp to the nearest edge texel. The default: full-frame footage
    /// never darkens at the border.
    #[default]
    Repeat,
    /// Reflect about the edge without repeating the edge texel (texel -1 reads texel 1); one
    /// reflection, then clamp, so anything farther out falls back to the edge texel.
    Mirror,
}

/// Which channel of the matte layer drives its per-pixel strength (spec § 3, § 4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatteChannel {
    /// Rec. 709 luma of the matte's RGB. The default.
    #[default]
    Luminance,
    /// The matte's alpha channel.
    Alpha,
    /// The matte's red channel.
    R,
    /// The matte's green channel.
    G,
    /// The matte's blue channel.
    B,
}

/// Parameters of one radial-blur application (spec § 3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadialBlurParams {
    /// Spin or zoom. Default spin.
    pub mode: RadialBlurMode,
    /// Blur length in pixels at half the frame diagonal from the center. Floor 0, no upper
    /// limit; a non-finite value behaves as 0. Default [`DEFAULT_AMOUNT`] (150).
    pub amount: f32,
    /// Effect center in pixels. `None` (the default) centers on the frame at
    /// `(width / 2, height / 2)`; an explicit center may lie outside the frame. A non-finite
    /// component falls back to the frame center.
    pub center: Option<[f32; 2]>,
    /// What samples past the frame read. Default repeat.
    pub edges: EdgeMode,
    /// Linear blend of original (0) and blurred (1), in premultiplied linear space after the
    /// blur. Clamped to `0..=1`; a non-finite value behaves as 0. Default 1.
    pub mix: f32,
    /// Which matte channel drives the per-pixel strength. Default luminance.
    pub matte_channel: MatteChannel,
    /// Invert the prepared matte before reading its strength. Default off.
    pub invert_matte: bool,
}

impl Default for RadialBlurParams {
    fn default() -> Self {
        Self {
            mode: RadialBlurMode::Spin,
            amount: DEFAULT_AMOUNT,
            center: None,
            edges: EdgeMode::Repeat,
            mix: 1.0,
            matte_channel: MatteChannel::Luminance,
            invert_matte: false,
        }
    }
}

/// The "smooth zoom" preset at an arbitrary strength: zoom mode, repeat edges, and
/// `amount = SMOOTH_ZOOM_AMOUNT_PER_STRENGTH * strength`. Negative strength floors to 0 and a
/// non-finite strength behaves as 0 (amount 0, an exact passthrough).
#[must_use]
pub fn smooth_zoom(strength: f32) -> RadialBlurParams {
    let amount = if strength.is_finite() {
        SMOOTH_ZOOM_AMOUNT_PER_STRENGTH * strength.max(0.0)
    } else {
        0.0
    };
    RadialBlurParams {
        amount,
        ..SMOOTH_ZOOM
    }
}

/// Half the frame diagonal in pixels, the distance from the frame center to the corner texel
/// `(0, 0)` (spec § 4.1). Zero for a zero-sized frame.
#[must_use]
pub fn half_diagonal(width: u32, height: u32) -> f32 {
    0.5 * (width as f32 * width as f32 + height as f32 * height as f32).sqrt()
}

/// The sample count of one application: the peak amount rounded up, clamped to
/// `1..=[MAX_SAMPLES]`, and the same for every pixel (spec § 4.3). Non-finite or negative
/// amounts count as 0, which clamps to one sample.
#[must_use]
pub fn sample_count(amount: f32) -> u32 {
    let amount = if amount.is_finite() { amount.max(0.0) } else { 0.0 };
    amount.ceil().clamp(1.0, MAX_SAMPLES as f32) as u32
}

/// The blur length at `distance` from the center: `amount * distance / half_diagonal`
/// (spec § 4.1), so the length reaches `amount` at half the frame diagonal. `half_diagonal` must
/// be non-zero. With an off-center center this rule is kept as is: the far corners are farther
/// than half a diagonal and get more than `amount`.
#[must_use]
pub fn blur_length(amount: f32, half_diagonal: f32, distance: f32) -> f32 {
    amount * distance / half_diagonal
}

/// Resolves one bilinear corner index on one axis under the edge policy (spec § 4.4). In-frame
/// indices map to themselves. `None` means the corner reads transparent black but keeps its
/// bilinear weight. Repeat clamps to the nearest edge texel; mirror reflects once about the edge
/// without repeating the edge texel and then clamps. An empty axis has nothing to read.
#[must_use]
pub fn edge_texel(mode: EdgeMode, index: i32, extent: u32) -> Option<u32> {
    if extent == 0 {
        return None;
    }
    let last = extent as i32 - 1;
    if (0..=last).contains(&index) {
        return Some(index as u32);
    }
    match mode {
        EdgeMode::Transparent => None,
        EdgeMode::Repeat => Some(index.clamp(0, last) as u32),
        EdgeMode::Mirror => {
            let reflected = if index < 0 {
                index.saturating_neg()
            } else {
                2_i32.saturating_mul(last).saturating_sub(index)
            };
            Some(reflected.clamp(0, last) as u32)
        }
    }
}

/// The matte's per-pixel strength (spec § 4.5): the chosen channel of the prepared matte (Rec.
/// 709 luma of RGB for the luminance choice, [`MATTE_LUMA`]), inverted when asked, clamped to
/// `0..=1`. The matte texel is read as stored; a non-finite strength reads as 0.
#[must_use]
pub fn matte_strength(texel: [f32; 4], channel: MatteChannel, invert: bool) -> f32 {
    let value = match channel {
        MatteChannel::Luminance => {
            MATTE_LUMA[0] * texel[0] + MATTE_LUMA[1] * texel[1] + MATTE_LUMA[2] * texel[2]
        }
        MatteChannel::Alpha => texel[3],
        MatteChannel::R => texel[0],
        MatteChannel::G => texel[1],
        MatteChannel::B => texel[2],
    };
    let value = if invert { 1.0 - value } else { value };
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Bilinear read at continuous texel-space coordinates under the edge policy (spec § 4.2, § 4.4).
/// Texel `i` has its center at coordinate `i`; `x` runs right and `y` runs down. Non-finite
/// coordinates read the origin texel so absurd parameters cannot poison the frame with NaN.
#[must_use]
pub fn sample_bilinear(image: &RgbaImage, mode: EdgeMode, x: f32, y: f32) -> [f32; 4] {
    if image.width == 0 || image.height == 0 {
        return [0.0; 4];
    }
    let x = if x.is_finite() { x } else { 0.0 };
    let y = if y.is_finite() { y } else { 0.0 };
    let tx = x - x.floor();
    let ty = y - y.floor();
    let xi0 = x.floor() as i32;
    let yi0 = y.floor() as i32;
    let xi1 = xi0.saturating_add(1);
    let yi1 = yi0.saturating_add(1);
    let corner = |xi: i32, yi: i32| -> [f32; 4] {
        match (
            edge_texel(mode, xi, image.width),
            edge_texel(mode, yi, image.height),
        ) {
            (Some(cx), Some(cy)) => image.texels[cy as usize * image.width as usize + cx as usize],
            _ => [0.0; 4],
        }
    };
    let a = corner(xi0, yi0);
    let b = corner(xi1, yi0);
    let c = corner(xi0, yi1);
    let d = corner(xi1, yi1);
    let top = lerp4(a, b, tx);
    let bottom = lerp4(c, d, tx);
    lerp4(top, bottom, ty)
}

/// `a + (b - a) * t` per channel: exact at `t` 0 and 1 and for constant corners.
fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let mut out = [0.0; 4];
    for ((o, a), b) in out.iter_mut().zip(&a).zip(&b) {
        *o = a + (b - a) * t;
    }
    out
}

/// The center in pixels: the parameter, or the frame center `(width / 2, height / 2)`; a
/// non-finite component falls back to the frame center too.
fn effective_center(center: Option<[f32; 2]>, width: u32, height: u32) -> [f32; 2] {
    match center {
        Some([cx, cy]) if cx.is_finite() && cy.is_finite() => [cx, cy],
        _ => [width as f32 / 2.0, height as f32 / 2.0],
    }
}

/// Applies radial blur to one premultiplied-linear RGBA picture (spec § 1-§ 8).
///
/// The blur averages `sample_count(amount)` bilinear samples spaced at the centers of equal
/// sub-intervals of each pixel's segment, summed in a fixed order (spec § 5) so a GPU port has
/// to match this arithmetic only to floating-point tolerance. The matte, when present and
/// size-matched, scales the blur length per pixel; the mix then blends original and blurred in
/// premultiplied linear space. The effect never grows the layer: the output size equals the
/// input size, a zero-sized frame passes through unchanged, and amount 0 (or a black matte at a
/// pixel) returns that pixel exactly. The common blend tail (spec § 3) is not applied here.
///
/// # Errors
///
/// [`RadialBlurError::MatteSizeMismatch`] when a matte is given whose size differs from the
/// picture's.
pub fn apply(
    image: &RgbaImage,
    params: &RadialBlurParams,
    matte: Option<&RgbaImage>,
) -> Result<RgbaImage, RadialBlurError> {
    if let Some(m) = matte.filter(|m| m.width != image.width || m.height != image.height) {
        return Err(RadialBlurError::MatteSizeMismatch {
            width: image.width,
            height: image.height,
            matte_width: m.width,
            matte_height: m.height,
        });
    }
    // A zero-sized frame: no output change (spec § 8).
    if image.width == 0 || image.height == 0 {
        return Ok(image.clone());
    }
    let amount = if params.amount.is_finite() {
        params.amount.max(0.0)
    } else {
        0.0
    };
    // Amount 0 returns the input exactly, at any mix (spec § 4.1, § 8).
    if amount == 0.0 {
        return Ok(image.clone());
    }
    let mix = if params.mix.is_finite() {
        params.mix.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let center = effective_center(params.center, image.width, image.height);
    let unit = amount / half_diagonal(image.width, image.height);
    let n = sample_count(amount);
    let n_f = n as f32;
    let half_n = n_f * 0.5;
    let width = image.width as usize;
    let mut out = vec![[0.0_f32; 4]; image.texels.len()];
    for py in 0..image.height {
        for px in 0..image.width {
            let idx = py as usize * width + px as usize;
            let dx = px as f32 - center[0];
            let dy = py as f32 - center[1];
            let (dir_x, dir_y) = match params.mode {
                RadialBlurMode::Zoom => (dx, dy),
                RadialBlurMode::Spin => (-dy, dx),
            };
            let strength = match matte {
                Some(m) => matte_strength(m.texels[idx], params.matte_channel, params.invert_matte),
                None => 1.0,
            };
            // The per-sample offset is `dir * scale * frac` with `frac` in (-0.5, 0.5): the |d|
            // in the length formula cancels against the direction's normalization, so the
            // center pixel needs no special case (its direction vector is zero).
            let scale = unit * strength;
            let orig = image.texels[idx];
            let blurred = if scale == 0.0 {
                // A black matte pixel sweeps no length: exact passthrough (spec § 4.5, § 8).
                orig
            } else {
                let mut sum = [0.0_f32; 4];
                for k in 0..n {
                    let frac = ((k as f32 + 0.5) - half_n) / n_f;
                    let c = scale * frac;
                    let s =
                        sample_bilinear(image, params.edges, px as f32 + dir_x * c, py as f32 + dir_y * c);
                    for (acc, v) in sum.iter_mut().zip(s) {
                        *acc += v;
                    }
                }
                [sum[0] / n_f, sum[1] / n_f, sum[2] / n_f, sum[3] / n_f]
            };
            out[idx] = [
                orig[0] * (1.0 - mix) + blurred[0] * mix,
                orig[1] * (1.0 - mix) + blurred[1] * mix,
                orig[2] * (1.0 - mix) + blurred[2] * mix,
                orig[3] * (1.0 - mix) + blurred[3] * mix,
            ];
        }
    }
    Ok(RgbaImage {
        width: image.width,
        height: image.height,
        texels: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random values for non-trivial pictures (no wall-clock, no global
    /// state).
    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *state
    }

    fn noise_image(width: u32, height: u32, seed: u32) -> RgbaImage {
        let mut state = seed;
        let mut img = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let v = |s: &mut u32| lcg(s) as f32 / 4_000_000_000.0;
                img.set(x, y, [v(&mut state), v(&mut state), v(&mut state), 1.0]);
            }
        }
        img
    }

    fn applied(image: &RgbaImage, params: &RadialBlurParams, matte: Option<&RgbaImage>) -> RgbaImage {
        let Ok(out) = apply(image, params, matte) else {
            panic!("apply failed");
        };
        out
    }

    // --- spec § 4.1: blur length and the half-diagonal rule ------------------------------

    #[test]
    fn blur_length_reaches_amount_at_half_diagonal() {
        // 8x8 frame: every corner sits exactly half a diagonal from the default center (4, 4).
        let hd = half_diagonal(8, 8);
        let corner = (4.0_f32 * 4.0 + 4.0_f32 * 4.0).sqrt();
        for amount in [1.0_f32, 12.0, 150.0, 2000.0] {
            let l = blur_length(amount, hd, corner);
            assert!((l - amount).abs() < 1e-3 * amount, "amount {amount}: {l}");
        }
        // The center itself has length zero.
        assert_eq!(blur_length(150.0, hd, 0.0), 0.0);
    }

    #[test]
    fn blur_length_exceeds_amount_off_center() {
        // Keeping the half-diagonal rule (documented decision): with the center in a corner the
        // far corner is 7/4 of half a diagonal away and gets 1.75x amount.
        let hd = half_diagonal(8, 8);
        let far = (7.0_f32 * 7.0 + 7.0_f32 * 7.0).sqrt();
        let l = blur_length(100.0, hd, far);
        assert!((l - 175.0).abs() < 0.2, "far corner length {l}");
        assert!(l > 100.0, "off-center far corners exceed the amount");
    }

    #[test]
    fn sample_count_clamps_one_to_511() {
        assert_eq!(sample_count(0.0), 1);
        assert_eq!(sample_count(f32::NAN), 1);
        assert_eq!(sample_count(-40.0), 1);
        assert_eq!(sample_count(0.999), 1);
        assert_eq!(sample_count(1.0), 1);
        assert_eq!(sample_count(1.001), 2);
        assert_eq!(sample_count(150.0), 150);
        assert_eq!(sample_count(510.5), 511);
        assert_eq!(sample_count(511.0), 511);
        assert_eq!(sample_count(2000.0), 511);
        assert_eq!(sample_count(1.0e30), 511);
        assert_eq!(sample_count(f32::INFINITY), 1);
    }

    // --- spec § 4.1/§ 8: amount 0 and non-finite amounts are exact passthrough -----------

    #[test]
    fn zero_amount_is_identity() {
        let img = noise_image(9, 7, 42);
        for amount in [0.0, -50.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let params = RadialBlurParams {
                amount,
                ..RadialBlurParams::default()
            };
            let out = applied(&img, &params, None);
            assert_eq!(out, img, "amount {amount}");
            // Any mix: the blurred half equals the original, so the mix cannot change anything.
            let mixed = RadialBlurParams {
                amount,
                mix: 0.37,
                ..RadialBlurParams::default()
            };
            assert_eq!(applied(&img, &mixed, None), img, "amount {amount}, mixed");
        }
    }

    #[test]
    fn amount_below_one_samples_only_the_pixel() {
        // N = ceil(amount) = 1 and the single sample sits at the center of the segment, on the
        // pixel itself: an exact passthrough for any values (spec § 4.2, § 4.3).
        let img = noise_image(6, 5, 7);
        let params = RadialBlurParams {
            mode: RadialBlurMode::Zoom,
            amount: 0.999,
            ..RadialBlurParams::default()
        };
        assert_eq!(applied(&img, &params, None), img);
        let spin = RadialBlurParams {
            amount: 1.0,
            ..RadialBlurParams::default()
        };
        assert_eq!(applied(&img, &spin, None), img);
    }

    #[test]
    fn huge_amount_stays_finite() {
        // "Very large Amount: length unbounded, sample count capped at 511" (spec § 8): the
        // run must stay finite, not panic and not produce NaN.
        let img = noise_image(6, 6, 99);
        let params = RadialBlurParams {
            amount: 1.0e30,
            ..RadialBlurParams::default()
        };
        let out = applied(&img, &params, None);
        for t in out.texels() {
            for c in t {
                assert!(c.is_finite(), "channel {c}");
            }
        }
    }

    // --- constant images stay constant (spec § 4.2: equal box weights) -------------------

    #[test]
    fn constant_image_stays_constant() {
        let value = [1.0, 0.75, 0.5, 0.25];
        let img = RgbaImage::filled(9, 7, value);
        for mode in [RadialBlurMode::Spin, RadialBlurMode::Zoom] {
            for edges in [EdgeMode::Repeat, EdgeMode::Mirror] {
                let params = RadialBlurParams {
                    mode,
                    edges,
                    amount: 300.0,
                    ..RadialBlurParams::default()
                };
                assert_eq!(applied(&img, &params, None), img, "{mode:?} / {edges:?}");
            }
        }
        // An off-center center changes the lengths but a constant picture still cannot move.
        let off_center = RadialBlurParams {
            mode: RadialBlurMode::Zoom,
            edges: EdgeMode::Repeat,
            amount: 300.0,
            center: Some([2.25, 5.5]),
            ..RadialBlurParams::default()
        };
        assert_eq!(applied(&img, &off_center, None), img);
    }

    // --- symmetric input, symmetric output ----------------------------------------------

    #[test]
    fn point_symmetric_input_gives_point_symmetric_output() {
        // 8x6 with the center at (3.5, 2.5): the grid mirror (x, y) -> (7 - x, 5 - y) is exactly
        // the point reflection about the center, so both modes must return a point-symmetric
        // picture. Differences can only be a few ulps of rounding (mirrored bilinear weights
        // round at different points).
        let mut img = RgbaImage::new(8, 6);
        let mut state = 5_u32;
        for y in 0..3 {
            for x in 0..8 {
                let v = |s: &mut u32| lcg(s) as f32 / 4_000_000_000.0;
                let t = [v(&mut state), v(&mut state), v(&mut state), v(&mut state)];
                img.set(x, y, t);
                img.set(7 - x, 5 - y, t);
            }
        }
        for y in 0..6 {
            for x in 0..8 {
                let a = img.get(x, y);
                let b = img.get(7 - x, 5 - y);
                assert_eq!(a, b, "input symmetric at ({x},{y})");
            }
        }
        for mode in [RadialBlurMode::Spin, RadialBlurMode::Zoom] {
            let params = RadialBlurParams {
                mode,
                amount: 12.0,
                center: Some([3.5, 2.5]),
                ..RadialBlurParams::default()
            };
            let out = applied(&img, &params, None);
            for y in 0..6 {
                for x in 0..8 {
                    let a = out.get(x, y);
                    let b = out.get(7 - x, 5 - y);
                    let (Some(a), Some(b)) = (a, b) else {
                        panic!("output texel missing");
                    };
                    for c in 0..4 {
                        assert!(
                            (a[c] - b[c]).abs() < 1e-5,
                            "{mode:?} ({x},{y}) channel {c}: {} vs {}",
                            a[c],
                            b[c]
                        );
                    }
                }
            }
            // The blur is non-trivial: the output differs from the input.
            assert!(out.texels() != img.texels(), "{mode:?} did not blur");
        }
    }

    // --- zoom streaks radially, spin transversely (point source) -------------------------

    #[test]
    fn zoom_streaks_radially_and_spin_transversely() {
        // A single white texel on the horizontal axis through the center. Zoom smears along the
        // ray (horizontal), spin along the tangent (vertical). The second-moment tensor of the
        // output mass must be strongly elongated along the expected axis and free of a cross
        // term. Amount 20 on this frame keeps the spin sweep half-angle at
        // atan(0.49 * 20 / 28.3) ~ 19 degrees, inside the tangent approximation's range: at
        // larger sweeps the tangent departs from the arc (spec § 4.2) and the smear curls
        // around the circle instead of staying transverse.
        let mut img = RgbaImage::new(40, 40);
        let source = (30_u32, 20_u32);
        img.set(source.0, source.1, [1.0, 1.0, 1.0, 1.0]);
        let amount = 20.0_f32;
        let moments = |mode: RadialBlurMode| {
            let params = RadialBlurParams {
                mode,
                amount,
                ..RadialBlurParams::default()
            };
            let out = applied(&img, &params, None);
            let (mut mxx, mut myy, mut mxy, mut mass) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
            for y in 0..out.height() {
                for x in 0..out.width() {
                    let Some(t) = out.get(x, y) else {
                        panic!("texel missing");
                    };
                    let w = t[0];
                    let (dx, dy) = (x as f32 - source.0 as f32, y as f32 - source.1 as f32);
                    mxx += w * dx * dx;
                    myy += w * dy * dy;
                    mxy += w * dx * dy;
                    mass += w;
                }
            }
            (mxx, myy, mxy, mass, out)
        };
        // Zoom: radial (horizontal) streak.
        let (mxx, myy, mxy, mass, zoom_out) = moments(RadialBlurMode::Zoom);
        assert!(mass > 0.0, "zoom moved no mass");
        assert!(mxx > 4.0 * myy, "zoom not radial: {mxx} vs {myy}");
        assert!(mxy.abs() < 0.25 * mxx, "zoom streak tilted: {mxy}");
        // Spin: transverse (vertical) streak on the same source.
        let (mxx, myy, mxy, mass, spin_out) = moments(RadialBlurMode::Spin);
        assert!(mass > 0.0, "spin moved no mass");
        assert!(myy > 4.0 * mxx, "spin not transverse: {myy} vs {mxx}");
        assert!(mxy.abs() < 0.25 * myy, "spin streak tilted: {mxy}");
        // The point source is smeared, not kept: both modes dim the source texel.
        let Some(z) = zoom_out.get(source.0, source.1) else {
            panic!("texel missing");
        };
        assert!(z[0] < 0.5, "zoom kept the source: {}", z[0]);
        let Some(s) = spin_out.get(source.0, source.1) else {
            panic!("texel missing");
        };
        assert!(s[0] < 0.5, "spin kept the source: {}", s[0]);
        // The blur is exactly zero at the center itself (spec § 1): the center texel of the
        // black picture stays black.
        assert_eq!(zoom_out.get(20, 20), Some([0.0; 4]));
        assert_eq!(spin_out.get(20, 20), Some([0.0; 4]));
    }

    // --- spin preserves energy ------------------------------------------------------------

    #[test]
    fn spin_blur_preserves_energy() {
        // A smooth radial bump on a 128x128 frame with repeat edges: every sample stays inside
        // the frame's zero margin, so the only total-energy change is the tangent
        // approximation's second-order area thinning, about (amount / half-diagonal)^2 / 16 for
        // two samples. With amount 1.2 that is ~1.1e-5, an order of magnitude inside 1e-4.
        let size = 128_u32;
        let mut img = RgbaImage::new(size, size);
        let reach = 48.0_f32;
        for y in 0..size {
            for x in 0..size {
                let (dx, dy) = (x as f32 - 64.0, y as f32 - 64.0);
                let r = (dx * dx + dy * dy).sqrt();
                let t = if r < reach {
                    1.0 - (r / reach) * (r / reach)
                } else {
                    0.0
                };
                img.set(x, y, [t * t, t * t, t * t, 1.0]);
            }
        }
        let params = RadialBlurParams {
            mode: RadialBlurMode::Spin,
            amount: 1.2,
            edges: EdgeMode::Repeat,
            ..RadialBlurParams::default()
        };
        let out = applied(&img, &params, None);
        let energy = |image: &RgbaImage, alpha: bool| {
            image
                .texels()
                .iter()
                .map(|t| if alpha { t[3] } else { t[0] + t[1] + t[2] })
                .sum::<f32>()
        };
        let (rgb_in, rgb_out) = (energy(&img, false), energy(&out, false));
        assert!(rgb_in > 0.0);
        assert!(
            (rgb_out - rgb_in).abs() < 1e-4 * rgb_in,
            "rgb energy {rgb_in} -> {rgb_out}"
        );
        // Constant alpha is preserved exactly (each sample returns 1 under repeat edges).
        let (a_in, a_out) = (energy(&img, true), energy(&out, true));
        assert_eq!(a_out, a_in, "alpha energy");
    }

    // --- spec § 4.4: the edge table ------------------------------------------------------

    #[test]
    fn edge_texel_table() {
        // In-frame indices are identity in every mode.
        for mode in [EdgeMode::Transparent, EdgeMode::Repeat, EdgeMode::Mirror] {
            for i in 0..8_i32 {
                assert_eq!(edge_texel(mode, i, 8), Some(i as u32));
            }
        }
        // Transparent: out of frame reads nothing (zero with its weight).
        assert_eq!(edge_texel(EdgeMode::Transparent, -1, 8), None);
        assert_eq!(edge_texel(EdgeMode::Transparent, 8, 8), None);
        assert_eq!(edge_texel(EdgeMode::Transparent, 1000, 8), None);
        // Repeat: clamp to the nearest edge texel.
        assert_eq!(edge_texel(EdgeMode::Repeat, -1, 8), Some(0));
        assert_eq!(edge_texel(EdgeMode::Repeat, -1000, 8), Some(0));
        assert_eq!(edge_texel(EdgeMode::Repeat, 8, 8), Some(7));
        assert_eq!(edge_texel(EdgeMode::Repeat, 1000, 8), Some(7));
        // Mirror: reflect once about the edge without repeating the edge texel...
        assert_eq!(edge_texel(EdgeMode::Mirror, -1, 8), Some(1));
        assert_eq!(edge_texel(EdgeMode::Mirror, -2, 8), Some(2));
        assert_eq!(edge_texel(EdgeMode::Mirror, -7, 8), Some(7));
        assert_eq!(edge_texel(EdgeMode::Mirror, 8, 8), Some(6));
        assert_eq!(edge_texel(EdgeMode::Mirror, 9, 8), Some(5));
        assert_eq!(edge_texel(EdgeMode::Mirror, 14, 8), Some(0));
        // ...then clamp: more than one frame width outside falls back to the edge texel.
        assert_eq!(edge_texel(EdgeMode::Mirror, -8, 8), Some(7));
        assert_eq!(edge_texel(EdgeMode::Mirror, -1000, 8), Some(7));
        assert_eq!(edge_texel(EdgeMode::Mirror, 15, 8), Some(0));
        assert_eq!(edge_texel(EdgeMode::Mirror, 1000, 8), Some(0));
        // Extreme saturated indices resolve without overflow.
        assert_eq!(edge_texel(EdgeMode::Mirror, i32::MIN, 8), Some(7));
        assert_eq!(edge_texel(EdgeMode::Mirror, i32::MAX, 8), Some(0));
        // An empty axis has nothing to read.
        for mode in [EdgeMode::Transparent, EdgeMode::Repeat, EdgeMode::Mirror] {
            assert_eq!(edge_texel(mode, 0, 0), None);
        }
    }

    #[test]
    fn bilinear_reads_under_each_edge_mode() {
        // A 4x4 picture whose red encodes x+1 and green y+1 in exact eighths.
        let mut img = RgbaImage::new(4, 4);
        for y in 0..4_u32 {
            for x in 0..4_u32 {
                img.set(
                    x,
                    y,
                    [(x as f32 + 1.0) * 0.125, (y as f32 + 1.0) * 0.125, 0.5, 1.0],
                );
            }
        }
        // In-frame midpoint: plain bilinear, halfway between texels 0 and 1 on both axes.
        assert_eq!(
            sample_bilinear(&img, EdgeMode::Repeat, 0.5, 0.5),
            [0.1875, 0.1875, 0.5, 1.0]
        );
        // Mirror at x = -0.75: corners -1 -> texel 1 (r 0.25) and 0 (r 0.125), weights 0.75/0.25.
        assert_eq!(
            sample_bilinear(&img, EdgeMode::Mirror, -0.75, 0.0),
            [0.25 + (0.125 - 0.25) * 0.25, 0.125, 0.5, 1.0]
        );
        // Mirror at exactly x = -0.5: texel -1 reads texel 1 with half weight.
        assert_eq!(
            sample_bilinear(&img, EdgeMode::Mirror, -0.5, 0.0),
            [0.1875, 0.125, 0.5, 1.0]
        );
        // Transparent at x = -0.75: the whole corner -1 reads zero with its weight, so every
        // channel fades (spec § 4.4).
        assert_eq!(
            sample_bilinear(&img, EdgeMode::Transparent, -0.75, 0.0),
            [0.125 * 0.25, 0.125 * 0.25, 0.5 * 0.25, 1.0 * 0.25]
        );
        // Repeat at x = 3.5: corner 4 clamps to texel 3, so the read is texel 3 exactly.
        assert_eq!(
            sample_bilinear(&img, EdgeMode::Repeat, 3.5, 2.0),
            [0.5, 0.375, 0.5, 1.0]
        );
        // Non-finite coordinates read the origin texel instead of poisoning the frame.
        assert_eq!(
            sample_bilinear(&img, EdgeMode::Repeat, f32::NAN, 0.0),
            [0.125, 0.125, 0.5, 1.0]
        );
        assert_eq!(
            sample_bilinear(&img, EdgeMode::Repeat, f32::INFINITY, 0.0),
            [0.125, 0.125, 0.5, 1.0]
        );
        // An empty picture reads transparent black.
        assert_eq!(
            sample_bilinear(&RgbaImage::new(0, 4), EdgeMode::Mirror, 1.0, 1.0),
            [0.0; 4]
        );
    }

    #[test]
    fn transparent_fades_while_repeat_stays_opaque() {
        // A constant white picture under a long blur: repeat and mirror never darken, transparent
        // fades toward the border but keeps some weight (samples still land in frame).
        let img = RgbaImage::filled(8, 8, [1.0, 1.0, 1.0, 1.0]);
        let base = RadialBlurParams {
            amount: 40.0,
            center: Some([4.0, 4.0]),
            ..RadialBlurParams::default()
        };
        for edges in [EdgeMode::Repeat, EdgeMode::Mirror] {
            let params = RadialBlurParams { edges, ..base };
            assert_eq!(applied(&img, &params, None), img, "{edges:?}");
        }
        let params = RadialBlurParams {
            edges: EdgeMode::Transparent,
            ..base
        };
        let out = applied(&img, &params, None);
        let Some(corner) = out.get(7, 7) else {
            panic!("texel missing");
        };
        assert!(corner[3] > 0.0 && corner[3] < 0.95, "corner alpha {}", corner[3]);
        let Some(edge) = out.get(0, 4) else {
            panic!("texel missing");
        };
        assert!(edge[3] > 0.0 && edge[3] < 0.95, "edge alpha {}", edge[3]);
        // The center pixel keeps every sample on itself: exactly white, even under transparent
        // edges (the blur length is zero at the center, spec § 1).
        assert_eq!(out.get(4, 4), Some([1.0, 1.0, 1.0, 1.0]));
    }

    // --- spec § 4.5: the matte ------------------------------------------------------------

    #[test]
    fn matte_strength_channel_invert_clamp() {
        // Luminance follows Rec. 709.
        assert_eq!(
            matte_strength([1.0, 0.0, 0.0, 1.0], MatteChannel::Luminance, false),
            0.2126
        );
        assert_eq!(
            matte_strength([0.0, 1.0, 0.0, 1.0], MatteChannel::Luminance, false),
            0.7152
        );
        assert_eq!(
            matte_strength([0.0, 0.0, 1.0, 1.0], MatteChannel::Luminance, false),
            0.0722
        );
        let white = matte_strength([1.0, 1.0, 1.0, 1.0], MatteChannel::Luminance, false);
        assert!((white - 1.0).abs() < 1e-6, "white luma {white}");
        // Channel choice.
        assert_eq!(
            matte_strength([0.25, 0.75, 0.5, 0.5], MatteChannel::Alpha, false),
            0.5
        );
        assert_eq!(
            matte_strength([0.25, 0.75, 0.5, 0.5], MatteChannel::R, false),
            0.25
        );
        assert_eq!(
            matte_strength([0.25, 0.75, 0.5, 0.5], MatteChannel::G, false),
            0.75
        );
        assert_eq!(
            matte_strength([0.25, 0.75, 0.5, 0.5], MatteChannel::B, false),
            0.5
        );
        // Invert, then clamp.
        assert_eq!(
            matte_strength([0.25, 0.75, 0.5, 0.5], MatteChannel::R, true),
            0.75
        );
        assert_eq!(matte_strength([2.0, 2.0, 2.0, 2.0], MatteChannel::R, false), 1.0);
        assert_eq!(matte_strength([-3.0, 0.0, 0.0, 0.0], MatteChannel::R, false), 0.0);
        assert_eq!(matte_strength([2.0, 2.0, 2.0, 2.0], MatteChannel::R, true), 0.0);
        assert_eq!(matte_strength([f32::NAN; 4], MatteChannel::R, false), 0.0);
    }

    #[test]
    fn black_matte_passes_through_and_white_equals_no_matte() {
        let img = noise_image(7, 6, 11);
        let params = RadialBlurParams {
            amount: 25.0,
            ..RadialBlurParams::default()
        };
        // A black matte everywhere: exact passthrough at full strength (spec § 4.5, § 8).
        let black = RgbaImage::filled(7, 6, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(applied(&img, &params, Some(&black)), img);
        // A white matte means full strength everywhere: identical to no matte, bit for bit.
        let white = RgbaImage::filled(7, 6, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(applied(&img, &params, Some(&white)), applied(&img, &params, None));
        // A half matte halves the strength: still a blur, not a passthrough.
        let gray = RgbaImage::filled(7, 6, [0.5, 0.5, 0.5, 1.0]);
        let out = applied(&img, &params, Some(&gray));
        assert!(out.texels() != img.texels(), "gray matte should still blur");
    }

    #[test]
    fn matte_region_gate() {
        // White matte on the left half, black on the right: the right half passes through
        // exactly while the left half blurs.
        let img = noise_image(8, 4, 3);
        let mut matte = RgbaImage::new(8, 4);
        for y in 0..4_u32 {
            for x in 0..8_u32 {
                let v = if x < 4 { 1.0 } else { 0.0 };
                matte.set(x, y, [v, v, v, 1.0]);
            }
        }
        let params = RadialBlurParams {
            amount: 30.0,
            ..RadialBlurParams::default()
        };
        let out = applied(&img, &params, Some(&matte));
        for y in 0..4_u32 {
            for x in 0..8_u32 {
                let (Some(o), Some(i)) = (out.get(x, y), img.get(x, y)) else {
                    panic!("texel missing");
                };
                if x >= 4 {
                    assert_eq!(o, i, "black-matte pixel ({x},{y}) must pass through");
                }
            }
        }
        assert!(out.texels() != img.texels(), "white half should blur");
    }

    // --- spec § 4.6: the mix --------------------------------------------------------------

    #[test]
    fn mix_zero_returns_original_and_clamps() {
        let img = noise_image(6, 6, 17);
        let amount_40 = RadialBlurParams {
            amount: 40.0,
            ..RadialBlurParams::default()
        };
        // Mix 0 (and anything below it, and a non-finite mix): the original picture.
        for mix in [0.0, -0.5, f32::NAN] {
            let params = RadialBlurParams { mix, ..amount_40 };
            assert_eq!(applied(&img, &params, None), img, "mix {mix}");
        }
        // Mix above 100% clamps to exactly the mix-100 result.
        let full = applied(&img, &amount_40, None);
        let params = RadialBlurParams {
            mix: 2.0,
            ..amount_40
        };
        assert_eq!(applied(&img, &params, None), full);
        // And the mix-100 result is a real blur.
        assert!(full.texels() != img.texels());
    }

    // --- spec § 8: edge cases -------------------------------------------------------------

    #[test]
    fn non_finite_center_falls_back_to_frame_center() {
        let img = noise_image(6, 6, 23);
        let nan_center = RadialBlurParams {
            amount: 20.0,
            center: Some([f32::NAN, 3.0]),
            ..RadialBlurParams::default()
        };
        let default_center = RadialBlurParams {
            amount: 20.0,
            ..RadialBlurParams::default()
        };
        assert_eq!(
            applied(&img, &nan_center, None),
            applied(&img, &default_center, None)
        );
    }

    #[test]
    fn zero_size_frame_passes_through() {
        for (w, h) in [(0_u32, 0_u32), (0, 5), (5, 0)] {
            let img = RgbaImage::new(w, h);
            let params = RadialBlurParams {
                amount: 150.0,
                ..RadialBlurParams::default()
            };
            let out = applied(&img, &params, None);
            assert_eq!(out.width(), w);
            assert_eq!(out.height(), h);
            assert!(out.texels().is_empty());
        }
    }

    #[test]
    fn error_paths() {
        // A texel buffer that does not match its declared size is rejected.
        let Err(e) = RgbaImage::from_texels(2, 2, vec![[0.0; 4]; 3]) else {
            panic!("from_texels should have failed");
        };
        assert_eq!(
            e,
            RadialBlurError::TexelCountMismatch {
                width: 2,
                height: 2,
                len: 3,
                expected: 4
            }
        );
        // A matte of a different size than the picture is rejected, whatever the amount.
        let img = RgbaImage::new(4, 4);
        let matte = RgbaImage::new(3, 4);
        let Err(e) = apply(&img, &RadialBlurParams::default(), Some(&matte)) else {
            panic!("apply should have failed");
        };
        assert_eq!(
            e,
            RadialBlurError::MatteSizeMismatch {
                width: 4,
                height: 4,
                matte_width: 3,
                matte_height: 4
            }
        );
        let zero = RadialBlurParams {
            amount: 0.0,
            ..RadialBlurParams::default()
        };
        assert!(matches!(
            apply(&img, &zero, Some(&matte)),
            Err(RadialBlurError::MatteSizeMismatch { .. })
        ));
    }

    // --- spec § 5: determinism ------------------------------------------------------------

    #[test]
    fn identical_runs_are_bitwise_identical() {
        let img = noise_image(16, 12, 31);
        let params = RadialBlurParams {
            amount: 37.0,
            ..RadialBlurParams::default()
        };
        let a = applied(&img, &params, None);
        let b = applied(&img, &params, None);
        assert_eq!(a, b);
        assert_eq!(a.width(), 16);
        assert_eq!(a.height(), 12);
        assert!(a.texels() != img.texels(), "the blur must move something");
    }

    // --- the "smooth zoom" preset (spec § 7; a Regia design, not from the source) ---------

    #[test]
    fn smooth_zoom_preset() {
        assert_eq!(SMOOTH_ZOOM.mode, RadialBlurMode::Zoom);
        assert_eq!(SMOOTH_ZOOM.edges, EdgeMode::Repeat);
        assert_eq!(SMOOTH_ZOOM.amount, SMOOTH_ZOOM_AMOUNT_PER_STRENGTH);
        assert_eq!(smooth_zoom(1.0), SMOOTH_ZOOM);
        assert_eq!(smooth_zoom(2.0).amount, 2.0 * SMOOTH_ZOOM_AMOUNT_PER_STRENGTH);
        assert_eq!(smooth_zoom(0.0).amount, 0.0);
        assert_eq!(smooth_zoom(-5.0).amount, 0.0);
        assert_eq!(smooth_zoom(f32::NAN).amount, 0.0);
        // Zero strength is an exact passthrough of any picture.
        let img = noise_image(5, 5, 77);
        assert_eq!(applied(&img, &smooth_zoom(0.0), None), img);
        // Strength 1 really blurs, in zoom mode, without darkening the border (repeat edges).
        let constant = RgbaImage::filled(8, 8, [0.5; 4]);
        assert_eq!(applied(&constant, &smooth_zoom(1.0), None), constant);
    }

    // --- the picture buffer itself ---------------------------------------------------------

    #[test]
    fn image_buffer_access() {
        let mut img = RgbaImage::new(3, 2);
        assert_eq!(img.width(), 3);
        assert_eq!(img.height(), 2);
        assert_eq!(img.texels().len(), 6);
        assert_eq!(img.get(0, 0), Some([0.0; 4]));
        assert_eq!(img.get(3, 0), None);
        assert_eq!(img.get(0, 2), None);
        img.set(2, 1, [0.25; 4]);
        assert_eq!(img.get(2, 1), Some([0.25; 4]));
        // Out-of-range writes are ignored, not fatal.
        img.set(3, 0, [1.0; 4]);
        img.set(0, 2, [1.0; 4]);
        assert_eq!(img.texels()[5], [0.25; 4]);
        let empty = RgbaImage::new(0, 3);
        assert!(empty.texels().is_empty());
        assert_eq!(empty.get(0, 0), None);
        assert_eq!(
            RgbaImage::filled(2, 2, [0.5; 4]).texels(),
            [[0.5; 4]; 4].as_slice()
        );
        let Ok(built) = RgbaImage::from_texels(2, 1, vec![[0.1; 4], [0.2; 4]]) else {
            panic!("from_texels failed");
        };
        assert_eq!(built.get(1, 0), Some([0.2; 4]));
    }
}
