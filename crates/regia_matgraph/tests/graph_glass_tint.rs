// SPDX-License-Identifier: MPL-2.0
//! Transmissive material-graph glass keeps its hue (ADR-347; the "graph-glass tint" item of the
//! post-alpha roadmap). GPU test (lavapipe in CI), no golden: the same red pane in front of a
//! neutral grey backdrop renders once as a standard transmissive material and once as a graph
//! material the host made transmissive (`LabMaterial::transmission`), and the view through the
//! pane must be red in both, the two frames alike.
//!
//! Before the fix a transmissive graph drew in one premultiplied blend whose alpha dimmed what
//! is behind by the base colour's luminance only: the backdrop came through grey (red ≈ green)
//! instead of red.

use regia_matgraph::{MaterialGraph, Node, ParamDecl, ParamValue, default_params, lab_material};
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabVertex, RenderDevice,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
/// The pane's tint: strongly red, so a lost hue is far outside any rounding.
const TINT: [f32; 3] = [1.0, 0.2, 0.2];
const TRANSMISSION: f32 = 0.95;
const ROUGHNESS: f32 = 0.1;

/// A quad of half size `h` in the plane `z`, facing +Z (counter-clockwise seen from +Z).
fn quad(h: f32, z: f32, material: usize) -> LabMesh {
    let mut mesh = LabMesh::unskinned();
    for (p, uv) in [
        ([-h, -h, z], [0.0, 1.0]),
        ([h, -h, z], [1.0, 1.0]),
        ([h, h, z], [1.0, 0.0]),
        ([-h, h, z], [0.0, 0.0]),
    ] {
        mesh.vertices
            .push(LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], uv));
    }
    mesh.indices = vec![0, 1, 2, 0, 2, 3];
    mesh.material = material;
    mesh
}

/// A neutral unlit backdrop filling the frame and a 1 m pane at the centre, in front of it.
fn scene(pane: LabMaterial) -> LabScene {
    let backdrop = LabMaterial {
        base_color: [0.8, 0.8, 0.8, 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.6,
        near: 0.05,
        far: 20.0,
        depth_prepass: true,
        // No translucency maps: this test is about the view through the pane.
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([-0.3, -0.5, -0.8], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![backdrop, pane],
        meshes: vec![quad(3.0, -2.0, 0), quad(0.5, 0.0, 1)],
    }
}

/// The pane as a standard (hand-written) transmissive material.
fn standard_pane() -> LabMaterial {
    LabMaterial {
        base_color: [TINT[0], TINT[1], TINT[2], 1.0],
        roughness: ROUGHNESS,
        metallic: 0.0,
        transmission: TRANSMISSION,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    }
}

/// The same pane as a material graph (base colour and roughness from parameters), made
/// transmissive by the host as a shell does. The standard block keeps its default grey colour:
/// only the graph knows the tint.
fn graph_pane() -> LabMaterial {
    let mut graph = MaterialGraph::new("glass_tint");
    graph.params.push(ParamDecl::new("tint", ParamValue::Color(TINT)));
    graph
        .params
        .push(ParamDecl::new("roughness", ParamValue::Float(ROUGHNESS)));
    graph.nodes.insert("tint".into(), Node::Param("tint".into()));
    graph
        .nodes
        .insert("roughness".into(), Node::Param("roughness".into()));
    graph.output.base_color = Some("tint".into());
    graph.output.roughness = Some("roughness".into());
    let compiled = regia_matgraph::compile(&graph).expect("compile glass graph");
    let mut material = lab_material(
        &graph,
        &compiled,
        default_params(&graph, 0.0),
        &Default::default(),
    )
    .expect("glass graph material");
    assert_ne!(
        [
            material.base_color[0],
            material.base_color[1],
            material.base_color[2]
        ],
        TINT,
        "the standard block must not carry the tint, or the test cannot tell the paths apart"
    );
    material.transmission = TRANSMISSION;
    material.alpha_mode = ALPHA_PREMULTIPLIED;
    material
}

/// A fresh renderer per frame, so no cache entry is shared between the two versions.
fn render(device: &RenderDevice, scene: &LabScene) -> Vec<u8> {
    FrameRenderer::new()
        .render_offscreen(device, &scene.as_frame_scene_with_quality(FrameQuality::LAB))
        .expect("offscreen frame")
        .pixels
}

/// Mean RGB of the `size` × `size` block whose top-left corner is (`x0`, `y0`).
fn mean_rgb(pixels: &[u8], x0: u32, y0: u32, size: u32) -> [f64; 3] {
    let mut sum = [0.0f64; 3];
    for y in y0..y0 + size {
        for x in x0..x0 + size {
            let at = ((y * WIDTH + x) * 4) as usize;
            for (c, s) in sum.iter_mut().enumerate() {
                *s += f64::from(pixels[at + c]);
            }
        }
    }
    let n = f64::from(size * size);
    sum.map(|s| s / n)
}

#[test]
fn transmissive_graph_glass_tints_what_is_behind_like_the_standard_material() {
    let device = RenderDevice::headless().expect("headless device");
    let standard = render(&device, &scene(standard_pane()));
    let graph = render(&device, &scene(graph_pane()));
    assert_eq!(standard.len(), (WIDTH * HEIGHT * 4) as usize);
    assert_eq!(graph.len(), standard.len());

    // The backdrop alone (a corner): bright and neutral, so any hue at the centre is the pane's.
    let corner = mean_rgb(&standard, 8, 8, 24);
    // The pane covers about ±65 px around the centre; a 40 px block inside it.
    let (cx, cy) = (WIDTH / 2 - 20, HEIGHT / 2 - 20);
    let through_standard = mean_rgb(&standard, cx, cy, 40);
    let through_graph = mean_rgb(&graph, cx, cy, 40);
    eprintln!("backdrop {corner:.1?}, standard pane {through_standard:.1?}, graph pane {through_graph:.1?}");
    assert!(
        corner[1] > 120.0,
        "backdrop too dark to show a tint: {corner:.1?}"
    );
    assert!(
        (corner[0] - corner[1]).abs() < 8.0 && (corner[1] - corner[2]).abs() < 8.0,
        "backdrop is not neutral: {corner:.1?}"
    );

    // Control: the standard transmissive path tints the view through the pane.
    assert!(
        through_standard[0] > 2.0 * through_standard[1] && through_standard[0] > 60.0,
        "standard glass does not tint what is behind: {through_standard:.1?}"
    );
    // The regression: the graph pane tints it too (it came through grey before ADR-347).
    assert!(
        through_graph[0] > 2.0 * through_graph[1] && through_graph[0] > 60.0,
        "graph glass lost its hue: {through_graph:.1?} (standard {through_standard:.1?})"
    );

    // And the two frames agree. Like the graph parity tests (≤ 1/255), plus one step for the
    // two blends a transmissive pane draws with.
    let max = standard
        .iter()
        .zip(&graph)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        max <= 2,
        "graph glass differs from the standard glass by {max}/255"
    );
}
