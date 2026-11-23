// SPDX-License-Identifier: MPL-2.0
//! Which local lights the froxel fog sees (ADR-1365).
//!
//! The fog uniform has [`FOG_LIGHT_SLOTS`] local-light slots. [`crate::extract_frame`] turns every
//! extracted point and spot light into a [`FogLight`] ([`FogLight::from_local`]: point lumens
//! become candela, spot candela pass through), scores each one from the primary camera
//! ([`fog_light_score`]) and keeps the strongest ([`pick_fog_lights`]). A light picked last frame
//! has its score raised by [`FOG_PICK_HYSTERESIS`] for the ranking, so two lights of nearly equal
//! score do not swap in and out every frame, and a kept light keeps its slot.
//!
//! The pick and its scores are in [`FogLightPick`]; `render.fog_lights` reports them.
//!
//! Light in froxel fog: Wronski, "Volumetric Fog" (SIGGRAPH 2014); Hillaire, "Physically Based and
//! Unified Volumetric Rendering in Frostbite" (SIGGRAPH 2015).

use regia_app::prelude::*;

use crate::{FogLight, LabLocalLight};

/// Local-light slots in the fog uniform (`FogGpu::lights` holds three rows per slot).
pub const FOG_LIGHT_SLOTS: usize = 4;

/// A light picked last frame ranks with its score times this, so a newcomer must beat it by
/// 25 % to take its slot.
pub const FOG_PICK_HYSTERESIS: f32 = 1.25;

/// `KHR_lights_punctual` § 5.3: luminous intensity (cd) = luminous power (lm) / 4π.
const LUMENS_TO_CANDELA: f32 = 1.0 / (4.0 * std::f32::consts::PI);

impl FogLight {
    /// The fog light for an extracted local light, in the units the forward pass uses: a point
    /// light's lumens divided by 4π (candela), a spot light's candela as is, with its cone.
    #[must_use]
    pub fn from_local(light: &LabLocalLight) -> Self {
        match *light {
            LabLocalLight::Point(p) => {
                Self::point(p.position, p.color, p.intensity * LUMENS_TO_CANDELA, p.radius)
            }
            LabLocalLight::Spot(s) => Self::spot(
                s.position,
                s.direction,
                s.color,
                s.intensity,
                s.range,
                s.inner_angle,
                s.outer_angle,
            ),
        }
    }
}

/// A light the fog may pick: a stable key (the entity's bits on the runtime path) and the light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogLightCandidate {
    /// Identifies the light across frames (hysteresis and slot order).
    pub key: u64,
    /// The light in fog units.
    pub light: FogLight,
}

/// One picked light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogLightChoice {
    /// The candidate's key.
    pub key: u64,
    /// The light uploaded to the fog.
    pub light: FogLight,
    /// [`fog_light_score`] this frame (without the hysteresis factor).
    pub score: f32,
    /// True when the light was picked last frame too.
    pub held: bool,
}

/// The fog's pick for the last extracted frame (`render.fog_lights`).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct FogLightPick {
    /// The picked lights in slot order (at most [`FOG_LIGHT_SLOTS`]).
    pub chosen: Vec<FogLightChoice>,
    /// Local lights the frame had (the pick's candidates).
    pub candidates: usize,
    /// The camera position the lights were scored from.
    pub eye: [f32; 3],
}

/// How much a light can light the fog the camera sees:
/// `candela × max(color) × window(gap) / max(d², r²)`.
///
/// `d` is the camera's distance to the light and `r` the light's radius: inside its radius every
/// position scores as if at `r` (no blow-up at the light). `gap = max(d − r, 0)` is the distance
/// from the camera to the light's sphere, and `window(x) = (1 − (x / reach)⁴)₊` fades lights whose
/// lit air is beyond `reach` (the froxel range, the view's far plane). The window is taken on
/// the gap, not on `d`, so a muzzle flash 20 m ahead with an 8 m radius still scores. 0 for a
/// light that cannot light anything (non-finite, no radius, no intensity).
#[must_use]
pub fn fog_light_score(light: &FogLight, eye: [f32; 3], reach: f32) -> f32 {
    let candela = light.intensity * light.color.iter().copied().fold(0.0_f32, f32::max);
    let radius = light.radius;
    let finite = candela.is_finite() && radius.is_finite() && light.position.iter().all(|v| v.is_finite());
    if !finite || candela <= 0.0 || radius <= 0.0 {
        return 0.0;
    }
    let offset = [
        light.position[0] - eye[0],
        light.position[1] - eye[1],
        light.position[2] - eye[2],
    ];
    let d2 = offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2];
    if !d2.is_finite() {
        return 0.0;
    }
    let gap = (d2.sqrt() - radius).max(0.0);
    let window = if reach.is_finite() && reach > 0.0 {
        (1.0 - (gap / reach).powi(4)).max(0.0)
    } else {
        1.0
    };
    candela * window / d2.max(radius * radius)
}

