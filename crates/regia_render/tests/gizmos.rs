// SPDX-License-Identifier: MPL-2.0
//! Gizmo pass end to end on lavapipe.
//!
//! - `every_primitive_matches_the_lavapipe_golden`: one frame with every gizmo primitive and line
//!   style (lines, strips, gradients, arrows, axes, circles, spheres, arcs, grids, AABBs, frusta,
//!   capsules, cylinders, cones, tori, rounded boxes, cuboids, text, dashed/dotted, miter and
//!   round joints, perspective width, a retained-style transformed batch). Judged by eye; the
//!   golden is `docs/evidence/gizmos/gizmos_all.png` (bless with `REGIA_BLESS=1` on lavapipe
//!   only; `REGIA_PREVIEW=1` writes `<temp>/gizmo-preview` and skips the compare).
//! - `scene_depth_hides_lines_unless_on_top`: a box hides a depth-tested line, not an on-top one.
//! - `gizmos_draw_at_every_quality_preset`: the pass runs after post/TAAU/MSAA/scaled rendering
//!   at every `RenderProfilePreset`, and inside a split-screen viewport.
//! - `atlas_label_quads_sample_their_page`: a debug-label glyph quad draws its atlas page's coverage
//!   (a full page draws, an empty one does not) and is hidden behind geometry (ADR-7254).
//!
//! SSIM is the global luma form (Wang, Bovik, Sheikh, Simoncelli, IEEE TIP 2004).

use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_core::math::{Aabb, Color, Isometry, Mat4, Quat, UVec2, Vec2, Vec3};
use regia_render::gizmos::{
    DefaultGizmoConfigGroup, GizmoBatch, GizmoBuffer, GizmoConfig, GizmoFrame, GizmoLineConfig,
    GizmoLineJoint, GizmoLineStyle,
};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget,
    LabMaterial, LabVertex, PostSettings, RenderDevice, RenderProfilePreset, RenderSettings, box_mesh,
    translate_mesh,
};

const W: u32 = 640;
const H: u32 = 360;
const SSIM_MIN: f64 = 0.98;

type Buffer = GizmoBuffer<DefaultGizmoConfigGroup>;

fn evidence_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/gizmos")
}

fn quad(half: f32, y: f32) -> Vec<LabVertex> {
    [[-half, -half], [half, -half], [half, half], [-half, half]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, y, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

const QUAD_INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

struct Meshes {
    ground: Vec<LabVertex>,
    block: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
}

impl Meshes {
    fn new(block_at: [f32; 3], block_half: [f32; 3]) -> Self {
        let (vertices, indices) = box_mesh(block_half);
        Self {
            ground: quad(12.0, 0.0),
            block: (translate_mesh(&vertices, block_at), indices),
            material: LabMaterial {
                base_color: [0.35, 0.36, 0.38, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
        }
    }

    fn frame_meshes(&self) -> Vec<FrameMesh<'_>> {
        fn mesh<'m>(
            key: u64,
            vertices: &'m [LabVertex],
            indices: &'m [u32],
            material: &'m LabMaterial,
        ) -> FrameMesh<'m> {
            FrameMesh {
                is_static: false,
                mesh_key: key,
                material_key: 1,
                object_key: key,
                vertices,
                indices,
                material,
                model: Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        }
        vec![
            mesh(1, &self.ground, &QUAD_INDICES, &self.material),
            mesh(2, &self.block.0, &self.block.1, &self.material),
        ]
    }
}

#[allow(clippy::too_many_arguments)]
fn scene<'a>(
    meshes: Vec<FrameMesh<'a>>,
    width: u32,
    height: u32,
    eye: [f32; 3],
    target: [f32; 3],
    fov_y: f32,
    quality: FrameQuality,
    post: Option<&'a PostSettings>,
) -> FrameScene<'a> {
    FrameScene {
        width,
        height,
        eye,
        target,
        fov_y,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.4,
        double_sided: true,
        post,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.4, -1.0, -0.3], [1.0, 1.0, 1.0], 1.2),
        local_lights: &[],
        quality,
        meshes,
    }
}

fn config(width: f32) -> GizmoConfig {
    GizmoConfig {
        line: GizmoLineConfig {
            width,
            ..GizmoLineConfig::default()
        },
        ..GizmoConfig::default()
    }
}

