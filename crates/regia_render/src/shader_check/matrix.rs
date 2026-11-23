// SPDX-License-Identifier: MPL-2.0
//! The shader permutation matrix (ADR-2644): every module a shipped preset can compose, on every
//! backend a shipped platform translates it for.
//!
//! [`super::validate_engine_shaders`] creates the modules of a fixed list and the pipelines of a
//! Desktop High and a Mobile High frame. A permutation outside that list (a preset's lite tier, a
//! studio-off forward module with thin film, a material graph's instanced depth pass on the
//! table path) compiled first when a game drew it. The matrix lists them all:
//!
//! - the engine modules of [`super::shader_sources`];
//! - per platform of [`SHIPPED_PLATFORMS`] and preset of [`platform_presets`], the forward
//!   permutations the preset's profile can compose ([`forward_permutations_for`]): the lite or
//!   the full shader, the CPU and GPU-driven paths, display-encoded and linear targets, the studio
//!   axis (`STUDIO_IBL_OFF`, ADR-2308), the material binding tiers (`MATERIAL_SSBO`,
//!   `TEXTURE_TABLE`), the opt-in feature axes (glass, mask decals, thin film, lightmaps,
//!   subsurface, OIT), the traced sun on desktop and the Adreno workaround on Android;
//! - per preset, every graph pipeline set [`enumerate_profile`] lists for the sample material
//!   graphs, one module per pass ([`graph_pass_module`]).
//!
//! Inputs with the same file, source and defines are composed once. Composed modules are written
//! back to WGSL (naga's writer) and hashed; identical modules from different define sets (a
//! define the module never tests) are checked once and listed together. Each distinct module
//! then runs the backends of [`MatrixBackend`]: naga validation, the explicit-layout scan,
//! `spirv-val`, a pipeline per entry point on a validation-enabled device (lavapipe on CI),
//! naga's HLSL writer for Shader Model 6.0 and 5.1 with the FXC rules, and naga's MSL writer
//! with wgpu's Metal options ([`crate::shader_msl_check`]). [`ShaderMatrixReport`] is the result,
//! `cargo xtask check-shaders --matrix [--json]` prints it.
//!
//! The pipeline backend builds its own layouts from the module (it cannot borrow the frame's):
//! one bind group layout per group, each binding visible to the stages that use it; vertex inputs
//! in one interleaved buffer; one colour target per fragment output in the smallest format the
//! output covers; a `Depth32Float` attachment. It proves the driver accepts the module and its
//! interface, not that the frame's own layouts match it: the per-preset frames
//! ([`validate_preset_frames`]) cover those.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::num::NonZeroU32;
use std::sync::Mutex;

use naga_oil::compose::ShaderDefValue;
use serde::{Deserialize, Serialize};

use super::{LINES, install_logger, layout_scan_words, spirv_val_words, validation_hits, write_spirv};
use crate::permutation_policy::{
    CompileCostModel, GraphPassModule, GraphPermutationInput, PermutationBudget, SHIPPED_PLATFORMS,
    enumerate_profile, graph_pass_module, platform_presets,
};
use crate::shader_lib::compose_source;
use crate::surface_graph::{
    GraphCode, SAMPLE_SURFACE, SAMPLE_VERTEX, code_defs, forward_source_with_code, vertex_host_defs,
    vertex_host_source,
};
use crate::{RenderDevice, RenderError, RenderProfile, RenderProfilePreset};

/// Version of [`ShaderMatrixReport`]'s JSON shape (`check-shaders --json`). A tool report, not a
/// content format: bump it when a field changes meaning or goes away.
pub const SHADER_MATRIX_REPORT_VERSION: u32 = 1;

/// `TEXTURE_TABLE_SIZE` the matrix composes the table path with. The runtime takes it from the
/// device ([`crate::texture_table`]); the size only changes an array length.
pub const MATRIX_TEXTURE_TABLE_SIZE: u32 = 1024;

/// HLSL that needs Shader Model 6 (wave and quad intrinsics, 16- and 64-bit scalar types). naga
/// writes these at any shader model; FXC (5.1) fails on them.
pub(crate) const FXC_REJECTS: [&str; 13] = [
    "WaveActive",
    "WaveGetLane",
    "WaveIsFirstLane",
    "WavePrefix",
    "WaveReadLane",
    "WaveMultiPrefix",
    "QuadReadAcross",
    "QuadReadLaneAt",
    "int64_t",
    "uint64_t",
    "float16_t",
    "int16_t",
    "uint16_t",
];

/// How much of the matrix to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixScope {
    /// A fixed small subset: one full-forward desktop preset's base and studio-off modules, the
    /// Mobile Low lite module, one graph pass and two compute modules. The default test runs it.
    Subset,
    /// Every input (`check-shaders --matrix`).
    Full,
}

impl MatrixScope {
    /// `subset` or `full`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Subset => "subset",
            Self::Full => "full",
        }
    }
}

/// One backend of the matrix, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixBackend {
    /// naga_oil composition of the source with the defines.
    Compose,
    /// naga validation (all capabilities) and the texture-parameter hazard check.
    Naga,
    /// The Function/Private explicit-layout scan of the SPIR-V (ADR-026; Workgroup hits are
    /// ADR-135 warnings).
    LayoutScan,
    /// `spirv-val --target-env vulkan1.3`.
    SpirvVal,
    /// The module and a pipeline per entry point on a validation-enabled device.
    VulkanPipeline,
    /// naga's HLSL writer, Shader Model 6.0 (DXC).
    HlslSm6,
    /// naga's HLSL writer, Shader Model 5.1 (FXC), with the Shader Model 6 text scan and the
    /// Direct3D 12 limits and FXC loop, addressing and barrier rules
    /// ([`crate::shader_hlsl::dx12_module_problems`]).
    HlslSm51,
    /// naga's MSL writer with wgpu's Metal options ([`crate::shader_msl_check`]).
    Msl,
}

impl MatrixBackend {
    /// Every backend, in run order.
    pub const ALL: [Self; 8] = [
        Self::Compose,
        Self::Naga,
        Self::LayoutScan,
        Self::SpirvVal,
        Self::VulkanPipeline,
        Self::HlslSm6,
        Self::HlslSm51,
        Self::Msl,
    ];

    /// The report name (`compose`, `naga`, `layout_scan`, `spirv_val`, `vulkan_pipeline`,
    /// `hlsl_sm6`, `hlsl_sm51`, `msl`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Naga => "naga",
            Self::LayoutScan => "layout_scan",
            Self::SpirvVal => "spirv_val",
            Self::VulkanPipeline => "vulkan_pipeline",
            Self::HlslSm6 => "hlsl_sm6",
            Self::HlslSm51 => "hlsl_sm51",
            Self::Msl => "msl",
        }
    }
}

/// The outcome of one backend on one module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendStatus {
    /// Ran and passed.
    Pass,
    /// Ran and failed.
    Fail,
    /// Did not run: an earlier stage failed, the device lacks a feature the module needs, or the
    /// run turned the backend off (the detail says which).
    Skipped,
}

/// One backend's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendResult {
    /// The backend.
    pub backend: MatrixBackend,
    /// Its outcome.
    pub status: BackendStatus,
    /// The error, or why it was skipped, or what passed (pipelines built).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Where a permutation comes from: `engine`, or `<platform>/<preset> <axis>`.
pub type MatrixOrigin = String;

/// One input of the matrix: a source composed with a define set.
#[derive(Debug, Clone)]
pub struct MatrixInput {
    /// The file name the composer sees (`lab_forward.wgsl`).
    pub file: String,
    /// The WGSL source.
    pub source: Cow<'static, str>,
    /// The defines.
    pub defs: HashMap<String, ShaderDefValue>,
    /// Where it comes from.
    pub origin: MatrixOrigin,
    /// `true`: the renderer hands this source to wgpu as plain WGSL (`wgsl_source`), not
    /// through the composer, so the matrix parses it the same way (no `#import`, no defines,
    /// every naga capability: ray queries, `unpack2x16float`).
    pub plain: bool,
}

impl MatrixInput {
    /// An input of `file` with `source`, the boolean `defines` and `origin`.
    #[must_use]
    pub fn new(
        file: impl Into<String>,
        source: impl Into<Cow<'static, str>>,
        defines: &[&str],
        origin: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            source: source.into(),
            defs: bool_defs(defines),
            origin: origin.into(),
            plain: false,
        }
    }

    /// An input of `file` the renderer creates from plain WGSL (`wgsl_source`), with `origin`.
    #[must_use]
    pub fn plain(
        file: impl Into<String>,
        source: impl Into<Cow<'static, str>>,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            source: source.into(),
            defs: HashMap::new(),
            origin: origin.into(),
            plain: true,
        }
    }
}

/// One define set that composes a module, with every origin that asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixPermutation {
    /// The defines, sorted (`NAME` or `NAME=value`).
    pub defines: Vec<String>,
    /// Every origin, sorted (`engine`, `linux/DesktopHigh forward studio_off`, ...).
    pub origins: Vec<String>,
}

/// One distinct composed module and its backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixModule {
    /// The file name.
    pub file: String,
    /// FNV-1a 64 of naga's WGSL re-emit of the composed module, 16 hex digits (of the input text
    /// and defines for a ray-query module, which naga's WGSL writer cannot write; of the file and
    /// defines, prefixed `x`, when composition or validation failed).
    pub hash: String,
    /// The define sets that compose it.
    pub permutations: Vec<MatrixPermutation>,
    /// One result per [`MatrixBackend`], in run order.
    pub backends: Vec<BackendResult>,
    /// No backend failed.
    pub ok: bool,
}

impl MatrixModule {
    /// The result of `backend`.
    #[must_use]
    pub fn backend(&self, backend: MatrixBackend) -> Option<&BackendResult> {
        self.backends.iter().find(|result| result.backend == backend)
    }

    /// `file [defines]` of the first permutation, for messages.
    #[must_use]
    pub fn label(&self) -> String {
        let defines = self
            .permutations
            .first()
            .map(|p| p.defines.join(","))
            .unwrap_or_default();
        format!("{} [{defines}]", self.file)
    }
}

/// One preset's frame (the frame's own pipelines) under validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetFrameResult {
    /// The preset name, or `Lab`.
    pub preset: String,
    /// Its outcome.
    pub status: BackendStatus,
    /// The validation errors, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The whole matrix (`check-shaders --matrix --json`, `docs/06-testing-and-ci.md` § shader gate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShaderMatrixReport {
    /// [`SHADER_MATRIX_REPORT_VERSION`].
    pub format_version: u32,
    /// [`MatrixScope::name`].
    pub scope: String,
    /// Inputs listed (every origin of every define set).
    pub inputs: usize,
    /// Distinct (file, source, defines) compositions.
    pub compositions: usize,
    /// Distinct composed modules, sorted by file then hash.
    pub modules: Vec<MatrixModule>,
    /// The adapter the pipeline backend ran on (`None` when it did not run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The per-preset frames (empty unless the run asked for them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<PresetFrameResult>,
    /// Validation messages the run could not tie to one module (a failure).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unattributed: Vec<String>,
    /// Modules with a failed backend.
    pub failed_modules: usize,
    /// The Rust and WGSL struct agreement of the engine's shaders (`check-shaders` fills it,
    /// ADR-3002); `None` when the run did not check it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<crate::shader_contract::ContractSummary>,
    /// The binding counts of every preset against the device-class limits (`check-shaders` fills
    /// it, ADR-3002); `None` when the run did not count them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::shader_limits::LimitsReport>,
}

