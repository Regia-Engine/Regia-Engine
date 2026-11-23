// SPDX-License-Identifier: MPL-2.0
//! The forward shader's closed set of feature axes and the permutations a process composed
//! (ADR-1552; `docs/master-plan.md` § 5.5a item 4: the `#import` split of `lab_forward.wgsl`
//! names its feature axes, so the permutation enumeration has a closed set).
//!
//! [`FORWARD_AXES`] lists every shader define `lab_forward.wgsl` and its `regia::forward::*`
//! modules test with `#ifdef` / `#ifndef` / `#if`; a test keeps the list and the sources equal, so
//! a new axis cannot appear without being declared here. Every composition of the forward shader
//! records its define set; [`forward_permutation_report`] lists the distinct sets composed so far
//! in this process, flags defines outside the axes, and compares the count with
//! [`FORWARD_PERMUTATION_BUDGET`]. `render.graph` publishes the report.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, PoisonError};

use naga_oil::compose::ShaderDefValue;

/// One shader-define axis of the forward shader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardAxis {
    /// The define name.
    pub name: &'static str,
    /// What turning it on changes.
    pub what: &'static str,
}

/// Every feature axis of the forward shader, in no particular order.
pub const FORWARD_AXES: &[ForwardAxis] = &[
    ForwardAxis {
        name: "ALPHA_TEST",
        what: "masked materials discard below their alpha cutoff",
    },
    ForwardAxis {
        name: "GLASS",
        what: "glass instances: pane shading, damage and the front layer",
    },
    ForwardAxis {
        name: "MASK_DECALS",
        what: "per-instance mask recolor and UV decal slots (group 3)",
    },
    ForwardAxis {
        name: "TEXTURE_TABLE",
        what: "material textures from the bindless texture table (ADR-863)",
    },
    ForwardAxis {
        name: "MATERIAL_SSBO",
        what: "material constants from the storage arena instead of a uniform",
    },
    ForwardAxis {
        name: "MATERIAL_GRAPH",
        what: "a material graph's generated `material_surface`",
    },
    ForwardAxis {
        name: "MATERIAL_GRAPH_VERTEX",
        what: "a material graph with a vertex stage (offsets)",
    },
    ForwardAxis {
        name: "MATERIAL_GRAPH_UV1",
        what: "a material graph that reads the second UV set",
    },
    ForwardAxis {
        name: "GPU_DRIVEN",
        what: "object data from the GPU-driven object buffer",
    },
    ForwardAxis {
        name: "GPU_INSTANCED",
        what: "GPU-driven instanced batches",
    },
    ForwardAxis {
        name: "VISBUFFER",
        what: "shading entry used by the visibility-buffer resolve",
    },
    ForwardAxis {
        name: "VIS_SINGLE_PASS",
        what: "one visibility resolve over every material: the whole arena, explicit gradients (ADR-2071)",
    },
    ForwardAxis {
        name: "RESIDENCY_FEEDBACK",
        what: "texel-need feedback stores for texture streaming (ADR-1110)",
    },
    ForwardAxis {
        name: "SUN_SHADOW_RT",
        what: "ray-traced sun shadow (hardware ray tracing variant)",
    },
    ForwardAxis {
        name: "SUN_SHADOW_PCF_ONLY",
        what: "sun shadow without the near tile and PCSS: the Adreno driver workaround, set once per process (ADR-1749)",
    },
    ForwardAxis {
        name: "STUDIO_IBL_OFF",
        what: "no image-light ground plane, local captures or image-light debug views: the frame-level studio axis of a frame without a studio feature (ADR-2308), and always with SUN_SHADOW_PCF_ONLY on Adreno, whose driver fails to link the full shader with them (ADR-2042)",
    },
    ForwardAxis {
        name: "skip_srgb_encode",
        what: "the visibility-buffer resolve leaves the sRGB encode to its target",
    },
    ForwardAxis {
        name: "LIGHTMAP",
        what: "baked lightmap receivers: the UV2 stream and the lightmap lookup (group 3, ADR-1623)",
    },
    ForwardAxis {
        name: "SUBSURFACE",
        what: "screen-space subsurface scattering: the diffuse split and the two extra targets (ADR-2049)",
    },
    ForwardAxis {
        name: "OIT",
        what: "weighted blended transparency: the accumulation and revealage targets (ADR-2065)",
    },
    ForwardAxis {
        name: "THIN_FILM",
        what: "thin-film interference over the base specular: its Fresnel and map (group 3, ADR-1977)",
    },
    ForwardAxis {
        name: "FORWARD_LITE_LOW",
        what: "the lite shader's Mobile Low tier: one sun cascade, 4 local lights (ADR-1874)",
    },
    ForwardAxis {
        name: "FORWARD_LITE_HIGH",
        what: "the lite shader's Mobile High tier: two sun cascades, 8 local lights, local shadows (ADR-1874)",
    },
    ForwardAxis {
        name: "LITE_SURFACE_WEATHER",
        what: "the lite shader's surface weather and precipitation extinction, wet frames only (spec 99, ADR-2032)",
    },
    ForwardAxis {
        name: "RESTIR_DI",
        what: "stochastic many-light sampling: the opaque pass shades the pixel's reservoir light instead of the list's points and spots (group 0 binding 37, ADR-2652)",
    },
    ForwardAxis {
        name: "FRAME_BLUE_NOISE",
        what: "a device whose view layout has the blue-noise table (group 0 binding 38): shadow rotation, hashed alpha and pane SSR read it on frames that ask (ADR-2652)",
    },
    ForwardAxis {
        name: "FORWARD_TRIM",
        what: "no fitted-emitter or LTC sphere light, surface weather, aerial haze in the weather in-scatter or pane reflection test: always with SUN_SHADOW_PCF_ONLY on Adreno, whose compiler crashes or fails to link the full shader with them (ADR-3145)",
    },
    ForwardAxis {
        name: "BRDF_ENERGY_COMPENSATED",
        what: "the direct BRDF's Kulla-Conty energy compensation and diffuse coupling, and the matching image-light diffuse weight (BrdfEnergy::Compensated, ADR-2632)",
    },
    ForwardAxis {
        name: "FRAME_FORWARD_DECALS",
        what: "a device whose view layout has the forward-decal storage buffer (group 0 binding 39: decals, tile masks, atlas): the surface gathers its tile's decals before lighting (process switch, ADR-3255)",
    },
];

