// SPDX-License-Identifier: MPL-2.0
//! The traced sun's casters from outside the mesh list (ADR-2310, `book/agents/hwrt.md`).
//!
//! A shared draw that draws nothing hands the trace a proxy: a slab floating to the side of the
//! scene of `hwrt_frame.rs`. Its shadow on the ground exists only if the proxy entered the traced
//! scene. The compute reference runs on every adapter (lavapipe in the gate); the ray-query frames
//! (the TLAS with shared, masked and moving casters, the refit over 100 frames) need a device with
//! ray queries: they run where the adapter has them (lavapipe does) and print
//! `skipped: no ray queries on <adapter>` where it does not (`regia_test_utils::hwrt`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use regia_render::hwrt::{
    RtShadowRequest, SharedTraceCasters, SunShadowReason, SunShadowVariant, TraceCasterInstance,
    TraceCasterMesh,
};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, RenderDevice,
    SharedFrameInfo, SharedPass, SharedPassDraw, SharedUpload, box_mesh, translate_mesh,
};

const W: u32 = 192;
const H: u32 = 144;
/// Direction the sun's light travels (not unit length).
const LIGHT: [f32; 3] = [0.5, -1.0, 0.25];
/// The proxy slab: centre and half extent (x, z), metres. Its shadow centre lands at (-4, 0, 2).
const SLAB: [f32; 3] = [-6.0, 4.0, 1.0];
const SLAB_HALF: f32 = 1.5;

/// What the test draw was told.
#[derive(Default)]
struct Calls {
    trace_on: AtomicUsize,
    trace_off: AtomicUsize,
    sun_traced: AtomicBool,
}

/// A shared draw that draws nothing and hands the trace a slab.
struct ProxyDraw {
    proxy: bool,
    takes_traced_sun: bool,
    built: bool,
    meshes: Vec<TraceCasterMesh>,
    instances: Vec<TraceCasterInstance>,
    calls: Arc<Calls>,
}

impl ProxyDraw {
    fn new(proxy: bool, takes_traced_sun: bool) -> (Self, Arc<Calls>) {
        let calls = Arc::new(Calls::default());
        let h = SLAB_HALF;
        let positions = vec![[-h, 0.0, -h], [h, 0.0, -h], [-h, 0.0, h], [h, 0.0, h]];
        let mesh = TraceCasterMesh {
            key: 7,
            generation: 1,
            positions: Arc::from(positions),
            indices: Arc::from(vec![0, 2, 1, 1, 2, 3]),
        };
        let transform = regia_core::glam::Mat4::from_translation(SLAB.into()).to_cols_array_2d();
        let draw = Self {
            proxy,
            takes_traced_sun,
            built: false,
            meshes: vec![mesh],
            instances: vec![TraceCasterInstance { mesh: 7, transform }],
            calls: Arc::clone(&calls),
        };
        (draw, calls)
    }
}

impl SharedPassDraw for ProxyDraw {
    fn prepare(&mut self, _info: &SharedFrameInfo, _upload: &mut SharedUpload<'_>) {}
    fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: SharedPass) {}
    fn set_trace_casters(&mut self, on: bool) {
        let counter = if on {
            &self.calls.trace_on
        } else {
            &self.calls.trace_off
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.built = on && self.proxy;
    }
    fn trace_casters(&self) -> Option<SharedTraceCasters<'_>> {
        self.built.then_some(SharedTraceCasters {
            meshes: &self.meshes,
            instances: &self.instances,
        })
    }
    fn takes_traced_sun(&self) -> bool {
        self.takes_traced_sun
    }
    fn set_engine_sun_traced(&mut self, traced: bool) {
        self.calls.sun_traced.store(traced, Ordering::Relaxed);
    }
}

