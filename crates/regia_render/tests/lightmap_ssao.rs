// SPDX-License-Identifier: MPL-2.0
//! Screen-space AO over a lightmap receiver (ADR-2635): the bake already holds the receiver's
//! occlusion, so the frame's SSAO must not darken the lightmap's diffuse term a second time.
//!
//! A floor quad (the receiver, lit by a uniform lightmap) meets a plain wall box. With the
//! lightmap off, the floor takes the flat ambient and the SSAO darkens it at the wall's foot: the
//! scene shows the kernel at work. With the lightmap on, the same floor pixels must keep their
//! light within a small share of that darkening (only the specular keeps the SSAO forms), on the
//! CPU loop, the GPU-driven batches and a visibility-buffer frame alike. No golden image: every
//! check compares frames rendered here (lavapipe, deterministic).

use std::sync::{Arc, OnceLock};

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_asset_pipeline::meshlet::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::{
    DdgiSettings, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabMesh, LabScene, LabVertex, MeshletBuilder, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SsaoMode, box_mesh, translate_mesh,
};

const W: u32 = 160;
const H: u32 = 96;
const FLOOR: u64 = 0x0A0F_100E;
const WALL: u64 = 0x0A0F_3A11;
/// The lightmap's uniform irradiance (lux) and the flat ambient of the unlightmapped frames.
const BAKED_LUX: f32 = 3_000.0;
const AMBIENT_LUX: f32 = 3_000.0;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

fn renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    // A fresh and a reset capture must see the same bricks, even when task workers
    // are descheduled. Keep the profile's GI, with deterministic bake completion.
    renderer.set_ddgi_settings(DdgiSettings {
        bake_on_caller: true,
        ..DdgiSettings::default()
    });
    renderer
}

