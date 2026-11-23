// SPDX-License-Identifier: MPL-2.0
//! Shipped-shader cost per render profile (`docs/master-plan.md` § 5.5a remainder, ADR-2664).
//!
//! The permutation manifest counts material-graph pipelines. This module counts the other half
//! of what a profile ships: the engine's own forward shader permutations
//! ([`forward_permutations_for`]), each composed and validated by naga:
//!
//! - [`ShaderCostRow::permutations`]: define sets that compose,
//! - [`ShaderCostRow::wgsl_bytes`]: naga's WGSL re-emit of each composed module, summed,
//! - [`ShaderCostRow::spirv_bytes`]: the SPIR-V naga writes for all entry points, summed,
//! - [`ShaderCostRow::compile_cost_ms`]: an ESTIMATE from [`CompileCostModel`], and
//! - [`ShaderCostRow::measured_naga_ms`]: the wall time of the composing and writing in this
//!   run. It is reported and never asserted on, compared or budgeted.
//!
//! [`ShaderBytesBudget`] is the per-profile ceiling of the WGSL bytes: a new permutation axis
//! multiplies the define sets and trips it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::matrix::{forward_permutations_for, forward_source};
use crate::RenderProfilePreset;
use crate::permutation_policy::{CompileCostModel, platform_presets};

/// Version of [`ShaderCostReport`]'s JSON shape (`regia cook --size-report --json`, `shaders`).
pub const SHADER_COST_REPORT_VERSION: u32 = 1;

/// The ceiling of shipped forward-shader WGSL bytes for one profile. Provisional numbers
/// (ADR-2664); `Regia.toml` `[cook.permutation_budget.<Profile>] max_shader_bytes` overrides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShaderBytesBudget {
    /// Most generated WGSL bytes over the profile's forward define sets.
    pub max_wgsl_bytes: u64,
}

impl ShaderBytesBudget {
    /// The default budget of `preset`: Mobile ships the lite tier and gets the smaller ceiling.
    #[must_use]
    pub const fn for_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow | RenderProfilePreset::MobileHigh => Self {
                max_wgsl_bytes: 48 * 1024 * 1024,
            },
            _ => Self {
                max_wgsl_bytes: 256 * 1024 * 1024,
            },
        }
    }
}

/// One profile's shipped forward-shader cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShaderCostRow {
    /// Preset name ([`RenderProfilePreset::name`]).
    pub profile: String,
    /// Forward define sets that compose and validate (the permutation count).
    pub permutations: u32,
    /// Define sets that failed to compose or validate (their bytes are not counted).
    pub failed: u32,
    /// Generated WGSL bytes over the composed define sets.
    pub wgsl_bytes: u64,
    /// SPIR-V bytes after naga over the composed define sets.
    pub spirv_bytes: u64,
    /// ESTIMATED cold compile time, milliseconds ([`CompileCostModel`]): not a measurement.
    pub compile_cost_ms: u64,
    /// MEASURED naga front-end plus back-end time of this run, milliseconds (the first composition
    /// of each define set in this process; a repeat reuses its time). Varies with the host; never asserted
    /// on and not part of any budget.
    pub measured_naga_ms: f64,
    /// The WGSL-bytes ceiling checked.
    pub budget: ShaderBytesBudget,
    /// [`Self::wgsl_bytes`] is above [`Self::budget`].
    pub over_budget: bool,
}

impl ShaderCostRow {
    /// Why the row is over budget, or `None`. A define set that failed to compose counts 0
    /// bytes, so a row with failures is a problem too: its byte total is not a measurement.
    #[must_use]
    pub fn budget_problem(&self) -> Option<String> {
        if self.failed > 0 {
            return Some(format!(
                "{}: {} define set(s) FAILED to compose, so the {} B shipped forward WGSL is not measured",
                self.profile, self.failed, self.wgsl_bytes
            ));
        }
        self.over_budget.then(|| {
            format!(
                "{}: {} B shipped forward WGSL, budget {} B",
                self.profile, self.wgsl_bytes, self.budget.max_wgsl_bytes
            )
        })
    }

