// SPDX-License-Identifier: MPL-2.0
//! Shader-permutation policy (`docs/master-plan.md` § 5.5a, ADR-2026): the cook enumerates every
//! material-graph pipeline a project can use per render profile, a budget per profile is checked
//! in the cook report and by `ship_check`, and the runtime refuses to build a graph pipeline set
//! that is not on the cooked list (a warning in dev builds, the standard surface in shipping).
//!
//! The engine's own pipeline set is fixed: it does not grow with project content, the warm list
//! covers it and [`crate::forward_permutations`] keeps its define axes closed. A material graph is
//! the open axis: each graph code compiles its own pipeline set ([`crate::GraphTarget`] per
//! render target), so this module counts those.
//!
//! - [`enumerate_profile`] lists, for one profile, every graph pipeline set and the passes in it
//!   (`graph_set_passes` mirrors the pipelines `graph_pipelines::build_set` creates; a lavapipe test
//!   counts the creates and compares).
//! - [`PermutationBudget`] is the per-profile ceiling: pipelines and an estimated cold compile
//!   time ([`CompileCostModel`]).
//! - [`PermutationManifest`] is the cooked list (`permutations.json` in the platform's cooked
//!   cache, [`PERMUTATION_MANIFEST_FORMAT_VERSION`]).
//! - [`PermutationGuard`] is the runtime side: installed on a [`crate::FrameRenderer`]
//!   ([`crate::FrameRenderer::set_permutation_guard`]), it admits a graph the list names, and for an
//!   unlisted one either warns and builds ([`PermutationEnforcement::Warn`]) or refuses it, so its
//!   draws keep the standard surface of the same pass and nothing is built
//!   ([`PermutationEnforcement::Fallback`]). Neither stalls the frame: async mode builds on workers.
//! - [`startup_guard`] picks the list a process starts with: `content/permutations.json` in a
//!   package, the cooked cache in a dev run. `RenderPlugin` installs it at startup.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{RenderProfile, RenderProfilePreset};

/// Version of [`PermutationManifest`]'s JSON shape. Bump it with a migration note in
/// `docs/formats/permutations.md` when the shape changes.
pub const PERMUTATION_MANIFEST_FORMAT_VERSION: u32 = 1;

/// File name of the cooked permutation manifest inside a platform's cooked cache directory.
pub const PERMUTATION_MANIFEST_FILE: &str = "permutations.json";

/// The passes of a graph pipeline set on every target (`graph_pipelines::build_set`).
pub const GRAPH_BASE_PASSES: [&str; 6] = [
    "depth",
    "forward",
    "blend",
    "premul",
    "additive",
    "transmit_multiply",
];

/// The instanced GPU-driven passes a set adds on a GPU-driven target.
pub const GRAPH_INSTANCED_PASSES: [&str; 4] = [
    "forward_instanced",
    "forward_mask_instanced",
    "depth_instanced",
    "depth_mask_instanced",
];

/// Shader modules the base passes compile (one forward module for all six).
const BASE_MODULES: u32 = 1;
/// Shader modules the instanced passes compile (colour opaque/mask, depth opaque/mask).
const INSTANCED_MODULES: u32 = 4;

/// The render-target features a profile draws graphs with, as far as the cook can know them.
///
/// The device can still change two of them: a device without GPU-driven indirect draws drops
/// [`Self::instanced`], and one that cannot render `Rg11b10Ufloat` draws to `Rgba16Float`. Both
/// only remove pipelines or change the colour format of the same pipelines, so they do not add to
/// the cooked count, and the guard does not compare them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileTarget {
    /// Colour format class: `rgba16f` or `rg11b10` (the profile's HDR target; runtime frames are
    /// linear HDR).
    pub format: &'static str,
    /// MSAA sample count (1 or 4).
    pub samples: u32,
    /// The GPU-driven instanced variants are built.
    pub instanced: bool,
}

impl ProfileTarget {
    /// The target `profile` draws graphs with on a device that has every feature it asks for.
    #[must_use]
    pub fn of(profile: &RenderProfile) -> Self {
        let samples = if matches!(profile.anti_aliasing, crate::AntiAliasing::Msaa4x) {
            4
        } else {
            1
        };
        Self {
            format: match profile.hdr_target {
                crate::HdrTarget::Rg11B10 => "rg11b10",
                crate::HdrTarget::Rgba16f => "rgba16f",
            },
            samples,
            // `prewarm_material_graphs` normalises the same way: instancing is single-sample.
            instanced: profile.gpu_driven && samples <= 1,
        }
    }

    /// The [`crate::GraphTarget`] the runtime builds this target's graph sets for on a device with
    /// `capabilities` (ADR-4901): the profile's HDR format, or `Rgba16Float` where `Rg11b10Ufloat`
    /// does not render, linear (no sRGB encode), single-sided, its sample count, and the instanced
    /// variants only where the device draws GPU-driven. The same set count as the cook's, minus
    /// the instanced passes a device without GPU-driven draws drops.
    #[must_use]
    pub fn graph_target(&self, capabilities: &crate::RenderCapabilities) -> crate::GraphTarget {
        let format = if self.format == "rg11b10" && capabilities.rg11b10_renderable {
            wgpu::TextureFormat::Rg11b10Ufloat
        } else {
            wgpu::TextureFormat::Rgba16Float
        };
        let gpu_driven = crate::gpu_driven::IndirectStrategy::select(capabilities).uses_gpu_objects();
        crate::GraphTarget {
            format,
            encode: false,
            double_sided: false,
            samples: self.samples,
            instanced: self.instanced && gpu_driven && self.samples <= 1,
            sun_rt: false,
        }
    }
}

/// One material-graph code the project can draw: the cook's input to the enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPermutationInput {
    /// Pipeline cache key ([`crate::GraphCode::key`]).
    pub key: u64,
    /// Where it came from (the cooked source path), for reports.
    pub name: String,
    /// The graph has a vertex stage (world position offset): shadow and velocity passes.
    pub vertex: bool,
    /// The graph reads the second UV set.
    pub uv1: bool,
    /// Bytes of generated WGSL (surface plus vertex stage).
    pub code_bytes: usize,
    /// The material is drawn double-sided in a culled frame (a second set, cull none).
    pub double_sided: bool,
}

impl GraphPermutationInput {
    /// The input of `code` named `name`, single-sided.
    #[must_use]
    pub fn from_code(name: impl Into<String>, code: &crate::GraphCode) -> Self {
        Self {
            key: code.key(),
            name: name.into(),
            vertex: code.vertex.is_some(),
            uv1: code.uv1,
            code_bytes: code.surface.len() + code.vertex.as_ref().map_or(0, |v| v.len()),
            double_sided: false,
        }
    }
}