fn batch(buffer: &Buffer, config: &GizmoConfig) -> GizmoBatch {
    GizmoBatch::from_view(buffer.buffer(), Mat4::IDENTITY, config)
}

/// Every primitive, laid out in rows on the ground in front of the camera.
fn every_primitive() -> GizmoFrame {
    let mut batches = Vec::new();
    let yellow = Color::rgb(1.0, 0.85, 0.1);
    let cyan = Color::rgb(0.1, 0.9, 1.0);
    let orange = Color::rgb(1.0, 0.45, 0.05);
    let pink = Color::rgb(1.0, 0.3, 0.7);
    let white = Color::WHITE;
    let ground = Isometry::from_rotation(Quat::from_rotation_x(-FRAC_PI_2));

    // Ground grid: a slight negative bias keeps it from z-fighting the ground plane.
    let mut grid = Buffer::new();
    grid.grid(
        Isometry::new(ground.rotation, Vec3::new(0.0, 0.0, -1.0)),
        UVec2::new(18, 12),
        Vec2::splat(1.0),
        Color::rgb(0.45, 0.45, 0.5),
    )
    .outer_edges();
    batches.push(batch(
        &grid,
        &GizmoConfig {
            depth_bias: -0.02,
            ..config(1.0)
        },
    ));

    let mut b = Buffer::new();
    let up = |x: f32, z: f32, y: f32| Vec3::new(x, y, z);
    // Back row (z = -5): round shapes.
    b.sphere(up(-6.0, -5.0, 1.0), 0.9, cyan).resolution(32);
    b.capsule(up(-3.0, -5.0, 1.2), 0.5, 0.6, yellow).resolution(8);
    b.cylinder(up(0.0, -5.0, 0.8), 0.7, 0.8, orange).resolution(12);
    b.cone(up(3.0, -5.0, 0.9), 0.8, 1.8, pink).resolution(12);
    b.torus(
        Isometry::new(Quat::from_rotation_x(0.6), up(6.0, -5.0, 1.0)),
        0.25,
        0.8,
        cyan,
    )
    .major_resolution(16)
    .minor_resolution(4);
    // Middle row (z = -2): boxes and arcs.
    b.aabb_3d(
        Aabb::new(Vec3::new(-0.7, 0.0, -0.7), Vec3::new(0.7, 1.4, 0.7)),
        Mat4::from_translation(up(-6.0, -2.0, 0.0)),
        yellow,
    );
    b.rounded_cuboid(
        Isometry::new(Quat::from_rotation_y(0.5), up(-3.0, -2.0, 0.7)),
        Vec3::new(1.4, 1.2, 1.0),
        orange,
    )
    .edge_radius(0.25);
    b.cuboid(
        Isometry::new(Quat::from_rotation_y(FRAC_PI_4), up(0.0, -2.0, 0.6)),
        Vec3::splat(0.6),
        white,
    );
    b.arc_3d(4.0, 0.9, up(3.0, -2.0, 0.05), pink).resolution(24);
    b.short_arc_3d_between(
        up(3.0, -2.0, 0.05),
        up(3.9, -2.0, 0.05),
        up(3.0, -2.0, 0.95),
        yellow,
    );
    b.circle(Isometry::new(ground.rotation, up(6.0, -2.0, 0.02)), 0.9, cyan);
    b.ellipse(up(6.0, -2.0, 1.0), Vec2::new(0.8, 0.4), cyan);
    b.cross(up(6.0, -2.0, 1.0), 0.3, white);
    // Front row (z = 1): arrows, axes, gradients, frustum, tetrahedron, plane.
    b.arrow(up(-7.0, 1.0, 0.2), up(-5.0, 1.0, 1.5), orange)
        .with_double_end();
    b.axes(Mat4::from_translation(up(-3.0, 1.0, 0.05)), 1.2);
    b.linestrip_gradient((0..=12).map(|i| {
        let t = i as f32 / 12.0;
        (
            up(-1.0 + 2.0 * t, 1.0, 0.3 + (t * 9.0).sin() * 0.25 + 0.3),
            Color::rgb(1.0 - t, t, 0.5),
        )
    }));
    b.line_gradient(up(-1.0, 1.0, 0.1), up(1.0, 1.0, 0.1), Color::RED, Color::BLUE);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(0.7, 1.5, 0.3, 1.6);
    let view =
        regia_core::glam::camera::rh::view::look_at_mat4(up(2.4, 1.0, 0.8), up(4.0, 0.0, 0.8), Vec3::Y);
    b.frustum(proj * view, yellow);
    b.tetrahedron(
        up(6.0, 1.0, 0.0),
        [
            Vec3::new(0.0, 1.3, 0.0),
            Vec3::new(-0.7, 0.0, -0.5),
            Vec3::new(0.7, 0.0, -0.5),
            Vec3::new(0.0, 0.0, 0.7),
        ],
        pink,
    );
    b.rect(
        Isometry::new(ground.rotation, up(-6.0, 3.5, 0.02)),
        Vec2::new(1.6, 1.0),
        white,
    );
    b.rounded_rect(
        Isometry::new(ground.rotation, up(-3.5, 3.5, 0.02)),
        Vec2::new(2.0, 1.0),
        cyan,
    )
    .corner_radius(0.35);
    b.plane(up(6.0, 3.6, 0.02), Vec3::Y, Color::rgb(0.6, 0.6, 1.0))
        .cell_count(UVec2::new(2, 2))
        .spacing(Vec2::splat(0.6));
    // The text is drawn into the gizmos_all golden: it keeps the old name until the golden is
    // re-blessed (ADR-1421).
    b.text(up(0.0, -6.5, 2.6), "FERRITE GIZMOS 0123", 0.7, Vec2::ZERO, white); // rename-keep
    batches.push(batch(&b, &config(2.0)));

    // Thick strips with joints: miter (left) and round (right).
    for (x, joints, color) in [
        (-1.5, GizmoLineJoint::Miter, yellow),
        (1.5, GizmoLineJoint::Round(6), cyan),
    ] {
        let mut s = Buffer::new();
        s.linestrip(
            [(-0.8, 0.0), (-0.4, 0.8), (0.0, 0.1), (0.4, 0.9), (0.8, 0.2)]
                .map(|(dx, dy)| Vec3::new(x + dx, 0.3 + dy, 3.8)),
            color,
        );
        batches.push(batch(
            &s,
            &GizmoConfig {
                line: GizmoLineConfig {
                    width: 9.0,
                    joints,
                    ..GizmoLineConfig::default()
                },
                ..GizmoConfig::default()
            },
        ));
    }
    // Dashed and dotted lines.
    for (z, style) in [
        (
            2.4,
            GizmoLineStyle::Dashed {
                gap_scale: 3.0,
                line_scale: 5.0,
            },
        ),
        (2.9, GizmoLineStyle::Dotted),
    ] {
        let mut s = Buffer::new();
        s.line(Vec3::new(-1.6, 0.05, z), Vec3::new(1.6, 0.05, z), white);
        batches.push(batch(
            &s,
            &GizmoConfig {
                line: GizmoLineConfig {
                    width: 4.0,
                    style,
                    ..GizmoLineConfig::default()
                },
                ..GizmoConfig::default()
            },
        ));
    }
    // Perspective width: the same 0.05 m-wide rail from near to far.
    let mut p = Buffer::new();
    p.line(Vec3::new(8.0, 0.05, 4.5), Vec3::new(8.0, 0.05, -6.0), orange);
    batches.push(batch(
        &p,
        &GizmoConfig {
            line: GizmoLineConfig {
                width: 40.0,
                perspective: true,
                ..GizmoLineConfig::default()
            },
            ..GizmoConfig::default()
        },
    ));
    // A retained-style batch: local geometry placed by a transform.
    let mut r = Buffer::new();
    r.sphere(Vec3::ZERO, 0.5, pink).resolution(16);
    r.arrow(Vec3::ZERO, Vec3::Y, pink);
    batches.push(GizmoBatch::from_view(
        r.buffer(),
        Mat4::from_scale_rotation_translation(
            Vec3::splat(1.2),
            Quat::from_rotation_z(0.4),
            Vec3::new(-8.0, 0.8, 3.2),
        ),
        &config(3.0),
    ));
    GizmoFrame {
        batches,
        ..GizmoFrame::default()
    }
}

