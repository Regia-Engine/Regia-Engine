// SPDX-License-Identifier: MPL-2.0
//! Easing functions: pure maps of `t ∈ [0, 1]` with `f(0) = 0` and `f(1) = 1`.
//!
//! The set and its formulas are Robert Penner's easing equations (published 2001 under the BSD licence).
//! `back` and `elastic` overshoot the `[0, 1]` range between the ends. The functions do not clamp `t`;
//! [`Easing::apply`] does.
//!
//! The sine, exponential and elastic arms evaluate on the deterministic `f64` kernel
//! ([`det_cos`], `det_sin`, `det_exp2`) and round once to `f32`, and the polynomial arms are
//! explicit multiply chains: a tween replayed with the same `t` stores the same reflected bits
//! on every target and in every build profile (audit-g-core-determinism § 2).

use std::f32::consts::{FRAC_PI_2, PI};

use serde::{Deserialize, Serialize};

use super::rng::{det_cos, det_exp2, det_sin};

/// `t`.
#[inline]
pub fn linear(t: f32) -> f32 {
    t
}

/// `t^n` as an explicit f32 multiply chain. `powi` is a runtime multiply chain but the compiler
/// folds a constant call in f64 and rounds once (see `tests/float_folding.rs`), so debug and
/// release builds of the same literal can differ; the chain is IEEE f32 either way.
#[inline]
fn pow_chain(t: f32, n: u32) -> f32 {
    match n {
        2 => t * t,
        3 => t * t * t,
        4 => {
            let u = t * t;
            u * u
        }
        5 => {
            let u = t * t;
            u * u * t
        }
        _ => t.powi(n as i32),
    }
}

/// Shared shape of the polynomial in/out/in-out families.
macro_rules! power_easing {
    ($in:ident, $out:ident, $in_out:ident, $n:expr, $name:literal) => {
        #[doc = concat!("Accelerating ", $name, ": `t^", stringify!($n), "`.")]
        #[inline]
        pub fn $in(t: f32) -> f32 {
            pow_chain(t, $n)
        }

        #[doc = concat!("Decelerating ", $name, ".")]
        #[inline]
        pub fn $out(t: f32) -> f32 {
            1.0 - pow_chain(1.0 - t, $n)
        }

        #[doc = concat!("Accelerating then decelerating ", $name, ".")]
        #[inline]
        pub fn $in_out(t: f32) -> f32 {
            if t < 0.5 {
                2f32.powi($n - 1) * pow_chain(t, $n)
            } else {
                1.0 - pow_chain(2.0 - 2.0 * t, $n) / 2.0
            }
        }
    };
}

power_easing!(quad_in, quad_out, quad_in_out, 2, "quadratic");
power_easing!(cubic_in, cubic_out, cubic_in_out, 3, "cubic");
power_easing!(quart_in, quart_out, quart_in_out, 4, "quartic");
power_easing!(quint_in, quint_out, quint_in_out, 5, "quintic");

/// Accelerating sine: `1 - cos(t·π/2)`, on the deterministic kernel.
#[inline]
pub fn sine_in(t: f32) -> f32 {
    1.0 - det_cos(t * FRAC_PI_2)
}

/// Decelerating sine: `sin(t·π/2)`, on the deterministic kernel.
#[inline]
pub fn sine_out(t: f32) -> f32 {
    det_sin(t * FRAC_PI_2)
}

/// Sine in-out: `(1 - cos(t·π)) / 2`, on the deterministic kernel.
#[inline]
pub fn sine_in_out(t: f32) -> f32 {
    (1.0 - det_cos(t * PI)) / 2.0
}

/// Accelerating exponential: `2^(10t - 10)`, exactly 0 at `t = 0`.
#[inline]
pub fn expo_in(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else {
        det_exp2(f64::from(10.0 * t - 10.0)) as f32
    }
}

/// Decelerating exponential: `1 - 2^(-10t)`, exactly 1 at `t = 1`.
#[inline]
pub fn expo_out(t: f32) -> f32 {
    if t >= 1.0 {
        1.0
    } else {
        1.0 - det_exp2(f64::from(-10.0 * t)) as f32
    }
}

