// SPDX-License-Identifier: MPL-2.0
//! Two-eye frames (spec 58 § 8.12-13, ADR-1918): an eye is a texture view with its own matrices
//! and state. Runs on any headless adapter (lavapipe is the reference). No golden image is
//! involved: every check compares frames of this test with each other.

use std::sync::Mutex;

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    DirectionalLightUniform, EyeCamera, EyeFrame, EyeHiddenArea, FrameQuality, FrameRenderer, FrameScene,
    FrameTarget, LabMaterial, LabMesh, LabScene, LabVertex, RenderDevice, STEREO_EYE_VIEWS,
    TextureViewSettings, hidden_area_clip_vertices, read_rgba8,
};

static SERIAL_GPU: Mutex<()> = Mutex::new(());

const W: u32 = 96;
const H: u32 = 64;
const NEAR: f32 = 0.1;
const FAR: f32 = 200.0;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(center: [f32; 3], half: f32) -> LabMesh {
    let [x, y, z] = center;
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x - half, y - half, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + half, y - half, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + half, y + half, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - half, y + half, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

fn lab(meshes: Vec<LabMesh>) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 0.0, -1.0],
        fov_y: 1.2,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 20_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.8, 0.8, 0.8, 1.0],
            ..LabMaterial::default()
        }],
        meshes,
    }
}

/// A near and a far quad, straight ahead of the origin.
fn depth_pair() -> LabScene {
    lab(vec![quad([0.0, 0.0, -1.0], 0.1), quad([0.0, 0.0, -8.0], 0.8)])
}

/// An off-axis frustum in the frame's standard depth.
fn off_axis(left: f32, right: f32, down: f32, up: f32) -> Mat4 {
    let (w, h) = (right - left, up - down);
    let m22 = FAR / (NEAR - FAR);
    Mat4::from_cols(
        Vec4::new(2.0 / w, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 2.0 / h, 0.0, 0.0),
        Vec4::new((right + left) / w, (up + down) / h, m22, -1.0),
        Vec4::new(0.0, 0.0, NEAR * m22, 0.0),
    )
}

/// The eye at `x` on the X axis looking down -Z.
fn eye_at(x: f32, projection: Mat4) -> EyeCamera {
    let view = Mat4::from_translation(Vec3::new(-x, 0.0, 0.0));
    EyeCamera {
        view,
        projection,
        cull: projection * view,
    }
}