    /// One line for the size report; the estimate and the measurement are labelled.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let failed = if self.failed > 0 {
            format!(", {} FAILED to compose", self.failed)
        } else {
            String::new()
        };
        format!(
            "{}: {} permutations, {} B WGSL, {} B SPIR-V, compile cost ~{} ms (estimate), naga {:.0} ms (measured){failed}{}",
            self.profile,
            self.permutations,
            self.wgsl_bytes,
            self.spirv_bytes,
            self.compile_cost_ms,
            self.measured_naga_ms,
            if self.over_budget { " OVER BUDGET" } else { "" }
        )
    }
}

/// The shipped-shader cost of every profile of one platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShaderCostReport {
    /// [`SHADER_COST_REPORT_VERSION`].
    pub format_version: u32,
    /// Platform tag.
    pub platform: String,
    /// One row per real-time profile of the platform.
    pub profiles: Vec<ShaderCostRow>,
}

impl ShaderCostReport {
    /// Every profile over its WGSL-bytes budget or with define sets that failed to compose, as
    /// messages.
    #[must_use]
    pub fn budget_problems(&self) -> Vec<String> {
        self.profiles
            .iter()
            .filter_map(ShaderCostRow::budget_problem)
            .collect()
    }
}

/// Bytes of one composed define set, by `(file, sorted defines)`: Windows, macOS and Linux share
/// most of their sets, so a report over several platforms composes each once per process.
static MEASURED: OnceLock<Mutex<HashMap<String, Measured>>> = OnceLock::new();

/// `(WGSL bytes, SPIR-V bytes, naga milliseconds)` of one define set, `None` when it fails.
type Measured = Option<(u64, u64, f64)>;

/// [`measure`] through the process cache.
fn measure_cached(
    source: &str,
    file: &str,
    defs: &[(String, naga_oil::compose::ShaderDefValue)],
) -> Measured {
    let mut names: Vec<String> = defs.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
    names.sort();
    let key = format!("{file}\0{}", names.join(","));
    let cache = MEASURED.get_or_init(Default::default);
    if let Some(hit) = cache.lock().ok().and_then(|map| map.get(&key).copied()) {
        return hit;
    }
    let started = Instant::now();
    let value = measure(source, file, defs.iter().cloned().collect())
        .map(|(wgsl, spirv)| (wgsl, spirv, started.elapsed().as_secs_f64() * 1000.0));
    if let Ok(mut map) = cache.lock() {
        map.insert(key, value);
    }
    value
}

/// Composes `source` with `defs`; returns the WGSL re-emit bytes and the SPIR-V bytes, or `None`
/// when composition, validation or a back end fails.
fn measure(
    source: &str,
    file: &str,
    defs: HashMap<String, naga_oil::compose::ShaderDefValue>,
) -> Option<(u64, u64)> {
    let module = crate::shader_lib::compose_source(source, file, defs).ok()?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .ok()?;
    let text = naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty()).ok()?;
    let words = super::write_spirv(file, &module, &info).ok()?;
    Some((text.len() as u64, words.len() as u64 * 4))
}

/// The cost row of `preset` on `platform` against `budget`.
///
/// `extra_axes` multiplies every define set by `2^extra_axes`, the way that many new boolean
/// `#ifdef` axes of the forward shader would: each copy ships its own module of the same size.
/// Production callers pass 0; the budget test passes 1 to prove a new axis trips the row.
#[must_use]
pub fn shader_cost_row(
    platform: &str,
    preset: RenderProfilePreset,
    budget: ShaderBytesBudget,
    extra_axes: u32,
) -> ShaderCostRow {
    let _span = tracing::info_span!("shader_cost_row", profile = preset.name()).entered();
    let copies = 1u64 << extra_axes.min(16);
    let (mut permutations, mut failed, mut wgsl, mut spirv, mut measured_naga_ms) =
        (0u32, 0u32, 0u64, 0u64, 0.0f64);
    for set in forward_permutations_for(platform, preset) {
        match measure_cached(forward_source(set.file), set.file, &set.defs) {
            Some((w, s, ms)) => {
                measured_naga_ms += ms;
                permutations = permutations.saturating_add(u32::try_from(copies).unwrap_or(u32::MAX));
                wgsl += w * copies;
                spirv += s * copies;
            }
            None => failed += 1,
        }
    }
    let mean_kib_bytes = usize::try_from(wgsl / u64::from(permutations.max(1))).unwrap_or(usize::MAX);
    // One module and one pipeline per define set.
    let estimate =
        CompileCostModel::for_preset(preset).estimate_ms(permutations, permutations, mean_kib_bytes);
    ShaderCostRow {
        profile: preset.name().to_string(),
        permutations,
        failed,
        wgsl_bytes: wgsl,
        spirv_bytes: spirv,
        compile_cost_ms: estimate.ceil() as u64,
        measured_naga_ms,
        budget,
        over_budget: wgsl > budget.max_wgsl_bytes,
    }
}

