// SPDX-License-Identifier: MPL-2.0
//! Foreground graph emission must survive across the opaque terrain / clear-sky silhouette.
//! Independent scene for WIN_PLAY2; no host assets, source, or golden images.

use std::sync::Arc;

use regia_render::{
    ALPHA_ADDITIVE, ALPHA_BLEND, ALPHA_PREMULTIPLIED, AtmosphereParameters, DirectionalLightUniform,
    FrameQuality, FrameRenderer, IndirectStrategy, LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams,
    PostSettings, SurfaceGraph,
};

const W: u32 = 96;
const H: u32 = 96;

const HOLO: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(0.0), 1.0, 0.0, 1.0, in.world_normal, 1.0,
        vec3<f32>(2000.0, 12000.0, 20000.0), 0.0,
    );
}
";

fn quad(x: [f32; 2], y: [f32; 2], z: f32, material: usize) -> LabMesh {
    LabMesh {
        vertices: [[x[0], y[0]], [x[1], y[0]], [x[1], y[1]], [x[0], y[1]]]
            .into_iter()
            .map(|[x, y]| LabVertex::from_pos_normal_uv([x, y, z], [0.0, 0.0, 1.0], [0.0; 2]))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn scene(holo: bool, backing: bool, prepass: bool) -> LabScene {
    // Lower half of the view: opaque terrain-like geometry, well behind the near plate.
    let mut meshes = vec![quad([-20.0, 20.0], [-20.0, 0.0], -8.0, 0)];
    if backing {
        meshes.push(quad([-0.22, 0.22], [-0.22, 0.22], -0.55, 1));
    }
    if holo {
        meshes.push(quad([-0.20, 0.20], [-0.20, 0.20], -0.5, 2));
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0; 3],
        target: [0.0, 0.0, -1.0],
        fov_y: 1.0,
        near: 0.05,
        far: 100.0,
        depth_prepass: prepass,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 20.0,
        double_sided: false,
        post: Some(PostSettings {
            enabled: true,
            exposure_ev: 12.0,
            auto_exposure: false,
            bloom: false,
            taa: false,
            fxaa: false,
            film_grain: 0.0,
            vignette: 0.0,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, -1.0], [1.0; 3], 1000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.2, 0.15, 0.1, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.004, 0.005, 0.008, 1.0],
                double_sided: true,
                ..LabMaterial::default()
            },
            LabMaterial {
                alpha_mode: ALPHA_ADDITIVE,
                double_sided: true,
                surface_graph: Some(Arc::new(SurfaceGraph::new(HOLO))),
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

fn region_delta(before: &[u8], after: &[u8], y: u32) -> f32 {
    let mut delta = 0.0;
    for row in y..y + 8 {
        for x in 40..56 {
            let i = ((row * W + x) * 4) as usize;
            delta += f32::from(after[i + 1]) - f32::from(before[i + 1]);
        }
    }
    delta / 128.0
}

#[test]
fn additive_graph_crosses_terrain_and_sky_with_and_without_backing() {
    let device = regia_test_utils::serial_gpu::serial_device().expect("headless device is required");
    let mut failures = Vec::new();
    for gpu_driven in [false, true] {
        for (airless, atmosphere) in [(false, true), (true, true), (false, false)] {
            let quality = FrameQuality {
                physical_atmosphere: atmosphere,
                gpu_driven,
                ..FrameQuality::LAB
            };
            let mut renderer = FrameRenderer::new();
            renderer.set_async_material_graphs(false);
            renderer.set_async_sky(false);
            renderer.set_outdoor(OutdoorParams {
                planet: if airless {
                    AtmosphereParameters::airless()
                } else {
                    AtmosphereParameters::default()
                },
                ..OutdoorParams::default()
            });
            for prepass in [false, true] {
                for backing in [true, false] {
                    let off = renderer
                        .render_offscreen(
                            &device,
                            &scene(false, backing, prepass).as_frame_scene_with_quality(quality),
                        )
                        .expect("no-holo frame");
                    let on = renderer
                        .render_offscreen(
                            &device,
                            &scene(true, backing, prepass).as_frame_scene_with_quality(quality),
                        )
                        .expect("holo frame");
                    let sky = region_delta(&off.pixels, &on.pixels, 24);
                    let terrain = region_delta(&off.pixels, &on.pixels, 64);
                    if gpu_driven && IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
                        assert!(renderer.draw_stats().instanced > 0, "exercise GPU opaque draws");
                    }
                    eprintln!(
                        "gpu_driven={gpu_driven} airless={airless} atmosphere={atmosphere} prepass={prepass} backing={backing}: sky delta={sky:.2}, terrain delta={terrain:.2}"
                    );
                    if sky < 20.0 || terrain < 20.0 {
                        failures.push((gpu_driven, airless, atmosphere, prepass, backing, sky, terrain));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "holo emission lost (GPU draws, airless, atmosphere, prepass, backing, sky delta, terrain delta): {failures:?}"
    );
}

#[test]
fn transparent_modes_keep_sky_emission_and_respect_foreground_depth() {
    let device = regia_test_utils::serial_gpu::serial_device().expect("headless device is required");
    let mut renderer = FrameRenderer::new();
    renderer.set_async_material_graphs(false);
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::airless(),
        ..OutdoorParams::default()
    });
    for graph in [false, true] {
        for (mode, oit) in [
            (ALPHA_ADDITIVE, false),
            (ALPHA_BLEND, false),
            (ALPHA_PREMULTIPLIED, false),
            (ALPHA_BLEND, true),
            (ALPHA_PREMULTIPLIED, true),
        ] {
            let quality = FrameQuality {
                physical_atmosphere: true,
                order_independent_transparency: oit,
                ..FrameQuality::LAB
            };
            let mut off_scene = scene(false, false, true);
            // A narrow opaque bar in FRONT of the holo. It covers the central sample
            // but leaves the off-center sample exposed on both sides of the horizon.
            off_scene.meshes.push(quad([-0.04, 0.04], [-0.2, 0.2], -0.3, 1));
            let off = renderer
                .render_offscreen(&device, &off_scene.as_frame_scene_with_quality(quality))
                .expect("occluder without holo");
            let mut on_scene = off_scene;
            on_scene.meshes.push(quad([-0.20, 0.20], [-0.20, 0.20], -0.5, 2));
            let material = &mut on_scene.materials[2];
            material.alpha_mode = mode;
            material.base_color = [0.0, 0.0, 0.0, 0.5];
            if graph {
                // Same emissive surface, with half coverage to exercise actual blending.
                material.surface_graph = Some(Arc::new(SurfaceGraph::new(
                    HOLO.replace("vec3<f32>(0.0), 1.0,", "vec3<f32>(0.0), 0.5,"),
                )));
            } else {
                material.surface_graph = None;
                material.emissive = [2000.0, 12000.0, 20000.0];
            }
            let on = renderer
                .render_offscreen(&device, &on_scene.as_frame_scene_with_quality(quality))
                .expect("occluder with holo");
            for y in [28, 68] {
                let visible = ((y * W + 28) * 4 + 1) as usize;
                let covered = ((y * W + 48) * 4) as usize;
                let delta = i16::from(on.pixels[visible]) - i16::from(off.pixels[visible]);
                eprintln!("graph={graph} mode={mode} oit={oit} y={y}: visible delta={delta}");
                assert!(
                    delta > 20,
                    "transparent emission lost: graph={graph} mode={mode} oit={oit} y={y} delta={delta}"
                );
                assert_eq!(
                    &on.pixels[covered..covered + 3],
                    &off.pixels[covered..covered + 3],
                    "opaque foreground must occlude: graph={graph} mode={mode} oit={oit} y={y}"
                );
            }
        }
    }
}

#[test]
fn held_sky_and_holo_do_not_accumulate_and_sky_settings_invalidate_reuse() {
    let device = regia_test_utils::serial_gpu::serial_device().expect("headless device is required");
    let mut renderer = FrameRenderer::new();
    renderer.set_async_material_graphs(false);
    renderer.set_async_sky(false);
    renderer.set_ssgi_half_res(Some(false));
    let quality = FrameQuality {
        physical_atmosphere: true,
        screen_space_gi: true,
        paged_sun_shadows: true,
        ..FrameQuality::LAB
    };
    let lab = scene(true, false, true);
    let frame = lab.as_frame_scene_with_quality(quality);
    let first = renderer.render_offscreen(&device, &frame).expect("first frame");
    for _ in 0..4 {
        let held = renderer.render_offscreen(&device, &frame).expect("held frame");
        assert_eq!(
            held.pixels, first.pixels,
            "a held holo must not accumulate or lose sky emission"
        );
        assert!(
            renderer.screen_pass_report().screen_cache_hit,
            "exercise the held-image cache"
        );
    }
    renderer.set_star_flux(!renderer.star_flux());
    renderer
        .render_offscreen(&device, &frame)
        .expect("new sky setting");
    assert!(
        !renderer.screen_pass_report().screen_cache_hit,
        "a changed sky background must invalidate the cached forward image"
    );
    let without_atmosphere = lab.as_frame_scene_with_quality(FrameQuality {
        physical_atmosphere: false,
        ..quality
    });
    let clear = renderer
        .render_offscreen(&device, &without_atmosphere)
        .expect("sky disabled");
    assert!(
        !renderer.screen_pass_report().screen_cache_hit,
        "disabling the physical sky must invalidate its cached background"
    );
    let sky_corner = ((4 * W + 4) * 4 + 2) as usize;
    assert!(
        i16::from(first.pixels[sky_corner]) - i16::from(clear.pixels[sky_corner]) > 20,
        "an uncovered sky pixel must become the clear background when the sky is disabled"
    );
}
