// SPDX-License-Identifier: MPL-2.0
//! The interior shade over a baked GI volume (ADR-7975), on the GPU (lavapipe in CI). No golden:
//! every check reads the linear HDR frame (`FrameRenderer::request_hdr_capture`) and compares
//! pixels of the same frame, or two frames of the same setup.
//!
//! The scene is a closed tube (floor, roof, two side walls and two end walls) with an
//! `InteriorVolume` over its air and the eye inside it, so the forward pass takes the interior
//! shade. No sun, no sky map: the light is the interior floor and, where installed, a synthetic
//! baked volume (one interior detail brick, every probe valid and unoccluded) whose uniform sky
//! radiance rises linearly across the tube, `L(x) = SKY · (4 + x)`.
//!
//! - `the_interior_shade_reads_the_baked_volume_where_it_covers` (Desktop Low, the tier that
//!   relies on baked GI for tunnels) and `..._on_desktop_high`: two floor blocks mirrored about
//!   the tube's axis differ, within 5 %, by the ratio the exterior shade draws at the same blocks
//!   from the same volume (the interior shade off and no box: the baked sample the exterior
//!   has always read), and by the baked irradiance's ratio (the CPU mirror `sample_static_gi`,
//!   averaged over the blocks' floor points, normal and view direction); each block's level
//!   matches the exterior's within 5 %. Before ADR-7975 the interior shade read only the neutral
//!   floor: both blocks were equal (the "flat floor" of the v0.5.0 Known issue). A floor-row
//!   table (frame, exterior frame, CPU bake with and without the view bias) is printed.
//! - `a_tube_without_a_covering_volume_keeps_the_flat_floor`: a volume installed but switched off,
//!   and no volume at all, draw the same frame bit for bit; a volume that does not cover the tube
//!   leaves the floor flat (the interior shade as before). The frames' hashes are printed for a
//!   byte comparison with the parent revision.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, BrickFlags, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, SH_Y00,
    ShRgb, VolumeLayout,
};
use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    BakedGiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, INTERIOR_AMBIENT_FLOOR_LUX,
    InteriorVolume, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SsaoMode, sample_static_gi,
};

const W: u32 = 160;
const H: u32 = 96;
/// Frames per still: more than the screen-space GI's settle (about 9) though the baked policy
/// turns it off; the last one is read back.
const FRAMES: u32 = 10;
/// The tube's air: x in [-2, 2], y in [0, 3], z in [-8, 8].
const TUBE_CENTER: [f32; 3] = [0.0, 1.5, 0.0];
const TUBE_HALF: [f32; 3] = [2.0, 1.5, 8.0];
/// The eye, on the tube's axis, and its target on the floor ahead.
const EYE: [f32; 3] = [0.0, 2.2, 5.0];
const TARGET: [f32; 3] = [0.0, 0.0, -2.0];
const FOV_Y: f32 = 1.0;
const NEAR: f32 = 0.05;
const FAR: f32 = 100.0;
/// The two floor points, mirrored about the tube's axis (x = 0): the gradient's ends.
const FLOOR_LEFT: [f32; 3] = [-1.25, 0.0, -1.0];
const FLOOR_RIGHT: [f32; 3] = [1.25, 0.0, -1.0];
/// Probe sky radiance scale (cd/m²): L(x) = SKY · (4 + x), 2 · SKY at x = −2, 6 · SKY at x = 2.
const SKY: f32 = 50.0;
/// Floor albedo.
const ALBEDO: f32 = 0.5;
/// The stated bound on the ratio of the two floor points against the baked irradiance's ratio.
const RATIO_BOUND: f64 = 0.05;

/// Serializes device use across this binary's test threads (ADR-1438).
static SERIAL_GPU: Mutex<()> = Mutex::new(());

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| {
        RenderDevice::headless().unwrap_or_else(|err| panic!("baked GI needs a GPU (lavapipe is one): {err}"))
    })
}

