// SPDX-License-Identifier: MPL-2.0
//! The stochastic SSR roughness cutoff knob (v0.1.14, DO27 ask 7): the default is unchanged, the
//! setter clamps and is what the renderer reads, and the environment override beats it. Its own
//! test binary: the knob is process-wide, so it must not race the renderer's golden tests.

use regia_render::{
    SSSR_ROUGHNESS_BAND, SSSR_ROUGHNESS_CUTOFF, SSSR_ROUGHNESS_CUTOFF_ENV, SSSR_ROUGHNESS_CUTOFF_RANGE,
    set_sssr_roughness_cutoff, sssr_roughness_cutoff, sssr_roughness_cutoff_env,
};

#[test]
fn cutoff_knob_defaults_to_055_clamps_and_yields_to_the_environment() {
    assert_eq!(SSSR_ROUGHNESS_CUTOFF, 0.55);
    assert_eq!(SSSR_ROUGHNESS_CUTOFF_RANGE, (SSSR_ROUGHNESS_BAND, 1.0));
    let env = sssr_roughness_cutoff_env();
    assert_eq!(
        env.is_some(),
        regia_core::brand::env_var(SSSR_ROUGHNESS_CUTOFF_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .is_some_and(f32::is_finite),
        "the override is set exactly when the variable holds a number"
    );
    let effective = |host: f32| env.unwrap_or(host);
    assert_eq!(
        sssr_roughness_cutoff(),
        effective(SSSR_ROUGHNESS_CUTOFF),
        "default"
    );
    assert_eq!(set_sssr_roughness_cutoff(0.7), 0.7);
    assert_eq!(sssr_roughness_cutoff(), effective(0.7));
    assert_eq!(set_sssr_roughness_cutoff(0.01), SSSR_ROUGHNESS_BAND, "clamped up");
    assert_eq!(set_sssr_roughness_cutoff(3.0), 1.0, "clamped down");
    assert_eq!(
        set_sssr_roughness_cutoff(f32::NAN),
        SSSR_ROUGHNESS_CUTOFF,
        "NaN is the default"
    );
    assert_eq!(sssr_roughness_cutoff(), effective(SSSR_ROUGHNESS_CUTOFF));
}
