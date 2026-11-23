// SPDX-License-Identifier: MPL-2.0
//! The direct BRDF's energy model (ADR-2632, `book/agents/brdf-energy.md`).
//!
//! [`BrdfEnergy::Legacy`] is the BRDF every frame shipped with: Lambert weighted by
//! `1 − F(l·h)` and, with a material's `multi_scatter`, the GGX lobe scaled by Turquin's
//! compensation and the diffuse by `1 − E_s(μo)` (the single-scatter albedo). A white dielectric
//! seen at a grazing angle reflects more than it receives there: up to 1.12 of the incident energy
//! with `multi_scatter` and 1.68 without it (the look-measure finding, gap row "BRDF energy
//! excess").
//!
//! [`BrdfEnergy::Compensated`] is the Kulla & Conty 2017 form ("Revisiting Physically Based
//! Shading at Imageworks", SIGGRAPH course notes, § "Energy compensation" and "Energy
//! conservation of the diffuse"):
//!
//! - the multiple-scattering lobe is an additive, reciprocal term
//!   `f_ms = F_add (1 − E(μo)) (1 − E(μi)) / (π (1 − E_avg))` with
//!   `F_add = F_avg² E_avg / (1 − F_avg (1 − E_avg))`, where `E` is the F = 1 single-scatter
//!   albedo (the DFG table's B channel) and `E_avg` its cosine-weighted average (A);
//! - the diffuse is the share of the light the whole specular layer leaves, on both sides:
//!   `ρ/π · (1 − E_spec(μo)) (1 − E_spec(μi)) / (1 − E_spec,avg)`, with
//!   `E_spec(μ) = E_F(μ) + F_add (1 − E(μ))` and `E_spec,avg = F0 (E_avg − b̄) + b̄ + F_add (1 − E_avg)`,
//!   `b̄` the table's own average of the Fresnel-bias part ([`fresnel_bias_average`]).
//!
//! Both terms are symmetric in the two directions (Helmholtz reciprocity), and a white material
//! reflects at most the energy it receives at every angle: the diffuse integrates to
//! `ρ (1 − E_spec(μo))` and the specular to `E_spec(μo)`. `E_F(μ)`, the Fresnel-weighted
//! single-scatter albedo, is the split-sum Fresnel share `(F0·scale + bias) / (scale + bias)` of
//! the height-correlated lobe's albedo `E`. The image-based terms get the same accounting: the
//! diffuse environment is weighted by `1 − (F0·scale + bias) − ms`, the share the split-sum
//! specular and its multiple scattering leave.
//!
//! The setting is a forward shader axis ([`BRDF_ENERGY_COMPENSATED_DEF`]) and a path-tracer
//! flag. Off (the default) the shaders compose exactly what they composed before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

use naga_oil::compose::ShaderDefValue;

/// The forward shader define of [`BrdfEnergy::Compensated`] (`lab_forward.wgsl`,
/// `forward_mobile.wgsl` and the visibility-buffer resolve built from the former).
pub const BRDF_ENERGY_COMPENSATED_DEF: &str = "BRDF_ENERGY_COMPENSATED";

/// The environment variable that sets the process-wide mode ([`process_brdf_energy`]) before
/// the first forward module composes: `compensated` or `legacy`.
pub const BRDF_ENERGY_ENV: &str = "REGIA_BRDF_ENERGY";

/// The energy model of the direct BRDF and the image-based terms (ADR-2632).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BrdfEnergy {
    /// The shipped BRDF: Lambert under `1 − F(l·h)`, Turquin's specular compensation. Frames
    /// and goldens are unchanged.
    #[default]
    Legacy,
    /// Kulla & Conty's reciprocal multiple scattering and diffuse coupling: a white furnace
    /// returns at most its input at every angle and roughness.
    Compensated,
}

impl BrdfEnergy {
    /// Every mode, in declaration order.
    pub const ALL: [Self; 2] = [Self::Legacy, Self::Compensated];

