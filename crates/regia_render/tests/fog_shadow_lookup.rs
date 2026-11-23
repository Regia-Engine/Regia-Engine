// SPDX-License-Identifier: MPL-2.0
//! The froxel fog's sun-shadow lookup (ADR-3122): air under an opaque slab reads no sun, air in
//! the open keeps it, lit ground does not self-shadow, and the scatter pass samples only air in
//! front of a surface.
//!
//! The CPU tests run the lookup's mirror (`regia_render::FogSunCascade::sample`,
//! `fog_sample_depth`) against an analytic shadow map. The GPU test renders the spec 27 fog room
//! (a closed black room with a 1 m slit in its roof, height fog, a 60° sun) on lavapipe and checks
//! the shader through the same rule: pixels whose rays never come near the shaft must stay dark.
//! The room is an interior volume, so the sky does not light its fog and the sun is the only
//! light: any light in those pixels is sun that leaked through the roof. No golden.

use regia_core::glam::camera::rh::proj::directx::{orthographic, perspective};
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    DirectionalLightUniform, FogCascadeState, FogSunCascade, FogSunShadowProbe, FogSunShadowRule,
    FrameQuality, FrameRenderer, GiMode, InteriorVolume, LabMaterial, LabMesh, LabScene, LabVertex,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, VolumetricFogSettings,
    fog_sample_depth, fog_sample_weight,
};

// --- An analytic sun cascade over a roof slab ----------------------------------------------------

/// Height of the roof slab's top face (metres).
const ROOF_Y: f32 = 6.0;

/// Light travelling down at `elevation_deg`, toward +X.
fn sun_travel(elevation_deg: f32) -> [f32; 3] {
    let e = elevation_deg.to_radians();
    [e.cos(), -e.sin(), 0.0]
}

/// An orthographic sun cascade of half-width `radius` around `center` (depth `0..1`, the eye
/// `back` metres up-sun of the centre), drawn at `resolution`² texels.
fn cascade(sun: [f32; 3], center: Vec3, radius: f32, back: f32, resolution: u32) -> FogSunCascade {
    let dir = Vec3::from_array(sun).normalize();
    let up = if dir.y.abs() > 0.99 { Vec3::Z } else { Vec3::Y };
    let eye = center - dir * back;
    let view = look_at_mat4(eye, center, up);
    let proj = orthographic(-radius, radius, -radius, radius, 0.0, back + radius);
    FogSunCascade::new((proj * view).to_cols_array_2d(), resolution)
}

/// The world point a shadow-map texel centre's ray starts from (NDC depth 0).
fn texel_ray(c: &FogSunCascade, texel: [u32; 2]) -> (Vec3, Vec3) {
    let m = Mat4::from_cols_array_2d(&c.light_view_proj);
    let inv = m.inverse();
    let size = c.resolution as f32;
    let u = (texel[0] as f32 + 0.5) / size;
    let v = (texel[1] as f32 + 0.5) / size;
    let ndc = |z: f32| {
        let h = inv * Vec4::new(u * 2.0 - 1.0, 1.0 - v * 2.0, z, 1.0);
        h.truncate() / h.w
    };
    let (a, b) = (ndc(0.0), ndc(1.0));
    (a, b - a)
}

/// The depth the shadow pass stores at `texel` for a world that is one horizontal plane at
/// `plane_y` (a roof wider than the cascade, or level ground): the NDC depth where the texel
/// centre's ray meets it, 1 when it misses.
fn plane_depth(c: &FogSunCascade, texel: [u32; 2], plane_y: f32) -> f32 {
    let (origin, span) = texel_ray(c, texel);
    if span.y.abs() < 1.0e-9 {
        return 1.0;
    }
    let t = (plane_y - origin.y) / span.y;
    if (0.0..=1.0).contains(&t) { t } else { 1.0 }
}

/// Sun visibility of `world` under `rule` in a world whose only caster is the plane at `plane_y`.
fn visibility(c: &FogSunCascade, rule: FogSunShadowRule, sun: [f32; 3], world: Vec3, plane_y: f32) -> f32 {
    let s = c.sample(rule, sun, world.to_array());
    s.texel.map_or(1.0, |t| s.visibility(plane_depth(c, t, plane_y)))
}

