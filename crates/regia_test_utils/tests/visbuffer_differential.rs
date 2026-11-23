// SPDX-License-Identifier: MPL-2.0
//! The visibility-buffer path against the classic path on the golden scenes (ADR-2071).
//!
//! Each scene draws three ways on one adapter: through the classic forward path, through the
//! visibility buffer with its per-material resolve, and through the single-pass resolve on a device
//! with the texture table. The classic and visibility frames must agree within a tolerance (their
//! rasterizers and their LOD cuts differ a little by design); the two resolves must agree exactly.
//!
//! These frames are not the goldens themselves: the path needs a post stack (linear HDR), so each
//! scene takes `PostSettings::golden_deterministic` on all three. No golden image is read or written.
//!
//! Beside the golden scenes, `skinned` mixes forward skinned meshes into the visibility frame
//! (ADR-2689). The outdoor scene (terrain, sky, fog) needs the terrain crate and is
//! `regia_terrain/tests/visbuffer_outdoor.rs`.

use std::sync::Arc;

use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::visbuffer::{MeshletBuilder, MeshletMesh};
use regia_render::{
    FrameQuality, FrameRenderer, JointMatrix, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings,
    RenderDevice, RenderError, VisibilityPath, VisibilitySelect,
};
use regia_test_utils::golden::ssim_rgba8;
use regia_test_utils::scene_loader::{LabView, ensure_model, load_gltf_model_with_view};
use regia_test_utils::scenes::{
    GOLDEN_HEIGHT, GOLDEN_WIDTH, alpha_modes_scene, clustered_lights_scene, ibl_spheres_scene,
    light_shadows_scene, material_sphere_grid_scene, post_demo_scene, primitives_row_scene,
    surface_maps_scene,
};

/// The cooker's cluster hierarchy, as a game's cooked meshes have it.
fn cooked_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        };
        build_meshlet_mesh_with_attributes(&positions, Some(attributes), indices, &MeshletSettings::default())
            .ok()
    })
}

/// A single level for every mesh: rasterizer and resolve differences only, no LOD cut.
fn single_level_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

fn renderer_on(device: &RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    // llvmpipe spaces anisotropic taps by the coordinate's implicit quad derivatives, which a
    // full-screen resolve cannot give it (`visbuffer::tests::comparison_renderer`). Hardware
    // filters from the explicit gradients and keeps the shipped sampler.
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        renderer.set_material_anisotropy_for_tests(1);
    }
    renderer
}

/// `scene`'s third frame (a fresh renderer's first frames still settle) through `path`, or the
/// classic path when `path` is `None`, and how many instances took the visibility path.
fn frame(
    device: &RenderDevice,
    scene: &LabScene,
    path: Option<VisibilityPath>,
    builder: Option<MeshletBuilder>,
) -> Result<(Vec<u8>, FrameRenderer), RenderError> {
    let mut renderer = renderer_on(device);
    renderer.set_meshlet_builder(builder);
    let mut quality = FrameQuality::LAB;
    quality.visibility_buffer = path.is_some();
    if let Some(path) = path {
        quality.visibility_path = path;
    }
    let frame_scene = scene.as_frame_scene_with_quality(quality);
    renderer.render_offscreen(device, &frame_scene)?;
    renderer.render_offscreen(device, &frame_scene)?;
    let pixels = renderer.render_offscreen(device, &frame_scene)?.pixels;
    Ok((pixels, renderer))
}

/// (largest channel difference, fraction of pixels whose channels all differ by at most 2).
fn diff(a: &[u8], b: &[u8]) -> (u8, f64) {
    let mut worst = 0u8;
    let mut close = 0usize;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        worst = worst.max(d);
        close += usize::from(d <= 2);
    }
    (worst, close as f64 / (a.len() / 4).max(1) as f64)
}

