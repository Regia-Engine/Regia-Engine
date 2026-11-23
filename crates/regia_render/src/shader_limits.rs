// SPDX-License-Identifier: MPL-2.0
//! Binding counts per shader stage against the limits of each device class (ADR-3002).
//!
//! Every compiled permutation of the shader matrix ([`crate::shader_matrix_inputs`]) is composed
//! and validated, and for each entry point this module counts the resources the stage reads:
//! uniform buffers, storage buffers, storage textures, sampled textures, samplers, bind groups and
//! the largest uniform and storage binding. The counts are held to a table of
//! [`LimitClass`]es. A Mobile preset over the `mobile` class fails; the `downlevel` class (wgpu's
//! portable floor) and the `desktop` class only report headroom. On Android a pipeline over a
//! device limit fails when the pipeline is created, and a CI host never sees it.
//!
//! The `mobile` gate is the smallest number of three rows per metric: the engine's own Mobile tier
//! floor (`MinspecProfile::Mobile`), a Pixel 6 (Mali-G78) and a Fairphone 5 (Adreno 643). The two
//! device rows are Vulkan limits read from the phones with `adb shell cmd gpu vkjson` on 2026-11-05,
//! so a number is what the driver reports, not a vendor document. Raw Vulkan per-stage descriptor
//! counts on both phones are so high (524288 and 8388608) that wgpu's own caps decide; the floor
//! row carries the number the engine can rely on.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::shader_check::{MatrixInput, define_key};

/// Version of [`LimitsReport`].
pub const SHADER_LIMITS_REPORT_VERSION: u32 = 1;

/// What one entry point reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingCounts {
    /// Uniform buffer bindings.
    pub uniform_buffers: u32,
    /// Storage buffer bindings.
    pub storage_buffers: u32,
    /// Storage texture bindings.
    pub storage_textures: u32,
    /// Sampled (and depth) texture bindings outside binding arrays. A binding array of textures
    /// adds nothing here: its elements count in `binding_array_elements` instead.
    pub sampled_textures: u32,
    /// Sampler bindings.
    pub samplers: u32,
    /// Elements of binding arrays of textures (the texture table), counted apart from the
    /// others. Not one of [`LIMIT_METRICS`]: no class limits it.
    pub binding_array_elements: u32,
    /// Highest bind group index read, plus one.
    pub bind_groups: u32,
    /// Size of the largest uniform binding read, in bytes.
    pub max_uniform_bytes: u64,
    /// Size of the largest storage binding read (a runtime-sized array counts one element), in bytes.
    pub max_storage_bytes: u64,
}

/// One entry point's usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageUsage {
    /// The entry point name.
    pub entry: String,
    /// `vertex`, `fragment` or `compute`.
    pub stage: String,
    /// What it reads.
    pub counts: BindingCounts,
}

/// The metrics a class limits, in report order.
pub const LIMIT_METRICS: [&str; 8] = [
    "uniform_buffers",
    "storage_buffers",
    "storage_textures",
    "sampled_textures",
    "samplers",
    "bind_groups",
    "uniform_binding_bytes",
    "storage_binding_bytes",
];

impl BindingCounts {
    /// The value of a metric of [`LIMIT_METRICS`].
    #[must_use]
    pub fn metric(&self, name: &str) -> u64 {
        match name {
            "uniform_buffers" => u64::from(self.uniform_buffers),
            "storage_buffers" => u64::from(self.storage_buffers),
            "storage_textures" => u64::from(self.storage_textures),
            "sampled_textures" => u64::from(self.sampled_textures),
            "samplers" => u64::from(self.samplers),
            "bind_groups" => u64::from(self.bind_groups),
            "uniform_binding_bytes" => self.max_uniform_bytes,
            "storage_binding_bytes" => self.max_storage_bytes,
            _ => 0,
        }
    }
}

fn stage_name(stage: naga::ShaderStage) -> &'static str {
    match stage {
        naga::ShaderStage::Vertex => "vertex",
        naga::ShaderStage::Fragment => "fragment",
        naga::ShaderStage::Compute => "compute",
        _ => "other",
    }
}