/// Most distinct forward permutations one process should compose. A provisional ceiling until
/// the cook report counts permutations per profile (master plan § 5.3 item 13); the report flags
/// a process above it, nothing is refused.
pub const FORWARD_PERMUTATION_BUDGET: usize = 256;

/// The forward shader's file name as the composer sees it.
pub const FORWARD_SHADER_FILE: &str = "lab_forward.wgsl";

/// The forward shader and its modules, `(file, source)`.
pub const FORWARD_SOURCES: [(&str, &str); 9] = [
    (FORWARD_SHADER_FILE, include_str!("../shaders/lab_forward.wgsl")),
    ("forward_ltc.wgsl", include_str!("../shaders/forward_ltc.wgsl")),
    (
        "forward_occluder_geometry.wgsl",
        include_str!("../shaders/forward_occluder_geometry.wgsl"),
    ),
    (
        "forward_occlusion.wgsl",
        include_str!("../shaders/forward_occlusion.wgsl"),
    ),
    (
        "forward_env_ground.wgsl",
        include_str!("../shaders/forward_env_ground.wgsl"),
    ),
    ("forward_util.wgsl", include_str!("../shaders/forward_util.wgsl")),
    (
        "forward_surface_weather.wgsl",
        include_str!("../shaders/forward_surface_weather.wgsl"),
    ),
    (
        crate::forward_lite::FORWARD_LITE_SHADER_FILE,
        include_str!("../shaders/forward_mobile.wgsl"),
    ),
    (
        "forward_decals.wgsl",
        include_str!("../shaders/forward_decals.wgsl"),
    ),
];