impl ShaderMatrixReport {
    /// No module failed, no frame failed and no validation message is unattributed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failed_modules == 0
            && self.unattributed.is_empty()
            && self.frames.iter().all(|f| f.status != BackendStatus::Fail)
    }

    /// One line per failure: `file [defines] backend: detail`, then frames and unattributed lines.
    #[must_use]
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        for module in self.modules.iter().filter(|m| !m.ok) {
            for result in module.backends.iter().filter(|r| r.status == BackendStatus::Fail) {
                out.push(format!(
                    "{} {}: {}",
                    module.label(),
                    result.backend.name(),
                    result.detail.as_deref().unwrap_or("failed")
                ));
            }
        }
        for frame in self.frames.iter().filter(|f| f.status == BackendStatus::Fail) {
            out.push(format!(
                "frame {}: {}",
                frame.preset,
                frame.detail.as_deref().unwrap_or("failed")
            ));
        }
        out.extend(
            self.unattributed
                .iter()
                .map(|line| format!("unattributed: {line}")),
        );
        out
    }

    /// Counts per backend: `(backend, pass, fail, skipped)`.
    #[must_use]
    pub fn backend_counts(&self) -> Vec<(MatrixBackend, usize, usize, usize)> {
        MatrixBackend::ALL
            .iter()
            .map(|&backend| {
                let mut counts = (backend, 0, 0, 0);
                for module in &self.modules {
                    match module.backend(backend).map(|r| r.status) {
                        Some(BackendStatus::Pass) => counts.1 += 1,
                        Some(BackendStatus::Fail) => counts.2 += 1,
                        Some(BackendStatus::Skipped) | None => counts.3 += 1,
                    }
                }
                counts
            })
            .collect()
    }

    /// The one-paragraph text summary `check-shaders --matrix` prints.
    #[must_use]
    pub fn summary(&self) -> String {
        let permutations: usize = self.modules.iter().map(|m| m.permutations.len()).sum();
        let mut text = format!(
            "shader matrix ({}): {} inputs, {} compositions, {} define sets, {} distinct modules, {} failed",
            self.scope,
            self.inputs,
            self.compositions,
            permutations,
            self.modules.len(),
            self.failed_modules
        );
        for (backend, pass, fail, skipped) in self.backend_counts() {
            text.push_str(&format!(
                "\n  {:<16} pass {pass:>4}  fail {fail:>4}  skipped {skipped:>4}",
                backend.name()
            ));
        }
        if let Some(adapter) = &self.adapter {
            text.push_str(&format!("\n  pipelines on {adapter}"));
        }
        for frame in &self.frames {
            text.push_str(&format!("\n  frame {:<12} {:?}", frame.preset, frame.status));
        }
        text
    }

    /// Pretty JSON.
    ///
    /// # Errors
    ///
    /// A serde error (not expected for this shape).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parses [`Self::to_json`] output.
    ///
    /// # Errors
    ///
    /// A serde error on malformed text.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// What a matrix run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderMatrixOptions {
    /// Which inputs.
    pub scope: MatrixScope,
    /// Create each module and its pipelines on a validation-enabled device.
    pub pipelines: bool,
    /// Render each preset's frame under validation ([`validate_preset_frames`]).
    pub frames: bool,
    /// Worker threads for composition and the CPU backends (at least 1).
    pub jobs: usize,
}

impl ShaderMatrixOptions {
    /// `scope` with pipelines and frames on and [`default_matrix_jobs`] workers.
    #[must_use]
    pub fn new(scope: MatrixScope) -> Self {
        Self {
            scope,
            pipelines: true,
            frames: scope == MatrixScope::Full,
            jobs: default_matrix_jobs(),
        }
    }
}

/// Environment variable with the matrix's worker count (default: the available parallelism,
/// at most 6).
pub const SHADER_MATRIX_JOBS_ENV: &str = "REGIA_SHADER_MATRIX_JOBS";

/// The worker count [`ShaderMatrixOptions::new`] uses.
#[must_use]
pub fn default_matrix_jobs() -> usize {
    regia_core::brand::env_var(SHADER_MATRIX_JOBS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(6)))
}

fn bool_defs(names: &[&str]) -> HashMap<String, ShaderDefValue> {
    names
        .iter()
        .map(|name| ((*name).to_string(), ShaderDefValue::Bool(true)))
        .collect()
}

/// `defs` as sorted `NAME` / `NAME=value` strings (false booleans dropped), as
/// `forward_permutations` records them.
#[must_use]
pub fn define_key(defs: &HashMap<String, ShaderDefValue>) -> Vec<String> {
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
    key
}

/// FNV-1a 64 (Fowler, Noll, Vo): a stable hash for report keys.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The material binding tiers a platform's devices draw with (`bindless::add_material_defs`):
/// the uniform arena everywhere, the storage arena and the texture table off Android.
fn material_tiers(platform: &str) -> Vec<(&'static str, Vec<(String, ShaderDefValue)>)> {
    let mut tiers = vec![("material_uniform", Vec::new())];
    if platform != "android" {
        let ssbo = (
            crate::bindless::MATERIAL_SSBO_DEF.to_string(),
            ShaderDefValue::Bool(true),
        );
        tiers.push(("material_ssbo", vec![ssbo.clone()]));
        tiers.push((
            "texture_table",
            vec![
                ssbo,
                (
                    crate::texture_table::TEXTURE_TABLE_DEF.to_string(),
                    ShaderDefValue::Bool(true),
                ),
                (
                    crate::texture_table::TEXTURE_TABLE_SIZE_DEF.to_string(),
                    ShaderDefValue::UInt(MATRIX_TEXTURE_TABLE_SIZE),
                ),
            ],
        ));
    }
    tiers
}

/// One forward permutation of a preset: the defines and the axis name for the origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardPermutation {
    /// The composer's file name (`lab_forward.wgsl` or the lite file).
    pub file: &'static str,
    /// The defines.
    pub defs: Vec<(String, ShaderDefValue)>,
    /// The axes, space separated (`gpu_instanced linear studio_off texture_table`).
    pub axes: String,
}

/// Every forward permutation `preset` can compose on `platform`'s devices: the base and feature
/// modules of the full shader (or the lite tier's, where the profile takes it), crossed with the
/// output encoding, the studio axis, the material binding tiers, the traced sun on desktop and
/// the Adreno workaround on Android.
///
/// It mirrors the define sets the frame's pipeline sites compose (`frame/pipelines.rs`,
/// `gpu_driven.rs`, the feature modules under `frame/`): the opt-in features pair with
/// `ALPHA_TEST` on the CPU path, subsurface and OIT draw linear HDR only, lightmap receivers also
/// draw instanced.
#[must_use]
pub fn forward_permutations_for(platform: &str, preset: RenderProfilePreset) -> Vec<ForwardPermutation> {
    use crate::forward_lite::ForwardLite;
    let profile = RenderProfile::from_preset(preset);
    let android = platform == "android";
    let mut out = Vec::new();
    let mut push = |file: &'static str, defs: Vec<(String, ShaderDefValue)>, axes: Vec<&str>| {
        out.push(ForwardPermutation {
            file,
            defs,
            axes: axes.join(" "),
        });
    };
    let b = |name: &str| (name.to_string(), ShaderDefValue::Bool(true));

    // The lite tier, on tile GPUs (`forward_lite::adapter_takes_lite`).
    if profile.forward_lite != ForwardLite::Off {
        let tier = profile.forward_lite;
        for alpha in [false, true] {
            for encode in [false, true] {
                for weather in [false, true] {
                    let defs = crate::forward_lite::lite_defs(tier, alpha, encode, weather);
                    let mut axes = vec!["lite", tier.name()];
                    if alpha {
                        axes.push("alpha_test");
                    }
                    if !encode {
                        axes.push("linear");
                    }
                    if weather {
                        axes.push("weather");
                    }
                    push(
                        crate::forward_lite::FORWARD_LITE_SHADER_FILE,
                        defs.into_iter().collect(),
                        axes,
                    );
                }
                let mut defs: Vec<_> = crate::forward_lite::lite_defs(tier, alpha, encode, false)
                    .into_iter()
                    .collect();
                defs.push(b("LIGHTMAP"));
                let mut axes = vec!["lite", tier.name(), "lightmap"];
                if alpha {
                    axes.push("alpha_test");
                }
                if !encode {
                    axes.push("linear");
                }
                push(crate::forward_lite::FORWARD_LITE_SHADER_FILE, defs, axes);
            }
        }
    }

    // The full shader: every preset draws it on a device the lite tier does not take.
    // (defines, axis, linear only)
    let mut modules: Vec<(Vec<&str>, &str, bool)> = vec![(vec!["ALPHA_TEST"], "forward", false)];
    if profile.gpu_driven {
        modules.extend([
            (vec!["GPU_DRIVEN"], "gpu", false),
            (vec!["GPU_DRIVEN", "ALPHA_TEST"], "gpu alpha_test", false),
            (vec!["GPU_DRIVEN", "GPU_INSTANCED"], "gpu_instanced", false),
            (
                vec!["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"],
                "gpu_instanced alpha_test",
                false,
            ),
            (
                vec!["GPU_DRIVEN", "GPU_INSTANCED", "LIGHTMAP"],
                "gpu_instanced lightmap",
                false,
            ),
            (
                vec!["GPU_DRIVEN", "GPU_INSTANCED", "LIGHTMAP", "ALPHA_TEST"],
                "gpu_instanced lightmap alpha_test",
                false,
            ),
        ]);
    }
    modules.extend([
        (vec!["GLASS", "ALPHA_TEST"], "glass", false),
        (vec!["MASK_DECALS", "ALPHA_TEST"], "mask_decals", false),
        (vec!["THIN_FILM", "ALPHA_TEST"], "thin_film", false),
        (vec!["LIGHTMAP", "ALPHA_TEST"], "lightmap", false),
        (vec!["SUBSURFACE", "ALPHA_TEST"], "subsurface", true),
        (vec!["OIT", "ALPHA_TEST"], "oit", true),
    ]);
    let sun_rt: &[bool] = if android { &[false] } else { &[false, true] };
    let workaround: &[bool] = if android { &[false, true] } else { &[false] };
    for (names, axis, linear_only) in &modules {
        let encodes: &[bool] = if *linear_only { &[false] } else { &[true, false] };
        for &encode in encodes {
            for studio_off in [false, true] {
                for &rt in sun_rt {
                    for &adreno in workaround {
                        if adreno && !studio_off {
                            // The workaround always composes the studio-off module (ADR-2042).
                            continue;
                        }
                        for (tier, material) in material_tiers(platform) {
                            let mut defs: Vec<(String, ShaderDefValue)> =
                                names.iter().map(|n| b(n)).collect();
                            let mut axes = vec![*axis];
                            if !encode {
                                defs.push(b("skip_srgb_encode"));
                                axes.push("linear");
                            }
                            if studio_off {
                                defs.push(b(crate::shader_workarounds::STUDIO_IBL_OFF_DEF));
                                axes.push("studio_off");
                            }
                            if rt {
                                defs.push(b(crate::hwrt::SUN_SHADOW_RT_DEF));
                                axes.push("sun_rt");
                            }
                            if adreno {
                                // Every define the workaround adds at runtime
                                // (`bindless::add_material_defs`), `FORWARD_TRIM` included.
                                for name in crate::shader_workarounds::workaround_defs(true) {
                                    if !defs.iter().any(|(have, _)| have == name) {
                                        defs.push(b(name));
                                    }
                                }
                                axes.push("adreno");
                            }
                            defs.extend(material);
                            axes.push(tier);
                            push(crate::forward_permutations::FORWARD_SHADER_FILE, defs, axes);
                        }
                    }
                }
            }
        }
    }
    out
}