/// Radiance SH of a uniform radiance `l` (no L1).
fn uniform(l: f32) -> ShRgb {
    let k = 4.0 * std::f32::consts::PI * SH_Y00 * l;
    [[k, 0.0, 0.0, 0.0], [k, 0.0, 0.0, 0.0], [k, 0.0, 0.0, 0.0]]
}

/// One interior brick of 1 m × 1 m × 2 m cells from `origin` (5 × 4 × 9 probes, 4 m × 3 m × 16 m),
/// every probe valid with far moments, lit by the sky component at `SKY · (4 + x)`.
fn volume_at(origin: [f32; 3]) -> Arc<BakedGiVolume> {
    let far = 16.0;
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![DetailBrick {
            origin,
            spacing: [1.0, 1.0, 2.0],
            counts: [5, 4, 9],
            first_probe: 0,
            flags: BrickFlags::INTERIOR,
        }],
        moment_resolution: 4,
        max_distance: far,
    };
    layout.assign_brick_offsets();
    let brick = &layout.bricks[0];
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|i| {
            let (x, y, z) = brick.coords(i);
            let p = brick.nominal_position(x, y, z);
            ProbeData {
                sh: [
                    uniform(0.0),
                    uniform(SKY * (4.0 + p[0])),
                    uniform(0.0),
                    uniform(0.0),
                ],
                moments: vec![[far, far * far]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            }
        })
        .collect();
    Arc::new(
        BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .unwrap_or_else(|err| panic!("synthetic volume: {err:?}")),
    )
}

/// The brick over the tube's air.
fn tube_volume() -> Arc<BakedGiVolume> {
    static V: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    V.get_or_init(|| volume_at([-2.0, 0.0, -8.0])).clone()
}

fn quad(corners: [[f32; 3]; 4], material: usize) -> LabMesh {
    let [a, b, c, _] = corners.map(Vec3::from_array);
    let normal = (b - a).cross(c - a).normalize_or_zero().to_array();
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// The tube's six inner faces, each facing the air (the frame is double-sided as well).
fn tube() -> Vec<LabMesh> {
    let [hx, hy, hz] = TUBE_HALF;
    let (y0, y1) = (TUBE_CENTER[1] - hy, TUBE_CENTER[1] + hy);
    vec![
        // Floor (up) and roof (down).
        quad([[-hx, y0, hz], [hx, y0, hz], [hx, y0, -hz], [-hx, y0, -hz]], 0),
        quad([[-hx, y1, -hz], [hx, y1, -hz], [hx, y1, hz], [-hx, y1, hz]], 0),
        // Side walls (+x facing at x = −hx, −x facing at x = hx).
        quad([[-hx, y0, -hz], [-hx, y1, -hz], [-hx, y1, hz], [-hx, y0, hz]], 0),
        quad([[hx, y0, hz], [hx, y1, hz], [hx, y1, -hz], [hx, y0, -hz]], 0),
        // End walls: closed.
        quad([[-hx, y0, -hz], [hx, y0, -hz], [hx, y1, -hz], [-hx, y1, -hz]], 0),
        quad([[hx, y0, hz], [-hx, y0, hz], [-hx, y1, hz], [hx, y1, hz]], 0),
    ]
}

fn scene() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 9.0,
        ambient_lux: INTERIOR_AMBIENT_FLOOR_LUX,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [ALBEDO, ALBEDO, ALBEDO, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: tube(),
    }
}

/// `preset` with the interior shade on and nothing that keeps a history or adds its own light.
fn quality(preset: RenderProfilePreset) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    q.interior_ambient = true;
    q.baked_gi = true;
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.ssao = SsaoMode::Off;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q
}

/// A renderer with the tube's interior volume (airless, so no aerial perspective) and `volume`.
fn renderer(volume: Option<Arc<BakedGiVolume>>, enabled: bool) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_outdoor(OutdoorParams {
        planet: regia_render::AtmosphereParameters::from_preset(regia_render::AtmospherePreset::Airless),
        interiors: vec![InteriorVolume::with_floor(TUBE_CENTER, TUBE_HALF)],
        ..OutdoorParams::default()
    });
    let installed = volume.is_some();
    r.set_baked_gi(volume);
    if installed {
        r.set_baked_gi_enabled(Some(enabled));
        r.set_baked_gi_settings(BakedGiSettings::default());
    }
    r
}

