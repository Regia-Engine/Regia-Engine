// SPDX-License-Identifier: MPL-2.0
//! Studio frames hold their pixels across engine releases (ADR-1966).
//!
//! A game team pinned a release after its studio frames moved between v0.2.7 and v0.2.8. The
//! engine had not changed them: the frames below are byte-identical at both tags on lavapipe,
//! and the shift came from the team's own studio parameters. These tests pin the two studio
//! set-ups so an engine change that moves a studio frame fails here, by name, instead of being
//! found downstream.
//!
//! - `a_sunlit_studio_frame_keeps_its_pixels`: a studio frame as a product viewer draws it, built
//!   from the same public calls: `set_studio_gradient`, a 64,000 lux sun with cascaded shadows,
//!   height fog in the horizon colour, the AgX post at a fixed exposure, and Desktop High with
//!   the atmosphere, clouds, probes and baked GI switched off.
//! - `a_sunless_studio_frame_keeps_its_pixels`: the gradient alone (no sun) on a floor and a
//!   sphere, Desktop High at its defaults, fixed exposure.
//!
//! The expected values were measured on the build server's lavapipe (llvmpipe) at v0.2.7 (e4defa90d)
//! and v0.2.8 (18bfbe1bb), identical at both; a level of slack covers rounding. Other adapters
//! print the values and skip the compare. If a change moves these frames on purpose, update the
//! values in the same commit and say why in its message, as for a golden image.
//!
//! Desktop High has the physical atmosphere on, and on such a frame its LUT sky and IBL bake are
//! drawn over an installed studio environment: that mix is the default frame, pinned above, and
//! it is what a game team fitted its exposure on, so it does not change (ADR-2016). The other
//! behaviour, the installed environment as the frame's sky and image light (ADR-2000 (0)), is
//! opt-in through `FrameRenderer::set_environment_authority(EnvironmentAuthority::Studio)`;
//! `the_studio_authority_frames_keep_their_pixels` pins it, measured on the build server's lavapipe at
//! 2f8116ebf (studio-lighting) and the v0.2.10 candidate; its two sunless spheres were re-pinned
//! at the v0.5.0 candidate cf2e54363 (ADR-5716 (7), owner sign-off 2026-11-13, ADR-7552).
//!
//! `a_sunless_image_studio_frame_keeps_its_pixels` is the same set-up lit by an HDR equirect image
//! at the default 256 base, measured at v0.2.8 (ADR-2016 keeps that bake).
//!
//! `REGIA_STUDIO_PIN_DUMP=<dir>` writes each frame's RGBA bytes (192 × 128) to
//! `<dir>/<frame>.rgba`, for a byte comparison against another build.

use regia_render::{
    DirectionalLightUniform, EnvironmentAuthority, FogSettings, FrameQuality, FrameRenderer, GiMode,
    LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, StudioEquirect, StudioGradient, TextureStreaming, TonemapOperator,
    exposure_from_ev100, translate_mesh, uv_sphere,
};

const W: u32 = 192;
const H: u32 = 128;
/// The sunless frame lit by `studio_image`, measured on lavapipe at v0.2.8 (18bfbe1bb) and at
/// ADR-2016: zenith side, horizon, floor, sphere, frame mean. The sky is the atmosphere's, as in
/// the gradient frame.
const IMAGE_ZENITH: [u8; 3] = [171, 207, 242];
const IMAGE_HORIZON: [u8; 3] = [231, 237, 242];
const IMAGE_FLOOR: [u8; 3] = [150, 146, 166];
// The sphere re-pinned at ADR-5716 (7) (it catches the floor's bounce light), owner approved 2026-11-12.
const IMAGE_SPHERE: [u8; 3] = [165, 85, 80];
const IMAGE_MEAN: [u8; 3] = [168, 175, 196];
/// Largest per-channel difference from the measured value that still passes, levels.
const SLACK: i32 = 2;

fn floor_mesh(half: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let v = |x: f32, z: f32| LabVertex {
        position: [x, 0.0, z],
        normal: [0.0, 1.0, 0.0],
        uv: [x / half, z / half],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [1.0, 0.0, 0.0, 0.0],
    };
    (
        vec![v(-half, -half), v(half, -half), v(half, half), v(-half, half)],
        vec![0, 2, 1, 0, 3, 2],
    )
}

