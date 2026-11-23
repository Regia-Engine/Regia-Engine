// SPDX-License-Identifier: MPL-2.0
//! Equirectangular environment maps (HDRI skies) as tracer and bake inputs, and the CPU side
//! of the sky: the same lookup the kernels make ([`PtSky::radiance`]), its SH projection and
//! exact irradiance (the references of the bake's energy checks), and sun extraction.
//!
//! **Mapping** ([`equirect_uv`]): row 0 is the zenith (+Y), the last row the nadir; the image's
//! horizontal centre looks along −Z (Regia Engine's forward) and `u` grows toward +X, so seen from
//! inside, the map reads as a panorama with +Y up and the right half of the image on the right.
//! A map is turned `rotation` radians about +Y (positive turns +Z toward +X).
//!
//! **Sun extraction** ([`EnvMap::extract_sun`]): an HDRI that holds the sun puts most of its
//! light in a few texels a fixed probe ray set rarely hits, and the runtime keeps the direct sun
//! live anyway. The brightest connected region above a threshold is lifted out of the map
//! (its texels take the mean of the ring around it) and becomes a sun disc carrying the excess:
//! direction, illuminance at normal incidence and angular radius from the region's texels, so
//! map + sun deliver the light the map alone did. A region whose directions are not a compact
//! disc (a full-width latitude band has a disc's area, but the vectors cancel) is refused and
//! the map is left unchanged.

use std::path::Path;

use regia_asset::baked_gi::{SH_Y00, SH_Y1, ShRgb};
use regia_core::glam::Vec3;

use crate::PtError;
use crate::pack::{sky_uv, table_sample};
use crate::scene::{PtSky, PtSun};

/// Most texels an equirect sky may hold (2048 × 1024: 32 MiB of RGBA f32 on the GPU).
pub const MAX_EQUIRECT_TEXELS: usize = 2048 * 1024;

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// `d` turned by `-rotation` about +Y (world → map space).
fn unrotate(d: Vec3, rotation: f32) -> Vec3 {
    let (s, c) = rotation.sin_cos();
    Vec3::new(d.x * c - d.z * s, d.y, d.x * s + d.z * c)
}

/// `d` turned by `rotation` about +Y (map → world space; +Z toward +X for a positive angle).
#[must_use]
pub fn rotate_y(d: [f32; 3], rotation: f32) -> [f32; 3] {
    let (s, c) = rotation.sin_cos();
    [d[0] * c + d[2] * s, d[1], -d[0] * s + d[2] * c]
}

/// Map coordinates (u, v) in [0, 1)² × [0, 1] of the world direction `d` for a map turned
/// `rotation` radians about +Y (the kernels' `equirect_uv`).
#[must_use]
pub fn equirect_uv(d: [f32; 3], rotation: f32) -> (f32, f32) {
    let l = unrotate(Vec3::from_array(d).normalize_or(Vec3::Y), rotation);
    let u = (0.5 + l.x.atan2(-l.z) / std::f32::consts::TAU).rem_euclid(1.0);
    let v = l.y.clamp(-1.0, 1.0).acos() / std::f32::consts::PI;
    (u, v)
}

/// The world direction of map coordinates (u, v) (the inverse of [`equirect_uv`]).
#[must_use]
pub fn equirect_dir(u: f32, v: f32, rotation: f32) -> [f32; 3] {
    let theta = v * std::f32::consts::PI;
    let phi = (u - 0.5) * std::f32::consts::TAU;
    let s = theta.sin();
    rotate_y([s * phi.sin(), theta.cos(), -s * phi.cos()], rotation)
}

impl PtSky {
    /// Radiance (cd/m²) toward the unit direction `d`, as the kernels look it up (bilinear,
    /// U wrapped, V clamped; scale applied).
    #[must_use]
    pub fn radiance(&self, d: [f32; 3]) -> [f32; 3] {
        match self {
            Self::None => [0.0; 3],
            Self::Uniform(c) => *c,
            Self::Table {
                width,
                height,
                texels,
                sun_direction,
                scale,
            } => {
                let sun = Vec3::from_array(*sun_direction).normalize_or(Vec3::Y);
                let (u, v) = sky_uv(Vec3::from_array(d), sun);
                let c = table_sample(*width, *height, texels, u, v);
                [c[0] * scale, c[1] * scale, c[2] * scale]
            }
            Self::Equirect {
                width,
                height,
                texels,
                rotation,
                scale,
            } => {
                let (u, v) = equirect_uv(d, *rotation);
                let c = table_sample(*width, *height, texels, u, v);
                [c[0] * scale, c[1] * scale, c[2] * scale]
            }
        }
    }
}

