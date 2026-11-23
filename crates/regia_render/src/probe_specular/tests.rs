// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::frame_work::counted::*;

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    (Vec3::from(a) - Vec3::from(b)).length()
}

/// The fitted mean vector in the reference's frame (n = +z, v in the xz plane, x ≥ 0).
fn fit_in_frame(n_dot_v: f32, alpha: f32) -> [f32; 3] {
    let s = (1.0 - n_dot_v * n_dot_v).max(0.0).sqrt();
    specular_lobe_vector([0.0, 0.0, 1.0], [-s, 0.0, n_dot_v], n_dot_v, alpha)
}

/// A deterministic generator in [0, 1) (64-bit LCG, Knuth's MMIX constants).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A vector inside the unit ball (rejection from the cube).
    fn in_ball(&mut self) -> [f32; 3] {
        loop {
            let v = [
                2.0 * self.next() - 1.0,
                2.0 * self.next() - 1.0,
                2.0 * self.next() - 1.0,
            ];
            if Vec3::from(v).length_squared() <= 1.0 {
                return v;
            }
        }
    }
}

/// The six irradiances / π of light `a + b·l` per channel (`b[c]` the gradient of channel c).
fn cube_of(a: [f32; 3], b: [[f32; 3]; 3]) -> [[f32; 3]; 6] {
    std::array::from_fn(|j| {
        let axis = j / 2;
        let sign = if j.is_multiple_of(2) { 1.0 } else { -1.0 };
        std::array::from_fn(|c| a[c] + sign * (2.0 / 3.0) * b[c][axis])
    })
}

/// The fit against numerical integration of the GGX lobe, over the roughness and view angles a
/// frame meets (the ADR quotes 0.036 worst over α ∈ [0, 1], n·v ≥ 0.1).
#[test]
fn the_fitted_lobe_matches_numerical_integration() {
    for alpha in [0.1, 0.25, 0.5, 0.75, 1.0] {
        for n_dot_v in [0.15, 0.4, 0.7, 0.95] {
            let reference = ggx_lobe_reference(n_dot_v, alpha);
            let fit = fit_in_frame(n_dot_v, alpha);
            let err = dist(fit, reference.mean);
            assert!(
                err < 0.04,
                "α {alpha}, n·v {n_dot_v}: fit {fit:?} vs integrated {:?} (error {err})",
                reference.mean
            );
            assert!(
                reference.mean[0] <= 1e-6 && reference.mean[1].abs() < 1e-4,
                "the mean lies on the mirror side of the n-v plane: {:?}",
                reference.mean
            );
        }
    }
}

/// The reference integration itself: its albedo agrees with the engine's GGX directional albedo
/// (`directional_albedo_ggx`, sampled independently with visible normals) and never exceeds 1.
#[test]
fn the_reference_lobe_keeps_no_more_energy_than_it_receives() {
    // The midpoint rule is about 10⁻³ off the sampled estimate here; a narrow lobe at grazing
    // (α 0.05, n·v 0.1) needs finer cells than the reference uses, so it is not checked.
    for alpha in [0.2f32, 0.5, 0.8, 1.0] {
        for n_dot_v in [0.1, 0.3, 0.6, 1.0] {
            let reference = ggx_lobe_reference(n_dot_v, alpha);
            assert!(
                reference.albedo <= 1.0 + 1e-3,
                "α {alpha}, n·v {n_dot_v}: albedo {} > 1",
                reference.albedo
            );
            let sampled = crate::directional_albedo_ggx(n_dot_v, alpha.sqrt());
            assert!(
                (reference.albedo - sampled).abs() < 0.01,
                "α {alpha}, n·v {n_dot_v}: integrated albedo {} vs sampled {sampled}",
                reference.albedo
            );
        }
    }
}

#[test]
fn a_mirror_reads_the_mirror_direction_at_full_length() {
    for n_dot_v in [0.05f32, 0.3, 0.7, 1.0] {
        let lobe = specular_lobe(n_dot_v, 0.0);
        assert_eq!(lobe.lerp, 1.0);
        assert_eq!(lobe.length, 1.0);
        let s = (1.0 - n_dot_v * n_dot_v).sqrt();
        let r = [-s, 0.0, n_dot_v];
        assert!(dist(fit_in_frame(n_dot_v, 0.0), r) < 1e-6, "n·v {n_dot_v}");
    }
}

