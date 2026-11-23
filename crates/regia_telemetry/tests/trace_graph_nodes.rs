// SPDX-License-Identifier: MPL-2.0
//! Spec 54 § 3 (ADR-9070): a `.regiatrace` recorded while a [`RenderGraph`] runs lists one pass
//! scope per live graph node, in compiled order, on the `gpu` lane (GPU: lavapipe counts).
//!
//! Its own test binary: the recorder and the breadcrumb writer are process-wide.

use regia_render::breadcrumbs;
use regia_render::{
    GraphExecution, GraphInput, GraphOutput, GraphResource, RenderDevice, RenderGraph, RenderGraphNode,
    TrackedDevice, TransientPool,
};
use regia_telemetry::trace::{self, EventKind, Lane, RecorderState, TraceConfig};

fn node(id: &str, inputs: &[&str], outputs: &[&str]) -> RenderGraphNode {
    RenderGraphNode {
        id: id.into(),
        inputs: inputs.iter().map(|n| GraphInput { name: (*n).into() }).collect(),
        outputs: outputs
            .iter()
            .map(|n| GraphOutput {
                name: (*n).into(),
                desc: GraphResource::Imported,
            })
            .collect(),
        slot: None,
        pipelines: Vec::new(),
        record: Box::new(|_| Ok(())),
    }
}

#[test]
fn a_recorded_trace_lists_one_scope_per_graph_node_in_order() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let tracked = TrackedDevice::observe(&device.device);
    let mut graph = RenderGraph::versioned();
    graph.add_node(node("graph_shadow", &[], &["shadow"]));
    graph.add_node(node("graph_gbuffer", &[], &["gbuffer"]));
    graph.add_node(node("graph_debug", &[], &["debug_only"]));
    graph.add_node(node("graph_lighting", &["shadow", "gbuffer"], &["color"]));
    graph.add_node(node("graph_post", &["color"], &["color"]));
    let out = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("graph_out"),
        size: wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let imports = [
        ("shadow", &view),
        ("gbuffer", &view),
        ("debug_only", &view),
        ("color", &view),
    ];

    let dir = regia_test_utils::unique_temp_dir("trace-graph-nodes");
    let recorder = trace::recorder();
    assert_eq!(recorder.state(), RecorderState::Idle);
    trace::configure(TraceConfig {
        dir: dir.clone(),
        ..TraceConfig::default()
    });
    if let Err(e) = recorder.start(Some("graph"), None) {
        panic!("start: {e}");
    }
    let mut pool = TransientPool::default();
    for frame in 1..=2u64 {
        breadcrumbs::begin_frame(frame);
        pool.begin_frame(frame);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let run = graph.execute(
            GraphExecution {
                device: &tracked,
                encoder: &mut encoder,
                pool: &mut pool,
                imports: &imports,
                frame_index: frame,
            },
            &["color"],
        );
        assert_eq!(run.recorded.len(), 4, "{:?}", run.recorded);
        device.queue.submit([encoder.finish()]);
        // A host that submits the graph's encoder itself publishes the thread's markers.
        breadcrumbs::note_untracked_submit(&device.queue);
    }
    breadcrumbs::begin_frame(3);
    if let Err(e) = recorder.stop() {
        panic!("stop: {e}");
    }
    let read = trace::read_trace(&dir.join("graph.regiatrace")).unwrap_or_else(|e| panic!("reads: {e}"));
    for frame in 1..=2u64 {
        let passes: Vec<(String, bool)> = read
            .events
            .iter()
            .filter(|e| e.lane == Lane::Gpu)
            .filter_map(|e| match &e.kind {
                EventKind::PassOpen { name, frame: f, .. } if *f == frame => Some((name.to_string(), true)),
                EventKind::PassClose { name, frame: f, .. } if *f == frame => Some((name.to_string(), false)),
                _ => None,
            })
            .filter(|(name, _)| name.starts_with("graph_"))
            .collect();
        let expected: Vec<(String, bool)> = ["graph_shadow", "graph_gbuffer", "graph_lighting", "graph_post"]
            .iter()
            .flat_map(|n| [((*n).to_string(), true), ((*n).to_string(), false)])
            .collect();
        assert_eq!(
            passes, expected,
            "frame {frame}: one scope per live node in compiled order; the culled node has none"
        );
    }
}
