// SPDX-License-Identifier: MPL-2.0
//! The frame graph on the agent protocol (`book/agents/render-graph.md`, ADR-1552):
//! `render.graph` lists the runtime renderer's built-in passes in recording order, the named
//! slots between them, the passes a host registered at those slots
//! ([`regia_render::FrameRenderer::register_pass`]) with their counters, and the last frame's GPU
//! timings ([`regia_render::FrameRenderer::graph_report`]).

use regia_app::prelude::*;
use regia_render::{FrameGraphReport, FrameStep, RuntimeRenderer, slot_color_space, slot_name};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::graph_report)",
    )
}

fn report_json(report: &FrameGraphReport) -> Value {
    let timings = report.gpu.passes();
    let gpu_ms = |label: &str| {
        timings
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, ms)| f64::from(*ms))
    };
    let steps: Vec<Value> = report
        .steps
        .iter()
        .map(|step| match step {
            FrameStep::Builtin {
                name,
                module,
                reads,
                writes,
                timing,
                pipelines,
            } => json!({
                "kind": "pass",
                "name": name,
                "module": module,
                "reads": reads,
                "writes": writes,
                "pipelines": pipelines,
                "gpu_ms": timing.and_then(gpu_ms).filter(|_| report.gpu_timing),
            }),
            FrameStep::Slot(slot) => {
                let passes: Vec<&str> = report
                    .extensions
                    .iter()
                    .filter(|p| p.slot == *slot)
                    .map(|p| p.name.as_str())
                    .collect();
                json!({
                    "kind": "slot",
                    "name": slot_name(*slot),
                    "wired": slot_color_space(*slot).is_some(),
                    "color": slot_color_space(*slot).map(|space| space.as_str()),
                    "passes": passes,
                })
            }
        })
        .collect();
    let extensions: Vec<Value> = report
        .extensions
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "slot": slot_name(p.slot),
                "wired": p.wired,
                "reads": p.reads,
                "writes": p.writes,
                "runs": p.runs,
                "failures": p.failures,
                "last_cpu_ms": p.last_cpu_ms,
                "last_frame": p.last_frame,
                "last_error": p.last_error,
            })
        })
        .collect();
    let graph = &report.graph;
    let nodes: Vec<Value> = graph
        .nodes
        .iter()
        .map(|node| {
            json!({
                "name": node.name,
                "registered": node.registered,
                "slot": node.slot.map(slot_name),
                "reads": node.reads,
                "writes": node.writes,
                "pipelines": node.pipelines,
                "live": node.live,
                "buffer_reads": node.buffer_reads,
                "buffer_writes": node.buffer_writes,
            })
        })
        .collect();
    let transients = &graph.transients;
    // Every pooled texture with its slot key (ADR-7877: the two HDR scratch images are keyed).
    let pooled: Vec<Value> = graph
        .pooled
        .iter()
        .map(|entry| {
            json!({
                "label": entry.label,
                "key": entry.key,
                "width": entry.desc.width,
                "height": entry.desc.height,
                "format": format!("{:?}", entry.desc.format),
                "bytes": entry.bytes,
                "generation": entry.generation,
                "used_this_frame": entry.used_this_frame,
            })
        })
        .collect();
    let dispatch = &report.dispatch;
    json!({
        "steps": steps,
        "graph": {
            "nodes": nodes,
            "order": graph.order,
            "error": graph.error.as_ref().map(|err| json!({ "code": err.code, "message": err.message })),
            "warm_keys": graph.warm_keys,
            "transients": {
                "textures": transients.textures,
                "bytes": transients.bytes,
                "in_use": transients.in_use,
                "created": transients.created,
                "reused": transients.reused,
                "evicted": transients.evicted,
                "aliased": transients.aliased,
            },
            "keyed": {
                "textures": graph.keyed.textures,
                "bytes": graph.keyed.bytes,
                "in_use": graph.keyed.in_use,
                "created": graph.keyed.created,
                "reused": graph.keyed.reused,
                "evicted": graph.keyed.evicted,
            },
            "transient_writes": graph.transient_writes,
            "pooled": pooled,
            "buffers": graph.buffers.iter().map(|b| json!({
                "name": b.name, "owner": b.owner, "memory": b.memory, "bytes": b.bytes,
                "generation": b.generation, "imported": b.imported, "history": b.history,
                "writers": b.writers, "readers": b.readers,
            })).collect::<Vec<_>>(),
        },
        "dispatch": {
            "frames": dispatch.frames,
            "last_frame": dispatch.last_frame,
            "recorded": dispatch.recorded,
            "inactive": dispatch.inactive,
            "skipped": dispatch.skipped,
            "late": dispatch.late,
            "violations": dispatch.violations,
            "last_violation": dispatch.last_violation.map(|v| json!({
                "code": v.code,
                "node": v.node,
                "frame": v.frame,
            })),
        },
        "extensions": extensions,
        "gpu_timing": report.gpu_timing,
        "gpu_scene_ms": f64::from(report.gpu.scene_ms),
        "forward_permutations": {
            "count": report.forward_permutations.permutations.len(),
            "budget": report.forward_permutations.budget,
            "over_budget": report.forward_permutations.over_budget,
            "undeclared": report.forward_permutations.undeclared,
            "axes": regia_render::FORWARD_AXES.iter().map(|axis| axis.name).collect::<Vec<_>>(),
        },
    })
}