/// Rougher lobes lean toward the normal and shorten, never the other way; the roughest is
/// centered on the normal at about the clamped cosine's 2/3 (so the rough end meets the diffuse
/// lookup without a jump).
#[test]
fn rougher_lobes_lean_to_the_normal_and_shorten() {
    for n_dot_v in [0.0f32, 0.1, 0.5, 0.9, 1.0] {
        let mut last = specular_lobe(n_dot_v, 0.0);
        for step in 1..=100 {
            let lobe = specular_lobe(n_dot_v, step as f32 / 100.0);
            assert!(
                lobe.length <= last.length + 1e-6,
                "length grew at n·v {n_dot_v}, step {step}"
            );
            assert!(
                lobe.lerp <= last.lerp + 1e-6,
                "lean grew at n·v {n_dot_v}, step {step}"
            );
            assert!(
                (0.45..=1.0).contains(&lobe.length),
                "length {} out of range",
                lobe.length
            );
            last = lobe;
        }
        assert_eq!(last.lerp, 0.0, "α = 1 is centered on the normal");
    }
    let roughest = specular_lobe(1.0, 1.0).length;
    assert!((roughest - 2.0 / 3.0).abs() < 0.06, "α = 1 length {roughest}");
}

/// White furnace: uniform light comes back unchanged through any lobe, from the cube and from the
/// sky map's two samples, and the detail ratio of a uniform sky is 1.
#[test]
fn uniform_light_comes_back_unchanged_at_every_roughness() {
    let light = [0.8f32, 1.7, 3.2];
    let cube = [light; 6];
    let mut rng = Lcg(7);
    for _ in 0..200 {
        let m = rng.in_ball();
        let got = cube_lobe_radiance(&cube, m);
        assert!(dist(got, light) < 1e-5, "m {m:?}: {got:?}");
        let len = Vec3::from(m).length();
        assert!(dist(axis_lobe_radiance(light, light, len), light) < 1e-5);
    }
    for alpha in [0.0f32, 0.3, 1.0] {
        let m = specular_lobe_vector([0.0, 1.0, 0.0], [0.6, 0.8, 0.0], 0.8, alpha);
        assert!(dist(cube_lobe_radiance(&cube, m), light) < 1e-5, "α {alpha}");
    }
    assert_eq!(specular_detail(2.5, 2.5), 1.0);
}

/// Light linear in direction is rebuilt exactly from its cosine-convolved cube (the 3/2
/// deconvolution of band 1): the lobe reads a + b·m for any mean vector.
#[test]
fn linear_light_is_rebuilt_exactly_from_the_cube() {
    let mut rng = Lcg(11);
    for _ in 0..100 {
        let a = [0.2 + rng.next(), 0.2 + rng.next(), 0.2 + rng.next()];
        // |b| ≤ a keeps every cube value positive (no clamp in the screen pass).
        let b: [[f32; 3]; 3] = std::array::from_fn(|c| {
            let v = Vec3::from(rng.in_ball()) * a[c];
            v.to_array()
        });
        let cube = cube_of(a, b);
        let m = rng.in_ball();
        let got = cube_lobe_radiance(&cube, m);
        for (c, g) in got.iter().enumerate() {
            let want = a[c] + Vec3::from(b[c]).dot(Vec3::from(m));
            assert!((g - want).abs() < 1e-5, "channel {c}: {g} vs {want}");
        }
        // The sky map's two-sample form along the lobe's own axis.
        let len = Vec3::from(m).length();
        if len > 1e-3 {
            let dir = Vec3::from(m) / len;
            let e_pos = std::array::from_fn(|c| a[c] + (2.0 / 3.0) * Vec3::from(b[c]).dot(dir));
            let e_neg = std::array::from_fn(|c| a[c] - (2.0 / 3.0) * Vec3::from(b[c]).dot(dir));
            let axis = axis_lobe_radiance(e_pos, e_neg, len);
            assert!(dist(axis, got) < 1e-5, "{axis:?} vs {got:?}");
        }
    }
}

