// SPDX-License-Identifier: MPL-2.0
//! Baking authoring curves into fixed-size LUTs the GPU update shader can sample.
//!
//! Authoring keys ([`FloatKey`], [`ColorKey`]) are sparse and irregular; the GPU wants flat
//! arrays of a fixed sample count. [`CurveLut`] packs every curve in the frame into one `f32`
//! buffer: curve `id` occupies `[id * SAMPLES, (id + 1) * SAMPLES)`, linearly interpolated by
//! `t = age / life`. Constant curves (all keys equal, or empty) get a constant LUT so the shader
//! never branches.

use regia_core::math::Color;

use crate::format::{ColorKey, FloatKey};

/// Samples baked per curve.
pub const CURVE_SAMPLES: u32 = 8;

/// Packs many curves into one buffer of `(curve_count * CURVE_SAMPLES)` f32-quads (colour curves)
/// or f32s (scalar curves).
#[derive(Debug)]
pub struct CurveLut {
    /// Packed scalar LUTs: curve `i` = `scalars[i * SAMPLES..(i + 1) * SAMPLES]`.
    scalars: Vec<f32>,
    /// Packed colour LUTs (linear RGBA): curve `i` = 4 consecutive f32 per sample.
    colors: Vec<f32>,
    /// Deduplicated constant values so identical curves share ids.
    ids: std::collections::HashMap<CurveKey, u32>,
    /// Process-unique identity (a clone gets its own): with the append-only sample arrays,
    /// `(identity, lengths)` names the contents ([`Self::upload_key`]).
    identity: u64,
}

