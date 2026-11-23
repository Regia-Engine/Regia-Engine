// SPDX-License-Identifier: MPL-2.0
//! One mesh drawn both rigid and skinned keeps a stable rest-pose bound (ADR-4577).
//!
//! The cache used to store one sphere and overwrite it from whichever draw visited last. A rigid
//! draw of a mesh whose last visitor was skinned then had no sphere, so the GPU cull dropped it.
//! Both orders must keep both draws, and the rigid draw's sphere is the rest pose under its model.
//! Counters only: no wall-clock assert.

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::{Mat4, Vec3};
use regia_render::view_math::look_at_view;
use regia_render::{
    DirectionalLightUniform, DrawStats, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    Frustum, JointMatrix, LabMaterial, LabVertex, RenderDevice, ShadowCast, frame_path_counters,
};

const W: u32 = 96;
const H: u32 = 64;
const MESH_KEY: u64 = 0x4D_E5_B0_01;
const EYE: [f32; 3] = [0.0, 0.0, 4.0];
const TARGET: [f32; 3] = [0.0, 0.0, 0.0];
const FOV_Y: f32 = 0.9;
const NEAR: f32 = 0.1;
const FAR: f32 = 30.0;
/// Rigid draw of the picture checks. Far enough from the skinned quad that the two do not share
/// pixels, so draw order cannot change the image through the depth test.
const PICTURE_X: f32 = -1.2;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn gpu_driven(device: &RenderDevice) -> bool {
    device.capabilities.multi_draw_indirect && device.capabilities.indirect_first_instance
}

/// A camera-facing quad bound entirely to joint 0.
///
/// The color shader skins every draw whose weights sum above zero, and a rigid draw's palette
/// base is the identity stored at slot 0. A second influence would read the next mesh's joints
/// (the skinned draw's palette is packed immediately after that identity) and walk the rigid quad
/// out of the view. The skinned draw still submits two joints, so it does not fold; only joint 0
/// is weighted, and that joint is what moves its palette sphere.
fn quad() -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [
        ([-0.35, -0.35, 0.0], [0.0, 1.0]),
        ([0.35, -0.35, 0.0], [1.0, 1.0]),
        ([0.35, 0.35, 0.0], [1.0, 0.0]),
        ([-0.35, 0.35, 0.0], [0.0, 0.0]),
    ];
    let vertices = corners
        .into_iter()
        .map(|(position, uv)| {
            let mut vertex = LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv);
            vertex.weights = [1.0, 0.0, 0.0, 0.0];
            vertex.joints = [0, 0, 0, 0];
            vertex
        })
        .collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

/// Rest-pose sphere: AABB center plus the farthest vertex (the renderer's `mesh_bounds`).
fn rest_sphere(vertices: &[LabVertex]) -> ([f32; 3], f32) {
    let first = vertices[0].position;
    let mut lo = first;
    let mut hi = first;
    for vertex in &vertices[1..] {
        for axis in 0..3 {
            lo[axis] = lo[axis].min(vertex.position[axis]);
            hi[axis] = hi[axis].max(vertex.position[axis]);
        }
    }
    let center = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let center_v = Vec3::from_array(center);
    let radius = vertices
        .iter()
        .map(|vertex| (Vec3::from_array(vertex.position) - center_v).length())
        .fold(0.0_f32, f32::max);
    (center, radius)
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    Mat4::from_translation(Vec3::new(x, y, z)).to_cols_array_2d()
}

fn material(color: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        roughness: 1.0,
        metallic: 0.0,
        unlit: true,
        ..LabMaterial::default()
    }
}

