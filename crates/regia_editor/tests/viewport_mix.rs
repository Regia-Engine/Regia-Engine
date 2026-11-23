// SPDX-License-Identifier: MPL-2.0
//! What main's editor features look like in the runtime-path viewport (`docs/specs/19-editor.md`
//! § 3): a greybox primitive (Create ▸ Shape), an animated skinned character and an audio
//! emitter with its gizmo, captured offscreen with the editor overlays. The PNG is written for a
//! person to review (`REGIA_VIEWPORT_SHOTS`, default the temp dir).

use std::sync::Arc;

use regia::audio::AudioEmitter;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_editor::anim::demo_rig;
use regia_editor::audio_gizmo::{AUDIO_MAX, AUDIO_MIN};
use regia_editor::render::{PaneRequest, capture_pane};
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_render::{CpuMesh, JointMatrix, LabMaterial, LabVertex, MeshRenderer, Primitive, SkinJoints};
use regia_scene::prelude::{Name, Transform};

const W: u32 = 640;
const H: u32 = 400;

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

fn edit_world() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(EditorCamera::default());
    app
}

/// The demo biped's skinned body: one box per bone, built in bind (model) space and weighted
/// fully to the bone's child joint, so the palette poses it.
fn biped_mesh(skeleton: &regia_animation::skeleton::Skeleton) -> CpuMesh {
    let bind = skeleton.bind_globals();
    let (cube_vertices, cube_indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let mut vertices: Vec<LabVertex> = Vec::new();
    let mut indices = Vec::new();
    for (joint, parent) in skeleton.parents.iter().enumerate() {
        let Some(parent) = parent.map(|p| p as usize) else {
            continue;
        };
        let a = bind[parent].w_axis.truncate();
        let b = bind[joint].w_axis.truncate();
        let length = (b - a).length();
        if length < 1e-3 {
            continue;
        }
        let rotation = Quat::from_rotation_arc(Vec3::Y, (b - a) / length);
        let model =
            Mat4::from_scale_rotation_translation(Vec3::new(0.11, length, 0.11), rotation, (a + b) * 0.5);
        let base = vertices.len() as u32;
        for vertex in &cube_vertices {
            let mut v = *vertex;
            v.position = model.transform_point3(Vec3::from(v.position)).to_array();
            v.normal = (rotation * Vec3::from(v.normal)).normalize().to_array();
            v.joints = [parent as u32, 0, 0, 0];
            v.weights = [1.0, 0.0, 0.0, 0.0];
            vertices.push(v);
        }
        indices.extend(cube_indices.iter().map(|i| base + i));
    }
    CpuMesh { vertices, indices }
}

/// The palette of the demo biped's `clip` at `time` (`global · inverse_bind`, model space).
fn palette(skeleton: &regia_animation::skeleton::Skeleton, clip: &str, time: f32) -> Vec<JointMatrix> {
    let clips = demo_rig::clips();
    let clip = clips.iter().find(|c| c.name == clip).expect("demo clip");
    let mut pose = skeleton.bind_pose_pose();
    clip.sample_pose(time, &mut pose);
    skeleton
        .globals(&pose, None)
        .iter()
        .zip(&skeleton.inverse_bind_matrices)
        .map(|(global, inverse_bind)| JointMatrix::from_mat4(*global * *inverse_bind))
        .collect()
}

fn count(image: &egui::ColorImage, test: impl Fn(egui::Color32) -> bool) -> usize {
    image.pixels.iter().filter(|p| test(**p)).count()
}

fn save(image: &egui::ColorImage, name: &str) {
    let dir = regia_core::brand::env_var("REGIA_VIEWPORT_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-viewport-shots"));
    let _ = std::fs::create_dir_all(&dir);
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    if let Some(buffer) = image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, bytes) {
        let path = dir.join(name);
        if buffer.save(&path).is_ok() {
            eprintln!("viewport shot: {}", path.display());
        }
    }
}

