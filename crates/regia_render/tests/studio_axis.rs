// SPDX-License-Identifier: MPL-2.0
//! The forward shader's frame-level studio axis (ADR-2308).
//!
//! A frame without a studio feature (no image-light ground plane, local capture or image-light
//! debug view) composes the forward module without the studio extras (`STUDIO_IBL_OFF`). These
//! tests hold that module to the full one's pixels on the default frames (a sunlit day exterior
//! under the physical atmosphere, the product-viewer studio, the sunless gradient studio and the
//! image studio under the studio authority), byte for byte, on whatever adapter runs them; they
//! pin the pipeline count of the axis (the full module's sets exactly double the studio-off ones
//! while a studio feature is on, and nothing more), and they check the `forward_studio` warm: a
//! studio scene warms the other module, a game map without a studio warms nothing.

use regia_render::{
    DirectionalLightUniform, EnvGroundPlane, EnvironmentAuthority, ForwardStudioMode, FrameQuality,
    FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, StudioEquirect, StudioGradient, TextureStreaming, translate_mesh, uv_sphere,
};

const W: u32 = 192;
const H: u32 = 128;

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

/// A floor and a sphere; `sun_lux` 0 is sunless.
fn scene(sun_lux: f32, sphere_metallic: f32) -> LabScene {
    let (sv, si) = uv_sphere(1.0, 48, 24);
    let (fv, fi) = floor_mesh(60.0);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.6, 6.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 0.9,
        near: 0.05,
        far: 250.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: if sun_lux > 0.0 { 14.5 } else { 12.0 },
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 0.98, 0.95], sun_lux),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                metallic: 0.0,
                roughness: 0.8,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.3, 0.2, 1.0],
                metallic: sphere_metallic,
                roughness: 0.35,
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

fn desktop_high() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.texture_streaming = TextureStreaming::OFF;
    quality
}