/// A 4 m floor quad at y = 0 facing up, UV 0..1 over it.
fn floor() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([-2.0, 0.0, 2.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([2.0, 0.0, 2.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([2.0, 0.0, -2.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-2.0, 0.0, -2.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

/// One entry `floor` over the whole 4 × 4 atlas, unwrapped 1:1 from the quad, every texel
/// `BAKED_LUX`.
fn lightmap() -> Arc<Lightmap> {
    lightmap_with_irradiance([BAKED_LUX; 3])
}

fn lightmap_with_irradiance(irradiance: [f32; 3]) -> Arc<Lightmap> {
    let map = Lightmap::from_irradiance(
        LightmapLayout {
            width: 4,
            height: 4,
            density: 1.0,
            padding: 0,
            entries: vec![LightmapEntry {
                name: "floor".into(),
                source_vertices: 4,
                rect: [0, 0, 4, 4],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 16.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &[irradiance; 16],
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    );
    match map {
        Ok(map) => Arc::new(map),
        Err(err) => panic!("valid lightmap: {err:?}"),
    }
}

/// The wall (a plain box standing on the floor's far half); no sun, no sky map.
fn lab() -> LabScene {
    let (v, indices) = box_mesh([1.6, 0.8, 0.15]);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.6, 2.6],
        target: [0.0, 0.2, -0.6],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: AMBIENT_LUX,
        double_sided: true,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.95,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [0.0, 0.8, -0.8]),
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn quality(ssao: bool, (gpu_driven, visibility_buffer): (bool, bool)) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.ssao = if ssao {
        SsaoMode::GtaoHalfRes
    } else {
        SsaoMode::Off
    };
    q.gpu_driven = gpu_driven;
    q.visibility_buffer = visibility_buffer;
    q
}

/// Builds the clusters a visibility-buffer frame draws.
fn meshlet_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        });
        build_meshlet_mesh_with_attributes(&positions, attributes, indices, &MeshletSettings::default()).ok()
    })
}

/// The floor (a receiver of the lightmap while `settings.enabled`) and the wall, two frames (the
/// second is returned) so every history the passes keep is settled. A visibility-buffer frame must
/// draw the receiver in the visibility buffer, not fall back to a forward path.
fn render(renderer: &mut FrameRenderer, settings: LightmapSettings, quality: FrameQuality) -> Vec<u8> {
    let lab = lab();
    let base = lab.as_frame_scene_with_quality(quality);
    let wall = base.meshes[0];
    let (fv, fi) = floor();
    let map = lightmap();
    let built = match receiver_mesh(&map, 0, &fv, &fi) {
        Ok(built) => built,
        Err(err) => panic!("the baked mesh: {err:?}"),
    };
    let scene = FrameScene {
        meshes: vec![
            FrameMesh {
                mesh_key: 0x0A0F_0001 << 3,
                object_key: FLOOR,
                vertices: &built.vertices,
                indices: &built.indices,
                uv1: &built.uv2,
                ..wall
            },
            FrameMesh {
                mesh_key: 0x0A0F_0002 << 3,
                object_key: WALL,
                ..wall
            },
        ],
        ..base.clone()
    };
    // ADR-4760: retain compiled pipelines between the five comparisons, while resetting
    // all temporal and shadow history exactly as a fresh renderer's first frame does.
    renderer.camera_cut();
    let visibility = quality.visibility_buffer;
    if visibility {
        renderer.set_meshlet_builder(Some(meshlet_builder()));
    }
    let lightmapped = settings.enabled;
    renderer.set_lightmap(Some(map), settings);
    renderer.set_lightmap_receivers(&[(FLOOR, 0)]);
    let mut last = Vec::new();
    for _ in 0..2 {
        last = match renderer.render_offscreen(device(), &scene) {
            Ok(frame) => frame.pixels,
            Err(err) => panic!("frame: {err:?}"),
        };
    }
    assert_eq!(last.len(), (W * H * 4) as usize);
    if visibility {
        assert!(
            renderer.visbuffer_stats().instances >= 2,
            "both meshes are cluster instances"
        );
        if lightmapped {
            let stats = renderer.lightmap_forward_stats();
            assert_eq!((stats.visbuffer, stats.fallback), (1, 0), "{stats:?}");
        }
    }
    last
}

fn luma(img: &[u8], i: usize) -> f32 {
    0.2126 * f32::from(img[i]) + 0.7152 * f32::from(img[i + 1]) + 0.0722 * f32::from(img[i + 2])
}

fn mean(img: &[u8], at: &[usize]) -> f32 {
    at.iter().map(|&i| luma(img, i)).sum::<f32>() / at.len().max(1) as f32
}

fn on(intensity: f32) -> LightmapSettings {
    LightmapSettings {
        enabled: true,
        intensity,
        ..LightmapSettings::default()
    }
}

#[test]
fn ssao_leaves_a_lightmap_receiver_s_diffuse_term_alone() {
    for switches in [(false, false), (true, false), (true, true)] {
        let mut renderer = renderer();
        // The floor's pixels: what a change of the lightmap's intensity moves (SSAO off).
        let lit = render(&mut renderer, on(1.0), quality(false, switches));
        let twice = render(&mut renderer, on(2.0), quality(false, switches));
        assert!(
            lit == render(&mut renderer, on(1.0), quality(false, switches)),
            "{switches:?}: a reset cached capture matches the fresh renderer"
        );
        let floor: Vec<usize> = (0..(W * H) as usize)
            .map(|p| p * 4)
            .filter(|&i| luma(&twice, i) > luma(&lit, i) + 10.0)
            .collect();
        assert!(
            floor.len() > (W * H / 8) as usize,
            "{switches:?}: the floor fills the lower frame ({} pixels)",
            floor.len()
        );

        // The control: the same floor without the lightmap (the flat ambient); the SSAO
        // darkens it at the wall's foot.
        let off = LightmapSettings::default();
        let plain = render(&mut renderer, off, quality(false, switches));
        let plain_ao = render(&mut renderer, off, quality(true, switches));
        let crease: Vec<usize> = floor
            .iter()
            .copied()
            .filter(|&i| luma(&plain, i) > luma(&plain_ao, i) + 6.0)
            .collect();
        assert!(
            crease.len() >= 40,
            "{switches:?}: the SSAO darkens the unlightmapped floor at the wall ({} pixels)",
            crease.len()
        );
        let plain_drop = 1.0 - mean(&plain_ao, &crease) / mean(&plain, &crease);

        // The receiver over the same pixels: the bake holds that occlusion, so the SSAO leaves
        // its diffuse term alone (the specular lobe keeps a trace of it).
        let baked_ao = render(&mut renderer, on(1.0), quality(true, switches));
        let baked_drop = 1.0 - mean(&baked_ao, &crease) / mean(&lit, &crease);
        assert!(
            mean(&lit, &crease) > 40.0 && mean(&lit, &crease) < 230.0,
            "{switches:?}: the receiver is mid-grey, not clipped ({})",
            mean(&lit, &crease)
        );
        eprintln!(
            "{switches:?}: {} crease pixels; SSAO darkens the plain floor {:.1} %, the receiver {:.1} %",
            crease.len(),
            plain_drop * 100.0,
            baked_drop * 100.0
        );
        assert!(
            baked_drop < 0.2 * plain_drop,
            "{switches:?}: SSAO darkens the receiver by {:.1} % at the wall's foot, the plain floor by {:.1} %",
            baked_drop * 100.0,
            plain_drop * 100.0
        );
    }
}

/// The wall is not a receiver: the SSAO still darkens it where it meets the floor, in the
/// lightmapped frame as in the plain one (the change reaches only receivers).
#[test]
fn ssao_still_darkens_what_is_not_a_receiver() {
    let switches = (false, false);
    let mut renderer = renderer();
    let lit = render(&mut renderer, on(1.0), quality(false, switches));
    let twice = render(&mut renderer, on(2.0), quality(false, switches));
    let lit_ao = render(&mut renderer, on(1.0), quality(true, switches));
    let plain = render(
        &mut renderer,
        LightmapSettings::default(),
        quality(false, switches),
    );
    let plain_ao = render(
        &mut renderer,
        LightmapSettings::default(),
        quality(true, switches),
    );
    // Wall pixels: (nearly) not moved by the lightmap's intensity (the post stack's glow of the
    // brighter floor may reach them by a level or two), and darkened by the SSAO in the plain
    // frame.
    let wall: Vec<usize> = (0..(W * H) as usize)
        .map(|p| p * 4)
        .filter(|&i| {
            (luma(&twice, i) - luma(&lit, i)).abs() <= 2.0 && luma(&plain, i) > luma(&plain_ao, i) + 3.0
        })
        .collect();
    eprintln!("wall pixels the SSAO darkens: {}", wall.len());
    assert!(
        wall.len() >= 20,
        "the wall's foot darkens ({} pixels)",
        wall.len()
    );
    let plain_drop = mean(&plain, &wall) - mean(&plain_ao, &wall);
    let lit_drop = mean(&lit, &wall) - mean(&lit_ao, &wall);
    assert!(
        lit_drop > 0.5 * plain_drop,
        "the wall keeps its SSAO next to a receiver: {lit_drop:.1} against {plain_drop:.1}"
    );
}

/// Partial capture coverage must not mix the bake's and capture's occlusion policies.
/// A blue capture retains its original screen occlusion; a red bake carries its own
/// visibility and must not acquire that occlusion when the sources share a receiver.
#[test]
fn partial_capture_and_bake_keep_independent_occlusion() {
    use regia_render::{EnvironmentAuthority, LocalCaptureBake, LocalCaptureDesc, StudioEquirect};
    let frame = |specular, ssao, baked_red, weight| {
        let mut lab = lab();
        lab.ibl = true;
        lab.ambient_lux = 0.0;
        lab.materials[0].base_color = [1.0; 4];
        lab.materials[0].metallic = 1.0;
        lab.materials[0].roughness = 0.5;
        let mut q = quality(ssao, (false, false));
        q.ssr = false;
        q.screen_space_gi = false;
        q.gi_probes = false;
        let base = lab.as_frame_scene_with_quality(q);
        let wall = base.meshes[0];
        let (vertices, indices) = floor();
        let map = lightmap_with_irradiance([baked_red, 0.0, 0.0]);
        let built = receiver_mesh(&map, 0, &vertices, &indices).expect("floor receiver");
        let scene = FrameScene {
            meshes: vec![
                FrameMesh {
                    mesh_key: 0x0A0F_0001 << 3,
                    object_key: FLOOR,
                    vertices: &built.vertices,
                    indices: &built.indices,
                    uv1: &built.uv2,
                    ..wall
                },
                FrameMesh {
                    object_key: WALL,
                    ..wall
                },
            ],
            ..base.clone()
        };
        let mut renderer = FrameRenderer::new();
        renderer.set_lightmap(Some(map), LightmapSettings { specular, ..on(1.0) });
        renderer.set_lightmap_receivers(&[(FLOOR, 0)]);
        renderer.set_environment_authority(EnvironmentAuthority::Studio);
        let sky = StudioEquirect::new(64, 32, vec![[0.0; 3]; 64 * 32]).expect("black sky");
        assert!(renderer.set_studio_equirect(device(), Some(&sky)));
        let width = regia_render::LOCAL_CAPTURE_WIDTH;
        // Half-float RGBA: uniform blue radiance, opaque surface coverage.
        let texel = [0, 0, 0, 0, 0, 0x34, 0, 0x3c];
        let bake = LocalCaptureBake {
            width,
            coverage: 1.0,
            mips: (0..regia_render::local_capture_mip_count())
                .map(|m| texel.repeat(((width >> m).max(1) * ((width / 2) >> m).max(1)) as usize))
                .collect(),
        };
        let mut desc = LocalCaptureDesc::new(1, [0.0; 3], [10.0; 3]);
        desc.weight = weight;
        desc.bake = Some(Arc::new(bake));
        renderer.place_local_capture(desc).expect("capture");
        renderer.select_local_captures(device(), scene.eye, 1);
        renderer.render_offscreen(device(), &scene).expect("settle");
        renderer.request_hdr_capture();
        renderer.render_offscreen(device(), &scene).expect("frame");
        renderer
            .take_hdr_capture(device())
            .expect("readback")
            .expect("HDR")
            .rgb
    };
    for weight in [0.25, 0.5, 0.75] {
        let legacy = frame(false, true, 0.0, weight);
        let legacy_open = frame(false, false, 0.0, weight);
        let baked = frame(true, true, BAKED_LUX, weight);
        let baked_open = frame(true, false, BAKED_LUX, weight);
        let floor: Vec<usize> = baked_open
            .iter()
            .enumerate()
            .filter(|(_, p)| p[0] > 1.0)
            .map(|(i, _)| i)
            .collect();
        assert!(
            floor.len() > (W * H / 8) as usize,
            "red bake identifies the floor"
        );
        let crease: Vec<usize> = floor
            .iter()
            .copied()
            .filter(|&i| legacy_open[i][2] > legacy[i][2] * 1.05 + 0.1)
            .collect();
        assert!(
            crease.len() >= 20,
            "capture must see screen occlusion: {} pixels",
            crease.len()
        );
        for &i in &floor {
            assert!(
                (baked[i][2] - legacy[i][2]).abs() <= legacy[i][2] * 0.003 + 0.05,
                "capture weight {weight}, pixel {i}: blue {} vs legacy {}",
                baked[i][2],
                legacy[i][2]
            );
            assert!(
                (baked[i][0] - baked_open[i][0]).abs() <= baked_open[i][0] * 0.003 + 0.05,
                "capture weight {weight}, pixel {i}: baked red {} vs without SSAO {}",
                baked[i][0],
                baked_open[i][0]
            );
        }
    }
}