    /// The protocol name: `legacy` or `compensated`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Compensated => "compensated",
        }
    }

    /// The mode named `name` ([`Self::name`], case-insensitive, trimmed), if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|mode| mode.name().eq_ignore_ascii_case(name))
    }

    /// The path tracer's code for the mode (`globals.dfg.z` in `pt.wgsl`): 0 legacy, 1 compensated.
    #[must_use]
    pub fn shader_code(self) -> u32 {
        match self {
            Self::Legacy => 0,
            Self::Compensated => 1,
        }
    }

    /// Adds [`BRDF_ENERGY_COMPENSATED_DEF`] to a forward module's `defs` for
    /// [`Self::Compensated`]; [`Self::Legacy`] adds nothing, so the module is the one it always was.
    pub fn add_forward_def(self, defs: &mut HashMap<String, ShaderDefValue>) {
        if self == Self::Compensated {
            defs.insert(
                BRDF_ENERGY_COMPENSATED_DEF.to_string(),
                ShaderDefValue::Bool(true),
            );
        }
    }
}

const UNSET: u8 = u8::MAX;
static PROCESS: AtomicU8 = AtomicU8::new(UNSET);

fn code(mode: BrdfEnergy) -> u8 {
    match mode {
        BrdfEnergy::Legacy => 0,
        BrdfEnergy::Compensated => 1,
    }
}

/// The process-wide mode every forward module composes with: what [`set_process_brdf_energy`]
/// last set, else [`BRDF_ENERGY_ENV`] (read once), else [`BrdfEnergy::Legacy`]. Pipelines built
/// before a change keep the mode they were built with.
#[must_use]
pub fn process_brdf_energy() -> BrdfEnergy {
    let mut value = PROCESS.load(Ordering::Relaxed);
    if value == UNSET {
        let from_env = regia_core::brand::env_var(BRDF_ENERGY_ENV)
            .ok()
            .and_then(|v| BrdfEnergy::from_name(&v))
            .unwrap_or_default();
        // A concurrent `set_process_brdf_energy` wins over the environment.
        value = match PROCESS.compare_exchange(UNSET, code(from_env), Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => code(from_env),
            Err(current) => current,
        };
    }
    if value == 1 {
        BrdfEnergy::Compensated
    } else {
        BrdfEnergy::Legacy
    }
}

/// Sets the process-wide mode (see [`process_brdf_energy`]). Forward modules composed from now
/// on take it; a renderer's existing pipelines do not change until they are rebuilt.
pub fn set_process_brdf_energy(mode: BrdfEnergy) {
    PROCESS.store(code(mode), Ordering::Relaxed);
}

/// Adds the process-wide mode's define to a forward module's `defs` (the composer's material
/// defines path, beside the driver workarounds).
pub(crate) fn add_process_def(defs: &mut HashMap<String, ShaderDefValue>) {
    process_brdf_energy().add_forward_def(defs);
}

/// Startup: a render profile that opted in to a non-default model sets the process-wide mode before
/// the first frame composes a forward module. A `Legacy` profile, the default, leaves the mode alone,
/// so it never undoes [`BRDF_ENERGY_ENV`] or a host's [`set_process_brdf_energy`].
pub(crate) fn apply_profile_model(settings: Option<regia_app::prelude::Res<crate::RenderSettings>>) {
    let Some(settings) = settings else {
        return;
    };
    if settings.profile.brdf_energy != BrdfEnergy::Legacy {
        set_process_brdf_energy(settings.profile.brdf_energy);
    }
}

/// A DFG table sample as the shaders read it: split-sum scale and bias, the F = 1 single-scatter
/// albedo `E(μ)` and its cosine-weighted average `E_avg` (`crate::build_brdf_lut`).
pub type DfgSample = [f32; 4];

/// The Fresnel-weighted single-scatter albedo `E_F(μ)` of one channel with normal-incidence
/// reflectance `f0`: the split sum's Fresnel share of the height-correlated lobe's albedo.
#[must_use]
pub fn single_scatter_albedo(f0: f32, s: DfgSample) -> f32 {
    let split = s[0] + s[1];
    if split <= 1e-4 {
        return 0.0;
    }
    ((f0 * s[0] + s[1]) / split * s[2]).clamp(0.0, 1.0)
}

/// The headroom added to the multiple-scattering lobe's normalization `1 − E_avg`: one step of
/// the 8-bit table (`E_avg` is rounded to 1/255, and rounds to 1 on the smoothest rows). A larger
/// denominator only takes energy away, so the lobe never reflects more than it should.
pub const MS_NORMALIZATION_MARGIN: f32 = 1.0 / 255.0;

/// Highest roughness of the fit [`fresnel_bias_average`]: the last table row's centre (the table
/// is constant above it).
const FIT_MAX_ROUGHNESS: f32 = 0.984_375;

