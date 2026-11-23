// SPDX-License-Identifier: MPL-2.0
//! The lite forward path of the Mobile tiers on tile GPUs (ADR-1874).
//!
//! `lab_forward.wgsl` costs a phone 0.15-0.39 us per pixel, spread over every lighting feature:
//! its register pressure is the cost, not any one term (ADR-1749). A frame whose profile asks for
//! [`ForwardLite::Low`] or [`ForwardLite::High`], on an adapter [`adapter_takes_lite`] accepts,
//! draws its opaque and masked standard materials with `shaders/forward_mobile.wgsl` instead.
//! Blended, transmissive, graph and mask-decal draws keep their own pipelines. A lightmap receiver
//! draws the lite shader composed with `LIGHTMAP` (ADR-2107, `frame/lightmap_paths.rs`).
//!
//! Everything else keeps the lab shader, so a desktop adapter draws the same pixels at every
//! preset, the Mobile rows included (the golden frames render those on lavapipe).
//!
//! `REGIA_FORWARD_LITE` (`auto`, `off`, `low`, `high`) overrides the choice for a process, and
//! [`crate::FrameRenderer::set_forward_lite`] overrides it for one renderer.

use std::sync::atomic::{AtomicU8, Ordering};

use naga_oil::compose::ShaderDefValue;
use std::collections::HashMap;

/// The file name of the lite forward shader as the composer sees it.
pub const FORWARD_LITE_SHADER_FILE: &str = "forward_mobile.wgsl";

/// The environment variable that overrides the choice for a process.
pub const FORWARD_LITE_ENV: &str = "REGIA_FORWARD_LITE";

/// Which forward fragment shader a frame's opaque and masked standard materials draw with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ForwardLite {
    /// The full forward shader (`lab_forward.wgsl`).
    #[default]
    Off,
    /// One sun cascade with one hardware 2x2 tap, 4 local lights, no local shadows.
    Low,
    /// Two sun cascades with four taps, 8 local lights, one tap per local shadow.
    High,
}

impl ForwardLite {
    /// Stable lower-case name (`"off"`, `"low"`, `"high"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::High => "high",
        }
    }

    /// Parses [`Self::name`] (case-insensitive).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Off, Self::Low, Self::High]
            .into_iter()
            .find(|tier| tier.name().eq_ignore_ascii_case(name.trim()))
    }

    /// The tier's shader define; `None` for [`Self::Off`].
    #[must_use]
    pub const fn shader_def(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Low => Some("FORWARD_LITE_LOW"),
            Self::High => Some("FORWARD_LITE_HIGH"),
        }
    }
}

/// How far the sun cascades reach at [`ForwardLite::Low`], metres. Its one 1024² cascade would
/// otherwise span the whole view, too coarse to resolve a crate on a 200 m view; here its texels
/// are a fifth of that size. Shadows fade out over the last 15 % of the range.
pub const LOW_SUN_RANGE_M: f32 = 40.0;

/// The longest reach of the sun cascades at `tier`, metres; infinite when the view decides.
#[must_use]
pub const fn sun_range_m(tier: ForwardLite) -> f32 {
    match tier {
        ForwardLite::Low => LOW_SUN_RANGE_M,
        ForwardLite::Off | ForwardLite::High => f32::INFINITY,
    }
}

/// PCI vendor id of Arm (Mali GPUs) as Vulkan reports it.
pub const ARM_VENDOR: u32 = 0x13B5;
/// PCI vendor id of Imagination Technologies (PowerVR GPUs) as Vulkan reports it.
pub const IMAGINATION_VENDOR: u32 = 0x1010;

/// Whether an adapter of `vendor` takes the lite path a profile asks for: every Android adapter,
/// and the tile GPU vendors anywhere else (a Snapdragon laptop on a Mobile preset). Desktop GPUs
/// and software rasterizers do not, so the Mobile rows keep their pixels there.
#[must_use]
pub const fn adapter_takes_lite(vendor: u32, android: bool) -> bool {
    android
        || matches!(
            vendor,
            crate::shader_workarounds::QUALCOMM_VENDOR | ARM_VENDOR | IMAGINATION_VENDOR
        )
}