/// The unjittered cull frustum the frame builds for this test's camera.
fn cull_frustum() -> Frustum {
    let view = look_at_view(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj = perspective(FOV_Y, W as f32 / H as f32, NEAR, FAR);
    Frustum::from_view_proj(&(proj * view))
}

/// World sphere of the rigid draw: the rest pose under `model` (no inflate, uniform scale 1).
fn rigid_world_sphere(model: [[f32; 4]; 4], rest: ([f32; 3], f32)) -> ([f32; 3], f32) {
    let model = Mat4::from_cols_array_2d(&model);
    let center = model.transform_point3(Vec3::from_array(rest.0));
    let scale = model
        .x_axis
        .length_squared()
        .max(model.y_axis.length_squared())
        .max(model.z_axis.length_squared())
        .sqrt();
    (center.to_array(), rest.1 * scale)
}

struct Owned {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    red: LabMaterial,
    blue: LabMaterial,
    identity_palette: [JointMatrix; 2],
    far_palette: [JointMatrix; 2],
}

impl Owned {
    fn new() -> Self {
        let (vertices, indices) = quad();
        Self {
            vertices,
            indices,
            red: material([0.9, 0.15, 0.1]),
            blue: material([0.1, 0.25, 0.95]),
            identity_palette: [JointMatrix::identity(), JointMatrix::identity()],
            far_palette: [
                JointMatrix::translation(400.0, 0.0, 0.0),
                JointMatrix::translation(400.0, 0.0, 0.0),
            ],
        }
    }

    fn rest(&self) -> ([f32; 3], f32) {
        rest_sphere(&self.vertices)
    }
}

#[derive(Clone, Copy)]
struct DrawSpec<'a> {
    object_key: u64,
    material_key: u64,
    material: &'a LabMaterial,
    model: [[f32; 4]; 4],
    joints: &'a [JointMatrix],
}

fn scene<'a>(owned: &'a Owned, draws: &[DrawSpec<'a>], shadows: bool) -> FrameScene<'a> {
    let quality = FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    };
    FrameScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: shadows,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.2, -0.8, -0.4], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: &[],
        quality,
        meshes: draws
            .iter()
            .map(|draw| FrameMesh {
                is_static: false,
                mesh_key: MESH_KEY,
                material_key: draw.material_key,
                object_key: draw.object_key,
                vertices: &owned.vertices,
                indices: &owned.indices,
                material: draw.material,
                model: draw.model,
                joints: draw.joints,
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            })
            .collect(),
    }
}

fn render(device: &RenderDevice, scene: &FrameScene<'_>) -> (Vec<u8>, DrawStats) {
    let mut renderer = FrameRenderer::new();
    let image = renderer.render_offscreen(device, scene).expect("frame");
    (image.pixels, renderer.draw_stats())
}

fn differs(a: &[u8], b: &[u8]) -> bool {
    a != b
}

/// Full-buffer equality. A mismatch reports the first pixel and how many differ, not the buffers.
fn assert_same_picture(left: &[u8], right: &[u8], what: &str) {
    if left == right {
        return;
    }
    if left.len() != right.len() {
        panic!("{what}: picture len {} != {}", left.len(), right.len());
    }
    let n = left.len() / 4;
    let mut mismatch = 0usize;
    let mut first = None;
    for pixel in 0..n {
        let offset = pixel * 4;
        if left[offset..offset + 4] != right[offset..offset + 4] {
            mismatch += 1;
            if first.is_none() {
                first = Some(pixel);
            }
        }
    }
    let sample = |buf: &[u8], pixel: usize| {
        let offset = pixel * 4;
        [buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]]
    };
    let pixel = first.unwrap_or(0);
    panic!(
        "{what}: {mismatch} pixels differ (len {} vs {}), first pixel {pixel} left {:?} right {:?}",
        left.len(),
        right.len(),
        sample(left, pixel),
        sample(right, pixel),
    );
}

/// Rigid then skinned, and skinned then rigid. `skin` is the palette; the rigid draw has none.
fn both_orders<'a>(owned: &'a Owned, rigid_x: f32, skin: &'a [JointMatrix]) -> [Vec<DrawSpec<'a>>; 2] {
    let rigid = DrawSpec {
        object_key: 0x4D_E5_B0_21,
        material_key: 0x4D_E5_B0_11,
        material: &owned.red,
        model: translate(rigid_x, 0.0, 0.0),
        joints: &[],
    };
    let skinned = DrawSpec {
        object_key: 0x4D_E5_B0_22,
        material_key: 0x4D_E5_B0_12,
        material: &owned.blue,
        model: translate(0.0, 0.0, 0.0),
        joints: skin,
    };
    [vec![rigid, skinned], vec![skinned, rigid]]
}