fn scene(
    eye: [f32; 3],
    target: [f32; 3],
    floor: [f32; 3],
    sphere: [f32; 3],
    sphere_roughness: f32,
) -> LabScene {
    let (sv, si) = uv_sphere(1.0, 64, 32);
    let (fv, fi) = floor_mesh(60.0);
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.9,
        near: 0.05,
        far: 250.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [floor[0], floor[1], floor[2], 1.0],
                metallic: 0.0,
                roughness: 0.8,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [sphere[0], sphere[1], sphere[2], 1.0],
                metallic: 0.0,
                roughness: sphere_roughness,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: fv,
                indices: fi,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&sv, [0.0, 1.0, 0.0]),
                indices: si,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn pixel(p: &[u8], x: u32, y: u32) -> [u8; 3] {
    let i = ((y * W + x) * 4) as usize;
    [p[i], p[i + 1], p[i + 2]]
}

fn mean(p: &[u8]) -> [u8; 3] {
    let mut sum = [0u64; 3];
    for texel in p.as_chunks::<4>().0 {
        for (s, v) in sum.iter_mut().zip(texel) {
            *s += u64::from(*v);
        }
    }
    let n = u64::from(W * H);
    sum.map(|s| u8::try_from(s / n).unwrap_or(u8::MAX))
}

/// Writes the frame's bytes to `$REGIA_STUDIO_PIN_DUMP/<name>.rgba` when that is set.
fn dump(name: &str, p: &[u8]) {
    let Some(dir) = std::env::var_os("REGIA_STUDIO_PIN_DUMP") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump dir");
    let tmp = dir.join(format!("{name}.rgba.{}.tmp", std::process::id()));
    std::fs::write(&tmp, p).expect("dump");
    std::fs::rename(&tmp, dir.join(format!("{name}.rgba"))).expect("dump rename");
}

/// Compares the named samples with their measured values on lavapipe; prints them everywhere.
fn check(device: &RenderDevice, frame: &str, measured_at: &str, samples: &[(&str, [u8; 3], [u8; 3])]) {
    for (name, got, want) in samples {
        eprintln!("{frame}: {name} {got:?} (measured {want:?})");
    }
    if device.adapter.get_info().device_type != wgpu::DeviceType::Cpu {
        eprintln!("{frame}: hardware adapter; the values are measured on lavapipe, compare skipped");
        return;
    }
    for (name, got, want) in samples {
        let off = got
            .iter()
            .zip(want)
            .map(|(g, w)| (i32::from(*g) - i32::from(*w)).abs())
            .max()
            .unwrap_or(0);
        assert!(
            off <= SLACK,
            "{frame}: {name} is {got:?}, measured {want:?} at {measured_at} ({off} levels off). \
             An engine change moved a studio frame: if that is intended, update the value and say \
             why in the commit message"
        );
    }
}

fn render(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    frame: &regia_render::FrameScene<'_>,
) -> Vec<u8> {
    // Enough frames for the temporal resolve to settle on a still camera.
    let mut pixels = Vec::new();
    for _ in 0..8 {
        pixels = renderer.render_offscreen(device, frame).expect("frame").pixels;
    }
    pixels
}

/// The product-viewer frame (atmosphere off). `authority` is set first unless `None`, which
/// leaves every setting at its default.
fn sunlit_frame(device: &RenderDevice, authority: Option<EnvironmentAuthority>) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    if let Some(authority) = authority {
        renderer.set_environment_authority(authority);
    }
    // A cyclorama ramp from a sky colour (cd/m²): deeper overhead, paler toward the horizon, a
    // gray floor bounce below.
    let sky = [1_700.0f32, 2_100.0, 2_800.0];
    let luminance = 0.2126 * sky[0] + 0.7152 * sky[1] + 0.0722 * sky[2];
    let zenith = sky.map(|v| v * 0.85);
    let horizon = sky.map(|v| (0.6 * v + 0.4 * luminance) * 1.1);
    let gradient = StudioGradient {
        falloff: 1.0,
        ..StudioGradient::new(zenith, horizon, [830.0; 3])
    };
    assert_eq!(renderer.set_studio_gradient(device, Some(&gradient)), Ok(true));
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.physical_atmosphere = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.cloud_reprojection = false;
    quality.gi = GiMode::Ambient;
    quality.gi_probes = false;
    quality.screen_space_gi = false;
    quality.baked_gi = false;
    quality.reflection_probes = false;
    quality.texture_streaming = TextureStreaming::OFF;
    quality.emissive_lights = 0;
    quality.horizontal_plus = false;
    let exposure_ev = 14.3;
    let post = PostSettings {
        enabled: true,
        bloom: true,
        auto_exposure: false,
        exposure_ev,
        exposure_compensation: 1.6,
        tonemap: TonemapOperator::AgX,
        taa: false,
        fxaa: false,
        vignette: 0.0,
        film_grain: 0.0,
        ..PostSettings::default()
    };
    // The sun at 66° elevation, 35° around.
    let (elevation, azimuth) = (66.0f32.to_radians(), 35.0f32.to_radians());
    let level = elevation.cos();
    let mut lab = scene([0.0, 1.6, 7.0], [0.0, 1.45, 6.0], [0.35; 3], [0.18; 3], 0.5);
    lab.exposure_ev = exposure_ev;
    lab.ambient_lux = 6_500.0;
    lab.directional = DirectionalLightUniform::new(
        [-level * azimuth.sin(), -elevation.sin(), level * azimuth.cos()],
        [1.0, 0.98, 0.95],
        64_000.0,
    );
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    frame.fog = FogSettings {
        enabled: true,
        density: 0.005,
        height_falloff: 0.06,
        base_height: 0.0,
        color: horizon.map(|v| v * exposure_from_ev100(exposure_ev)),
        sky_inscatter: false,
    };
    render(device, &mut renderer, &frame)
}