/// Counts per entry point of a module.
///
/// # Errors
///
/// naga rejects the module or its layout.
pub fn module_usage(module: &naga::Module) -> Result<Vec<StageUsage>, String> {
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    .map_err(|err| err.to_string())?;
    let mut layouter = naga::proc::Layouter::default();
    layouter.update(module.to_ctx()).map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for (index, entry) in module.entry_points.iter().enumerate() {
        let fun = info.get_entry_point(index);
        let mut c = BindingCounts::default();
        for (handle, global) in module.global_variables.iter() {
            let Some(binding) = &global.binding else { continue };
            if fun[handle].is_empty() {
                continue;
            }
            c.bind_groups = c.bind_groups.max(binding.group + 1);
            let mut ty = global.ty;
            let mut array_elements = None;
            if let naga::TypeInner::BindingArray { base, size } = &module.types[ty].inner {
                array_elements = Some(match size {
                    naga::ArraySize::Constant(n) => n.get(),
                    _ => 0,
                });
                ty = *base;
            }
            let size = u64::from(layouter[ty].size);
            match global.space {
                naga::AddressSpace::Uniform => {
                    c.uniform_buffers += 1;
                    c.max_uniform_bytes = c.max_uniform_bytes.max(size);
                }
                naga::AddressSpace::Storage { .. } => {
                    c.storage_buffers += 1;
                    c.max_storage_bytes = c.max_storage_bytes.max(size);
                }
                naga::AddressSpace::Handle => match &module.types[ty].inner {
                    naga::TypeInner::Sampler { .. } => c.samplers += 1,
                    naga::TypeInner::Image {
                        class: naga::ImageClass::Storage { .. },
                        ..
                    } => c.storage_textures += 1,
                    naga::TypeInner::Image { .. } => match array_elements {
                        Some(n) => c.binding_array_elements += n,
                        None => c.sampled_textures += 1,
                    },
                    _ => {}
                },
                _ => {}
            }
        }
        out.push(StageUsage {
            entry: entry.name.clone(),
            stage: stage_name(entry.stage).into(),
            counts: c,
        });
    }
    Ok(out)
}

/// One number of a limit table with where it comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitNumber {
    /// The limit.
    pub value: u64,
    /// Where the number comes from.
    pub source: String,
    /// `false`: the lead must confirm it (`adb shell cmd gpu vkjson` or `vulkaninfo`).
    pub verified: bool,
}

/// A device class and its limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitClass {
    /// `downlevel`, `mobile` or `desktop`.
    pub name: String,
    /// `true`: a preset of this class over a number fails the check.
    pub gate: bool,
    /// Metric name to number.
    pub limits: BTreeMap<String, LimitNumber>,
    /// The rows the numbers were minimized over, for the report.
    pub rows: Vec<String>,
}

const WGPU_DEFAULTS: &str = "wgpu-types 30.0.1 Limits::default()";
const WGPU_DOWNLEVEL: &str = "wgpu-types 30.0.1 Limits::downlevel_defaults()";
const FLOOR: &str =
    "regia_render::minspec MinspecProfile::Mobile (engine tier floor; Mali-G710 gpuinfo report 51090)";
const PIXEL6: &str = "Pixel 6, Mali-G78, driver 226500608: adb shell cmd gpu vkjson, 2026-11-05";
const FP5: &str = "Fairphone 5, Adreno (TM) 643, driver 2149654528: adb shell cmd gpu vkjson, 2026-11-05";
const PASCAL: &str = "regia_render::minspec MinspecProfile::Pascal (GTX 1080, gpuinfo report 52316)";

/// The raw Vulkan limits read from the phones, by metric. Per-stage descriptor counts are the
/// `maxPerStageDescriptor*` limits, binding sizes are `maxUniformBufferRange` and
/// `maxStorageBufferRange`, groups are `maxBoundDescriptorSets`.
const PIXEL6_ROW: [(&str, u64); 8] = [
    ("uniform_buffers", 8_388_608),
    ("storage_buffers", 8_388_608),
    ("storage_textures", 8_388_608),
    ("sampled_textures", 8_388_608),
    ("samplers", 8_388_608),
    ("bind_groups", 7),
    ("uniform_binding_bytes", 2_147_483_648),
    ("storage_binding_bytes", 2_147_483_648),
];
const FP5_ROW: [(&str, u64); 8] = [
    ("uniform_buffers", 524_288),
    ("storage_buffers", 524_288),
    ("storage_textures", 524_288),
    ("sampled_textures", 524_288),
    ("samplers", 524_288),
    ("bind_groups", 4),
    ("uniform_binding_bytes", 65_536),
    ("storage_binding_bytes", 536_870_912),
];

