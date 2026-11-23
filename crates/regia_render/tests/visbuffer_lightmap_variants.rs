// SPDX-License-Identifier: MPL-2.0
//! Lightmap receivers on the visibility path, per material (ADR-2689): the path counts receiver
//! instances and the materials that hold one, so only those materials need the resolve's
//! `LIGHTMAP` variant. A material with no receiver shades exactly as in a frame with no lightmap.
//! No golden image: every check compares frames rendered here (lavapipe, deterministic).

use std::sync::Arc;

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::visbuffer::MeshletMesh;
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh,
    LabScene, LabVertex, MeshletBuilder, PostSettings, RenderDevice,
};

const W: u32 = 64;
const H: u32 = 64;
const KEY_A: u64 = 0x0B1E_C7B1;
const KEY_B: u64 = 0x0B1E_C7B2;
const KEY_PLAIN: u64 = 0x0B1E_C7B3;
const MESH: u64 = 0x0A11_0202 << 3;
/// Material keys: the receivers' material and the plain instance's.
const RECEIVER_MATERIAL: u64 = 0x5EED_0001;
const PLAIN_MATERIAL: u64 = 0x5EED_0002;
/// The plain instance's pixel (x = 0.65).
const PLAIN_PX: (u32, u32) = (50, 32);

fn quad() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([-0.3, -0.3, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.3, -0.3, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.3, 0.3, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-0.3, 0.3, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

/// An 8 × 2 atlas with one entry over all of it.
fn lightmap() -> Lightmap {
    let irr: Vec<[f32; 3]> = (0..16).map(|i| [4_000.0 + 1_000.0 * (i % 8) as f32; 3]).collect();
    Lightmap::from_irradiance(
        LightmapLayout {
            width: 8,
            height: 2,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "a".into(),
                source_vertices: 4,
                rect: [0, 0, 8, 2],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 0.36,
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
        &irr,
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap")
}

fn builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

/// One instance of the receiver-ready mesh at `x`.
fn instance<'a>(
    template: FrameMesh<'a>,
    (vertices, indices, uv1): (&'a [LabVertex], &'a [u32], &'a [[f32; 2]]),
    (x, object_key): (f32, u64),
    (material_key, material): (u64, &'a LabMaterial),
) -> FrameMesh<'a> {
    FrameMesh {
        mesh_key: MESH,
        object_key,
        material_key,
        material,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [x, 0.0, 0.0, 1.0],
        ],
        vertices,
        indices,
        uv1,
        ..template
    }
}

struct Frame {
    img: Vec<u8>,
    stats: regia_render::visbuffer::VisBufferStats,
    resolve: regia_render::VisResolveReport,
}

/// Three instances of one receiver-ready mesh: two of the receivers' material (the first one, or
/// both when `both`, are receivers) and one of another material that never is.
fn frame(device: &RenderDevice, map: Option<&Arc<Lightmap>>, both: bool) -> Frame {
    let (v, i) = quad();
    let unwrap_map = lightmap();
    let built = receiver_mesh(&unwrap_map, 0, &v, &i).expect("the baked mesh");
    let lab = LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 2.0],
        target: [0.0; 3],
        fov_y: std::f32::consts::FRAC_PI_3,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices: v.clone(),
            indices: i.clone(),
            ..LabMesh::unskinned()
        }],
    };
    let receiver_material = LabMaterial::default();
    let plain_material = LabMaterial {
        base_color: [0.6, 0.7, 0.8, 1.0],
        ..LabMaterial::default()
    };
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let geometry = (&built.vertices[..], &built.indices[..], &built.uv2[..]);
    let scene = FrameScene {
        quality: FrameQuality {
            visibility_buffer: true,
            ..base.quality
        },
        meshes: vec![
            instance(
                template,
                geometry,
                (-0.65, KEY_A),
                (RECEIVER_MATERIAL, &receiver_material),
            ),
            instance(
                template,
                geometry,
                (0.0, KEY_B),
                (RECEIVER_MATERIAL, &receiver_material),
            ),
            instance(
                template,
                geometry,
                (0.65, KEY_PLAIN),
                (PLAIN_MATERIAL, &plain_material),
            ),
        ],
        ..base.clone()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_meshlet_builder(Some(builder()));
    if let Some(map) = map {
        renderer.set_lightmap(
            Some(Arc::clone(map)),
            LightmapSettings {
                enabled: true,
                ..LightmapSettings::default()
            },
        );
        let receivers: &[(u64, u32)] = if both {
            &[(KEY_A, 0), (KEY_B, 0)]
        } else {
            &[(KEY_A, 0)]
        };
        renderer.set_lightmap_receivers(receivers);
    }
    let img = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    Frame {
        img,
        stats: renderer.visbuffer_stats(),
        resolve: renderer.visbuffer_resolve_report(),
    }
}

fn px(img: &[u8], (x, y): (u32, u32)) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
}

#[test]
fn only_materials_with_a_receiver_count_for_the_lightmap_variant() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    let map = Arc::new(lightmap());
    let one = frame(&device, Some(&map), false);
    assert_eq!(one.stats.instances, 3, "{:?}", one.stats);
    assert_eq!(one.stats.materials, 2, "{:?}", one.stats);
    assert_eq!(
        (one.stats.receiver_instances, one.stats.receiver_materials),
        (1, 1),
        "one receiver, in one of the two materials: {:?}",
        one.stats
    );
    let both = frame(&device, Some(&map), true);
    assert_eq!(
        (both.stats.receiver_instances, both.stats.receiver_materials),
        (2, 1),
        "two receivers of one material: {:?}",
        both.stats
    );
    let none = frame(&device, None, false);
    assert_eq!(
        (none.stats.receiver_instances, none.stats.receiver_materials),
        (0, 0),
        "{:?}",
        none.stats
    );
    // The resolve shades only the receivers' material through the variant (the frame hookup).
    assert_eq!(
        (one.resolve.materials, one.resolve.lightmap_materials),
        (2, 1),
        "{:?}",
        one.resolve
    );
    assert_eq!(none.resolve.lightmap_materials, 0, "{:?}", none.resolve);
    // Depth pass, the plain material, the receivers' material: three draws, two shade pipelines.
    assert_eq!(one.resolve.draws, 3, "{:?}", one.resolve);
    // The material without a receiver shades exactly as with no lightmap at all.
    assert_eq!(px(&one.img, PLAIN_PX), px(&none.img, PLAIN_PX));
}