fn render_frame(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    scene: &FrameScene<'_>,
    gizmos: Option<GizmoFrame>,
) -> Vec<u8> {
    renderer.set_gizmos(gizmos.map(Arc::new));
    renderer
        .render_offscreen(device, scene)
        .unwrap_or_else(|err| panic!("render: {err}"))
        .pixels
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("evidence dir");
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png pixels");
    writer.finish().expect("png finish");
}

fn read_png(path: &Path) -> Vec<u8> {
    let file = std::fs::File::open(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    buf
}

fn luma(px: &[u8; 4]) -> f64 {
    0.2126 * f64::from(px[0]) + 0.7152 * f64::from(px[1]) + 0.0722 * f64::from(px[2])
}

/// Global luma SSIM.
fn luma_ssim(a: &[u8], b: &[u8]) -> f64 {
    let la: Vec<f64> = a.as_chunks::<4>().0.iter().map(luma).collect();
    let lb: Vec<f64> = b.as_chunks::<4>().0.iter().map(luma).collect();
    let n = la.len() as f64;
    let (ma, mb) = (la.iter().sum::<f64>() / n, lb.iter().sum::<f64>() / n);
    let (mut va, mut vb, mut cov) = (0.0, 0.0, 0.0);
    for (x, y) in la.iter().zip(&lb) {
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
        cov += (x - ma) * (y - mb);
    }
    let (va, vb, cov) = (va / n, vb / n, cov / n);
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

fn changed(a: &[u8], b: &[u8], threshold: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(x, y)| x.iter().zip(*y).take(3).any(|(p, q)| p.abs_diff(*q) > threshold))
        .count()
}

fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

const EYE: [f32; 3] = [0.0, 7.5, 11.0];
const TARGET: [f32; 3] = [0.0, 0.0, -1.0];

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn every_primitive_matches_the_lavapipe_golden() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    let meshes = Meshes::new([0.0, -5.0, 0.0], [0.1, 0.1, 0.1]);
    let mut renderer = FrameRenderer::new();
    let scene = scene(
        meshes.frame_meshes(),
        W,
        H,
        EYE,
        TARGET,
        0.85,
        FrameQuality::LAB,
        None,
    );
    let without = render_frame(&device, &mut renderer, &scene, None);
    let with = render_frame(&device, &mut renderer, &scene, Some(every_primitive()));
    let moved = changed(&with, &without, 12);
    eprintln!("gizmo pixels: {moved}");
    assert!(moved > 20_000, "the gizmos barely drew ({moved} changed pixels)");
    // The text row is white strokes near the top of the frame.
    let bright_top = with
        .as_chunks::<4>()
        .0
        .iter()
        .take((W * H / 3) as usize)
        .filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200)
        .count();
    assert!(
        bright_top > 200,
        "stroke text missing ({bright_top} white pixels in the top third)"
    );

    let preview = std::env::temp_dir().join("gizmo-preview");
    write_preview(&preview.join("gizmos_all.png"), W, H, &with);
    write_preview(&preview.join("gizmos_none.png"), W, H, &without);
    let path = evidence_dir().join("gizmos_all.png");
    if regia_core::brand::env_var("REGIA_BLESS").as_deref() == Ok("1") {
        write_png(&path, W, H, &with);
        return;
    }
    if regia_core::brand::env_var("REGIA_PREVIEW").as_deref() == Ok("1") {
        return;
    }
    if device.adapter.get_info().device_type != wgpu::DeviceType::Cpu {
        eprintln!("hardware GPU: golden is blessed on lavapipe; compare skipped");
        return;
    }
    let golden = read_png(&path);
    assert_eq!(golden.len(), with.len(), "golden size");
    let score = luma_ssim(&with, &golden);
    eprintln!("gizmos_all SSIM {score:.4}");
    assert!(score >= SSIM_MIN, "gizmos_all SSIM {score:.4} below {SSIM_MIN}");
}