/// The sample material graphs the matrix enumerates graph sets for: a surface-only graph and a
/// graph with a vertex stage that reads the second UV set, single- and double-sided.
fn sample_graphs() -> Vec<(GraphPermutationInput, GraphCode)> {
    let surface = GraphCode::surface(SAMPLE_SURFACE);
    let staged = GraphCode {
        surface: SAMPLE_SURFACE.into(),
        vertex: Some(SAMPLE_VERTEX.into()),
        uv1: true,
    };
    let mut out = Vec::new();
    for (name, code) in [("sample_surface", surface), ("sample_staged", staged)] {
        let input = GraphPermutationInput::from_code(name, &code);
        out.push((
            GraphPermutationInput {
                double_sided: true,
                ..input.clone()
            },
            code.clone(),
        ));
        out.push((input, code));
    }
    out
}

/// The graph-pass inputs of `preset` on `platform`: every set [`enumerate_profile`] lists for
/// the sample graphs, one module per pass, on the profile's linear HDR target, with and without
/// the traced sun on desktop.
fn graph_inputs(platform: &str, preset: RenderProfilePreset, out: &mut Vec<MatrixInput>) {
    let graphs = sample_graphs();
    let inputs: Vec<GraphPermutationInput> = graphs.iter().map(|(input, _)| input.clone()).collect();
    let listed = enumerate_profile(
        preset,
        &inputs,
        PermutationBudget::for_preset(preset),
        CompileCostModel::for_preset(preset),
    );
    let sun_rt: &[bool] = if platform == "android" {
        &[false]
    } else {
        &[false, true]
    };
    for entry in &listed.entries {
        let Some((input, code)) = graphs
            .iter()
            .find(|(input, _)| format!("{:016x}", input.key) == entry.key)
        else {
            continue;
        };
        for pass in &entry.passes {
            let Some(kind) = graph_pass_module(pass) else {
                out.push(MatrixInput {
                    file: "lab_forward_graph.wgsl".into(),
                    source: Cow::Borrowed(""),
                    defs: HashMap::new(),
                    origin: format!(
                        "{platform}/{} graph {} {pass} (no module)",
                        listed.profile, input.name
                    ),
                    plain: false,
                });
                continue;
            };
            for &rt in sun_rt {
                let origin = format!(
                    "{platform}/{} graph {}{} {pass}{}",
                    listed.profile,
                    input.name,
                    if entry.double_sided { " double_sided" } else { "" },
                    if rt { " sun_rt" } else { "" }
                );
                let (file, source, defs) = match kind {
                    GraphPassModule::Forward { extra, target_defs } => {
                        if rt && !target_defs {
                            continue;
                        }
                        let mut all = extra.to_vec();
                        if target_defs {
                            // Runtime frames are linear HDR (`ProfileTarget`).
                            all.push("skip_srgb_encode");
                            if rt {
                                all.push(crate::hwrt::SUN_SHADOW_RT_DEF);
                            }
                        }
                        let source = match forward_source_with_code(code) {
                            Ok(source) => source,
                            Err(err) => {
                                out.push(compose_error_input("lab_forward_graph.wgsl", &origin, &err));
                                continue;
                            }
                        };
                        ("lab_forward_graph.wgsl", source, code_defs(code, &all))
                    }
                    GraphPassModule::Shadow { .. } | GraphPassModule::Velocity if rt => continue,
                    GraphPassModule::Shadow { extra } => match vertex_host_source(code, false) {
                        Ok(Some(source)) => ("lab_shadow_graph.wgsl", source, vertex_host_defs(code, extra)),
                        Ok(None) => continue,
                        Err(err) => {
                            out.push(compose_error_input("lab_shadow_graph.wgsl", &origin, &err));
                            continue;
                        }
                    },
                    GraphPassModule::Velocity => match vertex_host_source(code, true) {
                        Ok(Some(source)) => ("velocity_graph.wgsl", source, vertex_host_defs(code, &[])),
                        Ok(None) => continue,
                        Err(err) => {
                            out.push(compose_error_input("velocity_graph.wgsl", &origin, &err));
                            continue;
                        }
                    },
                };
                out.push(MatrixInput {
                    file: file.to_string(),
                    source: Cow::Owned(source),
                    defs,
                    origin,
                    plain: false,
                });
            }
        }
    }
}

/// An input whose source could not be built: composing the marker fails with `err` in the text.
fn compose_error_input(file: &str, origin: &str, err: &RenderError) -> MatrixInput {
    MatrixInput {
        file: file.to_string(),
        source: Cow::Owned(format!("// source generation failed: {err}\n@@@\n")),
        defs: HashMap::new(),
        origin: origin.to_string(),
        plain: false,
    }
}

/// Every input of `scope`.
#[must_use]
pub fn shader_matrix_inputs(scope: MatrixScope) -> Vec<MatrixInput> {
    let mut out = Vec::new();
    match scope {
        MatrixScope::Full => {
            for (name, source, defs) in super::shader_sources() {
                out.push(MatrixInput {
                    file: name.to_string(),
                    source: Cow::Borrowed(source),
                    defs,
                    origin: "engine".into(),
                    plain: false,
                });
            }
            for extra in crate::shader_hlsl::extra_engine_modules() {
                out.push(match extra.defs {
                    Some(defs) => MatrixInput::new(extra.file, extra.source, defs, "engine"),
                    None => MatrixInput::plain(extra.file, extra.source, "engine"),
                });
            }
            for platform in SHIPPED_PLATFORMS {
                for preset in platform_presets(platform) {
                    for permutation in forward_permutations_for(platform, preset) {
                        out.push(MatrixInput {
                            file: permutation.file.to_string(),
                            source: Cow::Borrowed(forward_source(permutation.file)),
                            defs: permutation.defs.into_iter().collect(),
                            origin: format!("{platform}/{} {}", preset.name(), permutation.axes),
                            plain: false,
                        });
                    }
                    graph_inputs(platform, preset, &mut out);
                }
            }
        }
        MatrixScope::Subset => {
            let preset = RenderProfilePreset::DesktopHigh;
            for permutation in forward_permutations_for("linux", preset) {
                let keep = matches!(
                    permutation.axes.as_str(),
                    "forward material_uniform" | "gpu_instanced linear studio_off material_ssbo"
                );
                if keep {
                    out.push(MatrixInput {
                        file: permutation.file.to_string(),
                        source: Cow::Borrowed(forward_source(permutation.file)),
                        defs: permutation.defs.into_iter().collect(),
                        origin: format!("linux/{} {}", preset.name(), permutation.axes),
                        plain: false,
                    });
                }
            }
            let mobile = RenderProfilePreset::MobileLow;
            if let Some(lite) = forward_permutations_for("android", mobile)
                .into_iter()
                .find(|p| p.axes == "lite low alpha_test linear")
            {
                out.push(MatrixInput {
                    file: lite.file.to_string(),
                    source: Cow::Borrowed(forward_source(lite.file)),
                    defs: lite.defs.into_iter().collect(),
                    origin: format!("android/{} {}", mobile.name(), lite.axes),
                    plain: false,
                });
            }
            let mut graphs = Vec::new();
            graph_inputs("linux", preset, &mut graphs);
            out.extend(
                graphs
                    .into_iter()
                    .filter(|input| input.origin.ends_with("sample_staged velocity")),
            );
            out.push(MatrixInput::new(
                "cluster_build.wgsl",
                include_str!("../../shaders/cluster_build.wgsl"),
                &[],
                "engine",
            ));
            // A compute module that gathers an integer texture through a sampler (the layout
            // needs a non-filtering sampler there).
            out.push(MatrixInput::new(
                "sssr_reproject.wgsl",
                include_str!("../../shaders/sssr_reproject.wgsl"),
                &[],
                "engine",
            ));
        }
    }
    out
}

/// The source of a forward file name.
pub(super) fn forward_source(file: &str) -> &'static str {
    if file == crate::forward_lite::FORWARD_LITE_SHADER_FILE {
        include_str!("../../shaders/forward_mobile.wgsl")
    } else {
        include_str!("../../shaders/lab_forward.wgsl")
    }
}

/// One composition: the merged input and, once composed, its module.
struct Composition {
    file: String,
    source: Cow<'static, str>,
    defs: HashMap<String, ShaderDefValue>,
    defines: Vec<String>,
    origins: BTreeSet<String>,
    plain: bool,
}

/// A composed and validated module, overrides substituted.
struct Prepared {
    module: naga::Module,
    info: naga::valid::ModuleInfo,
}

/// The result of composing and validating one composition.
enum Composed {
    /// Composition failed.
    ComposeFailed(String),
    /// naga validation (or the hazard check, or override substitution) failed.
    Invalid(String),
    /// Ready for the backends, with the hash of naga's WGSL re-emit.
    Ready(Box<Prepared>, u64),
}

fn compose_one(c: &Composition) -> Composed {
    let label = if c.plain {
        format!("{} (plain WGSL)", c.file)
    } else {
        format!("{} [{}]", c.file, c.defines.join(","))
    };
    let module = if c.plain {
        // As `wgsl_source` hands it to wgpu: parsed, texture-handle arguments specialized.
        match naga::front::wgsl::parse_str(&c.source) {
            Ok(mut module) => {
                crate::specialize_handle_arguments(&mut module);
                module
            }
            Err(err) => return Composed::ComposeFailed(format!("{label}: wgsl parse: {err}")),
        }
    } else {
        match compose_source(&c.source, &c.file, c.defs.clone()) {
            Ok(module) => module,
            Err(err) => return Composed::ComposeFailed(err.to_string()),
        }
    };
    let hazards = crate::image_parameter_hazards(&module);
    if !hazards.is_empty() {
        return Composed::Invalid(format!(
            "{label}: image op through a texture parameter: {}",
            hazards.join("; ")
        ));
    }
    let info = match naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    {
        Ok(info) => info,
        Err(err) => return Composed::Invalid(format!("{label}: naga validation: {err}")),
    };
    let constants = naga::back::PipelineConstants::new();
    let (module, info) =
        match naga::back::pipeline_constants::process_overrides(&module, &info, None, &constants) {
            Ok((module, info)) => (module.into_owned(), info.into_owned()),
            Err(err) => return Composed::Invalid(format!("{label}: pipeline overrides: {err}")),
        };
    // naga 30's WGSL writer cannot write ray-query statements (it reaches `unreachable!`), so
    // such a module is keyed by its input text and defines instead of the re-emit.
    let text = if uses_ray_query(&module) {
        format!("{}\0{}", c.defines.join(","), c.source)
    } else {
        match naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty()) {
            Ok(text) => text,
            Err(err) => return Composed::Invalid(format!("{label}: wgsl re-emit: {err}")),
        }
    };
    let mut bytes = c.file.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(text.as_bytes());
    Composed::Ready(Box::new(Prepared { module, info }), fnv1a(&bytes))
}