/// Cascades the fog may read: half-widths from a near cascade to a long view's last one (the
/// fog's cascade is the first whose split reaches 150 m), each with the frame's up-sun reach
/// (`cascade_back`: at least the radius, the view's shadow range up to 2 km) and 2048² texels.
fn cascades_to_test(sun: [f32; 3], center: Vec3) -> Vec<(f32, FogSunCascade)> {
    [12.0, 40.0, 110.0, 180.0, 700.0]
        .into_iter()
        .map(|radius| {
            let back = f32::max(radius, if radius > 500.0 { 2000.0 } else { 200.0 });
            (radius, cascade(sun, center, radius, back, 2048))
        })
        .collect()
}

/// Edge positions of the sample in the cascade's square: the centre, half way, the start of the
/// legacy fade band, inside it and at its outer end.
const EDGES: [f32; 5] = [0.0, 0.5, 0.86, 0.93, 0.99];

#[test]
fn air_under_a_roof_reads_no_sun_at_every_cascade_and_edge_position() {
    let mut legacy_leaks = 0;
    let mut cases = 0;
    for elevation in [20.0_f32, 45.0, 60.0, 80.0] {
        let sun = sun_travel(elevation);
        let dir = Vec3::from_array(sun);
        let slope = elevation.to_radians().tan().recip();
        let roof = Vec3::new(3.0, ROOF_Y, -2.0);
        for (radius, c0) in cascades_to_test(sun, roof) {
            // Deeper under the roof (along the sun) than the cascade resolves: two calibrated
            // biases plus a texel of the roof's depth slope (the sample's place in its texel).
            let bias_m = c0.bias_ndc(FogSunShadowRule::Calibrated, sun) * c0.depth_m;
            let point = roof + dir * (2.0 * bias_m + c0.texel_m * slope);
            let back = f32::max(radius, if radius > 500.0 { 2000.0 } else { 200.0 });
            for edge in EDGES {
                // Place the cascade so `point` sits at `edge` of its square along light x.
                let m = Mat4::from_cols_array_2d(&c0.light_view_proj);
                let right = Vec3::new(m.x_axis.x, m.y_axis.x, m.z_axis.x).normalize();
                let c = cascade(sun, point - right * (edge * radius), radius, back, 2048);
                let s = c.sample(FogSunShadowRule::Calibrated, sun, point.to_array());
                assert_ne!(s.state, FogCascadeState::Outside, "edge {edge}: {s:?}");
                assert!((s.edge - edge).abs() < 1.0e-3, "placed at {edge}: {s:?}");
                let v = visibility(&c, FogSunShadowRule::Calibrated, sun, point, ROOF_Y);
                assert!(
                    v == 0.0,
                    "calibrated: {:.3} m under the roof, sun {elevation}°, cascade ±{radius} m, edge {edge}: \
                     visibility {v} ({s:?})",
                    ROOF_Y - point.y
                );
                cases += 1;
                if visibility(&c, FogSunShadowRule::Legacy, sun, point, ROOF_Y) > 0.0 {
                    legacy_leaks += 1;
                }
            }
        }
    }
    // The detector: the legacy rule lets sun into this air (its bias is a share of the depth
    // extent, and its fade band lights shadowed air near the square's edge).
    assert!(
        legacy_leaks > cases / 2,
        "legacy leaked in {legacy_leaks} of {cases} cases"
    );
}

#[test]
fn air_in_the_open_keeps_the_sun_under_both_rules() {
    for elevation in [20.0, 45.0, 60.0, 80.0] {
        let sun = sun_travel(elevation);
        // Above the roof: nothing between the sample and the sun.
        let point = Vec3::new(3.0, ROOF_Y + 2.0, -2.0);
        for (radius, c) in cascades_to_test(sun, point) {
            for rule in FogSunShadowRule::ALL {
                let v = visibility(&c, rule, sun, point, ROOF_Y);
                assert!(
                    v == 1.0,
                    "{rule:?}, sun {elevation}°, ±{radius} m: visibility {v}"
                );
            }
        }
    }
}