fn scene(box_x: f32, masked_card: bool, blended: bool) -> LabScene {
    let (gv, gi) = box_mesh([40.0, 0.05, 40.0]);
    let (bv, bi) = box_mesh([1.0, 1.0, 1.0]);
    let mut meshes = vec![
        LabMesh {
            vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
            indices: gi,
            material: 0,
            ..LabMesh::unskinned()
        },
        LabMesh {
            vertices: translate_mesh(&bv, [0.0, 3.0, 0.0]),
            indices: bi.clone(),
            material: 1,
            model: regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(box_x, 0.0, 0.0))
                .to_cols_array_2d(),
            ..LabMesh::unskinned()
        },
    ];
    if masked_card {
        let (cv, ci) = box_mesh([1.0, 0.02, 1.0]);
        meshes.push(LabMesh {
            vertices: translate_mesh(&cv, [5.0, 3.0, -5.0]),
            indices: ci,
            material: 2,
            ..LabMesh::unskinned()
        });
    }
    if blended {
        meshes.push(LabMesh {
            vertices: translate_mesh(&bv, [6.0, 1.0, 4.0]),
            indices: bi,
            material: 3,
            ..LabMesh::unskinned()
        });
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 16.0, 10.0],
        target: [1.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new(LIGHT, [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.2, 0.3, 0.6, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.2, 0.5, 0.2, 1.0],
                alpha_mode: regia_render::ALPHA_MASK,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.2, 0.2, 0.5],
                alpha_mode: regia_render::ALPHA_BLEND,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

fn quality(request: RtShadowRequest) -> FrameQuality {
    let mut quality = FrameQuality::LAB;
    quality.sun_shadow_trace = request;
    quality
}

fn frame(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    request: RtShadowRequest,
) -> Vec<u8> {
    let frame = lab.as_frame_scene_with_quality(quality(request));
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

fn luma(pixels: &[u8], (x, y): (u32, u32)) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.25 * f32::from(pixels[i]) + 0.5 * f32::from(pixels[i + 1]) + 0.25 * f32::from(pixels[i + 2])
}

/// The pixel a world point lands on, with the frame's camera.
fn project(world: [f32; 3]) -> (u32, u32) {
    let lab = scene(0.0, false, false);
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        lab.eye.into(),
        lab.target.into(),
        regia_core::glam::Vec3::Y,
    );
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        lab.fov_y,
        W as f32 / H as f32,
        lab.near,
        lab.far,
    );
    let clip = proj * view * regia_core::glam::Vec3::from(world).extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - ndc.y * 0.5) * H as f32) as u32,
    )
}

/// Ground under the slab's shadow.
fn under_slab() -> (u32, u32) {
    let t = SLAB[1] / -LIGHT[1];
    project([SLAB[0] + LIGHT[0] * t, 0.0, SLAB[2] + LIGHT[2] * t])
}

/// Open ground away from every shadow.
fn open_ground() -> (u32, u32) {
    project([-7.0, 0.0, 5.0])
}

fn with_draw(proxy: bool, takes_traced_sun: bool) -> (FrameRenderer, Arc<Calls>) {
    let (draw, calls) = ProxyDraw::new(proxy, takes_traced_sun);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw)));
    (renderer, calls)
}

/// The proxy enters the compute reference's scene: the ground under the slab is in shadow, the
/// report counts the shared instance, and the draw was asked to build its casters.
#[test]
fn the_compute_reference_traces_the_shared_proxy() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let (mut renderer, calls) = with_draw(true, false);
    let lab = scene(0.0, false, false);
    let pixels = frame(&device, &mut renderer, &lab, RtShadowRequest::Compute);
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ComputeFallback);
    assert_eq!(report.shared_casters, 1);
    assert_eq!(report.set_casters, 0);
    assert_eq!(report.casters, 3, "the ground, the box and the slab");
    assert_eq!(
        report.triangles, 26,
        "two boxes of twelve triangles and the slab's two"
    );
    assert!(calls.trace_on.load(Ordering::Relaxed) >= 1);
    let (shadow, sun) = (luma(&pixels, under_slab()), luma(&pixels, open_ground()));
    assert!(
        shadow < sun * 0.6,
        "under the slab {shadow:.1}, open ground {sun:.1}"
    );

    // A steady frame rebuilds nothing.
    frame(&device, &mut renderer, &lab, RtShadowRequest::Compute);
    assert_eq!(renderer.sun_trace_report().scene_builds, 1);
}

/// Without a proxy the slab is nowhere in the scene, so its shadow is missing: the proxy alone made
/// the shadow of the test above.
#[test]
fn without_a_proxy_the_shared_casters_are_missing() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let (mut renderer, _) = with_draw(false, false);
    let pixels = frame(
        &device,
        &mut renderer,
        &scene(0.0, false, false),
        RtShadowRequest::Compute,
    );
    let report = renderer.sun_trace_report();
    assert_eq!(report.shared_casters, 0);
    assert_eq!(report.casters, 2);
    let (shadow, sun) = (luma(&pixels, under_slab()), luma(&pixels, open_ground()));
    assert!(
        shadow > sun * 0.9,
        "under the slab {shadow:.1}, open ground {sun:.1}"
    );
}

/// `on` without ray queries keeps the old fallback, the screen-space trace on the shadow map, also
/// with a proxy (the default device does not ask for ray queries, whatever the adapter offers).
#[test]
fn on_without_ray_queries_keeps_the_screen_fallback() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    assert!(
        !device
            .device
            .features()
            .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY)
    );
    for proxy in [true, false] {
        let (mut renderer, _) = with_draw(proxy, false);
        frame(
            &device,
            &mut renderer,
            &scene(0.0, false, false),
            RtShadowRequest::On,
        );
        let report = renderer.sun_trace_report();
        assert_eq!(
            report.selection.variant,
            SunShadowVariant::ScreenTrace,
            "proxy {proxy}"
        );
        assert_eq!(report.selection.reason, SunShadowReason::ScreenFallback);
        assert_eq!(
            report.sun_maps_skipped, 0,
            "the screen trace reads the shadow map"
        );
    }
}