/// Energy bounds on arbitrary (clamped, non-linear) cubes: never negative, never above the
/// brightest a non-negative field with that mean can send (4 × the mean), and over a pair of
/// opposite lobes no light is made while the clamp is idle (the pair averages to the mean).
#[test]
fn reconstructed_lobe_light_stays_within_the_energy_bounds() {
    let mut rng = Lcg(23);
    for _ in 0..500 {
        let cube: [[f32; 3]; 6] = std::array::from_fn(|_| {
            let k = rng.next();
            // Half the cubes are strongly directional (most of the light from one side).
            let skew = if k < 0.5 { rng.next() * 0.05 } else { rng.next() };
            [skew, skew * rng.next(), skew * rng.next() * 2.0]
        });
        let m = rng.in_ball();
        let got = cube_lobe_radiance(&cube, m);
        let flipped = cube_lobe_radiance(&cube, (-Vec3::from(m)).to_array());
        for (c, (g, f)) in got.iter().zip(flipped).enumerate() {
            let mean = cube.iter().map(|e| e[c]).sum::<f32>() / 6.0;
            assert!(*g >= 0.0);
            assert!(*g <= 4.0 * mean + 1e-6, "channel {c}: {g} > 4 × {mean}");
            assert!(0.5 * (g + f) >= mean - 1e-6);
        }
        // A short lobe (|m| ≤ 1/3) never meets the clamp: the pair is exactly the mean.
        let short = (Vec3::from(m) / 3.0).to_array();
        let s0 = cube_lobe_radiance(&cube, short);
        let s1 = cube_lobe_radiance(&cube, (-Vec3::from(short)).to_array());
        for (c, (x0, x1)) in s0.iter().zip(s1).enumerate() {
            let mean = cube.iter().map(|e| e[c]).sum::<f32>() / 6.0;
            assert!((0.5 * (x0 + x1) - mean).abs() < 1e-5);
        }
    }
}

/// A single directional light: its true irradiance cube (clamped cosine, every value ≥ 0) never
/// reflects more than the light's own L1 peak, W/π, through any lobe, but for the ambient cube's
/// own error: toward a cube diagonal the six values overstate the mean (√3/6 of W/π instead of
/// 1/4), which lets a mirror lobe there read up to 3.9 % over the peak (bounded here at 4 %).
#[test]
fn a_single_light_never_reflects_more_than_its_peak() {
    let mut rng = Lcg(31);
    for _ in 0..100 {
        let w = Vec3::from(rng.in_ball()).normalize_or(Vec3::Y);
        let watts = 5.0f32;
        let cube: [[f32; 3]; 6] = std::array::from_fn(|j| {
            let sign = if j.is_multiple_of(2) { 1.0 } else { -1.0 };
            let axis = Vec3::AXES[j / 2] * sign;
            [watts / std::f32::consts::PI * w.dot(axis).max(0.0); 3]
        });
        let peak = watts / std::f32::consts::PI;
        for _ in 0..20 {
            let m = rng.in_ball();
            let got = cube_lobe_radiance(&cube, m);
            assert!(got[0] <= 1.04 * peak, "{} > 1.04 × {peak}", got[0]);
        }
    }
}

#[test]
fn the_detail_ratio_is_capped_and_safe_on_a_dark_sky() {
    assert_eq!(specular_detail(100.0, 1.0), PROBE_SPECULAR_DETAIL_MAX);
    assert_eq!(specular_detail(0.5, 1.0), 0.5);
    assert_eq!(specular_detail(-1.0, 1.0), 0.0);
    assert_eq!(specular_detail(5.0, 0.0), 1.0);
    assert_eq!(specular_detail(0.0, 0.0), 1.0);
}

#[test]
fn modes_round_trip_and_set_their_bit() {
    assert_eq!(ProbeSpecular::default(), ProbeSpecular::Lobe);
    for mode in [
        ProbeSpecular::Lobe,
        ProbeSpecular::RoughOnly,
        ProbeSpecular::LobeHue,
    ] {
        assert_eq!(ProbeSpecular::from_name(mode.name()), Some(mode));
    }
    assert_eq!(
        ProbeSpecular::LobeHue.ctrl_bits(),
        CTRL_PROBE_SPECULAR | CTRL_PROBE_SPECULAR_HUE
    );
    assert_eq!(ProbeSpecular::from_name("mirror"), None);
    assert_eq!(ProbeSpecular::Lobe.ctrl_bits(), CTRL_PROBE_SPECULAR);
    assert_eq!(ProbeSpecular::RoughOnly.ctrl_bits(), 0);
    // Clear of the bits `ddgi.rs` and `baked_gi.rs` already use (1, 2, 4, 8, 8..11, 12).
    assert_eq!(CTRL_PROBE_SPECULAR & 0x1fff, 0);
    // Between `baked_gi.rs`' 16 and the inactive-probe shift (bits 8..11).
    assert_eq!(CTRL_PROBE_SPECULAR_HUE & 0x1f1f, 0);
}

/// The blue-heavy studio gradient's cube (`tests/studio_gradient.rs`), by numerical integration.
fn gradient_cube() -> [[f32; 3]; 6] {
    let side = [1557.56, 1997.09, 2861.04];
    [
        side,
        side,
        [2466.67, 3733.34, 6333.35],
        [600.0, 600.0, 600.0],
        side,
        side,
    ]
}