/// The passes (one pipeline each) of a graph set built with or without the instanced variants,
/// for a graph with or without a vertex stage (`graph_pipelines::build_set`).
#[must_use]
pub fn graph_set_passes(instanced: bool, vertex: bool) -> Vec<&'static str> {
    let mut passes: Vec<&'static str> = GRAPH_BASE_PASSES.to_vec();
    if instanced {
        passes.extend_from_slice(&GRAPH_INSTANCED_PASSES);
    }
    if vertex {
        passes.push("shadow");
        if instanced {
            passes.push("shadow_instanced");
        }
        passes.push("velocity");
    }
    passes
}

/// Shader modules a graph set compiles ([`graph_set_passes`]'s set).
#[must_use]
pub fn graph_set_modules(instanced: bool, vertex: bool) -> u32 {
    let mut modules = BASE_MODULES;
    if instanced {
        modules += INSTANCED_MODULES;
    }
    if vertex {
        // CPU shadow, the instanced shadow, velocity.
        modules += 1 + u32::from(instanced) + 1;
    }
    modules
}

/// Estimated cold compile cost of one pipeline set (no driver cache): per shader module, per
/// pipeline and per KiB of generated code. Provisional constants (ADR-2026): calibrate them from
/// `REGIA_PIPELINE_TIMING=1` runs on the reference devices.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompileCostModel {
    /// Milliseconds per composed and validated shader module.
    pub module_ms: f64,
    /// Milliseconds per pipeline the driver compiles.
    pub pipeline_ms: f64,
    /// Extra milliseconds per KiB of generated graph code, per module.
    pub code_kib_ms: f64,
}

impl CompileCostModel {
    /// Desktop-class devices (Desktop presets and the Deck).
    pub const DESKTOP: Self = Self {
        module_ms: 30.0,
        pipeline_ms: 12.0,
        code_kib_ms: 2.0,
    };
    /// Phones (Mobile presets): slower CPUs and drivers.
    pub const MOBILE: Self = Self {
        module_ms: 90.0,
        pipeline_ms: 40.0,
        code_kib_ms: 6.0,
    };

    /// The model of `preset`.
    #[must_use]
    pub const fn for_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow | RenderProfilePreset::MobileHigh => Self::MOBILE,
            _ => Self::DESKTOP,
        }
    }

    /// Estimated milliseconds of `modules` modules of `code_bytes` generated code and `pipelines`
    /// pipelines.
    #[must_use]
    pub fn estimate_ms(&self, modules: u32, pipelines: u32, code_bytes: usize) -> f64 {
        let kib = code_bytes as f64 / 1024.0;
        f64::from(modules) * (self.module_ms + kib * self.code_kib_ms)
            + f64::from(pipelines) * self.pipeline_ms
    }
}

/// The ceiling one profile's material-graph pipelines must stay under (`ship_check` fails above
/// it). Provisional numbers (ADR-2026); a project overrides them in `Regia.toml`
/// (`[cook.permutation_budget.<Profile>]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermutationBudget {
    /// Most graph pipelines.
    pub max_pipelines: u32,
    /// Most estimated cold compile time, milliseconds.
    pub max_compile_ms: u64,
}

impl PermutationBudget {
    /// Desktop presets and the Deck.
    pub const DESKTOP: Self = Self {
        max_pipelines: 4096,
        max_compile_ms: 120_000,
    };
    /// Mobile presets.
    pub const MOBILE: Self = Self {
        max_pipelines: 1024,
        max_compile_ms: 90_000,
    };

    /// The default budget of `preset`.
    #[must_use]
    pub const fn for_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow | RenderProfilePreset::MobileHigh => Self::MOBILE,
            _ => Self::DESKTOP,
        }
    }
}

/// The real-time presets a platform's package can run (`linux`, `windows`, `macos`, `android`):
/// the profiles the cook enumerates. The offline Cinematic preset is never a game frame.
#[must_use]
pub fn platform_presets(platform: &str) -> Vec<RenderProfilePreset> {
    use RenderProfilePreset as P;
    match platform {
        "android" => vec![P::MobileLow, P::MobileHigh],
        "linux" => vec![P::DesktopLow, P::DesktopHigh, P::DesktopUltra, P::Deck],
        _ => vec![P::DesktopLow, P::DesktopHigh, P::DesktopUltra],
    }
}

/// Every platform the cook enumerates a package for ([`platform_presets`]); the shader matrix
/// (`cargo xtask check-shaders --matrix`, ADR-2644) walks them all.
pub const SHIPPED_PLATFORMS: [&str; 4] = ["linux", "windows", "macos", "android"];

/// The shader module one graph pass compiles (`graph_pipelines::build_set`, one pass per name of
/// [`graph_set_passes`]): the host source and the defines added to the graph's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphPassModule {
    /// The graph's forward module with `extra` defines; with `target_defs` the target's
    /// `skip_srgb_encode` (linear targets) and `SUN_SHADOW_RT` (a traced sun) apply as well.
    Forward {
        /// Defines added to the graph's.
        extra: &'static [&'static str],
        /// The target's encode and sun-trace defines apply.
        target_defs: bool,
    },
    /// The vertex host's shadow module with `extra` defines.
    Shadow {
        /// Defines added to the vertex host's.
        extra: &'static [&'static str],
    },
    /// The vertex host's velocity module.
    Velocity,
}

/// The module the graph pass `pass` compiles, or `None` for a name [`graph_set_passes`] never
/// lists.
#[must_use]
pub fn graph_pass_module(pass: &str) -> Option<GraphPassModule> {
    const MASK: &[&str] = &["ALPHA_TEST"];
    Some(match pass {
        p if GRAPH_BASE_PASSES.contains(&p) => GraphPassModule::Forward {
            extra: MASK,
            target_defs: true,
        },
        "forward_instanced" => GraphPassModule::Forward {
            extra: &["GPU_DRIVEN", "GPU_INSTANCED"],
            target_defs: true,
        },
        "forward_mask_instanced" => GraphPassModule::Forward {
            extra: &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"],
            target_defs: true,
        },
        "depth_instanced" => GraphPassModule::Forward {
            extra: &["GPU_DRIVEN"],
            target_defs: false,
        },
        "depth_mask_instanced" => GraphPassModule::Forward {
            extra: &["GPU_DRIVEN", "ALPHA_TEST"],
            target_defs: false,
        },
        "shadow" => GraphPassModule::Shadow { extra: &[] },
        "shadow_instanced" => GraphPassModule::Shadow {
            extra: &["GPU_DRIVEN"],
        },
        "velocity" => GraphPassModule::Velocity,
        _ => return None,
    })
}