/// A frame a comparison can mean something on: most pixels hold shading information in every
/// channel, and the frame is not one flat colour. Saturated or blank frames compare as equal
/// whatever the paths do, so the test must fail on them, not pass.
fn frame_problem(pixels: &[u8]) -> Option<String> {
    let texels = pixels.as_chunks::<4>().0;
    let clipped = texels.iter().filter(|p| p[..3].iter().any(|c| *c >= 250)).count();
    let unclipped = 1.0 - clipped as f64 / texels.len().max(1) as f64;
    if unclipped < 0.6 {
        return Some(format!(
            "only {:.1} % of the pixels are unclipped",
            unclipped * 100.0
        ));
    }
    let mut low = [255u8; 3];
    let mut high = [0u8; 3];
    for p in texels {
        for c in 0..3 {
            low[c] = low[c].min(p[c]);
            high[c] = high[c].max(p[c]);
        }
    }
    let range = (0..3).map(|c| high[c] - low[c]).max().unwrap_or(0);
    (range < 32).then(|| format!("the frame is nearly flat: channel range {range}"))
}

/// Writes `pixels` under `REGIA_VIS_DIFF_DIR` when it is set, for a look by eye.
fn dump(name: &str, pixels: &[u8], width: u32, height: u32) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_VIS_DIFF_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{name}.png"));
    if let Err(err) = image::save_buffer(&path, pixels, width, height, image::ColorType::Rgba8) {
        eprintln!("{}: {err}", path.display());
    }
}

/// What a scene's frames must meet. `close` is the share of pixels within 2 levels and `ssim` the
/// structural similarity. Metal rounds rasterizer edges differently from the forward draw
/// (ADR-275), so the floors there sit lower, where a wrong or blank frame still fails.
#[derive(Clone, Copy)]
struct Tolerance {
    close: f64,
    ssim: f64,
}

/// Scenes every path draws the same way, up to edge pixels.
const STRICT: Tolerance = Tolerance {
    close: 0.985,
    ssim: 0.985,
};

/// Two bent arms (a box skinned to two joints, the upper half turned about the elbow) standing in
/// the shadowed golden scene, in front of its sphere and in the spot light (ADR-2689). Skinned
/// meshes stay on the forward path in both frames, so this is the mixed frame: forward skinned
/// draws over, under and shadowing the visibility path's rigid ones.
fn skinned_scene() -> LabScene {
    use regia_core::glam::{Mat4, Vec3};
    let mut scene = light_shadows_scene();
    let (vertices, indices) = regia_render::box_mesh([0.09, 0.45, 0.09]);
    // The palette carries the placement (the model stays identity, as for a palette in world space).
    let arm = |x: f32, z: f32, bend: f32| {
        let vertices: Vec<LabVertex> = vertices
            .iter()
            .map(|v| {
                // Lower half on joint 0, upper half on joint 1, blended across the elbow.
                let t = ((v.position[1] + 0.12) / 0.24).clamp(0.0, 1.0);
                LabVertex {
                    position: [v.position[0], v.position[1] + 0.45, v.position[2]],
                    joints: [0, 1, 0, 0],
                    weights: [1.0 - t, t, 0.0, 0.0],
                    ..*v
                }
            })
            .collect();
        // Joint 1 turns about the elbow (0, 0.45, 0) by `bend` about Z.
        let elbow = Vec3::new(0.0, 0.45, 0.0);
        let place = Mat4::from_translation(Vec3::new(x, 0.0, z));
        let turn =
            Mat4::from_translation(elbow) * Mat4::from_rotation_z(bend) * Mat4::from_translation(-elbow);
        (
            vertices,
            vec![
                JointMatrix::from_mat4(place),
                JointMatrix::from_mat4(place * turn),
            ],
        )
    };
    for (x, z, bend) in [(0.7, 0.55, 0.6), (-0.65, 0.45, -0.8)] {
        let (vertices, joints) = arm(x, z, bend);
        scene.meshes.push(LabMesh {
            vertices,
            indices: indices.clone(),
            material: 0,
            joints,
            ..LabMesh::unskinned()
        });
    }
    scene
}

