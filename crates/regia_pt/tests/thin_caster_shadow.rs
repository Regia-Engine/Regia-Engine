// SPDX-License-Identifier: MPL-2.0
//! A thin, close caster's sun shadow at hero distance against the path tracer (lane
//! shadow-detail).
//!
//! DO27's X-12 side hero shot: a 9 m camera, the hull's fin standing a few tens of centimetres
//! off the cargo box, a 35° sun lighting the box at n·l ≈ 0.45. The path tracer and Cycles
//! print the fin's crisp triangle; the raster's sun PCSS (Desktop High) used a 1° sun and a
//! 1.5-texel filter floor, which rounded it.
//!
//! Scene: a wall (the receiver, the x = 0 plane facing +X), a triangular plate 0.5 m in
//! front of it and parallel to it (double-sided, the caster), the sun 35° high and 55° round
//! from the wall's normal, a camera 9 m out at 1536×1152 / 2 with a 50 mm lens (30.2°
//! vertical). Desktop High's sun cascades and PCSS against a direct-light trace with the real
//! sun disc (0.27°). On the wall pixels the two shadow masks (visibility < 0.5) must overlap
//! with IoU ≥ `MIN_IOU`, and the raster's penumbra (0.1 < visibility < 0.9) must stay thin: at most
//! `MAX_PENUMBRA_PER_EDGE_PX` pixels per pixel of the tracer's shadow outline.
//!
//! The raster's visibility is the frame divided by the same frame without sun shadows (no
//! IBL, no ambient: the sun is the only light). The bounds apply to the cascades alone;
//! with Desktop High's screen-space contact shadows on (ADR-128) a looser guard applies.

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabMaterial, LabVertex, PostSettings,
    RenderDevice, RenderSettings, ShadowCast,
};

const W: u32 = 768;
const H: u32 = 576;
/// 50 mm on a 36 mm-wide 4:3 frame: 2·atan(13.5 / 50).
const FOV_Y_DEG: f32 = 30.23;
const SUN_ELEVATION_DEG: f32 = 35.0;
/// Sun azimuth from the wall's normal (+X) toward +Z, degrees.
const SUN_AZIMUTH_DEG: f32 = 55.0;
/// Caster plate distance in front of the wall, metres.
const GAP: f32 = 0.5;
/// The real sun (`regia_render::shadow::SUN_PCSS_ANGULAR_RADIUS_TAN`).
const SUN_ANGULAR_RADIUS: f32 = 0.004_675;
/// Raster penumbra pixels allowed per pixel of the tracer's shadow outline. At 9 m one pixel
/// is ~8 mm; the cascade's ~1.1 cm texels land 2.2× stretched on a wall at n·l = 0.47, so
/// the filter alone spans a few pixels. Lavapipe, 2026-10-09 (cascades alone): 1.57 with the
/// real sun and a 1-texel floor; 2.23 with the old 1° sun and 1.5-texel floor (rejected).
const MAX_PENUMBRA_PER_EDGE_PX: f32 = 2.0;
/// Shadow-outline IoU against the tracer (cascades alone). Lavapipe, 2026-10-09: 0.928
/// (0.926 with the old disc and floor: the outline was in place, only blurred).
const MIN_IOU: f32 = 0.91;
/// The same with Desktop High's contact shadows on (see the test).
const MIN_IOU_WITH_CONTACT: f32 = 0.87;

const WALL_ALBEDO: [f32; 3] = [0.5, 0.5, 0.5];
const FIN_ALBEDO: [f32; 3] = [0.3, 0.6, 0.3];

