// SPDX-License-Identifier: MPL-2.0
//! The frame's transient pool (ADR-2018): the HDR target, the refraction copy, the glass front
//! layer and the SSAO targets come from the view's pool, the same textures every frame.
//!
//! A refracting glass frame in Desktop High takes its targets from the pool on the first frame;
//! later frames create nothing and reuse every one of them; `render.graph`'s report shows the
//! counters. A frame without refracting glass takes three textures fewer (the copy and the
//! front layer's two), and the pool drops them after its idle frames.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FramePass, FramePassContext, FrameQuality, FrameRenderer,
    GraphSlot, LabMaterial, LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, TextureViewSettings,
};

const W: u32 = 160;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A floor with bright bars under a refracting glass pane, Desktop High (SSAO on, linear HDR
/// target, screen-space refraction on, TAAU off): the frame takes the HDR target, the
/// refraction copy and the SSAO targets every frame.
fn glass_scene() -> (LabScene, PostSettings, FrameQuality) {
    let slab = |half: [f32; 3], model: Mat4, material: usize| {
        let (vertices, indices) = regia_render::box_mesh(half);
        LabMesh {
            vertices,
            indices,
            material,
            model: model.to_cols_array_2d(),
            ..LabMesh::unskinned()
        }
    };
    let floor = LabMaterial {
        base_color: [0.4, 0.4, 0.4, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let bar = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 0.9, 0.8],
        emissive_intensity: 3000.0,
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.05,
        transmission: 1.0,
        ior: 1.5,
        transmission_thickness: 0.05,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    };
    let mut meshes = vec![slab(
        [10.0, 0.005, 10.0],
        Mat4::from_translation(Vec3::new(0.0, -1.005, 0.0)),
        0,
    )];
    for i in -6..=6 {
        meshes.push(slab(
            [0.125, 0.2, 10.0],
            Mat4::from_translation(Vec3::new(i as f32 * 0.7, -0.8, 0.0)),
            1,
        ));
    }
    meshes.push(slab([1.2, 0.005, 1.2], Mat4::from_rotation_z(0.3), 2));
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: [0.0, 4.0, 2.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], 20000.0),
        local_lights: Vec::new(),
        materials: vec![floor, bar, glass],
        meshes,
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.screen_space_refraction = true;
    (scene, post, quality)
}

#[test]
fn steady_frames_reuse_every_pooled_target_and_create_none() {
    let device = device();
    let (scene, post, quality) = glass_scene();
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &frame).expect("first frame");
    let first = renderer.graph_report().graph.transients;
    // HDR target, refraction copy, front layer depth and transmittance, SSAO kernel and blur.
    assert_eq!(first.textures, 6, "{first:?}");
    assert_eq!(first.created, 6);
    assert_eq!(first.in_use, 6);
    assert!(first.bytes > 0);
    for _ in 0..5 {
        renderer.render_offscreen(&device, &frame).expect("steady frame");
    }
    let steady = renderer.graph_report().graph.transients;
    assert_eq!(
        steady.created, first.created,
        "a steady frame creates no transient"
    );
    assert_eq!(steady.reused, first.reused + 5 * first.textures as u64);
    assert_eq!(steady.evicted, 0);

    // Without refraction the copy and the front layer are not taken; the pool drops them.
    let mut plain = quality;
    plain.screen_space_refraction = false;
    let mut frame = scene.as_frame_scene_with_quality(plain);
    frame.post = Some(&post);
    for _ in 0..(regia_render::TransientPool::DEFAULT_IDLE_FRAMES + 2) {
        renderer.render_offscreen(&device, &frame).expect("plain frame");
    }
    let plain = renderer.graph_report().graph.transients;
    assert_eq!(plain.in_use, 3, "{plain:?}");
    assert_eq!(
        plain.textures, 3,
        "the idle refraction targets are dropped: {plain:?}"
    );
    assert_eq!(plain.evicted, 3);
    assert_eq!(plain.created, first.created);
}

/// A `before_post` pass that declares a colour write (and records nothing): the frame hands it a
/// per-frame copy of the HDR image (ADR-1863).
struct BeforePostWriter;

impl FramePass for BeforePostWriter {
    fn name(&self) -> &str {
        "before_post_writer"
    }
    fn slot(&self) -> GraphSlot {
        GraphSlot::BeforePost
    }
    fn reads(&self) -> &[&'static str] {
        &["color"]
    }
    fn writes(&self) -> &[&'static str] {
        &["color"]
    }
    fn record(&mut self, _ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        Ok(())
    }
}

/// The `before_post` copy is a frame transient of the view's pool (ADR-4945, J04): it is taken
/// from the pool the frame a writing `before_post` pass needs it, steady frames reuse it and
/// create nothing, and once no pass needs it the pool drops it after its idle frames. (Before,
/// the copy was a field of the pass list, shared by every view and outside the pool's counters.)
#[test]
fn the_before_post_copy_is_a_pooled_transient_of_the_view() {
    let device = device();
    let (scene, post, quality) = glass_scene();
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &frame).expect("first frame");
    let plain = renderer.graph_report().graph.transients;
    renderer
        .register_pass(Box::new(BeforePostWriter))
        .expect("register");
    renderer.render_offscreen(&device, &frame).expect("copy frame");
    let copied = renderer.graph_report().graph.transients;
    assert_eq!(
        copied.textures,
        plain.textures + 1,
        "the before_post copy is taken from the view's pool: {copied:?}"
    );
    assert_eq!(copied.created, plain.created + 1);
    assert_eq!(copied.in_use, plain.in_use + 1);
    for _ in 0..4 {
        renderer.render_offscreen(&device, &frame).expect("steady frame");
    }
    let steady = renderer.graph_report().graph.transients;
    assert_eq!(steady.created, copied.created, "a steady frame creates no copy");
    assert_eq!(steady.evicted, 0);

    // A texture view of the same scene takes its copy from its own pool (ADR-4945): the main
    // view's pool gains nothing and keeps its textures for the main view's next frame.
    let mut view_frame = scene.as_frame_scene_with_quality(quality);
    view_frame.post = Some(&post);
    renderer
        .render_texture_view(&device, 7, view_frame, &TextureViewSettings::default())
        .expect("texture view");
    renderer
        .render_offscreen(&device, &frame)
        .expect("main frame after the view");
    let main = renderer.graph_report().graph.transients;
    assert_eq!(
        (main.textures, main.created, main.evicted),
        (steady.textures, steady.created, 0),
        "the texture view's frame left the main view's pool alone: {main:?}"
    );

    assert!(renderer.remove_pass("before_post_writer").is_some());
    for _ in 0..(regia_render::TransientPool::DEFAULT_IDLE_FRAMES + 2) {
        renderer.render_offscreen(&device, &frame).expect("no-copy frame");
    }
    let after = renderer.graph_report().graph.transients;
    assert_eq!(
        after.textures, plain.textures,
        "the idle copy is dropped: {after:?}"
    );
    assert_eq!(after.evicted, 1);
    assert_eq!(after.created, copied.created);
}
