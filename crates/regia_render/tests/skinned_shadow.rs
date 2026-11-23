// SPDX-License-Identifier: MPL-2.0
//! Skinned casters in the dynamic-caster shadow class (`specs/88`, ADR-2028, ADR-5418), on any
//! device (lavapipe or a card).
//!
//! A crowd of two-joint skinned units stands on a ground plane under a low sun, with paged sun
//! cascades. Joint 1 turns each unit's upper half (a vertical card) a quarter turn about the
//! vertical axis. Four pin vertices on joint 1 make that half's bind box square, so the quarter
//! turn leaves the joint-palette bound (ADR-248) where it was: the pose changes inside the bound,
//! as a wind sway does inside a plant's inflated bound. The page cache cannot see that move. The
//! oracle is a fresh renderer, which draws every page:
//!
//! - class on: after the pose changes, the warm renderer matches the fresh one within 2 levels;
//! - class off: the warm renderer's cached pages keep the pose they were drawn with;
//! - a held pose moves no pixel more than 2 levels from one frame to the next;
//! - a pose that moves the bound redraws its pages with the class off too.
//!
//! The joint cap (`MAX_JOINTS_PER_SKIN`): a skin whose vertices weight a joint past its palette
//! or past the cap is refused and counted in `DrawIntegrity::skinned_refused`, never drawn with
//! another draw's joints.
//!
//! No golden: every check compares two renderers of the same build or reads the counters.

use std::f32::consts::FRAC_PI_2;

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, DynamicCasterShadows, FrameQuality, FrameRenderer, FrameScene, JointMatrix,
    LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabVertex, MAX_JOINTS_PER_SKIN,
    RenderDevice, RenderProfile, RenderProfilePreset,
};

const SIZE: u32 = 96;
/// Units in the crowd.
const UNITS: usize = 4;
const UNIT_MESH: u64 = 0x5D_0001;
const GROUND_MESH: u64 = 0x5D_0002;
const UNIT_MATERIAL: u64 = 0x5D_1000;
const GROUND_MATERIAL: u64 = 0x5D_1001;
const OBJECT_BASE: u64 = 0x5D_2000;

/// Serialises device creation and teardown across this binary's test threads (ADR-1438), as
/// `skinned_instancing.rs` does. Declared first in each test, so the device drops while held.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    Mat4::from_translation(Vec3::new(x, y, z)).to_cols_array_2d()
}

fn vertex(position: [f32; 3], normal: [f32; 3], joint: u32) -> LabVertex {
    let mut v = LabVertex::from_pos_normal_uv(position, normal, [0.0, 0.0]);
    v.joints = [joint, 0, 0, 0];
    v.weights = [1.0, 0.0, 0.0, 0.0];
    v
}