fn from_wgpu(limits: &wgpu::Limits) -> [(&'static str, u64); 8] {
    [
        (
            "uniform_buffers",
            u64::from(limits.max_uniform_buffers_per_shader_stage),
        ),
        (
            "storage_buffers",
            u64::from(limits.max_storage_buffers_per_shader_stage),
        ),
        (
            "storage_textures",
            u64::from(limits.max_storage_textures_per_shader_stage),
        ),
        (
            "sampled_textures",
            u64::from(limits.max_sampled_textures_per_shader_stage),
        ),
        ("samplers", u64::from(limits.max_samplers_per_shader_stage)),
        ("bind_groups", u64::from(limits.max_bind_groups)),
        ("uniform_binding_bytes", limits.max_uniform_buffer_binding_size),
        ("storage_binding_bytes", limits.max_storage_buffer_binding_size),
    ]
}

/// An adapter with no limit, so a tier clamp returns its own numbers.
fn unbounded() -> wgpu::Limits {
    wgpu::Limits {
        max_bind_groups: u32::MAX,
        max_uniform_buffers_per_shader_stage: u32::MAX,
        max_storage_buffers_per_shader_stage: u32::MAX,
        max_storage_textures_per_shader_stage: u32::MAX,
        max_sampled_textures_per_shader_stage: u32::MAX,
        max_samplers_per_shader_stage: u32::MAX,
        max_uniform_buffer_binding_size: u64::MAX,
        max_storage_buffer_binding_size: u64::MAX,
        ..wgpu::Limits::default()
    }
}

/// Cache key of one composed module: file, source hash, defines, plain-WGSL flag.
type UsageKey = (String, u64, Vec<String>, bool);

/// One source row of a limit class: its source text, the eight metric values, and whether it is verified.
type ClassRow<'a> = (&'a str, [(&'static str, u64); 8], bool);

fn class_from_rows(name: &str, gate: bool, rows: &[ClassRow<'_>]) -> LimitClass {
    let mut limits: BTreeMap<String, LimitNumber> = BTreeMap::new();
    for (source, row, verified) in rows {
        for (metric, value) in row {
            let better = limits.get(*metric).is_none_or(|n| *value < n.value);
            if better {
                limits.insert(
                    (*metric).to_string(),
                    LimitNumber {
                        value: *value,
                        source: (*source).to_string(),
                        verified: *verified,
                    },
                );
            }
        }
    }
    LimitClass {
        name: name.into(),
        gate,
        limits,
        rows: rows.iter().map(|(s, _, _)| (*s).to_string()).collect(),
    }
}

/// The limit classes: `downlevel` (wgpu's portable floor, report only), `mobile` (the gate for
/// Mobile presets) and `desktop` (report only).
#[must_use]
pub fn limit_classes() -> Vec<LimitClass> {
    use crate::minspec::MinspecProfile;
    let floor = from_wgpu(&MinspecProfile::Mobile.clamp_limits(&unbounded()));
    let pascal = from_wgpu(&MinspecProfile::Pascal.clamp_limits(&unbounded()));
    // The Pascal clamp leaves the per-stage descriptor counts it does not constrain unbounded;
    // those take wgpu's own defaults, a floor every desktop adapter exceeds.
    let defaults = from_wgpu(&wgpu::Limits::default());
    let mut desktop = pascal;
    for (i, (_, value)) in desktop.iter_mut().enumerate() {
        if *value >= u64::from(u32::MAX) {
            *value = defaults[i].1;
        }
    }
    vec![
        class_from_rows(
            "downlevel",
            false,
            &[(
                WGPU_DOWNLEVEL,
                from_wgpu(&wgpu::Limits::downlevel_defaults()),
                true,
            )],
        ),
        class_from_rows(
            "mobile",
            true,
            &[
                (FLOOR, floor, true),
                (PIXEL6, PIXEL6_ROW, true),
                (FP5, FP5_ROW, true),
            ],
        ),
        class_from_rows(
            "desktop",
            false,
            &[(PASCAL, desktop, true), (WGPU_DEFAULTS, defaults, true)],
        ),
    ]
}

/// The worst use of one metric by one preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricUse {
    /// A metric of [`LIMIT_METRICS`].
    pub metric: String,
    /// The largest value any stage of any module of the preset reads.
    pub used: u64,
    /// The class limit.
    pub limit: u64,
    /// `limit - used`, negative when over.
    pub headroom: i64,
    /// The file of the module that reaches `used`.
    pub module: String,
    /// The entry point and stage.
    pub entry: String,
}

/// One preset against its class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetLimits {
    /// `android/MobileHigh`.
    pub preset: String,
    /// The class it is held to.
    pub class: String,
    /// Composed modules (file and define set) counted.
    pub modules: usize,
    /// The worst use of each metric.
    pub worst: Vec<MetricUse>,
    /// Metrics over the class limit (a failure on a gate class).
    pub over: Vec<String>,
    /// Metrics over the `downlevel` class (informational).
    pub over_downlevel: Vec<String>,
}

/// The result of [`run_shader_limits`]; serialized by `check-shaders --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsReport {
    /// [`SHADER_LIMITS_REPORT_VERSION`].
    pub format_version: u32,
    /// The class tables with sources.
    pub classes: Vec<LimitClass>,
    /// Every shipped preset.
    pub presets: Vec<PresetLimits>,
    /// The engine-wide modules (outside any preset's permutation list) against the `mobile` class.
    pub engine: PresetLimits,
    /// Modules that failed to compose (the matrix reports them too).
    pub skipped: Vec<String>,
    /// Failures: a Mobile preset over a limit.
    pub failures: Vec<String>,
    /// Numbers no source backs, for the lead to confirm. Empty when every row is measured.
    pub unverified: Vec<String>,
}

impl LimitsReport {
    /// `true` when no gate class is over.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

struct Tally {
    modules: usize,
    best: BTreeMap<&'static str, (u64, String, String)>,
}

impl Tally {
    fn new() -> Self {
        Self {
            modules: 0,
            best: BTreeMap::new(),
        }
    }

    fn add(&mut self, file: &str, usage: &[StageUsage]) {
        self.modules += 1;
        for u in usage {
            for metric in LIMIT_METRICS {
                let value = u.counts.metric(metric);
                let entry = self
                    .best
                    .entry(metric)
                    .or_insert((0, String::new(), String::new()));
                if value > entry.0 {
                    *entry = (value, file.to_string(), format!("{} ({})", u.entry, u.stage));
                }
            }
        }
    }

    fn finish(&self, name: &str, class: &LimitClass, downlevel: &LimitClass) -> PresetLimits {
        let mut worst = Vec::new();
        let mut over = Vec::new();
        let mut over_downlevel = Vec::new();
        for metric in LIMIT_METRICS {
            let (used, module, entry) = self.best.get(metric).cloned().unwrap_or_default();
            let limit = class.limits.get(metric).map_or(u64::MAX, |n| n.value);
            let headroom = i64::try_from(limit).unwrap_or(i64::MAX) - i64::try_from(used).unwrap_or(i64::MAX);
            if used > limit {
                over.push(format!("{metric} {used} > {limit} ({module} {entry})"));
            }
            if downlevel.limits.get(metric).is_some_and(|n| used > n.value) {
                over_downlevel.push(format!(
                    "{metric} {used} > {} ({module} {entry})",
                    downlevel.limits[metric].value
                ));
            }
            worst.push(MetricUse {
                metric: metric.into(),
                used,
                limit,
                headroom,
                module,
                entry,
            });
        }
        PresetLimits {
            preset: name.into(),
            class: class.name.clone(),
            modules: self.modules,
            worst,
            over,
            over_downlevel,
        }
    }
}

/// The preset an input's origin names (`linux/DesktopHigh forward` -> `linux/DesktopHigh`), or
/// `None` for an engine-wide module.
fn origin_preset(origin: &str) -> Option<&str> {
    if origin == "engine" {
        return None;
    }
    origin.split(' ').next().filter(|p| p.contains('/'))
}

/// Counts the binding use of every input and holds each preset to its class.
///
/// Inputs come from [`crate::shader_matrix_inputs`]; a module that does not compose is listed in
/// `skipped` (the matrix reports why).
#[must_use]
pub fn run_shader_limits(inputs: &[MatrixInput]) -> LimitsReport {
    let classes = limit_classes();
    let by_name = |n: &str| {
        classes
            .iter()
            .find(|c| c.name == n)
            .cloned()
            .unwrap_or_else(|| classes[0].clone())
    };
    let (downlevel, mobile, desktop) = (by_name("downlevel"), by_name("mobile"), by_name("desktop"));
    let mut cache: HashMap<UsageKey, Option<Vec<StageUsage>>> = HashMap::new();
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    let mut engine = Tally::new();
    let mut skipped = Vec::new();
    for input in inputs {
        if input.source.is_empty() {
            continue;
        }
        let key = (
            input.file.clone(),
            {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                input.source.hash(&mut h);
                h.finish()
            },
            define_key(&input.defs),
            input.plain,
        );
        let usage = cache.entry(key).or_insert_with(|| {
            let module = if input.plain {
                crate::shader_contract::parse_plain(&input.source)
            } else {
                crate::shader_lib::compose_source(&input.source, &input.file, input.defs.clone())
                    .map_err(|e| e.to_string())
            };
            match module.and_then(|m| module_usage(&m)) {
                Ok(u) => Some(u),
                Err(err) => {
                    skipped.push(format!("{}: {err}", input.file));
                    None
                }
            }
        });
        let Some(usage) = usage else { continue };
        match origin_preset(&input.origin) {
            Some(preset) => tallies
                .entry(preset.to_string())
                .or_insert_with(Tally::new)
                .add(&input.file, usage),
            None => engine.add(&input.file, usage),
        }
    }
    let mut presets = Vec::new();
    let mut failures = Vec::new();
    for (name, tally) in &tallies {
        let is_mobile = name.split('/').nth(1).is_some_and(|p| p.starts_with("Mobile"));
        let class = if is_mobile { &mobile } else { &desktop };
        let result = tally.finish(name, class, &downlevel);
        if class.gate {
            failures.extend(result.over.iter().map(|o| format!("{name}: {o}")));
        }
        presets.push(result);
    }
    let engine = engine.finish("engine", &mobile, &downlevel);
    let unverified = classes
        .iter()
        .flat_map(|c| {
            c.limits
                .iter()
                .filter(|(_, n)| !n.verified)
                .map(|(m, n)| format!("{} {m} = {} ({})", c.name, n.value, n.source))
                .collect::<Vec<_>>()
        })
        .collect();
    LimitsReport {
        format_version: SHADER_LIMITS_REPORT_VERSION,
        classes,
        presets,
        engine,
        skipped,
        failures,
        unverified,
    }
}

/// The limit class a preset's pipelines are held to (ADR-4901): `mobile` for the Mobile presets,
/// `desktop` for every other real-time preset ([`limit_classes`]).
#[must_use]
pub fn preset_limit_class(preset: crate::RenderProfilePreset) -> &'static str {
    use crate::RenderProfilePreset as P;
    match preset {
        P::MobileLow | P::MobileHigh => "mobile",
        _ => "desktop",
    }
}

/// What one host module of a material graph's pipeline set reads, per entry point (ADR-4901).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphModuleUsage {
    /// The graph pass module: `forward`, `forward, instanced`, `depth, instanced`, `shadow`,
    /// `shadow, instanced` or `velocity` (the modules `validate_graph_code` composes).
    pub module: String,
    /// What each entry point reads.
    pub stages: Vec<StageUsage>,
}