/// Runs `work` over `items` on `jobs` threads, results in item order.
fn parallel_map<T: Sync, R: Send>(items: &[T], jobs: usize, work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let jobs = jobs.max(1).min(items.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(at) else {
                        break;
                    };
                    let result = work(item);
                    if let Ok(mut slots) = results.lock() {
                        slots[at] = Some(result);
                    }
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .into_iter()
        .flatten()
        .collect()
}

fn pass(backend: MatrixBackend, detail: Option<String>) -> BackendResult {
    BackendResult {
        backend,
        status: BackendStatus::Pass,
        detail,
    }
}

fn fail(backend: MatrixBackend, detail: String) -> BackendResult {
    BackendResult {
        backend,
        status: BackendStatus::Fail,
        detail: Some(detail),
    }
}

fn skipped(backend: MatrixBackend, why: &str) -> BackendResult {
    BackendResult {
        backend,
        status: BackendStatus::Skipped,
        detail: Some(why.to_string()),
    }
}

/// The CPU backends of one prepared module: layout scan, `spirv-val`, HLSL 6.0 and 5.1, MSL.
fn cpu_backends(label: &str, prepared: &Prepared) -> Vec<BackendResult> {
    let mut out = Vec::new();
    match write_spirv(label, &prepared.module, &prepared.info) {
        Ok(words) => {
            match layout_scan_words(label, &words) {
                Ok(warnings) if warnings.is_empty() => out.push(pass(MatrixBackend::LayoutScan, None)),
                Ok(warnings) => out.push(pass(
                    MatrixBackend::LayoutScan,
                    Some(format!("{} Workgroup warning(s) (ADR-135)", warnings.len())),
                )),
                Err(err) => out.push(fail(MatrixBackend::LayoutScan, err)),
            }
            match spirv_val_words(label, &words) {
                Ok(()) => out.push(pass(MatrixBackend::SpirvVal, None)),
                Err(err) => out.push(fail(MatrixBackend::SpirvVal, err)),
            }
        }
        Err(err) => {
            out.push(fail(MatrixBackend::LayoutScan, err.clone()));
            out.push(fail(MatrixBackend::SpirvVal, err));
        }
    }
    // Ray queries need Shader Model 6.5, which wgpu's DX12 backend compiles with DXC only:
    // such a module is written as 6.5 and has no FXC (5.1) form to check.
    if uses_ray_query(&prepared.module) {
        match hlsl_text(label, prepared, naga::back::hlsl::ShaderModel::V6_5) {
            Ok(_) => out.push(pass(
                MatrixBackend::HlslSm6,
                Some("Shader Model 6.5 (ray queries)".into()),
            )),
            Err(err) => out.push(fail(MatrixBackend::HlslSm6, err)),
        }
        out.push(skipped(
            MatrixBackend::HlslSm51,
            "ray queries need Shader Model 6.5 (DXC); FXC never compiles this module",
        ));
    } else {
        match hlsl_text(label, prepared, naga::back::hlsl::ShaderModel::V6_0) {
            Ok(_) => out.push(pass(MatrixBackend::HlslSm6, None)),
            Err(err) => out.push(fail(MatrixBackend::HlslSm6, err)),
        }
        out.push(hlsl_sm51(label, prepared));
    }
    match crate::shader_msl_check::msl_entry_points(label, &prepared.module, &prepared.info) {
        Ok(entries) => out.push(pass(
            MatrixBackend::Msl,
            Some(format!("{entries} entry point(s)")),
        )),
        Err(err) => out.push(fail(MatrixBackend::Msl, err)),
    }
    out
}

/// `true` when `module` declares an acceleration structure or a ray query.
fn uses_ray_query(module: &naga::Module) -> bool {
    module.types.iter().any(|(_, ty)| {
        matches!(
            ty.inner,
            naga::TypeInner::AccelerationStructure { .. } | naga::TypeInner::RayQuery { .. }
        )
    })
}

/// The FXC (Shader Model 5.1) backend of one prepared module.
fn hlsl_sm51(label: &str, prepared: &Prepared) -> BackendResult {
    let mut fxc = Vec::new();
    match hlsl_text(label, prepared, naga::back::hlsl::ShaderModel::V5_1) {
        Ok(text) => {
            let found: Vec<&str> = FXC_REJECTS.into_iter().filter(|t| text.contains(t)).collect();
            if !found.is_empty() {
                fxc.push(format!(
                    "{label}: Shader Model 6 HLSL in the FXC (5.1) text: {}",
                    found.join(", ")
                ));
            }
        }
        Err(err) => fxc.push(err),
    }
    // The Direct3D 12 limits and FXC's loop, addressing and barrier rules (ADR-201, ADR-1583).
    fxc.extend(crate::shader_hlsl::dx12_module_problems(label, &prepared.module));
    fxc.dedup();
    if fxc.is_empty() {
        pass(MatrixBackend::HlslSm51, None)
    } else {
        fail(MatrixBackend::HlslSm51, fxc.join("; "))
    }
}

/// The HLSL of every entry point of `prepared` for `shader_model`, with the writer options
/// `shader_hlsl` uses (the special-constants buffer bound, as wgpu-hal always binds it).
fn hlsl_text(
    label: &str,
    prepared: &Prepared,
    shader_model: naga::back::hlsl::ShaderModel,
) -> Result<String, String> {
    use naga::back::hlsl;
    let options = hlsl::Options {
        shader_model,
        special_constants_binding: Some(hlsl::BindTarget {
            space: u8::MAX,
            ..hlsl::BindTarget::default()
        }),
        ..hlsl::Options::default()
    };
    let mut all = String::new();
    for entry in &prepared.module.entry_points {
        let pipeline = hlsl::PipelineOptions {
            entry_point: Some((entry.stage, entry.name.clone())),
        };
        let mut text = String::new();
        let mut writer = hlsl::Writer::new(&mut text, &options, &pipeline);
        let reflection = writer
            .write(&prepared.module, &prepared.info, None)
            .map_err(|err| format!("{label} ({}, {shader_model:?}): hlsl: {err}", entry.name))?;
        for written in reflection.entry_point_names {
            written.map_err(|err| {
                format!(
                    "{label} ({}, {shader_model:?}): hlsl entry point: {err}",
                    entry.name
                )
            })?;
        }
        all.push_str(&text);
    }
    Ok(all)
}

/// Runs the matrix over `scope`'s inputs.
///
/// # Errors
///
/// [`RenderError`] when the pipeline backend's device or the validation log capture cannot be
/// created. Module failures are in the report, not errors.
pub fn run_shader_matrix(options: &ShaderMatrixOptions) -> Result<ShaderMatrixReport, RenderError> {
    let mut report = run_matrix_inputs(shader_matrix_inputs(options.scope), options)?;
    report.scope = options.scope.name().to_string();
    Ok(report)
}

/// Runs the matrix over `inputs` (a test passes its own fixtures).
///
/// # Errors
///
/// As [`run_shader_matrix`].
pub fn run_matrix_inputs(
    inputs: Vec<MatrixInput>,
    options: &ShaderMatrixOptions,
) -> Result<ShaderMatrixReport, RenderError> {
    let _span = tracing::info_span!("shader_matrix", inputs = inputs.len()).entered();
    let input_count = inputs.len();
    // 1. Merge equal (file, source, defines).
    let mut merged: BTreeMap<(String, u64, Vec<String>, bool), Composition> = BTreeMap::new();
    for input in inputs {
        let defines = define_key(&input.defs);
        let key = (
            input.file.clone(),
            fnv1a(input.source.as_bytes()),
            defines.clone(),
            input.plain,
        );
        merged
            .entry(key)
            .or_insert_with(|| Composition {
                file: input.file,
                source: input.source,
                defs: input.defs,
                defines,
                origins: BTreeSet::new(),
                plain: input.plain,
            })
            .origins
            .insert(input.origin);
    }
    let compositions: Vec<Composition> = merged.into_values().collect();

    // 2. Compose and validate in parallel.
    let composed = parallel_map(&compositions, options.jobs, compose_one);

    // 3. Group by module hash.
    struct Group {
        file: String,
        prepared: Option<Box<Prepared>>,
        early: Vec<BackendResult>,
        permutations: Vec<MatrixPermutation>,
    }
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
    for (composition, result) in compositions.iter().zip(composed) {
        let permutation = MatrixPermutation {
            defines: composition.defines.clone(),
            origins: composition.origins.iter().cloned().collect(),
        };
        let failed_key = || {
            let mut bytes = composition.file.as_bytes().to_vec();
            bytes.extend(composition.defines.join(",").bytes());
            bytes.extend_from_slice(&fnv1a(composition.source.as_bytes()).to_le_bytes());
            format!("x{:016x}", fnv1a(&bytes))
        };
        let (hash, prepared, early) = match result {
            Composed::ComposeFailed(err) => (
                failed_key(),
                None,
                vec![
                    fail(MatrixBackend::Compose, err),
                    skipped(MatrixBackend::Naga, "composition failed"),
                ],
            ),
            Composed::Invalid(err) => (
                failed_key(),
                None,
                vec![pass(MatrixBackend::Compose, None), fail(MatrixBackend::Naga, err)],
            ),
            Composed::Ready(prepared, hash) => (
                format!("{hash:016x}"),
                Some(prepared),
                vec![
                    pass(MatrixBackend::Compose, None),
                    pass(MatrixBackend::Naga, None),
                ],
            ),
        };
        let group = groups
            .entry((composition.file.clone(), hash))
            .or_insert_with(|| Group {
                file: composition.file.clone(),
                prepared,
                early,
                permutations: Vec::new(),
            });
        group.permutations.push(permutation);
    }
    let groups: Vec<((String, String), Group)> = groups.into_iter().collect();

    // 4. CPU backends in parallel.
    let cpu: Vec<Option<Vec<BackendResult>>> = parallel_map(&groups, options.jobs, |((_, _), group)| {
        let label = format!(
            "{} [{}]",
            group.file,
            group
                .permutations
                .first()
                .map(|p| p.defines.join(","))
                .unwrap_or_default()
        );
        group
            .prepared
            .as_ref()
            .map(|prepared| cpu_backends(&label, prepared))
    });

    // 5. The device backend on the same workers: wgpu keeps error scopes per thread, and each
    // module's objects carry its hash in their labels, so a driver message names its module.
    let mut unattributed = Vec::new();
    let mut adapter = None;
    let mut device_results: Vec<Option<BackendResult>> = (0..groups.len()).map(|_| None).collect();
    if options.pipelines {
        install_logger()?;
        let mut flags = crate::device::instance_flags();
        flags.insert(wgpu::InstanceFlags::VALIDATION);
        let device = RenderDevice::headless_with_flags(flags)?;
        adapter = Some(device.adapter.get_info().name);
        if let Ok(mut lines) = LINES.lock() {
            lines.clear();
        }
        device_results = parallel_map(&groups, options.jobs, |((_, hash), group)| {
            group
                .prepared
                .as_ref()
                .map(|prepared| device_backend(&device, hash, &group.file, prepared))
        });
        if let Ok(mut lines) = LINES.lock() {
            unattributed.extend(validation_hits(&lines));
            lines.clear();
        }
    }

    // 6. Assemble.
    let mut modules = Vec::new();
    for ((((file, hash), group), cpu), device) in groups.into_iter().zip(cpu).zip(device_results) {
        let mut backends = group.early;
        let device = device.unwrap_or_else(|| {
            if group.prepared.is_none() {
                skipped(MatrixBackend::VulkanPipeline, "an earlier stage failed")
            } else {
                skipped(MatrixBackend::VulkanPipeline, "pipelines off in this run")
            }
        });
        match cpu {
            Some(cpu) => {
                backends.extend(cpu);
                backends.push(device);
            }
            None => {
                for backend in [
                    MatrixBackend::LayoutScan,
                    MatrixBackend::SpirvVal,
                    MatrixBackend::HlslSm6,
                    MatrixBackend::HlslSm51,
                    MatrixBackend::Msl,
                ] {
                    backends.push(skipped(backend, "an earlier stage failed"));
                }
                backends.push(device);
            }
        }
        backends.sort_by_key(|result| result.backend);
        let ok = backends.iter().all(|result| result.status != BackendStatus::Fail);
        let mut permutations = group.permutations;
        permutations.sort_by(|a, b| a.defines.cmp(&b.defines));
        modules.push(MatrixModule {
            file,
            hash,
            permutations,
            backends,
            ok,
        });
    }
    let failed_modules = modules.iter().filter(|m| !m.ok).count();
    let mut frames = Vec::new();
    if options.frames {
        frames = validate_preset_frames(&preset_frame_list())?;
    }
    Ok(ShaderMatrixReport {
        format_version: SHADER_MATRIX_REPORT_VERSION,
        scope: "inputs".into(),
        inputs: input_count,
        compositions: compositions.len(),
        modules,
        adapter,
        frames,
        unattributed,
        failed_modules,
        contract: None,
        limits: None,
    })
}

/// Device features a module needs beyond wgpu's defaults, by name, and whether `device` has
/// them all.
fn missing_features(module: &naga::Module, device: &wgpu::Device) -> Vec<&'static str> {
    let have = device.features();
    let mut need = Vec::new();
    for (_, ty) in module.types.iter() {
        match &ty.inner {
            naga::TypeInner::BindingArray { base, .. } => {
                let buffer = matches!(module.types[*base].inner, naga::TypeInner::Struct { .. });
                need.push(if buffer {
                    (wgpu::Features::BUFFER_BINDING_ARRAY, "BUFFER_BINDING_ARRAY")
                } else {
                    (wgpu::Features::TEXTURE_BINDING_ARRAY, "TEXTURE_BINDING_ARRAY")
                });
            }
            naga::TypeInner::AccelerationStructure { .. } | naga::TypeInner::RayQuery { .. } => {
                need.push((wgpu::Features::EXPERIMENTAL_RAY_QUERY, "EXPERIMENTAL_RAY_QUERY"));
            }
            naga::TypeInner::Scalar(scalar) | naga::TypeInner::Vector { scalar, .. }
                if scalar.kind == naga::ScalarKind::Float && scalar.width == 2 =>
            {
                need.push((wgpu::Features::SHADER_F16, "SHADER_F16"));
            }
            _ => {}
        }
    }
    for entry in &module.entry_points {
        let mut bindings: Vec<&naga::Binding> = Vec::new();
        collect_bindings(module, entry, &mut bindings);
        for binding in bindings {
            match binding {
                naga::Binding::BuiltIn(naga::BuiltIn::PrimitiveIndex) => {
                    need.push((wgpu::Features::PRIMITIVE_INDEX, "PRIMITIVE_INDEX"));
                }
                naga::Binding::BuiltIn(naga::BuiltIn::ClipDistances) => {
                    need.push((wgpu::Features::CLIP_DISTANCES, "CLIP_DISTANCES"));
                }
                naga::Binding::Location {
                    blend_src: Some(_), ..
                } => need.push((wgpu::Features::DUAL_SOURCE_BLENDING, "DUAL_SOURCE_BLENDING")),
                _ => {}
            }
        }
    }
    let mut missing: Vec<&'static str> = need
        .into_iter()
        .filter(|(feature, _)| !have.contains(*feature))
        .map(|(_, name)| name)
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}