/// One unit: a 0.2 m post (joint 0) from the ground to 0.4 m, and a 1.0 × 1.2 m card (joint 1)
/// in the plane z = 0 from 0.4 m to 1.6 m. Four pin vertices on joint 1 at the corners of a
/// 1 × 1 m square (no triangle uses them) make joint 1's bind box square in x and z.
fn unit_mesh() -> LabMesh {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let (post, _) = regia_render::box_mesh([0.1, 0.2, 0.1]);
    let (_, post_indices) = regia_render::box_mesh([0.1, 0.2, 0.1]);
    for v in post {
        let mut p = v.position;
        p[1] += 0.2;
        vertices.push(vertex(p, v.normal, 0));
    }
    indices.extend(post_indices);
    let base = vertices.len() as u32;
    for (x, y) in [(-0.5, 0.4), (0.5, 0.4), (0.5, 1.6), (-0.5, 1.6)] {
        vertices.push(vertex([x, y, 0.0], [0.0, 0.0, 1.0], 1));
    }
    indices.extend([0, 1, 2, 0, 2, 3].map(|i| base + i));
    for (x, z) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
        vertices.push(vertex([x, 1.0, z], [0.0, 1.0, 0.0], 1));
    }
    LabMesh {
        vertices,
        indices,
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn ground() -> LabMesh {
    let e = 4.0;
    let n = [0.0, 1.0, 0.0];
    LabMesh {
        vertices: [[-e, 0.0, -e], [e, 0.0, -e], [e, 0.0, e], [-e, 0.0, e]]
            .into_iter()
            .map(|p| LabVertex::from_pos_normal_uv(p, n, [0.0, 0.0]))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
        material: 1,
        ..LabMesh::unskinned()
    }
}

/// Where unit `i` stands (x, z).
fn spot(i: usize) -> (f32, f32) {
    let x = if i.is_multiple_of(2) { -1.6 } else { 0.2 };
    let z = if i < 2 { -1.3 } else { 1.1 };
    (x, z)
}

/// Unit `i`'s palette at `step`: joint 1 turned `(step + i) mod 2` quarter turns about the
/// vertical axis through the unit's origin, so every unit flips each step and neighbours
/// alternate. A quarter turn keeps joint 1's square bind box exactly.
fn pose(step: usize, i: usize) -> Vec<JointMatrix> {
    let turns = ((step + i) % 2) as f32;
    vec![
        JointMatrix::identity(),
        JointMatrix::from_mat4(Mat4::from_rotation_y(turns * FRAC_PI_2)),
    ]
}

/// The palette at `step` with joint 1 also slid `slide` metres along +z: the bound moves.
fn slid_pose(step: usize, i: usize, slide: f32) -> Vec<JointMatrix> {
    let turns = ((step + i) % 2) as f32;
    vec![
        JointMatrix::identity(),
        JointMatrix::from_mat4(
            Mat4::from_translation(Vec3::new(0.0, 0.0, slide)) * Mat4::from_rotation_y(turns * FRAC_PI_2),
        ),
    ]
}

fn lab(palettes: &dyn Fn(usize) -> Vec<JointMatrix>) -> LabScene {
    let unit = unit_mesh();
    let mut meshes = vec![ground()];
    for i in 0..UNITS {
        let (x, z) = spot(i);
        meshes.push(LabMesh {
            model: translate(x, 0.0, z),
            joints: palettes(i),
            ..unit.clone()
        });
    }
    LabScene {
        width: SIZE,
        height: SIZE,
        eye: [0.5, 4.5, 4.5],
        target: [0.6, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 30.0,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.8, -0.6, 0.1], [1.0; 3], 40_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.7, 0.45, 0.3, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
            LabMaterial::default(),
        ],
        meshes,
    }
}

fn paged() -> FrameQuality {
    FrameQuality {
        paged_sun_shadows: true,
        ..FrameQuality::LAB
    }
}

/// Stable keys: one mesh (`unit_key`) and material for the units, one object key each.
fn keyed_as(lab: &LabScene, quality: FrameQuality, unit_key: u64) -> FrameScene<'_> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    for (i, mesh) in scene.meshes.iter_mut().enumerate() {
        mesh.object_key = OBJECT_BASE + i as u64;
        if i == 0 {
            mesh.mesh_key = GROUND_MESH;
            mesh.material_key = GROUND_MATERIAL;
        } else {
            mesh.mesh_key = unit_key;
            mesh.material_key = UNIT_MATERIAL;
        }
    }
    scene
}

/// A frame of `lab` with the units under mesh key `unit_key` (a mesh key names its vertices).
fn render_as(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene, unit_key: u64) -> Vec<u8> {
    match renderer.render_offscreen(device, &keyed_as(lab, paged(), unit_key)) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
    }
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    render_as(renderer, device, lab, UNIT_MESH)
}

/// The crowd at `step`.
fn at(step: usize) -> LabScene {
    lab(&|i| pose(step, i))
}

/// Pixels (RGBA8) whose largest channel difference is above `tolerance`.
fn differing(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > tolerance))
        .count()
}

/// 1 % of the frame's pixels.
const ONE_PERCENT: usize = (SIZE * SIZE / 100) as usize;

const ON: DynamicCasterShadows = DynamicCasterShadows::On {
    cascades: 2,
    local_lights: false,
};