#[test]
fn samples_on_lit_ground_do_not_shadow_themselves() {
    // Froxel samples pulled onto a surface sit on it: level ground in the sun must read lit at
    // every sub-texel position (the calibrated bias's slope term).
    // Include 6° (near the slope cap) and an overhead sun when checking the smaller constant
    // margin: the entire slope term must still keep lit ground from shadowing itself.
    for elevation in [6.0, 10.0, 20.0, 45.0, 60.0, 80.0, 90.0] {
        let sun = sun_travel(elevation);
        for (radius, c) in cascades_to_test(sun, Vec3::ZERO) {
            for k in 0..64 {
                let f = k as f32 / 64.0;
                let point = Vec3::new(radius * 0.3 * (f - 0.5), 0.0, radius * 0.2 * (0.37 - f));
                let v = visibility(&c, FogSunShadowRule::Calibrated, sun, point, 0.0);
                assert!(
                    v == 1.0,
                    "lit ground at {point:?}, sun {elevation}°, ±{radius} m (texel {} m): visibility {v}",
                    c.texel_m
                );
            }
        }
    }
}

#[test]
fn calibrated_bias_is_texels_not_a_share_of_the_depth_extent() {
    let sun = sun_travel(60.0);
    let (_, long) = cascades_to_test(sun, Vec3::ZERO)[4];
    let legacy_m = long.bias_ndc(FogSunShadowRule::Legacy, sun) * long.depth_m;
    let calibrated_m = long.bias_ndc(FogSunShadowRule::Calibrated, sun) * long.depth_m;
    assert!(legacy_m > 4.0, "legacy bias on a 2 km view is metres: {legacy_m}");
    assert!(
        calibrated_m < 3.0 * long.texel_m,
        "calibrated bias {calibrated_m} m against texel {} m",
        long.texel_m
    );
}

#[test]
fn air_three_quarters_of_a_texel_under_a_flat_roof_is_shadowed() {
    // With an overhead sun the roof has no depth slope or sub-texel depth uncertainty.
    // A whole-texel constant bias nevertheless lights this band of air under the roof.
    // Keep a half-texel guard, rather than letting it stand for a full texel of empty air.
    let sun = [0.0, -1.0, 0.0];
    let roof = Vec3::new(3.0, ROOF_Y, -2.0);
    for (radius, c) in cascades_to_test(sun, roof) {
        let point = roof - Vec3::Y * (0.75 * c.texel_m);
        assert_eq!(
            visibility(&c, FogSunShadowRule::Calibrated, sun, point, ROOF_Y),
            0.0,
            "air under the roof at {point:?}, cascade ±{radius} m, texel {} m",
            c.texel_m
        );
        assert_eq!(visibility(&c, FogSunShadowRule::Legacy, sun, point, ROOF_Y), 1.0);
    }
}