/// One listed graph pipeline set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermutationEntry {
    /// [`crate::GraphCode::key`] as 16 hex digits (a JSON number would lose bits).
    pub key: String,
    /// The cooked source the code came from.
    pub name: String,
    /// Built cull-none (a double-sided material in a culled frame).
    pub double_sided: bool,
    /// The passes, one pipeline each.
    pub passes: Vec<String>,
}

/// One profile's enumeration and its budget check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfilePermutations {
    /// Preset name ([`RenderProfilePreset::name`]).
    pub profile: String,
    /// The target the profile draws graphs with.
    pub target: ProfileTargetJson,
    /// Every listed graph pipeline set, sorted by key then sidedness.
    pub entries: Vec<PermutationEntry>,
    /// Pipelines in all entries.
    pub pipelines: u32,
    /// Shader modules in all entries.
    pub modules: u32,
    /// Generated graph code bytes, once per distinct code.
    pub code_bytes: u64,
    /// Estimated cold compile time of every entry, milliseconds ([`CompileCostModel`]).
    pub estimated_compile_ms: u64,
    /// The budget checked.
    pub budget: PermutationBudget,
    /// [`Self::pipelines`] or [`Self::estimated_compile_ms`] is above [`Self::budget`].
    pub over_budget: bool,
}

impl ProfilePermutations {
    /// One line for the cook report: `DesktopHigh: 3 sets, 30 pipelines (budget 4096), ~0.4 s cold (budget 120 s)`.
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "{}: {} graph sets, {} pipelines (budget {}), {} modules, {} B code, ~{:.1} s cold compile (budget {} s){}",
            self.profile,
            self.entries.len(),
            self.pipelines,
            self.budget.max_pipelines,
            self.modules,
            self.code_bytes,
            self.estimated_compile_ms as f64 / 1000.0,
            self.budget.max_compile_ms / 1000,
            if self.over_budget { " OVER BUDGET" } else { "" }
        )
    }

    /// Why the profile is over budget, or `None`.
    #[must_use]
    pub fn budget_problem(&self) -> Option<String> {
        let mut problems = Vec::new();
        if self.pipelines > self.budget.max_pipelines {
            problems.push(format!(
                "{} material-graph pipelines, budget {}",
                self.pipelines, self.budget.max_pipelines
            ));
        }
        if self.estimated_compile_ms > self.budget.max_compile_ms {
            problems.push(format!(
                "~{} ms estimated cold compile, budget {} ms",
                self.estimated_compile_ms, self.budget.max_compile_ms
            ));
        }
        (!problems.is_empty()).then(|| format!("{}: {}", self.profile, problems.join("; ")))
    }
}

/// [`ProfileTarget`] in the manifest (owned strings, so it deserialises).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileTargetJson {
    /// Colour format class.
    pub format: String,
    /// MSAA sample count.
    pub samples: u32,
    /// Instanced GPU-driven variants built.
    pub instanced: bool,
}

impl From<ProfileTarget> for ProfileTargetJson {
    fn from(target: ProfileTarget) -> Self {
        Self {
            format: target.format.to_string(),
            samples: target.samples,
            instanced: target.instanced,
        }
    }
}

/// Lists every graph pipeline set `graphs` need on `preset` and checks it against `budget`.
/// Duplicate inputs (the same code from several instances) list once.
#[must_use]
pub fn enumerate_profile(
    preset: RenderProfilePreset,
    graphs: &[GraphPermutationInput],
    budget: PermutationBudget,
    cost: CompileCostModel,
) -> ProfilePermutations {
    let target = ProfileTarget::of(&RenderProfile::from_preset(preset));
    // (key, double_sided) → (first name, vertex, code bytes).
    let mut sets: BTreeMap<(u64, bool), (&str, bool, usize)> = BTreeMap::new();
    let mut codes: BTreeMap<u64, usize> = BTreeMap::new();
    for graph in graphs {
        sets.entry((graph.key, graph.double_sided)).or_insert((
            graph.name.as_str(),
            graph.vertex,
            graph.code_bytes,
        ));
        codes.entry(graph.key).or_insert(graph.code_bytes);
    }
    let (mut pipelines, mut modules, mut compile_ms) = (0u32, 0u32, 0.0f64);
    let entries: Vec<PermutationEntry> = sets
        .iter()
        .map(|(&(key, double_sided), &(name, vertex, code_bytes))| {
            let passes = graph_set_passes(target.instanced, vertex);
            let set_modules = graph_set_modules(target.instanced, vertex);
            let set_pipelines = passes.len() as u32;
            pipelines += set_pipelines;
            modules += set_modules;
            compile_ms += cost.estimate_ms(set_modules, set_pipelines, code_bytes);
            PermutationEntry {
                key: format!("{key:016x}"),
                name: name.to_string(),
                double_sided,
                passes: passes.into_iter().map(str::to_string).collect(),
            }
        })
        .collect();
    let estimated_compile_ms = compile_ms.ceil() as u64;
    ProfilePermutations {
        profile: preset.name().to_string(),
        target: target.into(),
        entries,
        pipelines,
        modules,
        code_bytes: codes.values().map(|&b| b as u64).sum(),
        estimated_compile_ms,
        budget,
        over_budget: pipelines > budget.max_pipelines || estimated_compile_ms > budget.max_compile_ms,
    }
}

/// The serde shape of `permutations.json` for `cargo xtask check-formats`
/// (`docs/formats/permutations.md`, ADR-2316): the manifest and every struct it reaches.
#[must_use]
pub fn permutation_manifest_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<PermutationManifest>(),
        serde_field_names::<ProfilePermutations>(),
        serde_field_names::<ProfileTargetJson>(),
        serde_field_names::<PermutationEntry>(),
        serde_field_names::<PermutationBudget>(),
    ])
}

