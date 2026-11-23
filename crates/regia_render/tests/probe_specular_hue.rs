// SPDX-License-Identifier: MPL-2.0
//! `ProbeSpecular::LobeHue` (ADR-2996): the mirror sphere of `studio_gradient.rs` under the probe GI.
//!
//! The first test of that file is an open defect on the default: a mirror lobe aimed at the nadir
//! under a blue-heavy sky drives blue negative first in the per-channel L1 evaluation, and the clamp
//! at 0 leaves a red-to-yellow spot. These tests run its scenario with the mode on, through traced
//! probes and through a baked volume (with its ZH3 evaluation too), and check that the mode leaves
//! the default frame alone (the warm frame's counters have their own binary,
//! `probe_specular_hue_warm.rs`).

use std::sync::Arc;

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, SH_Y1, ShRgb,
    TerrainLayer, VolumeLayout,
};
use regia_render::{
    BakedGiSettings, DdgiSettings, DirectionalLightUniform, EnvGroundPlane, EnvironmentAuthority,
    FrameQuality, FrameRenderer, GiMode, LabMaterial, LabMesh, LabScene, PostSettings, ProbeSpecular,
    RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, StudioEquirect, StudioGradient,
    horizontal_plus_fov_y, translate_mesh, uv_sphere,
};

const W: u32 = 256;
const H: u32 = 256;
const FOV_Y: f32 = 0.75;

fn gradient() -> StudioGradient {
    StudioGradient::new(
        [2400.0, 4200.0, 8000.0],
        [2600.0, 2800.0, 3000.0],
        [600.0, 600.0, 600.0],
    )
}

fn scene() -> LabScene {
    let (v, i) = uv_sphere(3.0, 128, 64);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 2.0, 9.0],
        target: [0.0, 2.0, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: 13.0,
            bloom: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [1.0, 1.0, 1.0, 1.0],
            metallic: 1.0,
            roughness: 0.05,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [0.0, 2.0, 0.0]),
            indices: i,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn quality() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = SsaoMode::Off;
    quality.taau = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality
}

fn renderer(device: &RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_environment_authority(EnvironmentAuthority::Studio);
    assert_eq!(renderer.set_studio_gradient(device, Some(&gradient())), Ok(true));
    renderer
}

fn set_mode(renderer: &mut FrameRenderer, mode: ProbeSpecular) {
    renderer.set_ddgi_settings(DdgiSettings {
        specular: mode,
        ..renderer.ddgi_settings().clone()
    });
}