static COMPOSED: Mutex<BTreeSet<Vec<String>>> = Mutex::new(BTreeSet::new());

/// Records the define set of one composition of `file_path` when it is the forward shader.
/// Called by the composer (pipeline creation, never per frame).
pub(crate) fn note_compose(file_path: &str, defs: &HashMap<String, ShaderDefValue>) {
    if file_path != FORWARD_SHADER_FILE && file_path != crate::forward_lite::FORWARD_LITE_SHADER_FILE {
        return;
    }
    let mut key: Vec<String> = defs
        .iter()
        .filter(|(_, value)| !matches!(value, ShaderDefValue::Bool(false)))
        .map(|(name, value)| match value {
            ShaderDefValue::Bool(_) => name.clone(),
            ShaderDefValue::Int(v) => format!("{name}={v}"),
            ShaderDefValue::UInt(v) => format!("{name}={v}"),
        })
        .collect();
    key.sort();
    COMPOSED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key);
}

/// [`forward_permutation_report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardPermutationReport {
    /// The distinct define sets composed so far in this process, each sorted, the set sorted.
    pub permutations: Vec<Vec<String>>,
    /// Defines composed that are not in [`FORWARD_AXES`] (should stay empty).
    pub undeclared: Vec<String>,
    /// [`FORWARD_PERMUTATION_BUDGET`].
    pub budget: usize,
    /// More permutations than the budget were composed.
    pub over_budget: bool,
}

/// The forward permutations this process composed, checked against the axes and the budget.
#[must_use]
pub fn forward_permutation_report() -> ForwardPermutationReport {
    let permutations: Vec<Vec<String>> = COMPOSED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect();
    let undeclared: BTreeSet<String> = permutations
        .iter()
        .flatten()
        .map(|define| define.split('=').next().unwrap_or(define).to_string())
        .filter(|name| !FORWARD_AXES.iter().any(|axis| axis.name == name))
        .collect();
    ForwardPermutationReport {
        over_budget: permutations.len() > FORWARD_PERMUTATION_BUDGET,
        permutations,
        undeclared: undeclared.into_iter().collect(),
        budget: FORWARD_PERMUTATION_BUDGET,
    }
}

/// The sorted define set the forward shader composes for a material graph with a vertex stage
/// (`vertex`) and/or a read of the second UV set (`uv1`): the graph axes of
/// [`FORWARD_AXES`] (`MATERIAL_GRAPH`, `MATERIAL_GRAPH_VERTEX`, `MATERIAL_GRAPH_UV1`). The cook
/// report counts the distinct sets over a project's cooked graphs with it.
#[must_use]
pub fn graph_permutation_defines(vertex: bool, uv1: bool) -> Vec<String> {
    let mut defs = vec!["MATERIAL_GRAPH".to_string()];
    if vertex {
        defs.push("MATERIAL_GRAPH_VERTEX".to_string());
    }
    if uv1 {
        defs.push("MATERIAL_GRAPH_UV1".to_string());
    }
    defs.sort();
    defs
}

/// How a frame renderer picks the forward shader's studio axis (ADR-2308).
///
/// The studio image-light extras (the ground plane's cover and reflection, the local reflection
/// captures and the image-light debug views) cost a frame that never uses them about
/// 0.03-0.05 ms of mesh forward on an RTX 5090 (ADR-2087 (4)). Under [`Self::Auto`] a frame with
/// none of them on ([`StudioFeatures::any`]) composes the forward shader without them
/// (`STUDIO_IBL_OFF`); its pixels are those of the full shader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ForwardStudioMode {
    /// The full shader only while a studio feature is on (the default).
    #[default]
    Auto,
    /// The full shader on every frame, as before ADR-2308: an A/B baseline.
    Full,
}

impl ForwardStudioMode {
    /// Every mode, in declaration order.
    pub const ALL: [Self; 2] = [Self::Auto, Self::Full];

    /// The protocol name: `auto` or `full`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }

    /// The mode named `name` ([`Self::name`]), if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.name() == name.trim())
    }
}