/// [`fresnel_bias_average`]'s polynomial in perceptual roughness, highest power first.
const BIAS_AVERAGE_FIT: [f32; 7] = [
    1.2292322,
    -3.8261223,
    4.299933,
    -1.9304305,
    0.18521781,
    -0.0021520231,
    0.047328003,
];

/// The cosine-weighted average over n·v of the Fresnel-bias part of [`single_scatter_albedo`],
/// `b̄ = 2 ∫ bias / (scale + bias) · E(μ) μ dμ`, at perceptual roughness `perceptual`: a degree-6
/// fit of the table's own average (`build_brdf_lut`), within 0.0005 everywhere
/// (`regia_pt` test `the_bias_average_fit_tracks_the_table`). With it the specular layer's
/// average albedo is `F0 (E_avg − b̄) + b̄`, the diffuse coupling's exact normalization (the
/// smooth limit is Schlick's `F_avg`: `b̄ → 1/21`). Mirrors `pbr_brdf.wgsl::fresnel_bias_average`.
#[must_use]
pub fn fresnel_bias_average(perceptual: f32) -> f32 {
    let r = perceptual.clamp(0.0, FIT_MAX_ROUGHNESS);
    BIAS_AVERAGE_FIT.iter().fold(0.0, |acc, c| acc * r + c)
}

/// Kulla & Conty's Fresnel term of the multiple-scattering lobe,
/// `F_add = F_avg² E_avg / (1 − F_avg (1 − E_avg))`, with Schlick's `F_avg = f0 + (1 − f0)/21`.
#[must_use]
pub fn multi_scatter_fresnel(f0: f32, e_avg: f32) -> f32 {
    let favg = f0 + (1.0 - f0) / 21.0;
    let e_avg = e_avg.clamp(0.0, 1.0);
    favg * favg * e_avg / (1.0 - favg * (1.0 - e_avg)).max(1e-4)
}

/// The compensated BRDF's two energy terms for one channel, cosine excluded: the multiple-
/// scattering lobe `f_ms` and the diffuse lobe per unit diffuse colour (multiply by `ρ`). `so`
/// and `si` are the DFG samples at the view's and the light's cosines at perceptual roughness
/// `perceptual`; `multi_scatter` turns the specular's multiple scattering on (a material's flag).
/// Mirrors `pbr_brdf.wgsl::with_compensation`.
#[must_use]
pub fn compensated_terms(
    f0: f32,
    perceptual: f32,
    so: DfgSample,
    si: DfgSample,
    multi_scatter: bool,
) -> (f32, f32) {
    let e_avg = so[3].clamp(0.0, 1.0);
    let f_add = if multi_scatter {
        multi_scatter_fresnel(f0, e_avg)
    } else {
        0.0
    };
    let e_o = so[2].clamp(0.0, 1.0);
    let e_i = si[2].clamp(0.0, 1.0);
    let inv_pi = std::f32::consts::FRAC_1_PI;
    let ms = f_add * (1.0 - e_o) * (1.0 - e_i) * inv_pi / (1.0 - e_avg + MS_NORMALIZATION_MARGIN);
    let spec_o = (single_scatter_albedo(f0, so) + f_add * (1.0 - e_o)).min(1.0);
    let spec_i = (single_scatter_albedo(f0, si) + f_add * (1.0 - e_i)).min(1.0);
    let bias = fresnel_bias_average(perceptual);
    let spec_avg = (f0 * (e_avg - bias).max(0.0) + bias + f_add * (1.0 - e_avg)).min(1.0);
    let diffuse = (1.0 - spec_o) * (1.0 - spec_i) * inv_pi / (1.0 - spec_avg).max(1e-4);
    (ms, diffuse)
}

/// The share of the diffuse environment light the specular layer leaves under
/// [`BrdfEnergy::Compensated`] for one channel: `1 − E_ibl − ms`, where `E_ibl = f0·scale + bias`
/// is the split-sum specular and `ms` its multiple scattering (`multi_scatter_ems` in
/// `pbr_lighting.wgsl`, when `multi_scatter`). Mirrors `pbr_brdf.wgsl::compensated_env_diffuse`.
#[must_use]
pub fn compensated_env_diffuse(f0: f32, s: DfgSample, multi_scatter: bool) -> f32 {
    let ess = (s[0] + s[1]).clamp(0.0, 1.0);
    let fss = f0 * s[0] + s[1];
    let ms = if multi_scatter {
        let favg = f0 + (1.0 - f0) / 21.0;
        let ems = 1.0 - ess;
        fss * favg * ems / (1.0 - favg * ems).max(1e-4)
    } else {
        0.0
    };
    (1.0 - fss - ms).clamp(0.0, 1.0)
}