/// The quarter turn really is inside the bound, and really moves the shadow.
#[test]
fn the_poses_move_the_shadow_by_more_than_one_percent() {
    let _gpu = serial();
    let device = device();
    let a = render(&mut FrameRenderer::new(), &device, &at(0));
    let b = render(&mut FrameRenderer::new(), &device, &at(1));
    let moved = differing(&a, &b, 2);
    println!("poses differ on {moved} of {} pixels", SIZE * SIZE);
    assert!(
        moved > ONE_PERCENT,
        "the quarter turn must move the cards and their shadows on > 1 % of pixels: {moved}"
    );
    // A fresh renderer draws the same frame twice.
    let again = render(&mut FrameRenderer::new(), &device, &at(1));
    assert_eq!(differing(&b, &again, 2), 0, "a fresh frame is reproducible");
}

#[test]
fn an_animated_crowds_shadow_follows_the_pose_with_the_class_on() {
    let _gpu = serial();
    let device = device();
    let truth = render(&mut FrameRenderer::new(), &device, &at(1));

    let mut renderer = FrameRenderer::new();
    renderer.set_dynamic_caster_shadows(ON);
    render(&mut renderer, &device, &at(0));
    render(&mut renderer, &device, &at(0));
    let warm = render(&mut renderer, &device, &at(1));
    let report = renderer.dynamic_caster_report();
    assert_eq!(
        report.animated_casters, UNITS as u32,
        "every bounded skinned caster is in the class: {report:?}"
    );
    assert_eq!(report.skinned_casters, UNITS as u32, "{report:?}");
    assert_eq!(report.policy.cascade_mask, 0b11, "{report:?}");
    assert!(
        report.dynamic_pages > 0,
        "the units' pages are refreshed: {report:?}"
    );
    let stale = differing(&warm, &truth, 2);
    assert_eq!(
        stale, 0,
        "with the class on the cached frame matches a full redraw at the same pose"
    );
    // And it keeps matching as the crowd keeps moving.
    for step in 2..5 {
        let warm = render(&mut renderer, &device, &at(step));
        let truth = render(&mut FrameRenderer::new(), &device, &at(step));
        assert_eq!(differing(&warm, &truth, 2), 0, "step {step}");
    }
}

#[test]
fn with_the_class_off_the_cached_shadow_keeps_the_drawn_pose() {
    let _gpu = serial();
    let device = device();
    let truth_a = render(&mut FrameRenderer::new(), &device, &at(0));
    let truth_b = render(&mut FrameRenderer::new(), &device, &at(1));
    let moved = differing(&truth_a, &truth_b, 2);

    let mut renderer = FrameRenderer::new();
    renderer.set_dynamic_caster_shadows(DynamicCasterShadows::Off);
    render(&mut renderer, &device, &at(0));
    render(&mut renderer, &device, &at(0));
    let warm = render(&mut renderer, &device, &at(1));
    let report = renderer.dynamic_caster_report();
    assert_eq!(report.animated_casters, UNITS as u32, "{report:?}");
    assert_eq!(report.dynamic_pages, 0, "{report:?}");
    assert!(
        !report.policy.refreshes(),
        "nothing is refreshed with the class off: {report:?}"
    );
    let stale = differing(&warm, &truth_b, 2);
    println!("class off: {stale} stale pixels (the poses differ on {moved})");
    // The cards draw at pose B; only their shadows (cached at pose A) are stale.
    assert!(
        stale > ONE_PERCENT / 2,
        "the cached pages hold pose A, so the shadow differs from a full redraw at B: {stale} pixels \
         (the poses differ on {moved})"
    );
    assert!(
        stale < moved,
        "the cards themselves draw at pose B: {stale} of {moved}"
    );
}

#[test]
fn a_held_pose_moves_no_pixel_more_than_two_levels_between_frames() {
    let _gpu = serial();
    let device = device();
    for mode in [ON, DynamicCasterShadows::Tier, DynamicCasterShadows::Off] {
        let mut renderer = FrameRenderer::new();
        renderer.set_dynamic_caster_shadows(mode);
        render(&mut renderer, &device, &at(0));
        let mut last = render(&mut renderer, &device, &at(1));
        for frame in 0..4 {
            let next = render(&mut renderer, &device, &at(1));
            let jumped = differing(&last, &next, 2);
            assert_eq!(
                jumped, 0,
                "{mode:?} frame {frame}: a held pose moved {jumped} pixels by more than 2 levels"
            );
            last = next;
        }
    }
}