#[test]
fn primitive_animated_character_and_audio_gizmo_render_through_the_runtime_path() {
    let device = device();
    let mut app = edit_world();
    let world = app.world_mut();

    // A red torus made through Create ▸ Shape.
    world.insert_resource(EditorCamera {
        target: Vec3::new(-2.2, 0.8, 0.0),
        ..EditorCamera::default()
    });
    let shape = regia_editor::shapes::create_shape(world, "torus").expect("shape");
    if let Some(mut primitive) = world.get_mut::<Primitive>(shape) {
        primitive.color = [0.9, 0.05, 0.05, 1.0];
    }
    if let Some(mut transform) = world.get_mut::<Transform>(shape) {
        transform.rotation = Quat::from_rotation_x(1.2);
    }

    // The demo biped, skinned and posed mid-stride by its Walk clip.
    let skeleton = demo_rig::skeleton();
    let body = Arc::new(biped_mesh(&skeleton));
    let blue = Arc::new(LabMaterial {
        base_color: [0.1, 0.3, 0.95, 1.0],
        roughness: 0.6,
        ..LabMaterial::default()
    });
    let character = world
        .spawn((
            Name::new("Walker"),
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
            MeshRenderer::new(Arc::clone(&body), Arc::clone(&blue)),
            SkinJoints {
                palette: palette(&skeleton, "Walk", 0.25),
            },
        ))
        .id();

    // An audio emitter, selected so its range spheres draw.
    let emitter = world
        .spawn((
            Name::new("Speaker"),
            Transform::from_translation(Vec3::new(2.4, 1.0, 0.0)),
            AudioEmitter {
                min_distance: 0.6,
                max_distance: 1.4,
                ..AudioEmitter::default()
            },
        ))
        .id();
    let mut selection = Selection::default();
    selection.select(emitter, SelectMode::Replace);
    world.insert_resource(selection);

    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: Vec3::new(0.0, 0.9, 0.0),
            distance: 6.5,
            yaw: 0.0,
            pitch: 0.3,
            ..EditorCamera::default()
        },
        width: W,
        height: H,
        ids: true,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let frame = capture_pane(&device, world, &request).expect("viewport capture");
    save(&frame.image, "viewport_mix.png");

    let red = count(&frame.image, |p| {
        u32::from(p.r()) > u32::from(p.g()) + 60 && u32::from(p.r()) > u32::from(p.b()) + 60
    });
    let blue_px = count(&frame.image, |p| {
        u32::from(p.b()) > u32::from(p.r()) + 50 && u32::from(p.b()) > u32::from(p.g()) + 20
    });
    let audio = count(&frame.image, |p| p == AUDIO_MIN || p == AUDIO_MAX);
    assert!(red > 1_500, "the torus draws its own red mesh ({red} px)");
    assert!(blue_px > 1_500, "the skinned character draws ({blue_px} px)");
    assert!(
        audio > 150,
        "the selected emitter's range spheres draw ({audio} px)"
    );
    assert!(
        frame.info.meshes >= 2,
        "torus part + character extracted: {:?}",
        frame.info.meshes
    );

    // The primitive resolved onto a hidden part; the level entity kept its components.
    assert!(world.get::<MeshRenderer>(shape).is_none());
    assert!(regia_editor::mesh_source::has_resolved_parts(world, shape));

    // The id buffer maps the character's pixels back to it (picking a skinned mesh).
    let ids = frame.ids.as_ref().expect("id buffer");
    let picked: Vec<Entity> = ids.entities_in_rect((W / 2 - 40, H / 4), (W / 2 + 40, 3 * H / 4));
    assert!(picked.contains(&character), "character pickable: {picked:?}");

    // A close-up of the character for a person to judge the pose.
    let close = PaneRequest {
        camera: EditorCamera {
            target: Vec3::new(0.0, 0.95, 0.0),
            distance: 2.8,
            yaw: 0.9,
            pitch: 0.15,
            ..EditorCamera::default()
        },
        ids: false,
        ..request.clone()
    };
    let closeup = capture_pane(&device, world, &close).expect("close-up capture");
    save(&closeup.image, "viewport_mix_character.png");

    // Another moment of the walk moves the limbs: the animation drives the picture.
    if let Some(mut joints) = world.get_mut::<SkinJoints>(character) {
        joints.palette = palette(&skeleton, "Walk", 0.75);
    }
    let later = capture_pane(&device, world, &request).expect("second capture");
    save(&later.image, "viewport_mix_later.png");
    let moved = frame
        .image
        .pixels
        .iter()
        .zip(&later.image.pixels)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        moved > 300,
        "the walk cycle changes the character's pixels ({moved} px)"
    );

    // Game view hides the audio gizmo with every other helper.
    world.insert_resource(regia_editor::views::ViewportLayout {
        game_view: true,
        ..Default::default()
    });
    let game = capture_pane(&device, world, &request).expect("game view capture");
    let audio_game = count(&game.image, |p| p == AUDIO_MIN || p == AUDIO_MAX);
    assert_eq!(audio_game, 0, "no audio gizmo in game view");
}