/// The defect: a mirror aimed at the nadir under the blue sky. The default clamps blue and green
/// to 0 and leaves red; every channel of the mode keeps its share of its mean.
#[test]
fn a_nadir_lobe_keeps_every_channel() {
    let cube = gradient_cube();
    let down = [0.0, -1.0, 0.0];
    let old = cube_lobe_radiance(&cube, down);
    assert!(
        old[1] == 0.0 && old[2] == 0.0 && old[0] > 100.0,
        "the defect: {old:?}"
    );
    let new = cube_lobe_radiance_hue(&cube, down);
    for (c, value) in new.iter().enumerate() {
        let mean = cube.iter().map(|e| e[c]).sum::<f32>() / 6.0;
        assert!(
            *value >= PROBE_SPECULAR_HUE_FLOOR * mean - 1e-3,
            "channel {c}: {new:?}"
        );
    }
    // The spot's color does not come back: the lobe's color is near the mean's, never without blue.
    assert!(new[2] > new[0], "{new:?}");
}

/// A lobe at 0.9 of its mean's luminance or more, no channel at its floor, is the default exactly.
/// Below that its luminance is kept while its color fades to the mean's. Every lobe keeps each
/// channel's floor share, and the default's bound of 4 × the mean still holds.
#[test]
fn the_mode_changes_only_a_dim_lobe_or_a_channel_at_its_floor() {
    let mut rng = Lcg(41);
    let luma = |v: [f32; 3]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
    for _ in 0..500 {
        let cube: [[f32; 3]; 6] = std::array::from_fn(|_| {
            let skew = if rng.next() < 0.5 {
                rng.next() * 0.05
            } else {
                rng.next()
            };
            [skew, skew * rng.next(), skew * rng.next() * 2.0]
        });
        let m = rng.in_ball();
        let old = cube_lobe_radiance(&cube, m);
        let new = cube_lobe_radiance_hue(&cube, m);
        let means: [f32; 3] = std::array::from_fn(|c| cube.iter().map(|e| e[c]).sum::<f32>() / 6.0);
        let floors = means.map(|mean| PROBE_SPECULAR_HUE_FLOOR * mean);
        let lifted: [f32; 3] = std::array::from_fn(|c| old[c].max(floors[c]));
        for c in 0..3 {
            assert!(
                new[c] >= floors[c] - 1e-4 * floors[c].max(1.0),
                "channel {c}: {new:?} {floors:?}"
            );
            assert!(
                new[c] <= 4.0 * means[c] + 1e-5,
                "channel {c}: {} > 4 × {}",
                new[c],
                means[c]
            );
        }
        assert!((luma(new) - luma(lifted)).abs() <= 1e-4 * luma(lifted).max(1.0));
        if luma(lifted) >= 0.9 * luma(means) {
            assert_eq!(new, lifted, "a lobe at the mean's level keeps its own color");
        }
    }
}

/// The ring around the spot: a lobe half way down under the blue sky read magenta (red above green)
/// with the floor alone. It takes the mean's color now.
#[test]
fn a_dim_lobe_takes_the_means_color() {
    let cube = gradient_cube();
    let r = (-45.0f32).to_radians();
    let lobe = cube_lobe_radiance_hue(&cube, [r.cos(), r.sin(), 0.0]);
    assert!(lobe[0] < lobe[1] && lobe[1] < lobe[2], "{lobe:?}");
    let means: [f32; 3] = std::array::from_fn(|c| cube.iter().map(|e| e[c]).sum::<f32>() / 6.0);
    assert!(hue_agreement(lobe, means) > 0.99, "{lobe:?}");
}

/// A red wall opposite a green one (the case one shared scale for the channels gets wrong): each
/// wall keeps its color at full strength through the lobe aimed at it.
#[test]
fn opposite_colored_walls_keep_their_colors() {
    let side = [0.2, 0.2, 0.2];
    let cube = [[3.0, 0.2, 0.2], [0.2, 3.0, 0.2], side, side, side, side];
    let toward_red = cube_lobe_radiance_hue(&cube, [1.0, 0.0, 0.0]);
    assert_eq!(toward_red[0], cube_lobe_radiance(&cube, [1.0, 0.0, 0.0])[0]);
    assert!(
        toward_red[0] > 4.0 * toward_red[1],
        "the red wall reads red: {toward_red:?}"
    );
    let toward_green = cube_lobe_radiance_hue(&cube, [-1.0, 0.0, 0.0]);
    assert!(
        toward_green[1] > 4.0 * toward_green[0],
        "the green wall reads green: {toward_green:?}"
    );
}