/// A scene, its name and what it must meet.
fn scenes() -> Vec<(&'static str, LabScene, Tolerance)> {
    let hdr = |mut scene: LabScene| {
        scene.depth_prepass = true;
        // The post stack's exposure replaces the scene's: left at its default, a daylight scene
        // tonemaps to near white and every comparison passes on saturated pixels.
        let exposure_ev = scene.exposure_ev;
        scene
            .post
            .get_or_insert_with(PostSettings::golden_deterministic)
            .exposure_ev = exposure_ev;
        scene
    };
    // The full-scene golden, where the model is on this machine (`golden_sponza` skips the same
    // way): textured, masked and many materials. Its few hundred differing pixels are stripes on
    // the pillars, where this sun direction leaves the shadow test on an exact tie and rounding
    // decides it: nudged by 1e-4 the sun moves classic against itself by over a thousand pixels,
    // and classic against the path falls to a handful (ADR-2071).
    let sponza = ensure_model("Sponza")
        .ok()
        .and_then(|path| load_gltf_model_with_view(&path, LabView::SponzaAtrium, None).ok())
        .map(|mut scene| {
            scene.width = GOLDEN_WIDTH;
            scene.height = GOLDEN_HEIGHT;
            ("sponza", hdr(scene), STRICT)
        });
    if sponza.is_none() {
        eprintln!("the Sponza model is not on this machine: its row is skipped");
    }
    let mut scenes = vec![
        ("material_sphere_grid", hdr(material_sphere_grid_scene()), STRICT),
        ("clustered_lights", hdr(clustered_lights_scene()), STRICT),
        ("light_shadows", hdr(light_shadows_scene()), STRICT),
        ("surface_maps", hdr(surface_maps_scene()), STRICT),
        ("alpha_modes", hdr(alpha_modes_scene()), STRICT),
        ("ibl_spheres", hdr(ibl_spheres_scene()), STRICT),
        ("post_stack", hdr(post_demo_scene(None)), STRICT),
        ("primitives_row", hdr(primitives_row_scene()), STRICT),
        ("skinned", hdr(skinned_scene()), STRICT),
    ];
    scenes.extend(sponza);
    scenes
}

const SINGLE_PASS: VisibilityPath = VisibilityPath {
    select: VisibilitySelect::Always,
    single_pass_resolve: true,
};

/// The level request is process-wide: only one test at a time may create a device.
static DEVICE_CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device_at(level: u32) -> Result<RenderDevice, RenderError> {
    let _guard = DEVICE_CREATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    regia_render::texture_table::set_device_level_request(level);
    let device = RenderDevice::headless();
    regia_render::texture_table::set_device_level_request(0);
    device
}