#[test]
fn scene_depth_hides_lines_unless_on_top() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    let (w, h) = (128, 128);
    // A 2 m block at the origin, camera 5 m away; two lines 2 m behind it.
    let meshes = Meshes::new([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
    let all = meshes.frame_meshes();
    let scene = scene(
        vec![all[1]],
        w,
        h,
        [0.0, 0.0, 5.0],
        [0.0, 0.0, 0.0],
        0.8,
        FrameQuality::LAB,
        None,
    );
    let mut hidden = Buffer::new();
    hidden.line(Vec3::new(-3.0, 0.3, -2.0), Vec3::new(3.0, 0.3, -2.0), Color::RED);
    let mut on_top = Buffer::new();
    on_top.line(
        Vec3::new(-3.0, -0.3, -2.0),
        Vec3::new(3.0, -0.3, -2.0),
        Color::GREEN,
    );
    let frame = GizmoFrame {
        batches: vec![
            batch(&hidden, &config(5.0)),
            batch(
                &on_top,
                &GizmoConfig {
                    on_top: true,
                    ..config(5.0)
                },
            ),
        ],
        ..GizmoFrame::default()
    };
    let mut renderer = FrameRenderer::new();
    let pixels = render_frame(&device, &mut renderer, &scene, Some(frame));
    write_preview(
        &std::env::temp_dir().join("gizmo-preview/occlusion.png"),
        w,
        h,
        &pixels,
    );
    // Rows of the two lines: y_ndc = ±0.3 / 7 / tan(0.4).
    let row = |y: f32| ((1.0 - y / 7.0 / 0.4f32.tan()) * 0.5 * h as f32) as u32;
    let (red_row, green_row) = (row(0.3), row(-0.3));
    let centre = w / 2;
    let behind_block = pixel(&pixels, w, centre, red_row);
    assert!(
        !(behind_block[0] > 200 && behind_block[1] < 80),
        "the depth-tested line shows through the block: {behind_block:?}"
    );
    let beside_block = pixel(&pixels, w, w - 12, red_row);
    assert!(
        beside_block[0] > 200 && beside_block[1] < 80,
        "the depth-tested line is missing beside the block: {beside_block:?}"
    );
    let over_block = pixel(&pixels, w, centre, green_row);
    assert!(
        over_block[1] > 200 && over_block[0] < 80,
        "the on-top line is hidden by the block: {over_block:?}"
    );
}

#[test]
fn gizmos_draw_at_every_quality_preset() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    let (w, h) = (320u32, 180u32);
    let meshes = Meshes::new([0.0, 0.5, -2.0], [0.5, 0.5, 0.5]);
    for preset in [
        RenderProfilePreset::MobileLow,
        RenderProfilePreset::MobileHigh,
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopUltra,
        RenderProfilePreset::Deck,
    ] {
        let settings = RenderSettings::from_profile(preset);
        let quality = settings.quality();
        let post = settings.resolve_post();
        let scene = scene(
            meshes.frame_meshes(),
            w,
            h,
            EYE,
            TARGET,
            0.85,
            quality,
            Some(&post),
        );
        let without = render_frame(&device, &mut FrameRenderer::new(), &scene, None);
        let with = render_frame(
            &device,
            &mut FrameRenderer::new(),
            &scene,
            Some(every_primitive()),
        );
        write_preview(
            &std::env::temp_dir().join(format!("gizmo-preview/preset_{preset:?}.png")),
            w,
            h,
            &with,
        );
        let moved = changed(&with, &without, 12);
        eprintln!(
            "{preset:?}: msaa {} scale {}‰ taau {} → {moved} gizmo pixels",
            quality.msaa_samples, quality.render_scale_permille, quality.taau
        );
        assert!(
            moved > 4_000,
            "{preset:?}: gizmos barely drew ({moved} changed pixels)"
        );
    }

    // No preset turns MSAA on today; the multisampled scene-depth variant still has to work.
    for msaa in [4u32, 2] {
        let quality = FrameQuality {
            msaa_samples: msaa,
            ..FrameQuality::LAB
        };
        let scene = scene(meshes.frame_meshes(), w, h, EYE, TARGET, 0.85, quality, None);
        let without = render_frame(&device, &mut FrameRenderer::new(), &scene, None);
        let with = render_frame(
            &device,
            &mut FrameRenderer::new(),
            &scene,
            Some(every_primitive()),
        );
        write_preview(
            &std::env::temp_dir().join(format!("gizmo-preview/msaa{msaa}.png")),
            w,
            h,
            &with,
        );
        let moved = changed(&with, &without, 12);
        eprintln!("LAB + MSAA x{msaa} → {moved} gizmo pixels");
        assert!(
            moved > 4_000,
            "MSAA x{msaa}: gizmos barely drew ({moved} changed pixels)"
        );
    }

    // Split screen: a gizmo frame drawn into the right half only.
    let settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    let post = settings.resolve_post();
    let half = scene(
        meshes.frame_meshes(),
        w / 2,
        h,
        EYE,
        TARGET,
        0.85,
        settings.quality(),
        Some(&post),
    );
    let mut renderer = FrameRenderer::new();
    renderer.set_gizmos(Some(Arc::new(every_primitive())));
    let target = renderer.offscreen_target(&device, w, h, false);
    renderer
        .render_into(
            &device,
            &half,
            FrameTarget::rect(
                &target.view,
                wgpu::TextureFormat::Rgba8Unorm,
                w,
                h,
                [w / 2, 0, w / 2, h],
                false,
            ),
        )
        .expect("split-screen view");
    let pixels = renderer.read_offscreen(&device, target).expect("readback").pixels;
    write_preview(
        &std::env::temp_dir().join("gizmo-preview/split.png"),
        w,
        h,
        &pixels,
    );
    let bright = |x0: u32, x1: u32| {
        (0..h)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(&pixels, w, x, y)[..3].iter().any(|c| *c > 180))
            .count()
    };
    let (left, right) = (bright(0, w / 2), bright(w / 2, w));
    eprintln!("split screen bright pixels: left {left}, right {right}");
    assert!(right > 1_000, "gizmos missing from the right-hand view ({right})");
    assert!(
        left < right / 10,
        "gizmos leaked outside the viewport ({left} vs {right})"
    );
}