/// Uniform light comes back unchanged through the mode, as through the default, at every length of
/// the mean vector: the detail ratio is then 1 in every channel.
#[test]
fn uniform_light_comes_back_unchanged_through_the_mode() {
    let l = [0.8, 1.7, 3.1];
    let cube = [l; 6];
    let mut rng = Lcg(43);
    for _ in 0..50 {
        let got = cube_lobe_radiance_hue(&cube, rng.in_ball());
        let sky = axis_lobe_radiance_hue(l, l, rng.next());
        let detail = specular_detail_rgb(l, sky, got, 1.0);
        for c in 0..3 {
            assert!((got[c] - l[c]).abs() < 1e-5);
            assert!((detail[c] - 1.0).abs() < 1e-5);
        }
    }
}

/// The detail per channel: where probes and sky agree, the frame reads the prefilter's own color.
#[test]
fn the_rgb_detail_reads_the_prefilters_color_where_probes_and_sky_agree() {
    let prefilter = [600.0, 600.0, 600.0];
    let lobe = axis_lobe_radiance_hue([2466.67, 3733.34, 6333.35].map(|v| v / 8.0), [600.0; 3], 1.0);
    let detail = specular_detail_rgb(prefilter, lobe, lobe, 1.0);
    for c in 0..3 {
        assert!(
            (lobe[c] * detail[c] - prefilter[c]).abs() < 1e-2,
            "{lobe:?} {detail:?}"
        );
    }
    // Capped per channel, 1 on a dark channel, never negative.
    let sky = [1.0, 1.0, 0.0];
    assert_eq!(
        specular_detail_rgb([100.0, 0.5, 5.0], sky, sky, 0.0),
        [PROBE_SPECULAR_DETAIL_MAX, 0.5, 1.0]
    );
    assert_eq!(specular_detail_rgb([-1.0; 3], [1.0; 3], [1.0; 3], 0.0), [0.0; 3]);
}

/// Where the probes do not see that sky (the object hides it from them, a room's walls) the ratio
/// is the luminance detail: no complement of the sky's color on the probes' light.
#[test]
fn the_rgb_detail_falls_back_to_luminance_where_the_colors_differ() {
    let blue_sky = [310.0, 411.0, 612.0];
    for probes in [[500.0, 500.0, 500.0], [900.0, 80.0, 60.0], [0.0; 3]] {
        assert_eq!(hue_agreement(probes, blue_sky), 0.0, "{probes:?}");
        assert_eq!(specular_detail_rgb([600.0; 3], blue_sky, probes, 1.25), [1.25; 3]);
    }
    assert!((hue_agreement(blue_sky, blue_sky) - 1.0).abs() < 1e-5);
    // The level of the light does not decide it, only its color.
    let dim = blue_sky.map(|v| v * 1e-3);
    assert!((hue_agreement(dim, blue_sky) - 1.0).abs() < 1e-5);
    // The map's band 2 at the nadir, measured on the gradient: still agreeing.
    assert!(hue_agreement(blue_sky, [306.0, 433.0, 693.0]) > 0.9);
}

/// The plane's swap: what it hides leaves, its light arrives, and no channel falls under its share
/// of the lobe whatever the plane hides.
#[test]
fn the_ground_swap_keeps_each_channels_share() {
    assert_eq!(
        ground_swap([10.0, 10.0, 10.0], [2.0, 4.0, 6.0], [1.0, 0.0, 0.0]),
        [9.0, 6.0, 4.0]
    );
    let swapped = ground_swap([10.0, 5.0, 1.0], [100.0, 100.0, 100.0], [0.0; 3]);
    for (got, lobe) in swapped.iter().zip([10.0f32, 5.0, 1.0]) {
        assert!(
            (got - PROBE_SPECULAR_HUE_FLOOR * lobe).abs() < 1e-5,
            "{swapped:?}"
        );
    }
}

/// The shader carries the same constants as this mirror.
#[test]
fn the_shader_constants_match_the_mirror() {
    for v in LEN_A.iter().chain(&LEN_B).chain(&LEN_GRAZE).chain(&LERP_K) {
        let text = format!("{v}");
        assert!(
            PROBE_SPECULAR_WGSL.contains(&text),
            "probe_specular.wgsl lacks {text}"
        );
    }
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_DETAIL_MAX: f32 = {PROBE_SPECULAR_DETAIL_MAX:.1}")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_HUE_FLOOR: f32 = {PROBE_SPECULAR_HUE_FLOOR:.1}")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_CTRL_HUE: u32 = {CTRL_PROBE_SPECULAR_HUE}u")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_HUE_FADE_LOW: f32 = {HUE_FADE_LOW}")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_HUE_FADE_HIGH: f32 = {HUE_FADE_HIGH}")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_HUE_AGREE_LOW: f32 = {HUE_AGREE_LOW}")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_HUE_AGREE_HIGH: f32 = {HUE_AGREE_HIGH}")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_MAX_GRADIENT: f32 = {MAX_GRADIENT:.1}")));
    assert!(PROBE_SPECULAR_WGSL.contains(&format!("PSPEC_DECONVOLVE: f32 = {DECONVOLVE:.1}")));
    assert!(crate::ddgi_shading_wgsl().contains("fn ddgi_screen_gi("));
}

