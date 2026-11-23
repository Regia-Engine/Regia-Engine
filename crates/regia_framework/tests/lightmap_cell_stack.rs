// SPDX-License-Identifier: MPL-2.0
//! Streamed cells' lightmaps stack in fixed slots (ADR-3318): a cell streaming in or out leaves
//! the other cells' frames byte-identical.
//!
//! Cell A's floor is a lightmap receiver. Cells B and C load and unload around it in the sequence a
//! streaming camera produces ({A}, {A, B}, {A, B, C}, {A, C}, {A}); each step stacks the loaded
//! cells with `stack_cell_lightmaps`, installs the atlas and renders the floor through A's entry.
//! Every frame must be the same bytes (lavapipe, deterministic), and the stack must never re-lay.
//! A control frame with A's light doubled proves the frame reads the lightmap. No golden image.

use std::sync::{Arc, OnceLock};

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_framework::lightmap_level::{CellLightmapStack, stack_cell_lightmaps};
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh,
    LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
    translate_mesh,
};
use regia_scene::stream_grid::CellCoord;

const W: u32 = 160;
const H: u32 = 96;
const FLOOR: u64 = 0x0C31_0001;
const PROP: u64 = 0x0C31_0002;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
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

/// A cell's map as a bake writes it: an 8 × 8 atlas, one entry `name` in the middle 4 × 4 with a
/// 2-texel gutter dilated from the chart's edge, light `lux(x, y)` over the chart's texels.
fn cell_map(name: &str, lux: impl Fn(u32, u32) -> f32) -> Lightmap {
    let mut irradiance = vec![[0.0f32; 3]; 64];
    let mut coverage = vec![Coverage::DILATED; 64];
    for y in 0..8u32 {
        for x in 0..8u32 {
            let (cx, cy) = (x.clamp(2, 5) - 2, y.clamp(2, 5) - 2);
            let v = lux(cx, cy);
            irradiance[(y * 8 + x) as usize] = [v, 0.9 * v, 0.8 * v];
            if (2..6).contains(&x) && (2..6).contains(&y) {
                coverage[(y * 8 + x) as usize] = Coverage::BAKED;
            }
        }
    }
    let map = Lightmap::from_irradiance(
        LightmapLayout {
            width: 8,
            height: 8,
            density: 1.0,
            padding: 2,
            entries: vec![LightmapEntry {
                name: name.into(),
                source_vertices: 4,
                rect: [2, 2, 4, 4],
                page: 0,
                scale_offset: [0.5, 0.5, 0.25, 0.25],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 16.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &irradiance,
        coverage,
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    );
    match map {
        Ok(mut map) => {
            // Every cell file of a `--cells` bake has this exposure.
            let scale = 0.125 / map.exposure;
            for t in &mut map.texels {
                *t = regia_asset::baked_gi::rgb9e5(regia_asset::baked_gi::from_rgb9e5(*t).map(|v| v * scale));
            }
            map.exposure = 0.125;
            map
        }
        Err(err) => panic!("valid lightmap: {err:?}"),
    }
}

fn lab() -> LabScene {
    let (v, indices) = box_mesh([0.3, 0.3, 0.3]);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 2.2, 3.0],
        target: [0.0, 0.0, -0.4],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 500.0,
        double_sided: true,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.7, 0.7, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [1.4, 0.3, -1.2]),
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q
}

/// The floor drawn as a receiver of entry `entry` of `map`; the second of two frames.
fn render(map: Arc<Lightmap>, entry: usize, intensity: f32) -> Vec<u8> {
    let lab = lab();
    let base = lab.as_frame_scene_with_quality(quality());
    let prop = base.meshes[0];
    let (fv, fi) = floor();
    let built = match receiver_mesh(&map, entry, &fv, &fi) {
        Ok(built) => built,
        Err(err) => panic!("the baked mesh: {err:?}"),
    };
    let scene = FrameScene {
        meshes: vec![
            FrameMesh {
                mesh_key: 0x0C31_0001 << 3,
                object_key: FLOOR,
                vertices: &built.vertices,
                indices: &built.indices,
                uv1: &built.uv2,
                ..prop
            },
            FrameMesh {
                mesh_key: 0x0C31_0002 << 3,
                object_key: PROP,
                ..prop
            },
        ],
        ..base.clone()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_lightmap(
        Some(map),
        LightmapSettings {
            enabled: true,
            intensity,
            ..LightmapSettings::default()
        },
    );
    renderer.set_lightmap_receivers(&[(FLOOR, entry as u32)]);
    let mut last = Vec::new();
    for _ in 0..2 {
        last = match renderer.render_offscreen(device(), &scene) {
            Ok(frame) => frame.pixels,
            Err(err) => panic!("frame: {err:?}"),
        };
    }
    assert_eq!(last.len(), (W * H * 4) as usize);
    last
}

#[test]
fn a_cell_streaming_in_and_out_leaves_the_other_cells_frames_byte_identical() {
    let a = cell_map("floor_a", |x, y| 2_000.0 + 900.0 * x as f32 + 400.0 * y as f32);
    let b = cell_map("floor_b", |_, _| 9_000.0);
    let c = cell_map("floor_c", |x, _| 500.0 + 3_000.0 * x as f32);
    let (ka, kb, kc) = (CellCoord::new(0, 0), CellCoord::new(1, 0), CellCoord::new(2, 0));
    let mut stack = CellLightmapStack::default();
    let mut frames = Vec::new();
    let steps: [&[(CellCoord, &Lightmap)]; 5] = [
        &[(ka, &a)],
        &[(ka, &a), (kb, &b)],
        &[(ka, &a), (kb, &b), (kc, &c)],
        &[(ka, &a), (kc, &c)],
        &[(ka, &a)],
    ];
    let mut slot_a = None;
    for (k, cells) in steps.iter().enumerate() {
        let (map, regions) = match stack_cell_lightmaps(None, cells, &mut stack) {
            Ok(stacked) => stacked,
            Err(err) => panic!("step {k}: {err}"),
        };
        let Some(Some(region)) = regions.first() else {
            panic!("step {k}: A is stacked");
        };
        assert_eq!(
            *slot_a.get_or_insert(region.slot),
            region.slot,
            "A keeps its slot"
        );
        frames.push(render(Arc::new(map), region.first_entry, 1.0));
    }
    assert_eq!(stack.relayouts, 0, "the layout held: {stack:?}");
    for (k, f) in frames.iter().enumerate().skip(1) {
        let differ = f.iter().zip(&frames[0]).filter(|(x, y)| x != y).count();
        assert_eq!(
            differ, 0,
            "step {k}: {differ} bytes differ from the frame with A alone"
        );
    }
    // The frame reads the lightmap: twice the light is a different frame.
    let (map, regions) = match stack_cell_lightmaps(None, &[(ka, &a)], &mut stack) {
        Ok(stacked) => stacked,
        Err(err) => panic!("{err}"),
    };
    let entry = regions[0].map_or(0, |r| r.first_entry);
    let brighter = render(Arc::new(map), entry, 2.0);
    let moved = brighter.iter().zip(&frames[0]).filter(|(x, y)| x != y).count();
    assert!(
        moved > (W * H) as usize / 8,
        "the floor is lightmapped ({moved} bytes moved)"
    );
}
