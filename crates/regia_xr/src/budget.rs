// SPDX-License-Identifier: MPL-2.0
//! The frame budget (spec 58 § 10): a frame-time overrun beyond the budget for
//! [`OVER_BUDGET_FRAMES`] consecutive XR frames raises the diagnostic `xr_over_budget`
//! ([`XrOverBudget`], `stats.over_budget`) and, when `auto_scale` is on, lowers the render scale one
//! [`AUTO_SCALE_STEP`] (never below the 0.5 floor of `render_scale`).
//!
//! The budget is [`BUDGET_SHARE`] of the display period (10 ms at 90 Hz, 12.5 ms at 72 Hz): the
//! runtime keeps part of the frame for its own reprojection (spec 58 § 10: "about 9-10 ms at 90 Hz").
//! After a raise the run starts over, so the next raise (and the next step) needs another
//! [`OVER_BUDGET_FRAMES`] overruns measured at the new scale. The scale is never raised again by
//! this controller; a game or an agent raises it with `xr.set_option render_scale`.
//!
//! Frame times: the simulator never reads the wall clock here (spec 58 § 7: simulator runs are
//! bit-exact); its frame time is what [`XrFrameBudget::script_frame_ms`] (`xr.drive frame_time`)
//! set, and with none set its frames are not measured. Any other backend measures the CPU time of
//! the stereo frame the renderer drew (`XrRenderStats::last_cpu_ms`). `auto_scale` is a runtime
//! option (`xr.set_option auto_scale`), not a `config/xr.ron` field, so the file format is unchanged.

use regia_app::prelude::*;

use crate::config::RENDER_SCALE_RANGE;

/// Consecutive over-budget frames that raise `xr_over_budget` (N in spec 58 § 10).
pub const OVER_BUDGET_FRAMES: u32 = 10;

/// The render-scale step `auto_scale` takes per raise.
pub const AUTO_SCALE_STEP: f32 = 0.1;

/// The share of the display period an XR frame may use.
pub const BUDGET_SHARE: f32 = 0.9;

/// The diagnostic code.
pub const OVER_BUDGET_CODE: &str = "xr_over_budget";

/// The refresh rate assumed when none is known, Hz.
const FALLBACK_REFRESH_HZ: f32 = 90.0;

/// The frame budget in milliseconds at `refresh_hz`: [`BUDGET_SHARE`] of the display period
/// (the 90 Hz budget when the rate is unknown, zero or not finite).
#[must_use]
pub fn budget_ms_for(refresh_hz: f32) -> f32 {
    let hz = if refresh_hz.is_finite() && refresh_hz > 0.0 {
        refresh_hz
    } else {
        FALLBACK_REFRESH_HZ
    };
    BUDGET_SHARE * 1000.0 / hz
}

/// One [`AUTO_SCALE_STEP`] down from `scale`, on the hundredths grid and never below the
/// `render_scale` floor; `None` when `scale` is already at (or under) the floor.
#[must_use]
pub fn scale_down(scale: f32) -> Option<f32> {
    let floor = RENDER_SCALE_RANGE.0;
    if !scale.is_finite() || scale <= floor {
        return None;
    }
    let next = ((scale - AUTO_SCALE_STEP) * 100.0).round() / 100.0;
    Some(next.max(floor))
}

/// The `xr_over_budget` diagnostic: [`OVER_BUDGET_FRAMES`] consecutive frames over the budget.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct XrOverBudget {
    /// The frame time of the frame that completed the run, milliseconds.
    pub frame_ms: f32,
    /// The budget it was measured against, milliseconds.
    pub budget_ms: f32,
    /// Consecutive frames over the budget ([`OVER_BUDGET_FRAMES`]).
    pub frames: u32,
    /// The render scale before.
    pub scale_from: f32,
    /// The render scale after (equal to `scale_from` without `auto_scale` or at the floor).
    pub scale_to: f32,
}

/// The frame-budget watch: the `auto_scale` option, the consecutive-overrun count and the
/// simulator's scripted frame time.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct XrFrameBudget {
    /// Lower the render scale one step on each raise (`xr.set_option auto_scale`, default off).
    pub auto_scale: bool,
    /// Consecutive over-budget frames that raise the diagnostic.
    pub frames: u32,
    over_run: u32,
    scripted_ms: Option<f32>,
    last_frame_ms: Option<f32>,
    last: Option<XrOverBudget>,
}