/// `ddgi_pack_rgb9e5` (`ddgi_common.wgsl`) on the CPU, to write screen texels.
fn pack_rgb9e5(c: [f32; 3]) -> u32 {
    let m = c.map(|v| v.clamp(0.0, 65408.0));
    let maxc = m[0].max(m[1]).max(m[2]);
    let mut e = (maxc.max(1e-30).log2().floor() as i32).max(-16) + 16;
    let mut denom = 2f32.powi(e - 24);
    if (maxc / denom + 0.5).floor() >= 512.0 {
        denom *= 2.0;
        e += 1;
    }
    let q = m.map(|v| (v / denom + 0.5).floor().min(511.0) as u32);
    q[0] | (q[1] << 9) | (q[2] << 18) | ((e.clamp(0, 31) as u32) << 27)
}

const GPU_HEADER: &str = "
@group(0) @binding(0) var<uniform> ddgi: DdgiUniform;
@group(0) @binding(1) var<storage, read> ddgi_data: array<u32>;
@group(0) @binding(2) var<storage, read> cases: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> results: array<vec4<f32>>;
";

const GPU_ENTRY: &str = "
@compute @workgroup_size(1)
fn probe_spec_case(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let c0 = cases[3u * i];
    let c1 = cases[3u * i + 1u];
    let c2 = cases[3u * i + 2u];
    let m = probe_spec_lobe(c0.xyz, c1.xyz, c1.w, c0.w);
    let p = vec3<f32>(0.0, 0.0, c2.z);
    let g = ddgi_screen_gi(c2.xy, p, c0.xyz, m, vec3<f32>(0.0));
    results[8u * i] = vec4<f32>(m, 0.0);
    results[8u * i + 1u] = g.diffuse;
    results[8u * i + 2u] = vec4<f32>(g.spec, probe_spec_detail(c2.w, 1.0));
    results[8u * i + 3u] = ddgi_screen_irradiance(c2.xy, p, c0.xyz, vec3<f32>(0.0));
    // The mode's sky terms (ADR-2996), from values of the case itself.
    let sky_pos = c0.xyz * 3.0 + vec3<f32>(c2.w);
    let sky_neg = c1.xyz * 2.0 + vec3<f32>(0.25);
    let hue_sky = probe_spec_hue_axis_lobe(abs(sky_pos), abs(sky_neg), c1.w);
    results[8u * i + 4u] = vec4<f32>(hue_sky, 0.0);
    results[8u * i + 5u] = vec4<f32>(
        probe_spec_detail_rgb(abs(sky_pos), hue_sky, g.spec, probe_spec_detail(c2.w, 1.0)),
        probe_spec_hue_agreement(g.spec, hue_sky),
    );
    results[8u * i + 6u] = vec4<f32>(probe_spec_ground_swap(abs(sky_pos), abs(sky_neg), hue_sky), 0.0);
    results[8u * i + 7u] = vec4<f32>(probe_spec_axis_lobe(abs(sky_pos), abs(sky_neg), c1.w), 0.0);
}
";

/// The WGSL (`probe_specular.wgsl` through the probe shading module) against this mirror, on a
/// one-row screen lookup with a texel per case: the lobe vector, the lobe light rebuilt from the
/// texel, the detail ratio, and the diffuse result against `ddgi_screen_irradiance`, at every
/// resolution step of the lookup (`ddgi.screen.w` = pixels per texel: Full, Half and Quarter;
/// ADR-1377: `ddgi_screen_gi` read Half's texels at every step).
/// Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).
#[test]
fn the_shader_matches_its_cpu_mirror() {
    for step in [1u32, 2, 4] {
        shader_matches_its_cpu_mirror_at(step, ProbeSpecular::Lobe);
    }
}

/// [`the_shader_matches_its_cpu_mirror`] with [`ProbeSpecular::LobeHue`]'s bit up: the mode's lobe
/// light, sky lobe, per-channel detail and ground swap agree with their mirrors within the bound
/// of that test, 1e-4 of the value (1e-4 absolute under 1). The mode's cases include lobes that
/// meet the floor (random cubes, every length of the mean vector).
#[test]
fn the_shader_matches_its_cpu_mirror_with_the_hue_bit() {
    for step in [1u32, 2, 4] {
        shader_matches_its_cpu_mirror_at(step, ProbeSpecular::LobeHue);
    }
}