/// The environment variable that forces [`ForwardStudioMode::Full`] on every renderer of the
/// process (`full`); any other value leaves each renderer's own mode. For A/B runs of one binary.
pub const FORWARD_STUDIO_ENV: &str = "REGIA_FORWARD_STUDIO";

/// Whether `value` (of [`FORWARD_STUDIO_ENV`]) forces the full shader.
#[must_use]
pub fn forward_studio_env_forces_full(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("full"))
}

/// The studio features a frame renderer has on: what the full forward shader draws and the
/// studio-off module leaves out (ADR-2308).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StudioFeatures {
    /// The image light's ground plane is set and the installed light feeds it.
    pub ground_plane: bool,
    /// Local reflection captures installed.
    pub captures: u32,
    /// The image-light debug view: 0 the lit frame, 1 the image light alone, 2 its occlusion.
    pub image_light_view: u32,
}

impl StudioFeatures {
    /// Any studio feature is on: the frame composes the full forward shader.
    #[must_use]
    pub fn any(&self) -> bool {
        self.ground_plane || self.captures > 0 || self.image_light_view != 0
    }
}

/// The forward studio axis of one frame renderer (`FrameRenderer::forward_studio_report`,
/// `render.forward_studio`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardStudioReport {
    /// The renderer's mode.
    pub mode: ForwardStudioMode,
    /// [`FORWARD_STUDIO_ENV`] forces the full shader in this process.
    pub env_full: bool,
    /// The Adreno workaround composes every forward module without the studio extras
    /// (`STUDIO_IBL_OFF` with `SUN_SHADOW_PCF_ONLY`, ADR-2042).
    pub adreno_workaround: bool,
    /// The studio features on now.
    pub features: StudioFeatures,
    /// The next frame composes the studio-off module.
    pub studio_off: bool,
    /// Per-object forward pipeline sets built with the studio-off module.
    pub sets_studio_off: usize,
    /// Per-object forward pipeline sets built with the full module.
    pub sets_full: usize,
    /// GPU-driven colour pipelines built with the studio-off module.
    pub gpu_color_studio_off: usize,
    /// GPU-driven colour pipelines built with the full module.
    pub gpu_color_full: usize,
}

/// Adds `STUDIO_IBL_OFF` to a forward module's `defs` when `studio_off` (the frame-level studio
/// axis, ADR-2308). A module without the bit composes what it always did.
pub(crate) fn add_studio_off_def(defs: &mut HashMap<String, ShaderDefValue>, studio_off: bool) {
    if studio_off {
        defs.insert(
            crate::shader_workarounds::STUDIO_IBL_OFF_DEF.to_string(),
            ShaderDefValue::Bool(true),
        );
    }
}

/// The opaque forward permutation that shades the pixel's many-light reservoir (ADR-2652).
pub const RESTIR_DI_DEF: &str = "RESTIR_DI";

/// Adds [`RESTIR_DI_DEF`] to a forward module's `defs` when `on` (the frame-level many-light
/// axis of an opaque pipeline, ADR-2652). A module without the bit composes what it always did.
pub(crate) fn add_restir_di_def(defs: &mut HashMap<String, ShaderDefValue>, on: bool) {
    if on {
        defs.insert(RESTIR_DI_DEF.to_string(), ShaderDefValue::Bool(true));
    }
}