/// Lines the shader cannot place draw nothing stray (ADR-869): a segment wholly behind the
/// camera, one seen exactly end-on (both ends on one pixel: `normalize` of a zero screen vector
/// was NaN), and a strip that doubles back on itself with miter joints (the miter divided by
/// zero; it is now limited to 4 line widths). All draw on top, so depth cannot hide a stray
/// triangle; pixels away from the strip's row must match the frame without gizmos.
#[test]
fn degenerate_lines_draw_nothing_stray() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    let (w, h) = (128, 128);
    let meshes = Meshes::new([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
    let all = meshes.frame_meshes();
    let scene = scene(
        vec![all[1]],
        w,
        h,
        [0.0, 0.0, 5.0],
        [0.0, 0.0, 0.0],
        0.8,
        FrameQuality::LAB,
        None,
    );
    let on_top = |width: f32, joints: GizmoLineJoint| GizmoConfig {
        on_top: true,
        line: GizmoLineConfig {
            width,
            joints,
            ..GizmoLineConfig::default()
        },
        ..GizmoConfig::default()
    };
    let baseline = render_frame(&device, &mut FrameRenderer::new(), &scene, None);

    let mut stray = Buffer::new();
    // Behind the eye (z > 5): never visible.
    stray.line(Vec3::new(-3.0, 0.2, 8.0), Vec3::new(3.0, -0.2, 9.0), Color::RED);
    // On the view axis: both ends project to the center pixel.
    stray.line(Vec3::new(0.0, 0.0, 4.0), Vec3::new(0.0, 0.0, -10.0), Color::GREEN);
    let frame = GizmoFrame {
        batches: vec![batch(&stray, &on_top(8.0, GizmoLineJoint::None))],
        ..GizmoFrame::default()
    };
    let with_stray = render_frame(&device, &mut FrameRenderer::new(), &scene, Some(frame));
    assert_eq!(
        changed(&baseline, &with_stray, 8),
        0,
        "a line behind the camera or seen end-on drew pixels"
    );

    // A hairpin: out to x = 1 and straight back. Its joint at x = 1 has a zero miter angle.
    let mut hairpin = Buffer::new();
    hairpin.linestrip(
        [
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(-1.0, 0.0, 0.0),
        ],
        Color::rgb(1.0, 0.85, 0.1),
    );
    let frame = GizmoFrame {
        batches: vec![batch(&hairpin, &on_top(6.0, GizmoLineJoint::Miter))],
        ..GizmoFrame::default()
    };
    let with_hairpin = render_frame(&device, &mut FrameRenderer::new(), &scene, Some(frame));
    assert!(
        changed(&baseline, &with_hairpin, 8) > 0,
        "the hairpin strip itself must draw"
    );
    // The strip lies on the center row; 6 px wide plus a 12 px miter spike along the row. Rows
    // more than 24 px above or below it must be untouched.
    let row_bytes = (w * 4) as usize;
    let center = (h / 2) as usize;
    for (name, rows) in [("above", 0..center - 24), ("below", center + 24..h as usize)] {
        let a = &baseline[rows.start * row_bytes..rows.end * row_bytes];
        let b = &with_hairpin[rows.start * row_bytes..rows.end * row_bytes];
        assert_eq!(
            changed(a, b, 8),
            0,
            "the hairpin's miter reached the rows {name} it"
        );
    }
}

/// A preview image for a person to look at: written best-effort under the OS temp directory. A
/// preview is not asserted on, so a folder another user owns (a shared `/tmp`) or a read-only temp
/// volume prints a note instead of failing a correct render (audit-v035-flaky M6).
fn write_preview(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    let written = (|| -> Result<(), Box<dyn std::error::Error>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
        writer.finish()?;
        Ok(())
    })();
    if let Err(err) = written {
        eprintln!("preview {} not written: {err}", path.display());
    }
}