/// Midpoint quadrature over a `2·rows × rows` latitude-longitude grid of `f(direction, dω)`.
pub(crate) fn integrate_sphere(rows: u32, mut f: impl FnMut(Vec3, f32)) {
    let rows = rows.max(2);
    let cols = 2 * rows;
    let d_theta = std::f32::consts::PI / rows as f32;
    let d_phi = std::f32::consts::TAU / cols as f32;
    for j in 0..rows {
        let theta = (j as f32 + 0.5) * d_theta;
        let (st, ct) = theta.sin_cos();
        let d_omega = st * d_theta * d_phi;
        for i in 0..cols {
            let phi = (i as f32 + 0.5) * d_phi;
            let (sp, cp) = phi.sin_cos();
            f(Vec3::new(st * cp, ct, st * sp), d_omega);
        }
    }
}

/// The sky's radiance projected on the real L1 SH basis, `∫ L·Y dω` per channel, the
/// normalization of a probe trace's `sh` ([`crate::probe::ProbeTrace`]); midpoint quadrature
/// over `2·rows × rows` directions.
#[must_use]
pub fn sky_sh(sky: &PtSky, rows: u32) -> ShRgb {
    let mut acc = [[0.0f64; 4]; 3];
    integrate_sphere(rows, |d, w| {
        let l = sky.radiance(d.to_array());
        let basis = [SH_Y00, SH_Y1 * d.x, SH_Y1 * d.y, SH_Y1 * d.z];
        for (row, lc) in acc.iter_mut().zip(l) {
            for (r, b) in row.iter_mut().zip(basis) {
                *r += f64::from(lc * b * w);
            }
        }
    });
    acc.map(|row| row.map(|v| v as f32))
}

/// Irradiance (lux) the sky delivers to an unoccluded surface facing `n`, `∫ L·max(0, n·ω) dω`
/// (exact, not the L1 reconstruction).
#[must_use]
pub fn sky_irradiance(sky: &PtSky, n: [f32; 3], rows: u32) -> [f32; 3] {
    let n = Vec3::from_array(n).normalize_or(Vec3::Y);
    let mut acc = [0.0f64; 3];
    integrate_sphere(rows, |d, w| {
        let c = n.dot(d);
        if c > 0.0 {
            let l = sky.radiance(d.to_array());
            for (a, v) in acc.iter_mut().zip(l) {
                *a += f64::from(v * c * w);
            }
        }
    });
    acc.map(|v| v as f32)
}

/// A sun lifted out of an HDRI ([`EnvMap::extract_sun`]), in map space (before the map's
/// rotation; [`Self::to_sun`] applies it).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct ExtractedSun {
    /// Unit vector toward the sun, map space.
    pub toward: [f32; 3],
    /// Linear colour (luminance 1).
    pub color: [f32; 3],
    /// Illuminance at normal incidence, in map units (lux once the map is scaled to cd/m²).
    pub illuminance: f32,
    /// Angular radius of the disc of the region's solid angle, radians.
    pub angular_radius: f32,
    /// Solid angle of the lifted region, steradians.
    pub solid_angle: f32,
    /// Texels lifted.
    pub texels: usize,
    /// Luminance cut used (map units).
    pub threshold: f32,
}

impl ExtractedSun {
    /// The tracer's sun for a map turned `rotation` about +Y and scaled by `scale`.
    #[must_use]
    pub fn to_sun(self, rotation: f32, scale: f32) -> PtSun {
        PtSun {
            toward: rotate_y(self.toward, rotation),
            color: self.color,
            illuminance: self.illuminance * scale,
            angular_radius: self.angular_radius,
        }
    }
}

/// An equirect map in memory (linear RGB, row-major, top row = zenith).
#[derive(Clone, Debug, PartialEq)]
pub struct EnvMap {
    /// Width (360° of azimuth).
    pub width: u32,
    /// Height (180° of elevation).
    pub height: u32,
    /// `width × height` texels.
    pub rgb: Vec<[f32; 3]>,
}