/// The knob off tells the draw to drop its proxy, and the frame is the frame of a renderer that
/// never traced: cascades the trace skipped are drawn again.
#[test]
fn the_knob_off_releases_the_proxy_and_redraws_the_cascades() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let lab = scene(0.0, false, false);
    let (mut plain, _) = with_draw(true, true);
    let reference = frame(&device, &mut plain, &lab, RtShadowRequest::Off);

    let (mut renderer, calls) = with_draw(true, true);
    frame(&device, &mut renderer, &lab, RtShadowRequest::Compute);
    frame(&device, &mut renderer, &lab, RtShadowRequest::Compute);
    assert!(renderer.sun_trace_report().sun_maps_skipped >= 2);
    assert!(calls.sun_traced.load(Ordering::Relaxed));
    let back = frame(&device, &mut renderer, &lab, RtShadowRequest::Off);
    assert_eq!(
        calls.trace_off.load(Ordering::Relaxed),
        1,
        "told once that the trace stopped"
    );
    assert!(
        !calls.sun_traced.load(Ordering::Relaxed),
        "and that the cascades are back"
    );
    assert_eq!(back, reference, "the knob off is the default frame");
    frame(&device, &mut renderer, &lab, RtShadowRequest::Off);
    assert_eq!(
        calls.trace_off.load(Ordering::Relaxed),
        1,
        "an off frame asks nothing more"
    );
}

/// A traced frame stops drawing the sun cascades only while nothing reads them: a blended surface,
/// or a shared draw that cannot take the trace alone, keeps them, and the shadows stay right.
#[test]
fn unread_cascades_are_skipped_and_read_ones_kept() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let cases = [
        // (shared draw: Some(takes traced sun), blended surface, skipped)
        (None, false, true),
        (Some(true), false, true),
        (Some(false), false, false),
        (None, true, false),
    ];
    for (shared, blended, skipped) in cases {
        let mut renderer = FrameRenderer::new();
        if let Some(takes) = shared {
            renderer.set_shared_draw(Some(Box::new(ProxyDraw::new(true, takes).0)));
        }
        let lab = scene(0.0, false, blended);
        let pixels = frame(&device, &mut renderer, &lab, RtShadowRequest::Compute);
        let report = renderer.sun_trace_report();
        assert_eq!(
            report.sun_maps_skipped > 0,
            skipped,
            "{shared:?} blended {blended}"
        );
        let (shadow, sun) = (luma(&pixels, under_slab()), luma(&pixels, open_ground()));
        if shared.is_some() {
            assert!(
                shadow < sun * 0.6,
                "{shared:?}: under the slab {shadow:.1}, open {sun:.1}"
            );
        }
    }
}

/// On an adapter with ray queries: the shared proxy and the masked card enter the TLAS (the
/// instance count is the scene's), the frame reports `rt_shadows_ray_query`, and a box that moves
/// for 100 frames refits the top level each frame: no rebuild, the same instance count throughout.
#[test]
fn ray_queries_trace_shared_and_masked_casters_and_refit_moving_ones() {
    let Some(device) = regia_test_utils::hwrt::ray_query_device() else {
        return;
    };
    let (mut renderer, _) = with_draw(true, false);
    let pixels = frame(
        &device,
        &mut renderer,
        &scene(0.0, true, false),
        RtShadowRequest::On,
    );
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::RayQuery);
    assert_eq!(report.selection.reason.code(), "rt_shadows_ray_query");
    assert_eq!(report.failure, None);
    assert_eq!(report.shared_casters, 1);
    assert_eq!(report.masked_casters, 1);
    assert_eq!(report.skipped_masked, 0);
    assert_eq!(report.accel.instances, 4, "ground, box, masked card, slab");
    let (shadow, sun) = (luma(&pixels, under_slab()), luma(&pixels, open_ground()));
    assert!(
        shadow < sun * 0.6,
        "under the slab {shadow:.1}, open ground {sun:.1}"
    );
    let builds = report.scene_builds;

    for i in 1..=100 {
        let lab = scene(i as f32 * 0.01, true, false);
        frame(&device, &mut renderer, &lab, RtShadowRequest::On);
        let report = renderer.sun_trace_report();
        assert_eq!(report.accel.instances, 4, "frame {i}");
        assert!(report.accel.refit, "frame {i} refits");
        assert_eq!(report.accel.instances_moved, 1, "frame {i}: only the box moved");
        assert_eq!(report.accel.blas_built, 0);
    }
    let report = renderer.sun_trace_report();
    assert_eq!(
        report.scene_builds, builds,
        "no rebuild while only transforms change"
    );
    assert_eq!(report.tlas_refits, 100);
}