#[test]
fn calibrated_samples_lie_in_the_air_in_front_of_the_surface() {
    let (start, len) = (5.4_f32, 3.7_f32);
    let mid = (start * (start + len)).sqrt();
    for k in 1..20 {
        let surface = start + len * k as f32 / 20.0;
        for i in 0..4 {
            let legacy =
                fog_sample_depth(FogSunShadowRule::Legacy, start, mid, len, i, surface).unwrap_or(f32::NAN);
            let calibrated = fog_sample_depth(FogSunShadowRule::Calibrated, start, mid, len, i, surface)
                .unwrap_or(f32::NAN);
            assert!(legacy <= surface);
            assert!(
                calibrated > start && calibrated < surface,
                "sample {i} at {calibrated} with the surface at {surface} in [{start}, {}]",
                start + len
            );
        }
    }
    // A surface inside the slice's first quarter: legacy piles every sample onto it.
    let surface = start + 0.2 * len;
    let piled = (0..4)
        .filter(|i| fog_sample_depth(FogSunShadowRule::Legacy, start, mid, len, *i, surface) == Some(surface))
        .count();
    assert_eq!(piled, 4);
    // A sub-ray whose surface is in front of the slice sees no air in it (legacy samples the surface).
    for i in 0..4 {
        assert_eq!(
            fog_sample_depth(FogSunShadowRule::Calibrated, start, mid, len, i, start - 1.0),
            None
        );
        assert_eq!(
            fog_sample_depth(FogSunShadowRule::Legacy, start, mid, len, i, start - 1.0),
            Some(start - 1.0)
        );
    }
    // Weights: legacy averages the four; calibrated weighs by the air seen in the slice.
    assert_eq!(
        fog_sample_weight(FogSunShadowRule::Legacy, start, len, start + 0.1),
        1.0
    );
    assert_eq!(
        fog_sample_weight(FogSunShadowRule::Calibrated, start, len, start - 1.0),
        0.0
    );
    assert_eq!(
        fog_sample_weight(FogSunShadowRule::Calibrated, start, len, start + 5.0 * len),
        1.0
    );
    let thin = fog_sample_weight(FogSunShadowRule::Calibrated, start, len, start + 0.1 * len);
    assert!((thin - 0.1).abs() < 1.0e-5, "{thin}");
    // Slices clear of any surface keep the legacy placement.
    for i in 0..4 {
        let far = start + 10.0 * len;
        assert_eq!(
            fog_sample_depth(FogSunShadowRule::Legacy, start, mid, len, i, far),
            fog_sample_depth(FogSunShadowRule::Calibrated, start, mid, len, i, far)
        );
    }
}

#[test]
fn rule_names_round_trip() {
    for rule in FogSunShadowRule::ALL {
        assert_eq!(FogSunShadowRule::from_name(rule.as_str()), Some(rule));
    }
    assert_eq!(FogSunShadowRule::default(), FogSunShadowRule::Legacy);
    assert_eq!(
        VolumetricFogSettings::default().sun_shadow,
        FogSunShadowRule::Legacy
    );
}

// --- The fog room on lavapipe --------------------------------------------------------------------

const W: u32 = 256;
const H: u32 = 160;
const EYE: [f32; 3] = [-1.0, 3.0, 7.5];
const TARGET: [f32; 3] = [1.5, 2.6, 0.0];
const FOV_Y_DEG: f32 = 70.0;
const NEAR: f32 = 0.05;
const FAR: f32 = 200.0;
const SUN_ELEVATION: f32 = 60.0;
/// Half the room's width and depth, and its height (metres).
const ROOM_HALF: f32 = 8.0;
const ROOM_H: f32 = 6.0;
/// Half the slit's width (metres).
const SLIT_HALF: f32 = 0.5;