impl EnvMap {
    /// The most texels `constant` and `from_fn` will build (2^28).
    pub const MAX_GENERATED_TEXELS: u64 = 1 << 28;

    fn generated_size_ok(width: u32, height: u32) -> bool {
        u64::from(width) * u64::from(height) <= Self::MAX_GENERATED_TEXELS
    }

    fn default_empty() -> Self {
        Self {
            width: 0,
            height: 0,
            rgb: Vec::new(),
        }
    }

    /// Whether `rgb` holds exactly `width * height` texels (the indexing methods need it).
    fn shape_ok(&self) -> bool {
        self.rgb.len() as u64 == u64::from(self.width) * u64::from(self.height)
    }

    /// A map of one value everywhere. A size above [`EnvMap::MAX_GENERATED_TEXELS`] gives an
    /// empty 0 × 0 map, which `validate` rejects.
    #[must_use]
    pub fn constant(width: u32, height: u32, c: [f32; 3]) -> Self {
        if !Self::generated_size_ok(width, height) {
            return Self::default_empty();
        }
        Self {
            width,
            height,
            rgb: vec![c; (width as usize) * (height as usize)],
        }
    }

    /// A map filled with `f(direction)` at the texel centres (rotation 0).
    #[must_use]
    pub fn from_fn(width: u32, height: u32, f: impl Fn([f32; 3]) -> [f32; 3]) -> Self {
        if !Self::generated_size_ok(width, height) {
            return Self::default_empty();
        }
        let mut rgb = Vec::with_capacity((width as usize) * (height as usize));
        for j in 0..height {
            for i in 0..width {
                rgb.push(f(Self::texel_dir_of(width, height, i, j)));
            }
        }
        Self { width, height, rgb }
    }

    fn texel_dir_of(width: u32, height: u32, i: u32, j: u32) -> [f32; 3] {
        equirect_dir(
            (i as f32 + 0.5) / width.max(1) as f32,
            (j as f32 + 0.5) / height.max(1) as f32,
            0.0,
        )
    }

    /// Direction of texel (i, j)'s centre, map space.
    #[must_use]
    pub fn texel_dir(&self, i: u32, j: u32) -> [f32; 3] {
        Self::texel_dir_of(self.width, self.height, i, j)
    }

    /// Solid angle of a texel in row `j`, steradians.
    #[must_use]
    pub fn texel_solid_angle(&self, j: u32) -> f32 {
        let h = self.height.max(1) as f32;
        let d_theta = std::f32::consts::PI / h;
        let d_phi = std::f32::consts::TAU / self.width.max(1) as f32;
        // Exact band area: (cos θ0 − cos θ1)·dφ.
        let t0 = j as f32 * d_theta;
        (t0.cos() - (t0 + d_theta).cos()) * d_phi
    }