/// Composes every host module of a material graph's pipeline set (the forward module for the
/// colour, instanced and depth passes, and with a vertex stage the shadow and velocity modules,
/// as [`crate::surface_graph::validate_graph_code`] does) and counts what each entry point reads.
///
/// # Errors
///
/// The generated code does not compose or validate in a host module: the one compile failure
/// no authored graph node maps to (`generated_code_invalid`). The message names the module.
pub fn graph_code_usage(code: &crate::GraphCode) -> Result<Vec<GraphModuleUsage>, String> {
    use crate::surface_graph::{code_defs, forward_source_with_code, vertex_host_defs, vertex_host_source};
    let _span = tracing::info_span!("graph_code_usage").entered();
    let mut out = Vec::new();
    let mut count = |module: Result<naga::Module, crate::RenderError>, what: &str| -> Result<(), String> {
        let module = module.map_err(|e| format!("{what}: {e}"))?;
        let stages = module_usage(&module).map_err(|e| format!("{what}: {e}"))?;
        out.push(GraphModuleUsage {
            module: what.to_string(),
            stages,
        });
        Ok(())
    };
    let forward = forward_source_with_code(code).map_err(|e| format!("forward: {e}"))?;
    for (extra, what) in [
        (&["ALPHA_TEST"][..], "forward"),
        (
            &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"][..],
            "forward, instanced",
        ),
        (&["GPU_DRIVEN", "ALPHA_TEST"][..], "depth, instanced"),
    ] {
        count(
            crate::shader_lib::compose_source(&forward, "lab_forward_graph.wgsl", code_defs(code, extra)),
            what,
        )?;
    }
    if let Some(shadow) = vertex_host_source(code, false).map_err(|e| format!("shadow: {e}"))? {
        for (extra, what) in [(&[][..], "shadow"), (&["GPU_DRIVEN"][..], "shadow, instanced")] {
            count(
                crate::shader_lib::compose_source(
                    &shadow,
                    "lab_shadow_graph.wgsl",
                    vertex_host_defs(code, extra),
                ),
                what,
            )?;
        }
    }
    if let Some(velocity) = vertex_host_source(code, true).map_err(|e| format!("velocity: {e}"))? {
        count(
            crate::shader_lib::compose_source(&velocity, "velocity_graph.wgsl", vertex_host_defs(code, &[])),
            "velocity",
        )?;
    }
    Ok(out)
}

