// SPDX-License-Identifier: MPL-2.0
//! The options of `RegiaViewer.create`, and the drawing size they imply.

use serde::Deserialize;

use crate::WebError;

/// Which browser graphics API to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendChoice {
    /// WebGPU where the browser has it, WebGL2 otherwise.
    #[default]
    Auto,
    /// WebGPU or refuse.
    Webgpu,
    /// WebGL2 or refuse.
    Webgl2,
}

impl BackendChoice {
    /// Whether a failed WebGPU attempt may fall back to WebGL2 on the same canvas: only for `auto`, and
    /// only when WebGPU failed before it took the canvas (a canvas keeps the first context kind it hands
    /// out, so after that WebGL2 gets no context and would report a misleading "no backend").
    pub fn falls_back_to_webgl2(self, webgpu_took_canvas: bool) -> bool {
        self == BackendChoice::Auto && !webgpu_took_canvas
    }

    /// The JS name.
    pub fn name(self) -> &'static str {
        match self {
            BackendChoice::Auto => "auto",
            BackendChoice::Webgpu => "webgpu",
            BackendChoice::Webgl2 => "webgl2",
        }
    }
}

fn default_ratio() -> f32 {
    2.0
}

fn default_msaa() -> u32 {
    4
}

/// The second argument of `RegiaViewer.create`. Unknown fields are refused, so a typo does not pass as a
/// default.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PlayerOptions {
    /// URL of the shared studio package folder; the built-in studio when absent.
    #[serde(default)]
    pub studio: Option<String>,
    /// Which graphics API.
    #[serde(default)]
    pub backend: BackendChoice,
    /// Cap on `devicePixelRatio` (cost control).
    #[serde(default = "default_ratio")]
    pub max_pixel_ratio: f32,
    /// 1 or 4.
    #[serde(default = "default_msaa")]
    pub msaa: u32,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        PlayerOptions {
            studio: None,
            backend: BackendChoice::Auto,
            max_pixel_ratio: default_ratio(),
            msaa: default_msaa(),
        }
    }
}

impl PlayerOptions {
    /// Reads the options from JSON text and checks the values.
    pub fn parse(json: &str) -> Result<Self, WebError> {
        let value =
            serde_json::from_str(json).map_err(|e| WebError::new("bad_argument", format!("options: {e}")))?;
        Self::from_value(value)
    }

    /// Reads the options from a JSON value (null means all defaults) and checks them.
    pub fn from_value(value: serde_json::Value) -> Result<Self, WebError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        if !value.is_object() {
            return Err(WebError::new("bad_argument", "options must be an object"));
        }
        let o: PlayerOptions = serde_json::from_value(value)
            .map_err(|e| WebError::new("bad_argument", format!("options: {e}")))?;
        o.checked()
    }

    fn checked(self) -> Result<Self, WebError> {
        if !self.max_pixel_ratio.is_finite() || self.max_pixel_ratio <= 0.0 {
            return Err(WebError::new("bad_argument", "maxPixelRatio must be above 0"));
        }
        if self.msaa != 1 && self.msaa != 4 {
            return Err(WebError::new("bad_argument", "msaa must be 1 or 4"));
        }
        Ok(self)
    }
}

/// The largest drawing size either way, in device pixels: WebGL2's guaranteed minimum for a texture, so a
/// huge canvas degrades instead of failing.
pub const MAX_DRAWING_SIDE: u32 = 2048;

/// The drawing size for a canvas of `css` pixels: `css * min(dpr, max_ratio)`, at least 1, scaled down
/// keeping the shape when it exceeds [`MAX_DRAWING_SIDE`].
pub fn drawing_size(css: [f32; 2], dpr: f32, max_ratio: f32) -> [u32; 2] {
    let ratio = if dpr.is_finite() && dpr > 0.0 {
        dpr.min(max_ratio)
    } else {
        1.0
    };
    let w = sane(css[0]) * ratio;
    let h = sane(css[1]) * ratio;
    let longest = w.max(h);
    let fit = if longest > MAX_DRAWING_SIDE as f32 {
        MAX_DRAWING_SIDE as f32 / longest
    } else {
        1.0
    };
    [
        ((w * fit).round() as u32).max(1),
        ((h * fit).round() as u32).max(1),
    ]
}

fn sane(x: f32) -> f32 {
    if x.is_finite() && x > 0.0 { x } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit fix (sprint-fix-audit-jw, finding 6b): once WebGPU holds the canvas, auto does not try
    /// WebGL2 on it (that answered "no backend" for a reason that was WebGPU's).
    #[test]
    fn webgl2_is_tried_only_on_a_canvas_webgpu_never_took() {
        assert!(BackendChoice::Auto.falls_back_to_webgl2(false));
        assert!(!BackendChoice::Auto.falls_back_to_webgl2(true));
        assert!(!BackendChoice::Webgpu.falls_back_to_webgl2(false));
        assert!(!BackendChoice::Webgl2.falls_back_to_webgl2(false));
    }

    #[test]
    fn defaults_are_the_spec_values() {
        let o = PlayerOptions::parse("{}").expect("empty is fine");
        assert_eq!(o.backend, BackendChoice::Auto);
        assert_eq!(o.max_pixel_ratio, 2.0);
        assert_eq!(o.msaa, 4);
        assert_eq!(o.studio, None);
    }

    #[test]
    fn reads_the_js_names() {
        let o = PlayerOptions::parse(r#"{"studio":"s/","backend":"webgl2","maxPixelRatio":1.5,"msaa":1}"#)
            .expect("valid");
        assert_eq!(o.backend, BackendChoice::Webgl2);
        assert_eq!(o.max_pixel_ratio, 1.5);
        assert_eq!(o.msaa, 1);
        assert_eq!(o.studio.as_deref(), Some("s/"));
    }

    #[test]
    fn refuses_typos_and_bad_values() {
        for bad in [
            r#"{"maxPixelratio":1}"#,
            r#"{"msaa":2}"#,
            r#"{"maxPixelRatio":0}"#,
            r#"{"backend":"gl"}"#,
            "[]",
            "nonsense",
        ] {
            let e = PlayerOptions::parse(bad).expect_err(bad);
            assert_eq!(e.code, "bad_argument", "{bad}");
        }
    }

    #[test]
    fn the_ratio_is_capped_and_sizes_round() {
        assert_eq!(drawing_size([400.0, 300.0], 3.0, 2.0), [800, 600]);
        assert_eq!(drawing_size([400.0, 300.0], 1.0, 2.0), [400, 300]);
        assert_eq!(drawing_size([100.5, 50.2], 1.0, 2.0), [101, 50]);
    }

    #[test]
    fn empty_and_nonsense_stay_drawable() {
        assert_eq!(drawing_size([0.0, 0.0], 2.0, 2.0), [1, 1]);
        assert_eq!(drawing_size([f32::NAN, -5.0], f32::NAN, 2.0), [1, 1]);
    }

    #[test]
    fn a_huge_canvas_is_scaled_down_keeping_its_shape() {
        let [w, h] = drawing_size([4000.0, 2000.0], 2.0, 2.0);
        assert_eq!(w, MAX_DRAWING_SIDE);
        assert_eq!(h, MAX_DRAWING_SIDE / 2);
    }
}