fn vertex(position: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex {
        position,
        normal,
        uv: [0.0, 0.0],
        tangent: [0.0, 0.0, 1.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

fn material(albedo: [f32; 3], double_sided: bool) -> LabMaterial {
    LabMaterial {
        base_color: [albedo[0], albedo[1], albedo[2], 1.0],
        roughness: 1.0,
        metallic: 0.0,
        double_sided,
        ..LabMaterial::EMPTY
    }
}

fn to_sun() -> Vec3 {
    let (e, a) = (SUN_ELEVATION_DEG.to_radians(), SUN_AZIMUTH_DEG.to_radians());
    Vec3::new(e.cos() * a.cos(), e.sin(), e.cos() * a.sin())
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

struct Scene {
    wall: (Vec<LabVertex>, Vec<u32>),
    fin: (Vec<LabVertex>, Vec<u32>),
    wall_mat: LabMaterial,
    fin_mat: LabMaterial,
    post: PostSettings,
}

impl Scene {
    fn new() -> Self {
        let n = [1.0, 0.0, 0.0];
        let wall = (
            vec![
                vertex([0.0, 0.0, -4.0], n),
                vertex([0.0, 0.0, 3.0], n),
                vertex([0.0, 3.5, 3.0], n),
                vertex([0.0, 3.5, -4.0], n),
            ],
            vec![0, 2, 1, 0, 3, 2],
        );
        // A fin-like triangle: a long sloped leading edge up to a point, a short trailing edge.
        // Its shadow lands ~0.6 m down and 0.7 m toward −Z on the wall, clear of the plate as
        // the camera (below and on the −Z side) sees it.
        let fin = (
            vec![
                vertex([GAP, 1.3, -0.4], n),
                vertex([GAP, 2.4, 0.5], n),
                vertex([GAP, 2.0, 0.8], n),
            ],
            vec![0, 1, 2],
        );
        Self {
            wall,
            fin,
            wall_mat: material(WALL_ALBEDO, false),
            fin_mat: material(FIN_ALBEDO, true),
            post: PostSettings {
                enabled: true,
                bloom: false,
                auto_exposure: false,
                taa: false,
                ..PostSettings::default()
            },
        }
    }

    /// `contact`: Desktop High's screen-space contact shadows (ADR-128) on top of the cascades.
    fn frame(&self, sun_shadows: bool, contact: bool) -> FrameScene<'_> {
        let mut quality = RenderSettings::default().quality();
        quality.horizontal_plus = false;
        quality.render_scale_permille = 1000;
        quality.contact_shadows = contact;
        FrameScene {
            width: W,
            height: H,
            eye: [8.0, 0.6, -3.6],
            target: [0.0, 1.4, -0.3],
            fov_y: FOV_Y_DEG.to_radians(),
            near: 0.02,
            far: 1000.0,
            depth_prepass: true,
            directional_shadows: sun_shadows,
            ibl: false,
            exposure_ev: 13.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(&self.post),
            fog: regia_render::FogSettings::default(),
            directional: DirectionalLightUniform::new((-to_sun()).to_array(), [1.0; 3], 20_000.0),
            local_lights: &[],
            quality,
            meshes: vec![
                mesh(1, &self.wall, &self.wall_mat),
                mesh(2, &self.fin, &self.fin_mat),
            ],
        }
    }
}

fn mesh<'a>(
    key: u64,
    (verts, idx): &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
) -> FrameMesh<'a> {
    FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key: key,
        object_key: key,
        vertices: verts,
        indices: idx,
        material,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }
}

fn raster(device: &RenderDevice, scene: &FrameScene<'_>) -> Vec<[f32; 3]> {
    let mut renderer = FrameRenderer::new();
    // A few frames: the paged cascades settle, as in a still hero shot.
    for i in 0..3 {
        if i == 2 {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device, scene).expect("forward frame");
    }
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists");
    assert_eq!((hdr.width, hdr.height), (W, H));
    hdr.rgb
}

