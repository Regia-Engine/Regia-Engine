// SPDX-License-Identifier: MPL-2.0
//! Object-on-object sun shadows at RTS camera distances, against the path tracer (lane
//! csm-far).
//!
//! The look lane's Cycles sphere grid lost the shadow one 1 m sphere casts on its neighbour
//! 0.4 m away once the camera stood ~120 m off under a low sun. The CSM normal offset and
//! slope bias were specified in shadow texels, and the far cascades' texels are tens of
//! centimetres to metres, so the bias pushed the compare depth past the caster.
//!
//! Scene: a receiver sphere (r = 1 m) resting on a ground plane, a caster sphere of the same
//! size whose centre sits 2.4 m up-sun of the receiver's (a 0.4 m gap along the light) and
//! 0.5 m across that ray, away from the camera, so the edge of its shadow crosses the part
//! of the receiver's lit half the camera sees. The camera looks down 40° through a 45° lens at 2560×1440 from 30, 60, 120 and
//! 200 m; the sun stands 10°, 30° and 60° high. For every case:
//!
//! - **present**: the raster shadows at least 70 % of the receiver pixels the path tracer
//!   (direct light, the same sun disc) has in the caster's shadow;
//! - **umbra**: the raster's shadow on the receiver differs from the tracer's by at most
//!   a band of shadow texels around the true boundary (`umbra_tolerance`);
//! - **no acne**: on the receiver wherever n·l > 0.1, and on the ground where the tracer is
//!   lit within the raster filter's reach (not the penumbra of a real shadow edge), pixels
//!   keep at least 80 % of the raster's unshadowed sun in the raster frame with the caster's
//!   shadow off (the receiver's and the ground's self-shadow only).
//!
//! The raster's sun visibility is the HDR frame divided by the same frame with the caster's
//! shadow off (no ambient, no IBL: the sun is the only light).

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtCamera, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabVertex,
    PostSettings, RenderDevice, ShadowCast, ShadowFilter,
};

const W: u32 = 2560;
const H: u32 = 1440;
const FOV_Y_DEG: f32 = 45.0;
/// The tracer renders the centred `CROP`² window of the frame (same pixel grid: a narrower
/// pinhole on the same axis). The full 2560×1440 path buffer exceeds lavapipe's 128 MiB
/// storage binding; the pair sits at the frame centre.
const CROP: u32 = 720;
const PITCH_DEG: f32 = 40.0;
/// Camera azimuth from −Z toward +X (the sun's side), degrees.
const CAM_AZIMUTH_DEG: f32 = 20.0;
const RADIUS: f32 = 1.0;
/// Centre-to-centre distance along the light (gap 0.4 m).
const ALONG_LIGHT: f32 = 2.4;
/// Caster offset across the light (+Z, away from the camera).
const ACROSS_LIGHT: f32 = 0.5;
/// The raster's sun disc for PCSS (`SUN_ANGULAR_RADIUS_TAN` in `lab_forward.wgsl`).
const SUN_ANGULAR_RADIUS: f32 = 0.0175;

const RECEIVER_ALBEDO: [f32; 3] = [0.6, 0.3, 0.3];
const CASTER_ALBEDO: [f32; 3] = [0.3, 0.6, 0.3];
const GROUND_ALBEDO: [f32; 3] = [0.4, 0.4, 0.4];