fn frames(device: &RenderDevice, renderer: &mut FrameRenderer, quality: FrameQuality, count: u32) -> Vec<u8> {
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..count {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Mean color of the 8 columns around the sphere's vertical diameter on row `y`.
fn row(pixels: &[u8], y: u32) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    for x in W / 2 - 4..W / 2 + 4 {
        let i = ((y * W + x) * 4) as usize;
        for (c, v) in sum.iter_mut().enumerate() {
            *v += f32::from(pixels[i + c]) / 8.0;
        }
    }
    sum
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// First and last sphere rows inside its silhouette, with a margin that stays off the limb.
fn sphere_rows() -> (u32, u32) {
    let half_fov = horizontal_plus_fov_y(FOV_Y, W as f32 / H as f32) * 0.5;
    let ndc = (3.0_f32 / 9.0).asin().tan() / half_fov.tan();
    let half_rows = (ndc * H as f32 * 0.5).floor() as u32;
    let span = 2 * half_rows;
    (H / 2 - half_rows + span / 8, H / 2 + half_rows - span / 8)
}

/// How far the channels separate on the sphere's reflected ground: the largest difference between
/// a row's channels, in display levels, of the rows below the horizon's soft edge. A neutral ground
/// has none; the defect's spot is [202, 33, 0], 202 levels of it.
fn ground_separation(pixels: &[u8]) -> f32 {
    let (_, bottom) = sphere_rows();
    let mid = H / 2;
    let mut worst = 0.0f32;
    for y in mid + (bottom - mid) / 4..=bottom {
        let c = row(pixels, y);
        let (top, low) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
        worst = worst.max(top - low);
    }
    worst
}

fn print_rows(what: &str, pixels: &[u8]) {
    let (top, bottom) = sphere_rows();
    let rows: Vec<_> = (0..=8)
        .map(|k| top + (bottom - top) * k / 8)
        .map(|y| row(pixels, y).map(|v| v.round() as i32))
        .collect();
    eprintln!("{what}: rows top to bottom {rows:?}");
}

/// The checks `a_metal_sphere_reflects_the_gradient_from_ground_through_horizon_to_zenith` makes of
/// the reflected sky (`studio_gradient.rs`), and that no spot separates the channels.
fn assert_sky_ramp_and_no_spot(what: &str, pixels: &[u8], most_separation: f32) {
    let (top, bottom) = sphere_rows();
    let mid = H / 2;
    let mut worst_drop = 0.0f32;
    for y in top..mid {
        worst_drop = worst_drop.max(luma(row(pixels, y + 1)) - luma(row(pixels, y)));
    }
    let (sky_top, horizon, ground) = (row(pixels, top), row(pixels, mid), row(pixels, bottom));
    assert!(
        worst_drop <= 1.5,
        "{what}: the sky darkens going up by {worst_drop:.2} levels"
    );
    assert!(
        luma(sky_top) > luma(horizon) + 12.0,
        "{what}: zenith side not brighter"
    );
    assert!(
        (sky_top[2] - sky_top[0]) > (horizon[2] - horizon[0]) + 15.0,
        "{what}: zenith side not bluer"
    );
    assert!(
        luma(horizon) > luma(ground) + 10.0,
        "{what}: ground not darker than the horizon"
    );
    let separation = ground_separation(pixels);
    assert!(
        separation <= most_separation,
        "{what}: the reflected ground separates its channels by {separation:.1} levels"
    );
    let share = weakest_channel_share(pixels);
    assert!(
        share > 0.5,
        "{what}: a channel of the reflected ground is lost ({share:.3})"
    );
}

/// [`assert_sky_ramp_and_no_spot`] and that test's flat ground. Its 4 levels hold for a volume that
/// holds the open sky; traced probes also see the sphere itself, which moves the lobe's level there.
fn assert_sky_ramp_and_flat_ground(what: &str, pixels: &[u8]) {
    assert_sky_ramp_and_no_spot(what, pixels, 12.0);
    let (_, bottom) = sphere_rows();
    let mid = H / 2;
    let lower: Vec<f32> = (mid + (bottom - mid) / 2..=bottom)
        .map(|y| luma(row(pixels, y)))
        .collect();
    let spread =
        lower.iter().copied().fold(f32::MIN, f32::max) - lower.iter().copied().fold(f32::MAX, f32::min);
    assert!(
        spread <= 4.0,
        "{what}: the ground reflection is not flat ({spread:.2} levels)"
    );
}

/// The gradient's L1 radiance: per channel, its mean and its gradient along +Y (the field is
/// symmetric about Y). Midpoint rule over the elevation.
fn gradient_l1() -> ([f32; 3], [f32; 3]) {
    let g = gradient();
    let mut mean = [0.0f32; 3];
    let mut grad = [0.0f32; 3];
    let steps = 4096;
    for j in 0..steps {
        let y = -1.0 + 2.0 * (j as f32 + 0.5) / steps as f32;
        let side = (1.0 - y * y).sqrt();
        let l = g.radiance([side, y, 0.0]);
        for c in 0..3 {
            mean[c] += l[c] * 0.5 * (2.0 / steps as f32);
            grad[c] += 3.0 * l[c] * y * 0.5 * (2.0 / steps as f32);
        }
    }
    (mean, grad)
}

/// A terrain-layer volume around the sphere whose every probe holds the gradient's L1 light, as a
/// bake of the open studio would. No moments in the way: the distance clamp is far.
fn gradient_volume() -> Arc<BakedGiVolume> {
    l1_volume(1.0)
}

/// [`gradient_volume`] with the gradient's L1 times `gain`, for a sharper field than the sky's.
fn l1_volume(gain: f32) -> Arc<BakedGiVolume> {
    let (mean, grad) = gradient_l1();
    let grad = grad.map(|g| g * gain);
    let l00 = 4.0 * std::f32::consts::PI * SH_Y00;
    let l1 = 4.0 * std::f32::consts::PI / 3.0 * SH_Y1;
    let sh: ShRgb = std::array::from_fn(|c| [l00 * mean[c], 0.0, l1 * grad[c], 0.0]);
    let zero: ShRgb = [[0.0; 4]; 3];
    let counts = [9u32, 9u32];
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-16.0, -16.0],
            spacing: [4.0, 4.0],
            counts,
            levels: vec![1.0, 3.0, 6.0, 9.0],
            ground: vec![-2.0; (counts[0] * counts[1]) as usize],
        }),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 64.0,
    };
    layout.assign_brick_offsets();
    let far = 64.0f32;
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|_| ProbeData {
            sh: [zero, sh, zero, zero],
            moments: vec![[far, far * far]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
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
        .expect("the gradient's volume"),
    )
}