/// The tier a frame draws: `renderer` (the [`crate::FrameRenderer::set_forward_lite`] override)
/// over `env` (a parsed [`FORWARD_LITE_ENV`]) over the profile's `asked` tier, which applies when
/// `adapter_ok`.
#[must_use]
pub const fn resolve_tier(
    asked: ForwardLite,
    adapter_ok: bool,
    env: Option<ForwardLite>,
    renderer: Option<ForwardLite>,
) -> ForwardLite {
    if let Some(tier) = renderer {
        return tier;
    }
    if let Some(tier) = env {
        return tier;
    }
    if adapter_ok { asked } else { ForwardLite::Off }
}

/// What [`note_adapter`] decided: bit 0 the adapter takes the lite path, bits 1-2 the environment
/// override (0 none, 1 off, 2 low, 3 high).
static ADAPTER: AtomicU8 = AtomicU8::new(0);

fn env_code(tier: Option<ForwardLite>) -> u8 {
    match tier {
        None => 0,
        Some(ForwardLite::Off) => 1,
        Some(ForwardLite::Low) => 2,
        Some(ForwardLite::High) => 3,
    }
}

fn env_from_bits(state: u8) -> Option<ForwardLite> {
    match state >> 1 {
        1 => Some(ForwardLite::Off),
        2 => Some(ForwardLite::Low),
        3 => Some(ForwardLite::High),
        _ => None,
    }
}

/// The `REGIA_FORWARD_LITE` tier `note_adapter` last stored, when the variable named `off`,
/// `low` or `high`. `None` when it was unset, `auto`, or empty.
#[must_use]
pub fn recorded_env() -> Option<ForwardLite> {
    env_from_bits(ADAPTER.load(Ordering::Relaxed))
}

/// Replaces the environment tier `note_adapter` stores and returns the previous one.
///
/// Bit 0 (whether the adapter takes the lite path) is left as it is. A test sets a tier without
/// a device; a real device records the variable in `note_adapter`. The value is process-global.
#[must_use]
pub fn set_recorded_env(env: Option<ForwardLite>) -> Option<ForwardLite> {
    let previous = recorded_env();
    let takes = ADAPTER.load(Ordering::Relaxed) & 1;
    ADAPTER.store(takes | (env_code(env) << 1), Ordering::Relaxed);
    previous
}

/// Parses a [`FORWARD_LITE_ENV`] value: a tier name, or `None` for `auto`, empty and garbage.
#[must_use]
pub fn parse_env(value: Option<&str>) -> Option<ForwardLite> {
    value.and_then(ForwardLite::from_name)
}

/// Records the adapter the render device picked.
pub(crate) fn note_adapter(info: &wgpu::AdapterInfo) {
    let env = regia_core::brand::env_var_os(FORWARD_LITE_ENV);
    let env = parse_env(env.as_ref().and_then(|v| v.to_str()));
    let takes = adapter_takes_lite(info.vendor, cfg!(target_os = "android"));
    ADAPTER.store(u8::from(takes) | (env_code(env) << 1), Ordering::Relaxed);
}

/// [`resolve_tier`] with this process's adapter and environment.
#[must_use]
pub fn frame_tier(asked: ForwardLite, renderer: Option<ForwardLite>) -> ForwardLite {
    let state = ADAPTER.load(Ordering::Relaxed);
    resolve_tier(asked, state & 1 != 0, env_from_bits(state), renderer)
}

/// The lite shader's surface-weather define (spec 99 § 6.5): wetness, puddles, ripples, snow and
/// the precipitation's extinction. Composed only for a frame that is wet, snowy or has an
/// extinction, so a dry frame draws with the pipelines it always had (ADR-2032).
pub const LITE_SURFACE_WEATHER_DEF: &str = "LITE_SURFACE_WEATHER";

