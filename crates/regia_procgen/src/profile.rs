// SPDX-License-Identifier: MPL-2.0
//! Schedule profiling (spec 62 § 5.1, ADR-2724): runs a generator's graph on its captured world at
//! several thread counts without publishing anything, and reports per-node wall times and whether
//! every schedule produced the same bytes. Behind the agent's `procgen.profile`.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;

use crate::exec::{CancelToken, Evaluator, Parallelism, RunError, RunInputs, RunLimits, RunLimitsOverride};
use crate::graph::resolve_graph;
use crate::world::{
    ProcGenRegistry, ProcGenSettings, ProcGenerator, capture, generator_seed, read_graph_text,
    terrain_edit_gate,
};

/// Most thread counts one profile may ask for.
pub const MAX_PROFILE_RUNS: usize = 8;

/// One node's line of a profiled run.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileNode {
    /// Node id.
    pub id: u32,
    /// Node type.
    pub ty: String,
    /// Wall time, milliseconds.
    pub ms: f64,
    /// Elements over its outputs.
    pub elements: usize,
}

/// One profiled run.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileRun {
    /// Requested threads (0 = the serial schedule).
    pub threads: usize,
    /// Output hash, or `None` when the run failed.
    pub output_hash: Option<[u8; 32]>,
    /// Run wall time, milliseconds.
    pub total_ms: f64,
    /// Per node in compiled order (empty when the run failed).
    pub nodes: Vec<ProfileNode>,
    /// `(node, code)` of a failed run.
    pub error: Option<(Option<u32>, String)>,
}

/// Runs `entity`'s graph once per entry of `threads` (0 = serial, else a dedicated pool of that
/// size), each without a cache, on one captured snapshot.
///
/// # Errors
///
/// `(code, message)`: `procgen_not_generator`, the graph load code, `procgen_param_type` or
/// `procgen_graph_invalid`.
pub fn profile_generator(
    world: &mut World,
    entity: Entity,
    threads: &[usize],
) -> Result<Vec<ProfileRun>, (String, String)> {
    let _span = tracing::info_span!("procgen.profile", runs = threads.len()).entered();
    let Some(generator) = world.get::<ProcGenerator>(entity).cloned() else {
        return Err(("procgen_not_generator".into(), "not a generator".into()));
    };
    let registry = world.resource::<ProcGenRegistry>().0.clone();
    let resolved =
        resolve_graph(&generator.graph, &|p| read_graph_text(world, p)).map_err(|e| (e.code, e.message))?;
    let params = generator.parsed_params().map_err(|k| {
        (
            "procgen_param_type".to_string(),
            format!("parameter `{k}` is not RON"),
        )
    })?;
    let merged = resolved.merged_with(&params);
    let compiled = resolved
        .graph
        .compile_full(&registry, &merged, &|p| read_graph_text(world, p))
        .map_err(|e| ("procgen_graph_invalid".to_string(), e.to_string()))?;
    let graphs: BTreeMap<String, String> =
        crate::graph::collect_subgraphs_with(&resolved.graph, &merged, &|p| read_graph_text(world, p));
    let (snap, _) = capture(world, entity, &compiled);
    let settings = world.resource::<ProcGenSettings>().clone();
    let mut limits = RunLimits::for_platform();
    if let Some(l) = generator.limits {
        let u = |v: Option<u64>| v.and_then(|x| usize::try_from(x).ok());
        limits = limits.with_override(&RunLimitsOverride {
            points_per_item: u(l.points_per_item),
            total_elements: u(l.total_elements),
            memory_bytes: u(l.memory_bytes),
            prefabs_per_generator: u(l.prefabs_per_generator),
            ..RunLimitsOverride::default()
        });
    }
    let inputs = RunInputs {
        world: Arc::new(snap),
        gen_seed: generator_seed(world, entity),
        tier_index: 0,
        cell: None,
        // The tier the generator's own run uses: a `Baked` generator with a fixed tier runs at it,
        // not at the running tier (corr-procgen).
        quality: crate::world::run_quality(&generator, &settings),
        limits,
        keep_debug: false,
        graphs: Arc::new(graphs),
        terrain_edits: terrain_edit_gate(&generator, &settings),
        run_rect: None,
        tile: None,
    };
    Ok(threads
        .iter()
        .take(MAX_PROFILE_RUNS)
        .map(|&t| {
            let ev = Evaluator::new().with_parallelism(Parallelism::threads(t));
            match ev.run(&compiled, &inputs, &CancelToken::new()) {
                Ok(r) => ProfileRun {
                    threads: t,
                    output_hash: Some(r.output_hash),
                    total_ms: r.report.total_ms,
                    nodes: r
                        .report
                        .nodes
                        .iter()
                        .map(|n| ProfileNode {
                            id: n.id,
                            ty: n.ty.clone(),
                            ms: n.ms,
                            elements: n.elements,
                        })
                        .collect(),
                    error: None,
                },
                Err(e) => ProfileRun {
                    threads: t,
                    output_hash: None,
                    total_ms: 0.0,
                    nodes: Vec::new(),
                    error: Some(match e {
                        RunError::Node { node, code, .. } => (Some(node), code),
                        RunError::Cancelled => (None, "cancelled".to_string()),
                    }),
                },
            }
        })
        .collect())
}