/// A quad centred on `c` spanning `±u` and `±v`, facing `u × v`.
fn quad(c: Vec3, u: Vec3, v: Vec3) -> LabMesh {
    let n = u.cross(v).normalize_or(Vec3::Y).to_array();
    let corners = [c - u - v, c + u - v, c + u + v, c - u + v];
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p.to_array(), n, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// The spec 27 fog room: black floor, walls and a two-plate roof around the slit.
fn room() -> LabScene {
    let (hx, hy, hz) = (ROOM_HALF, ROOM_H / 2.0, ROOM_HALF);
    let plate = (ROOM_HALF - SLIT_HALF) / 2.0;
    let mut meshes = vec![quad(Vec3::ZERO, Vec3::X * hx, -Vec3::Z * hz)];
    for (c, u, v) in [
        (Vec3::new(hx, hy, 0.0), -Vec3::Z * hz, Vec3::Y * hy),
        (Vec3::new(-hx, hy, 0.0), Vec3::Z * hz, Vec3::Y * hy),
        (Vec3::new(0.0, hy, hz), Vec3::X * hx, Vec3::Y * hy),
        (Vec3::new(0.0, hy, -hz), -Vec3::X * hx, Vec3::Y * hy),
        (
            Vec3::new(-(SLIT_HALF + plate), ROOM_H, 0.0),
            Vec3::X * plate,
            Vec3::Z * hz,
        ),
        (
            Vec3::new(SLIT_HALF + plate, ROOM_H, 0.0),
            Vec3::X * plate,
            Vec3::Z * hz,
        ),
    ] {
        meshes.push(quad(c, u, v));
    }
    let mut black = LabMaterial {
        base_color: [0.03, 0.03, 0.03, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..LabMaterial::default()
    };
    black.double_sided = true;
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y_DEG.to_radians(),
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 8.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new(sun_travel(SUN_ELEVATION), [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![black],
        meshes,
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = true;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

fn outdoor(rule: FogSunShadowRule, probe: &FogSunShadowProbe) -> OutdoorParams {
    OutdoorParams {
        planet: regia_render::AtmosphereParameters::from_preset(regia_render::AtmospherePreset::Airless),
        fog: VolumetricFogSettings {
            density: 0.08,
            height_falloff: 0.4,
            base_height: 0.0,
            albedo: [0.9; 3],
            anisotropy: 0.0,
            planet_medium: false,
            sun_shadow: rule,
            ..VolumetricFogSettings::default()
        },
        // The room is an interior: its fog sees no sky, so the sun is its only light.
        interiors: vec![InteriorVolume::with_floor(
            [0.0, ROOM_H / 2.0, 0.0],
            [ROOM_HALF, ROOM_H / 2.0, ROOM_HALF],
        )],
        fog_shadow_probe: Some(probe.clone()),
        ..OutdoorParams::default()
    }
}

/// Where a pixel's ray goes in the room.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PixelClass {
    /// The ray comes no nearer than [`DARK_MARGIN_M`] (measured across the shaft at the roof) to
    /// the sunlit air, and ends on a shadowed surface.
    Dark,
    /// The ray crosses at least [`SHAFT_PATH_M`] of sunlit air and ends on a shadowed surface.
    Shaft,
    /// Anything else (near the shaft, the lit floor patch, the slit's sky).
    Other,
}

const DARK_MARGIN_M: f32 = 1.5;
const SHAFT_PATH_M: f32 = 1.0;

/// How far `p` is from the sunlit air across the shaft, measured at the roof (0 inside it).
fn shaft_distance(p: Vec3) -> f32 {
    let tan = SUN_ELEVATION.to_radians().tan();
    let x_roof = p.x - (ROOM_H - p.y) / tan;
    (x_roof.abs() - SLIT_HALF).max(0.0)
}

fn classify() -> Vec<PixelClass> {
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj = perspective(FOV_Y_DEG.to_radians(), W as f32 / H as f32, NEAR, FAR);
    let inv = (proj * view).inverse();
    let eye = Vec3::from_array(EYE);
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let ndc = Vec4::new(
                (x as f32 + 0.5) / W as f32 * 2.0 - 1.0,
                1.0 - (y as f32 + 0.5) / H as f32 * 2.0,
                1.0,
                1.0,
            );
            let far = inv * ndc;
            let dir = (far.truncate() / far.w - eye).normalize();
            // Exit distance from the room box.
            let exit = |o: f32, d: f32, lo: f32, hi: f32| {
                if d > 1.0e-6 {
                    (hi - o) / d
                } else if d < -1.0e-6 {
                    (lo - o) / d
                } else {
                    f32::INFINITY
                }
            };
            let t = exit(eye.x, dir.x, -ROOM_HALF, ROOM_HALF)
                .min(exit(eye.y, dir.y, 0.0, ROOM_H))
                .min(exit(eye.z, dir.z, -ROOM_HALF, ROOM_HALF));
            let hit = eye + dir * t;
            let lit_end = (hit.y >= ROOM_H - 1.0e-3 && hit.x.abs() <= SLIT_HALF + 0.3)
                || (hit.y <= 1.0e-3 && shaft_distance(hit) <= 0.3);
            let steps = 512;
            let mut nearest = f32::INFINITY;
            let mut inside = 0.0;
            for k in 0..steps {
                let p = eye + dir * (t * (k as f32 + 0.5) / steps as f32);
                let d = shaft_distance(p);
                nearest = nearest.min(d);
                if d == 0.0 {
                    inside += t / steps as f32;
                }
            }
            out.push(if lit_end {
                PixelClass::Other
            } else if nearest > DARK_MARGIN_M {
                PixelClass::Dark
            } else if inside >= SHAFT_PATH_M {
                PixelClass::Shaft
            } else {
                PixelClass::Other
            });
        }
    }
    out
}

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Mean linear luminance of the room's dark and shaft pixels after 12 frames under `rule`, and
/// the probe it filled.
fn render_room(
    device: &RenderDevice,
    rule: FogSunShadowRule,
    classes: &[PixelClass],
) -> (f32, f32, FogSunShadowProbe) {
    let lab = room();
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 8.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let mut frame = lab.as_frame_scene_with_quality(quality());
    frame.post = Some(&post);
    let probe = FogSunShadowProbe::default();
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_outdoor(outdoor(rule, &probe));
    for i in 0..12 {
        if i == 11 {
            renderer.request_hdr_capture();
        }
        renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("fog room frame: {err}"));
    }
    let hdr = renderer
        .take_hdr_capture(device)
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the post stack ran, so the HDR copy exists"));
    let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let mean = |class: PixelClass| {
        let (sum, n) = hdr
            .rgb
            .iter()
            .zip(classes)
            .filter(|(_, c)| **c == class)
            .fold((0.0_f64, 0_u32), |(s, n), (rgb, _)| {
                (s + f64::from(lum(*rgb)), n + 1)
            });
        (sum / f64::from(n.max(1))) as f32
    };
    (mean(PixelClass::Dark), mean(PixelClass::Shaft), probe)
}

#[test]
fn the_fog_room_keeps_the_sun_out_of_the_air_under_its_roof() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let classes = classify();
    let count = |c: PixelClass| classes.iter().filter(|k| **k == c).count();
    eprintln!(
        "pixels: dark {} shaft {} other {}",
        count(PixelClass::Dark),
        count(PixelClass::Shaft),
        count(PixelClass::Other)
    );
    assert!(count(PixelClass::Dark) > 2000 && count(PixelClass::Shaft) > 500);
    let (legacy_dark, legacy_shaft, _) = render_room(&device, FogSunShadowRule::Legacy, &classes);
    let (dark, shaft, probe) = render_room(&device, FogSunShadowRule::Calibrated, &classes);
    eprintln!(
        "legacy: dark {legacy_dark:.3} shaft {legacy_shaft:.3} ({:.4}); calibrated: dark {dark:.3} shaft {shaft:.3} ({:.4})",
        legacy_dark / legacy_shaft.max(1e-9),
        dark / shaft.max(1e-9)
    );
    assert!(shaft > 0.0, "the shaft is lit: {shaft}");
    assert!(
        dark <= 0.02 * shaft,
        "calibrated: the air under the roof reads sun: dark {dark} against shaft {shaft}"
    );
    // The probe says which cascade the fog read, and its mirror puts air under the roof in it.
    let read = probe
        .last()
        .unwrap_or_else(|| panic!("the fog pass filled the probe"));
    eprintln!(
        "probe: {read:?}; bias {:.3} m along the sun (legacy {:.3} m)",
        read.bias_m(),
        read.cascade
            .map_or(0.0, |c| c.depth_m * regia_render::FOG_LEGACY_SHADOW_BIAS_NDC)
    );
    assert_eq!(read.rule, FogSunShadowRule::Calibrated);
    assert!(
        read.cascade_index.is_some() && read.cascade_count >= 1,
        "{read:?}"
    );
    let under = read
        .sample([-4.0, ROOM_H - 0.3, 0.0])
        .unwrap_or_else(|| panic!("a cascade was read: {read:?}"));
    assert_eq!(under.state, FogCascadeState::Covered, "{under:?}");
    assert!(read.bias_m() < 0.5, "a texel-sized bias: {} m", read.bias_m());
}