/// `render.graph`: `{ steps: [{ kind: "pass", name, module, reads, writes, pipelines, gpu_ms } | {
/// kind: "slot", name, wired, color, passes }], graph: { nodes: [{ name, registered, slot, reads,
/// writes, pipelines, live }], order, error, warm_keys, transients: { textures, bytes, in_use,
/// created, reused, evicted, aliased } }, dispatch: { frames, last_frame, recorded, inactive, skipped,
/// late, violations, last_violation: { code, node, frame } | null }, extensions: [{ name, slot,
/// wired, reads, writes, runs, failures, last_cpu_ms, last_frame, last_error }], gpu_timing,
/// gpu_scene_ms,
/// forward_permutations: { count, budget, over_budget, undeclared, axes } }` of the runtime
/// renderer. `gpu_ms` is `null` while GPU timing is off or for an untimed pass.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    Ok(report_json(&renderer.0.graph_report()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{FramePass, FramePassContext, FrameRenderer, GraphSlot};

    #[test]
    fn graph_buffers_are_exposed_with_owners_and_dependencies() {
        let mut renderer = FrameRenderer::new();
        renderer
            .register_pass(Box::new(BufferReader))
            .expect("register buffer reader");
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(renderer));
        let got = get(&mut world).expect("render.graph handler");
        let buffers = got["graph"]["buffers"].as_array().expect("buffer audit");
        assert_eq!(buffers.len(), 10);
        let counts = buffers
            .iter()
            .find(|b| b["name"] == "counts_buf")
            .expect("counts");
        assert!(
            counts["readers"]
                .as_array()
                .expect("readers")
                .iter()
                .any(|n| n == "buffer_reader")
        );
        for b in buffers {
            assert_eq!(b["bytes"], 0);
            assert!(!b["owner"].as_str().expect("buffer audit").is_empty());
            assert!(!b["writers"].as_array().expect("buffer audit").is_empty());
            assert!(!b["readers"].as_array().expect("buffer audit").is_empty());
        }
        let cluster = got["graph"]["nodes"]
            .as_array()
            .expect("buffer audit")
            .iter()
            .find(|n| n["name"] == "cluster_lights")
            .expect("buffer audit");
        assert_eq!(cluster["buffer_writes"], json!(["counts_buf#1", "indices_buf#1"]));
        assert_eq!(cluster["buffer_reads"], json!(["lights_buf#0"]));
        assert_eq!(cluster["writes"], json!(["clusters#1"]));
    }

    struct BufferReader;

    impl FramePass for BufferReader {
        fn name(&self) -> &str {
            "buffer_reader"
        }
        fn slot(&self) -> GraphSlot {
            GraphSlot::AfterOpaque
        }
        fn reads(&self) -> &[&'static str] {
            &["counts_buf"]
        }
        fn record(&mut self, _ctx: &mut FramePassContext<'_>) -> Result<(), String> {
            Ok(())
        }
    }

    struct Outline;

    impl FramePass for Outline {
        fn name(&self) -> &str {
            "outline"
        }
        fn slot(&self) -> GraphSlot {
            GraphSlot::AfterPost
        }
        fn reads(&self) -> &[&'static str] {
            &["output", "depth"]
        }
        fn record(&mut self, _ctx: &mut FramePassContext<'_>) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn lists_the_built_in_passes_slots_and_registered_passes() {
        let mut world = World::new();
        let mut renderer = FrameRenderer::new();
        renderer.register_pass(Box::new(Outline)).expect("register");
        world.insert_resource(RuntimeRenderer(renderer));
        let got = get(&mut world).expect("read");
        let steps = got["steps"].as_array().expect("steps");
        let names: Vec<&str> = steps.iter().filter_map(|s| s["name"].as_str()).collect();
        let post = names.iter().position(|n| *n == "post_stack").expect("post");
        let after_post = names.iter().position(|n| *n == "after_post").expect("slot");
        assert!(post < after_post, "{names:?}");
        let slot = &steps[after_post];
        assert_eq!(slot["kind"], "slot");
        assert_eq!(slot["wired"], true);
        assert_eq!(slot["color"], "display");
        assert_eq!(slot["passes"][0], "outline");
        let opaque = steps
            .iter()
            .find(|s| s["name"] == "after_opaque")
            .expect("after_opaque");
        assert_eq!(opaque["wired"], true);
        assert_eq!(opaque["color"], "scene_linear_hdr");
        assert_eq!(got["extensions"][0]["name"], "outline");
        assert_eq!(got["extensions"][0]["runs"], 0);
        assert_eq!(got["extensions"][0]["reads"][1], "depth");
        assert_eq!(got["gpu_timing"], false);
        assert_eq!(
            got["forward_permutations"]["axes"].as_array().map(Vec::len),
            Some(regia_render::FORWARD_AXES.len())
        );
        assert_eq!(got["forward_permutations"]["budget"], 256);
        assert!(
            steps
                .iter()
                .filter(|s| s["kind"] == "pass")
                .all(|s| s["gpu_ms"].is_null())
        );
        // Every pass row names the `frame::` module that records it.
        let forward = steps
            .iter()
            .find(|s| s["name"] == "forward")
            .expect("forward row");
        assert_eq!(forward["module"], "forward_pass");
        assert!(
            steps
                .iter()
                .filter(|s| s["kind"] == "pass")
                .all(|s| s["module"].as_str().is_some_and(|m| !m.is_empty()))
        );
    }

    /// The keyed pool, the built-in transient writes and the pooled listing (ADR-7877): a
    /// renderer that drew nothing reports both HDR scratch resources, zero keyed textures and an
    /// empty listing; the scratch writers list their transient writes.
    #[test]
    fn lists_the_keyed_pool_and_the_built_in_transient_writes() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let got = get(&mut world).expect("read");
        let graph = &got["graph"];
        assert_eq!(
            graph["transient_writes"],
            json!(["outdoor_hdr_scratch", "translucent_hdr_scratch"])
        );
        assert_eq!(graph["keyed"]["textures"], 0);
        assert_eq!(graph["keyed"]["bytes"], 0);
        assert_eq!(graph["pooled"], json!([]));
        let nodes = graph["nodes"].as_array().expect("nodes");
        let writes = |name: &str| -> Vec<String> {
            nodes
                .iter()
                .find(|n| n["name"] == name)
                .and_then(|n| n["writes"].as_array())
                .map(|w| w.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        assert!(
            writes("fog").contains(&"outdoor_hdr_scratch#3".to_string()),
            "{:?}",
            writes("fog")
        );
        assert!(
            writes("translucent_hdr").contains(&"translucent_hdr_scratch#1".to_string()),
            "{:?}",
            writes("translucent_hdr")
        );
    }

    /// The compiled frame graph (ADR-2018): versioned reads and writes per node, the registered
    /// pass at its slot, the warm keys and the transient pool.
    #[test]
    fn lists_the_versioned_frame_graph_warm_keys_and_transients() {
        let mut world = World::new();
        let mut renderer = FrameRenderer::new();
        renderer.register_pass(Box::new(Outline)).expect("register");
        world.insert_resource(RuntimeRenderer(renderer));
        let got = get(&mut world).expect("read");
        let graph = &got["graph"];
        assert!(graph["error"].is_null(), "{graph}");
        let nodes = graph["nodes"].as_array().expect("nodes");
        let node = |name: &str| {
            nodes
                .iter()
                .find(|n| n["name"] == name)
                .unwrap_or_else(|| panic!("{name} in {nodes:?}"))
        };
        assert_eq!(node("forward")["writes"][0], "color#4");
        assert_eq!(node("post_stack")["writes"][0], "output#1");
        // The outline reads the output after the post stack and the translucent layers.
        assert_eq!(node("outline")["reads"][0], "output#2");
        assert_eq!(node("outline")["registered"], true);
        assert_eq!(node("outline")["slot"], "after_post");
        // It writes nothing, so it always runs.
        assert_eq!(node("outline")["live"], true);
        assert!(node("forward")["slot"].is_null());
        let order: Vec<&str> = graph["order"]
            .as_array()
            .expect("order")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let post = order.iter().position(|n| *n == "post_stack").expect("post");
        let outline = order.iter().position(|n| *n == "outline").expect("outline");
        assert!(post < outline, "{order:?}");
        assert_eq!(graph["warm_keys"][0], "blue_noise");
        assert_eq!(graph["warm_keys"][1], "sun_shadow_trace");
        // ADR-2652: the many-light sampling step runs after the sun trace.
        assert_eq!(graph["warm_keys"][2], "restir_di");
        assert_eq!(graph["warm_keys"][3], "visbuffer_path");
        assert_eq!(graph["warm_keys"][4], "subsurface");
        // ADR-2308: the forward pass also warms the studio-off twins (`forward_studio`).
        assert_eq!(graph["warm_keys"][6], "forward_studio");
        assert_eq!(graph["warm_keys"][7], "oit");
        assert_eq!(graph["warm_keys"][8], "ssgi_half_res");
        assert!(
            graph["warm_keys"]
                .as_array()
                .is_some_and(|keys| keys.iter().any(|k| k == "depth_match_msaa"))
        );
        let taau = steps_row(&got, "taau");
        assert_eq!(taau["pipelines"][0], "tsr");
        // No frame drawn: the pool is empty.
        assert_eq!(graph["transients"]["textures"], 0);
        assert_eq!(graph["transients"]["created"], 0);
        // ADR-4905: no frame closed yet, so the dispatch lists nothing and counts nothing.
        let dispatch = &got["dispatch"];
        assert_eq!(dispatch["frames"], 0);
        assert!(dispatch["last_frame"].is_null());
        assert_eq!(dispatch["recorded"].as_array().map(Vec::len), Some(0));
        assert_eq!(dispatch["violations"], 0);
        assert!(dispatch["last_violation"].is_null());
        // Two-phase occlusion is off by default: its node is listed as a step, not in the graph.
        assert!(nodes.iter().all(|n| n["name"] != "prepass_occlusion"));
        assert_eq!(
            steps_row(&got, "prepass_occlusion")["module"],
            "prepass_two_phase"
        );
        // So is the shadow casters' (ADR-5178); the node comes first among the steps.
        assert!(nodes.iter().all(|n| n["name"] != "shadow_occlusion"));
        assert_eq!(steps_row(&got, "shadow_occlusion")["module"], "shadow_two_phase");
        assert_eq!(got["steps"][0]["name"], "shadow_occlusion");
    }

    /// Switching two-phase shadow occlusion on adds its node to the listed graph right before
    /// `shadows` (ADR-5178), and switching it off removes it.
    #[test]
    fn the_shadow_occlusion_node_follows_the_switch() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let order = |world: &mut World| -> Vec<String> {
            let got = get(world).expect("read");
            got["graph"]["order"]
                .as_array()
                .expect("order")
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        };
        assert!(!order(&mut world).iter().any(|n| n == "shadow_occlusion"));
        world
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_shadow_occlusion(true);
        let on = order(&mut world);
        assert_eq!(on.first().map(String::as_str), Some("shadow_occlusion"), "{on:?}");
        assert_eq!(on.get(1).map(String::as_str), Some("shadows"), "{on:?}");
        world
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_shadow_occlusion(false);
        assert!(!order(&mut world).iter().any(|n| n == "shadow_occlusion"));
    }

    fn steps_row<'a>(got: &'a Value, name: &str) -> &'a Value {
        got["steps"]
            .as_array()
            .and_then(|steps| steps.iter().find(|s| s["name"] == name))
            .unwrap_or_else(|| panic!("no step {name}"))
    }

    #[test]
    fn without_a_renderer_it_is_not_found() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no renderer");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
    }
}