/// The cost of every real-time profile of `platform`; `budgets` overrides by preset name.
#[must_use]
pub fn shader_cost_report(platform: &str, budgets: &BTreeMap<String, ShaderBytesBudget>) -> ShaderCostReport {
    let profiles = platform_presets(platform)
        .into_iter()
        .map(|preset| {
            let budget = budgets
                .get(preset.name())
                .copied()
                .unwrap_or_else(|| ShaderBytesBudget::for_preset(preset));
            shader_cost_row(platform, preset, budget, 0)
        })
        .collect();
    ShaderCostReport {
        format_version: SHADER_COST_REPORT_VERSION,
        platform: platform.to_string(),
        profiles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A define set that fails to compose counts 0 bytes, so a row with failures is a budget
    /// problem whatever its byte total (corr-low-render, corr-render-frame-upscaler finding 7).
    #[test]
    fn a_row_with_failed_define_sets_is_a_budget_problem() {
        let row = ShaderCostRow {
            profile: "X".into(),
            permutations: 1,
            failed: 1,
            wgsl_bytes: 0,
            spirv_bytes: 0,
            compile_cost_ms: 0,
            measured_naga_ms: 0.0,
            budget: ShaderBytesBudget {
                max_wgsl_bytes: u64::MAX,
            },
            over_budget: false,
        };
        let problem = row.budget_problem();
        assert!(
            problem.as_deref().is_some_and(|p| p.contains("FAILED")),
            "{problem:?}"
        );
        let report = ShaderCostReport {
            format_version: SHADER_COST_REPORT_VERSION,
            platform: "linux".into(),
            profiles: vec![row.clone(), ShaderCostRow { failed: 0, ..row }],
        };
        assert_eq!(report.budget_problems().len(), 1);
    }

    #[test]
    fn a_profile_row_counts_the_composed_sets_and_a_synthetic_axis_trips_the_budget() {
        let preset = RenderProfilePreset::MobileLow;
        let open = ShaderBytesBudget {
            max_wgsl_bytes: u64::MAX,
        };
        let base = shader_cost_row("android", preset, open, 0);
        assert_eq!(base.failed, 0, "{}", base.summary_line());
        assert!(base.permutations > 0);
        assert!(base.wgsl_bytes > 0 && base.spirv_bytes > 0 && base.compile_cost_ms > 0);
        assert!(!base.over_budget);
        assert_eq!(
            base.permutations as usize,
            forward_permutations_for("android", preset).len()
        );
        // A budget at exactly the shipped bytes holds; one more axis doubles the sets and fails.
        let exact = ShaderBytesBudget {
            max_wgsl_bytes: base.wgsl_bytes,
        };
        assert!(!shader_cost_row("android", preset, exact, 0).over_budget);
        let grown = shader_cost_row("android", preset, exact, 1);
        assert_eq!(grown.permutations, base.permutations * 2);
        assert_eq!(grown.wgsl_bytes, base.wgsl_bytes * 2);
        assert!(grown.over_budget);
        assert!(
            grown
                .budget_problem()
                .is_some_and(|p| p.starts_with("MobileLow:"))
        );
    }
}