/// The first defect's own numbers, traced probes: the default keeps its spot (so these tests would
/// see it), the mode has none, and everything else that test asks of the sphere holds.
#[test]
fn traced_probes_reflect_the_ground_without_a_channel_separated_spot() {
    let device = RenderDevice::headless().expect("lavapipe");
    let mut off = renderer(&device);
    let off_pixels = frames(&device, &mut off, quality(), 2);
    let mut on = renderer(&device);
    set_mode(&mut on, ProbeSpecular::LobeHue);
    let on_pixels = frames(&device, &mut on, quality(), 2);
    print_rows("traced off", &off_pixels);
    print_rows("traced on", &on_pixels);
    eprintln!(
        "traced: separation off {:.1}, on {:.1}",
        ground_separation(&off_pixels),
        ground_separation(&on_pixels)
    );
    assert!(
        ground_separation(&off_pixels) > 60.0,
        "the default's spot is the control of this test: {:.1} levels",
        ground_separation(&off_pixels)
    );
    // Traced probes also see the sphere itself, which hides the blue sky from those under it, so
    // their lobe's color there is theirs and not the sky's: 12.2 levels measured on lavapipe, against
    // the default's 202.5, and no channel lost.
    assert_sky_ramp_and_no_spot("traced probes, lobe_hue", &on_pixels, 16.0);
}

fn baked_quality() -> FrameQuality {
    let mut quality = quality();
    quality.gi_probes = false;
    quality.baked_gi = true;
    quality
}

fn baked_renderer(device: &RenderDevice, mode: ProbeSpecular, zh3: bool) -> FrameRenderer {
    baked_renderer_with(device, gradient_volume(), mode, zh3)
}

fn baked_renderer_with(
    device: &RenderDevice,
    volume: Arc<BakedGiVolume>,
    mode: ProbeSpecular,
    zh3: bool,
) -> FrameRenderer {
    let mut renderer = renderer(device);
    renderer.set_baked_gi(Some(volume));
    renderer.set_baked_gi_settings(BakedGiSettings {
        specular: mode,
        zh3,
        ..BakedGiSettings::default()
    });
    renderer
}

/// The same through a baked volume, with its linear lookup and with ZH3: ZH3 rebuilds the six
/// lookup irradiances with a band-2 term, and that term must not bring the spot back.
#[test]
fn baked_probes_reflect_the_ground_without_a_channel_separated_spot() {
    let device = RenderDevice::headless().expect("lavapipe");
    for zh3 in [false, true] {
        let mut off = baked_renderer(&device, ProbeSpecular::Lobe, zh3);
        let off_pixels = frames(&device, &mut off, baked_quality(), 2);
        assert!(off.baked_gi_status().active, "the bake lights the frame");
        let mut on = baked_renderer(&device, ProbeSpecular::LobeHue, zh3);
        let on_pixels = frames(&device, &mut on, baked_quality(), 2);
        print_rows(&format!("baked zh3 {zh3} off"), &off_pixels);
        print_rows(&format!("baked zh3 {zh3} on"), &on_pixels);
        eprintln!(
            "baked zh3 {zh3}: separation off {:.1}, on {:.1}",
            ground_separation(&off_pixels),
            ground_separation(&on_pixels)
        );
        assert!(
            ground_separation(&off_pixels) > 60.0,
            "zh3 {zh3}: the default's spot is the control: {:.1} levels",
            ground_separation(&off_pixels)
        );
        assert_sky_ramp_and_flat_ground(&format!("baked probes (zh3 {zh3}), lobe_hue"), &on_pixels);
    }
}

/// The smallest channel over the largest, worst of the reflected ground's rows: 0 is the defect's
/// separated spot, 1 a neutral row.
fn weakest_channel_share(pixels: &[u8]) -> f32 {
    let (_, bottom) = sphere_rows();
    let mid = H / 2;
    let mut worst = 1.0f32;
    for y in mid + (bottom - mid) / 4..=bottom {
        let c = row(pixels, y);
        worst = worst.min(c[0].min(c[1]).min(c[2]) / c[0].max(c[1]).max(c[2]).max(1.0));
    }
    worst
}