/// A skinned pose is known on the CPU: a unit whose palette is the one it drew with last frame is
/// not refreshed, even with the class on, so a held pose costs no dynamic page. An animation
/// updated every other frame (animation LOD) stays exact: the frame that changes the palette
/// refreshes, the frame that repeats it keeps the page.
#[test]
fn a_held_pose_is_not_refreshed_and_a_half_rate_animation_stays_exact() {
    let _gpu = serial();
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.set_dynamic_caster_shadows(ON);
    render(&mut renderer, &device, &at(0));
    render(&mut renderer, &device, &at(1));
    let posed = renderer.dynamic_caster_report();
    assert_eq!(posed.skinned_casters, UNITS as u32, "{posed:?}");
    assert!(posed.dynamic_pages > 0, "{posed:?}");
    render(&mut renderer, &device, &at(1));
    let held = renderer.dynamic_caster_report();
    assert_eq!(
        held.skinned_casters, 0,
        "a held pose is not in the class: {held:?}"
    );
    assert_eq!(held.animated_casters, 0, "{held:?}");
    assert_eq!(held.dynamic_pages, 0, "a held pose refreshes nothing: {held:?}");
    // Half-rate animation: each pose drawn on two frames.
    for frame in 4..12 {
        let step = frame / 2;
        let warm = render(&mut renderer, &device, &at(step));
        let report = renderer.dynamic_caster_report();
        let expected = if frame.is_multiple_of(2) { UNITS as u32 } else { 0 };
        assert_eq!(report.skinned_casters, expected, "frame {frame}: {report:?}");
        let truth = render(&mut FrameRenderer::new(), &device, &at(step));
        assert_eq!(differing(&warm, &truth, 2), 0, "frame {frame}");
    }
}

/// With the class off a skinned caster is still a moving caster: a pose that moves its palette
/// bound dirties the pages it covered and covers, so the shadow follows.
#[test]
fn a_pose_that_moves_the_bound_redraws_with_the_class_off() {
    let _gpu = serial();
    let device = device();
    let slid = lab(&|i| slid_pose(1, i, 0.4));
    let truth = render(&mut FrameRenderer::new(), &device, &slid);
    let mut renderer = FrameRenderer::new();
    renderer.set_dynamic_caster_shadows(DynamicCasterShadows::Off);
    render(&mut renderer, &device, &at(0));
    render(&mut renderer, &device, &at(0));
    let warm = render(&mut renderer, &device, &slid);
    assert_eq!(differing(&warm, &truth, 2), 0);
}

/// Skinned instancing (ADR-1368) on or off draws the same shadows with the class on.
#[test]
fn skinned_instancing_does_not_change_the_refreshed_shadow() {
    let _gpu = serial();
    let device = device();
    let frames = |instancing: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_skinned_instancing(instancing);
        renderer.set_dynamic_caster_shadows(ON);
        render(&mut renderer, &device, &at(0));
        render(&mut renderer, &device, &at(0));
        render(&mut renderer, &device, &at(1))
    };
    assert_eq!(differing(&frames(true), &frames(false), 2), 0);
}

/// Without a palette bound (bounds switched off) a skinned caster is unbounded: every cascade is
/// redrawn every frame, whatever the class says, and the shadow is right.
#[test]
fn an_unbounded_skin_redraws_whole_cascades_in_any_mode() {
    let _gpu = serial();
    let device = device();
    let truth = render(&mut FrameRenderer::new(), &device, &at(1));
    let mut renderer = FrameRenderer::new();
    renderer.set_skinned_bounds(false);
    renderer.set_dynamic_caster_shadows(DynamicCasterShadows::Off);
    render(&mut renderer, &device, &at(0));
    render(&mut renderer, &device, &at(0));
    let warm = render(&mut renderer, &device, &at(1));
    let report = renderer.dynamic_caster_report();
    assert_eq!(report.skinned_casters, 0, "{report:?}");
    assert_eq!(differing(&warm, &truth, 2), 0);
}