/// Exponential in-out.
#[inline]
pub fn expo_in_out(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t >= 1.0 {
        1.0
    } else if t < 0.5 {
        det_exp2(f64::from(20.0 * t - 10.0)) as f32 / 2.0
    } else {
        (2.0 - det_exp2(f64::from(10.0 - 20.0 * t)) as f32) / 2.0
    }
}

/// Accelerating circular: `1 - sqrt(1 - t²)`.
#[inline]
pub fn circ_in(t: f32) -> f32 {
    1.0 - (1.0 - t * t).max(0.0).sqrt()
}

/// Decelerating circular: `sqrt(1 - (t - 1)²)`.
#[inline]
pub fn circ_out(t: f32) -> f32 {
    (1.0 - (t - 1.0) * (t - 1.0)).max(0.0).sqrt()
}

/// Circular in-out.
#[inline]
pub fn circ_in_out(t: f32) -> f32 {
    if t < 0.5 {
        circ_in(2.0 * t) / 2.0
    } else {
        (1.0 + circ_out(2.0 * t - 1.0)) / 2.0
    }
}

/// Overshoot amount of the `back` family (≈ 10 % overshoot).
const BACK: f32 = 1.701_58;

/// Accelerating with a small backwards wind-up.
#[inline]
pub fn back_in(t: f32) -> f32 {
    (BACK + 1.0) * t * t * t - BACK * t * t
}

/// Decelerating with a small overshoot.
#[inline]
pub fn back_out(t: f32) -> f32 {
    1.0 - back_in(1.0 - t)
}

/// Back in-out (the overshoot scaled by 1.525 so each half overshoots ≈ 10 %).
#[inline]
pub fn back_in_out(t: f32) -> f32 {
    let c = BACK * 1.525;
    let s = |u: f32| (c + 1.0) * u * u * u - c * u * u;
    if t < 0.5 {
        s(2.0 * t) / 2.0
    } else {
        1.0 - s(2.0 - 2.0 * t) / 2.0
    }
}

/// Accelerating elastic (a decaying sine wind-up), on the deterministic kernel.
#[inline]
pub fn elastic_in(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t >= 1.0 {
        1.0
    } else {
        -(det_exp2(f64::from(10.0 * t - 10.0)) as f32) * det_sin((10.0 * t - 10.75) * (2.0 * PI / 3.0))
    }
}

/// Decelerating elastic (a decaying sine overshoot).
#[inline]
pub fn elastic_out(t: f32) -> f32 {
    1.0 - elastic_in(1.0 - t)
}

/// Elastic in-out.
#[inline]
pub fn elastic_in_out(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t >= 1.0 {
        1.0
    } else {
        let wave = det_sin((20.0 * t - 11.125) * (2.0 * PI / 4.5));
        if t < 0.5 {
            -(det_exp2(f64::from(20.0 * t - 10.0)) as f32) * wave / 2.0
        } else {
            det_exp2(f64::from(10.0 - 20.0 * t)) as f32 * wave / 2.0 + 1.0
        }
    }
}

/// Decelerating bounce: four parabolic arcs of decreasing height.
#[inline]
pub fn bounce_out(t: f32) -> f32 {
    const N: f32 = 7.5625;
    const D: f32 = 2.75;
    if t < 1.0 / D {
        N * t * t
    } else if t < 2.0 / D {
        let u = t - 1.5 / D;
        N * u * u + 0.75
    } else if t < 2.5 / D {
        let u = t - 2.25 / D;
        N * u * u + 0.9375
    } else {
        let u = t - 2.625 / D;
        N * u * u + 0.984_375
    }
}

/// Accelerating bounce.
#[inline]
pub fn bounce_in(t: f32) -> f32 {
    1.0 - bounce_out(1.0 - t)
}

/// Bounce in-out.
#[inline]
pub fn bounce_in_out(t: f32) -> f32 {
    if t < 0.5 {
        (1.0 - bounce_out(1.0 - 2.0 * t)) / 2.0
    } else {
        (1.0 + bounce_out(2.0 * t - 1.0)) / 2.0
    }
}

