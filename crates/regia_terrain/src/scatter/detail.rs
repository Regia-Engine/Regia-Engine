// SPDX-License-Identifier: MPL-2.0
//! Opt-in detail limits that make dense rock scatter cheap ([`ScatterDetail`], lane
//! scatter-density).
//!
//! A ground densely strewn with small stones (DO27's Mars after the NASA photo comparison) is
//! mostly pebbles a few centimetres across. Past a few tens of metres each covers less than a
//! pixel: drawing it costs a triangle setup per sub-pixel triangle, an instance in every cull,
//! and a shadow caster in every cascade page, and adds nothing to the image the terrain's own
//! texture does not already show. The limits here end each instance by its own projected size
//! instead of only by its class's distance fade:
//!
//! - **Screen-size fade**: an instance shrinks into the ground (the same pivot scale as the
//!   distance fade) as its projected radius falls from [`ScatterDetail::fade_ratio`] times
//!   [`ScatterDetail::min_radius_px`] to the limit. The scale follows an ease-out quartic, so
//!   almost all of the shrink happens once the instance is already small on screen; a symmetric
//!   smoothstep dropped the large pixels in a few frames and a tile popped. A pebble ends at a
//!   few tens of metres, a boulder of the same class keeps its range. Tiles and sub-blocks whose
//!   largest instance is already below the limit are neither streamed nor culled per instance.
//! - **Small casters**: instances smaller than [`ScatterDetail::shadow_min_radius_m`] cast no
//!   shadow map shadow; the contact AO pool under every rock and the screen-space contact
//!   shadows ground them.
//!
//! The projected size is measured at a reference output height and field of view (the
//! game's, like [`super::impostor::ImpostorSettings`]), so streaming and the fade agree and do
//! not change with the view's zoom. Off (`None`, the default) the frame is unchanged.

/// Detail limits for a [`super::field::ScatterField`] ([`super::field::ScatterField::set_detail`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScatterDetail {
    /// Projected radius (pixels at the reference resolution) where an instance has shrunk out;
    /// the fade starts at [`Self::fade_ratio`] times this. 0 turns the screen-size fade off.
    pub min_radius_px: f32,
    /// Where the fade starts, as a multiple of [`Self::min_radius_px`] (> 1; 2 by default).
    /// Close to 1 the fade is a cut (tests compare against it).
    pub fade_ratio: f32,
    /// Instances with a bounding radius below this (metres, after scale) cast no shadow-map
    /// shadow. 0 keeps every class's own shadow policy.
    pub shadow_min_radius_m: f32,
    /// Reference output height, pixels.
    pub reference_height_px: f32,
    /// Reference vertical field of view, radians.
    pub reference_fov_y: f32,
}

impl Default for ScatterDetail {
    /// The lane's recommendation for a dense ground (measured on DO27's Switchbacks at 10×):
    /// shrink out below 1 px radius (2 px wide) at 1440p, no shadow-map shadows from stones
    /// under 12 cm radius.
    fn default() -> Self {
        Self {
            min_radius_px: 1.0,
            fade_ratio: 2.0,
            shadow_min_radius_m: 0.12,
            reference_height_px: 1440.0,
            // 90° horizontal at 16:9.
            reference_fov_y: 2.0 * (1.0f32 / (16.0 / 9.0)).atan(),
        }
    }
}