/// Every argument and result binding of `entry` (struct members flattened).
fn collect_bindings<'a>(
    module: &'a naga::Module,
    entry: &'a naga::EntryPoint,
    out: &mut Vec<&'a naga::Binding>,
) {
    let mut add = |ty: naga::Handle<naga::Type>, binding: &'a Option<naga::Binding>| {
        if let Some(binding) = binding {
            out.push(binding);
        } else if let naga::TypeInner::Struct { members, .. } = &module.types[ty].inner {
            out.extend(members.iter().filter_map(|m| m.binding.as_ref()));
        }
    };
    for argument in &entry.function.arguments {
        add(argument.ty, &argument.binding);
    }
    if let Some(result) = &entry.function.result {
        add(result.ty, &result.binding);
    }
}

/// `(location, type)` of every `@location` argument (struct members flattened).
fn location_inputs(module: &naga::Module, entry: &naga::EntryPoint) -> Vec<(u32, naga::Handle<naga::Type>)> {
    let mut out = Vec::new();
    for argument in &entry.function.arguments {
        match &argument.binding {
            Some(naga::Binding::Location { location, .. }) => out.push((*location, argument.ty)),
            Some(naga::Binding::BuiltIn(_)) => {}
            None => {
                if let naga::TypeInner::Struct { members, .. } = &module.types[argument.ty].inner {
                    for member in members {
                        if let Some(naga::Binding::Location { location, .. }) = member.binding {
                            out.push((location, member.ty));
                        }
                    }
                }
            }
        }
    }
    out.sort_by_key(|(location, _)| *location);
    out
}

/// `(location, type)` of every `@location` output of `entry`.
fn location_outputs(module: &naga::Module, entry: &naga::EntryPoint) -> Vec<(u32, naga::Handle<naga::Type>)> {
    let mut out = Vec::new();
    if let Some(result) = &entry.function.result {
        match &result.binding {
            Some(naga::Binding::Location { location, .. }) => out.push((*location, result.ty)),
            Some(naga::Binding::BuiltIn(_)) => {}
            None => {
                if let naga::TypeInner::Struct { members, .. } = &module.types[result.ty].inner {
                    for member in members {
                        if let Some(naga::Binding::Location { location, .. }) = member.binding {
                            out.push((location, member.ty));
                        }
                    }
                }
            }
        }
    }
    out.sort_by_key(|(location, _)| *location);
    out
}

/// `(scalar, components)` of a scalar or vector type.
fn numeric(module: &naga::Module, ty: naga::Handle<naga::Type>) -> Option<(naga::Scalar, u32)> {
    match module.types[ty].inner {
        naga::TypeInner::Scalar(scalar) => Some((scalar, 1)),
        naga::TypeInner::Vector { size, scalar } => Some((scalar, size as u32)),
        _ => None,
    }
}

/// The vertex format of a `@location` input type.
pub(crate) fn vertex_format(
    module: &naga::Module,
    ty: naga::Handle<naga::Type>,
) -> Option<wgpu::VertexFormat> {
    use naga::ScalarKind as K;
    use wgpu::VertexFormat as F;
    let (scalar, n) = numeric(module, ty)?;
    if scalar.width != 4 {
        return None;
    }
    Some(match (scalar.kind, n) {
        (K::Float, 1) => F::Float32,
        (K::Float, 2) => F::Float32x2,
        (K::Float, 3) => F::Float32x3,
        (K::Float, 4) => F::Float32x4,
        (K::Uint, 1) => F::Uint32,
        (K::Uint, 2) => F::Uint32x2,
        (K::Uint, 3) => F::Uint32x3,
        (K::Uint, 4) => F::Uint32x4,
        (K::Sint, 1) => F::Sint32,
        (K::Sint, 2) => F::Sint32x2,
        (K::Sint, 3) => F::Sint32x3,
        (K::Sint, 4) => F::Sint32x4,
        _ => return None,
    })
}

/// The smallest colour format a fragment output of this type covers (wgpu accepts a target with
/// no more components than the output writes).
fn target_format(module: &naga::Module, ty: naga::Handle<naga::Type>) -> Option<wgpu::TextureFormat> {
    use naga::ScalarKind as K;
    use wgpu::TextureFormat as T;
    let (scalar, n) = numeric(module, ty)?;
    Some(match (scalar.kind, n) {
        (K::Float, 1) => T::R16Float,
        (K::Float, 2 | 3) => T::Rg16Float,
        (K::Float, _) => T::Rgba16Float,
        (K::Uint, 1) => T::R32Uint,
        (K::Uint, 2 | 3) => T::Rg32Uint,
        (K::Uint, _) => T::Rgba32Uint,
        (K::Sint, 1) => T::R32Sint,
        (K::Sint, 2 | 3) => T::Rg32Sint,
        (K::Sint, _) => T::Rgba32Sint,
        _ => return None,
    })
}

/// The texture format of a storage image.
fn storage_format(format: naga::StorageFormat) -> wgpu::TextureFormat {
    use naga::StorageFormat as S;
    use wgpu::TextureFormat as T;
    match format {
        S::R8Unorm => T::R8Unorm,
        S::R8Snorm => T::R8Snorm,
        S::R8Uint => T::R8Uint,
        S::R8Sint => T::R8Sint,
        S::R16Uint => T::R16Uint,
        S::R16Sint => T::R16Sint,
        S::R16Float => T::R16Float,
        S::Rg8Unorm => T::Rg8Unorm,
        S::Rg8Snorm => T::Rg8Snorm,
        S::Rg8Uint => T::Rg8Uint,
        S::Rg8Sint => T::Rg8Sint,
        S::R32Uint => T::R32Uint,
        S::R32Sint => T::R32Sint,
        S::R32Float => T::R32Float,
        S::Rg16Uint => T::Rg16Uint,
        S::Rg16Sint => T::Rg16Sint,
        S::Rg16Float => T::Rg16Float,
        S::Rgba8Unorm => T::Rgba8Unorm,
        S::Rgba8Snorm => T::Rgba8Snorm,
        S::Rgba8Uint => T::Rgba8Uint,
        S::Rgba8Sint => T::Rgba8Sint,
        S::Bgra8Unorm => T::Bgra8Unorm,
        S::Rgb10a2Uint => T::Rgb10a2Uint,
        S::Rgb10a2Unorm => T::Rgb10a2Unorm,
        S::Rg11b10Ufloat => T::Rg11b10Ufloat,
        S::R64Uint => T::R64Uint,
        S::Rg32Uint => T::Rg32Uint,
        S::Rg32Sint => T::Rg32Sint,
        S::Rg32Float => T::Rg32Float,
        S::Rgba16Uint => T::Rgba16Uint,
        S::Rgba16Sint => T::Rgba16Sint,
        S::Rgba16Float => T::Rgba16Float,
        S::Rgba32Uint => T::Rgba32Uint,
        S::Rgba32Sint => T::Rgba32Sint,
        S::Rgba32Float => T::Rgba32Float,
        S::R16Unorm => T::R16Unorm,
        S::R16Snorm => T::R16Snorm,
        S::Rg16Unorm => T::Rg16Unorm,
        S::Rg16Snorm => T::Rg16Snorm,
        S::Rgba16Unorm => T::Rgba16Unorm,
        S::Rgba16Snorm => T::Rgba16Snorm,
    }
}

/// The global variable an image or sampler expression reads (through binding-array indexing).
fn root_global(
    function: &naga::Function,
    mut expr: naga::Handle<naga::Expression>,
) -> Option<naga::Handle<naga::GlobalVariable>> {
    loop {
        match function.expressions[expr] {
            naga::Expression::GlobalVariable(handle) => return Some(handle),
            naga::Expression::Access { base, .. } | naga::Expression::AccessIndex { base, .. } => expr = base,
            naga::Expression::Load { pointer } => expr = pointer,
            _ => return None,
        }
    }
}

/// Samplers that sample a depth texture without a comparison or gather an integer texture:
/// those must be non-filtering.
fn non_filtering_samplers(module: &naga::Module) -> BTreeSet<naga::Handle<naga::GlobalVariable>> {
    let mut out = BTreeSet::new();
    let functions = module
        .functions
        .iter()
        .map(|(_, f)| f)
        .chain(module.entry_points.iter().map(|e| &e.function));
    for function in functions {
        for (_, expression) in function.expressions.iter() {
            let naga::Expression::ImageSample {
                image,
                sampler,
                depth_ref,
                ..
            } = *expression
            else {
                continue;
            };
            let (Some(image), Some(sampler)) = (root_global(function, image), root_global(function, sampler))
            else {
                continue;
            };
            let mut ty = module.global_variables[image].ty;
            if let naga::TypeInner::BindingArray { base, .. } = module.types[ty].inner {
                ty = base;
            }
            // A depth texture sampled without a comparison, or an integer texture (gathered),
            // cannot take a filtering sampler.
            let unfilterable = match module.types[ty].inner {
                naga::TypeInner::Image {
                    class: naga::ImageClass::Depth { .. },
                    ..
                } => depth_ref.is_none(),
                naga::TypeInner::Image {
                    class: naga::ImageClass::Sampled { kind, .. },
                    ..
                } => matches!(kind, naga::ScalarKind::Sint | naga::ScalarKind::Uint),
                _ => false,
            };
            if unfilterable {
                out.insert(sampler);
            }
        }
    }
    out
}