/// The class's local-light half: a posed unit keeps its light's atlas tile redrawing.
#[test]
fn a_skinned_casters_atlas_tile_is_redrawn_with_local_lights() {
    let _gpu = serial();
    let device = device();
    let light = LabLocalLight::Point(LabPointLight {
        position: [-0.6, 1.2, -1.3],
        color: [1.0; 3],
        intensity: 2_000.0,
        radius: 6.0,
        cast_shadows: true,
        source_radius: None,
    });
    let tiles = |local_lights: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_dynamic_caster_shadows(DynamicCasterShadows::On {
            cascades: 1,
            local_lights,
        });
        let mut last = Vec::new();
        for step in 0..3 {
            let mut scene = at(step);
            scene.local_lights = vec![light];
            last = render(&mut renderer, &device, &scene);
        }
        (renderer.dynamic_caster_report(), last)
    };
    let (on, warm) = tiles(true);
    assert!(on.dynamic_atlas_tiles >= 1, "{on:?}");
    let (off, _) = tiles(false);
    assert_eq!(off.dynamic_atlas_tiles, 0, "{off:?}");
    let mut scene = at(2);
    scene.local_lights = vec![light];
    let truth = render(&mut FrameRenderer::new(), &device, &scene);
    assert_eq!(
        differing(&warm, &truth, 2),
        0,
        "the point light's shadow follows the pose"
    );
}

// ---- The joint cap ----

/// A one-unit scene (the ground and unit 0) whose unit weights joint `joint` with a palette of
/// `palette` joints (joint 1 of the palette is the quarter turn, the rest identity).
fn cap_scene(joint: u32, palette: usize) -> LabScene {
    let mut scene = at(1);
    scene.meshes.truncate(2);
    for v in &mut scene.meshes[1].vertices {
        if v.joints[0] == 1 {
            v.joints[0] = joint;
        }
    }
    let mut joints = vec![JointMatrix::identity(); palette];
    if let Some(slot) = joints.get_mut(joint as usize) {
        *slot = JointMatrix::from_mat4(Mat4::from_rotation_y(FRAC_PI_2));
    }
    scene.meshes[1].joints = joints;
    scene
}

/// The mesh key of [`cap_scene`]'s unit: its vertices differ per weighted joint.
fn cap_key(joint: u32) -> u64 {
    0x5D_8000_0000 | u64::from(joint)
}

fn render_cap(renderer: &mut FrameRenderer, device: &RenderDevice, joint: u32, palette: usize) -> Vec<u8> {
    render_as(renderer, device, &cap_scene(joint, palette), cap_key(joint))
}

fn ground_only() -> LabScene {
    let mut scene = at(1);
    scene.meshes.truncate(1);
    scene
}

#[test]
fn a_skin_inside_the_cap_draws() {
    let _gpu = serial();
    let device = device();
    let last = MAX_JOINTS_PER_SKIN as u32 - 1;
    let mut renderer = FrameRenderer::new();
    let at_cap = render_cap(&mut renderer, &device, last, MAX_JOINTS_PER_SKIN);
    assert_eq!(renderer.draw_integrity().skinned_refused, 0);
    let small = render_cap(&mut FrameRenderer::new(), &device, 1, 2);
    assert_eq!(
        differing(&at_cap, &small, 2),
        0,
        "joint 255 of a 256-joint palette draws like joint 1 of a 2-joint one"
    );
    // A palette longer than the cap whose vertices stay under it draws its first 256 joints.
    let mut renderer = FrameRenderer::new();
    let long = render_cap(&mut renderer, &device, 1, MAX_JOINTS_PER_SKIN + 40);
    assert_eq!(renderer.draw_integrity().skinned_refused, 0);
    assert_eq!(differing(&long, &small, 2), 0);
}

