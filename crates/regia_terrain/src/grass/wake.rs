// SPDX-License-Identifier: MPL-2.0
//! A passing body's air wake bending the grass (ADR-1502).
//!
//! The host hands [`TerrainGrassHandle::set_wake`](super::TerrainGrassHandle::set_wake) a
//! [`GrassWake`] each frame: the closed-form wake of `regia_core::math::wake` packed for the
//! shader ([`WakeShape::gpu_words`]) and the grass's response to it. The blade shader
//! (`grass_wake_gust` / `grass_wake_tip` in `grass.wgsl`) evaluates the same formula at each
//! blade root, and [`GrassWake::tip_offset`] is its CPU twin, which tests compare with
//! [`WakeShape::sample`]. The velocity pass reads the body's previous state (rows 2 and 3), so
//! TAA and motion blur see the flick move.

use regia_core::glam::Vec3;
use regia_core::math::wake::{WAKE_GPU_ROWS, WakeBody, WakeShape, gust_from_words};
use serde::{Deserialize, Serialize};

/// `vec4` rows of the wake in `GrassParams` (the wake's rows plus the grass response).
pub const GRASS_WAKE_ROWS: usize = WAKE_GPU_ROWS + 1;

/// The wake the grass bends to this frame.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrassWake {
    /// [`WakeShape::gpu_words`] of the body now and the frame before.
    pub words: [[f32; 4]; WAKE_GPU_ROWS],
    /// Largest tip displacement as a fraction of the blade height (`0..=1`), approached as the
    /// gust grows well past [`Self::half_bend_m_s`].
    pub bend: f32,
    /// Gust at which the tip has moved half of [`Self::bend`], m/s.
    pub half_bend_m_s: f32,
}

impl GrassWake {
    /// Default [`Self::bend`].
    pub const DEFAULT_BEND: f32 = 0.6;
    /// Default [`Self::half_bend_m_s`].
    pub const DEFAULT_HALF_BEND_M_S: f32 = 4.0;

    /// The wake of a body that was at `previous` the frame before and is at `current` now,
    /// with the default grass response.
    #[must_use]
    pub fn new(shape: &WakeShape, current: &WakeBody, previous: &WakeBody) -> Self {
        Self {
            words: shape.gpu_words(current, previous),
            bend: Self::DEFAULT_BEND,
            half_bend_m_s: Self::DEFAULT_HALF_BEND_M_S,
        }
    }

    /// Whether the wake can bend anything (the shader skips it otherwise).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.words[5][2] > 0.0 && self.bend > 0.0
    }

    /// The rows as uploaded: the wake's rows and `(bend, half_bend_m_s, 0, 0)`.
    #[must_use]
    pub fn gpu_rows(&self) -> [[f32; 4]; GRASS_WAKE_ROWS] {
        let mut rows = [[0.0; 4]; GRASS_WAKE_ROWS];
        rows[..WAKE_GPU_ROWS].copy_from_slice(&self.words);
        let bend = if self.bend.is_finite() {
            self.bend.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let half = if self.half_bend_m_s.is_finite() {
            self.half_bend_m_s.max(1.0e-3)
        } else {
            Self::DEFAULT_HALF_BEND_M_S
        };
        rows[WAKE_GPU_ROWS] = [bend, half, 0.0, 0.0];
        rows
    }

    /// CPU twin of `grass_wake_tip`: the displacement of a blade point `s` of the way from the
    /// root (0) to the tip (1) of a blade `height` metres tall rooted at `root`. `previous`
    /// reads the body's state the frame before.
    #[must_use]
    pub fn tip_offset(&self, root: Vec3, height: f32, s: f32, previous: bool) -> Vec3 {
        let rows = self.gpu_rows();
        if rows[5][2] <= 0.0 {
            return Vec3::ZERO;
        }
        let gust = gust_from_words(&self.words, if previous { 2 } else { 0 }, root);
        let horizontal = regia_core::glam::Vec2::new(gust.x, gust.z);
        let m = horizontal.length();
        if m <= 1.0e-4 {
            return Vec3::ZERO;
        }
        let [bend, half, _, _] = rows[WAKE_GPU_ROWS];
        let response = bend * m / (m + half);
        let lean = horizontal / m * (response * height * s * s);
        Vec3::new(lean.x, -0.5 * response * response * height * s * s, lean.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(speed: f32) -> (WakeShape, WakeBody) {
        (
            WakeShape::default(),
            WakeBody {
                position: Vec3::new(0.0, 5.0, 0.0),
                velocity: Vec3::new(speed, 0.0, 0.0),
            },
        )
    }

    #[test]
    fn rows_pack_the_response_after_the_wake() {
        let (shape, body) = pass(60.0);
        let wake = GrassWake::new(&shape, &body, &body);
        let rows = wake.gpu_rows();
        assert_eq!(&rows[..WAKE_GPU_ROWS], &wake.words[..]);
        assert_eq!(rows[WAKE_GPU_ROWS][0], GrassWake::DEFAULT_BEND);
        assert!(wake.is_active());
    }

    #[test]
    fn blades_behind_the_body_lean_with_the_gust_and_roots_stay() {
        let (shape, body) = pass(60.0);
        let wake = GrassWake::new(&shape, &body, &body);
        // 3 m behind the body, 2 m to the side: just passed.
        let root = Vec3::new(-3.0, 0.0, 2.0);
        let tip = wake.tip_offset(root, 0.6, 1.0, false);
        let gust = shape.sample(&body, root).gust;
        assert!(gust.length() > 0.0);
        assert!(tip.x * gust.x > 0.0 && tip.z * gust.z >= 0.0, "{tip} vs {gust}");
        assert!(tip.y <= 0.0);
        assert_eq!(wake.tip_offset(root, 0.6, 0.0, false), Vec3::ZERO);
        // Ahead of the body: still.
        assert_eq!(
            wake.tip_offset(Vec3::new(10.0, 0.0, 2.0), 0.6, 1.0, false),
            Vec3::ZERO
        );
    }

    #[test]
    fn previous_rows_move_the_flick_between_frames() {
        let (shape, now) = pass(60.0);
        let before = WakeBody {
            position: now.position - now.velocity / 60.0,
            ..now
        };
        let wake = GrassWake::new(&shape, &now, &before);
        // Right behind the body now; the frame before, the body had not reached it.
        let root = Vec3::new(-0.5, 0.0, 1.0);
        assert!(wake.tip_offset(root, 0.6, 1.0, false).length() > 0.0);
        assert_eq!(wake.tip_offset(root, 0.6, 1.0, true), Vec3::ZERO);
    }

    #[test]
    fn a_still_body_bends_nothing() {
        let (shape, body) = pass(0.0);
        let wake = GrassWake::new(&shape, &body, &body);
        assert!(!wake.is_active());
        assert_eq!(
            wake.tip_offset(Vec3::new(-1.0, 0.0, 1.0), 0.6, 1.0, false),
            Vec3::ZERO
        );
    }

    #[test]
    fn the_shader_mirrors_the_cpu_twin() {
        let src = include_str!("../gpu/grass.wgsl");
        for needle in [
            "fn grass_wake_gust(",
            "fn grass_wake_tip(",
            "grass_params.wake[5].z > 0.0",
            "(1.0 - x) * exp(-x) * (1.0 - smoothstep(4.0, 6.0, x))",
        ] {
            assert!(src.contains(needle), "grass.wgsl lost `{needle}`");
        }
        let common = include_str!("../gpu/grass_common.wgsl");
        assert!(common.contains(&format!("wake: array<vec4<f32>, {GRASS_WAKE_ROWS}>")));
    }
}