/// The gradient's field is too broad for ZH3's band-2 term (it is L1 exactly there, and the frames
/// above are the same with it on): a sharper field does turn it on, and it keeps every channel too.
#[test]
fn zh3_keeps_every_channel_where_its_band_2_term_is_on() {
    let device = RenderDevice::headless().expect("lavapipe");
    let sharp = l1_volume(1.6);
    let draw = |mode: ProbeSpecular, zh3: bool| {
        let mut r = baked_renderer_with(&device, sharp.clone(), mode, zh3);
        frames(&device, &mut r, baked_quality(), 2)
    };
    let (l1_on, zh3_on, zh3_off) = (
        draw(ProbeSpecular::LobeHue, false),
        draw(ProbeSpecular::LobeHue, true),
        draw(ProbeSpecular::Lobe, true),
    );
    print_rows("sharp zh3 on", &zh3_on);
    print_rows("sharp zh3 default", &zh3_off);
    assert_ne!(l1_on, zh3_on, "the band-2 term is on in this field");
    let (off, on) = (weakest_channel_share(&zh3_off), weakest_channel_share(&zh3_on));
    eprintln!("sharp zh3: weakest channel share off {off:.3}, on {on:.3}");
    assert!(off < 0.05, "the default's spot is the control: {off:.3}");
    assert!(on > 0.3, "every channel stays: {on:.3}");
}

fn green_plane() -> EnvGroundPlane {
    EnvGroundPlane {
        height: -1.5,
        albedo: [0.05, 0.9, 0.05],
        sun_bounce: false,
        ..EnvGroundPlane::default()
    }
}

/// A plane under the sphere reaches the mode's probe lobe: the mode reflects the plane's color in
/// the lower rows, which the same mode without the plane does not show, and leaves the upper rows
/// alone. Until ADR-5716 the default's lobe never saw the plane (the ADR-1103 lobe replaced the
/// reflection the plane was mixed into, and the probes read the image's lower half), so this test
/// compared the mode with the default. Since ADR-5716 (7) a traced probe ray that meets no surface
/// lands on the plane: the default's probes hold the plane too, and its lobe reflects it. The
/// mode is compared with itself without the plane instead (ADR-9231).
#[test]
fn the_ground_plane_reaches_the_probe_lobe() {
    let device = RenderDevice::headless().expect("lavapipe");
    let (top, bottom) = sphere_rows();
    let (mut plain, mut off, mut on) = (renderer(&device), renderer(&device), renderer(&device));
    set_mode(&mut plain, ProbeSpecular::LobeHue);
    set_mode(&mut on, ProbeSpecular::LobeHue);
    off.set_environment_ground(Some(green_plane())).expect("plane");
    on.set_environment_ground(Some(green_plane())).expect("plane");
    let plain_pixels = frames(&device, &mut plain, quality(), 2);
    let off_pixels = frames(&device, &mut off, quality(), 2);
    let on_pixels = frames(&device, &mut on, quality(), 2);
    print_rows("no plane, lobe_hue", &plain_pixels);
    print_rows("plane, default", &off_pixels);
    print_rows("plane, lobe_hue", &on_pixels);
    let greenness = |c: [f32; 3]| c[1] - c[0].max(c[2]);
    let (plain_ground, off_ground, on_ground) = (
        row(&plain_pixels, bottom),
        row(&off_pixels, bottom),
        row(&on_pixels, bottom),
    );
    assert!(
        greenness(on_ground) > 30.0,
        "the plane's green shows in the lower rows: {on_ground:?}"
    );
    assert!(
        greenness(on_ground) > greenness(plain_ground) + 30.0,
        "the plane brings it, not the mode: {on_ground:?} vs {plain_ground:?} without the plane"
    );
    // ADR-5716 (7): the default's probes hold the plane, so its lobe reflects it too. (Not a
    // reference for the mode's level: at the nadir the default's L1 overshoots, the spot of the
    // first test, now green.)
    assert!(
        greenness(off_ground) > 30.0,
        "the default's probes reflect the plane: {off_ground:?}"
    );
    // The reflected sky keeps its level from the zenith down to the row just above the horizon (the
    // sphere's centre row): the plane moves it only through the L1 cross term. The traced probes
    // hold the plane since ADR-5716; a lobe that also took the plane's swap counted it twice and
    // tinted the row above the horizon green by 24 levels (ADR-9232).
    for k in 0..=3 {
        let y = top + (bottom - top) * k / 8;
        let (a, b) = (row(&plain_pixels, y), row(&on_pixels, y));
        for c in 0..3 {
            assert!(
                (a[c] - b[c]).abs() <= 6.0,
                "the sky side moved in channel {c} on row {y}: {a:?} vs {b:?}"
            );
        }
    }
}