/// Source of [`CurveLut::identity`].
static NEXT_LUT_IDENTITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_identity() -> u64 {
    NEXT_LUT_IDENTITY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Default for CurveLut {
    fn default() -> Self {
        Self {
            scalars: Vec::new(),
            colors: Vec::new(),
            ids: std::collections::HashMap::new(),
            identity: next_identity(),
        }
    }
}

impl Clone for CurveLut {
    /// A clone may append different curves than its source, so it gets its own identity.
    fn clone(&self) -> Self {
        Self {
            scalars: self.scalars.clone(),
            colors: self.colors.clone(),
            ids: self.ids.clone(),
            identity: next_identity(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CurveKey {
    Scalar([u32; CURVE_SAMPLES as usize]),
    Color([u32; CURVE_SAMPLES as usize * 4]),
}

/// A handle to a baked curve inside a [`CurveLut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CurveId(pub u32);

impl CurveLut {
    /// Bakes a scalar curve (empty → constant `default_value`). Returns its id.
    pub fn bake_scalar(&mut self, keys: &[FloatKey], default_value: f32) -> CurveId {
        let mut samples = [0f32; CURVE_SAMPLES as usize];
        for (i, s) in samples.iter_mut().enumerate() {
            *s = sample_scalar(keys, i as f32 / (CURVE_SAMPLES - 1) as f32, default_value);
        }
        let key = CurveKey::Scalar(samples.map(f32::to_bits));
        let next_id = (self.scalars.len() / CURVE_SAMPLES as usize) as u32;
        let id = *self.ids.entry(key).or_insert(next_id);
        if id == next_id {
            self.scalars.extend_from_slice(&samples);
        }
        CurveId(id)
    }

    /// Bakes a colour curve (empty → constant white). Returns its id.
    pub fn bake_color(&mut self, keys: &[ColorKey]) -> CurveId {
        let mut samples = [0f32; CURVE_SAMPLES as usize * 4];
        for i in 0..CURVE_SAMPLES as usize {
            let c = sample_color(keys, i as f32 / (CURVE_SAMPLES - 1) as f32);
            samples[i * 4..i * 4 + 4].copy_from_slice(&c);
        }
        let key = CurveKey::Color(samples.map(f32::to_bits));
        let next_id = (self.colors.len() / (CURVE_SAMPLES as usize * 4)) as u32;
        let id = *self.ids.entry(key).or_insert(next_id);
        if id == next_id {
            self.colors.extend_from_slice(&samples);
        }
        CurveId(id)
    }

    /// Packed scalar samples, ready for `queue.write_buffer`.
    pub fn scalars(&self) -> &[f32] {
        &self.scalars
    }

    /// Packed colour samples, ready for `queue.write_buffer`.
    pub fn colors(&self) -> &[f32] {
        &self.colors
    }

    /// Names the current contents: the arrays only ever grow by appending, so this LUT's
    /// identity plus both lengths changes exactly when the samples do. [`crate::gpu::VfxGpu`]
    /// uploads a LUT only when its key differs from the last upload's.
    pub fn upload_key(&self) -> (u64, usize, usize) {
        (self.identity, self.scalars.len(), self.colors.len())
    }

    /// Number of baked curves of each kind (for buffer sizing).
    pub fn curve_counts(&self) -> (u32, u32) {
        (
            (self.scalars.len() / CURVE_SAMPLES as usize) as u32,
            (self.colors.len() / (CURVE_SAMPLES as usize * 4)) as u32,
        )
    }
}

/// Piecewise-linear scalar curve sample at `t` (clamped), or `default_value` when empty.
pub fn sample_scalar(keys: &[FloatKey], t: f32, default_value: f32) -> f32 {
    if keys.is_empty() {
        return default_value;
    }
    let t = t.clamp(0.0, 1.0);
    let mut prev = &keys[0];
    if t <= prev.t {
        return prev.v;
    }
    for key in &keys[1..] {
        if t <= key.t {
            let span = key.t - prev.t;
            let f = if span <= f32::EPSILON {
                0.0
            } else {
                (t - prev.t) / span
            };
            return prev.v + (key.v - prev.v) * f;
        }
        prev = key;
    }
    prev.v
}

/// Piecewise-linear colour gradient sample at `t` (clamped), or white when empty.
pub fn sample_color(keys: &[ColorKey], t: f32) -> [f32; 4] {
    if keys.is_empty() {
        return Color::WHITE.to_vec4().to_array();
    }
    let t = t.clamp(0.0, 1.0);
    let parse = |k: &ColorKey| {
        crate::format::parse_color(&k.color).unwrap_or_else(|_| Color::WHITE.to_vec4().to_array())
    };
    let mut prev_t = keys[0].t;
    let mut prev_c = parse(&keys[0]);
    if t <= prev_t {
        return prev_c;
    }
    for key in &keys[1..] {
        let c = parse(key);
        if t <= key.t {
            let span = key.t - prev_t;
            let f = if span <= f32::EPSILON {
                0.0
            } else {
                (t - prev_t) / span
            };
            let mut out = [0f32; 4];
            for (o, (a, b)) in out.iter_mut().zip(prev_c.iter().zip(c.iter())) {
                *o = a + (b - a) * f;
            }
            return out;
        }
        prev_t = key.t;
        prev_c = c;
    }
    prev_c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_sampling_is_clamped_and_linear() {
        let keys = [FloatKey { t: 0.0, v: 0.0 }, FloatKey { t: 1.0, v: 2.0 }];
        assert!((sample_scalar(&keys, -1.0, 9.0) - 0.0).abs() < 1e-6);
        assert!((sample_scalar(&keys, 0.25, 9.0) - 0.5).abs() < 1e-6);
        assert!((sample_scalar(&keys, 2.0, 9.0) - 2.0).abs() < 1e-6);
        assert!((sample_scalar(&[], 0.3, 7.0) - 7.0).abs() < 1e-6);
    }

    #[test]
    fn color_sampling_lerps_and_clamps() {
        let keys = [
            ColorKey {
                t: 0.0,
                color: "#000000".into(),
            },
            ColorKey {
                t: 1.0,
                color: "#ffffff".into(),
            },
        ];
        let mid = sample_color(&keys, 0.5);
        assert!((mid[0] - 0.5).abs() < 0.01, "{mid:?}");
        assert_eq!(sample_color(&keys, 0.0)[0], 0.0);
        let white = sample_color(&[], 0.5);
        assert!((white[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn baking_deduplicates_and_packs() {
        let mut lut = CurveLut::default();
        let keys = [FloatKey { t: 0.0, v: 1.0 }, FloatKey { t: 1.0, v: 1.0 }];
        let a = lut.bake_scalar(&keys, 1.0);
        let b = lut.bake_scalar(&keys, 1.0);
        assert_eq!(a, b);
        assert_eq!(lut.curve_counts().0, 1);
        assert_eq!(lut.scalars().len(), CURVE_SAMPLES as usize);
        let ramp = lut.bake_scalar(&[FloatKey { t: 0.0, v: 0.0 }, FloatKey { t: 1.0, v: 1.0 }], 1.0);
        assert_ne!(ramp, a);
        assert_eq!(lut.curve_counts().0, 2);
        // Ramp LUT endpoints are 0 and 1.
        assert!(
            lut.scalars()[CURVE_SAMPLES as usize..]
                .first()
                .is_some_and(|v| v.abs() < 1e-6)
        );
        assert!(lut.scalars().last().is_some_and(|v| (v - 1.0).abs() < 1e-6));
    }
}