fn vertex(position: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex {
        position,
        normal,
        uv: [0.0, 0.0],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

fn material(albedo: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [albedo[0], albedo[1], albedo[2], 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..LabMaterial::EMPTY
    }
}

/// One sun / distance case's statistics.
#[derive(Debug, Default)]
struct CaseStats {
    /// Interior receiver pixels lit enough to judge (n·l above the terminator band).
    domain: usize,
    /// … in the caster's shadow in the tracer.
    pt_shadow: usize,
    /// … in shadow in the raster.
    raster_shadow: usize,
    /// … in shadow in both.
    both: usize,
    /// Tracer-lit receiver pixels whose own shadow (caster off) darkens the raster by > 20 %.
    receiver_acne: usize,
    receiver_lit: usize,
    /// The same on the ground.
    ground_acne: usize,
    ground_lit: usize,
    /// Domain pixels on the tracer's shadow boundary (a 4-neighbour in the domain differs).
    boundary: usize,
}

fn near(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.02)
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

struct Rig {
    device: RenderDevice,
    tracer: PathTracer,
    sphere: (Vec<LabVertex>, Vec<u32>),
    ground: (Vec<LabVertex>, Vec<u32>),
    receiver_mat: LabMaterial,
    caster_mat: LabMaterial,
    ground_mat: LabMaterial,
}

impl Rig {
    fn new() -> Self {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let tracer = PathTracer::new(&device).expect("compile the path tracer");
        let s = 2000.0;
        let n = [0.0, 1.0, 0.0];
        let ground = (
            vec![
                vertex([-s, 0.0, -s], n),
                vertex([-s, 0.0, s], n),
                vertex([s, 0.0, s], n),
                vertex([s, 0.0, -s], n),
            ],
            vec![0, 1, 2, 0, 2, 3],
        );
        Self {
            device,
            tracer,
            sphere: regia_render::uv_sphere(RADIUS, 128, 64),
            ground,
            receiver_mat: material(RECEIVER_ALBEDO),
            caster_mat: material(CASTER_ALBEDO),
            ground_mat: material(GROUND_ALBEDO),
        }
    }

    fn scene<'a>(
        &'a self,
        distance: f32,
        sun_elevation_deg: f32,
        quality: FrameQuality,
        caster_shadow: ShadowCast,
        post: &'a PostSettings,
    ) -> FrameScene<'a> {
        let e = sun_elevation_deg.to_radians();
        let to_sun = Vec3::new(e.cos(), e.sin(), 0.0);
        let receiver = Vec3::new(0.0, RADIUS, 0.0);
        let caster = receiver + to_sun * ALONG_LIGHT + Vec3::new(0.0, 0.0, ACROSS_LIGHT);
        let (az, pitch) = (CAM_AZIMUTH_DEG.to_radians(), PITCH_DEG.to_radians());
        let back = Vec3::new(az.sin() * pitch.cos(), pitch.sin(), -az.cos() * pitch.cos());
        let eye = receiver + back * distance;
        let mesh =
            |key: u64, verts: &'a [LabVertex], idx: &'a [u32], mat: &'a LabMaterial, at: Vec3, shadow| {
                FrameMesh {
                    is_static: false,
                    mesh_key: key,
                    material_key: key,
                    object_key: key,
                    vertices: verts,
                    indices: idx,
                    material: mat,
                    model: Mat4::from_translation(at).to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow,
                    uv1: &[],
                    lod_dither: 0.0,
                }
            };
        FrameScene {
            width: W,
            height: H,
            eye: eye.to_array(),
            target: receiver.to_array(),
            fov_y: FOV_Y_DEG.to_radians(),
            near: 0.1,
            far: 12_000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 13.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: Some(post),
            fog: regia_render::FogSettings::default(),
            directional: DirectionalLightUniform::new((-to_sun).to_array(), [1.0, 1.0, 1.0], 20_000.0),
            local_lights: &[],
            quality,
            meshes: vec![
                mesh(
                    1,
                    &self.ground.0,
                    &self.ground.1,
                    &self.ground_mat,
                    Vec3::ZERO,
                    ShadowCast::All,
                ),
                mesh(
                    2,
                    &self.sphere.0,
                    &self.sphere.1,
                    &self.receiver_mat,
                    receiver,
                    ShadowCast::All,
                ),
                mesh(
                    3,
                    &self.sphere.0,
                    &self.sphere.1,
                    &self.caster_mat,
                    caster,
                    caster_shadow,
                ),
            ],
        }
    }

    fn raster(&self, scene: &FrameScene<'_>) -> Vec<[f32; 3]> {
        let mut renderer = FrameRenderer::new();
        renderer.request_hdr_capture();
        renderer
            .render_offscreen(&self.device, scene)
            .expect("forward frame");
        let hdr = renderer
            .take_hdr_capture(&self.device)
            .expect("hdr readback")
            .expect("the post stack ran, so the HDR copy exists");
        assert_eq!((hdr.width, hdr.height), (W, H));
        hdr.rgb
    }

    /// `edge_px`: the raster filter's reach (pixels) — self-shadow is judged only where the
    /// tracer is lit that far around, not in the penumbra of a real shadow edge.
    fn run(
        &self,
        distance: f32,
        sun_elevation_deg: f32,
        quality: FrameQuality,
        tag: &str,
        edge_px: usize,
    ) -> CaseStats {
        let post = PostSettings {
            enabled: true,
            bloom: false,
            auto_exposure: false,
            taa: false,
            ..PostSettings::default()
        };
        let lit = self.raster(&self.scene(distance, sun_elevation_deg, quality, ShadowCast::Off, &post));
        // The same frame with no sun shadows at all: self-shadow acne is judged against it, so
        // shading differences to the tracer at the terminator and silhouettes do not count.
        let mut unshadowed_scene = self.scene(distance, sun_elevation_deg, quality, ShadowCast::Off, &post);
        unshadowed_scene.directional_shadows = false;
        let unshadowed = self.raster(&unshadowed_scene);
        let shadowed = self.raster(&self.scene(distance, sun_elevation_deg, quality, ShadowCast::All, &post));
        let mut pt_scene = from_frame(
            &self.scene(distance, sun_elevation_deg, quality, ShadowCast::All, &post),
            &ExtractOptions {
                sky: Some(PtSky::None),
                sun_angular_radius: SUN_ANGULAR_RADIUS,
                ..ExtractOptions::default()
            },
        );
        let half_tan = (FOV_Y_DEG.to_radians() * 0.5).tan() * CROP as f32 / H as f32;
        pt_scene.camera = PtCamera::pinhole(
            pt_scene.camera.eye,
            pt_scene.camera.target,
            2.0 * half_tan.atan(),
            CROP,
            CROP,
        );
        let (x0, y0) = (((W - CROP) / 2) as usize, ((H - CROP) / 2) as usize);
        let crop = |img: &[[f32; 3]]| -> Vec<[f32; 3]> {
            (0..CROP as usize)
                .flat_map(|y| {
                    let row = (y0 + y) * W as usize + x0;
                    img[row..row + CROP as usize].iter().copied()
                })
                .collect()
        };
        let (lit, shadowed, unshadowed) = (crop(&lit), crop(&shadowed), crop(&unshadowed));
        let settings = PtSettings {
            spp: 1,
            max_bounces: 1,
            jitter: false,
            ..PtSettings::default()
        };
        let pt = self.tracer.render(&pt_scene, &settings).expect("trace");

        let (e_sin, e_cos) = sun_elevation_deg.to_radians().sin_cos();
        let (w, h) = (CROP as usize, CROP as usize);
        let is = |i: usize, albedo: [f32; 3]| pt.depth[i] > 0.0 && near(pt.albedo[i], albedo);
        let interior =
            |i: usize, albedo: [f32; 3]| [i, i - 1, i + 1, i - w, i + w].iter().all(|&j| is(j, albedo));
        let receiver_peak = (0..w * h)
            .filter(|&i| is(i, RECEIVER_ALBEDO))
            .map(|i| luma(lit[i]))
            .fold(0.0f32, f32::max);
        let mut st = CaseStats::default();
        let mut raster_mask = vec![false; w * h];
        let mut pt_mask = vec![false; w * h];
        let mut domain_mask = vec![false; w * h];
        let mut acne_mask = vec![false; w * h];
        // Tracer fully lit within `edge_px` (a separable min filter of the sun visibility).
        let lit_here: Vec<bool> = pt.shadow.iter().map(|&v| v > 0.99).collect();
        let mut lit_row = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let (a, b) = (x.saturating_sub(edge_px), (x + edge_px).min(w - 1));
                lit_row[y * w + x] = (a..=b).all(|xx| lit_here[y * w + xx]);
            }
        }
        let mut lit_around = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let (a, b) = (y.saturating_sub(edge_px), (y + edge_px).min(h - 1));
                lit_around[y * w + x] = (a..=b).all(|yy| lit_row[yy * w + x]);
            }
        }
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let i = y * w + x;
                let pt_direct = luma(pt.direct[i]);
                let pt_lit = lit_around[i];
                if interior(i, GROUND_ALBEDO) && pt_lit && pt_direct > 0.0 {
                    st.ground_lit += 1;
                    if luma(lit[i]) < 0.8 * luma(unshadowed[i]) {
                        st.ground_acne += 1;
                        acne_mask[i] = true;
                    }
                }
                if !interior(i, RECEIVER_ALBEDO) {
                    continue;
                }
                let l_off = luma(lit[i]);
                // Without the caster nothing can shadow a point of the convex receiver that
                // faces the sun, so every pixel with n·l above 0.1 must be lit (below that the
                // cascades cap the receiver-plane slope at tan θ = 12, and the facet
                // terminator darkens both renderers differently).
                let nn = pt.normal[i];
                if nn[0] * e_cos + nn[1] * e_sin > 0.1 {
                    st.receiver_lit += 1;
                    if l_off < 0.8 * luma(unshadowed[i]) {
                        st.receiver_acne += 1;
                        acne_mask[i] = true;
                        if regia_core::brand::env_var_os("CSM_FAR_DEBUG").is_some() {
                            let l = [e_cos, e_sin, 0.0];
                            eprintln!(
                                "  acne px ({x},{y}) n {nn:?} n.l {:.3} off/unshadowed {:.3} pt.shadow {:.2}",
                                nn[0] * l[0] + nn[1] * l[1] + nn[2] * l[2],
                                l_off / luma(unshadowed[i]).max(1e-6),
                                pt.shadow[i]
                            );
                        }
                    }
                }
                // Judge the caster's shadow away from the terminator band (n·l > ~0.1).
                if l_off < 0.1 * receiver_peak {
                    continue;
                }
                st.domain += 1;
                domain_mask[i] = true;
                let vis = luma(shadowed[i]) / l_off.max(1e-6);
                let in_pt = pt.shadow[i] < 0.5;
                let in_r = vis < 0.5;
                pt_mask[i] = in_pt;
                raster_mask[i] = in_r;
                st.pt_shadow += usize::from(in_pt);
                st.raster_shadow += usize::from(in_r);
                st.both += usize::from(in_pt && in_r);
            }
        }
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let i = y * w + x;
                if domain_mask[i]
                    && [i - 1, i + 1, i - w, i + w]
                        .iter()
                        .any(|&j| domain_mask[j] && pt_mask[j] != pt_mask[i])
                {
                    st.boundary += 1;
                }
            }
        }
        if let Some(dir) = regia_core::brand::env_var_os("CSM_FAR_OUT") {
            write_evidence(
                std::path::Path::new(&dir),
                tag,
                (&shadowed, &pt.direct),
                &Masks {
                    domain: &domain_mask,
                    raster: &raster_mask,
                    pt: &pt_mask,
                    acne: &acne_mask,
                },
            );
        }
        st
    }
}