/// The bind group layout entry of one global, or why it cannot be expressed.
fn layout_entry(
    module: &naga::Module,
    handle: naga::Handle<naga::GlobalVariable>,
    binding: u32,
    visibility: wgpu::ShaderStages,
    non_filtering: &BTreeSet<naga::Handle<naga::GlobalVariable>>,
) -> Result<wgpu::BindGroupLayoutEntry, String> {
    let var = &module.global_variables[handle];
    let mut ty = var.ty;
    let mut count = None;
    if let naga::TypeInner::BindingArray { base, size } = module.types[ty].inner {
        ty = base;
        count = match size {
            naga::ArraySize::Constant(n) => Some(n),
            _ => NonZeroU32::new(MATRIX_TEXTURE_TABLE_SIZE),
        };
    }
    let binding_ty = match (&var.space, &module.types[ty].inner) {
        (naga::AddressSpace::Uniform, _) => wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        (naga::AddressSpace::Storage { access }, _) => wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage {
                read_only: !access.contains(naga::StorageAccess::STORE),
            },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        (_, naga::TypeInner::Sampler { comparison: true }) => {
            wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison)
        }
        (_, naga::TypeInner::Sampler { comparison: false }) => {
            wgpu::BindingType::Sampler(if non_filtering.contains(&handle) {
                wgpu::SamplerBindingType::NonFiltering
            } else {
                wgpu::SamplerBindingType::Filtering
            })
        }
        (_, naga::TypeInner::Image { dim, arrayed, class }) => {
            let view_dimension = match (dim, arrayed) {
                (naga::ImageDimension::D1, _) => wgpu::TextureViewDimension::D1,
                (naga::ImageDimension::D2, false) => wgpu::TextureViewDimension::D2,
                (naga::ImageDimension::D2, true) => wgpu::TextureViewDimension::D2Array,
                (naga::ImageDimension::D3, _) => wgpu::TextureViewDimension::D3,
                (naga::ImageDimension::Cube, false) => wgpu::TextureViewDimension::Cube,
                (naga::ImageDimension::Cube, true) => wgpu::TextureViewDimension::CubeArray,
            };
            match *class {
                naga::ImageClass::Sampled { kind, multi } => wgpu::BindingType::Texture {
                    sample_type: match kind {
                        naga::ScalarKind::Sint => wgpu::TextureSampleType::Sint,
                        naga::ScalarKind::Uint => wgpu::TextureSampleType::Uint,
                        // Multisampled float textures cannot be filterable.
                        _ => wgpu::TextureSampleType::Float { filterable: !multi },
                    },
                    view_dimension,
                    multisampled: multi,
                },
                naga::ImageClass::Depth { multi } => wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension,
                    multisampled: multi,
                },
                naga::ImageClass::Storage { format, access } => wgpu::BindingType::StorageTexture {
                    access: if access.contains(naga::StorageAccess::ATOMIC) {
                        wgpu::StorageTextureAccess::Atomic
                    } else if access.contains(naga::StorageAccess::LOAD | naga::StorageAccess::STORE) {
                        wgpu::StorageTextureAccess::ReadWrite
                    } else if access.contains(naga::StorageAccess::STORE) {
                        wgpu::StorageTextureAccess::WriteOnly
                    } else {
                        wgpu::StorageTextureAccess::ReadOnly
                    },
                    format: storage_format(format),
                    view_dimension,
                },
                naga::ImageClass::External => wgpu::BindingType::ExternalTexture,
            }
        }
        (_, naga::TypeInner::AccelerationStructure { vertex_return }) => {
            wgpu::BindingType::AccelerationStructure {
                vertex_return: *vertex_return,
            }
        }
        (space, _) => return Err(format!("binding {binding}: no layout for a {space:?} resource")),
    };
    Ok(wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: binding_ty,
        count,
    })
}

/// Entry points `a` and `b` use different globals at one `@group` / `@binding`: they cannot share
/// a pipeline.
fn binding_conflict(prepared: &Prepared, a: usize, b: usize) -> bool {
    let used = |index: usize| -> BTreeMap<(u32, u32), naga::Handle<naga::GlobalVariable>> {
        let info = prepared.info.get_entry_point(index);
        prepared
            .module
            .global_variables
            .iter()
            .filter_map(|(handle, var)| {
                let binding = var.binding?;
                (!info[handle].is_empty()).then_some(((binding.group, binding.binding), handle))
            })
            .collect()
    };
    let (left, right) = (used(a), used(b));
    left.iter()
        .any(|(slot, handle)| right.get(slot).is_some_and(|other| other != handle))
}

/// A storage buffer or storage texture the shader may write.
fn writable(module: &naga::Module, var: &naga::GlobalVariable) -> bool {
    match (&var.space, &module.types[var.ty].inner) {
        (naga::AddressSpace::Storage { access }, _) => access.contains(naga::StorageAccess::STORE),
        (
            _,
            naga::TypeInner::Image {
                class: naga::ImageClass::Storage { access, .. },
                ..
            },
        ) => access.intersects(naga::StorageAccess::STORE | naga::StorageAccess::ATOMIC),
        _ => false,
    }
}

/// The pipeline layout of the entry points `entries` of `prepared`: each global visible to the
/// stages whose entry point uses it.
fn pipeline_layout(
    device: &wgpu::Device,
    label: &str,
    prepared: &Prepared,
    entries: &[usize],
) -> Result<wgpu::PipelineLayout, String> {
    let module = &prepared.module;
    let non_filtering = non_filtering_samplers(module);
    let mut stages = wgpu::ShaderStages::NONE;
    for &index in entries {
        stages |= match module.entry_points[index].stage {
            naga::ShaderStage::Vertex => wgpu::ShaderStages::VERTEX,
            naga::ShaderStage::Fragment => wgpu::ShaderStages::FRAGMENT,
            naga::ShaderStage::Compute => wgpu::ShaderStages::COMPUTE,
            _ => wgpu::ShaderStages::NONE,
        };
    }
    let mut groups: BTreeMap<u32, BTreeMap<u32, wgpu::BindGroupLayoutEntry>> = BTreeMap::new();
    for (handle, var) in module.global_variables.iter() {
        let Some(binding) = var.binding else {
            continue;
        };
        // Visible to every stage of the pipeline when any of its entry points uses it (as the
        // frame's layouts are): naga's per-entry use misses a global a stage reaches only through
        // a pointer argument, which wgpu still requires. A writable storage resource stays off
        // the vertex stage (that needs `VERTEX_WRITABLE_STORAGE`).
        let used = entries
            .iter()
            .any(|&index| !prepared.info.get_entry_point(index)[handle].is_empty());
        if !used {
            continue;
        }
        let mut visibility = stages;
        if writable(module, var) && visibility.contains(wgpu::ShaderStages::FRAGMENT) {
            visibility.remove(wgpu::ShaderStages::VERTEX);
        }
        let entry = layout_entry(module, handle, binding.binding, visibility, &non_filtering)?;
        if groups
            .entry(binding.group)
            .or_default()
            .insert(binding.binding, entry)
            .is_some()
        {
            return Err(format!(
                "two globals at @group({}) @binding({}) in one pipeline",
                binding.group, binding.binding
            ));
        }
    }
    let last = groups.keys().next_back().copied();
    let mut layouts = Vec::new();
    if let Some(last) = last {
        for group in 0..=last {
            let entries: Vec<wgpu::BindGroupLayoutEntry> = groups
                .get(&group)
                .map(|entries| entries.values().cloned().collect())
                .unwrap_or_default();
            layouts.push(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries: &entries,
            }));
        }
    }
    let refs: Vec<Option<&wgpu::BindGroupLayout>> = layouts.iter().map(Some).collect();
    Ok(device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &refs,
        immediate_size: 0,
    }))
}

/// One interleaved vertex buffer for `inputs`: `(stride, attributes)`.
fn vertex_attributes(
    module: &naga::Module,
    inputs: &[(u32, naga::Handle<naga::Type>)],
) -> Result<(u64, Vec<wgpu::VertexAttribute>), String> {
    let mut offset = 0u64;
    let mut attributes = Vec::new();
    for &(location, ty) in inputs {
        let format = vertex_format(module, ty)
            .ok_or_else(|| format!("@location({location}): no vertex format for its type"))?;
        attributes.push(wgpu::VertexAttribute {
            format,
            offset,
            shader_location: location,
        });
        offset += format.size();
    }
    Ok((offset.max(4), attributes))
}

/// Creates the module and a pipeline per entry point on `device`, inside one validation error
/// scope; the validation messages that name `hash` belong to it.
fn device_backend(device: &RenderDevice, hash: &str, file: &str, prepared: &Prepared) -> BackendResult {
    let _span = tracing::debug_span!("shader_matrix_pipelines", file).entered();
    let missing = missing_features(&prepared.module, &device.device);
    if !missing.is_empty() {
        return skipped(
            MatrixBackend::VulkanPipeline,
            &format!("device lacks {}", missing.join(", ")),
        );
    }
    let label = format!("matrix {file} {hash}");
    let d = &device.device;
    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(&label),
        source: wgpu::ShaderSource::Naga(Cow::Owned(prepared.module.clone())),
    });
    let module = &prepared.module;
    let mut built = 0usize;
    let mut problems = Vec::new();
    let vertices: Vec<usize> = module
        .entry_points
        .iter()
        .enumerate()
        .filter(|(_, e)| e.stage == naga::ShaderStage::Vertex)
        .map(|(i, _)| i)
        .collect();
    let depth = wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Depth32Float,
        depth_write_enabled: Some(true),
        depth_compare: Some(wgpu::CompareFunction::LessEqual),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    };
    let render = |vertex: usize, fragment: Option<usize>, problems: &mut Vec<String>| -> bool {
        let vs = &module.entry_points[vertex];
        let mut entries = vec![vertex];
        entries.extend(fragment);
        let layout = match pipeline_layout(d, &label, prepared, &entries) {
            Ok(layout) => layout,
            Err(err) => {
                problems.push(format!("{}: {err}", vs.name));
                return false;
            }
        };
        let (stride, attributes) = match vertex_attributes(module, &location_inputs(module, vs)) {
            Ok(pair) => pair,
            Err(err) => {
                problems.push(format!("{}: {err}", vs.name));
                return false;
            }
        };
        let buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: stride,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes,
        })];
        let buffers: &[Option<wgpu::VertexBufferLayout<'_>>] =
            if attributes.is_empty() { &[] } else { &buffers };
        let mut targets = Vec::new();
        if let Some(fs) = fragment {
            let fs = &module.entry_points[fs];
            for (location, ty) in location_outputs(module, fs) {
                let Some(format) = target_format(module, ty) else {
                    problems.push(format!(
                        "{}: @location({location}) output has no colour format",
                        fs.name
                    ));
                    return false;
                };
                while targets.len() < location as usize {
                    targets.push(None);
                }
                targets.push(Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                }));
            }
        }
        let _pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some(vs.name.as_str()),
                compilation_options: Default::default(),
                buffers,
            },
            fragment: fragment.map(|fs| wgpu::FragmentState {
                module: &shader,
                entry_point: Some(module.entry_points[fs].name.as_str()),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(depth.clone()),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        true
    };
    for (index, entry) in module.entry_points.iter().enumerate() {
        match entry.stage {
            naga::ShaderStage::Compute => match pipeline_layout(d, &label, prepared, &[index]) {
                Ok(layout) => {
                    let _pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: Some(&label),
                        layout: Some(&layout),
                        module: &shader,
                        entry_point: Some(entry.name.as_str()),
                        compilation_options: Default::default(),
                        cache: None,
                    });
                    built += 1;
                }
                Err(err) => problems.push(format!("{}: {err}", entry.name)),
            },
            naga::ShaderStage::Vertex => {
                if render(index, None, &mut problems) {
                    built += 1;
                }
            }
            naga::ShaderStage::Fragment => {
                // A vertex entry whose outputs carry every input this one reads and that binds no
                // other global at one of its bindings (a module can hold several pipelines whose
                // stages reuse a binding slot); the closest one declared before it first.
                let wanted = location_inputs(module, entry);
                let mut candidates: Vec<usize> = vertices
                    .iter()
                    .copied()
                    .filter(|&v| {
                        let outputs = location_outputs(module, &module.entry_points[v]);
                        wanted.iter().all(|(location, ty)| {
                            outputs.iter().any(|(l, t)| {
                                l == location && module.types[*t].inner == module.types[*ty].inner
                            })
                        }) && !binding_conflict(prepared, v, index)
                    })
                    .collect();
                candidates.sort_by_key(|&v| (v > index, index.abs_diff(v)));
                let feeder = candidates.first().copied();
                match feeder {
                    Some(vertex) => {
                        if render(vertex, Some(index), &mut problems) {
                            built += 1;
                        }
                    }
                    None if vertices.is_empty() => {}
                    None => problems.push(format!(
                        "{}: no vertex entry point of the module writes its inputs",
                        entry.name
                    )),
                }
            }
            _ => {}
        }
    }
    if d.poll(wgpu::PollType::Poll).is_err() {
        tracing::warn!(target: "regia_render", "shader-matrix poll failed");
    }
    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
        problems.push(err.to_string());
    }
    if let Ok(mut lines) = LINES.lock() {
        // Messages that name this module; the rest stay for their owner or end up unattributed.
        let (mine, rest): (Vec<String>, Vec<String>) = lines.drain(..).partition(|line| line.contains(hash));
        *lines = rest;
        problems.extend(validation_hits(&mine));
    }
    if problems.is_empty() {
        pass(
            MatrixBackend::VulkanPipeline,
            Some(format!("{built} pipeline(s)")),
        )
    } else {
        fail(MatrixBackend::VulkanPipeline, problems.join("; "))
    }
}