#[test]
fn a_skin_past_the_cap_or_its_palette_is_refused_and_counted() {
    let _gpu = serial();
    let device = device();
    let empty = render(&mut FrameRenderer::new(), &device, &ground_only());
    for (joint, palette, why) in [
        (
            MAX_JOINTS_PER_SKIN as u32 + 20,
            MAX_JOINTS_PER_SKIN + 40,
            "past the cap",
        ),
        (5, 2, "past its palette"),
    ] {
        let mut renderer = FrameRenderer::new();
        let refused = render_cap(&mut renderer, &device, joint, palette);
        let integrity = renderer.draw_integrity();
        assert_eq!(integrity.skinned_refused, 1, "{why}: {integrity:?}");
        assert_eq!(
            differing(&refused, &empty, 2),
            0,
            "{why}: the refused unit draws nothing (no other draw's joints) and casts nothing"
        );
        // The count is per frame: the next frame counts it again, a fixed one counts nothing.
        render_cap(&mut renderer, &device, joint, palette);
        assert_eq!(renderer.draw_integrity().skinned_refused, 1, "{why}");
        render_cap(&mut renderer, &device, 1, 2);
        assert_eq!(renderer.draw_integrity().skinned_refused, 0, "{why}");
    }
}

// ---- Tier cost ----

/// Mesh key of the bench's static boxes (rigid, one mesh: they instance).
const PROP_MESH: u64 = 0x5D_0003;
const PROP_MATERIAL: u64 = 0x5D_1002;

/// How the bench crowd moves.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    /// Each unit turns its upper half a little every frame, in place.
    Turning,
    /// Turning, and walking 4 cm along +z per frame (the model moves).
    Walking,
    /// One pose held on every frame (a paused or idle crowd).
    Standing,
}

/// The bench scene at `step`: a 24 m ground, 1 200 static boxes (the scenery a whole-cascade
/// redraw pays for) and a 170-unit crowd (the DO27 chase count) moving as `motion` says.
fn bench_scene(step: usize, motion: Motion) -> LabScene {
    let step_pose = if motion == Motion::Standing { 0 } else { step };
    let unit = unit_mesh();
    let mut lab = at(step);
    lab.width = 1920;
    lab.height = 1080;
    lab.meshes.truncate(1);
    lab.meshes[0].vertices.iter_mut().for_each(|v| {
        v.position[0] *= 3.0;
        v.position[2] *= 3.0;
    });
    let (prop_vertices, prop_indices) = regia_render::box_mesh([0.2, 0.4, 0.2]);
    for k in 0..1200 {
        let (x, z) = ((k % 40) as f32 * 0.6 - 11.7, (k / 40) as f32 * 0.8 - 11.6);
        lab.meshes.push(LabMesh {
            vertices: prop_vertices.clone(),
            indices: prop_indices.clone(),
            material: 1,
            model: translate(x, 0.4, z),
            ..LabMesh::unskinned()
        });
    }
    for i in 0..170 {
        let walked = if motion == Motion::Walking {
            0.04 * step as f32
        } else {
            0.0
        };
        let (x, z) = ((i % 17) as f32 * 1.2 - 10.0, (i / 17) as f32 * 1.2 - 6.0 + walked);
        // A continuous turn: each unit at its own phase, so the crowd moves every frame.
        let angle = 0.15 * step_pose as f32 + i as f32 * 0.7;
        lab.meshes.push(LabMesh {
            model: translate(x, 0.0, z),
            joints: vec![
                JointMatrix::identity(),
                JointMatrix::from_mat4(Mat4::from_rotation_y(angle)),
            ],
            ..unit.clone()
        });
    }
    lab.eye = [0.0, 9.0, 14.0];
    lab.target = [0.0, 0.0, 0.0];
    lab.far = 80.0;
    lab
}