/// An HDR studio image: a sky ramp with a small hot source, cd/m².
fn studio_image() -> StudioEquirect {
    let (w, h) = (256u32, 128u32);
    let mut texels = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let hot = if (x as i32 - 150).abs() < 2 && (y as i32 - 35).abs() < 2 {
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

fn gradient() -> StudioGradient {
    StudioGradient::new(
        [2400.0, 4200.0, 8000.0],
        [2600.0, 2800.0, 3000.0],
        [600.0, 600.0, 600.0],
    )
}

/// The default frames the axis must not move.
#[derive(Clone, Copy, Debug)]
enum Setup {
    /// A sunlit exterior on Desktop High at its defaults (physical atmosphere, probes, SSGI): no
    /// studio environment at all, what a game map draws.
    DayExterior,
    /// The gradient studio with a sun, Desktop High at its defaults.
    SunlitStudio,
    /// The gradient studio without a sun.
    SunlessStudio,
    /// An HDR image studio under the studio authority (ADR-2016), metal sphere.
    ImageStudio,
}

const SETUPS: [Setup; 4] = [
    Setup::DayExterior,
    Setup::SunlitStudio,
    Setup::SunlessStudio,
    Setup::ImageStudio,
];

fn renderer_for(device: &RenderDevice, setup: Setup, mode: ForwardStudioMode) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_studio_mode(mode);
    match setup {
        Setup::DayExterior => {}
        Setup::SunlitStudio | Setup::SunlessStudio => {
            assert_eq!(renderer.set_studio_gradient(device, Some(&gradient())), Ok(true));
        }
        Setup::ImageStudio => {
            renderer.set_environment_authority(EnvironmentAuthority::Studio);
            assert!(renderer.set_studio_equirect(device, Some(&studio_image())));
        }
    }
    renderer
}

fn lab_for(setup: Setup) -> LabScene {
    match setup {
        Setup::DayExterior => scene(100_000.0, 0.0),
        Setup::SunlitStudio => scene(64_000.0, 0.0),
        Setup::SunlessStudio => scene(0.0, 0.0),
        Setup::ImageStudio => scene(0.0, 1.0),
    }
}

/// Renders `frames` frames (the temporal resolve settles on a still camera) and returns the last.
fn render(device: &RenderDevice, renderer: &mut FrameRenderer, lab: &LabScene, frames: usize) -> Vec<u8> {
    let frame = lab.as_frame_scene_with_quality(desktop_high());
    let mut pixels = Vec::new();
    for _ in 0..frames {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Largest per-channel difference and how many bytes differ.
fn diff(a: &[u8], b: &[u8]) -> (u8, usize) {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).fold((0, 0), |(max, count), (x, y)| {
        let d = x.abs_diff(*y);
        (max.max(d), count + usize::from(d > 0))
    })
}

#[test]
fn the_default_frames_are_byte_identical_under_both_studio_modules() {
    let device = RenderDevice::headless().expect("lavapipe");
    for setup in SETUPS {
        let lab = lab_for(setup);
        let mut auto = renderer_for(&device, setup, ForwardStudioMode::Auto);
        let off = render(&device, &mut auto, &lab, 6);
        let report = auto.forward_studio_report();
        assert!(!report.features.any(), "{setup:?}: no studio feature is on");
        assert!(
            report.studio_off,
            "{setup:?}: the frame composes the studio-off module"
        );
        assert!(report.sets_studio_off > 0, "{setup:?}: {report:?}");
        assert_eq!(report.sets_full, 0, "{setup:?}: no full set is built: {report:?}");
        assert_eq!(report.gpu_color_full, 0, "{setup:?}: {report:?}");

        let mut full = renderer_for(&device, setup, ForwardStudioMode::Full);
        let on = render(&device, &mut full, &lab, 6);
        let report = full.forward_studio_report();
        assert!(
            !report.studio_off,
            "{setup:?}: the full mode composes the full module"
        );
        assert_eq!(report.sets_studio_off, 0, "{setup:?}: {report:?}");

        let (max, count) = diff(&off, &on);
        if (max, count) != (0, 0) {
            // Tell a moved frame from a frame that does not repeat: the same module twice.
            let mut again = renderer_for(&device, setup, ForwardStudioMode::Auto);
            let control = diff(&off, &render(&device, &mut again, &lab, 6));
            panic!(
                "{setup:?}: the studio-off module moved the default frame ({count} bytes differ, up \
                 to {max} levels; the studio-off frame against itself: {control:?}): a block it \
                 drops is not behind a uniform that is off in this frame"
            );
        }
    }
}

fn plane() -> EnvGroundPlane {
    EnvGroundPlane {
        albedo: [0.18; 3],
        radius: Some(80.0),
        ..EnvGroundPlane::default()
    }
}

/// The pipeline-count pin of the axis: a studio feature turned on builds exactly the full twins
/// of the sets the frame had (per-object sets and GPU-driven colour pipelines alike), turning it
/// off again builds nothing, and the frame with the feature on is the full mode's frame.
#[test]
fn a_studio_feature_builds_exactly_the_full_twins_and_off_again_builds_nothing() {
    let device = RenderDevice::headless().expect("lavapipe");
    let lab = lab_for(Setup::ImageStudio);
    let mut renderer = renderer_for(&device, Setup::ImageStudio, ForwardStudioMode::Auto);
    render(&device, &mut renderer, &lab, 2);
    let before = renderer.forward_studio_report();
    assert!(before.studio_off && before.sets_full == 0, "{before:?}");

    renderer.set_environment_ground(Some(plane())).expect("plane");
    assert!(
        renderer.environment_ground_active(),
        "the image light feeds the plane"
    );
    let with_plane = render(&device, &mut renderer, &lab, 6);
    let on = renderer.forward_studio_report();
    assert!(on.features.ground_plane, "{on:?}");
    assert!(
        !on.studio_off,
        "a frame with the ground plane composes the full module"
    );
    assert_eq!(on.sets_studio_off, before.sets_studio_off, "{on:?}");
    assert_eq!(
        on.sets_full, before.sets_studio_off,
        "the axis doubles the sets, no more: {on:?}"
    );
    assert_eq!(on.gpu_color_studio_off, before.gpu_color_studio_off, "{on:?}");
    assert_eq!(on.gpu_color_full, before.gpu_color_studio_off, "{on:?}");

    // The same history as the auto renderer (two frames without the plane, then six with it):
    // the temporal passes carry the first frames into the compared one.
    let mut full = renderer_for(&device, Setup::ImageStudio, ForwardStudioMode::Full);
    render(&device, &mut full, &lab, 2);
    full.set_environment_ground(Some(plane())).expect("plane");
    let full_frame = render(&device, &mut full, &lab, 6);
    assert_eq!(
        diff(&with_plane, &full_frame),
        (0, 0),
        "the plane's frame is the full mode's"
    );

    renderer.set_environment_ground(None).expect("no plane");
    render(&device, &mut renderer, &lab, 1);
    let off_again = renderer.forward_studio_report();
    assert!(off_again.studio_off, "{off_again:?}");
    assert_eq!(
        (off_again.sets_studio_off, off_again.sets_full),
        (on.sets_studio_off, on.sets_full),
        "turning the plane off builds nothing"
    );
    assert_eq!(
        (off_again.gpu_color_studio_off, off_again.gpu_color_full),
        (on.gpu_color_studio_off, on.gpu_color_full)
    );
}

/// The image-light debug view and the full mode are studio features of the axis too.
#[test]
fn the_debug_view_and_the_full_mode_select_the_full_module() {
    let mut renderer = FrameRenderer::new();
    assert_eq!(renderer.forward_studio_mode(), ForwardStudioMode::Auto);
    let report = renderer.forward_studio_report();
    if report.env_full || report.adreno_workaround {
        eprintln!("REGIA_FORWARD_STUDIO=full or the Adreno workaround is on: the axis is fixed");
        return;
    }
    assert!(
        report.studio_off,
        "a renderer without a studio feature: {report:?}"
    );
    renderer.set_image_light_view(regia_render::DebugViewMode::ImageLight);
    assert_eq!(renderer.studio_features().image_light_view, 1);
    assert!(!renderer.forward_studio_report().studio_off);
    renderer.set_image_light_view(regia_render::DebugViewMode::Lit);
    assert!(renderer.forward_studio_report().studio_off);
    renderer.set_forward_studio_mode(ForwardStudioMode::Full);
    assert!(!renderer.forward_studio_report().studio_off);
}

/// `forward_studio`: a renderer with a studio environment warms the other module of every set
/// its warm frame drew, so turning the ground plane on compiles no forward set on the frame
/// path; a game map without a studio warms nothing.
#[test]
fn the_forward_studio_warm_covers_a_studio_and_skips_a_game_map() {
    let device = RenderDevice::headless().expect("lavapipe");

    let lab = lab_for(Setup::DayExterior);
    let mut game = renderer_for(&device, Setup::DayExterior, ForwardStudioMode::Auto);
    render(&device, &mut game, &lab, 1);
    assert_eq!(game.warm_pipeline_key_for_tests(&device, "forward_studio"), 0);
    assert_eq!(game.forward_studio_report().sets_full, 0);

    let lab = lab_for(Setup::ImageStudio);
    let mut studio = renderer_for(&device, Setup::ImageStudio, ForwardStudioMode::Auto);
    render(&device, &mut studio, &lab, 1);
    let drawn = studio.forward_studio_report();
    assert!(studio.warm_pipeline_key_for_tests(&device, "forward_studio") >= 1);
    let warmed = studio.forward_studio_report();
    assert_eq!(warmed.sets_full, drawn.sets_studio_off, "{warmed:?}");
    assert_eq!(warmed.gpu_color_full, drawn.gpu_color_studio_off, "{warmed:?}");

    studio.set_environment_ground(Some(plane())).expect("plane");
    assert!(
        studio.environment_ground_active(),
        "the image light feeds the plane"
    );
    render(&device, &mut studio, &lab, 1);
    let on = studio.forward_studio_report();
    assert!(!on.studio_off);
    assert_eq!(
        (
            on.sets_studio_off,
            on.sets_full,
            on.gpu_color_studio_off,
            on.gpu_color_full
        ),
        (
            warmed.sets_studio_off,
            warmed.sets_full,
            warmed.gpu_color_studio_off,
            warmed.gpu_color_full
        ),
        "the warm built every forward pipeline the plane's frame binds"
    );
}
