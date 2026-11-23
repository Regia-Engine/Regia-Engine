// SPDX-License-Identifier: MPL-2.0
//! The frame's pass extension point (ADR-1552): a host pass registered at a named slot records
//! into the frame's encoder at that slot, in slot order, and a frame with no registered pass (or
//! with passes that record nothing) is byte-identical to the frame without the extension point.
//!
//! Runs on any headless adapter (lavapipe is the reference); no golden image is involved: every
//! check compares two frames of this test with each other or with a known clear colour.

use std::sync::{Arc, Mutex};

use regia_render::{
    DirectionalLightUniform, FramePass, FramePassContext, FrameRenderer, GraphSlot, LabMaterial, LabMesh,
    LabScene, LabVertex, PassRegisterError, PostSettings, RenderDevice, SlotColorSpace,
};

static SERIAL_GPU: Mutex<()> = Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => {
            eprintln!("frame_pass_extension adapter: {:?}", device.adapter.get_info());
            device
        }
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Two lit quads in front of the camera, with or without the post stack.
fn lab(post: bool) -> LabScene {
    let quad = |x: f32, z: f32| LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x - 0.5, -0.5, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, -0.5, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, 0.5, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.5, 0.5, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    };
    LabScene {
        width: 48,
        height: 32,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: post.then(PostSettings::default),
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.6, 0.5, 1.0],
            ..LabMaterial::default()
        }],
        meshes: vec![quad(-0.6, 0.0), quad(0.6, -0.5)],
    }
}

/// What a test pass saw at its slot.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    name: &'static str,
    slot: GraphSlot,
    color_space: SlotColorSpace,
    format: wgpu::TextureFormat,
    size: (u32, u32),
}

/// A pass that logs its context and optionally clears the slot's colour target.
struct Probe {
    name: &'static str,
    slot: GraphSlot,
    log: Arc<Mutex<Vec<Seen>>>,
    clear: Option<wgpu::Color>,
    fail: bool,
}

impl Probe {
    fn new(name: &'static str, slot: GraphSlot, log: &Arc<Mutex<Vec<Seen>>>) -> Self {
        Self {
            name,
            slot,
            log: Arc::clone(log),
            clear: None,
            fail: false,
        }
    }
}

impl FramePass for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn slot(&self) -> GraphSlot {
        self.slot
    }
    fn writes(&self) -> &[&'static str] {
        if self.clear.is_some() { &["output"] } else { &[] }
    }
    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Seen {
                name: self.name,
                slot: ctx.slot,
                color_space: ctx.color_space,
                format: ctx.color_format,
                size: ctx.color_size,
            });
        if self.fail {
            return Err("probe asked to fail".into());
        }
        if let Some(color) = self.clear {
            let _pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("probe clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: ctx.color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        Ok(())
    }
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    renderer
        .render_offscreen(device, &lab.as_frame_scene())
        .expect("offscreen frame")
        .pixels
}

#[test]
fn passes_record_at_their_slots_in_frame_order() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab(true);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut renderer = FrameRenderer::new();
    // Registered out of frame order: the slots, not the registration, order the passes.
    for (name, slot) in [
        ("e_overlay", GraphSlot::Overlay),
        ("c_hdr", GraphSlot::BeforePost),
        ("d_display", GraphSlot::AfterPost),
        ("a_opaque", GraphSlot::AfterOpaque),
        ("b_transparent", GraphSlot::AfterTransparent),
    ] {
        renderer
            .register_pass(Box::new(Probe::new(name, slot, &log)))
            .expect("register");
    }
    frame(&mut renderer, &device, &lab);
    let seen = log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let names: Vec<&str> = seen.iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        ["a_opaque", "b_transparent", "c_hdr", "d_display", "e_overlay"],
        "{seen:#?}"
    );
    for hdr in &seen[..3] {
        assert_eq!(hdr.color_space, SlotColorSpace::SceneLinearHdr, "{hdr:?}");
    }
    assert_eq!(seen[3].color_space, SlotColorSpace::Display);
    assert_eq!(seen[4].color_space, SlotColorSpace::Display);
    assert_eq!(seen[3].size, (lab.width, lab.height));
    let report = renderer.graph_report();
    assert!(
        report
            .extensions
            .iter()
            .all(|p| p.runs == 1 && p.wired && p.failures == 0),
        "{:#?}",
        report.extensions
    );
    assert_eq!(
        renderer.register_pass(Box::new(Probe::new("c_hdr", GraphSlot::Overlay, &log))),
        Err(PassRegisterError::DuplicateName("c_hdr".into()))
    );
}

#[test]
fn passes_that_record_nothing_leave_the_frame_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    for post in [true, false] {
        let lab = lab(post);
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut plain = FrameRenderer::new();
        let mut observed = FrameRenderer::new();
        let mut removed = FrameRenderer::new();
        for (name, slot) in [
            ("opaque_noop", GraphSlot::AfterOpaque),
            ("transparent_noop", GraphSlot::AfterTransparent),
            ("hdr_noop", GraphSlot::BeforePost),
            ("display_noop", GraphSlot::AfterPost),
            ("overlay_noop", GraphSlot::Overlay),
        ] {
            observed
                .register_pass(Box::new(Probe::new(name, slot, &log)))
                .expect("register");
            removed
                .register_pass(Box::new(Probe::new(name, slot, &log)))
                .expect("register");
            assert!(removed.remove_pass(name).is_some());
        }
        assert!(removed.registered_passes().is_empty());
        // Three frames each: temporal state (exposure, history) evolves the same way in all.
        for index in 0..3 {
            let a = frame(&mut plain, &device, &lab);
            let b = frame(&mut observed, &device, &lab);
            let c = frame(&mut removed, &device, &lab);
            assert!(
                a == b,
                "post {post}, frame {index}: a no-op pass changed the frame"
            );
            assert!(
                a == c,
                "post {post}, frame {index}: a removed pass changed the frame"
            );
        }
        assert!(
            !log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }
}