/// The worst use of each metric of [`LIMIT_METRICS`] over `usage` against `class`, and the
/// metrics over the class limit as `<metric> <used> > <limit> (<module> <entry> <stage>)`.
#[must_use]
pub fn graph_limit_check(usage: &[GraphModuleUsage], class: &LimitClass) -> (Vec<MetricUse>, Vec<String>) {
    let mut worst: Vec<MetricUse> = Vec::new();
    let mut over = Vec::new();
    for metric in LIMIT_METRICS {
        let limit = class.limits.get(metric).map_or(u64::MAX, |n| n.value);
        let mut best: Option<MetricUse> = None;
        for module in usage {
            for stage in &module.stages {
                let used = stage.counts.metric(metric);
                if used > limit {
                    over.push(format!(
                        "{metric} {used} > {limit} ({} {} {})",
                        module.module, stage.entry, stage.stage
                    ));
                }
                if best.as_ref().is_none_or(|b| used > b.used) {
                    best = Some(MetricUse {
                        metric: metric.to_string(),
                        used,
                        limit,
                        headroom: i64::try_from(limit).unwrap_or(i64::MAX)
                            - i64::try_from(used).unwrap_or(i64::MAX),
                        module: module.module.clone(),
                        entry: format!("{} ({})", stage.entry, stage.stage),
                    });
                }
            }
        }
        worst.extend(best);
    }
    (worst, over)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "
struct U { a: vec4<f32>, b: vec4<f32> }
struct S { v: vec4<f32> }
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var<storage, read_write> s: array<S>;
@group(1) @binding(0) var t0: texture_2d<f32>;
@group(1) @binding(1) var t1: texture_2d<f32>;
@group(1) @binding(2) var smp: sampler;
@group(2) @binding(0) var st: texture_storage_2d<rgba8unorm, write>;
@group(3) @binding(0) var<uniform> unused: U;
@compute @workgroup_size(1) fn cs() {
    let c = textureLoad(t0, vec2<i32>(0), 0) + textureLoad(t1, vec2<i32>(0), 0) + u.a + s[0].v;
    textureStore(st, vec2<i32>(0), c);
    _ = smp;
}
@fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(0.0); }
";

    #[test]
    fn counts_come_from_what_each_entry_point_reads() {
        let module = crate::shader_contract::parse_plain(SRC).expect("parses");
        let usage = module_usage(&module).expect("validates");
        let cs = usage.iter().find(|u| u.entry == "cs").expect("cs");
        assert_eq!(
            cs.counts,
            BindingCounts {
                uniform_buffers: 1,
                storage_buffers: 1,
                storage_textures: 1,
                sampled_textures: 2,
                samplers: 1,
                binding_array_elements: 0,
                bind_groups: 3,
                max_uniform_bytes: 32,
                max_storage_bytes: 16,
            }
        );
        let fs = usage.iter().find(|u| u.entry == "fs").expect("fs");
        assert_eq!(
            fs.counts,
            BindingCounts::default(),
            "an unused binding is not counted"
        );
    }

    /// A binding array of textures counts its elements apart and adds no sampled texture, as
    /// the `BindingCounts` docs say (corr-render-rest LOW 21: they said it counted as one).
    #[test]
    fn a_binding_array_counts_its_elements_not_a_sampled_texture() {
        let src = "
enable wgpu_binding_array;
@group(0) @binding(0) var table: binding_array<texture_2d<f32>, 4>;
@group(0) @binding(1) var plain: texture_2d<f32>;
@compute @workgroup_size(1) fn cs() {
    _ = textureLoad(table[1], vec2<i32>(0), 0) + textureLoad(plain, vec2<i32>(0), 0);
}
";
        let module = crate::shader_contract::parse_plain(src).expect("parses");
        let usage = module_usage(&module).expect("validates");
        let cs = usage.iter().find(|u| u.entry == "cs").expect("cs");
        assert_eq!(cs.counts.sampled_textures, 1);
        assert_eq!(cs.counts.binding_array_elements, 4);
        assert_eq!(cs.counts.metric("sampled_textures"), 1);
        assert!(!LIMIT_METRICS.contains(&"binding_array_elements"));
    }

    #[test]
    fn the_mobile_gate_is_the_smallest_of_the_floor_and_both_phones() {
        let classes = limit_classes();
        let mobile = classes.iter().find(|c| c.name == "mobile").expect("mobile");
        assert!(mobile.gate);
        // The Fairphone 5 reports 4 bound descriptor sets and a 64 KiB uniform range; the engine
        // tier floor holds uniforms to 16 KiB.
        assert_eq!(mobile.limits["bind_groups"].value, 4);
        assert_eq!(mobile.limits["uniform_binding_bytes"].value, 16 << 10);
        assert_eq!(mobile.limits["storage_buffers"].value, 8);
        let down = classes.iter().find(|c| c.name == "downlevel").expect("downlevel");
        assert_eq!(down.limits["storage_buffers"].value, 4);
        assert!(!down.gate);
    }

    #[test]
    fn a_mobile_preset_over_a_limit_fails_and_a_desktop_one_only_reports() {
        let tight = |preset: &str| MatrixInput {
            file: "t.wgsl".into(),
            source: SRC.into(),
            defs: HashMap::new(),
            origin: format!("android/{preset} x"),
            plain: true,
        };
        let report = run_shader_limits(&[tight("MobileLow")]);
        assert!(report.ok(), "{:?}", report.failures);
        // 33 sampled textures in one stage is over the mobile gate of 32.
        let mut many = String::new();
        let mut sum = String::from("vec4<f32>(0.0)");
        for i in 0..33 {
            many += &format!("@group(0) @binding({i}) var t{i}: texture_2d<f32>;\n");
            sum += &format!(" + textureLoad(t{i}, vec2<i32>(0), 0)");
        }
        let src = format!("{many}@fragment fn fs() -> @location(0) vec4<f32> {{ return {sum}; }}");
        let over = |origin: &str| MatrixInput {
            file: "many.wgsl".into(),
            source: src.clone().into(),
            defs: HashMap::new(),
            origin: origin.into(),
            plain: true,
        };
        let report = run_shader_limits(&[over("android/MobileHigh x")]);
        assert!(!report.ok());
        assert!(
            report.failures[0].contains("sampled_textures 33 > 32"),
            "{:?}",
            report.failures
        );
        let report = run_shader_limits(&[over("linux/DesktopHigh x")]);
        assert!(report.ok(), "desktop presets report headroom only");
        assert_eq!(
            report.presets[0].over_downlevel.len(),
            1,
            "33 sampled textures is over the downlevel 16"
        );
    }

    /// The Android permutations of every shipped preset fit the mobile class: the smallest of the
    /// engine's Mobile tier floor and the Pixel 6 and Fairphone 5 limits. Fails with the preset,
    /// the metric, the module and the entry point.
    #[test]
    fn every_android_preset_fits_the_mobile_class() {
        let inputs: Vec<MatrixInput> = crate::shader_matrix_inputs(crate::MatrixScope::Full)
            .into_iter()
            .filter(|i| i.origin.starts_with("android/"))
            .collect();
        assert!(!inputs.is_empty());
        let report = run_shader_limits(&inputs);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.ok(), "{}", report.failures.join("\n"));
        assert!(report.presets.iter().any(|p| p.preset == "android/MobileHigh"));
    }

    fn sample_code(vertex: bool) -> crate::GraphCode {
        crate::GraphCode {
            surface: crate::surface_graph::SAMPLE_SURFACE.into(),
            vertex: vertex.then(|| crate::surface_graph::SAMPLE_VERTEX.into()),
            uv1: false,
        }
    }

    /// A material graph's pipeline set is counted per pass module and fits the class of every
    /// shipped preset (ADR-4901): the Mobile presets against the `mobile` gate, the others
    /// against `desktop`.
    #[test]
    fn a_graph_set_is_counted_per_pass_module_and_fits_every_preset_class() {
        let classes = limit_classes();
        for vertex in [false, true] {
            let usage = graph_code_usage(&sample_code(vertex)).unwrap_or_else(|e| panic!("{e}"));
            let modules: Vec<&str> = usage.iter().map(|m| m.module.as_str()).collect();
            let expected: &[&str] = if vertex {
                &[
                    "forward",
                    "forward, instanced",
                    "depth, instanced",
                    "shadow",
                    "shadow, instanced",
                    "velocity",
                ]
            } else {
                &["forward", "forward, instanced", "depth, instanced"]
            };
            assert_eq!(modules, expected);
            for platform in crate::permutation_policy::SHIPPED_PLATFORMS {
                for preset in crate::permutation_policy::platform_presets(platform) {
                    let name = preset_limit_class(preset);
                    let class = classes.iter().find(|c| c.name == name).expect("class");
                    let (worst, over) = graph_limit_check(&usage, class);
                    assert_eq!(worst.len(), LIMIT_METRICS.len());
                    if class.gate {
                        assert!(over.is_empty(), "{platform}/{}: {over:?}", preset.name());
                    }
                }
            }
        }
        assert_eq!(
            preset_limit_class(crate::RenderProfilePreset::MobileLow),
            "mobile"
        );
        assert_eq!(preset_limit_class(crate::RenderProfilePreset::Deck), "desktop");
    }

    /// An invalid binding layout (a set over a class limit) is an explicit failure naming the
    /// metric, the pass module and the entry point.
    #[test]
    fn a_graph_set_over_a_class_limit_fails_naming_metric_and_module() {
        let usage = graph_code_usage(&sample_code(true)).unwrap_or_else(|e| panic!("{e}"));
        let mut tight = limit_classes()
            .into_iter()
            .find(|c| c.name == "mobile")
            .expect("mobile");
        for metric in ["sampled_textures", "bind_groups"] {
            if let Some(n) = tight.limits.get_mut(metric) {
                n.value = 1;
            }
        }
        let (worst, over) = graph_limit_check(&usage, &tight);
        assert!(
            over.iter()
                .any(|o| o.starts_with("sampled_textures ") && o.contains("forward")),
            "{over:?}"
        );
        assert!(over.iter().any(|o| o.starts_with("bind_groups ")), "{over:?}");
        let sampled = worst
            .iter()
            .find(|w| w.metric == "sampled_textures")
            .expect("sampled_textures");
        assert!(sampled.headroom < 0, "{sampled:?}");
    }

    /// Generated code a host module rejects is an error naming the module: the compile failure
    /// no authored node maps to is never skipped.
    #[test]
    fn generated_code_a_host_module_rejects_is_an_explicit_error() {
        let broken = crate::GraphCode {
            surface: "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {\n    return no_such_value;\n}\n".into(),
            vertex: None,
            uv1: false,
        };
        let err = graph_code_usage(&broken).expect_err("broken code");
        assert!(err.starts_with("forward"), "{err}");
    }
}