/// [`compensated_env_diffuse`] where only part of the environment's specular is added beside the
/// diffuse: `spec_cover` is the share of the diffuse environment light whose split-sum specular the
/// shader adds, `ms_cover` the share whose multiple scattering it adds (both clamped to 0..=1). The
/// diffuse gives up only what a specular term returns, so a flat ambient with no reflection (cover
/// 0) keeps the full diffuse. Mirrors `pbr_brdf.wgsl::compensated_env_diffuse_covered`.
#[must_use]
pub fn compensated_env_diffuse_covered(f0: f32, s: DfgSample, spec_cover: f32, ms_cover: f32) -> f32 {
    let ess = (s[0] + s[1]).clamp(0.0, 1.0);
    let fss = f0 * s[0] + s[1];
    let favg = f0 + (1.0 - f0) / 21.0;
    let ems = 1.0 - ess;
    let ms = fss * favg * ems / (1.0 - favg * ems).max(1e-4);
    let sc = spec_cover.clamp(0.0, 1.0);
    let mc = ms_cover.clamp(0.0, 1.0);
    (1.0 - fss * sc - ms * mc).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_the_default_is_legacy() {
        for mode in BrdfEnergy::ALL {
            assert_eq!(BrdfEnergy::from_name(mode.name()), Some(mode));
        }
        assert_eq!(
            BrdfEnergy::from_name(" Compensated\n"),
            Some(BrdfEnergy::Compensated)
        );
        assert_eq!(BrdfEnergy::from_name("kulla"), None);
        assert_eq!(BrdfEnergy::default(), BrdfEnergy::Legacy);
        assert_eq!(BrdfEnergy::Legacy.shader_code(), 0);
        assert_eq!(BrdfEnergy::Compensated.shader_code(), 1);
    }

    #[test]
    fn legacy_adds_no_define_and_compensated_adds_the_declared_axis() {
        let mut defs = HashMap::new();
        BrdfEnergy::Legacy.add_forward_def(&mut defs);
        assert!(defs.is_empty(), "a legacy module composes what it always did");
        BrdfEnergy::Compensated.add_forward_def(&mut defs);
        let names: Vec<&String> = defs.keys().collect();
        assert_eq!(names, [BRDF_ENERGY_COMPENSATED_DEF]);
        assert!(
            crate::forward_permutations::FORWARD_AXES
                .iter()
                .any(|axis| axis.name == BRDF_ENERGY_COMPENSATED_DEF)
        );
    }

    /// The image-based diffuse weight closes the split-sum furnace exactly: a white dielectric
    /// under a uniform environment returns `weight + E_ibl + ms = 1`, never more.
    #[test]
    fn the_environment_weight_closes_the_split_sum_furnace() {
        let lut = crate::build_brdf_lut();
        let n = lut.width as usize;
        for y in 0..n {
            for x in 0..n {
                let i = (y * n + x) * 4;
                let s: DfgSample = std::array::from_fn(|k| f32::from(lut.rgba[i + k]) / 255.0);
                for ms in [false, true] {
                    for f0 in [0.02f32, 0.04, 0.08] {
                        let w = compensated_env_diffuse(f0, s, ms);
                        let ess = (s[0] + s[1]).clamp(0.0, 1.0);
                        let fss = f0 * s[0] + s[1];
                        let favg = f0 + (1.0 - f0) / 21.0;
                        let extra = if ms {
                            fss * favg * (1.0 - ess) / (1.0 - favg * (1.0 - ess))
                        } else {
                            0.0
                        };
                        let total = w + fss + extra;
                        assert!(total <= 1.0 + 1e-5, "x {x} y {y} ms {ms} f0 {f0}: {total}");
                        assert!((0.0..=1.0).contains(&w));
                    }
                }
            }
        }
    }

    /// The covered weight gives up only what a specular term returns: no reflection added (cover
    /// 0) keeps the whole diffuse, full cover is the split-sum weight, and every partial cover
    /// still closes the furnace (`weight + cover·E_ibl + cover·ms <= 1`) without going under the
    /// full-cover weight.
    #[test]
    fn the_covered_weight_returns_only_what_a_specular_term_adds() {
        let lut = crate::build_brdf_lut();
        let n = lut.width as usize;
        for y in 0..n {
            for x in 0..n {
                let i = (y * n + x) * 4;
                let s: DfgSample = std::array::from_fn(|k| f32::from(lut.rgba[i + k]) / 255.0);
                for f0 in [0.02f32, 0.04, 0.08, 1.0] {
                    assert_eq!(compensated_env_diffuse_covered(f0, s, 0.0, 0.0), 1.0);
                    let full = compensated_env_diffuse_covered(f0, s, 1.0, 1.0);
                    assert!(
                        (full - compensated_env_diffuse(f0, s, true)).abs() <= 1e-6,
                        "x {x} y {y}"
                    );
                    let ss = compensated_env_diffuse_covered(f0, s, 1.0, 0.0);
                    assert!(
                        (ss - compensated_env_diffuse(f0, s, false)).abs() <= 1e-6,
                        "x {x} y {y}"
                    );
                    let ess = (s[0] + s[1]).clamp(0.0, 1.0);
                    let fss = f0 * s[0] + s[1];
                    let favg = f0 + (1.0 - f0) / 21.0;
                    let ms = fss * favg * (1.0 - ess) / (1.0 - favg * (1.0 - ess));
                    for cover in [0.25f32, 0.5, 0.75] {
                        let w = compensated_env_diffuse_covered(f0, s, cover, cover);
                        assert!(w >= full - 1e-6, "x {x} y {y} cover {cover}");
                        if f0 < 1.0 {
                            let total = w + cover * (fss + ms);
                            assert!(total <= 1.0 + 1e-5, "x {x} y {y} f0 {f0} cover {cover}: {total}");
                        }
                    }
                }
            }
        }
        // Out-of-range covers clamp.
        let s: DfgSample = [0.6, 0.05, 0.8, 0.85];
        assert_eq!(
            compensated_env_diffuse_covered(0.04, s, 2.0, -1.0),
            compensated_env_diffuse_covered(0.04, s, 1.0, 0.0)
        );
    }

    /// Both terms are symmetric in the view and the light (Helmholtz reciprocity).
    #[test]
    fn the_terms_are_symmetric_in_the_two_directions() {
        let a: DfgSample = [0.6, 0.05, 0.8, 0.85];
        let b: DfgSample = [0.3, 0.1, 0.6, 0.85];
        for ms in [false, true] {
            for f0 in [0.04f32, 0.5, 1.0] {
                let ab = compensated_terms(f0, 0.5, a, b, ms);
                let ba = compensated_terms(f0, 0.5, b, a, ms);
                assert!(
                    (ab.0 - ba.0).abs() < 1e-7 && (ab.1 - ba.1).abs() < 1e-7,
                    "{ab:?} {ba:?}"
                );
            }
        }
    }

    /// The smooth limit of the bias average is Schlick's `F_avg − F0 = (1 − F0)/21` share.
    #[test]
    fn the_bias_average_has_its_smooth_limit() {
        assert!((fresnel_bias_average(0.0) - 1.0 / 21.0).abs() < 1e-3);
        assert_eq!(fresnel_bias_average(1.0), fresnel_bias_average(FIT_MAX_ROUGHNESS));
        for i in 0..=20 {
            let b = fresnel_bias_average(i as f32 / 20.0);
            assert!((0.0..0.05).contains(&b), "{b}");
        }
    }

    /// The shader's copy of the fit is this one, coefficient for coefficient.
    #[test]
    fn the_shader_fit_is_the_cpu_fit() {
        let wgsl = include_str!("../shaders/pbr_brdf.wgsl");
        let body = wgsl
            .split("fn fresnel_bias_average")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("pbr_brdf.wgsl has fresnel_bias_average");
        for c in BIAS_AVERAGE_FIT {
            let text = format!("{}", c.abs());
            assert!(
                body.contains(&text),
                "coefficient {c} missing from the shader fit:\n{body}"
            );
        }
        assert!(
            body.contains("0.984375"),
            "the shader clamps at the last row centre"
        );
    }

    #[test]
    fn a_white_conductor_has_no_diffuse_and_full_multi_scatter_fresnel() {
        assert!((multi_scatter_fresnel(1.0, 0.7) - 1.0).abs() < 1e-6);
        let s: DfgSample = [0.5, 0.1, 0.6, 0.7];
        let (ms, _) = compensated_terms(1.0, 0.5, s, s, true);
        // f_ms = (1 − E)² / (π (1 − E_avg + margin)) for F_add = 1.
        let want = 0.4 * 0.4 / (std::f32::consts::PI * (0.3 + MS_NORMALIZATION_MARGIN));
        assert!((ms - want).abs() < 1e-6, "{ms} vs {want}");
        let (none, _) = compensated_terms(1.0, 0.5, s, s, false);
        assert_eq!(none, 0.0);
    }

    /// Every forward module the renderer composes takes the define: the lab shader (CPU and
    /// GPU-driven paths, thin film, subsurface, the Adreno workaround, glass), the visibility-buffer
    /// resolve built from it and every lite tier. Each composes, passes naga's validator and
    /// `spirv-val` (the shader gate's checks), and differs from its legacy module.
    #[test]
    fn every_forward_module_composes_and_validates_with_the_compensated_define() {
        let lab = include_str!("../shaders/lab_forward.wgsl");
        let mut cases: Vec<(&str, &str, Vec<&str>)> = vec![
            ("lab_forward.wgsl", lab, vec![]),
            ("lab_forward.wgsl", lab, vec!["ALPHA_TEST", "skip_srgb_encode"]),
            ("lab_forward.wgsl", lab, vec!["GPU_DRIVEN", "skip_srgb_encode"]),
            ("lab_forward.wgsl", lab, vec!["THIN_FILM", "ALPHA_TEST"]),
            (
                "lab_forward.wgsl",
                lab,
                vec!["SUBSURFACE", "ALPHA_TEST", "skip_srgb_encode"],
            ),
            (
                "lab_forward.wgsl",
                lab,
                vec!["SUN_SHADOW_PCF_ONLY", "STUDIO_IBL_OFF", "ALPHA_TEST"],
            ),
            ("lab_forward.wgsl", lab, vec!["GLASS", "ALPHA_TEST"]),
            (
                "vis_resolve.wgsl",
                crate::visbuffer::gpu::resolve_source(),
                vec!["VISBUFFER"],
            ),
        ];
        let lite = include_str!("../shaders/forward_mobile.wgsl");
        let mut lite_sets = Vec::new();
        for tier in [
            crate::forward_lite::ForwardLite::Low,
            crate::forward_lite::ForwardLite::High,
        ] {
            for weather in [false, true] {
                lite_sets.push(crate::forward_lite::lite_defs(tier, true, true, weather));
            }
            let mut lit = crate::forward_lite::lite_defs(tier, false, true, false);
            lit.insert("LIGHTMAP".to_string(), ShaderDefValue::Bool(true));
            lite_sets.push(lit);
        }
        cases.push(("lab_forward.wgsl", lab, vec!["MASK_DECALS", "ALPHA_TEST"]));
        let to_defs = |names: &[&str]| -> HashMap<String, ShaderDefValue> {
            names
                .iter()
                .map(|n| ((*n).to_string(), ShaderDefValue::Bool(true)))
                .collect()
        };
        let mut all: Vec<(&str, &str, HashMap<String, ShaderDefValue>)> = cases
            .into_iter()
            .map(|(file, source, names)| (file, source, to_defs(&names)))
            .collect();
        for defs in lite_sets {
            all.push((crate::forward_lite::FORWARD_LITE_SHADER_FILE, lite, defs));
        }
        let mut problems = Vec::new();
        for (file, source, legacy_defs) in all {
            let mut defs = legacy_defs.clone();
            BrdfEnergy::Compensated.add_forward_def(&mut defs);
            let module = match crate::compose_source(source, file, defs.clone()) {
                Ok(module) => module,
                Err(err) => {
                    problems.push(format!("{file} {defs:?}: {err}"));
                    continue;
                }
            };
            if let Err(err) = crate::shader_check::spirv_validate(file, &module) {
                problems.push(format!("{file} {defs:?}: {err}"));
            }
            let legacy = crate::compose_source(source, file, legacy_defs).expect("legacy module composes");
            let count = |m: &naga::Module| -> usize {
                m.entry_points
                    .iter()
                    .map(|e| e.function.expressions.len())
                    .sum::<usize>()
                    + m.functions
                        .iter()
                        .map(|(_, f)| f.expressions.len())
                        .sum::<usize>()
            };
            assert_ne!(
                count(&module),
                count(&legacy),
                "{file}: the define changed nothing"
            );
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
