// SPDX-License-Identifier: MPL-2.0
//! Presentation math (specs/50 § 2.7): fullscreen fit rectangles and the `VideoFullscreen` component.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// How a picture fills a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Reflect)]
pub enum Fit {
    /// Whole picture visible, bars on two sides.
    #[default]
    Letterbox,
    /// Surface covered, picture cropped.
    Fill,
    /// Picture stretched to the surface.
    Stretch,
}

/// A rectangle in surface pixels (may extend past the surface for `Fill`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FitRect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

/// The rectangle a picture of display aspect `aspect` (width / height) occupies on a `sw` x `sh`
/// surface, centered.
pub fn fit_rect(aspect: f32, sw: f32, sh: f32, fit: Fit) -> FitRect {
    if !(aspect.is_finite() && aspect > 0.0 && sw > 0.0 && sh > 0.0) || fit == Fit::Stretch {
        return FitRect {
            x: 0.0,
            y: 0.0,
            w: sw.max(0.0),
            h: sh.max(0.0),
        };
    }
    let surface = sw / sh;
    let wider = aspect > surface;
    let (w, h) = match (fit, wider) {
        (Fit::Letterbox, true) | (Fit::Fill, false) => (sw, sw / aspect),
        _ => (sh * aspect, sh),
    };
    FitRect {
        x: (sw - w) * 0.5,
        y: (sh - h) * 0.5,
        w,
        h,
    }
}

/// Draws a player's picture fullscreen after the 3D scene (below the UI when `layer < 0`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Reflect, Serialize, Deserialize)]
pub struct VideoFullscreen {
    /// Fit mode.
    pub fit: Fit,
    /// Order among fullscreen videos; negative draws under the UI.
    pub layer: i32,
    /// The `skip_video` action closes the player.
    pub skippable: bool,
}

impl Default for VideoFullscreen {
    fn default() -> Self {
        Self {
            fit: Fit::Letterbox,
            layer: 0,
            skippable: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: FitRect, b: [f32; 4]) -> bool {
        (a.x - b[0]).abs() < 1e-3
            && (a.y - b[1]).abs() < 1e-3
            && (a.w - b[2]).abs() < 1e-3
            && (a.h - b[3]).abs() < 1e-3
    }

    /// specs/50 § 8 test 12: 16:9 into 4:3 and 21:9 surfaces.
    #[test]
    fn letterbox_fill_stretch() {
        let a = 16.0 / 9.0;
        assert!(close(
            fit_rect(a, 1024.0, 768.0, Fit::Letterbox),
            [0.0, 96.0, 1024.0, 576.0]
        ));
        assert!(close(
            fit_rect(a, 1024.0, 768.0, Fit::Fill),
            [-170.6667, 0.0, 1365.3334, 768.0]
        ));
        assert!(close(
            fit_rect(a, 1024.0, 768.0, Fit::Stretch),
            [0.0, 0.0, 1024.0, 768.0]
        ));
        assert!(close(
            fit_rect(a, 2520.0, 1080.0, Fit::Letterbox),
            [300.0, 0.0, 1920.0, 1080.0]
        ));
        assert!(close(
            fit_rect(a, 2520.0, 1080.0, Fit::Fill),
            [0.0, -168.75, 2520.0, 1417.5]
        ));
    }

    #[test]
    fn degenerate_inputs_stretch() {
        assert!(close(
            fit_rect(0.0, 10.0, 10.0, Fit::Letterbox),
            [0.0, 0.0, 10.0, 10.0]
        ));
        assert!(close(
            fit_rect(f32::NAN, 10.0, 5.0, Fit::Fill),
            [0.0, 0.0, 10.0, 5.0]
        ));
    }
}
