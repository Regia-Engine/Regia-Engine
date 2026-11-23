// SPDX-License-Identifier: MPL-2.0
//! Engine-shader hot reload end to end (`docs/specs/12-assets.md` § 8): a WGSL file edited on
//! disk is validated, committed and picked up by an existing renderer within one second, and the
//! rendered pixels change. A broken edit is reported and the previous shader keeps rendering.
//!
//! The test works on a scratch copy of `utils.wgsl` (registered as an engine shader directory),
//! never on the checkout's own file.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetReloadInbox, AssetServerSettings, AssetWatchStatus, SourceKind};
use regia_render::shader_reload::{ShaderReloadJobs, clear_shader_overrides, shader_epoch};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, RenderDevice, RenderPlugin,
};

const UTILS: &str = include_str!("../shaders/utils.wgsl");

fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

/// Mean brightness of the centre pixel of a lit grey quad.
fn render(renderer: &mut FrameRenderer, device: &RenderDevice) -> f32 {
    let vertices = quad();
    let indices = [0u32, 2, 1, 0, 3, 2];
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        ..LabMaterial::default()
    };
    let scene = FrameScene {
        width: 32,
        height: 32,
        eye: [0.0, 4.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.8),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &indices,
            material: &material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: regia_render::LOD_DITHER_NONE,
        }],
    };
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    let i = ((16 * 32 + 16) * 4) as usize;
    (f32::from(pixels[i]) + f32::from(pixels[i + 1]) + f32::from(pixels[i + 2])) / 3.0
}

fn write_atomically(path: &Path, text: &str) {
    let tmp = path.with_file_name(".regia-tmp-utils");
    std::fs::write(&tmp, text).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

#[test]
fn editing_an_engine_shader_swaps_pipelines_within_one_second_and_errors_keep_the_old_one() {
    let device = RenderDevice::headless().unwrap_or_else(|e| panic!("no headless GPU (lavapipe?): {e}"));
    let dir: PathBuf = std::env::temp_dir().join(format!("regia_shader_reload_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let utils = dir.join("utils.wgsl");
    std::fs::write(&utils, UTILS).expect("copy");

    let (tx, inbox) = AssetReloadInbox::channel();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(inbox);
    app.add_plugins(AssetPlugin {
        settings: AssetServerSettings {
            project_root: dir.clone(),
            platform: "linux".into(),
            content_dir: None,
        },
    });
    app.add_plugins(RenderPlugin { headless: true });
    app.world_mut().resource_mut::<ShaderReloadJobs>().engine_dirs = vec![dir.clone()];
    let _watch = regia_asset::hot_reload::spawn_source_forwarder(
        std::slice::from_ref(&dir),
        Duration::from_millis(80),
        |kind| kind == SourceKind::Shader,
        tx,
    )
    .expect("watch");
    app.update();

    let mut renderer = FrameRenderer::new();
    let before = render(&mut renderer, &device);
    assert!(before > 30.0, "the quad is lit: {before}");

    // A valid edit: halve the display transform's output.
    let epoch = shader_epoch();
    let edited = UTILS.replacen(
        "fn default_display_transform(color: vec3<f32>) -> vec3<f32> {",
        "fn default_display_transform(color: vec3<f32>) -> vec3<f32> {\n    return 0.5 * default_display_transform_unscaled(color);\n}\n\nfn default_display_transform_unscaled(color: vec3<f32>) -> vec3<f32> {",
        1,
    );
    assert_ne!(edited, UTILS, "edit point exists");
    let start = Instant::now();
    write_atomically(&utils, &edited);
    while shader_epoch() == epoch && start.elapsed() < Duration::from_secs(10) {
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    let took = start.elapsed();
    assert!(
        shader_epoch() > epoch,
        "edit never committed: {:?}",
        app.world().resource::<ShaderReloadJobs>().last_error
    );
    let after = render(&mut renderer, &device);
    eprintln!("shader swap in {took:?}: center {before} -> {after}");
    assert_reload_budget(took, "shader swap");
    assert!(
        after < before * 0.75,
        "the same renderer draws with the edited shader: {before} -> {after}"
    );

    // A broken edit: reported, not committed, the edited shader keeps rendering.
    let epoch = shader_epoch();
    let errors = app.world().resource::<AssetWatchStatus>().errors;
    let start = Instant::now();
    write_atomically(&utils, &edited.replacen("fn saturate", "fn saturate saturate", 1));
    while app.world().resource::<AssetWatchStatus>().errors == errors
        && start.elapsed() < Duration::from_secs(10)
    {
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(
        app.world().resource::<AssetWatchStatus>().errors,
        errors,
        "the broken shader is reported"
    );
    assert_reload_budget(start.elapsed(), "shader error report");
    assert_eq!(shader_epoch(), epoch, "a broken shader must not swap");
    let line = app
        .world()
        .resource::<AssetWatchStatus>()
        .last_line()
        .unwrap_or_default();
    assert!(line.starts_with("cook error in "), "{line}");
    assert!(app.world().resource::<ShaderReloadJobs>().last_error.is_some());
    let kept = render(&mut renderer, &device);
    assert!(
        (kept - after).abs() < 1.0,
        "old pipelines kept: {after} -> {kept}"
    );

    clear_shader_overrides();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The spec's one-second reload budget, asserted in release builds only: a debug test binary on a
/// loaded machine measures the scheduler, not the reload (audit-v035-flaky H1). Debug runs print it.
fn assert_reload_budget(took: Duration, what: &str) {
    eprintln!("{what} took {took:?}");
    if !cfg!(debug_assertions) {
        assert!(took < Duration::from_secs(1), "{what} took {took:?}");
    }
}