#[test]
fn an_after_post_pass_writes_the_output_and_a_failing_pass_is_skipped() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab(true);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut renderer = FrameRenderer::new();
    let mut fill = Probe::new("fill_red", GraphSlot::AfterPost, &log);
    fill.clear = Some(wgpu::Color::RED);
    renderer.register_pass(Box::new(fill)).expect("fill");
    let mut broken = Probe::new("broken", GraphSlot::Overlay, &log);
    broken.fail = true;
    renderer.register_pass(Box::new(broken)).expect("broken");
    let pixels = frame(&mut renderer, &device, &lab);
    assert!(
        pixels.as_chunks::<4>().0.iter().all(|px| *px == [255, 0, 0, 255]),
        "the after-post clear is the final image (nothing draws after it here)"
    );
    let report = renderer.graph_report();
    let broken = report
        .extensions
        .iter()
        .find(|p| p.name == "broken")
        .expect("listed");
    assert_eq!((broken.runs, broken.failures), (0, 1));
    assert_eq!(broken.last_error.as_deref(), Some("probe asked to fail"));
    // The next frame without the fill pass shows the scene again.
    assert!(renderer.remove_pass("fill_red").is_some());
    let scene = frame(&mut renderer, &device, &lab);
    assert!(scene.as_chunks::<4>().0.iter().any(|px| *px != [255, 0, 0, 255]));
}

/// The frame graph drives the built-in passes (ADR-4905): in every frame each built-in node the
/// graph holds records exactly once, in the graph's dependency order, or is listed inactive by its
/// per-frame switch; none is skipped, repeated or late. Registered passes keep their named slots
/// alongside. The dispatch report is the oracle (`frame_graph` unit tests show it failing on a
/// skipped, repeated or out-of-order node); no image is involved.
#[test]
fn every_held_built_in_records_once_in_graph_order() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(Probe::new("late_look", GraphSlot::AfterPost, &log)))
        .expect("after_post");
    // It writes the display image without reading it, so the graph culls the overlay passes before
    // it (`live: false`); they still record, as before the dispatch.
    let mut hud = Probe::new("hud", GraphSlot::Overlay, &log);
    hud.clear = Some(wgpu::Color::BLACK);
    renderer.register_pass(Box::new(hud)).expect("overlay");
    let mut last_frame = None;
    for (post, two_phase) in [(true, false), (false, false), (true, true), (true, false)] {
        renderer.set_two_phase_occlusion(two_phase);
        let before = renderer.graph_report().dispatch.frames;
        frame(&mut renderer, &device, &lab(post));
        let report = renderer.graph_report();
        let dispatch = &report.dispatch;
        let label = format!("post {post}, two-phase {two_phase}");
        assert_eq!(dispatch.violations, 0, "{label}: {:?}", dispatch.last_violation);
        assert!(
            dispatch.skipped.is_empty() && dispatch.late.is_empty(),
            "{label}: {dispatch:?}"
        );
        assert_eq!(
            dispatch.frames,
            before + 1,
            "{label}: one closed frame per render"
        );
        assert!(
            dispatch.last_frame > last_frame,
            "{label}: {:?}",
            dispatch.last_frame
        );
        last_frame = dispatch.last_frame;
        // Every built-in node the graph holds, culled or live, in node order.
        let held: Vec<&str> = report
            .graph
            .nodes
            .iter()
            .filter(|n| !n.registered)
            .map(|n| n.name.as_str())
            .collect();
        let culled: Vec<&str> = report
            .graph
            .nodes
            .iter()
            .filter(|n| !n.registered && !n.live)
            .map(|n| n.name.as_str())
            .collect();
        assert!(
            culled.contains(&"gizmos"),
            "{label}: the blind overlay write culls gizmos: {culled:?}"
        );
        // Recorded in the graph's order, each once.
        let ranks: Vec<usize> = dispatch
            .recorded
            .iter()
            .map(|name| {
                held.iter()
                    .position(|h| h == name)
                    .expect("recorded node is held")
            })
            .collect();
        assert!(
            ranks.windows(2).all(|w| w[0] < w[1]),
            "{label}: {:?}",
            dispatch.recorded
        );
        // Recorded and inactive cover the held nodes exactly once.
        let mut covered: Vec<&str> = dispatch
            .recorded
            .iter()
            .chain(&dispatch.inactive)
            .copied()
            .collect();
        covered.sort_unstable();
        let mut want = held.clone();
        want.sort_unstable();
        assert_eq!(covered, want, "{label}");
        for always in [
            "shadows",
            "cluster_lights",
            "forward",
            "scalar_overlays",
            "gizmos",
        ] {
            assert!(
                dispatch.recorded.contains(&always),
                "{label}: {always} not recorded"
            );
        }
        assert_eq!(dispatch.recorded.contains(&"post_stack"), post, "{label}");
        assert_eq!(held.contains(&"prepass_occlusion"), two_phase, "{label}");
    }
    // The registered passes ran at their named slots, once per frame each.
    let seen = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let at = |name: &str| {
        seen.iter()
            .filter(|s| s.name == name)
            .map(|s| s.slot)
            .collect::<Vec<_>>()
    };
    assert_eq!(at("late_look"), vec![GraphSlot::AfterPost; 4]);
    assert_eq!(at("hud"), vec![GraphSlot::Overlay; 4]);
}