/// The linear HDR frame (cd/m²) after [`FRAMES`] frames, with the interior shade on.
fn capture(r: &mut FrameRenderer, preset: RenderProfilePreset) -> Vec<[f32; 3]> {
    capture_with(r, quality(preset))
}

/// [`capture`] at `quality`.
fn capture_with(r: &mut FrameRenderer, quality: FrameQuality) -> Vec<[f32; 3]> {
    let lab = scene();
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: lab.exposure_ev,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    for i in 0..FRAMES {
        if i + 1 == FRAMES {
            r.request_hdr_capture();
        }
        r.render_offscreen(device(), &frame)
            .unwrap_or_else(|err| panic!("frame: {err}"));
    }
    let hdr = r
        .take_hdr_capture(device())
        .unwrap_or_else(|err| panic!("read back: {err}"))
        .unwrap_or_else(|| panic!("the frame recorded its HDR copy"));
    assert_eq!((hdr.width, hdr.height), (W, H));
    hdr.rgb
}

/// Pixel of a world point (the lab camera: right-handed look-at, depth 0..1).
fn project(p: [f32; 3]) -> (u32, u32) {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(EYE),
        Vec3::from_array(TARGET),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, NEAR, FAR);
    let clip: Vec4 = (proj * view as Mat4) * Vec3::from_array(p).extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - ndc.y * 0.5) * H as f32) as u32,
    )
}