/// The defines of one lite composition: the tier, `alpha_test` (a `discard` for masks and LOD
/// cross-fades; opaque draws go without, which keeps a tile GPU's hidden-surface removal),
/// `encode` (a non-sRGB target; `false` adds `skip_srgb_encode`) and `weather`
/// ([`LITE_SURFACE_WEATHER_DEF`]). Bounded: 16 sets per device, 8 of them only on wet frames.
pub(crate) fn lite_defs(
    tier: ForwardLite,
    alpha_test: bool,
    encode: bool,
    weather: bool,
) -> HashMap<String, ShaderDefValue> {
    let mut defs = HashMap::new();
    if weather {
        defs.insert(LITE_SURFACE_WEATHER_DEF.to_string(), ShaderDefValue::Bool(true));
    }
    if let Some(def) = tier.shader_def() {
        defs.insert(def.to_string(), ShaderDefValue::Bool(true));
    }
    if alpha_test {
        defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    }
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    const LITE: &str = include_str!("../shaders/forward_mobile.wgsl");
    const LAB: &str = include_str!("../shaders/lab_forward.wgsl");

    /// The fields of `struct name` in `source`, comments and blank lines dropped.
    fn block(source: &str, name: &str) -> Vec<String> {
        let start = source
            .find(&format!("struct {name} {{"))
            .unwrap_or_else(|| panic!("struct {name} not found"));
        source[start..]
            .lines()
            .skip(1)
            .take_while(|line| !line.starts_with('}'))
            .map(|line| line.split("//").next().unwrap_or_default().trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    #[test]
    fn the_lite_blocks_are_the_lab_blocks() {
        for name in [
            "LabFrame",
            "GpuLight",
            "DirectionalLight",
            "LabMaterial",
            "ShadowUniform",
            "ShadowUniforms",
            "ShadowTile",
        ] {
            assert_eq!(block(LITE, name), block(LAB, name), "struct {name} drifted");
        }
    }

    /// The body of `fn name(` in `source`, from its signature to the first closing brace in
    /// column 0, comments and blank lines dropped.
    fn function(source: &str, name: &str) -> Vec<String> {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("fn {name} not found"));
        source[start..]
            .lines()
            .skip(1)
            .take_while(|line| !line.starts_with('}'))
            .map(|line| line.split("//").next().unwrap_or_default().trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// The lite weather variant's copies of the lab shader's helpers (spec 99 § 6.5, ADR-2032)
    /// are the lab's text, so a change to how the response is packed or the rain in-scatters
    /// reaches phones too.
    #[test]
    fn the_lite_weather_helpers_are_the_lab_helpers() {
        for (lite, lab) in [
            ("lite_weather_response", "weather_response"),
            ("lite_weather_inscatter", "weather_inscatter"),
        ] {
            assert_eq!(
                function(LITE, lite),
                function(LAB, lab),
                "fn {lite} drifted from fn {lab}"
            );
        }
    }

    #[test]
    fn the_lite_varyings_are_a_subset_of_the_lab_vertex_output() {
        let lab = block(LAB, "VertexOutput");
        for field in block(LITE, "VertexOutput") {
            assert!(lab.contains(&field), "{field} is not a lab varying");
        }
    }

    #[test]
    fn every_lite_binding_is_declared_as_in_the_lab_shader() {
        for line in LITE.lines().filter(|line| line.starts_with("@group(")) {
            assert!(
                LAB.lines().any(|lab| lab.trim() == line),
                "{line} differs from the lab shader's binding"
            );
        }
    }

    /// Worst errors (radians) of the fits, measured on the 20 001 samples of the test below.
    const ACOS_FIT_ERROR: f64 = 0.0095;
    const ATAN_FIT_ERROR: f64 = 0.004;

    #[test]
    fn the_fast_equirect_fits_stay_inside_their_error() {
        // The two fits of `fast_equirect_uv` in `forward_mobile.wgsl`, over their whole range.
        let (mut worst_acos, mut worst_atan) = (0.0_f64, 0.0_f64);
        for i in 0..=20_000 {
            let x = f64::from(i) / 20_000.0;
            let fit_acos = (std::f64::consts::FRAC_PI_2 - 0.156_583 * x) * (1.0 - x).sqrt();
            let fit_atan = x * (std::f64::consts::FRAC_PI_4 + 0.273 * (1.0 - x));
            worst_acos = worst_acos.max((fit_acos - x.acos()).abs());
            worst_atan = worst_atan.max((fit_atan - x.atan()).abs());
        }
        assert!(worst_acos < ACOS_FIT_ERROR, "acos fit: {worst_acos}");
        assert!(worst_atan < ATAN_FIT_ERROR, "atan fit: {worst_atan}");
    }

    #[test]
    fn a_desktop_adapter_keeps_the_full_shader_whatever_the_profile_asks() {
        const NVIDIA: u32 = 0x10DE;
        const LAVAPIPE: u32 = 0x1_0005;
        for vendor in [NVIDIA, LAVAPIPE, 0x1002, 0x8086] {
            assert!(!adapter_takes_lite(vendor, false));
            assert_eq!(
                resolve_tier(ForwardLite::Low, false, None, None),
                ForwardLite::Off
            );
        }
        for vendor in [
            crate::shader_workarounds::QUALCOMM_VENDOR,
            ARM_VENDOR,
            IMAGINATION_VENDOR,
        ] {
            assert!(adapter_takes_lite(vendor, false));
        }
        assert!(adapter_takes_lite(0x144D, true), "every Android adapter");
    }

    #[test]
    fn the_renderer_override_beats_the_environment_and_the_environment_beats_the_profile() {
        use ForwardLite::{High, Low, Off};
        assert_eq!(resolve_tier(High, true, None, None), High);
        assert_eq!(
            resolve_tier(Off, true, None, None),
            Off,
            "a desktop row stays off"
        );
        assert_eq!(resolve_tier(High, true, Some(Off), None), Off);
        assert_eq!(resolve_tier(Off, false, Some(Low), None), Low);
        assert_eq!(resolve_tier(High, true, Some(Off), Some(Low)), Low);
        assert_eq!(resolve_tier(Low, true, Some(High), Some(Off)), Off);
    }

    #[test]
    fn only_the_low_tier_shortens_the_sun_range() {
        assert_eq!(sun_range_m(ForwardLite::Low), LOW_SUN_RANGE_M);
        assert!(
            sun_range_m(ForwardLite::Off).is_infinite(),
            "a desktop frame keeps its cascades"
        );
        assert!(sun_range_m(ForwardLite::High).is_infinite());
    }

    #[test]
    fn names_round_trip_and_auto_or_garbage_leave_the_choice_alone() {
        for tier in [ForwardLite::Off, ForwardLite::Low, ForwardLite::High] {
            assert_eq!(ForwardLite::from_name(tier.name()), Some(tier));
        }
        assert_eq!(parse_env(Some(" HIGH ")), Some(ForwardLite::High));
        assert_eq!(parse_env(Some("auto")), None);
        assert_eq!(parse_env(Some("maybe")), None);
        assert_eq!(parse_env(None), None);
    }

    #[test]
    fn a_composition_set_names_one_tier_and_only_declared_axes() {
        let mut seen = std::collections::BTreeSet::new();
        for tier in [ForwardLite::Low, ForwardLite::High] {
            for alpha_test in [false, true] {
                for encode in [false, true] {
                    for weather in [false, true] {
                        let defs = lite_defs(tier, alpha_test, encode, weather);
                        assert!(defs.keys().any(|k| k.starts_with("FORWARD_LITE_")));
                        assert_eq!(defs.contains_key(LITE_SURFACE_WEATHER_DEF), weather);
                        for name in defs.keys() {
                            assert!(
                                crate::forward_permutations::FORWARD_AXES
                                    .iter()
                                    .any(|axis| axis.name == name),
                                "{name} is not a forward axis"
                            );
                        }
                        let mut key: Vec<String> = defs.keys().cloned().collect();
                        key.sort();
                        seen.insert(key);
                    }
                }
            }
        }
        assert_eq!(
            seen.len(),
            16,
            "the lite set is exactly tier x alpha test x encode x weather"
        );
        // A dry frame's sets are the ones the lite path always composed.
        assert_eq!(
            lite_defs(ForwardLite::High, true, false, false).len(),
            3,
            "tier, alpha test, skip_srgb_encode"
        );
    }
}