/// One on-top label glyph quad, 1 m on a side at the origin, facing +Z, over the page's whole
/// texel rectangle.
fn label_frame(fill: u8, on_top: bool) -> GizmoFrame {
    use regia_render::gizmos::{GizmoLabelBatch, GizmoLabelVertex};
    let corner = |x: f32, y: f32, u: f32, v: f32| GizmoLabelVertex {
        position: [x, y, 0.0],
        uv: [u, v],
        color: [1.0, 0.05, 0.02, 1.0],
    };
    let (bl, br, tr, tl) = (
        corner(-0.5, -0.5, 0.0, 1.0),
        corner(0.5, -0.5, 1.0, 1.0),
        corner(0.5, 0.5, 1.0, 0.0),
        corner(-0.5, 0.5, 0.0, 0.0),
    );
    GizmoFrame {
        labels: vec![GizmoLabelBatch {
            on_top,
            page: (
                0,
                regia_render::DebugLabelPage {
                    size: 16,
                    texels: vec![fill; 256].into(),
                    generation: u64::from(fill) + 1,
                },
            ),
            vertices: vec![bl, br, tr, bl, tr, tl].into(),
        }],
        ..GizmoFrame::default()
    }
}

#[test]
fn atlas_label_quads_sample_their_page() {
    // ADR-7254: a label glyph quad's colour is its page's coverage times its colour. A full page
    // draws the quad, an empty page nothing, against the same frame without gizmos.
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    let (w, h) = (128, 128);
    let meshes = Meshes::new([0.0, 0.0, -3.0], [0.5, 0.5, 0.5]);
    let all = meshes.frame_meshes();
    let scene = scene(
        vec![all[1]],
        w,
        h,
        [0.0, 0.0, 5.0],
        [0.0, 0.0, 0.0],
        0.8,
        FrameQuality::LAB,
        None,
    );
    let baseline = render_frame(&device, &mut FrameRenderer::new(), &scene, None);
    let full = render_frame(
        &device,
        &mut FrameRenderer::new(),
        &scene,
        Some(label_frame(255, true)),
    );
    let empty = render_frame(
        &device,
        &mut FrameRenderer::new(),
        &scene,
        Some(label_frame(0, true)),
    );
    let centre = pixel(&full, w, w / 2, h / 2);
    eprintln!(
        "atlas label: centre {centre:?}, changed full {}, empty {}",
        changed(&baseline, &full, 8),
        changed(&baseline, &empty, 8)
    );
    assert!(
        centre[0] > 200 && centre[1] < 120,
        "a full page draws the label colour: {centre:?}"
    );
    assert!(
        changed(&baseline, &full, 8) > 100,
        "the full page's quad covers pixels"
    );
    assert_eq!(changed(&baseline, &empty, 8), 0, "an empty page draws nothing");
    // A depth-tested quad behind the block (z = -3, the block's front at -2.5) is hidden at the centre.
    let mut hidden = label_frame(255, false);
    for batch in &mut hidden.labels {
        let moved: Vec<_> = batch
            .vertices
            .iter()
            .map(|v| regia_render::gizmos::GizmoLabelVertex {
                position: [v.position[0] * 0.5, v.position[1] * 0.5, -4.0],
                ..*v
            })
            .collect();
        batch.vertices = moved.into();
    }
    let behind = render_frame(&device, &mut FrameRenderer::new(), &scene, Some(hidden));
    assert_eq!(
        pixel(&behind, w, w / 2, h / 2),
        pixel(&baseline, w, w / 2, h / 2),
        "a depth-tested label behind the block is hidden"
    );
}