fn luminance(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

/// Mean luminance of the 4 × 4 block centred on pixel `at`.
fn block(img: &[[f32; 3]], at: (u32, u32)) -> f64 {
    let mut sum = 0.0;
    for y in at.1 - 2..at.1 + 2 {
        for x in at.0 - 2..at.0 + 2 {
            sum += luminance(img[(y * W + x) as usize]);
        }
    }
    sum / 16.0
}

/// The two floor blocks, the right one taken as the left one's mirror column so the pair is
/// exactly symmetric about the axis.
fn floor_pair(img: &[[f32; 3]]) -> (f64, f64) {
    let (lx, ly) = project(FLOOR_LEFT);
    let right = (W - lx, ly);
    let (rx, ry) = project(FLOOR_RIGHT);
    assert!(
        rx.abs_diff(right.0) <= 1 && ry == ly,
        "the camera is symmetric about the tube's axis"
    );
    (block(img, (lx, ly)), block(img, right))
}

/// FNV-1a of the frame's bytes: a short fingerprint for comparing two revisions.
fn fingerprint(img: &[[f32; 3]]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for c in img {
        for v in c {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h
}

/// The baked irradiance / π the CPU mirror reads at a floor point (sky component, as baked).
fn baked_over_pi(p: [f32; 3]) -> f64 {
    baked_over_pi_with(p, &BakedGiSettings::default())
}

/// [`baked_over_pi`] with `settings`.
fn baked_over_pi_with(p: [f32; 3], settings: &BakedGiSettings) -> f64 {
    let up = [0.0, 1.0, 0.0];
    let v = Vec3::from_array(EYE) - Vec3::from_array(p);
    let v = v.normalize().to_array();
    let scales = [[0.0; 3], [1.0; 3], [0.0; 3], [0.0; 3]];
    let s = sample_static_gi(&tube_volume(), settings, &scales, p, up, v);
    assert!(s.coverage > 0.99, "the brick covers the floor at {p:?}: {s:?}");
    f64::from(s.irradiance_over_pi[1])
}

/// The floor point (y = 0) under pixel centre `(x, y)`, if that pixel's ray falls on the floor.
fn floor_point(x: u32, y: u32) -> Option<[f32; 3]> {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(EYE),
        Vec3::from_array(TARGET),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, NEAR, FAR);
    let inv = (proj * view as Mat4).inverse();
    let ndc = Vec3::new(
        (x as f32 + 0.5) / W as f32 * 2.0 - 1.0,
        1.0 - (y as f32 + 0.5) / H as f32 * 2.0,
        0.5,
    );
    let far = inv * ndc.extend(1.0);
    let eye = Vec3::from_array(EYE);
    let dir = (far.truncate() / far.w - eye).normalize_or_zero();
    if dir.y >= -1.0e-4 {
        return None;
    }
    let t = -eye.y / dir.y;
    let p = eye + dir * t;
    (p.x.abs() < TUBE_HALF[0] && p.z.abs() < TUBE_HALF[2]).then_some(p.to_array())
}

/// Prints the floor row through the two test points: the frame's luminance against the
/// albedo × baked E/π the CPU mirror reads at each pixel's floor point (a diagnostic for a
/// failing ratio: it shows whether the frame departs from the bake evenly or at one place).
fn print_floor_row(preset: RenderProfilePreset, img: &[[f32; 3]], exterior: &[[f32; 3]]) {
    let (_, ly) = project(FLOOR_LEFT);
    let unbiased = BakedGiSettings {
        view_bias: 0.0,
        ..BakedGiSettings::default()
    };
    let mut line = String::new();
    for x in (0..W).step_by(8) {
        let Some(p) = floor_point(x, ly) else { continue };
        let want = f64::from(ALBEDO) * baked_over_pi(p);
        let flat = f64::from(ALBEDO) * baked_over_pi_with(p, &unbiased);
        let c = img[(ly * W + x) as usize];
        let got = luminance(c);
        let ext = luminance(exterior[(ly * W + x) as usize]);
        line.push_str(&format!(
            "\n  px {x:3} floor x {:+.3} z {:+.3}: frame {got:8.3}, exterior frame {ext:8.3}, bake {want:8.3}, bake without view bias {flat:8.3}; frame/bake {:.4}, frame/unbiased {:.4}, frame/exterior {:.4}",
            p[0],
            p[2],
            got / want.max(1.0e-9),
            got / flat.max(1.0e-9),
            got / ext.max(1.0e-9)
        ));
    }
    eprintln!("{preset:?}: floor row {ly}:{line}");
}

/// The 4 × 4 pixels [`block`] averages around `at`.
fn block_pixels(at: (u32, u32)) -> impl Iterator<Item = (u32, u32)> {
    (at.1 - 2..at.1 + 2).flat_map(move |y| (at.0 - 2..at.0 + 2).map(move |x| (x, y)))
}

/// Albedo × the CPU mirror's baked E/π, averaged over the floor points of [`block`]'s pixels at
/// `at` (the points the frame's block shows, not the nominal test point: the block's centre
/// lies a few centimetres off it).
fn block_bake(at: (u32, u32)) -> f64 {
    let mut sum = 0.0;
    for (x, y) in block_pixels(at) {
        let p = floor_point(x, y).unwrap_or_else(|| panic!("pixel {x},{y} shows the floor"));
        sum += f64::from(ALBEDO) * baked_over_pi(p);
    }
    sum / 16.0
}

/// The pixels of the two floor blocks ([`floor_pair`]).
fn floor_pair_at() -> ((u32, u32), (u32, u32)) {
    let (lx, ly) = project(FLOOR_LEFT);
    ((lx, ly), (W - lx, ly))
}

fn check_reads_the_bake(preset: RenderProfilePreset) {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let img = capture(&mut renderer(Some(tube_volume()), true), preset);
    // The same tube and volume with the interior shade off and no interior box: the exterior
    // shade, which read the baked volume before ADR-7975. (With the interior shade on and no box
    // the whole view is interior.)
    let mut exterior = renderer(Some(tube_volume()), true);
    exterior.set_outdoor(OutdoorParams {
        planet: regia_render::AtmosphereParameters::from_preset(regia_render::AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    let exterior = capture_with(
        &mut exterior,
        FrameQuality {
            interior_ambient: false,
            ..quality(preset)
        },
    );
    print_floor_row(preset, &img, &exterior);
    let (left, right) = floor_pair(&img);
    let (el, er) = floor_pair(&exterior);
    let (at_l, at_r) = floor_pair_at();
    let (bl, br) = (block_bake(at_l), block_bake(at_r));
    let (got, ext, want) = (right / left.max(1.0e-9), er / el.max(1.0e-9), br / bl);
    eprintln!(
        "{preset:?}: interior floor left {left:.3} right {right:.3} cd/m2, ratio {got:.4}; exterior shade left {el:.3} right {er:.3}, ratio {ext:.4}; CPU albedo x baked E/pi left {bl:.3} right {br:.3}, ratio {want:.4}; flat floor {:.3}",
        f64::from(ALBEDO * INTERIOR_AMBIENT_FLOOR_LUX) / std::f64::consts::PI
    );
    assert!(
        want > 1.5,
        "the synthetic gradient is steep enough to see: {want}"
    );
    // The interior shade reads the baked sample the exterior reads.
    assert!(
        (got / ext - 1.0).abs() <= RATIO_BOUND,
        "{preset:?}: inside the interior box the floor must follow the baked volume's gradient as the \
         exterior shade does: right/left {got:.4}, the exterior's {ext:.4} (bound {RATIO_BOUND}); a ratio \
         near 1 is the flat floor"
    );
    for (name, a, b) in [("left", left, el), ("right", right, er)] {
        assert!(
            (a / b - 1.0).abs() <= RATIO_BOUND,
            "{preset:?}: the {name} floor reads {a:.3} cd/m2 inside the box, {b:.3} in the exterior shade"
        );
    }
    // And that gradient is the bake's (the CPU mirror at the blocks' floor points).
    assert!(
        (got / want - 1.0).abs() <= RATIO_BOUND,
        "{preset:?}: inside the interior box the floor must follow the baked volume's gradient: \
         right/left {got:.4}, the baked irradiance's ratio {want:.4} (bound {RATIO_BOUND}); \
         a ratio near 1 is the flat floor"
    );
    // And the level is the bake's, not the floor's: albedo × E/π within twice the bound.
    assert!(
        (left / bl - 1.0).abs() <= RATIO_BOUND * 2.0,
        "{preset:?}: the left floor reads {left:.3} cd/m2, the baked albedo × E/π {bl:.3}"
    );
}

#[test]
fn the_interior_shade_reads_the_baked_volume_where_it_covers() {
    check_reads_the_bake(RenderProfilePreset::DesktopLow);
}

#[test]
fn the_interior_shade_reads_the_baked_volume_on_desktop_high() {
    check_reads_the_bake(RenderProfilePreset::DesktopHigh);
}

#[test]
fn a_tube_without_a_covering_volume_keeps_the_flat_floor() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let preset = RenderProfilePreset::DesktopHigh;
    let none = capture(&mut renderer(None, true), preset);
    let off = capture(&mut renderer(Some(tube_volume()), false), preset);
    // A brick 100 m away: the baked lookup runs, but covers nothing the frame shows.
    let away = capture(&mut renderer(Some(volume_at([100.0, 0.0, -8.0])), true), preset);
    eprintln!(
        "fingerprints: no volume {:016x}, volume off {:016x}, volume away {:016x}",
        fingerprint(&none),
        fingerprint(&off),
        fingerprint(&away)
    );
    let differ = none.iter().zip(&off).filter(|(a, b)| a != b).count();
    assert_eq!(
        differ, 0,
        "a volume switched off draws the frame without one, bit for bit"
    );
    for (name, img) in [("no volume", &none), ("volume away", &away)] {
        let (left, right) = floor_pair(img);
        let flat = f64::from(ALBEDO * INTERIOR_AMBIENT_FLOOR_LUX) / std::f64::consts::PI;
        eprintln!("{name}: floor left {left:.4} right {right:.4}, flat floor {flat:.4}");
        assert!(left > 0.0, "{name}: the interior floor lights the tube");
        assert!(
            (right / left - 1.0).abs() <= 1.0e-3,
            "{name}: without a covering volume the interior floor is flat: {left} vs {right}"
        );
    }
}