/// The define names the forward sources test (`#ifdef`, `#ifndef`, `#if`), sorted.
#[must_use]
pub fn forward_source_axes() -> BTreeSet<&'static str> {
    let mut names = BTreeSet::new();
    for (_, source) in FORWARD_SOURCES {
        for line in source.lines() {
            let line = line.trim_start();
            let rest = ["#ifdef ", "#ifndef ", "#if "]
                .iter()
                .find_map(|directive| line.strip_prefix(directive));
            if let Some(rest) = rest
                && let Some(name) = rest.split_whitespace().next()
            {
                names.insert(name);
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_permutation_defines_are_declared_axes_and_distinct() {
        let all: Vec<Vec<String>> = [(false, false), (true, false), (false, true), (true, true)]
            .iter()
            .map(|&(v, u)| graph_permutation_defines(v, u))
            .collect();
        let distinct: BTreeSet<&Vec<String>> = all.iter().collect();
        assert_eq!(distinct.len(), 4);
        for define in all.iter().flatten() {
            assert!(FORWARD_AXES.iter().any(|a| a.name == define), "{define}");
        }
        assert_eq!(
            graph_permutation_defines(true, true),
            ["MATERIAL_GRAPH", "MATERIAL_GRAPH_UV1", "MATERIAL_GRAPH_VERTEX"]
        );
    }

    #[test]
    fn the_declared_axes_are_exactly_the_defines_the_sources_test() {
        let declared: BTreeSet<&str> = FORWARD_AXES.iter().map(|axis| axis.name).collect();
        assert_eq!(declared.len(), FORWARD_AXES.len(), "an axis is declared twice");
        assert_eq!(
            forward_source_axes(),
            declared,
            "a forward shader define is missing from FORWARD_AXES, or a declared axis is unused"
        );
    }

    #[test]
    fn the_studio_mode_names_round_trip_and_the_environment_forces_full_only() {
        for mode in ForwardStudioMode::ALL {
            assert_eq!(ForwardStudioMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(
            ForwardStudioMode::from_name(" full "),
            Some(ForwardStudioMode::Full)
        );
        assert_eq!(ForwardStudioMode::from_name("studio"), None);
        assert_eq!(ForwardStudioMode::default(), ForwardStudioMode::Auto);
        assert!(forward_studio_env_forces_full(Some("full")));
        assert!(forward_studio_env_forces_full(Some(" FULL\n")));
        assert!(!forward_studio_env_forces_full(Some("auto")));
        assert!(!forward_studio_env_forces_full(Some("1")));
        assert!(!forward_studio_env_forces_full(None));
    }

    #[test]
    fn the_studio_features_are_any_of_three_and_the_def_is_the_declared_axis() {
        assert!(!StudioFeatures::default().any());
        let on = [
            StudioFeatures {
                ground_plane: true,
                ..StudioFeatures::default()
            },
            StudioFeatures {
                captures: 1,
                ..StudioFeatures::default()
            },
            StudioFeatures {
                image_light_view: 2,
                ..StudioFeatures::default()
            },
        ];
        assert!(on.iter().all(StudioFeatures::any));
        let mut defs = HashMap::new();
        add_studio_off_def(&mut defs, false);
        assert!(
            defs.is_empty(),
            "a key without the bit composes what it always did"
        );
        add_studio_off_def(&mut defs, true);
        let names: Vec<&String> = defs.keys().collect();
        assert_eq!(names, ["STUDIO_IBL_OFF"]);
        assert!(FORWARD_AXES.iter().any(|axis| axis.name == "STUDIO_IBL_OFF"));
    }

    #[test]
    fn a_composition_is_recorded_once_per_define_set_and_only_for_the_forward_shader() {
        let defs = |names: &[&str]| -> HashMap<String, ShaderDefValue> {
            names
                .iter()
                .map(|n| ((*n).to_string(), ShaderDefValue::Bool(true)))
                .collect()
        };
        note_compose(FORWARD_SHADER_FILE, &defs(&["GLASS", "ALPHA_TEST"]));
        note_compose(FORWARD_SHADER_FILE, &defs(&["ALPHA_TEST", "GLASS"]));
        note_compose("lab_shadow.wgsl", &defs(&["NOT_AN_AXIS_SHADOW"]));
        let report = forward_permutation_report();
        let glass = vec!["ALPHA_TEST".to_string(), "GLASS".to_string()];
        assert_eq!(report.permutations.iter().filter(|p| **p == glass).count(), 1);
        assert!(!report.undeclared.iter().any(|d| d == "NOT_AN_AXIS_SHADOW"));
        assert_eq!(report.budget, FORWARD_PERMUTATION_BUDGET);
    }
}