/// The fog's lights for this frame: the [`FOG_LIGHT_SLOTS`] highest-scoring candidates with a
/// score above 0, ranked with [`FOG_PICK_HYSTERESIS`] for the keys in `previous` (last frame's
/// pick, in slot order). Ties rank by key. Lights kept from `previous` keep their relative slot
/// order and come first; new lights follow in rank order.
#[must_use]
pub fn pick_fog_lights(
    candidates: &[FogLightCandidate],
    eye: [f32; 3],
    reach: f32,
    previous: &[u64],
) -> Vec<FogLightChoice> {
    let mut ranked: Vec<(f32, FogLightChoice)> = candidates
        .iter()
        .filter_map(|c| {
            let score = fog_light_score(&c.light, eye, reach);
            if score <= 0.0 {
                return None;
            }
            let held = previous.contains(&c.key);
            let rank = if held { score * FOG_PICK_HYSTERESIS } else { score };
            Some((
                rank,
                FogLightChoice {
                    key: c.key,
                    light: c.light,
                    score,
                    held,
                },
            ))
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.key.cmp(&b.1.key)));
    ranked.truncate(FOG_LIGHT_SLOTS);
    let mut chosen: Vec<FogLightChoice> = previous
        .iter()
        .filter_map(|key| ranked.iter().find_map(|(_, c)| (c.key == *key).then_some(*c)))
        .collect();
    for (_, choice) in &ranked {
        if !choice.held {
            chosen.push(*choice);
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FogVolume, LabPointLight, LabSpotLight, SunScattering, VolumetricFogSettings, inscatter_per_metre,
    };

    const FOUR_PI: f32 = 4.0 * std::f32::consts::PI;

    fn dark_sun() -> SunScattering {
        SunScattering {
            travel: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: 0.0,
            shadow: 0.0,
        }
    }

    fn fog() -> (VolumetricFogSettings, [FogVolume; 1]) {
        let settings = VolumetricFogSettings {
            density: 0.0,
            anisotropy: 0.0,
            ..VolumetricFogSettings::default()
        };
        let volume = FogVolume::Box {
            center: [0.0, 0.0, 0.0],
            extents: [50.0, 50.0, 50.0],
            density: 0.1,
        };
        (settings, [volume])
    }

    /// The per-sample local in-scatter of one light, CPU mirror of `inscatter_at`.
    fn inscatter(light: FogLight, sample: [f32; 3]) -> [f32; 3] {
        let (settings, volumes) = fog();
        inscatter_per_metre(
            sample,
            [0.0, 0.0, -1.0],
            &dark_sun(),
            &[light],
            &settings,
            &volumes,
        )
    }

    fn spot_down_z(candela: f32) -> FogLight {
        FogLight::spot(
            [0.0, 0.0, 0.0],
            [0.0, 0.0, -1.0],
            [1.0; 3],
            candela,
            20.0,
            0.2,
            0.4,
        )
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1.0e-5 * a.abs().max(b.abs()).max(1.0e-12)
    }

    #[test]
    fn a_spot_on_axis_scatters_like_a_point_of_the_same_candela() {
        let spot = spot_down_z(1_000.0);
        let point = FogLight::point([0.0, 0.0, 0.0], [1.0; 3], 1_000.0, 20.0);
        for z in [-1.0_f32, -4.0, -9.5, -15.0] {
            let s = inscatter(spot, [0.0, 0.0, z]);
            let p = inscatter(point, [0.0, 0.0, z]);
            assert!(p[0] > 0.0, "the point lights the fog at z {z}: {p:?}");
            for c in 0..3 {
                assert!(close(s[c], p[c]), "z {z}: spot {s:?} point {p:?}");
            }
        }
        // Inside the inner cone (0.2 rad) the spot is still the point.
        let off = [3.0 * 0.15_f32.tan(), 0.0, -3.0];
        assert!(close(inscatter(spot, off)[1], inscatter(point, off)[1]));
    }

    #[test]
    fn a_spot_scatters_nothing_outside_its_outer_cone() {
        let spot = spot_down_z(1_000.0);
        // 0.45 rad off axis (outer 0.4), beside it, and behind the lamp.
        for sample in [
            [5.0 * 0.45_f32.tan(), 0.0, -5.0],
            [3.0, 0.0, 0.0],
            [0.0, 0.0, 2.0],
        ] {
            assert_eq!(inscatter(spot, sample), [0.0; 3], "{sample:?}");
        }
        // Between the cones the ramp is partial.
        let mid = inscatter(spot, [5.0 * 0.3_f32.tan(), 0.0, -5.0]);
        let point = inscatter(
            FogLight::point([0.0; 3], [1.0; 3], 1_000.0, 20.0),
            [5.0 * 0.3_f32.tan(), 0.0, -5.0],
        );
        assert!(mid[0] > 0.0 && mid[0] < point[0], "mid {mid:?} point {point:?}");
    }

    #[test]
    fn a_1000_cd_spot_equals_a_4pi_1000_lm_point() {
        // The forward pass treats spot intensity as candela and point intensity as lumens
        // (`pack_light`). Before ADR-1365 the fog divided both by 4π: the spot came out 12.6×
        // too dim.
        let spot = FogLight::from_local(&LabLocalLight::Spot(LabSpotLight {
            position: [0.0; 3],
            direction: [0.0, 0.0, -1.0],
            color: [1.0; 3],
            intensity: 1_000.0,
            range: 20.0,
            inner_angle: 0.2,
            outer_angle: 0.4,
            cast_shadows: false,
            source_radius: None,
        }));
        let point = FogLight::from_local(&LabLocalLight::Point(LabPointLight {
            position: [0.0; 3],
            color: [1.0; 3],
            intensity: FOUR_PI * 1_000.0,
            radius: 20.0,
            cast_shadows: false,
            source_radius: None,
        }));
        assert!(close(spot.intensity, 1_000.0), "spot {}", spot.intensity);
        assert!(close(point.intensity, 1_000.0), "point {}", point.intensity);
        let on_axis = [0.0, 0.0, -6.0];
        let s = inscatter(spot, on_axis);
        let p = inscatter(point, on_axis);
        assert!(close(s[0], p[0]), "spot {s:?} point {p:?}");
        assert!(point.cone.is_none());
        let cone = spot.cone.expect("a spot has a cone");
        assert!(close(cone.cos_inner, 0.2_f32.cos()) && close(cone.cos_outer, 0.4_f32.cos()));
    }

    #[test]
    fn the_packed_cone_row_is_the_same_ramp() {
        // What `inscatter_at` computes from the uniform row: saturate(dot(xyz, to_light) + w).
        let spot = spot_down_z(1.0);
        let row = spot.gpu_cone();
        for angle in [0.0_f32, 0.1, 0.25, 0.3, 0.35, 0.39, 0.41, 1.0, 2.5] {
            let to_light = [-angle.sin(), 0.0, angle.cos()];
            let shader =
                (row[0] * to_light[0] + row[1] * to_light[1] + row[2] * to_light[2] + row[3]).clamp(0.0, 1.0);
            let cpu = spot.cone_factor(to_light);
            assert!(
                (shader - cpu).abs() < 1.0e-4,
                "angle {angle}: shader {shader} cpu {cpu}"
            );
        }
        let point = FogLight::point([0.0; 3], [1.0; 3], 1.0, 1.0);
        assert_eq!(point.gpu_cone(), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(point.cone_factor([1.0, 0.0, 0.0]), 1.0);
    }

    #[test]
    fn the_fog_falloff_is_the_forward_passs_and_bounded_at_the_lamp() {
        // `distance_attenuation` in pbr_lighting.wgsl: saturate(1 − (d/r)⁴)² / (d² + 1).
        assert_eq!(crate::fog_light_falloff(0.0, 10.0), 1.0);
        assert!(close(
            crate::fog_light_falloff(1.0, 10.0),
            (1.0 - 1.0e-4_f32).powi(2) / 2.0
        ));
        assert!(close(
            crate::fog_light_falloff(4.0, 8.0),
            (1.0 - 0.0625_f32).powi(2) / 17.0
        ));
        assert_eq!(crate::fog_light_falloff(10.0, 10.0), 0.0);
        assert_eq!(crate::fog_light_falloff(12.0, 10.0), 0.0);
        assert_eq!(crate::fog_light_falloff(1.0, 0.0), 0.0);
        // A headlight at the eye: the air a millimetre from the lamp is about twice as bright as
        // the air a metre away (1 / (0 + 1) against 1 / (1 + 1)); a bare 1/d² made it a million
        // times brighter.
        let lamp = FogLight::point([0.0; 3], [1.0; 3], 40_000.0, 70.0);
        let touching = inscatter(lamp, [0.0, 0.0, -0.001]);
        let metre = inscatter(lamp, [0.0, 0.0, -1.0]);
        assert!(
            touching[0] <= 2.01 * metre[0],
            "touching {touching:?} metre {metre:?}"
        );
    }

    fn lamp(x: f32, candela: f32) -> FogLight {
        FogLight::point([x, 0.0, 0.0], [1.0; 3], candela, 4.0)
    }

    fn six() -> Vec<FogLightCandidate> {
        // Score at the origin: candela / max(x², 16). Strongest: keys 3, 5, 0, 4; then 1, 2.
        [
            (0, lamp(8.0, 3_200.0)),   // 50
            (1, lamp(10.0, 2_000.0)),  // 20
            (2, lamp(-30.0, 9_000.0)), // 10
            (3, lamp(2.0, 4_000.0)),   // 250 (inside its radius: / 16)
            (4, lamp(-6.0, 1_440.0)),  // 40
            (5, lamp(12.0, 14_400.0)), // 100
        ]
        .into_iter()
        .map(|(key, light)| FogLightCandidate { key, light })
        .collect()
    }

    fn keys(chosen: &[FogLightChoice]) -> Vec<u64> {
        chosen.iter().map(|c| c.key).collect()
    }

    #[test]
    fn the_four_strongest_at_the_camera_are_picked_in_a_stable_order() {
        let lights = six();
        let first = pick_fog_lights(&lights, [0.0; 3], 1_000.0, &[]);
        assert_eq!(keys(&first), vec![3, 5, 0, 4]);
        assert!(first.iter().all(|c| !c.held));
        assert!(close(first[0].score, 250.0), "{}", first[0].score);
        // Same lights, next frames: same set, same slots, now held.
        let mut previous = keys(&first);
        for _ in 0..3 {
            let next = pick_fog_lights(&lights, [0.0; 3], 1_000.0, &previous);
            assert_eq!(keys(&next), previous);
            assert!(next.iter().all(|c| c.held));
            previous = keys(&next);
        }
        // The input order does not matter.
        let mut reversed = lights.clone();
        reversed.reverse();
        assert_eq!(
            keys(&pick_fog_lights(&reversed, [0.0; 3], 1_000.0, &[])),
            vec![3, 5, 0, 4]
        );
    }

    #[test]
    fn hysteresis_holds_a_slot_until_a_newcomer_clearly_wins() {
        let mut lights = six();
        let previous = keys(&pick_fog_lights(&lights, [0.0; 3], 1_000.0, &[]));
        // Light 1 creeps past light 4 (40): 44 is within 25 %, so 4 keeps its slot.
        lights[1].light.intensity = 4_400.0;
        let held = pick_fog_lights(&lights, [0.0; 3], 1_000.0, &previous);
        assert_eq!(keys(&held), vec![3, 5, 0, 4]);
        // At 60 (> 40 × 1.25) light 1 takes the weakest slot; the other three keep theirs.
        lights[1].light.intensity = 6_000.0;
        let swapped = pick_fog_lights(&lights, [0.0; 3], 1_000.0, &previous);
        assert_eq!(keys(&swapped), vec![3, 5, 0, 1]);
        assert!(!swapped[3].held && swapped[0].held);
    }

    #[test]
    fn dead_lights_are_never_picked_and_far_air_fades() {
        let mut lights = vec![
            FogLightCandidate {
                key: 1,
                light: FogLight::point([f32::NAN, 0.0, 0.0], [1.0; 3], 1.0e6, 5.0),
            },
            FogLightCandidate {
                key: 2,
                light: FogLight::point([1.0, 0.0, 0.0], [1.0; 3], 1.0e6, 0.0),
            },
            FogLightCandidate {
                key: 3,
                light: FogLight::point([1.0, 0.0, 0.0], [0.0; 3], 1.0e6, 5.0),
            },
        ];
        assert!(pick_fog_lights(&lights, [0.0; 3], 100.0, &[]).is_empty());
        // A muzzle flash 20 m ahead with an 8 m radius scores; its lit air is 12 m away.
        let flash = FogLight::point([0.0, 0.0, -20.0], [1.0; 3], 5_000.0, 8.0);
        assert!(fog_light_score(&flash, [0.0; 3], 100.0) > 0.0);
        // Its sphere beyond the froxel range does not.
        assert_eq!(fog_light_score(&flash, [0.0, 0.0, 200.0], 100.0), 0.0);
        lights.push(FogLightCandidate { key: 4, light: flash });
        assert_eq!(keys(&pick_fog_lights(&lights, [0.0; 3], 100.0, &[])), vec![4]);
    }
}