macro_rules! easing_enum {
    ($($(#[$attr:meta])* $variant:ident => $f:ident),* $(,)?) => {
        /// A serialisable choice of easing function (for tweens and UI transitions authored in data).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
        pub enum Easing {
            $(
                #[doc = concat!("[`", stringify!($f), "`]")]
                $(#[$attr])*
                $variant,
            )*
        }

        impl Easing {
            /// Every easing, in declaration order.
            pub const ALL: &'static [Easing] = &[$(Easing::$variant),*];

            /// Evaluates the easing at `t`, clamped to `[0, 1]`.
            pub fn apply(self, t: f32) -> f32 {
                let t = t.clamp(0.0, 1.0);
                match self {
                    $(Easing::$variant => $f(t),)*
                }
            }
        }
    };
}

easing_enum! {
    #[default]
    Linear => linear,
    QuadIn => quad_in, QuadOut => quad_out, QuadInOut => quad_in_out,
    CubicIn => cubic_in, CubicOut => cubic_out, CubicInOut => cubic_in_out,
    QuartIn => quart_in, QuartOut => quart_out, QuartInOut => quart_in_out,
    QuintIn => quint_in, QuintOut => quint_out, QuintInOut => quint_in_out,
    SineIn => sine_in, SineOut => sine_out, SineInOut => sine_in_out,
    ExpoIn => expo_in, ExpoOut => expo_out, ExpoInOut => expo_in_out,
    CircIn => circ_in, CircOut => circ_out, CircInOut => circ_in_out,
    BackIn => back_in, BackOut => back_out, BackInOut => back_in_out,
    ElasticIn => elastic_in, ElasticOut => elastic_out, ElasticInOut => elastic_in_out,
    BounceIn => bounce_in, BounceOut => bounce_out, BounceInOut => bounce_in_out,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_exact() {
        assert_eq!(Easing::ALL.len(), 31);
        for e in Easing::ALL {
            assert!(e.apply(0.0).abs() < 1e-6, "{e:?}(0) = {}", e.apply(0.0));
            assert!((e.apply(1.0) - 1.0).abs() < 1e-6, "{e:?}(1) = {}", e.apply(1.0));
            assert_eq!(e.apply(-3.0), e.apply(0.0), "{e:?} clamps");
            assert_eq!(e.apply(4.0), e.apply(1.0), "{e:?} clamps");
        }
    }

    #[test]
    fn in_out_pairs_are_symmetric() {
        type Ease = fn(f32) -> f32;
        let pairs: [(Ease, Ease); 10] = [
            (quad_in, quad_out),
            (cubic_in, cubic_out),
            (quart_in, quart_out),
            (quint_in, quint_out),
            (sine_in, sine_out),
            (expo_in, expo_out),
            (circ_in, circ_out),
            (back_in, back_out),
            (elastic_in, elastic_out),
            (bounce_in, bounce_out),
        ];
        for (fin, fout) in pairs {
            for i in 0..=20 {
                let t = i as f32 / 20.0;
                assert!((fout(t) - (1.0 - fin(1.0 - t))).abs() < 1e-5, "t={t}");
            }
        }
        for f in [
            quad_in_out,
            cubic_in_out,
            sine_in_out,
            circ_in_out,
            back_in_out,
            bounce_in_out,
        ] {
            assert!((f(0.5) - 0.5).abs() < 1e-6);
            assert!((f(0.2) + f(0.8) - 1.0).abs() < 1e-5);
        }
        assert!((elastic_in_out(0.5) - 0.5).abs() < 1e-6);
        assert!((expo_in_out(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn analytic_values() {
        assert_eq!(quad_in(0.5), 0.25);
        assert_eq!(cubic_out(0.5), 0.875);
        assert_eq!(quart_in_out(0.25), 8.0 * 0.25f32.powi(4));
        assert!((sine_out(1.0 / 3.0) - 0.5).abs() < 1e-6);
        assert!((expo_in(0.5) - 2f32.powi(-5)).abs() < 1e-7);
        assert!((circ_out(0.5) - 0.75f32.sqrt()).abs() < 1e-6);
        assert!(back_in(0.2) < 0.0, "winds up backwards");
        assert!(back_out(0.8) > 1.0, "overshoots");
        assert!(bounce_out(1.0 / 2.75 - 1e-4) < 1.0);
        assert!(
            (bounce_out(1.0 / 2.75) - 1.0).abs() < 1e-5,
            "first arc lands on 1"
        );
    }
}