impl Default for XrFrameBudget {
    fn default() -> Self {
        XrFrameBudget {
            auto_scale: false,
            frames: OVER_BUDGET_FRAMES,
            over_run: 0,
            scripted_ms: None,
            last_frame_ms: None,
            last: None,
        }
    }
}

impl XrFrameBudget {
    /// Sets the simulator's frame time for every following frame (`xr.drive frame_time`); `None`
    /// stops measuring simulator frames. A value that is not finite or is negative is ignored
    /// (the agent refuses it before it gets here).
    pub fn script_frame_ms(&mut self, ms: Option<f32>) {
        match ms {
            Some(v) if v.is_finite() && v >= 0.0 => self.scripted_ms = Some(v),
            Some(_) => {}
            None => self.scripted_ms = None,
        }
    }

    /// The scripted frame time, if any.
    #[must_use]
    pub fn scripted_ms(&self) -> Option<f32> {
        self.scripted_ms
    }

    /// The last frame time observed, milliseconds.
    #[must_use]
    pub fn last_frame_ms(&self) -> Option<f32> {
        self.last_frame_ms
    }

    /// Consecutive frames over the budget so far.
    #[must_use]
    pub fn over_run(&self) -> u32 {
        self.over_run
    }

    /// The last diagnostic raised.
    #[must_use]
    pub fn last(&self) -> Option<XrOverBudget> {
        self.last
    }

    /// Observes one XR frame of `frame_ms` against `budget_ms` at render scale `scale`. Returns the
    /// diagnostic when this frame completes a run of [`Self::frames`] overruns (the run then starts
    /// over); its `scale_to` is one step down when `auto_scale` is on. A frame time that is not
    /// finite is ignored.
    pub fn observe(&mut self, frame_ms: f32, budget_ms: f32, scale: f32) -> Option<XrOverBudget> {
        if !frame_ms.is_finite() {
            return None;
        }
        self.last_frame_ms = Some(frame_ms);
        if frame_ms <= budget_ms {
            self.over_run = 0;
            return None;
        }
        self.over_run += 1;
        if self.over_run < self.frames.max(1) {
            return None;
        }
        self.over_run = 0;
        let scale_to = if self.auto_scale {
            scale_down(scale).unwrap_or(scale)
        } else {
            scale
        };
        let event = XrOverBudget {
            frame_ms,
            budget_ms,
            frames: self.frames.max(1),
            scale_from: scale,
            scale_to,
        };
        self.last = Some(event);
        Some(event)
    }
}

/// The frame time to observe for one XR frame: the simulator's scripted time (never the wall
/// clock), or the measured stereo-frame CPU time on any other backend.
#[must_use]
pub fn frame_time_ms(simulator: bool, scripted: Option<f32>, measured: Option<f32>) -> Option<f32> {
    if simulator {
        scripted
    } else {
        scripted.or(measured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_simulator_uses_only_its_script_and_a_real_backend_its_measure() {
        assert_eq!(frame_time_ms(true, None, Some(30.0)), None);
        assert_eq!(frame_time_ms(true, Some(4.0), Some(30.0)), Some(4.0));
        assert_eq!(frame_time_ms(false, None, Some(30.0)), Some(30.0));
        assert_eq!(frame_time_ms(false, Some(4.0), Some(30.0)), Some(4.0));
        assert_eq!(frame_time_ms(false, None, None), None);
    }

    #[test]
    fn a_frame_exactly_on_budget_is_inside_it_and_nan_is_ignored() {
        let mut b = XrFrameBudget {
            frames: 2,
            ..XrFrameBudget::default()
        };
        assert_eq!(b.observe(10.0, 10.0, 1.0), None);
        assert_eq!(b.over_run(), 0);
        assert_eq!(b.observe(10.5, 10.0, 1.0), None);
        assert_eq!(b.observe(f32::NAN, 10.0, 1.0), None);
        assert_eq!(b.over_run(), 1, "a NaN frame neither counts nor resets");
        let e = b.observe(10.5, 10.0, 1.0).expect("second overrun raises");
        assert_eq!((e.frames, e.scale_from, e.scale_to), (2, 1.0, 1.0));
        assert_eq!(b.last(), Some(e));
    }

    #[test]
    fn a_bad_scripted_time_is_ignored() {
        let mut b = XrFrameBudget::default();
        b.script_frame_ms(Some(3.0));
        b.script_frame_ms(Some(-1.0));
        b.script_frame_ms(Some(f32::INFINITY));
        assert_eq!(b.scripted_ms(), Some(3.0));
        b.script_frame_ms(None);
        assert_eq!(b.scripted_ms(), None);
    }
}