/// [`bench_scene`]'s keys: the ground, one instanced mesh for the boxes, one for the units.
fn bench_keyed(lab: &LabScene, quality: FrameQuality) -> FrameScene<'_> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    for (i, mesh) in scene.meshes.iter_mut().enumerate() {
        mesh.object_key = OBJECT_BASE + i as u64;
        (mesh.mesh_key, mesh.material_key) = match i {
            0 => (GROUND_MESH, GROUND_MATERIAL),
            _ if mesh.joints.is_empty() => (PROP_MESH, PROP_MATERIAL),
            _ => (UNIT_MESH, UNIT_MATERIAL),
        };
    }
    scene
}

/// Per tier (Desktop Low, the Deck, Desktop High, Desktop Ultra) and crowd motion (turning in
/// place, walking, standing), the GPU frame and shadow-section medians of four renderers drawn in turn,
/// frame by frame (one card, interleaved, so clocks and heat are shared): the class off, on
/// (two cascades and the atlas), the tier default, and the bounds off (every skinned caster
/// unbounded: the whole-cascade redraw every frame, the path before ADR-5418). Run on a card
/// with `cargo test -p regia_render --test skinned_shadow -- --ignored --nocapture`.
#[test]
#[ignore = "timing print for the lane report; no assertion on time"]
fn skinned_tier_cost_bench() {
    let _gpu = serial();
    let device = device();
    if !device.capabilities.timestamp_query {
        println!("skinned_tier_cost_bench: no TIMESTAMP_QUERY on this device, nothing measured");
        return;
    }
    let tiers = [
        ("Low", RenderProfilePreset::DesktopLow),
        ("Medium (Deck)", RenderProfilePreset::Deck),
        ("High", RenderProfilePreset::DesktopHigh),
        ("Ultra", RenderProfilePreset::DesktopUltra),
    ];
    let arms = [
        ("class off", DynamicCasterShadows::Off, true),
        (
            "class on",
            DynamicCasterShadows::On {
                cascades: 2,
                local_lights: true,
            },
            true,
        ),
        ("tier default", DynamicCasterShadows::Tier, true),
        ("bounds off (unbounded)", DynamicCasterShadows::Tier, false),
    ];
    const STEPS: usize = 48;
    const WARM: usize = 12;
    for motion in [Motion::Turning, Motion::Walking, Motion::Standing] {
        for (tier, preset) in tiers {
            let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
            let mut renderers: Vec<FrameRenderer> = arms
                .iter()
                .map(|&(_, mode, bounds)| {
                    let mut renderer = FrameRenderer::new();
                    renderer.set_dynamic_caster_shadows(mode);
                    renderer.set_skinned_bounds(bounds);
                    renderer.set_gpu_timing(&device, true);
                    renderer
                })
                .collect();
            let mut samples = vec![(Vec::new(), Vec::new(), 0u32); arms.len()];
            for step in 0..STEPS {
                let lab = bench_scene(step, motion);
                let scene = bench_keyed(&lab, quality);
                for (k, renderer) in renderers.iter_mut().enumerate() {
                    if let Err(err) = renderer.render_offscreen(&device, &scene) {
                        panic!("{tier} {}: {err}", arms[k].0);
                    }
                    if step >= WARM {
                        let t = renderer.gpu_timings();
                        samples[k].0.push(t.frame_ms);
                        samples[k].1.push(t.shadows_ms);
                        samples[k].2 += renderer.dynamic_caster_report().dynamic_pages;
                    }
                }
            }
            let median = |v: &mut Vec<f32>| {
                v.sort_by(f32::total_cmp);
                v.get(v.len() / 2).copied().unwrap_or(0.0)
            };
            for (k, (frame_ms, shadows_ms, pages)) in samples.iter_mut().enumerate() {
                let report = renderers[k].dynamic_caster_report();
                println!(
                    "skinned_tier_cost_bench {motion:?} | {tier} | {}: frame p50 {:.3} ms, shadows p50 {:.3} ms, \
                     {} skinned casters, {:.1} dynamic pages per frame, cascade mask {:#b}",
                    arms[k].0,
                    median(frame_ms),
                    median(shadows_ms),
                    report.skinned_casters,
                    f64::from(*pages) / (STEPS - WARM) as f64,
                    report.policy.cascade_mask,
                );
            }
        }
    }
}