/// The gradient alone on Desktop High at its defaults (the physical atmosphere on).
fn sunless_frame(device: &RenderDevice, authority: Option<EnvironmentAuthority>) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    if let Some(authority) = authority {
        renderer.set_environment_authority(authority);
    }
    let gradient = StudioGradient::new(
        [2400.0, 4200.0, 8000.0],
        [2600.0, 2800.0, 3000.0],
        [600.0, 600.0, 600.0],
    );
    assert_eq!(renderer.set_studio_gradient(device, Some(&gradient)), Ok(true));
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut lab = scene([0.0, 1.6, 6.0], [0.0, 1.2, 0.0], [0.5; 3], [0.8, 0.3, 0.2], 0.4);
    lab.post = Some(PostSettings {
        auto_exposure: false,
        exposure_ev: 12.0,
        ..PostSettings::default()
    });
    let frame = lab.as_frame_scene_with_quality(quality);
    render(device, &mut renderer, &frame)
}

/// An HDR studio image: a sky ramp with a small hot source, cd/m².
fn studio_image() -> StudioEquirect {
    let (w, h) = (512u32, 256u32);
    let mut texels = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let hot = if (x as i32 - 300).abs() < 3 && (y as i32 - 70).abs() < 3 {
                60_000.0
            } else {
                0.0
            };
            texels.push([
                500.0 + 1500.0 * (1.0 - fy) + 300.0 * fx + hot,
                700.0 + 1200.0 * (1.0 - fy) + hot,
                900.0 + 2500.0 * (1.0 - fy) * (1.0 - fy) + hot,
            ]);
        }
    }
    StudioEquirect::new(w, h, texels).expect("image")
}

/// The sunless set-up lit by an HDR image instead of the gradient, Desktop High at its defaults.
fn sunless_image_frame(device: &RenderDevice, authority: Option<EnvironmentAuthority>) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    if let Some(authority) = authority {
        renderer.set_environment_authority(authority);
    }
    assert!(renderer.set_studio_equirect(device, Some(&studio_image())));
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut lab = scene([0.0, 1.6, 6.0], [0.0, 1.2, 0.0], [0.5; 3], [0.8, 0.3, 0.2], 0.4);
    lab.post = Some(PostSettings {
        auto_exposure: false,
        exposure_ev: 12.0,
        ..PostSettings::default()
    });
    let frame = lab.as_frame_scene_with_quality(quality);
    render(device, &mut renderer, &frame)
}

fn sunlit_samples(p: &[u8]) -> Vec<(&'static str, [u8; 3], [u8; 3])> {
    vec![
        ("sky", pixel(p, 20, 6), [139, 149, 165]),
        ("above the horizon", pixel(p, 20, 40), [143, 152, 166]),
        ("horizon", pixel(p, 20, 52), [202, 202, 203]),
        ("floor", pixel(p, 20, 120), [205, 205, 205]),
        ("far floor", pixel(p, 150, 80), [206, 206, 206]),
        ("gray card sphere", pixel(p, 96, 70), [45, 48, 54]),
        ("frame mean", mean(p), [174, 177, 183]),
    ]
}

#[test]
fn a_sunlit_studio_frame_keeps_its_pixels() {
    let device = RenderDevice::headless().expect("lavapipe");
    let p = sunlit_frame(&device, None);
    dump("sunlit", &p);
    check(&device, "sunlit studio", "v0.2.7 and v0.2.8", &sunlit_samples(&p));
}