/// [`the_shader_matches_its_cpu_mirror`] for the lookup step `step`: case `i` sits on pixel
/// `step·i + ½`, the centre of texel `i`'s first pixel.
fn shader_matches_its_cpu_mirror_at(step: u32, mode: ProbeSpecular) {
    use wgpu::util::DeviceExt;

    const ALPHAS: [f32; 8] = [0.0, 0.05, 0.2, 0.4, 0.6, 0.8, 0.95, 1.0];
    const CASES: usize = ALPHAS.len();
    let device = crate::test_device();
    let d = &device.device;
    let mut rng = Lcg(97);
    let mut words = vec![0u32; CASES * 8];
    let mut cases = Vec::with_capacity(CASES * 12);
    let mut expect = Vec::with_capacity(CASES);
    for (i, alpha) in ALPHAS.into_iter().enumerate() {
        let cube: [[f32; 3]; 6] =
            std::array::from_fn(|_| [rng.next() * 4.0, rng.next() * 2.0, rng.next() * 0.5]);
        let distance = 10.0 + 5.0 * i as f32;
        let coverage = 0.25 + 0.1 * i as f32;
        for (j, e) in cube.iter().enumerate() {
            words[i * 8 + j] = pack_rgb9e5(*e);
        }
        words[i * 8 + 6] = distance.to_bits();
        words[i * 8 + 7] = coverage.to_bits();
        let decoded: [[f32; 3]; 6] = std::array::from_fn(|j| crate::ddgi::unpack_rgb9e5(words[i * 8 + j]));
        let n = Vec3::from(rng.in_ball()).normalize_or(Vec3::Y);
        let mut v = Vec3::from(rng.in_ball()).normalize_or(Vec3::Z);
        if v.dot(n) < 0.05 {
            v = (v + n * (0.05 - v.dot(n) + 0.3)).normalize();
        }
        let n_dot_v = n.dot(v).clamp(1e-5, 1.0);
        let r = n * (2.0 * n.dot(v)) - v;
        let pref_lum = 0.5 * i as f32;
        cases.extend_from_slice(&[n.x, n.y, n.z, alpha, r.x, r.y, r.z, n_dot_v]);
        cases.extend_from_slice(&[(step * i as u32) as f32 + 0.5, 0.5, distance, pref_lum]);
        expect.push((n, r, n_dot_v, alpha, decoded, coverage, pref_lum));
    }
    // `screen.w` is the lookup's resolution step (`step`: 1 full, 2 half, 4 quarter); half maps
    // the fragment at x = 2i + 0.5 to texel i.
    let uniform = crate::ddgi::DdgiUniformGpu::screen_only([0, CASES as u32, 1, step], 1 | mode.ctrl_bits());
    let usage = wgpu::BufferUsages::STORAGE;
    let uniform_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("probe spec test uniform"),
        contents: bytemuck::bytes_of(&uniform),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let data_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("probe spec test texels"),
        contents: bytemuck::cast_slice(&words),
        usage,
    });
    let case_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("probe spec test cases"),
        contents: bytemuck::cast_slice(&cases),
        usage,
    });
    let out_size = (CASES * 32 * 4) as u64;
    let out_buf = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe spec test results"),
        size: out_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe spec test readback"),
        size: out_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let source = format!("{GPU_HEADER}\n{}\n{GPU_ENTRY}", crate::ddgi_shading_wgsl());
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("probe spec test"),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("probe spec test"),
        layout: None,
        module: &module,
        entry_point: Some("probe_spec_case"),
        compilation_options: Default::default(),
        cache: None,
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("probe spec test"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: data_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: case_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: out_buf.as_entire_binding(),
            },
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("probe spec test"),
    });
    {
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("probe spec test"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&pipeline);
        pass.counted_set_bind_group(0, &bind, &[]);
        pass.counted_dispatch(CASES as u32, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&out_buf, 0, &staging, 0, out_size);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll the test device");
    rx.recv().expect("map callback").expect("map the results");
    let out: Vec<f32> =
        bytemuck::pod_collect_to_vec(&staging.slice(..).get_mapped_range().expect("mapped range"));
    staging.unmap();

    let close = |a: f32, b: f32, what: &str, i: usize| {
        assert!(
            (a - b).abs() <= 1e-4 * b.abs().max(1.0),
            "step {step} case {i} {what}: shader {a} vs mirror {b}"
        );
    };
    for (i, (n, r, n_dot_v, alpha, decoded, coverage, pref_lum)) in expect.into_iter().enumerate() {
        let row = &out[i * 32..i * 32 + 32];
        let m = specular_lobe_vector(n.to_array(), r.to_array(), n_dot_v, alpha);
        for (have, want) in row[..3].iter().zip(m) {
            close(*have, want, "lobe vector", i);
        }
        let texel = crate::DdgiScreenTexel { e: decoded, coverage };
        let diffuse = texel.at(n.to_array());
        for (have, want) in row[4..7].iter().zip(diffuse) {
            close(*have, want, "diffuse", i);
        }
        // The weighted average of one texel picked twice (the row is one texel high): to rounding.
        close(row[7], coverage, "coverage", i);
        let spec = match mode {
            ProbeSpecular::LobeHue => cube_lobe_radiance_hue(&decoded, [row[0], row[1], row[2]]),
            _ => cube_lobe_radiance(&decoded, [row[0], row[1], row[2]]),
        };
        for (have, want) in row[8..11].iter().zip(spec) {
            close(*have, want, "lobe light", i);
        }
        close(row[11], specular_detail(pref_lum, 1.0), "detail", i);
        // The same texels, weights and rebuild as the diffuse lookup (to float rounding: the two
        // functions may compile to different instruction orders).
        for (c, (gi, irr)) in row[4..8].iter().zip(&row[12..16]).enumerate() {
            assert!(
                (gi - irr).abs() <= 1e-6 * irr.abs().max(1.0),
                "step {step} case {i}: ddgi_screen_gi's diffuse {gi} differs from ddgi_screen_irradiance's {irr} in channel {c}"
            );
        }
        let (a, b) = (n.to_array(), r.to_array());
        let sky_pos: [f32; 3] = std::array::from_fn(|c| (a[c] * 3.0 + pref_lum).abs());
        let sky_neg: [f32; 3] = std::array::from_fn(|c| (b[c] * 2.0 + 0.25).abs());
        let hue_sky = axis_lobe_radiance_hue(sky_pos, sky_neg, n_dot_v);
        let detail = specular_detail_rgb(sky_pos, hue_sky, spec, specular_detail(pref_lum, 1.0));
        let swap = ground_swap(sky_pos, sky_neg, hue_sky);
        let plain_sky = axis_lobe_radiance(sky_pos, sky_neg, n_dot_v);
        for c in 0..3 {
            close(row[16 + c], hue_sky[c], "hue sky lobe", i);
            close(row[20 + c], detail[c], "rgb detail", i);
            close(row[24 + c], swap[c], "ground swap", i);
            close(row[28 + c], plain_sky[c], "plain sky lobe", i);
        }
        close(row[23], hue_agreement(spec, hue_sky), "hue agreement", i);
    }
}