/// Crop around the receiver: raster | tracer | masks (white both, red raster only, cyan
/// tracer only, magenta self-shadow acne), each scaled so the receiver spans ~240 px.
/// Per-pixel classifications of one case (`write_evidence`).
struct Masks<'a> {
    domain: &'a [bool],
    raster: &'a [bool],
    pt: &'a [bool],
    acne: &'a [bool],
}

fn write_evidence(
    dir: &std::path::Path,
    tag: &str,
    (raster, pt): (&[[f32; 3]], &[[f32; 3]]),
    masks: &Masks<'_>,
) {
    let Masks {
        domain,
        raster: raster_mask,
        pt: pt_mask,
        acne: acne_mask,
    } = *masks;
    let (w, h) = (CROP as usize, CROP as usize);
    let (mut x0, mut x1, mut y0, mut y1) = (w, 0, h, 0);
    for y in 0..h {
        for x in 0..w {
            if domain[y * w + x] {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let pad = (x1 - x0).max(y1 - y0);
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    let half = pad * 3 / 2;
    let side = 2 * half;
    let scale = (480 / side.max(1)).max(1);
    let out_side = side * scale;
    let peak = raster.iter().map(|c| luma(*c)).fold(0.0f32, f32::max).max(1e-6);
    let tone =
        |c: [f32; 3]| -> [u8; 3] { c.map(|v| ((v / peak).clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8) };
    let mut img = vec![0u8; out_side * 3 * out_side * 3];
    for oy in 0..out_side {
        for ox in 0..out_side {
            let sx = (cx + ox / scale) as isize - half as isize;
            let sy = (cy + oy / scale) as isize - half as isize;
            if sx < 0 || sy < 0 || sx >= w as isize || sy >= h as isize {
                continue;
            }
            let i = sy as usize * w + sx as usize;
            let mask = if acne_mask[i] {
                [255, 0, 255]
            } else if !domain[i] {
                [24, 24, 24]
            } else {
                match (raster_mask[i], pt_mask[i]) {
                    (true, true) => [255, 255, 255],
                    (true, false) => [255, 40, 40],
                    (false, true) => [40, 220, 255],
                    (false, false) => [90, 90, 90],
                }
            };
            for (panel, px) in [tone(raster[i]), tone(pt[i]), mask].iter().enumerate() {
                let o = (oy * out_side * 3 + panel * out_side + ox) * 3;
                img[o..o + 3].copy_from_slice(px);
            }
        }
    }
    let _ = std::fs::create_dir_all(dir);
    let Ok(file) = std::fs::File::create(dir.join(format!("{tag}.png"))) else {
        return;
    };
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        (out_side * 3) as u32,
        out_side as u32,
    );
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = enc.write_header() {
        let _ = writer.write_image_data(&img);
    }
}

fn desktop_high_shadows() -> FrameQuality {
    FrameQuality {
        csm_cascade_count: 4,
        csm_resolution: 2048,
        shadow_filter: ShadowFilter::PcssLite,
        paged_sun_shadows: true,
        specular_aa: false,
        ..FrameQuality::LAB
    }
}

/// Raster-vs-tracer disagreement allowed on the receiver: a band `band_px` wide on the
/// tracer's shadow boundary (`boundary` pixels long) plus 5 % of the tracer's shadow.
fn umbra_tolerance(pt_shadow: usize, boundary: usize, band_px: f32) -> usize {
    (boundary as f32 * band_px.max(1.0) + 0.05 * pt_shadow as f32) as usize
}

#[test]
fn sphere_pair_shadows_match_the_tracer_at_rts_distances() {
    let rig = Rig::new();
    let quality = desktop_high_shadows();
    let mut failures = Vec::new();
    eprintln!("dist  sun  domain  pt  raster  both  miss  extra  bound  band  tol  rec-acne  gnd-acne");
    for &distance in &[30.0f32, 60.0, 120.0, 200.0] {
        for &sun in &[10.0f32, 30.0, 60.0] {
            let tag = format!("d{distance:03.0}_sun{sun:02.0}");
            // One shadow texel of the cascade that holds the receiver, in pixels at the
            // receiver: the cascade's texel (world) over the pixel footprint at `distance`.
            let px_per_m = H as f32 / (2.0 * (FOV_Y_DEG.to_radians() * 0.5).tan() * distance);
            let texel_m = expected_texel_m(distance);
            // The PCSS filter's minimum radius is 1.5 texels; one pixel more for the pixel itself.
            let edge_px = (1.5 * texel_m * px_per_m).ceil() as usize + 1;
            let st = rig.run(distance, sun, quality, &tag, edge_px);
            let band = 0.75 * texel_m * px_per_m;
            let CaseStats {
                domain,
                pt_shadow,
                raster_shadow,
                both,
                ..
            } = st;
            let miss = pt_shadow - both;
            let extra = raster_shadow - both;
            let tol = umbra_tolerance(pt_shadow, st.boundary, band);
            eprintln!(
                "{distance:4.0} {sun:4.0} {domain:7} {pt_shadow:5} {raster_shadow:7} {both:5} {miss:5} {extra:6} {:5} {band:5.2} {tol:4} {:4}/{:<6} {:4}/{}",
                st.boundary, st.receiver_acne, st.receiver_lit, st.ground_acne, st.ground_lit
            );
            if pt_shadow * 10 < domain {
                failures.push(format!(
                    "{tag}: test geometry: the tracer shadows only {pt_shadow} of {domain} receiver px"
                ));
                continue;
            }
            if (both as f32) < 0.7 * pt_shadow as f32 {
                failures.push(format!(
                    "{tag}: shadow missing: raster shadows {both} of the tracer's {pt_shadow} px"
                ));
            }
            if miss + extra > tol {
                failures.push(format!(
                    "{tag}: umbra off: {miss} missed + {extra} extra px > {tol}"
                ));
            }
            if st.receiver_acne * 200 > st.receiver_lit.max(1) {
                failures.push(format!(
                    "{tag}: receiver acne: {} of {} lit px",
                    st.receiver_acne, st.receiver_lit
                ));
            }
            if st.ground_acne * 1000 > st.ground_lit.max(1) {
                failures.push(format!(
                    "{tag}: ground acne: {} of {} lit px",
                    st.ground_acne, st.ground_lit
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Upper bound of the sun cascade texel (metres) at the receiver for this test's camera:
/// the end of the cascade that holds `distance`, times the tight slice-sphere factor, over
/// the 2048² map. Used only to size the umbra tolerance band.
fn expected_texel_m(distance: f32) -> f32 {
    let bounds = regia_render::csm_boundaries(0.1, 4000.0, 4);
    let aspect = W as f32 / H as f32;
    let k = (FOV_Y_DEG.to_radians() * 0.5).tan() * (1.0 + aspect * aspect).sqrt();
    let end = bounds
        .iter()
        .skip(1)
        .copied()
        .find(|&b| b >= distance + 2.0 * ALONG_LIGHT)
        .unwrap_or(4000.0);
    2.0 * end * k.max(1.0) * 1.05 / 2048.0
}