#[test]
fn a_mesh_drawn_rigid_and_skinned_keeps_its_rest_bound_in_both_orders() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !gpu_driven(&device) {
        eprintln!("device has no indirect first-instance; the GPU cull this test pins is off");
        return;
    }
    let owned = Owned::new();
    let rest = owned.rest();
    let frustum = cull_frustum();

    // Both draws inside the view. The skinned palette is the rest pose, so its own sphere is
    // inside too. Either order must keep both draws, and the pictures match.
    let orders = both_orders(&owned, PICTURE_X, &owned.identity_palette);
    let mut pictures = Vec::new();
    for (index, draws) in orders.iter().enumerate() {
        let (pixels, stats) = render(&device, &scene(&owned, draws, false));
        assert_eq!(stats.drawn, 2, "order {index}: both draws survive, {stats:?}");
        assert_eq!(
            stats.culled, 0,
            "order {index}: neither sphere misses the view, {stats:?}"
        );
        assert_eq!(
            stats.instanced, 2,
            "order {index}: the rigid draw and the skinned draw both joined the GPU cull, {stats:?}"
        );
        assert_eq!(stats.skinned_draws, 1, "order {index}: {stats:?}");
        assert_eq!(stats.skinned_instanced, 1, "order {index}: {stats:?}");
        let world = rigid_world_sphere(translate(PICTURE_X, 0.0, 0.0), rest);
        assert!(
            frustum.intersects_sphere(world.0, world.1),
            "the rest sphere at x={PICTURE_X} is inside the view: {world:?}"
        );
        eprintln!(
            "order {index}: drawn={}, instanced={}, skinned_instanced={}, culled={}",
            stats.drawn, stats.instanced, stats.skinned_instanced, stats.culled
        );
        pictures.push(pixels);
    }
    assert_same_picture(
        &pictures[0],
        &pictures[1],
        "rigid-then-skinned and skinned-then-rigid drew different frames",
    );

    // The first two renderers above start cold, covering both insertion orders. Reuse one
    // renderer for the reference pictures and boundary sweep: this also exercises resident
    // entries without compiling a fresh set of pipelines for every camera-space sample.
    let mut comparison_renderer = FrameRenderer::new();
    let mut render = |device: &RenderDevice, scene: &FrameScene<'_>| {
        let image = comparison_renderer
            .render_offscreen(device, scene)
            .expect("frame");
        (image.pixels, comparison_renderer.draw_stats())
    };

    let (rigid_only, rigid_stats) = render(
        &device,
        &scene(
            &owned,
            &[DrawSpec {
                object_key: 0x4D_E5_B0_21,
                material_key: 0x4D_E5_B0_11,
                material: &owned.red,
                model: translate(PICTURE_X, 0.0, 0.0),
                joints: &[],
            }],
            false,
        ),
    );
    let (skinned_only, skinned_stats) = render(
        &device,
        &scene(
            &owned,
            &[DrawSpec {
                object_key: 0x4D_E5_B0_22,
                material_key: 0x4D_E5_B0_12,
                material: &owned.blue,
                model: translate(0.0, 0.0, 0.0),
                joints: &owned.identity_palette,
            }],
            false,
        ),
    );
    assert_eq!(rigid_stats.culled, 0, "{rigid_stats:?}");
    assert_eq!(skinned_stats.culled, 0, "{skinned_stats:?}");
    assert!(
        differs(&pictures[0], &rigid_only),
        "the skinned draw is missing from the mixed frame"
    );
    assert!(
        differs(&pictures[0], &skinned_only),
        "the rigid draw is missing from the mixed frame"
    );

    // The rigid draw's sphere is the rest pose: the CPU cull agrees with that sphere, including
    // a step that sits just inside it and a step that sits just outside it. A missing sphere
    // would not cull; a larger one would still include the outside step.
    let outside_x = 8.0_f32;
    let at = |x: f32| rigid_world_sphere(translate(x, 0.0, 0.0), rest);
    assert!(
        frustum.intersects_sphere(at(0.0).0, at(0.0).1),
        "the rest sphere at the origin is inside"
    );
    assert!(
        !frustum.intersects_sphere(at(outside_x).0, at(outside_x).1),
        "the rest sphere at x={outside_x} is outside"
    );
    let samples = [-0.7_f32, 0.0, 1.5, 3.0, outside_x];
    for x in samples {
        let world = rigid_world_sphere(translate(x, 0.0, 0.0), rest);
        let inside = frustum.intersects_sphere(world.0, world.1);
        for draws in &both_orders(&owned, x, &owned.identity_palette) {
            let (pixels, stats) = render(&device, &scene(&owned, draws, false));
            let rigid_culled = stats.culled == 1;
            assert_eq!(
                rigid_culled, !inside,
                "rigid x={x}: rest sphere {world:?} inside={inside}, stats={stats:?}"
            );
            assert_eq!(stats.drawn, if inside { 2 } else { 1 }, "x={x}: {stats:?}");
            assert_eq!(stats.skinned_draws, 1, "the skinned draw stays, x={x}: {stats:?}");
            if !inside {
                assert_same_picture(
                    &pixels,
                    &skinned_only,
                    &format!("a rigid draw outside its rest sphere must not change the picture, x={x}"),
                );
            }
        }
    }

    // A skinned draw ignores the rest pose. Its joints sit far outside the view while the rest
    // sphere (and the rigid draw) stay inside, so exactly the skinned draw is culled, in both
    // orders, and the picture is the rigid draw alone.
    let (rigid_home, _) = render(
        &device,
        &scene(
            &owned,
            &[DrawSpec {
                object_key: 0x4D_E5_B0_21,
                material_key: 0x4D_E5_B0_11,
                material: &owned.red,
                model: translate(0.0, 0.0, 0.0),
                joints: &[],
            }],
            false,
        ),
    );
    for draws in &both_orders(&owned, 0.0, &owned.far_palette) {
        let (pixels, stats) = render(&device, &scene(&owned, draws, false));
        assert_eq!(
            stats.culled, 1,
            "the palette sphere is outside, the rest pose is not: {stats:?}"
        );
        assert_eq!(stats.drawn, 1, "{stats:?}");
        assert_eq!(
            stats.skinned_draws, 0,
            "a culled skin is not a color draw: {stats:?}"
        );
        assert_eq!(
            stats.instanced, 1,
            "the rigid draw still joined the cull: {stats:?}"
        );
        assert_same_picture(
            &pixels,
            &rigid_home,
            "the skinned draw must not use the rest sphere",
        );
    }

    // Shadow casters read the same rule: both orders of a lit frame match.
    let lit = both_orders(&owned, PICTURE_X, &owned.identity_palette);
    let (lit_a, _) = render(&device, &scene(&owned, &lit[0], true));
    let (lit_b, _) = render(&device, &scene(&owned, &lit[1], true));
    assert_same_picture(&lit_a, &lit_b, "shadow caster bounds followed the last visitor");

    // A steady mixed frame creates no pipeline and no buffer.
    let mut renderer = FrameRenderer::new();
    let steady_scene = scene(&owned, &orders[0], false);
    for _ in 0..4 {
        renderer
            .render_offscreen(&device, &steady_scene)
            .expect("warm frame");
    }
    let scratch_before = renderer.frame_scratch_stats().growths;
    let before = frame_path_counters();
    renderer
        .render_offscreen(&device, &steady_scene)
        .expect("steady frame");
    let delta = frame_path_counters().since(before);
    assert_eq!(
        renderer.frame_scratch_stats().growths,
        scratch_before,
        "a steady frame grew scratch storage"
    );
    eprintln!("steady frame counters: {delta:?}");
    assert_eq!(delta.pipelines, 0, "a steady frame created a pipeline: {delta:?}");
    assert_eq!(delta.buffers, 0, "a steady frame allocated a buffer: {delta:?}");
    assert_eq!(delta.textures, 0, "a steady frame allocated a texture: {delta:?}");
}