/// One raster variant against the trace: (IoU, penumbra px per outline px, tracer shadow px).
fn measure(
    shadowed: &[[f32; 3]],
    unshadowed: &[[f32; 3]],
    pt: &regia_pt::PtImage,
    tag: &str,
) -> (f32, f32, usize) {
    let (w, h) = (W as usize, H as usize);
    let wall = |i: usize| {
        pt.depth[i] > 0.0
            && pt.albedo[i]
                .iter()
                .zip(WALL_ALBEDO)
                .all(|(a, b)| (a - b).abs() < 0.02)
    };
    let (mut raster_n, mut pt_n, mut both, mut either, mut penumbra, mut outline) = (0, 0, 0, 0, 0, 0);
    let mut masks = vec![[0u8; 3]; w * h];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            if ![i, i - 1, i + 1, i - w, i + w].iter().all(|&j| wall(j)) {
                continue;
            }
            let vis = luma(shadowed[i]) / luma(unshadowed[i]).max(1e-6);
            let in_r = vis < 0.5;
            let in_pt = pt.shadow[i] < 0.5;
            raster_n += usize::from(in_r);
            pt_n += usize::from(in_pt);
            both += usize::from(in_r && in_pt);
            either += usize::from(in_r || in_pt);
            penumbra += usize::from(vis > 0.1 && vis < 0.9);
            if [i - 1, i + 1, i - w, i + w]
                .iter()
                .any(|&j| wall(j) && (pt.shadow[j] < 0.5) != in_pt)
            {
                outline += 1;
            }
            masks[i] = match (in_r, in_pt) {
                (true, true) => [255, 255, 255],
                (true, false) => [255, 60, 60],
                (false, true) => [60, 200, 255],
                (false, false) => [70, 70, 70],
            };
        }
    }
    let iou = both as f32 / either.max(1) as f32;
    let penumbra_per_edge = penumbra as f32 / outline.max(1) as f32;
    eprintln!(
        "thin caster ({tag}): raster {raster_n} px, tracer {pt_n} px, IoU {iou:.3}; penumbra {penumbra} px \
         over a {outline} px outline ({penumbra_per_edge:.2} px per outline px)"
    );
    if let Some(dir) = regia_core::brand::env_var_os("THIN_CASTER_OUT") {
        let dir = std::path::Path::new(&dir);
        std::fs::create_dir_all(dir).expect("evidence dir");
        // Binary PPM: white both, red raster only, cyan tracer only, grey wall lit in both.
        let mut ppm = format!("P6 {W} {H} 255\n").into_bytes();
        ppm.extend(masks.iter().flatten());
        std::fs::write(dir.join(format!("thin_caster_masks_{tag}.ppm")), ppm).expect("write masks");
    }
    (iou, penumbra_per_edge, pt_n)
}

#[test]
fn a_thin_close_caster_casts_the_tracers_shadow_at_hero_distance() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let tracer = PathTracer::new(&device).expect("compile the path tracer");
    let scene = Scene::new();
    let pt_scene = from_frame(
        &scene.frame(true, false),
        &ExtractOptions {
            sky: Some(PtSky::None),
            sun_angular_radius: SUN_ANGULAR_RADIUS,
            ..ExtractOptions::default()
        },
    );
    let pt = tracer
        .render(
            &pt_scene,
            &PtSettings {
                spp: 16,
                max_bounces: 1,
                jitter: false,
                ..PtSettings::default()
            },
        )
        .expect("trace");

    // The sun cascades alone: the outline this lane is about.
    let unshadowed = raster(&device, &scene.frame(false, false));
    let csm = raster(&device, &scene.frame(true, false));
    let (iou, penumbra_per_edge, pt_n) = measure(&csm, &unshadowed, &pt, "csm");
    assert!(
        pt_n > 2000,
        "the fin's shadow must land on the visible wall ({pt_n} px)"
    );
    assert!(
        iou >= MIN_IOU,
        "shadow outline IoU vs the path tracer {iou:.3} < {MIN_IOU}"
    );
    assert!(
        penumbra_per_edge <= MAX_PENUMBRA_PER_EDGE_PX,
        "raster penumbra {penumbra_per_edge:.2} px per outline px > {MAX_PENUMBRA_PER_EDGE_PX}"
    );

    // Desktop High as shipped, contact shadows on: the screen-space march passes behind the
    // plate's on-screen image and darkens a band along the outline (IoU 0.892, penumbra 2.51 px
    // per outline px on lavapipe, 2026-10-09). A regression guard, not the cascades' bound.
    let full = raster(&device, &scene.frame(true, true));
    let (iou, _, _) = measure(&full, &unshadowed, &pt, "contact");
    assert!(
        iou >= MIN_IOU_WITH_CONTACT,
        "with contact shadows: IoU {iou:.3} < {MIN_IOU_WITH_CONTACT}"
    );
}