/// The default is not touched: asking for `Lobe` by name gives the same bytes as never asking, also
/// after a round trip through the mode, at the same frame number (frames carry a dither), and a frame
/// without probe light has nothing for the mode to change.
#[test]
fn the_default_frame_keeps_its_bytes() {
    let device = RenderDevice::headless().expect("lavapipe");
    let mut never = renderer(&device);
    let mut named = renderer(&device);
    set_mode(&mut named, ProbeSpecular::Lobe);
    assert_eq!(never.ddgi_settings().specular, ProbeSpecular::Lobe, "the default");
    let mut round_trip = renderer(&device);
    set_mode(&mut round_trip, ProbeSpecular::LobeHue);
    let moved = frames(&device, &mut round_trip, quality(), 2);
    let a = frames(&device, &mut never, quality(), 2);
    let b = frames(&device, &mut named, quality(), 2);
    assert_eq!(a, b, "`Lobe` by name is the default");
    assert_ne!(a, moved, "the mode changes this frame");

    let mut no_probes = quality();
    no_probes.gi = GiMode::Ambient;
    no_probes.gi_probes = false;
    no_probes.baked_gi = false;
    let mut plain = renderer(&device);
    let mut hue = renderer(&device);
    set_mode(&mut hue, ProbeSpecular::LobeHue);
    assert_eq!(
        frames(&device, &mut plain, no_probes, 2),
        frames(&device, &mut hue, no_probes, 2),
        "without probe light the mode changes nothing"
    );
}

fn gradient_image() -> StudioEquirect {
    let g = gradient();
    let (w, h) = (256u32, 128u32);
    let mut texels = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let theta = (y as f32 + 0.5) / h as f32 * std::f32::consts::PI;
        for _ in 0..w {
            texels.push(g.radiance([theta.sin(), theta.cos(), 0.0]));
        }
    }
    StudioEquirect::new(w, h, texels).expect("image")
}

fn worst_row_difference(a: &[u8], b: &[u8]) -> (f32, u32) {
    let (top, bottom) = sphere_rows();
    let mut worst = (0.0f32, top);
    for y in top..=bottom {
        let (ca, cb) = (row(a, y), row(b, y));
        let d = (0..3).map(|c| (ca[c] - cb[c]).abs()).fold(0.0, f32::max);
        if d > worst.0 {
            worst = (d, y);
        }
    }
    worst
}

/// The third open defect of `studio_gradient.rs` (34.5 levels, image against gradient) is that
/// test's order, not the environments: it draws the image on frames 1 and 2 of a renderer and the
/// gradient on frames 3 and 4 of the same one, so it compares probes of two ages. Two renderers of
/// the same age agree within its own 8 levels, in both modes, at every settle length measured.
#[test]
fn an_equirect_image_reflects_like_the_gradient_on_renderers_of_the_same_age() {
    let device = RenderDevice::headless().expect("lavapipe");
    for mode in [ProbeSpecular::Lobe, ProbeSpecular::LobeHue] {
        for count in [2u32, 8] {
            let mut a = FrameRenderer::new();
            a.set_environment_authority(EnvironmentAuthority::Studio);
            set_mode(&mut a, mode);
            assert!(a.set_studio_equirect(&device, Some(&gradient_image())));
            let image = frames(&device, &mut a, quality(), count);
            let mut b = renderer(&device);
            set_mode(&mut b, mode);
            let analytic = frames(&device, &mut b, quality(), count);
            let (worst, y) = worst_row_difference(&image, &analytic);
            eprintln!("{mode:?}, {count} frames: image against gradient {worst:.2} levels at row {y}");
            assert!(
                worst <= 8.0,
                "{mode:?}, {count} frames: {worst:.2} levels at row {y}"
            );
        }
    }
}

/// The book's comparison recipe on a still camera with the frame's screen-space effects on: a
/// switch of the mode shows on the next frames, against a renderer that never switched, at the same
/// frame number. A static screen reuses its composite, which must not hold the old mode.
#[test]
fn a_switch_of_the_mode_shows_on_a_still_frame() {
    let device = RenderDevice::headless().expect("lavapipe");
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    let mut stays = renderer(&device);
    let mut switches = renderer(&device);
    frames(&device, &mut stays, q, 12);
    frames(&device, &mut switches, q, 12);
    set_mode(&mut switches, ProbeSpecular::LobeHue);
    let a = frames(&device, &mut stays, q, 3);
    let b = frames(&device, &mut switches, q, 3);
    let (_, bottom) = sphere_rows();
    let (off, on) = (row(&a, bottom), row(&b, bottom));
    eprintln!("still frame: stayed {off:?}, switched {on:?}");
    let moved = a.iter().zip(&b).filter(|(x, y)| x.abs_diff(**y) > 8).count();
    assert!(moved > 500, "{moved} channels moved by more than 8 levels");
}
