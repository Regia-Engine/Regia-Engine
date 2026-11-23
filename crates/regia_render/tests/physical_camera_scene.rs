// SPDX-License-Identifier: MPL-2.0
//! The physical camera in scene files (`docs/specs/83-prefab-profiler-volumes.md` § 3.1,
//! ADR-2323): reflected round trip, a hand-written component without `format_version`, the
//! renderer ignoring a lens from a newer engine, and the format fingerprint.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_render::{
    Camera, CameraExposureMode, DepthOfFieldMode, ExtractedFrame, PHYSICAL_CAMERA_FORMAT_VERSION,
    PhysicalCamera, PostSettings, RenderSettings, extract_frame,
};
use regia_scene::components::{EntityGuid, Name};
use regia_scene::format::{SCENE_FORMAT_VERSION, SceneFile};
use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};
use regia_scene::{GlobalTransform, Transform};

fn scene_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    let handle = world.resource::<AppTypeRegistry>().0.clone();
    let mut registry = handle.write();
    regia_scene::components::register_primitives(&mut registry);
    registry.register::<Transform>();
    registry.register::<Name>();
    registry.register::<EntityGuid>();
    regia_render::register_physical_camera_types(&mut registry);
    drop(registry);
    world
}

fn scene_with(component: &str) -> SceneFile {
    let guid = Guid::new_random();
    let text = format!(
        r#"(
    format_version: {SCENE_FORMAT_VERSION},
    entities: [
        (guid: "{guid}", name: "Lens", components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 1.0, 0.0)),
            "regia_render::lens::PhysicalCamera": {component},
        }}),
    ],
    resources: {{}},
)"#
    );
    SceneFile::from_ron_str(&text).expect("scene parses")
}

fn the_lens(world: &mut World) -> PhysicalCamera {
    let mut query = world.query::<&PhysicalCamera>();
    *query.single(world).expect("one lens")
}

#[test]
fn a_lens_survives_save_and_load() {
    // Every field set away from its default, including the enums.
    let lens = PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: 11.5 },
        depth_of_field: Some(DepthOfFieldMode::Bokeh),
        iso: 400.0,
        shutter_seconds: 1.0 / 250.0,
        aperture_f_stops: 5.6,
        focal_length: Some(0.05),
        focal_distance: 3.0,
        ..PhysicalCamera::default()
    };
    let mut world = scene_world();
    world.spawn((lens, Transform::default(), EntityGuid(Guid::new_random())));
    let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    let saved = snapshot.to_ron_string().expect("write");
    assert!(saved.contains("regia_render::lens::PhysicalCamera"), "{saved}");

    let reread = SceneFile::from_ron_str(&saved).expect("reread");
    let mut again = scene_world();
    let spawned = spawn_scene(&mut again, &reread, &SpawnOptions::default()).expect("respawn");
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    assert_eq!(the_lens(&mut again), lens);
    let resaved = snapshot_scene(&mut again, &SnapshotFilter::default())
        .expect("snapshot 2")
        .to_ron_string()
        .expect("write 2");
    assert_eq!(resaved, saved, "byte-identical on the second save");

    // The default lens (infinite max depth, Inherit) round trips too.
    let mut world = scene_world();
    world.spawn((PhysicalCamera::default(), EntityGuid(Guid::new_random())));
    let saved = snapshot_scene(&mut world, &SnapshotFilter::default())
        .expect("snapshot")
        .to_ron_string()
        .expect("write");
    let mut again = scene_world();
    spawn_scene(
        &mut again,
        &SceneFile::from_ron_str(&saved).expect("reread"),
        &SpawnOptions::default(),
    )
    .expect("respawn");
    assert_eq!(the_lens(&mut again), PhysicalCamera::default());
}

#[test]
fn a_hand_written_lens_without_a_version_loads_as_v1() {
    let file = scene_with(
        "(shutter_angle: 0.5, motion_blur_samples: 2, depth_of_field: Some(Gaussian), focal_distance: 4.0, \
         aperture_f_stops: 8.0, sensor_height: 0.024, max_circle_of_confusion_diameter: 48.0, max_depth: 1000.0, \
         exposure: Physical, iso: 100.0, shutter_seconds: 0.008, shutter_reference_fps: 30.0, focal_length: None)",
    );
    let mut world = scene_world();
    let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    let lens = the_lens(&mut world);
    assert_eq!(lens.exposure, CameraExposureMode::Physical);
    assert_eq!(lens.aperture_f_stops, 8.0);
    let migrated = lens.migrate().expect("v0 migrates");
    assert_eq!(migrated.format_version, PHYSICAL_CAMERA_FORMAT_VERSION);
    assert!(lens.is_supported());
    let newer = PhysicalCamera {
        format_version: PHYSICAL_CAMERA_FORMAT_VERSION + 1,
        ..lens
    };
    assert!(!newer.is_supported());
    assert!(newer.migrate().is_err());
}

fn extracted_post(lens: Option<PhysicalCamera>) -> PostSettings {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(RenderSettings::default());
    let camera = app
        .world_mut()
        .spawn((GlobalTransform::IDENTITY, Camera::default()))
        .id();
    if let Some(lens) = lens {
        app.world_mut().entity_mut(camera).insert(lens);
    }
    app.add_systems(Render, extract_frame);
    app.update();
    let frame = app.world().resource::<ExtractedFrame>();
    frame.views[0].post.clone()
}

#[test]
fn the_renderer_ignores_a_lens_from_a_newer_engine() {
    let manual = PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: 3.0 },
        ..PhysicalCamera::default()
    };
    let none = format!("{:?}", extracted_post(None));
    assert_ne!(
        format!("{:?}", extracted_post(Some(manual))),
        none,
        "a supported manual lens changes the view"
    );
    let newer = PhysicalCamera {
        format_version: PHYSICAL_CAMERA_FORMAT_VERSION + 1,
        ..manual
    };
    assert_eq!(format!("{:?}", extracted_post(Some(newer))), none);
}

#[test]
fn the_fingerprint_names_every_field_and_variant() {
    let print = regia_render::physical_camera_schema_fingerprint();
    for needle in [
        "PhysicalCamera{format_version:u32,",
        "iso:f32,",
        "focal_length:core::option::Option<f32>,",
        "CameraExposureMode{Inherit|Manual(ev100:f32,)|Physical|};",
        "DepthOfFieldMode{Bokeh|Gaussian|};",
    ] {
        assert!(print.contains(needle), "{needle} missing from {print}");
    }
    assert_eq!(regia_render::physical_camera_migration_sources(), vec![0]);
}