    /// Reads an `.hdr`/`.pic` (RGBE) or uncompressed `.exr` (the plain `R`, `G`, `B` channels,
    /// else the first layer that has them).
    ///
    /// # Errors
    ///
    /// The file cannot be read, has an unknown extension, or does not decode.
    pub fn load(path: &Path) -> Result<Self, String> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match ext.as_str() {
            "hdr" | "pic" | "rgbe" => {
                let img = crate::hdr::read(path).map_err(|e| e.to_string())?;
                Ok(Self {
                    width: img.width,
                    height: img.height,
                    rgb: img.rgb,
                })
            }
            "exr" => {
                let img = crate::exr::read(path).map_err(|e| {
                    format!("{e} (compressed EXRs are not read: re-save uncompressed or as .hdr)")
                })?;
                let rgb = img
                    .rgb("")
                    .or_else(|| img.layers().iter().find_map(|l| img.rgb(l)))
                    .ok_or_else(|| "the EXR has no R, G, B channels".to_string())?;
                Ok(Self {
                    width: img.width,
                    height: img.height,
                    rgb,
                })
            }
            other => Err(format!(
                "unknown HDRI extension `.{other}` (read: .hdr, .pic, .exr)"
            )),
        }
    }

    /// Checks the texel count and that every value is finite and not negative.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("the map has no texels".into());
        }
        if self.rgb.len() != (self.width as usize) * (self.height as usize) {
            return Err(format!(
                "{} texels for a {} × {} map",
                self.rgb.len(),
                self.width,
                self.height
            ));
        }
        if let Some(k) = self
            .rgb
            .iter()
            .position(|c| c.iter().any(|v| !v.is_finite() || *v < 0.0))
        {
            return Err(format!(
                "texel ({}, {}) is negative or not finite: {:?}",
                k % self.width as usize,
                k / self.width as usize,
                self.rgb[k]
            ));
        }
        Ok(())
    }

    /// Box-filters the map by the smallest integer factor that brings its width to
    /// `max_width` or less and its texel count to [`MAX_EQUIRECT_TEXELS`] or less (the height by
    /// the same factor, at least 1). Each output texel is
    /// the solid-angle-weighted mean of its block, so the map's light is kept. Returns the
    /// factor (1 = unchanged).
    pub fn downsample_to(&mut self, max_width: u32) -> u32 {
        let mut k = self.width.div_ceil(max_width.max(1)).max(1);
        while (self.width.div_ceil(k) as usize) * (self.height.div_ceil(k).max(1) as usize)
            > MAX_EQUIRECT_TEXELS
        {
            k += 1;
        }
        if k == 1 || !self.shape_ok() {
            return 1;
        }
        let (w, h) = (self.width.div_ceil(k), self.height.div_ceil(k).max(1));
        let mut out = vec![[0.0f32; 3]; (w as usize) * (h as usize)];
        for (oj, row) in out.chunks_mut(w as usize).enumerate() {
            for (oi, px) in row.iter_mut().enumerate() {
                let mut sum = [0.0f64; 3];
                let mut wsum = 0.0f64;
                for j in (oj as u32 * k)..((oj as u32 + 1) * k).min(self.height) {
                    let dw = f64::from(self.texel_solid_angle(j));
                    for i in (oi as u32 * k)..((oi as u32 + 1) * k).min(self.width) {
                        let c = self.rgb[j as usize * self.width as usize + i as usize];
                        for (a, v) in sum.iter_mut().zip(c) {
                            *a += f64::from(v) * dw;
                        }
                        wsum += dw;
                    }
                }
                if wsum > 0.0 {
                    *px = sum.map(|v| (v / wsum) as f32);
                }
            }
        }
        self.width = w;
        self.height = h;
        self.rgb = out;
        k
    }

    /// Mean luminance over the sphere (solid-angle weighted).
    #[must_use]
    pub fn mean_luminance(&self) -> f32 {
        if !self.shape_ok() {
            return 0.0;
        }
        let mut sum = 0.0f64;
        for j in 0..self.height {
            let dw = f64::from(self.texel_solid_angle(j));
            for i in 0..self.width {
                sum += f64::from(lum(self.rgb[j as usize * self.width as usize + i as usize])) * dw;
            }
        }
        (sum / (4.0 * std::f64::consts::PI)) as f32
    }

    /// Lifts the sun out of the map (see the module docs). The region is the 8-connected set of
    /// texels around the brightest one whose luminance exceeds `threshold` × the mean
    /// luminance; its texels take the mean of the ring of texels around it, and the excess
    /// becomes the returned sun. `None` when no texel exceeds the cut.
    ///
    /// # Errors
    ///
    /// The region spans more than `max_solid_angle` steradians (it is not a sun: raise the
    /// threshold), its directions are not a compact disc (a full-width latitude band has a
    /// disc's area, but the vectors cancel), or it covers the whole map. The map is unchanged
    /// whenever this returns an error.
    pub fn extract_sun(
        &mut self,
        threshold: f32,
        max_solid_angle: f32,
    ) -> Result<Option<ExtractedSun>, String> {
        let _span = tracing::info_span!("pt_extract_sun", w = self.width, h = self.height).entered();
        let (w, h) = (self.width as usize, self.height as usize);
        if w == 0 || h == 0 || self.rgb.len() != w * h {
            return Ok(None);
        }
        let mean = self.mean_luminance();
        if mean.is_nan() || mean <= 0.0 || !threshold.is_finite() {
            return Ok(None);
        }
        let cut = threshold * mean;
        let Some((best, best_lum)) = self
            .rgb
            .iter()
            .map(|c| lum(*c))
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
        else {
            return Ok(None);
        };
        if best_lum <= cut {
            return Ok(None);
        }
        let neighbors = |k: usize| {
            let (i, j) = ((k % w) as i64, (k / w) as i64);
            let mut out = [usize::MAX; 8];
            let mut n = 0;
            for dj in -1..=1i64 {
                for di in -1..=1i64 {
                    let jj = j + dj;
                    if (di, dj) == (0, 0) || jj < 0 || jj >= h as i64 {
                        continue;
                    }
                    let ii = (i + di).rem_euclid(w as i64);
                    out[n] = jj as usize * w + ii as usize;
                    n += 1;
                }
            }
            out
        };
        let mut inside = vec![false; w * h];
        let mut region = vec![best];
        inside[best] = true;
        let mut next = 0;
        while next < region.len() {
            let k = region[next];
            next += 1;
            for q in neighbors(k) {
                if q != usize::MAX && !inside[q] && lum(self.rgb[q]) > cut {
                    inside[q] = true;
                    region.push(q);
                }
            }
        }
        let omega: f32 = region
            .iter()
            .map(|&k| self.texel_solid_angle((k / w) as u32))
            .sum();
        if omega > max_solid_angle {
            return Err(format!(
                "the brightest region above {threshold} × the mean luminance spans {omega:.4} sr \
                 ({} texels), more than {max_solid_angle} sr: it is not a sun disc (raise \
                 sun_threshold or turn extract_sun off)",
                region.len()
            ));
        }
        let mut ring_sum = [0.0f64; 3];
        let mut ring_w = 0.0f64;
        let mut seen = vec![false; w * h];
        for &k in &region {
            for q in neighbors(k) {
                if q != usize::MAX && !inside[q] && !seen[q] {
                    seen[q] = true;
                    let dw = f64::from(self.texel_solid_angle((q / w) as u32));
                    for (a, v) in ring_sum.iter_mut().zip(self.rgb[q]) {
                        *a += f64::from(v) * dw;
                    }
                    ring_w += dw;
                }
            }
        }
        if ring_w <= 0.0 {
            return Err("the region above the threshold covers the whole map".into());
        }
        let fill = ring_sum.map(|v| (v / ring_w) as f32);
        if let Some(err) = self.uncompact_disc_error(&region, fill, threshold, omega) {
            return Err(err);
        }
        // Direction: the excess-weighted centroid; then the excess at normal incidence.
        let mut centroid = Vec3::ZERO;
        for &k in &region {
            let dw = self.texel_solid_angle((k / w) as u32);
            let excess = (lum(self.rgb[k]) - lum(fill)).max(0.0);
            let d = Vec3::from_array(self.texel_dir((k % w) as u32, (k / w) as u32));
            centroid += d * excess * dw;
        }
        let toward = centroid.normalize_or(Vec3::Y);
        let mut e = [0.0f64; 3];
        for &k in &region {
            let dw = self.texel_solid_angle((k / w) as u32);
            let d = Vec3::from_array(self.texel_dir((k % w) as u32, (k / w) as u32));
            let cos = d.dot(toward).max(0.0);
            for ((a, v), f) in e.iter_mut().zip(self.rgb[k]).zip(fill) {
                *a += f64::from((v - f).max(0.0) * cos * dw);
            }
            self.rgb[k] = fill;
        }
        let e = e.map(|v| v as f32);
        let illuminance = lum(e);
        if illuminance.is_nan() || illuminance <= 0.0 {
            return Ok(None);
        }
        Ok(Some(ExtractedSun {
            toward: toward.to_array(),
            color: e.map(|v| v / illuminance),
            illuminance,
            angular_radius: (1.0 - omega / std::f32::consts::TAU).clamp(-1.0, 1.0).acos(),
            solid_angle: omega,
            texels: region.len(),
            threshold: cut,
        }))
    }

    /// The tracer's sky for this map.
    ///
    /// # Errors
    ///
    /// The map is malformed or holds more than [`MAX_EQUIRECT_TEXELS`] texels (downsample it
    /// first: [`Self::downsample_to`]).
    pub fn into_sky(self, rotation: f32, scale: f32) -> Result<PtSky, PtError> {
        self.validate().map_err(PtError::Scene)?;
        if self.rgb.len() > MAX_EQUIRECT_TEXELS {
            return Err(PtError::Scene(format!(
                "a {} × {} sky map is over the tracer's {MAX_EQUIRECT_TEXELS} texels: downsample it",
                self.width, self.height
            )));
        }
        Ok(PtSky::Equirect {
            width: self.width,
            height: self.height,
            texels: self.rgb.iter().map(|c| [c[0], c[1], c[2], 0.0]).collect(),
            rotation,
            scale,
        })
    }

    /// `Some` when `region` is not a compact disc, before any texel is overwritten.
    ///
    /// A spherical cap of solid angle Ω = 2π(1 − cos α) has mean resultant length at least
    /// cos α: every direction lies within α of the axis. A full-width latitude band has a small
    /// Ω, but the directions cancel, so normalizing the centroid points at a pole the texels do
    /// not face and the illuminance integral drops the band.
    fn uncompact_disc_error(
        &self,
        region: &[usize],
        fill: [f32; 3],
        threshold: f32,
        omega: f32,
    ) -> Option<String> {
        let w = self.width as usize;
        let mut acc = [0.0f64; 3];
        let mut weight = 0.0f64;
        for &k in region {
            let dw = f64::from(self.texel_solid_angle((k / w) as u32));
            let excess = f64::from((lum(self.rgb[k]) - lum(fill)).max(0.0));
            let wgt = excess * dw;
            if wgt == 0.0 {
                continue;
            }
            let d = self.texel_dir((k % w) as u32, (k / w) as u32);
            acc[0] += f64::from(d[0]) * wgt;
            acc[1] += f64::from(d[1]) * wgt;
            acc[2] += f64::from(d[2]) * wgt;
            weight += wgt;
        }
        if weight <= 0.0 {
            return None;
        }
        let len = (acc[0] * acc[0] + acc[1] * acc[1] + acc[2] * acc[2]).sqrt();
        let resultant = len / weight;
        // cos α of the cap with this solid angle. A hair of slack covers a texel centre that
        // sits just outside the cap the texel areas imply; a cancelled band is far below it.
        let min_resultant = f64::from((1.0 - omega / std::f32::consts::TAU).max(0.0));
        if resultant + 1.0e-4 < min_resultant {
            return Some(format!(
                "the brightest region above {threshold} × the mean luminance spans {omega:.4} sr \
                 ({} texels) but its directions are not a compact disc (mean resultant length \
                 {resultant:.4}, a disc of this area is at least {min_resultant:.4}): it is not a \
                 sun disc (raise sun_threshold or turn extract_sun off)",
                region.len()
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn the_mapping_centre_looks_forward_and_u_grows_to_the_right() {
        let (u, v) = equirect_uv([0.0, 0.0, -1.0], 0.0);
        assert!((u - 0.5).abs() < 1e-6 && (v - 0.5).abs() < 1e-6, "{u} {v}");
        let (u, _) = equirect_uv([1.0, 0.0, 0.0], 0.0);
        assert!((u - 0.75).abs() < 1e-6, "{u}");
        let (_, v) = equirect_uv([0.0, 1.0, 0.0], 0.0);
        assert!(v.abs() < 1e-6);
        // A quarter turn brings the map's forward (−Z) to −X.
        let (u, _) = equirect_uv([-1.0, 0.0, 0.0], PI / 2.0);
        assert!((u - 0.5).abs() < 1e-5, "{u}");
        for (u, v, r) in [(0.1f32, 0.3f32, 0.0f32), (0.9, 0.7, 1.2), (0.5, 0.05, -2.0)] {
            let (u2, v2) = equirect_uv(equirect_dir(u, v, r), r);
            assert!(
                (u - u2).abs() < 1e-5 && (v - v2).abs() < 1e-5,
                "{u},{v} → {u2},{v2}"
            );
        }
    }

    #[test]
    fn texel_solid_angles_cover_the_sphere() {
        let m = EnvMap::constant(64, 32, [1.0; 3]);
        let total: f32 = (0..32).map(|j| m.texel_solid_angle(j) * 64.0).sum();
        assert!((total - 4.0 * PI).abs() < 1e-4, "{total}");
        assert!((m.mean_luminance() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_constant_map_projects_like_the_uniform_sky() {
        let sky = EnvMap::constant(32, 16, [2.0, 1.0, 0.5])
            .into_sky(0.7, 1.5)
            .expect("sky");
        let sh = sky_sh(&sky, 64);
        let uni = sky_sh(&PtSky::Uniform([3.0, 1.5, 0.75]), 64);
        for ((row, u), c) in sh.iter().zip(&uni).zip([3.0f32, 1.5, 0.75]) {
            assert!((row[0] - u[0]).abs() < 1e-3 * u[0]);
            let want = 4.0 * PI * SH_Y00 * c;
            assert!((row[0] - want).abs() < 1e-3 * want);
            assert!(row[1..].iter().all(|v| v.abs() < 1e-3));
        }
        let e = sky_irradiance(&sky, [0.3, 0.9, 0.1], 128);
        assert!((e[0] - PI * 3.0).abs() < 2e-3 * PI * 3.0, "{e:?}");
    }

    #[test]
    fn an_upper_hemisphere_projects_to_its_analytic_moments() {
        let map = EnvMap::from_fn(128, 64, |d| if d[1] > 0.0 { [1.0; 3] } else { [0.0; 3] });
        let sky = map.into_sky(0.0, 1.0).expect("sky");
        let sh = sky_sh(&sky, 256);
        // ∫ Y00 over the upper hemisphere = 2π·Y00; ∫ Y1·y = Y1·π.
        assert!(
            (sh[0][0] - 2.0 * PI * SH_Y00).abs() < 0.01 * 2.0 * PI * SH_Y00,
            "{:?}",
            sh[0]
        );
        assert!((sh[0][2] - PI * SH_Y1).abs() < 0.01 * PI * SH_Y1, "{:?}", sh[0]);
        assert!(sh[0][1].abs() < 1e-3 && sh[0][3].abs() < 1e-3);
        let up = sky_irradiance(&sky, [0.0, 1.0, 0.0], 256)[0];
        assert!((up - PI).abs() < 0.01 * PI, "{up}");
    }

    #[test]
    fn downsampling_keeps_the_light() {
        let mut m = EnvMap::from_fn(256, 128, |d| [d[1].max(0.0) * 3.0 + 0.1, 0.5, d[0].abs()]);
        let before = m.mean_luminance();
        assert_eq!(m.downsample_to(64), 4);
        assert_eq!((m.width, m.height), (64, 32));
        let after = m.mean_luminance();
        assert!((before - after).abs() < 1e-4 * before, "{before} → {after}");
        assert_eq!(m.downsample_to(64), 1);
    }

    #[test]
    fn downsampling_also_meets_the_texel_cap() {
        // 4096 x 4096 at max_width 2048 would be 2048 x 2048 (over the cap): the factor grows.
        let mut m = EnvMap {
            width: 4096,
            height: 4096,
            rgb: vec![[1.0; 3]; 4096 * 4096],
        };
        let k = m.downsample_to(2048);
        assert_eq!(k, 3);
        assert!(m.rgb.len() <= MAX_EQUIRECT_TEXELS);
        assert!(m.into_sky(0.0, 1.0).is_ok());
    }

    /// A map with a small hot disc: extraction finds its direction and size, and map + sun
    /// deliver what the map did to surfaces facing many ways.
    #[test]
    fn sun_extraction_conserves_the_light_and_finds_the_disc() {
        let sun_dir = Vec3::new(0.3, 0.8, -0.5).normalize();
        let r = 2.0f32.to_radians();
        let hot = 50_000.0f32;
        let mut map = EnvMap::from_fn(1024, 512, |d| {
            let d = Vec3::from_array(d);
            let sky = 0.5 + 0.5 * d.y.max(0.0);
            if d.dot(sun_dir) > r.cos() {
                [hot, hot * 0.9, hot * 0.8]
            } else {
                [sky * 0.6, sky * 0.8, sky]
            }
        });
        let before = map.clone().into_sky(0.0, 1.0).expect("sky");
        let sun = map.extract_sun(100.0, 0.05).expect("extract").expect("a sun");
        let got = Vec3::from_array(sun.toward);
        assert!(
            got.dot(sun_dir) > 0.25f32.to_radians().cos(),
            "{got} vs {sun_dir}"
        );
        assert!(
            (sun.angular_radius - r).abs() < 0.2 * r,
            "{} vs {r}",
            sun.angular_radius
        );
        let after = map.into_sky(0.0, 1.0).expect("sky");
        let pt = sun.to_sun(0.0, 1.0);
        for n in [
            [0.0f32, 1.0, 0.0],
            sun_dir.to_array(),
            [1.0, 0.2, 0.0],
            [0.0, 0.3, -1.0],
        ] {
            let nv = Vec3::from_array(n).normalize();
            let want = lum(sky_irradiance(&before, n, 512));
            let got = lum(sky_irradiance(&after, n, 512))
                + pt.illuminance * nv.dot(Vec3::from_array(pt.toward)).max(0.0);
            assert!((got - want).abs() < 0.01 * want, "n {n:?}: {got} vs {want}");
        }
        // A second pass finds nothing above the cut.
        let mut again = EnvMap::from_fn(64, 32, |_| [1.0; 3]);
        assert!(again.extract_sun(100.0, 0.05).expect("ok").is_none());
    }

    #[test]
    fn a_broad_bright_region_is_not_taken_for_a_sun() {
        let mut map = EnvMap::from_fn(64, 32, |d| if d[1] > 0.5 { [1000.0; 3] } else { [0.01; 3] });
        let err = map.extract_sun(2.0, 0.05).expect_err("too broad");
        assert!(err.contains("not a sun disc"), "{err}");
    }

    fn row_map(row: usize, hot: f32) -> EnvMap {
        let (w, h) = (2048u32, 1024u32);
        let mut map = EnvMap::constant(w, h, [1.0; 3]);
        let w = w as usize;
        for px in &mut map.rgb[row * w..(row + 1) * w] {
            *px = [hot; 3];
        }
        map
    }

    fn assert_band_refused(row: usize) {
        let mut map = row_map(row, 100_000.0);
        let before = map.rgb.clone();
        match map.extract_sun(100.0, 0.03) {
            Err(err) => {
                assert!(
                    err.contains("not a sun disc"),
                    "row {row} should be refused as not a sun disc: {err}"
                );
                assert!(
                    map.rgb == before,
                    "row {row}: extraction wrote the fill before refusing the region"
                );
            }
            Ok(sun) => panic!("row {row}: a full-width latitude band was taken for a sun: {sun:?}"),
        }
    }

    /// A full-width equatorial row of a 2048×1024 map is under the 0.03 sr cap, but its
    /// direction vectors cancel. Extraction must refuse it and leave the map unchanged.
    /// A compact disc on a coarser map must still come back along that disc.
    #[test]
    fn correctness_g_latitude_band_is_not_a_sun_disc() {
        let sun_dir = Vec3::new(0.3, 0.8, -0.5).normalize();
        let radius = 3.0f32.to_radians();
        let mut disc = EnvMap::from_fn(256, 128, |d| {
            let d = Vec3::from_array(d);
            if d.dot(sun_dir) > radius.cos() {
                [50_000.0; 3]
            } else {
                [1.0; 3]
            }
        });
        let sun = disc.extract_sun(100.0, 0.03).expect("extract").expect("a sun");
        let got = Vec3::from_array(sun.toward);
        assert!(
            got.dot(sun_dir) > 1.0f32.to_radians().cos(),
            "compact disc: {got} vs {sun_dir}"
        );

        // Row 0 is a real polar cap (every texel faces +Y). It must still extract.
        let mut polar = row_map(0, 100_000.0);
        let polar_sun = polar
            .extract_sun(100.0, 0.03)
            .expect("extract")
            .expect("a polar cap");
        let up = Vec3::from_array(polar_sun.toward);
        assert!(up.dot(Vec3::Y) > 0.5f32.to_radians().cos(), "polar cap: {up}");

        // j = 511 is the row just above the equator; j = 512 is the row just below.
        assert_band_refused(511);
        assert_band_refused(512);
    }

    #[test]
    fn oversized_or_bad_maps_are_refused() {
        let big = EnvMap::constant(4096, 1024, [1.0; 3]);
        assert!(big.into_sky(0.0, 1.0).is_err());
        let mut nan = EnvMap::constant(4, 2, [1.0; 3]);
        nan.rgb[3] = [f32::NAN, 0.0, 0.0];
        assert!(nan.validate().is_err());
        assert!(EnvMap::load(Path::new("sky.png")).is_err());
    }

    #[test]
    fn absurd_generated_sizes_give_an_empty_map_and_mismatched_maps_do_not_index() {
        let m = EnvMap::constant(65536, 65536, [1.0; 3]);
        assert!(m.rgb.is_empty() && m.validate().is_err());
        let mut bad = EnvMap::constant(8, 4, [1.0; 3]);
        bad.rgb.truncate(3);
        assert_eq!(bad.mean_luminance(), 0.0);
        assert_eq!(bad.downsample_to(2), 1);
    }
}