#[test]
fn a_sunless_studio_frame_keeps_its_pixels() {
    let device = RenderDevice::headless().expect("lavapipe");
    let p = sunless_frame(&device, None);
    dump("sunless", &p);
    check(
        &device,
        "sunless studio",
        "v0.2.7 and v0.2.8",
        &[
            ("zenith side", pixel(&p, 20, 8), [171, 207, 242]),
            ("horizon", pixel(&p, 20, 56), [231, 237, 242]),
            ("floor", pixel(&p, 20, 120), [157, 176, 204]),
            ("sphere", pixel(&p, 96, 70), [172, 121, 124]), // ADR-5716 (7), owner approved 2026-11-12
            ("frame mean", mean(&p), [171, 191, 217]),
        ],
    );
}

#[test]
fn a_sunless_image_studio_frame_keeps_its_pixels() {
    let device = RenderDevice::headless().expect("lavapipe");
    let p = sunless_image_frame(&device, None);
    dump("sunless-image", &p);
    check(
        &device,
        "sunless image studio",
        "v0.2.8",
        &[
            ("zenith side", pixel(&p, 20, 8), IMAGE_ZENITH),
            ("horizon", pixel(&p, 20, 56), IMAGE_HORIZON),
            ("floor", pixel(&p, 20, 120), IMAGE_FLOOR),
            ("sphere", pixel(&p, 96, 70), IMAGE_SPHERE),
            ("frame mean", mean(&p), IMAGE_MEAN),
        ],
    );
}

/// The opt-in (ADR-2000 (0), ADR-2016): with `EnvironmentAuthority::Studio` the gradient is the
/// sunless frame's sky and image light. The sunlit frame has the atmosphere off, so the setting
/// does not reach it.
#[test]
fn the_studio_authority_frames_keep_their_pixels() {
    let device = RenderDevice::headless().expect("lavapipe");
    let p = sunless_frame(&device, Some(EnvironmentAuthority::Studio));
    dump("sunless-studio-authority", &p);
    check(
        &device,
        "sunless studio, studio authority",
        "2f8116ebf (studio-lighting) and the v0.2.10 candidate",
        &[
            ("zenith side", pixel(&p, 20, 8), [178, 193, 214]),
            ("horizon", pixel(&p, 20, 56), [158, 163, 168]),
            ("floor", pixel(&p, 20, 120), [143, 170, 205]),
            ("sphere", pixel(&p, 96, 70), [167, 122, 130]), // ADR-5716 (7), owner sign-off 2026-11-13
            ("frame mean", mean(&p), [159, 175, 200]),
        ],
    );
    let p = sunless_image_frame(&device, Some(EnvironmentAuthority::Studio));
    dump("sunless-image-studio-authority", &p);
    check(
        &device,
        "sunless image studio, studio authority",
        "ADR-2016 (the v0.2.10 candidate)",
        &[
            ("zenith side", pixel(&p, 20, 8), [154, 152, 170]),
            ("horizon", pixel(&p, 20, 56), [147, 147, 159]),
            ("floor", pixel(&p, 20, 120), [131, 123, 152]),
            ("sphere", pixel(&p, 96, 70), [159, 81, 80]), // ADR-5716 (7), owner sign-off 2026-11-13
            ("frame mean", mean(&p), [142, 133, 154]),
        ],
    );
    let p = sunlit_frame(&device, Some(EnvironmentAuthority::Studio));
    dump("sunlit-studio-authority", &p);
    check(
        &device,
        "sunlit studio, studio authority",
        "v0.2.7 and v0.2.8",
        &sunlit_samples(&p),
    );
}

/// Setting the default by hand changes no byte, and the opt-in changes the sunless frame, on
/// any adapter.
#[test]
fn the_atmosphere_authority_is_the_default_and_the_opt_in_differs() {
    let device = RenderDevice::headless().expect("lavapipe");
    assert_eq!(
        FrameRenderer::new().environment_authority(),
        EnvironmentAuthority::Atmosphere
    );
    assert_eq!(EnvironmentAuthority::default(), EnvironmentAuthority::Atmosphere);
    let untouched = sunless_frame(&device, None);
    let explicit = sunless_frame(&device, Some(EnvironmentAuthority::Atmosphere));
    assert!(untouched == explicit, "the explicit default moved a byte");
    let studio = sunless_frame(&device, Some(EnvironmentAuthority::Studio));
    assert!(
        untouched != studio,
        "the studio authority changed nothing on a frame with the atmosphere"
    );
}