/// `permutations.json` is at its first version: no migration steps (a cook output: an older file is
/// cooked again).
#[must_use]
pub fn permutation_manifest_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The cooked permutation list of one platform (`permutations.json`, `docs/formats/permutations.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermutationManifest {
    /// [`PERMUTATION_MANIFEST_FORMAT_VERSION`].
    pub format_version: u32,
    /// Cook platform tag.
    pub platform: String,
    /// Graph codes the project draws (after de-duplication).
    pub graphs: usize,
    /// One enumeration per profile the platform runs.
    pub profiles: Vec<ProfilePermutations>,
}

/// Why a manifest could not be read.
#[derive(Debug, thiserror::Error)]
pub enum PermutationManifestError {
    /// Not JSON of the manifest's shape.
    #[error("permutation manifest is not valid: {0}")]
    Parse(#[from] serde_json::Error),
    /// Written by a newer engine.
    #[error("permutation manifest format_version {found} is newer than this engine's {supported}; recook")]
    Version {
        /// Version in the file.
        found: u32,
        /// [`PERMUTATION_MANIFEST_FORMAT_VERSION`].
        supported: u32,
    },
}

impl PermutationManifest {
    /// Enumerates every profile of `platform` with the default budgets, overridden per profile
    /// name by `budgets`.
    #[must_use]
    pub fn enumerate(
        platform: &str,
        graphs: &[GraphPermutationInput],
        budgets: &BTreeMap<String, PermutationBudget>,
    ) -> Self {
        let distinct: BTreeSet<u64> = graphs.iter().map(|g| g.key).collect();
        let profiles = platform_presets(platform)
            .into_iter()
            .map(|preset| {
                let budget = budgets
                    .get(preset.name())
                    .copied()
                    .unwrap_or_else(|| PermutationBudget::for_preset(preset));
                enumerate_profile(preset, graphs, budget, CompileCostModel::for_preset(preset))
            })
            .collect();
        Self {
            format_version: PERMUTATION_MANIFEST_FORMAT_VERSION,
            platform: platform.to_string(),
            graphs: distinct.len(),
            profiles,
        }
    }

    /// The manifest as pretty JSON.
    ///
    /// # Errors
    ///
    /// Serialisation failure (not expected for this shape).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parses a manifest. Version 1 is the only shape so far.
    ///
    /// # Errors
    ///
    /// [`PermutationManifestError`] for bad JSON or a newer version.
    pub fn from_json(text: &str) -> Result<Self, PermutationManifestError> {
        let manifest: Self = serde_json::from_str(text)?;
        if manifest.format_version > PERMUTATION_MANIFEST_FORMAT_VERSION {
            return Err(PermutationManifestError::Version {
                found: manifest.format_version,
                supported: PERMUTATION_MANIFEST_FORMAT_VERSION,
            });
        }
        Ok(manifest)
    }

    /// The enumeration of `profile` (a preset name, any spelling [`RenderProfilePreset::from_name`] takes).
    #[must_use]
    pub fn profile(&self, profile: &str) -> Option<&ProfilePermutations> {
        let name: &str = match RenderProfilePreset::from_name(profile) {
            Some(preset) => preset.name(),
            None => profile,
        };
        self.profiles.iter().find(|p| p.profile == name)
    }

    /// Every profile's budget problem.
    #[must_use]
    pub fn budget_problems(&self) -> Vec<String> {
        self.profiles
            .iter()
            .filter_map(ProfilePermutations::budget_problem)
            .collect()
    }
}

/// What the runtime does with a graph pipeline set that is not on the cooked list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermutationEnforcement {
    /// No check (no manifest installed; labs, editors, tests).
    #[default]
    Off,
    /// Build it and log a warning once per graph (dev builds).
    Warn,
    /// Never build it: its draws use the standard surface of the same pass (shipping builds).
    Fallback,
}

impl PermutationEnforcement {
    /// The mode of this build: [`Self::Warn`] with the `dev-assets` feature (dev runs), else
    /// [`Self::Fallback`].
    #[must_use]
    pub const fn for_build() -> Self {
        if cfg!(feature = "dev-assets") {
            Self::Warn
        } else {
            Self::Fallback
        }
    }

    /// Stable snake-case name (agent protocol).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Fallback => "fallback",
        }
    }

    /// The mode named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Off, Self::Warn, Self::Fallback]
            .into_iter()
            .find(|mode| mode.name() == name)
    }
}

/// What [`PermutationGuard::admit`] decided for one graph set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermutationAdmission {
    /// On the list (or no check): build it.
    Listed,
    /// Not on the list, built anyway with a warning ([`PermutationEnforcement::Warn`]).
    Unlisted,
    /// Not on the list and refused: draw the standard surface ([`PermutationEnforcement::Fallback`]).
    Refused,
}

/// One profile's totals on the installed list (`render.permutations` → `guard.cooked`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermutationProfileCount {
    /// Preset name.
    pub profile: String,
    /// Graph pipeline sets.
    pub sets: usize,
    /// Pipelines in those sets.
    pub pipelines: u32,
    /// Shader modules in those sets.
    pub modules: u32,
    /// Generated graph code bytes, once per distinct code.
    pub code_bytes: u64,
    /// Estimated cold compile time, milliseconds.
    pub estimated_compile_ms: u64,
    /// The budget the cook checked.
    pub budget: PermutationBudget,
    /// The profile is over [`Self::budget`].
    pub over_budget: bool,
}

impl From<&ProfilePermutations> for PermutationProfileCount {
    fn from(listed: &ProfilePermutations) -> Self {
        Self {
            profile: listed.profile.clone(),
            sets: listed.entries.len(),
            pipelines: listed.pipelines,
            modules: listed.modules,
            code_bytes: listed.code_bytes,
            estimated_compile_ms: listed.estimated_compile_ms,
            budget: listed.budget,
            over_budget: listed.over_budget,
        }
    }
}

/// Counters of a [`PermutationGuard`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PermutationGuardStats {
    /// The mode's name.
    pub enforcement: &'static str,
    /// The profile whose list is installed (`None`: no list).
    pub profile: Option<String>,
    /// Where the list came from (a `permutations.json` path, or `agent`).
    pub source: Option<String>,
    /// Graph sets on the installed list.
    pub listed_sets: usize,
    /// Pipelines the installed list allows.
    pub listed_pipelines: u32,
    /// Sets admitted because their graph is listed.
    pub admitted: u64,
    /// Of [`Self::admitted`], sets on a target the list did not count: the other sidedness, or a
    /// sample count the profile does not draw. Same code and bounded, but above the cooked count.
    pub off_target: u64,
    /// Unlisted sets built with a warning.
    pub unlisted_built: u64,
    /// Unlisted sets refused (their draws use the standard surface).
    pub refused: u64,
    /// The first unlisted graph keys met (16 hex digits), at most [`MAX_REPORTED_KEYS`].
    pub unlisted_keys: Vec<String>,
    /// The list's totals per profile.
    pub cooked: Vec<PermutationProfileCount>,
}

/// Most unlisted keys [`PermutationGuardStats::unlisted_keys`] keeps.
pub const MAX_REPORTED_KEYS: usize = 32;

/// The runtime check of graph pipeline sets against the cooked list of the active profile.
///
/// The check is on the graph code, because code is the axis content can grow without bound. The
/// target decides nothing: the same code on the other sidedness (a scene that turns culling off)
/// or another sample count (a probe capture on an MSAA profile, a device without 4x) is a known
/// shader, so it is admitted and counted in [`PermutationGuardStats::off_target`] rather than
/// drawn with the wrong surface.
#[derive(Debug, Clone, Default)]
pub struct PermutationGuard {
    enforcement: PermutationEnforcement,
    profile: Option<String>,
    source: Option<String>,
    /// `(key, double_sided)` of every listed set (shared: a warm stand-in holds a copy).
    listed: Arc<BTreeSet<(u64, bool)>>,
    /// The listed sample count.
    samples: u32,
    listed_pipelines: u32,
    /// Every profile's totals on the list.
    cooked: Arc<Vec<PermutationProfileCount>>,
    admitted: u64,
    off_target: u64,
    unlisted_built: u64,
    refused: u64,
    unlisted: BTreeSet<(u64, bool)>,
}

/// Why a guard could not be built from a manifest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PermutationGuardError {
    /// The manifest has no enumeration for the profile.
    #[error("the permutation manifest has no profile `{0}`")]
    UnknownProfile(String),
    /// A key is not 16 hex digits.
    #[error("the permutation manifest lists a bad graph key `{0}`")]
    BadKey(String),
}

impl PermutationGuard {
    /// A guard that checks nothing ([`PermutationEnforcement::Off`]).
    #[must_use]
    pub fn off() -> Self {
        Self::default()
    }

    /// The guard for `profile`'s list in `manifest`.
    ///
    /// # Errors
    ///
    /// [`PermutationGuardError`] when the profile is missing or a key is malformed.
    pub fn from_manifest(
        manifest: &PermutationManifest,
        profile: &str,
        enforcement: PermutationEnforcement,
    ) -> Result<Self, PermutationGuardError> {
        let listed_profile = manifest
            .profile(profile)
            .ok_or_else(|| PermutationGuardError::UnknownProfile(profile.to_string()))?;
        let mut listed = BTreeSet::new();
        for entry in &listed_profile.entries {
            let key = u64::from_str_radix(&entry.key, 16)
                .map_err(|_| PermutationGuardError::BadKey(entry.key.clone()))?;
            listed.insert((key, entry.double_sided));
        }
        Ok(Self {
            enforcement,
            profile: Some(listed_profile.profile.clone()),
            listed: Arc::new(listed),
            samples: listed_profile.target.samples,
            listed_pipelines: listed_profile.pipelines,
            cooked: Arc::new(
                manifest
                    .profiles
                    .iter()
                    .map(PermutationProfileCount::from)
                    .collect(),
            ),
            ..Self::default()
        })
    }

    /// Names where the list came from (`render.permutations` → `guard.source`).
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// The mode.
    #[must_use]
    pub fn enforcement(&self) -> PermutationEnforcement {
        self.enforcement
    }

    /// Changes the mode, keeping the list and the counters.
    pub fn set_enforcement(&mut self, enforcement: PermutationEnforcement) {
        self.enforcement = enforcement;
    }

    /// Whether graph `key` is on the list, on any target (always true when off).
    #[must_use]
    pub fn is_listed(&self, key: u64) -> bool {
        self.enforcement == PermutationEnforcement::Off
            || self.listed.contains(&(key, false))
            || self.listed.contains(&(key, true))
    }

    /// Whether the list counted exactly this set.
    fn is_on_target(&self, key: u64, target: &crate::GraphTarget) -> bool {
        target.samples == self.samples && self.listed.contains(&(key, target.double_sided))
    }

    /// Decides one set before it is built and counts the decision. Called once per set (the
    /// cache remembers ready, pending and refused sets), never per draw.
    pub fn admit(&mut self, key: u64, target: &crate::GraphTarget) -> PermutationAdmission {
        if self.is_listed(key) {
            if self.enforcement != PermutationEnforcement::Off {
                self.admitted += 1;
                self.off_target += u64::from(!self.is_on_target(key, target));
            }
            return PermutationAdmission::Listed;
        }
        let first = self.unlisted.insert((key, target.double_sided));
        match self.enforcement {
            PermutationEnforcement::Off => PermutationAdmission::Listed,
            PermutationEnforcement::Warn => {
                self.unlisted_built += 1;
                if first {
                    tracing::warn!(
                        target: "regia_render",
                        graph = format_args!("{key:016x}"),
                        profile = self.profile.as_deref().unwrap_or("?"),
                        "material graph pipeline set is not in the cooked permutation list; built anyway \
                         (dev build). Recook so the cook report counts it (master plan § 5.5a)"
                    );
                }
                PermutationAdmission::Unlisted
            }
            PermutationEnforcement::Fallback => {
                self.refused += 1;
                if first {
                    tracing::warn!(
                        target: "regia_render",
                        graph = format_args!("{key:016x}"),
                        profile = self.profile.as_deref().unwrap_or("?"),
                        "material graph pipeline set is not in the cooked permutation list; its draws \
                         use the standard surface"
                    );
                }
                PermutationAdmission::Refused
            }
        }
    }

    /// Counters for the agent protocol.
    #[must_use]
    pub fn stats(&self) -> PermutationGuardStats {
        PermutationGuardStats {
            enforcement: self.enforcement.name(),
            profile: self.profile.clone(),
            source: self.source.clone(),
            listed_sets: self.listed.len(),
            listed_pipelines: self.listed_pipelines,
            admitted: self.admitted,
            off_target: self.off_target,
            unlisted_built: self.unlisted_built,
            refused: self.refused,
            unlisted_keys: self
                .unlisted
                .iter()
                .take(MAX_REPORTED_KEYS)
                .map(|(key, _)| format!("{key:016x}"))
                .collect(),
            cooked: self.cooked.to_vec(),
        }
    }
}

/// Environment variable naming the mode a process starts with: `off`, `warn` or `fallback`. Beats
/// the build's default ([`PermutationEnforcement::for_build`]).
pub const PERMUTATION_POLICY_ENV: &str = "REGIA_PERMUTATION_POLICY";

/// The profile of `manifest` that describes `profile`: the platform's default preset when its
/// target matches, else the first with the same target, else the first. Every profile of a
/// platform lists the same graphs, so this only picks the counts `render.permutations` reports.
fn listed_profile_name<'m>(manifest: &'m PermutationManifest, profile: &RenderProfile) -> Option<&'m str> {
    let target = ProfileTargetJson::from(ProfileTarget::of(profile));
    let default = RenderProfilePreset::platform_default().name();
    manifest
        .profiles
        .iter()
        .find(|p| p.profile == default && p.target == target)
        .or_else(|| manifest.profiles.iter().find(|p| p.target == target))
        .or_else(|| manifest.profiles.first())
        .map(|p| p.profile.as_str())
}

/// The guard for `text`, a cooked list, or `None` with a warning when it cannot be used: a damaged
/// file costs the check, never the game. A list without a single graph installs nothing: such a
/// project has no cooked graphs to protect, and any graph it draws is made in code.
fn guard_from_text(
    text: &str,
    source: &Path,
    profile: &RenderProfile,
    enforcement: PermutationEnforcement,
) -> Option<PermutationGuard> {
    let manifest = match PermutationManifest::from_json(text) {
        Ok(manifest) => manifest,
        Err(err) => {
            tracing::warn!(target: "regia_render", path = %source.display(), "{err}; no permutation check");
            return None;
        }
    };
    if manifest.graphs == 0 {
        return None;
    }
    let name = listed_profile_name(&manifest, profile)?;
    match PermutationGuard::from_manifest(&manifest, name, enforcement) {
        Ok(guard) => Some(guard.with_source(source.display().to_string())),
        Err(err) => {
            tracing::warn!(target: "regia_render", path = %source.display(), "{err}; no permutation check");
            None
        }
    }
}

/// The guard a process starts with, or `None` for no check.
///
/// A package (`content_dir` set) reads `<content_dir>/permutations.json` and uses `packaged`
/// ([`PermutationEnforcement::for_build`]); a package without the file has no check. A dev run
/// reads the cooked cache and always warns: a graph edited since the last cook is expected there.
/// `mode` is the value of [`PERMUTATION_POLICY_ENV`], which beats both.
#[must_use]
pub fn startup_guard(
    content_dir: Option<&Path>,
    cache_dir: &Path,
    profile: &RenderProfile,
    packaged: PermutationEnforcement,
    mode: Option<&str>,
) -> Option<PermutationGuard> {
    let default = if content_dir.is_some() {
        packaged
    } else {
        PermutationEnforcement::Warn
    };
    let enforcement = match mode.map(|name| (name, PermutationEnforcement::from_name(name))) {
        None => default,
        Some((_, Some(mode))) => mode,
        Some((name, None)) => {
            tracing::warn!(target: "regia_render", "{PERMUTATION_POLICY_ENV}={name} is not off, warn or fallback; ignored");
            default
        }
    };
    if enforcement == PermutationEnforcement::Off {
        return None;
    }
    let path = content_dir.unwrap_or(cache_dir).join(PERMUTATION_MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    guard_from_text(&text, &path, profile, enforcement)
}

/// Startup system: installs [`startup_guard`] on the runtime renderer. A guard a host or agent
/// installed first stays, and one installed later replaces this one.
pub(crate) fn install_startup_guard(
    assets: Option<regia_app::prelude::Res<regia_asset::AssetServer>>,
    settings: Option<regia_app::prelude::Res<crate::RenderSettings>>,
    renderer: Option<regia_app::prelude::ResMut<crate::RuntimeRenderer>>,
) {
    let (Some(assets), Some(mut renderer)) = (assets, renderer) else {
        return;
    };
    if renderer.0.permutation_guard_installed() {
        return;
    }
    // No settings: the renderer draws `RenderSettings::default()`, i.e. the runtime default.
    let preset = RenderProfile::from_preset(RenderProfilePreset::runtime_default());
    let profile = settings.as_ref().map_or(&preset, |settings| &settings.profile);
    let mode = regia_core::brand::env_var(PERMUTATION_POLICY_ENV).ok();
    if let Some(guard) = startup_guard(
        assets.settings_content_dir(),
        assets.cache_dir(),
        profile,
        PermutationEnforcement::for_build(),
        mode.as_deref(),
    ) {
        renderer.0.set_permutation_guard(guard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pass a graph set can list names the module it compiles, and the forward passes
    /// that draw colour carry the target's defines (ADR-2644).
    #[test]
    fn every_graph_pass_maps_to_its_module() {
        for (instanced, vertex) in [(false, false), (true, false), (false, true), (true, true)] {
            for pass in graph_set_passes(instanced, vertex) {
                assert!(graph_pass_module(pass).is_some(), "{pass} has no module");
            }
        }
        assert_eq!(
            graph_pass_module("forward"),
            Some(GraphPassModule::Forward {
                extra: &["ALPHA_TEST"],
                target_defs: true
            })
        );
        assert!(matches!(
            graph_pass_module("depth_instanced"),
            Some(GraphPassModule::Forward {
                target_defs: false,
                ..
            })
        ));
        assert_eq!(graph_pass_module("velocity"), Some(GraphPassModule::Velocity));
        assert_eq!(graph_pass_module("no_such_pass"), None);
        for platform in SHIPPED_PLATFORMS {
            assert!(!platform_presets(platform).is_empty(), "{platform}");
        }
    }

    /// The counting fixture: `n` surface-only graphs, `v` vertex-stage graphs, one double-sided
    /// use of the first, and a duplicate of the first (two instances of one code).
    fn fixture(n: u64, v: u64) -> Vec<GraphPermutationInput> {
        let graph = |key: u64, vertex: bool| GraphPermutationInput {
            key,
            name: format!("assets/g{key}.matgraph.ron"),
            vertex,
            uv1: false,
            code_bytes: 1024,
            double_sided: false,
        };
        let mut graphs: Vec<_> = (0..n).map(|k| graph(0x1000 + k, false)).collect();
        graphs.extend((0..v).map(|k| graph(0x2000 + k, true)));
        graphs.push(GraphPermutationInput {
            double_sided: true,
            ..graph(0x1000, false)
        });
        graphs.push(graph(0x1000, false));
        graphs
    }

    fn target(preset: RenderProfilePreset) -> crate::GraphTarget {
        let t = ProfileTarget::of(&RenderProfile::from_preset(preset));
        crate::GraphTarget {
            format: wgpu::TextureFormat::Rgba16Float,
            encode: false,
            double_sided: false,
            samples: t.samples,
            instanced: t.instanced,
            sun_rt: false,
        }
    }

    #[test]
    fn a_set_has_six_pipelines_plus_instanced_and_vertex_stage_passes() {
        assert_eq!(graph_set_passes(false, false).len(), 6);
        assert_eq!(graph_set_passes(true, false).len(), 10);
        assert_eq!(graph_set_passes(false, true).len(), 8);
        assert_eq!(graph_set_passes(true, true).len(), 13);
        assert_eq!(graph_set_modules(false, false), 1);
        assert_eq!(graph_set_modules(true, false), 5);
        assert_eq!(graph_set_modules(false, true), 3);
        assert_eq!(graph_set_modules(true, true), 8);
    }

    #[test]
    fn the_counting_fixture_counts_exactly_per_profile() {
        let graphs = fixture(3, 2);
        for preset in RenderProfilePreset::ALL {
            let t = ProfileTarget::of(&RenderProfile::from_preset(preset));
            let got = enumerate_profile(
                preset,
                &graphs,
                PermutationBudget::for_preset(preset),
                CompileCostModel::for_preset(preset),
            );
            // 3 surface sets + 1 double-sided set + 2 vertex sets; the duplicate lists once.
            assert_eq!(got.entries.len(), 6, "{preset:?}");
            let surface = graph_set_passes(t.instanced, false).len() as u32;
            let vertex = graph_set_passes(t.instanced, true).len() as u32;
            assert_eq!(got.pipelines, 4 * surface + 2 * vertex, "{preset:?}");
            assert_eq!(
                got.modules,
                4 * graph_set_modules(t.instanced, false) + 2 * graph_set_modules(t.instanced, true)
            );
            // Five distinct codes of 1 KiB.
            assert_eq!(got.code_bytes, 5 * 1024);
            assert!(!got.over_budget, "{}", got.summary_line());
            assert!(got.budget_problem().is_none());
        }
    }

    #[test]
    fn the_estimated_compile_time_follows_the_cost_model() {
        let graphs = fixture(1, 0);
        let cost = CompileCostModel {
            module_ms: 10.0,
            pipeline_ms: 1.0,
            code_kib_ms: 2.0,
        };
        // Two sets (the first graph single- and double-sided) of the profile's module count.
        let preset = RenderProfilePreset::DesktopLow;
        let t = ProfileTarget::of(&RenderProfile::from_preset(preset));
        let got = enumerate_profile(preset, &graphs, PermutationBudget::DESKTOP, cost);
        let per_set = f64::from(graph_set_modules(t.instanced, false)) * 12.0
            + graph_set_passes(t.instanced, false).len() as f64;
        assert_eq!(got.estimated_compile_ms, (2.0 * per_set).ceil() as u64);
    }

    #[test]
    fn exceeding_either_budget_flags_the_profile() {
        let graphs = fixture(40, 0);
        let preset = RenderProfilePreset::DesktopHigh;
        let tight = PermutationBudget {
            max_pipelines: 10,
            max_compile_ms: u64::MAX,
        };
        let got = enumerate_profile(preset, &graphs, tight, CompileCostModel::DESKTOP);
        assert!(got.over_budget);
        let problem = got.budget_problem().expect("over");
        assert!(problem.starts_with("DesktopHigh:"), "{problem}");
        assert!(problem.contains("budget 10"), "{problem}");
        let slow = PermutationBudget {
            max_pipelines: u32::MAX,
            max_compile_ms: 1,
        };
        let got = enumerate_profile(preset, &graphs, slow, CompileCostModel::DESKTOP);
        assert!(got.over_budget);
        assert!(got.budget_problem().expect("over").contains("cold compile"));
    }

    #[test]
    fn the_manifest_round_trips_and_lists_the_platform_profiles() {
        let graphs = fixture(2, 1);
        let mut budgets = BTreeMap::new();
        budgets.insert(
            "MobileHigh".to_string(),
            PermutationBudget {
                max_pipelines: 1,
                max_compile_ms: 1,
            },
        );
        let android = PermutationManifest::enumerate("android", &graphs, &budgets);
        assert_eq!(android.graphs, 3);
        let names: Vec<&str> = android.profiles.iter().map(|p| p.profile.as_str()).collect();
        assert_eq!(names, ["MobileLow", "MobileHigh"]);
        assert_eq!(android.budget_problems().len(), 1);
        assert!(android.budget_problems()[0].starts_with("MobileHigh:"));
        let text = android.to_json().expect("json");
        let back = PermutationManifest::from_json(&text).expect("parse");
        assert_eq!(back, android);
        assert!(back.profile("mobile_high").is_some());
        let linux = PermutationManifest::enumerate("linux", &graphs, &BTreeMap::new());
        assert!(linux.profile("Deck").is_some());
        assert!(linux.budget_problems().is_empty());
    }

    #[test]
    fn a_newer_manifest_is_refused() {
        let mut manifest = PermutationManifest::enumerate("linux", &[], &BTreeMap::new());
        manifest.format_version = PERMUTATION_MANIFEST_FORMAT_VERSION + 1;
        let text = manifest.to_json().expect("json");
        assert!(matches!(
            PermutationManifest::from_json(&text),
            Err(PermutationManifestError::Version { .. })
        ));
        assert!(matches!(
            PermutationManifest::from_json("{"),
            Err(PermutationManifestError::Parse(_))
        ));
    }

    #[test]
    fn the_guard_admits_listed_sets_and_warns_or_refuses_the_rest() {
        let graphs = fixture(2, 0);
        let manifest = PermutationManifest::enumerate("linux", &graphs, &BTreeMap::new());
        let preset = RenderProfilePreset::DesktopHigh;
        let t = target(preset);
        let mut warn =
            PermutationGuard::from_manifest(&manifest, "DesktopHigh", PermutationEnforcement::Warn)
                .expect("guard");
        assert_eq!(warn.admit(0x1000, &t), PermutationAdmission::Listed);
        // The fixture uses 0x1000 double-sided too.
        let ds = crate::GraphTarget {
            double_sided: true,
            ..t
        };
        assert_eq!(warn.admit(0x1000, &ds), PermutationAdmission::Listed);
        assert_eq!(warn.admit(0xdead, &t), PermutationAdmission::Unlisted);
        let stats = warn.stats();
        assert_eq!((stats.admitted, stats.off_target), (2, 0));
        assert_eq!((stats.unlisted_built, stats.refused), (1, 0));
        assert_eq!(stats.listed_sets, 3);
        assert_eq!(stats.profile.as_deref(), Some("DesktopHigh"));
        assert!(stats.unlisted_keys.contains(&"000000000000dead".to_string()));

        let mut ship =
            PermutationGuard::from_manifest(&manifest, "DesktopHigh", PermutationEnforcement::Fallback)
                .expect("guard");
        assert_eq!(ship.admit(0x1001, &t), PermutationAdmission::Listed);
        assert_eq!(ship.admit(0xdead, &t), PermutationAdmission::Refused);
        assert_eq!(ship.stats().refused, 1);
        assert_eq!(ship.stats().enforcement, "fallback");
        assert_eq!(ship.stats().cooked.len(), manifest.profiles.len());
        assert_eq!(ship.stats().cooked[0].sets, 3);

        let mut off = PermutationGuard::off();
        assert_eq!(off.admit(0xdead, &t), PermutationAdmission::Listed);
        assert_eq!(
            off.stats(),
            PermutationGuardStats {
                enforcement: "off",
                ..PermutationGuardStats::default()
            }
        );
    }

    #[test]
    fn a_listed_graph_on_another_target_is_admitted_and_counted() {
        let graphs = fixture(2, 0);
        let manifest = PermutationManifest::enumerate("linux", &graphs, &BTreeMap::new());
        let t = target(RenderProfilePreset::DesktopHigh);
        let mut ship =
            PermutationGuard::from_manifest(&manifest, "DesktopHigh", PermutationEnforcement::Fallback)
                .expect("guard");
        // A scene that turns culling off needs the double-sided set of a single-sided listing, and
        // a frame at another sample count needs other pipelines of the same code: refusing either
        // would draw a listed graph with the wrong surface.
        let culled_off = crate::GraphTarget {
            double_sided: true,
            ..t
        };
        let msaa = crate::GraphTarget { samples: 4, ..t };
        assert_eq!(ship.admit(0x1001, &culled_off), PermutationAdmission::Listed);
        assert_eq!(ship.admit(0x1001, &msaa), PermutationAdmission::Listed);
        assert_eq!(ship.admit(0x1001, &t), PermutationAdmission::Listed);
        let stats = ship.stats();
        assert_eq!((stats.admitted, stats.off_target, stats.refused), (3, 2, 0));
        assert_eq!(ship.admit(0xdead, &msaa), PermutationAdmission::Refused);
    }

    fn scratch_list(label: &str, graphs: &[GraphPermutationInput]) -> std::path::PathBuf {
        let dir = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(&dir).expect("dir");
        let manifest = PermutationManifest::enumerate("linux", graphs, &BTreeMap::new());
        let text = manifest.to_json().expect("json");
        std::fs::write(dir.join(PERMUTATION_MANIFEST_FILE), text).expect("write");
        dir
    }

    fn desktop() -> RenderProfile {
        RenderProfile::from_preset(RenderProfilePreset::DesktopHigh)
    }

    #[test]
    fn a_package_uses_the_build_mode_and_a_dev_run_always_warns() {
        let dir = scratch_list("modes", &fixture(1, 0));
        let empty = dir.join("empty");
        let ship = PermutationEnforcement::Fallback;
        let packaged = startup_guard(Some(&dir), &empty, &desktop(), ship, None).expect("package");
        assert_eq!(packaged.enforcement(), ship);
        assert_eq!(packaged.stats().profile.as_deref(), Some("DesktopHigh"));
        let source = dir.join(PERMUTATION_MANIFEST_FILE).display().to_string();
        assert_eq!(packaged.stats().source, Some(source));
        let dev = startup_guard(None, &dir, &desktop(), ship, None).expect("dev");
        assert_eq!(dev.enforcement(), PermutationEnforcement::Warn);
        // The environment beats both.
        let off = startup_guard(Some(&dir), &empty, &desktop(), ship, Some("off"));
        assert!(off.is_none());
        let warn = startup_guard(Some(&dir), &empty, &desktop(), ship, Some("warn")).expect("warn");
        assert_eq!(warn.enforcement(), PermutationEnforcement::Warn);
        let typo = startup_guard(Some(&dir), &empty, &desktop(), ship, Some("strict")).expect("typo");
        assert_eq!(typo.enforcement(), ship);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_usable_list_means_no_check_and_never_a_failure() {
        let ship = PermutationEnforcement::Fallback;
        // A project with no cooked graph: whatever it draws is made in code.
        let dir = scratch_list("none", &[]);
        assert!(startup_guard(Some(&dir), &dir, &desktop(), ship, None).is_none());
        // A package from before the list, and a damaged list.
        let missing = dir.join("missing");
        assert!(startup_guard(Some(&missing), &dir, &desktop(), ship, None).is_none());
        std::fs::write(dir.join(PERMUTATION_MANIFEST_FILE), "{").expect("write");
        assert!(startup_guard(Some(&dir), &dir, &desktop(), ship, None).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn asset_world(dir: &Path) -> regia_app::prelude::World {
        let mut world = regia_app::prelude::World::new();
        let settings = regia_asset::AssetServerSettings {
            project_root: dir.to_path_buf(),
            platform: "linux".into(),
            content_dir: Some(dir.to_path_buf()),
        };
        world.insert_resource(regia_asset::AssetServer::open(settings).expect("server"));
        world
    }

    #[test]
    fn the_startup_install_leaves_a_guard_the_host_installed() {
        use regia_ecs::ecs::system::RunSystemOnce;

        let dir = scratch_list("install", &fixture(1, 0));
        let mut world = asset_world(&dir);
        world.insert_resource(crate::RuntimeRenderer::default());
        world.run_system_once(install_startup_guard).expect("system");
        let stats = world.resource::<crate::RuntimeRenderer>().0.permutation_stats();
        assert_ne!(stats.enforcement, "off");
        assert_eq!(stats.listed_sets, 2);

        let mut world = asset_world(&dir);
        let mut renderer = crate::RuntimeRenderer::default();
        renderer.0.set_permutation_guard(PermutationGuard::off());
        world.insert_resource(renderer);
        world.run_system_once(install_startup_guard).expect("system");
        let stats = world.resource::<crate::RuntimeRenderer>().0.permutation_stats();
        assert_eq!(stats.enforcement, "off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_warm_stand_in_carries_the_guard() {
        // An abandoned warm leaves the stand-in as the runtime renderer: it must not run unchecked.
        let manifest = PermutationManifest::enumerate("linux", &fixture(1, 0), &BTreeMap::new());
        let guard =
            PermutationGuard::from_manifest(&manifest, "DesktopHigh", PermutationEnforcement::Fallback)
                .expect("guard");
        let mut runtime = crate::FrameRenderer::new();
        runtime.set_permutation_guard(guard);
        let mut stand_in = crate::FrameRenderer::new();
        runtime.share_installs_with(&mut stand_in);
        assert_eq!(stand_in.permutation_stats().enforcement, "fallback");
        assert_eq!(stand_in.permutation_stats().listed_sets, 2);
        // The copy is not an install: the warmed renderer keeps its own guard and counters.
        assert!(!stand_in.permutation_guard_installed());
    }

    #[test]
    fn a_guard_needs_a_listed_profile_and_good_keys() {
        let mut manifest = PermutationManifest::enumerate("android", &fixture(1, 0), &BTreeMap::new());
        assert_eq!(
            PermutationGuard::from_manifest(&manifest, "DesktopHigh", PermutationEnforcement::Warn).err(),
            Some(PermutationGuardError::UnknownProfile("DesktopHigh".into()))
        );
        manifest.profiles[0].entries[0].key = "xyz".into();
        assert_eq!(
            PermutationGuard::from_manifest(&manifest, "MobileLow", PermutationEnforcement::Warn).err(),
            Some(PermutationGuardError::BadKey("xyz".into()))
        );
    }

    #[test]
    fn enforcement_names_round_trip() {
        for mode in [
            PermutationEnforcement::Off,
            PermutationEnforcement::Warn,
            PermutationEnforcement::Fallback,
        ] {
            assert_eq!(PermutationEnforcement::from_name(mode.name()), Some(mode));
        }
        assert_eq!(PermutationEnforcement::from_name("strict"), None);
    }
}