impl ScatterDetail {
    /// Finite, non-negative values; a degenerate reference falls back to the default's.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let pos = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
        Self {
            min_radius_px: pos(self.min_radius_px),
            fade_ratio: if self.fade_ratio.is_finite() && self.fade_ratio > 1.0 {
                self.fade_ratio
            } else {
                d.fade_ratio
            },
            shadow_min_radius_m: pos(self.shadow_min_radius_m),
            reference_height_px: if self.reference_height_px.is_finite() && self.reference_height_px >= 1.0 {
                self.reference_height_px
            } else {
                d.reference_height_px
            },
            reference_fov_y: if self.reference_fov_y.is_finite()
                && self.reference_fov_y > 1e-3
                && self.reference_fov_y < 3.1
            {
                self.reference_fov_y
            } else {
                d.reference_fov_y
            },
        }
    }

    /// Pixels per (metre of radius / metre of distance) at the reference: the projected radius
    /// of an instance of radius `r` at distance `d` is `r * pixel_scale() / d`.
    #[must_use]
    pub fn pixel_scale(&self) -> f32 {
        self.reference_height_px * 0.5 / (self.reference_fov_y * 0.5).tan().max(1e-4)
    }

    /// Distance beyond which an instance of radius `radius` has shrunk out (infinite with the
    /// screen-size fade off).
    #[must_use]
    pub fn end_distance(&self, radius: f32) -> f32 {
        if self.min_radius_px <= 0.0 {
            f32::INFINITY
        } else {
            radius.max(0.0) * self.pixel_scale() / self.min_radius_px
        }
    }

    /// The screen-size fade of an instance of radius `radius` at distance `d`: 1 while its
    /// projected radius is at least [`Self::fade_ratio`] × [`Self::min_radius_px`], 0 at the
    /// limit, and an ease-out quartic between.
    ///
    /// `t` is 1 at the start of the band and 0 at the limit. `1 - (1 - t)⁴` has derivative 0
    /// at the start, so a frame removes few pixels while the instance is still large, and
    /// spends the steeper part of the curve once the instance covers little of a tile. A
    /// symmetric smoothstep (derivative 0 at both ends, steepest in the middle) popped tiles
    /// on a slow dolly: the middle of the band is where the instance is still ~1.5× the limit
    /// across. Outside the band the value is exactly 0 or 1, so a held frame of an instance
    /// that is not fading is unchanged.
    #[must_use]
    pub fn fade(&self, radius: f32, d: f32) -> f32 {
        if self.min_radius_px <= 0.0 {
            return 1.0;
        }
        let px = radius * self.pixel_scale() / d.max(1e-3);
        let band = self.min_radius_px * (self.fade_ratio - 1.0).max(1e-4);
        let t = ((px - self.min_radius_px) / band).clamp(0.0, 1.0);
        let u = 1.0 - t;
        1.0 - u * u * u * u
    }

    /// An instance of radius `radius` may cast a shadow-map shadow.
    #[must_use]
    pub fn casts(&self, radius: f32) -> bool {
        radius >= self.shadow_min_radius_m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_follows_projected_size_and_ends_where_it_says() {
        let d = ScatterDetail::default();
        // A 5 cm pebble and a 1 m boulder.
        let (pebble, boulder) = (0.05, 1.0);
        let end = d.end_distance(pebble);
        // 1440p reference, 90° horizontal at 16:9: pixel_scale 1280, so 0.05 * 1280 / 1 px.
        assert!((end - 64.0).abs() < 0.05, "pebble ends at {end} m");
        assert_eq!(d.fade(pebble, end * 0.4), 1.0);
        assert!(d.fade(pebble, end * 0.75) > 0.0 && d.fade(pebble, end * 0.75) < 1.0);
        assert_eq!(d.fade(pebble, end), 0.0);
        assert_eq!(d.fade(boulder, end), 1.0);
        // Ease-out: the same distance step drops the scale much less while the pebble is
        // still large (just inside the band) than while it is about to disappear.
        let start = end / d.fade_ratio;
        let step = (end - start) * 0.05;
        let drop_large = 1.0 - d.fade(pebble, start + step);
        let drop_small = d.fade(pebble, end - step);
        assert!(
            drop_large < drop_small * 0.05,
            "large drop {drop_large}, small drop {drop_small}"
        );
        // Monotone with distance: no pop.
        let mut last = 1.0;
        for i in 1..200 {
            let f = d.fade(pebble, i as f32 * 0.5);
            assert!(f <= last + 1e-6);
            last = f;
        }
        // Off: nothing changes.
        let off = ScatterDetail {
            min_radius_px: 0.0,
            ..d
        };
        assert_eq!(off.fade(pebble, 1.0e4), 1.0);
        assert!(off.end_distance(pebble).is_infinite());
        assert!(!d.casts(0.05) && d.casts(0.5));
    }

    #[test]
    fn sanitize_rejects_degenerate_references() {
        let s = ScatterDetail {
            min_radius_px: f32::NAN,
            fade_ratio: 0.5,
            shadow_min_radius_m: -1.0,
            reference_height_px: 0.0,
            reference_fov_y: 7.0,
        }
        .sanitized();
        assert_eq!(s.min_radius_px, 0.0);
        assert_eq!(s.shadow_min_radius_m, 0.0);
        assert_eq!(s.fade_ratio, 2.0);
        assert_eq!(
            s.reference_height_px,
            ScatterDetail::default().reference_height_px
        );
        assert_eq!(s.reference_fov_y, ScatterDetail::default().reference_fov_y);
    }
}