#[test]
fn the_visibility_path_agrees_with_the_classic_path_on_the_golden_scenes() {
    let device = match device_at(0) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    let metal = device.capabilities.backend == wgpu::Backend::Metal;
    let mut failures = Vec::new();
    for (name, scene, tolerance) in scenes() {
        let floor = |value: f64| if metal { value.min(0.97) } else { value };
        let (classic, _) = frame(&device, &scene, None, None).expect("classic frame");
        dump(&format!("{name}_classic"), &classic, scene.width, scene.height);
        if let Some(problem) = frame_problem(&classic) {
            failures.push(format!("{name}: the classic frame cannot be compared: {problem}"));
        }
        for (builder_name, builder) in [
            ("single_level", single_level_builder()),
            ("cooked", cooked_builder()),
        ] {
            let (pixels, renderer) = frame(&device, &scene, Some(VisibilityPath::DEFAULT), Some(builder))
                .expect("visibility frame");
            dump(
                &format!("{name}_{builder_name}"),
                &pixels,
                scene.width,
                scene.height,
            );
            let stats = renderer.visbuffer_stats();
            let (worst, close) = diff(&classic, &pixels);
            let ssim = ssim_rgba8(&classic, &pixels, scene.width, scene.height).expect("ssim");
            println!(
                "{name:>22} {builder_name:>12}: {:.3} % within 2 levels (worst {worst}), ssim {ssim:.4}, instances {}, materials {}",
                close * 100.0,
                stats.instances,
                stats.materials,
            );
            if !renderer.visbuffer_active() || stats.instances == 0 {
                failures.push(format!("{name} {builder_name}: nothing took the path"));
            }
            // Skinned meshes draw forward: only the rigid ones are instances.
            let rigid = scene.meshes.iter().filter(|mesh| mesh.joints.is_empty()).count();
            if stats.instances as usize > rigid {
                failures.push(format!(
                    "{name} {builder_name}: {} instances on the path, {rigid} rigid meshes",
                    stats.instances
                ));
            }
            if close < floor(tolerance.close) || ssim < floor(tolerance.ssim) {
                failures.push(format!(
                    "{name} {builder_name}: {:.3} % close (floor {:.1} %), ssim {ssim:.4} (floor {:.3})",
                    close * 100.0,
                    floor(tolerance.close) * 100.0,
                    floor(tolerance.ssim),
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The single-pass resolve is the per-material resolve, on every golden scene, byte for byte.
#[test]
fn the_single_pass_resolve_equals_the_per_material_resolve_on_the_golden_scenes() {
    let device = match device_at(regia_render::texture_table::TEXTURE_TABLE_LEVEL) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    if regia_render::texture_table::device_support(&device.device)
        .capacity()
        .is_none()
    {
        eprintln!("skip: this adapter stays off the texture table");
        return;
    }
    let mut failures = Vec::new();
    for (name, scene, _) in scenes() {
        let builder = single_level_builder();
        let (per_material, a) = frame(
            &device,
            &scene,
            Some(VisibilityPath::DEFAULT),
            Some(builder.clone()),
        )
        .expect("per-material frame");
        let (single, b) = frame(&device, &scene, Some(SINGLE_PASS), Some(builder)).expect("single pass");
        let (worst, close) = diff(&per_material, &single);
        let (ra, rb) = (a.visbuffer_resolve_report(), b.visbuffer_resolve_report());
        if let Some(problem) = frame_problem(&single) {
            failures.push(format!("{name}: the frame cannot be compared: {problem}"));
        }
        println!(
            "{name:>22}: worst {worst}, {:.3} % within 2 levels; draws {} -> {}, materials {}",
            close * 100.0,
            ra.draws,
            rb.draws,
            rb.materials
        );
        if !rb.single_pass || rb.draws != 1 {
            failures.push(format!("{name}: the single pass did not run: {rb:?}"));
        }
        if per_material != single {
            let at = differing_pixels(&per_material, &single, scene.width);
            failures.push(format!("{name}: worst {worst} at {at:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The first few `(x, y)` pixels where `a` and `b` differ, for the log.
fn differing_pixels(a: &[u8], b: &[u8], width: u32) -> Vec<(u32, u32)> {
    let width = width.max(1) as usize;
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .enumerate()
        .filter(|(_, (pa, pb))| pa != pb)
        .take(8)
        .map(|(i, _)| ((i % width) as u32, (i / width) as u32))
        .collect()
}

/// Reruns of the per-material resolve in `the_per_material_resolve_against_itself`.
const SELF_RUNS: usize = 4;

/// Diagnostic (ignored): the per-material resolve against itself, `SELF_RUNS + 1` fresh frames
/// per scene, and where it is unstable. The cull pass hands out draw-list slots with `atomicAdd`
/// (`vis_cull.wgsl`), so the visibility raster's cluster order, and with it the winner of an
/// exact depth tie, can change from run to run on any adapter that runs the cull's invocations
/// concurrently (ADR-2240). Run with `--ignored --nocapture`.
#[test]
#[ignore = "diagnostic: prints the per-material resolve's own run-to-run noise"]
fn the_per_material_resolve_against_itself() {
    let device = match device_at(regia_render::texture_table::TEXTURE_TABLE_LEVEL) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    println!("adapter: {:?}", device.adapter.get_info());
    for (name, scene, _) in scenes() {
        let run = || {
            frame(
                &device,
                &scene,
                Some(VisibilityPath::DEFAULT),
                Some(single_level_builder()),
            )
            .map(|(pixels, _)| pixels)
            .expect("per-material frame")
        };
        let first = run();
        for k in 1..=SELF_RUNS {
            let again = run();
            let (worst, _) = diff(&first, &again);
            let count = first
                .as_chunks::<4>()
                .0
                .iter()
                .zip(again.as_chunks::<4>().0)
                .filter(|(a, b)| a != b)
                .count();
            println!(
                "{name:>22} run {k}: {count} pixels differ from run 0 (worst {worst}) at {:?}",
                differing_pixels(&first, &again, scene.width)
            );
        }
    }
}

/// Leaf cards for the skinned row's masked case: three holed quads standing among the arms.
fn add_masked_cards(scene: &mut LabScene) -> u32 {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let a = if u * u / 0.5 + v * v / 0.95 < 1.0 { 255 } else { 0 };
            rgba.extend_from_slice(&[70, 170, 60, a]);
        }
    }
    let material = scene.materials.len();
    scene.materials.push(LabMaterial {
        albedo: Some(regia_render::LabTexture {
            width: size,
            height: size,
            rgba,
        }),
        alpha_mode: regia_render::ALPHA_MASK,
        alpha_cutoff: 0.5,
        double_sided: true,
        ..LabMaterial::default()
    });
    let corner =
        |x: f32, y: f32, u: f32, v: f32| LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [u, v]);
    let vertices = vec![
        corner(-0.25, 0.0, 0.0, 1.0),
        corner(0.25, 0.0, 1.0, 1.0),
        corner(0.25, 0.8, 1.0, 0.0),
        corner(-0.25, 0.8, 0.0, 0.0),
    ];
    let cards = [-0.3f32, 0.05, 0.4];
    for x in cards {
        let mut model = [[0.0f32; 4]; 4];
        for (axis, row) in model.iter_mut().enumerate() {
            row[axis] = 1.0;
        }
        model[3] = [x, 0.0, 0.7, 1.0];
        scene.meshes.push(LabMesh {
            vertices: vertices.clone(),
            indices: vec![0, 1, 2, 0, 2, 3],
            material,
            model,
            ..LabMesh::unskinned()
        });
    }
    cards.len() as u32
}

/// `scene`'s third frame under a fixed clock and wind, as `frame` draws it.
fn frame_in_wind(
    device: &RenderDevice,
    scene: &LabScene,
    path: Option<VisibilityPath>,
    builder: Option<MeshletBuilder>,
    (now, prev): (f32, f32),
    wind: [f32; 2],
) -> Result<(Vec<u8>, FrameRenderer), RenderError> {
    let mut renderer = renderer_on(device);
    renderer.set_meshlet_builder(builder);
    renderer.set_material_clock(now, prev);
    renderer.set_foliage_wind(Some(wind));
    let mut quality = FrameQuality::LAB;
    quality.visibility_buffer = path.is_some();
    if let Some(path) = path {
        quality.visibility_path = path;
    }
    let frame_scene = scene.as_frame_scene_with_quality(quality);
    renderer.render_offscreen(device, &frame_scene)?;
    renderer.render_offscreen(device, &frame_scene)?;
    let pixels = renderer.render_offscreen(device, &frame_scene)?.pixels;
    Ok((pixels, renderer))
}

/// A swaying masked tree against the classic path at fixed wind times (ADR-3916): the `alpha_modes`
/// scene's masked quad and the skinned scene's leaf cards, adopted with `FoliageSway`, within the
/// strict floors through both resolves. The negative control is the same frame in calm air.
#[test]
fn swaying_masked_meshes_agree_with_the_classic_path_at_fixed_wind_times() {
    let device = match device_at(regia_render::texture_table::TEXTURE_TABLE_LEVEL) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    if regia_render::texture_table::device_support(&device.device)
        .capacity()
        .is_none()
    {
        eprintln!("skip: the adapter cannot carry the texture table");
        return;
    }
    let metal = device.capabilities.backend == wgpu::Backend::Metal;
    let floor = |value: f64| if metal { value.min(0.97) } else { value };
    let sway = regia_render::FoliageSway {
        stiffness: 0.03,
        height_m: 0.8,
        flutter_m: 0.01,
        gust_period_s: 4.0,
        max_wind_m_per_s: 20.0,
        ..regia_render::FoliageSway::default()
    };
    let mut checked = 0;
    for (name, mut scene, _) in scenes() {
        if name != "skinned" {
            continue;
        }
        let cards = add_masked_cards(&mut scene);
        let leaf = scene.materials.len() - 1;
        scene.materials[leaf] = sway.adopt(&scene.materials[leaf]).expect("a plain material");
        for ((now, prev), wind) in [((0.7, 0.65), [6.0, 1.0]), ((2.3, 2.25), [9.0, -2.0])] {
            let (classic, _) =
                frame_in_wind(&device, &scene, None, None, (now, prev), wind).expect("classic");
            let (calm, _) = frame_in_wind(
                &device,
                &scene,
                Some(VisibilityPath::DEFAULT),
                Some(cooked_builder()),
                (now, prev),
                [0.0, 0.0],
            )
            .expect("calm");
            for (path_name, path) in [
                ("per_material", VisibilityPath::DEFAULT),
                ("single_pass", SINGLE_PASS),
            ] {
                let (pixels, renderer) = frame_in_wind(
                    &device,
                    &scene,
                    Some(path),
                    Some(cooked_builder()),
                    (now, prev),
                    wind,
                )
                .expect("visibility frame");
                let stats = renderer.visbuffer_stats();
                let (worst, close) = diff(&classic, &pixels);
                let ssim = ssim_rgba8(&classic, &pixels, scene.width, scene.height).expect("ssim");
                println!(
                    "{name} sway {path_name} t={now}: {:.3} % within 2 levels (worst {worst}), ssim {ssim:.4}, \
                     sway {} masked {} of {}",
                    close * 100.0,
                    stats.sway_instances,
                    stats.masked_instances,
                    stats.instances,
                );
                assert_eq!(
                    stats.sway_instances, cards,
                    "{path_name}: the cards sway on the path"
                );
                assert_eq!(stats.masked_instances, cards, "{path_name}: and are masked");
                assert!(stats.masked_tested, "{path_name}: alpha-tested");
                assert!(
                    close >= floor(STRICT.close) && ssim >= floor(STRICT.ssim),
                    "{path_name} t={now}: {:.3} % close, ssim {ssim:.4}",
                    close * 100.0
                );
                let (_, unmoved) = diff(&calm, &pixels);
                assert!(unmoved < 1.0, "{path_name}: the wind moves the frame");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 2, "the skinned row is in the scene list");
}

/// Masked meshes on a texture-table device (ADR-3915): `alpha_modes` puts its alpha-tested quad on
/// the path through the masked raster, the skinned row adds leaf cards to its mix of forward skinned
/// and rigid clusters, and each frame still agrees with the classic one under the strict floors,
/// through both resolves.
#[test]
fn masked_meshes_on_the_texture_table_agree_with_the_classic_path() {
    let device = match device_at(regia_render::texture_table::TEXTURE_TABLE_LEVEL) {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    if regia_render::texture_table::device_support(&device.device)
        .capacity()
        .is_none()
    {
        eprintln!("skip: the adapter cannot carry the texture table");
        return;
    }
    let metal = device.capabilities.backend == wgpu::Backend::Metal;
    let floor = |value: f64| if metal { value.min(0.97) } else { value };
    let mut rows = Vec::new();
    for (name, mut scene, _) in scenes() {
        match name {
            "alpha_modes" => rows.push((name, scene, 1)),
            "skinned" => {
                let cards = add_masked_cards(&mut scene);
                rows.push((name, scene, cards));
            }
            _ => {}
        }
    }
    assert_eq!(
        rows.len(),
        2,
        "the alpha_modes and skinned scenes are in the list"
    );
    for (name, scene, masked) in rows {
        let (classic, _) = frame(&device, &scene, None, None).expect("classic frame");
        for (path_name, path) in [
            ("per_material", VisibilityPath::DEFAULT),
            ("single_pass", SINGLE_PASS),
        ] {
            let (pixels, renderer) =
                frame(&device, &scene, Some(path), Some(cooked_builder())).expect("visibility frame");
            let stats = renderer.visbuffer_stats();
            let (worst, close) = diff(&classic, &pixels);
            let ssim = ssim_rgba8(&classic, &pixels, scene.width, scene.height).expect("ssim");
            println!(
                "{name} masked {path_name}: {:.3} % within 2 levels (worst {worst}), ssim {ssim:.4}, masked {} of {} instances, tested {}",
                close * 100.0,
                stats.masked_instances,
                stats.instances,
                stats.masked_tested,
            );
            assert_eq!(
                stats.masked_instances, masked,
                "{name} {path_name}: the masked meshes are on the path"
            );
            assert!(stats.masked_tested, "{name} {path_name}: alpha-tested");
            assert!(
                close >= floor(STRICT.close) && ssim >= floor(STRICT.ssim),
                "{name} {path_name}: {:.3} % close, ssim {ssim:.4}",
                close * 100.0
            );
        }
    }
}
