// SPDX-License-Identifier: MPL-2.0
//! A graph material that reads the second UV set on a mesh in a vertex-arena range.
//!
//! A draw's `base_vertex` offsets every vertex buffer slot, and the second UV stream is a
//! buffer of its own from vertex 0: a mesh that is not first in its arena chunk must still read
//! its own second UVs. The same frame drawn with the meshes uploaded in the other order (the
//! graph mesh at vertex 0) must match pixel for pixel.

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, GraphCode, LabMaterial, LabMesh, LabScene,
    LabVertex, RenderDevice, SurfaceGraph,
};

/// Unlit, coloured by the second UV set.
const GRAPH: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(in.second_uv, 0.0),
        1.0,
        0.0,
        1.0,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        1.0,
    );
}
";

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(x: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([x - 0.3, -0.3, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, -0.3, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, 0.3, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.3, 0.3, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn lab() -> LabScene {
    let (vertices, indices) = quad(0.0);
    LabScene {
        width: 32,
        height: 32,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn scene<'a>(base: &FrameScene<'a>, meshes: Vec<FrameMesh<'a>>) -> FrameScene<'a> {
    FrameScene {
        meshes,
        ..base.clone()
    }
}

#[test]
fn second_uvs_follow_the_mesh_into_its_arena_range() {
    let device = device();
    let lab = lab();
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let graph = LabMaterial {
        unlit: true,
        surface_graph: Some(Arc::new(SurfaceGraph::from_code(&GraphCode {
            surface: GRAPH.into(),
            vertex: None,
            uv1: true,
        }))),
        ..LabMaterial::default()
    };
    let (plain_v, plain_i) = quad(-0.5);
    let (graph_v, graph_i) = quad(0.5);
    let plain = FrameMesh {
        mesh_key: 0x0A11_0001 << 3,
        object_key: 0x0B1E_C701,
        vertices: &plain_v,
        indices: &plain_i,
        ..template
    };
    let graphed = |key: u64, uv1: &'static [[f32; 2]; 4]| FrameMesh {
        mesh_key: key,
        object_key: 0x0B1E_C702,
        material_key: 0x3A7_0001,
        material: &graph,
        vertices: &graph_v,
        indices: &graph_i,
        uv1,
        ..template
    };
    static YELLOW: [[f32; 2]; 4] = [[1.0, 1.0]; 4];
    static BLACK: [[f32; 2]; 4] = [[0.0, 0.0]; 4];

    // The plain quad uploads first: the graph quad's vertices start past it in the chunk.
    let offset = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(&base, vec![plain, graphed(0x0A11_0002 << 3, &YELLOW)]),
        )
        .expect("graph mesh second");
    // The graph quad uploads first, at vertex 0.
    let at_zero = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(&base, vec![graphed(0x0A11_0002 << 3, &YELLOW), plain]),
        )
        .expect("graph mesh first");
    // The second UVs reach the pixels at all.
    let unlit_black = FrameRenderer::new()
        .render_offscreen(
            &device,
            &scene(&base, vec![graphed(0x0A11_0002 << 3, &BLACK), plain]),
        )
        .expect("black second uvs");
    let differ = |a: &[u8], b: &[u8]| a.chunks(4).zip(b.chunks(4)).filter(|(a, b)| a != b).count();
    assert!(
        differ(&at_zero.pixels, &unlit_black.pixels) > 0,
        "the graph reads the second UV set"
    );
    assert_eq!(
        differ(&offset.pixels, &at_zero.pixels),
        0,
        "an arena offset reads the mesh's own second UVs"
    );
}