struct EyeImage {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

fn eye_image(device: &RenderDevice) -> EyeImage {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test eye"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    EyeImage { texture, view }
}

/// A view's reduced quality, so an eye and a texture view of the same scene compare.
fn reduced() -> FrameQuality {
    let mut quality = FrameQuality::LAB;
    TextureViewSettings::default().apply(&mut quality, None);
    quality
}

fn render_eye(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    eye: usize,
    camera: EyeCamera,
    scene: FrameScene<'_>,
    area: Option<EyeHiddenArea<'_>>,
) -> Vec<u8> {
    let image = eye_image(device);
    renderer
        .render_eye(
            device,
            EyeFrame {
                eye,
                camera,
                scene,
                target: FrameTarget::full(&image.view, FORMAT, W, H),
                hidden_area: area,
                depth_target: None,
            },
        )
        .expect("eye frame");
    read_rgba8(&device.device, &device.queue, &image.texture, W, H).expect("readback")
}

/// The column of the brightest-quad center in a row band: the mean x of the pixels brighter than
/// the background.
fn lit_center(pixels: &[u8], threshold: u8) -> f32 {
    let (mut sum, mut count) = (0.0, 0.0);
    for y in 0..H {
        for x in 0..W {
            if pixels[((y * W + x) * 4) as usize] > threshold {
                sum += x as f32;
                count += 1.0;
            }
        }
    }
    assert!(count > 0.0, "nothing above the threshold");
    sum / count
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn an_eye_with_the_look_at_matrices_matches_the_texture_view_of_the_same_camera() {
    let _gpu = serial();
    let device = device();
    let lab = depth_pair();
    let mut renderer = FrameRenderer::new();
    let settings = TextureViewSettings {
        size: [W, H],
        fxaa: false,
        ..TextureViewSettings::default()
    };
    let scene = lab.as_frame_scene_with_quality(reduced());
    renderer
        .render_texture_view(&device, 7, scene.clone(), &settings)
        .expect("texture view");
    let expected = renderer
        .read_texture_target(&device, 7)
        .expect("view pixels")
        .pixels;
    let aspect = W as f32 / H as f32;
    let projection = regia_core::glam::camera::rh::proj::directx::perspective(lab.fov_y, aspect, NEAR, FAR);
    let camera = eye_at(0.0, projection);
    let got = render_eye(&mut renderer, &device, 0, camera, scene, None);
    assert_eq!(got, expected, "an eye is a texture view with explicit matrices");
}

#[test]
fn eyes_see_parallax_that_shrinks_with_distance_and_equal_eyes_match() {
    let _gpu = serial();
    let device = device();
    let lab_near = lab(vec![quad([0.0, 0.0, -1.0], 0.1)]);
    let lab_far = lab(vec![quad([0.0, 0.0, -8.0], 0.8)]);
    let mut renderer = FrameRenderer::new();
    let projection = off_axis(-1.0, 1.0, -0.7, 0.7);
    let shift = |renderer: &mut FrameRenderer, lab: &LabScene, ipd: f32| {
        let scene = || lab.as_frame_scene_with_quality(reduced());
        let left = render_eye(
            renderer,
            &device,
            0,
            eye_at(-ipd * 0.5, projection),
            scene(),
            None,
        );
        let right = render_eye(renderer, &device, 1, eye_at(ipd * 0.5, projection), scene(), None);
        (lit_center(&left, 40) - lit_center(&right, 40), left, right)
    };
    let (near_shift, ..) = shift(&mut renderer, &lab_near, 0.1);
    let (far_shift, ..) = shift(&mut renderer, &lab_far, 0.1);
    // ipd * focal / depth, focal = 48 px for tangent 1 over half a 96-px image.
    assert!((near_shift - 0.1 * 48.0 / 1.0).abs() < 1.0, "near {near_shift}");
    assert!((far_shift - 0.1 * 48.0 / 8.0).abs() < 1.0, "far {far_shift}");
    assert!(near_shift > far_shift, "{near_shift} {far_shift}");
    let (zero, left, right) = shift(&mut renderer, &lab_near, 0.0);
    assert_eq!(zero, 0.0);
    assert_eq!(left, right, "IPD 0 gives identical eyes");
}

#[test]
fn an_asymmetric_frustum_moves_the_image_as_a_lens_does() {
    let _gpu = serial();
    let device = device();
    let lab = lab(vec![quad([0.0, 0.0, -2.0], 0.2)]);
    let mut renderer = FrameRenderer::new();
    let scene = || lab.as_frame_scene_with_quality(reduced());
    let centered = render_eye(
        &mut renderer,
        &device,
        0,
        eye_at(0.0, off_axis(-1.0, 1.0, -0.7, 0.7)),
        scene(),
        None,
    );
    let shifted = render_eye(
        &mut renderer,
        &device,
        0,
        eye_at(0.0, off_axis(-1.5, 0.5, -0.7, 0.7)),
        scene(),
        None,
    );
    let moved = lit_center(&shifted, 40) - lit_center(&centered, 40);
    // Half an image width of tangent shift: a quarter of the image.
    assert!((moved - 24.0).abs() < 1.0, "{moved}");
}

#[test]
fn the_hidden_area_is_masked_in_depth_and_the_rest_of_the_image_is_untouched() {
    let _gpu = serial();
    let device = device();
    let lab = lab(vec![quad([0.0, 0.0, -2.0], 2.0)]);
    let mut renderer = FrameRenderer::new();
    let projection = off_axis(-1.0, 1.0, -0.7, 0.7);
    let camera = eye_at(0.0, projection);
    // The left half of the view plane.
    let vertices = [[-5.0, -5.0], [0.0, -5.0], [0.0, 5.0], [-5.0, 5.0]];
    let clip = hidden_area_clip_vertices(&vertices, &[0, 1, 2, 0, 2, 3], &projection);
    let scene = || lab.as_frame_scene_with_quality(reduced());
    let whole = render_eye(&mut renderer, &device, 0, camera, scene(), None);
    let area = EyeHiddenArea {
        generation: 1,
        clip_vertices: &clip,
    };
    let masked = render_eye(&mut renderer, &device, 0, camera, scene(), Some(area));
    let depth = renderer.read_eye_depth(&device, 0, W, H).expect("eye depth");
    let at = |x: u32, y: u32| ((y * W + x) * 4) as usize;
    let depth_at = |x: u32, y: u32| depth[(y * W + x) as usize];
    assert_eq!(depth_at(10, 30), 0.0, "masked pixels hold the near plane");
    assert!(depth_at(80, 30) > 0.0, "the scene keeps the rest");
    assert_eq!(
        whole[at(80, 30)..at(80, 30) + 4],
        masked[at(80, 30)..at(80, 30) + 4]
    );
    assert!(
        masked[at(10, 30)] < whole[at(10, 30)],
        "no surface shades in the mask: {} vs {}",
        masked[at(10, 30)],
        whole[at(10, 30)]
    );
    // The mesh stays uploaded: the same generation with no vertices would rebuild nothing.
    assert_eq!(renderer.texture_view_ids(), [STEREO_EYE_VIEWS[0]]);
}

#[test]
fn eye_depth_reaches_the_layer_image_with_the_scene_depth() {
    let _gpu = serial();
    let device = device();
    let lab = depth_pair();
    let mut renderer = FrameRenderer::new();
    let image = eye_image(&device);
    let depth = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test eye depth"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let report = renderer
        .render_eye(
            &device,
            EyeFrame {
                eye: 1,
                camera: eye_at(0.0, off_axis(-1.0, 1.0, -0.7, 0.7)),
                scene: lab.as_frame_scene_with_quality(reduced()),
                target: FrameTarget::full(&image.view, FORMAT, W, H),
                hidden_area: None,
                depth_target: Some(&depth),
            },
        )
        .expect("eye");
    assert!(report.depth_copied);
    assert!(report.drawn > 0);
    let wrong = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("wrong size depth"),
        size: wgpu::Extent3d {
            width: 8,
            height: 8,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let report = renderer
        .render_eye(
            &device,
            EyeFrame {
                eye: 1,
                camera: eye_at(0.0, off_axis(-1.0, 1.0, -0.7, 0.7)),
                scene: lab.as_frame_scene_with_quality(reduced()),
                target: FrameTarget::full(&image.view, FORMAT, W, H),
                hidden_area: None,
                depth_target: Some(&wrong),
            },
        )
        .expect("eye");
    assert!(
        !report.depth_copied,
        "a mismatched image is skipped, not an error"
    );
}

#[test]
fn each_eye_keeps_its_own_history_and_the_main_view_is_untouched() {
    let _gpu = serial();
    let device = device();
    let lab = depth_pair();
    let mut quality = FrameQuality::LAB;
    quality.taau = true;
    let projection = off_axis(-1.0, 1.0, -0.7, 0.7);
    let run = |with_right_eye: bool| {
        let mut renderer = FrameRenderer::new();
        let main_before = renderer
            .render_offscreen(&device, &lab.as_frame_scene())
            .expect("main")
            .pixels;
        let mut left = Vec::new();
        for _ in 0..4 {
            renderer.begin_stereo_frame();
            left = render_eye(
                &mut renderer,
                &device,
                0,
                eye_at(-0.03, projection),
                lab.as_frame_scene_with_quality(quality),
                None,
            );
            if with_right_eye {
                render_eye(
                    &mut renderer,
                    &device,
                    1,
                    eye_at(0.03, projection),
                    lab.as_frame_scene_with_quality(quality),
                    None,
                );
            }
            renderer.end_stereo_frame();
        }
        let main_after = renderer
            .render_offscreen(&device, &lab.as_frame_scene())
            .expect("main")
            .pixels;
        assert_eq!(main_before, main_after, "the eyes leave the main view alone");
        left
    };
    assert_eq!(
        run(true),
        run(false),
        "the right eye never reaches the left eye's history"
    );
}

#[test]
fn a_union_cull_draws_what_only_the_other_eye_sees() {
    let _gpu = serial();
    let device = device();
    // Right of the left eye's frustum at this depth, inside the right eye's.
    let lab = lab(vec![quad([0.0, 0.0, -8.0], 1.0), quad([1.3, 0.0, -1.0], 0.05)]);
    let mut renderer = FrameRenderer::new();
    let narrow = off_axis(-0.2, 1.0, -0.7, 0.7);
    let draws = |renderer: &mut FrameRenderer, camera: EyeCamera| {
        let image = eye_image(&device);
        renderer
            .render_eye(
                &device,
                EyeFrame {
                    eye: 0,
                    camera,
                    scene: lab.as_frame_scene_with_quality(reduced()),
                    target: FrameTarget::full(&image.view, FORMAT, W, H),
                    hidden_area: None,
                    depth_target: None,
                },
            )
            .expect("eye")
            .drawn
    };
    let own = eye_at(0.0, narrow);
    assert_eq!(
        draws(&mut renderer, own),
        1,
        "its own frustum drops the side quad"
    );
    let union = EyeCamera {
        cull: off_axis(-0.2, 1.5, -0.7, 0.7) * own.view,
        ..own
    };
    assert_eq!(draws(&mut renderer, union), 2, "the union keeps it");
}

#[test]
fn an_eye_index_out_of_range_is_an_error_and_dropping_the_eyes_forgets_them() {
    let _gpu = serial();
    let device = device();
    let lab = depth_pair();
    let mut renderer = FrameRenderer::new();
    let image = eye_image(&device);
    let camera = eye_at(0.0, off_axis(-1.0, 1.0, -0.7, 0.7));
    let frame = |eye| EyeFrame {
        eye,
        camera,
        scene: lab.as_frame_scene_with_quality(reduced()),
        target: FrameTarget::full(&image.view, FORMAT, W, H),
        hidden_area: None,
        depth_target: None,
    };
    assert!(renderer.render_eye(&device, frame(2)).is_err());
    renderer.render_eye(&device, frame(0)).expect("eye");
    assert_eq!(renderer.texture_view_ids().len(), 1);
    assert!(
        renderer.read_eye_depth(&device, 1, W, H).is_err(),
        "eye 1 never rendered"
    );
    renderer.drop_stereo_views();
    assert!(renderer.texture_view_ids().is_empty());
}