/// ADR-1377: the dominant direction the probes are read along (the lobe's mean direction) is the
/// mirror direction `r` at roughness 0 and the normal `n` at roughness 1, at every view angle; at
/// normal incidence the lean is Lagarde & de Rousiers' `(1 − α)(√(1 − α) + α)` (2014, § 4.9.3),
/// and it never leans back toward the mirror as the roughness grows.
#[test]
fn the_dominant_direction_runs_from_the_mirror_to_the_normal() {
    let n = Vec3::new(0.0, 1.0, 0.0);
    for n_dot_v in [0.1f32, 0.3, 0.6, 0.9, 1.0] {
        let s = (1.0 - n_dot_v * n_dot_v).sqrt();
        let v = Vec3::new(s, n_dot_v, 0.0);
        let r = n * (2.0 * n.dot(v)) - v;
        let dir = |alpha: f32| {
            Vec3::from(specular_lobe_vector(n.to_array(), r.to_array(), n_dot_v, alpha)).normalize()
        };
        assert!(
            dir(0.0).distance(r) < 1e-6,
            "n·v {n_dot_v}: α 0 reads along r, got {:?}",
            dir(0.0)
        );
        assert!(
            dir(1.0).distance(n) < 1e-6,
            "n·v {n_dot_v}: α 1 reads along n, got {:?}",
            dir(1.0)
        );
        let mut last = 1.0f32;
        for k in 0..=20 {
            let lerp = specular_lobe(n_dot_v, k as f32 / 20.0).lerp;
            assert!(
                lerp <= last + 1e-6,
                "n·v {n_dot_v}: the lean grows at α {}",
                k as f32 / 20.0
            );
            last = lerp;
        }
    }
    for k in 0..=20 {
        let a = k as f32 / 20.0;
        let lagarde = (1.0 - a) * ((1.0 - a).sqrt() + a);
        let lerp = specular_lobe(1.0, a).lerp;
        assert!(
            (lerp - lagarde).abs() < 1e-3,
            "α {a}: lean {lerp} vs Lagarde {lagarde}"
        );
    }
}