/// The presets [`validate_preset_frames`] renders in a full matrix run: every shipped preset and
/// the lab frame.
fn preset_frame_list() -> Vec<Option<RenderProfilePreset>> {
    let mut presets: Vec<Option<RenderProfilePreset>> = vec![None];
    for preset in RenderProfilePreset::ALL {
        if SHIPPED_PLATFORMS
            .iter()
            .any(|p| platform_presets(p).contains(&preset))
        {
            presets.push(Some(preset));
        }
    }
    presets
}

/// Renders the quad scene of [`super::validate_engine_shaders`] with each preset's profile
/// (`None` is the lab frame) twice on a validation-enabled device, so the frame builds every
/// pipeline that preset draws with its own layouts. One result per preset.
///
/// # Errors
///
/// [`RenderError`] when the device or the log capture cannot be created, or a frame fails to
/// render.
pub fn validate_preset_frames(
    presets: &[Option<RenderProfilePreset>],
) -> Result<Vec<PresetFrameResult>, RenderError> {
    use crate::{FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabVertex, PostSettings};
    install_logger()?;
    let mut flags = crate::device::instance_flags();
    flags.insert(wgpu::InstanceFlags::VALIDATION);
    let device = RenderDevice::headless_with_flags(flags)?;
    let material = LabMaterial {
        base_color: [0.7, 0.7, 0.7, 1.0],
        metallic: 0.0,
        roughness: 0.6,
        ..LabMaterial::EMPTY
    };
    let vertices = [
        LabVertex::from_pos_normal_uv([-1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    let settings = PostSettings {
        auto_exposure: true,
        bloom: true,
        bloom_mips: 3,
        taa: true,
        ..PostSettings::default()
    };
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shader matrix frame"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut out = Vec::new();
    for preset in presets {
        let name = preset.map_or("Lab", |p| p.name()).to_string();
        let _span = tracing::info_span!("shader_matrix_frame", preset = %name).entered();
        if let Ok(mut lines) = LINES.lock() {
            lines.clear();
        }
        let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let quality = preset.map_or(FrameQuality::LAB, |p| {
            FrameQuality::from_profile(&RenderProfile::from_preset(p))
        });
        let scene = super::quad_scene(quality, &material, &vertices, &settings);
        let mut renderer = FrameRenderer::new();
        let mut problems = Vec::new();
        for _ in 0..2 {
            if let Err(err) = renderer.render_into(&device, &scene, FrameTarget::full(&view, format, 64, 64))
            {
                problems.push(err.to_string());
                break;
            }
        }
        drop(renderer);
        if device.device.poll(wgpu::PollType::Poll).is_err() {
            tracing::warn!(target: "regia_render", "shader-matrix poll failed");
        }
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            problems.push(err.to_string());
        }
        if let Ok(mut lines) = LINES.lock() {
            problems.extend(validation_hits(&lines));
            lines.clear();
        }
        out.push(PresetFrameResult {
            preset: name,
            status: if problems.is_empty() {
                BackendStatus::Pass
            } else {
                BackendStatus::Fail
            },
            detail: (!problems.is_empty()).then(|| problems.join("; ")),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pipelines: bool) -> ShaderMatrixOptions {
        ShaderMatrixOptions {
            scope: MatrixScope::Subset,
            pipelines,
            frames: false,
            jobs: 2,
        }
    }

    /// A fixture module: a vertex and fragment pair and a compute entry, with three opt-in
    /// breakages behind defines.
    const FIXTURE: &str = r"
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

struct Local {
    a: vec3<f32>,
    b: f32,
}

@group(0) @binding(0) var<uniform> tint: vec4<f32>;
@group(0) @binding(1) var<storage, read_write> counts: array<u32>;

#ifdef LOOP_SAMPLE
@group(1) @binding(0) var color: texture_2d<f32>;
@group(1) @binding(1) var smp: sampler;
#endif

@vertex
fn vs_main(@location(0) pos: vec3<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    return VsOut(vec4<f32>(pos, 1.0), uv);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
#ifdef BROKEN
    let wrong: f32 = vec2<f32>(1.0, 2.0);
#endif
    var acc = tint * vec4<f32>(in.uv, 0.0, 1.0);
#ifdef LOOP_SAMPLE
    // An implicit-derivative sample inside a loop: FXC must unroll it (ADR-201), DXC need not.
    for (var i = 0u; i < u32(tint.w); i++) {
        acc += textureSample(color, smp, in.uv + vec2<f32>(f32(i)));
    }
#endif
    return acc;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) id: vec3<u32>) {
#ifdef LOCAL_STRUCT
    var local: Local;
    local.b = f32(id.x);
    counts[id.x] = u32(local.b);
#endif
#ifdef UNUSED_AXIS
    // Defined, but nothing reads it: the module is the clean one.
#endif
    counts[id.x] = counts[id.x] + 1u;
}
";

    fn fixture(defines: &[&str]) -> MatrixInput {
        MatrixInput::new(
            "matrix_fixture.wgsl",
            FIXTURE,
            defines,
            format!("test {}", defines.join(" ")),
        )
    }

    fn module_with<'a>(report: &'a ShaderMatrixReport, define: &str) -> &'a MatrixModule {
        report
            .modules
            .iter()
            .find(|m| {
                m.permutations
                    .iter()
                    .any(|p| p.defines.iter().any(|d| d == define))
            })
            .unwrap_or_else(|| panic!("no module composed with {define}: {report:#?}"))
    }

    /// The deliberately broken permutations fail the matrix, each on its own backend, and the
    /// clean permutation of the same source passes every backend.
    #[test]
    fn a_broken_permutation_fails_the_matrix() {
        let inputs = vec![
            fixture(&[]),
            fixture(&["BROKEN"]),
            fixture(&["LOCAL_STRUCT"]),
            fixture(&["LOOP_SAMPLE"]),
        ];
        let report = match run_matrix_inputs(inputs, &options(false)) {
            Ok(report) => report,
            Err(err) => panic!("{err}"),
        };
        assert!(!report.passed());
        assert_eq!(report.failed_modules, 3, "{}", report.failures().join("\n"));
        let clean = report
            .modules
            .iter()
            .find(|m| m.permutations.iter().any(|p| p.defines.is_empty()))
            .unwrap_or_else(|| panic!("no clean module"));
        assert!(clean.ok, "{:#?}", clean.backends);

        let broken = module_with(&report, "BROKEN");
        let failed: Vec<MatrixBackend> = broken
            .backends
            .iter()
            .filter(|r| r.status == BackendStatus::Fail)
            .map(|r| r.backend)
            .collect();
        assert_eq!(failed.len(), 1, "{:#?}", broken.backends);
        assert!(
            matches!(failed[0], MatrixBackend::Compose | MatrixBackend::Naga),
            "{failed:?}"
        );
        assert!(broken.hash.starts_with('x'));

        let local = module_with(&report, "LOCAL_STRUCT");
        assert_eq!(
            local.backend(MatrixBackend::LayoutScan).map(|r| r.status),
            Some(BackendStatus::Fail),
            "{:#?}",
            local.backends
        );
        assert_eq!(
            local.backend(MatrixBackend::Msl).map(|r| r.status),
            Some(BackendStatus::Pass)
        );

        let looped = module_with(&report, "LOOP_SAMPLE");
        assert_eq!(
            looped.backend(MatrixBackend::HlslSm51).map(|r| r.status),
            Some(BackendStatus::Fail),
            "{:#?}",
            looped.backends
        );
        assert_eq!(
            looped.backend(MatrixBackend::HlslSm6).map(|r| r.status),
            Some(BackendStatus::Pass)
        );

        let failures = report.failures();
        assert!(failures.iter().any(|f| f.contains("[BROKEN]")), "{failures:?}");
        assert!(
            failures.iter().any(|f| f.contains("[LOOP_SAMPLE] hlsl_sm51")),
            "{failures:?}"
        );
    }

    /// Two define sets that compose the same module are checked once and listed together; equal
    /// inputs from two origins merge into one composition.
    #[test]
    fn identical_compositions_are_checked_once() {
        let inputs = vec![
            fixture(&[]),
            fixture(&["UNUSED_AXIS"]),
            MatrixInput::new("matrix_fixture.wgsl", FIXTURE, &[], "another origin"),
        ];
        let report = match run_matrix_inputs(inputs, &options(false)) {
            Ok(report) => report,
            Err(err) => panic!("{err}"),
        };
        assert_eq!(report.inputs, 3);
        assert_eq!(report.compositions, 2);
        assert_eq!(report.modules.len(), 1, "{report:#?}");
        let module = &report.modules[0];
        assert_eq!(module.permutations.len(), 2);
        assert_eq!(module.permutations[0].origins, ["another origin", "test "]);
        assert!(report.passed(), "{}", report.failures().join("\n"));
        assert_eq!(
            module.backend(MatrixBackend::VulkanPipeline).map(|r| r.status),
            Some(BackendStatus::Skipped)
        );
    }

    /// The full enumeration covers every shipped platform and preset, the lite tiers, every
    /// forward axis a preset can compose, and a module for every graph pass.
    /// The modules the renderer builds beyond the SPIR-V gate's list (DDGI, baked GI, the display
    /// stage, compute skinning and the rest) are matrix inputs, with the same text and defines.
    #[test]
    fn the_full_enumeration_holds_every_extra_engine_module() {
        let inputs = shader_matrix_inputs(MatrixScope::Full);
        for extra in crate::shader_hlsl::extra_engine_modules() {
            let want: BTreeSet<&str> = extra.defs.unwrap_or(&[]).iter().copied().collect();
            let found = inputs.iter().any(|input| {
                let defs: BTreeSet<&str> = input.defs.keys().map(String::as_str).collect();
                input.file == extra.file
                    && input.source.as_ref() == extra.source
                    && defs == want
                    && input.plain == extra.defs.is_none()
            });
            assert!(found, "{} is not a matrix input", extra.label());
        }
    }

    /// The modules of `every_extra_engine_module_passes_the_cpu_backends` that still fail one
    /// backend, with that backend (ADR-2644 (6)). The matrix itself reports them as failures;
    /// this list only keeps the test a ratchet. In the two ray-query modules the Function
    /// variable is the one naga 30's SPIR-V writer makes inside its `rayQueryGet*Intersection`
    /// helper (the blank intersection struct, `back/spv/ray/query.rs`), not one in the WGSL, so
    /// no WGSL rewrite removes it (ADR-7587). The two baked GI modules left the list with
    /// ADR-7587 (`baked_gi_modules_pass_unrelaxed_spirv_val`).
    const KNOWN_EXTRA_FAILURES: [(&str, MatrixBackend); 2] = [
        ("hwrt_shadow_ray_query.wgsl", MatrixBackend::LayoutScan),
        ("sun_trace_ray_query.wgsl", MatrixBackend::LayoutScan),
    ];

    /// The two baked GI modules, as wgpu receives them, hold no Function or Private variable
    /// with an explicit layout (ADR-7587): their SPIR-V passes the layout scan and plain
    /// `spirv-val --target-env vulkan1.3` with no VUID allowlisted (the gate's
    /// `spirv_val_words` drops `VUID-StandaloneSpirv-None-10684`; this test keeps it, as a
    /// validation layer does at run time).
    #[test]
    fn baked_gi_modules_pass_unrelaxed_spirv_val() {
        let dir = regia_test_utils::unique_temp_dir("baked-gi-spirv");
        let mut checked = 0;
        let mut failures = Vec::new();
        for extra in crate::shader_hlsl::extra_engine_modules() {
            if !extra.file.starts_with("baked_gi_") {
                continue;
            }
            checked += 1;
            let mut module = extra
                .module()
                .unwrap_or_else(|err| panic!("{}: {err}", extra.label()));
            if extra.defs.is_none() {
                crate::specialize_handle_arguments(&mut module);
            }
            let words = super::super::emit_spirv(extra.file, &module).unwrap_or_else(|err| panic!("{err}"));
            if let Err(err) = layout_scan_words(extra.file, &words) {
                failures.push(err);
            }
            let path = dir.join(format!("{}.spv", extra.file));
            let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
            std::fs::write(&path, bytes).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            match std::process::Command::new("spirv-val")
                .args(["--target-env", "vulkan1.3"])
                .arg(&path)
                .output()
            {
                Ok(output) if output.status.success() => {}
                Ok(output) => failures.push(format!(
                    "{}: spirv-val --target-env vulkan1.3 {}:\n{}{}",
                    extra.file,
                    path.display(),
                    String::from_utf8_lossy(&output.stderr),
                    String::from_utf8_lossy(&output.stdout)
                )),
                Err(err)
                    if err.kind() == std::io::ErrorKind::NotFound
                        && super::super::spirv_val_fallback_enabled() =>
                {
                    eprintln!(
                        "spirv-val not installed: {} checked by the layout scan only",
                        extra.file
                    );
                }
                Err(err) => panic!("spirv-val: {err}"),
            }
        }
        assert_eq!(checked, 2, "baked_gi_screen.wgsl and baked_gi_points.wgsl");
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every module the renderer builds beyond the SPIR-V gate's list passes every CPU backend
    /// (compose or plain parse, naga, the layout scan, `spirv-val`, HLSL 6 and 5.1, MSL), except
    /// the exact (module, backend) pairs of `KNOWN_EXTRA_FAILURES`: any other failure fails, and a
    /// listed pair that passes fails too, so the list only shrinks. Before ADR-2644 (6) the four
    /// plain-WGSL modules failed `compose` (the composer's capabilities reject ray queries and
    /// `unpack2x16float`), and five modules failed the layout scan with a function-local struct
    /// or array `var` (ADR-026): `skin_compute.wgsl`, `ddgi_screen.wgsl`, `ddgi_trace.wgsl`,
    /// `radiance_cache_update.wgsl` and `ssgi_cached.wgsl`.
    #[test]
    fn every_extra_engine_module_passes_the_cpu_backends() {
        let inputs: Vec<MatrixInput> = crate::shader_hlsl::extra_engine_modules()
            .into_iter()
            .map(|extra| match extra.defs {
                Some(defs) => MatrixInput::new(extra.file, extra.source, defs, "engine"),
                None => MatrixInput::plain(extra.file, extra.source, "engine"),
            })
            .collect();
        let count = inputs.len();
        let report = match run_matrix_inputs(
            inputs,
            &ShaderMatrixOptions {
                scope: MatrixScope::Subset,
                pipelines: false,
                frames: false,
                jobs: 2,
            },
        ) {
            Ok(report) => report,
            Err(err) => panic!("{err}"),
        };
        assert!(report.modules.len() >= count / 2, "{}", report.summary());
        let mut unexpected = Vec::new();
        for module in &report.modules {
            for result in &module.backends {
                let known = KNOWN_EXTRA_FAILURES.contains(&(module.file.as_str(), result.backend));
                match (result.status, known) {
                    (BackendStatus::Fail, false) => unexpected.push(format!(
                        "{} {}: {}",
                        module.label(),
                        result.backend.name(),
                        result.detail.as_deref().unwrap_or("")
                    )),
                    (BackendStatus::Fail, true) => {}
                    (_, true) => unexpected.push(format!(
                        "{} {} no longer fails: remove it from KNOWN_EXTRA_FAILURES",
                        module.label(),
                        result.backend.name()
                    )),
                    (_, false) => {}
                }
            }
        }
        assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
        assert!(report.unattributed.is_empty(), "{:?}", report.unattributed);
        // The plain modules reached every later backend (no composition failure), the five
        // modules ADR-2644 (6) rewrote and the two baked GI modules ADR-7587 rewrote pass the
        // layout scan.
        for file in [
            "hwrt_shadow_ray_query.wgsl",
            "baked_gi_points.wgsl",
            "baked_gi_screen.wgsl",
            "skin_compute.wgsl",
            "ddgi_screen.wgsl",
            "ddgi_trace.wgsl",
            "radiance_cache_update.wgsl",
            "ssgi_cached.wgsl",
        ] {
            let module = report
                .modules
                .iter()
                .find(|m| m.file == file)
                .unwrap_or_else(|| panic!("{file} not in the report"));
            assert_eq!(
                module.backend(MatrixBackend::Compose).map(|r| r.status),
                Some(BackendStatus::Pass),
                "{file}"
            );
            if !KNOWN_EXTRA_FAILURES.iter().any(|(f, _)| *f == file) {
                assert!(module.ok, "{}", module.label());
            }
        }
    }

    /// Correctness audit (render frame/upscaler): an Adreno permutation carries every define the
    /// runtime workaround adds (`shader_workarounds::workaround_defs(true)` through
    /// `bindless::add_material_defs`). It used to carry `SUN_SHADOW_PCF_ONLY` and
    /// `STUDIO_IBL_OFF` only, so no module was ever checked with `FORWARD_TRIM`, which every
    /// forward module an Adreno device builds is composed with.
    #[test]
    fn adreno_permutations_carry_every_workaround_define() {
        let mut adreno = 0;
        for preset in platform_presets("android") {
            for permutation in forward_permutations_for("android", preset) {
                if !permutation.axes.split(' ').any(|axis| axis == "adreno") {
                    continue;
                }
                adreno += 1;
                for name in crate::shader_workarounds::workaround_defs(true) {
                    assert!(
                        permutation.defs.iter().any(|(have, _)| have == name),
                        "{} [{}] lacks {name}",
                        permutation.file,
                        permutation.axes
                    );
                }
                let mut names: Vec<&str> = permutation.defs.iter().map(|(n, _)| n.as_str()).collect();
                let all = names.len();
                names.sort_unstable();
                names.dedup();
                assert_eq!(names.len(), all, "a define twice: [{}]", permutation.axes);
            }
        }
        assert!(adreno > 0, "android composes Adreno permutations");
    }

    #[test]
    fn the_full_enumeration_covers_every_preset_and_axis() {
        let inputs = shader_matrix_inputs(MatrixScope::Full);
        for platform in SHIPPED_PLATFORMS {
            for preset in platform_presets(platform) {
                let prefix = format!("{platform}/{} ", preset.name());
                assert!(
                    inputs.iter().any(|i| i.origin.starts_with(&prefix)),
                    "{prefix} missing"
                );
                assert!(
                    inputs
                        .iter()
                        .any(|i| i.origin.starts_with(&prefix) && i.origin.contains(" graph ")),
                    "{prefix} has no graph pass"
                );
            }
        }
        assert!(!inputs.iter().any(|i| i.origin.contains("(no module)")));
        let defines: BTreeSet<String> = inputs
            .iter()
            .flat_map(|i| define_key(&i.defs))
            .map(|d| d.split('=').next().unwrap_or(&d).to_string())
            .collect();
        // Axes the matrix does not compose: the visibility resolve's own entry (its modules are
        // the engine's `vis_*` list) and residency feedback (a device-and-process opt-in that only
        // adds stores to the table path).
        let not_composed = ["VISBUFFER", "VIS_SINGLE_PASS", "RESIDENCY_FEEDBACK"];
        for axis in crate::forward_permutations::FORWARD_AXES {
            if not_composed.contains(&axis.name) {
                continue;
            }
            assert!(defines.contains(axis.name), "axis {} never composed", axis.name);
        }
        for origin in ["android/MobileLow lite low", "android/MobileHigh lite high"] {
            assert!(inputs.iter().any(|i| i.origin.starts_with(origin)), "{origin}");
        }
        assert!(
            inputs
                .iter()
                .any(|i| i.origin.contains("studio_off") && i.origin.contains("thin_film"))
        );
        // Every pass of every listed graph set has a module.
        for pass in crate::permutation_policy::graph_set_passes(true, true) {
            assert!(
                inputs.iter().any(|i| i.origin.ends_with(&format!(" {pass}"))),
                "graph pass {pass} not composed"
            );
        }
    }

    /// The fixed subset composes, validates and builds its pipelines on every backend. It is the
    /// matrix the default test run checks; `check-shaders --matrix` runs the full one.
    #[test]
    fn the_matrix_subset_passes_every_backend() {
        let report = match run_shader_matrix(&ShaderMatrixOptions {
            scope: MatrixScope::Subset,
            pipelines: true,
            frames: false,
            jobs: 2,
        }) {
            Ok(report) => report,
            Err(err) => panic!("{err}"),
        };
        assert!(report.passed(), "{}", report.failures().join("\n"));
        assert_eq!(report.scope, "subset");
        assert!(report.modules.len() >= 4, "{}", report.summary());
        for module in &report.modules {
            for backend in MatrixBackend::ALL {
                let status = module.backend(backend).map(|r| r.status);
                assert_eq!(
                    status,
                    Some(BackendStatus::Pass),
                    "{} {}",
                    module.label(),
                    backend.name()
                );
            }
        }
        let json = match report.to_json() {
            Ok(json) => json,
            Err(err) => panic!("{err}"),
        };
        assert!(json.contains("\"format_version\": 1"));
        assert!(json.contains("\"vulkan_pipeline\""));
        match ShaderMatrixReport::from_json(&json) {
            Ok(back) => assert_eq!(back, report),
            Err(err) => panic!("{err}"),
        }
    }

    #[test]
    fn define_keys_sort_and_drop_false_booleans() {
        let mut defs = bool_defs(&["B", "A"]);
        defs.insert("OFF".into(), ShaderDefValue::Bool(false));
        defs.insert("SIZE".into(), ShaderDefValue::UInt(8));
        assert_eq!(define_key(&defs), ["A", "B", "SIZE=8"]);
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
