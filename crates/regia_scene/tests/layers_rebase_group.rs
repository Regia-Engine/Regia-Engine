// SPDX-License-Identifier: MPL-2.0
//! A layer member respawned under a live Transform-less group after a floating-origin rebase
//! (finding scene-ecs 6, the edge v05-rc1-scene-ecs left open; v0.5.1).
//!
//! A Transform-less root passes no frame to its children (`propagate`): they carry world meters
//! from the file, so a respawn after a rebase must shift them, although their parent is adopted
//! (live) and the adopted parent itself is never shifted. A Transform-less group under a
//! transformed parent passes that parent's (local-space) frame on: its respawned members stay
//! parent-relative.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::ecs::hierarchy::ChildOf;
use regia_scene::components::EntityGuid;
use regia_scene::layers::{LayerDefs, LayerDefsLibrary, LayerState, LayerTable, Side};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::prelude::*;

const DEFS_ID: Guid = Guid::from_u128(0xd0);
const LEVEL: LevelId = LevelId(Guid::from_u128(0x1e));
/// A Transform-less root group.
const GROUP: u128 = 0x100;
/// A member of `night` under `GROUP`, authored at (7, 0, 3).
const MEMBER: u128 = 0x101;
/// A transformed root at (2, 0, 0) …
const ROOT: u128 = 0x200;
/// … with a Transform-less group under it …
const INNER: u128 = 0x201;
/// … and a member of `night` under that, authored at (1, 0, 0) relative to `ROOT`.
const INNER_MEMBER: u128 = 0x202;
const OFFSET: [f64; 3] = [1000.0, 0.0, -500.0];

fn guid(v: u128) -> Guid {
    Guid::from_u128(v)
}

fn transform(x: f32, z: f32) -> String {
    format!(
        "\"regia_scene::transform::Transform\": (translation: ({x:?}, 0.0, {z:?}), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),"
    )
}

fn record(id: u128, parent: Option<u128>, components: &str) -> String {
    let parent = parent
        .map(|p| format!("parent: Some(\"{}\"),", guid(p)))
        .unwrap_or_default();
    format!(
        "(guid: \"{}\", {parent} components: {{ {components} }}),",
        guid(id)
    )
}

const NIGHT: &str = "\"regia_scene::layers::LayerMembership\": (layers: [\"night\"]),";

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let defs = LayerDefs::from_ron_str(
        r#"LayerDefs(format_version: 1, layers: [
            (name: "night", kind: Runtime, debug_color: (0.1, 0.1, 0.5), parent: None),
        ])"#,
    )
    .expect("defs");
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS_ID, defs);
    let text = format!(
        r#"SceneFile(format_version: {v}, entities: [{group} {member} {root} {inner} {inner_member}], resources: {{
            "regia_scene::layers::LevelLayers": (
                defs: ["asset:{defs}"],
                entries: [(layer: "night", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone)],
            ),
        }})"#,
        v = SCENE_FORMAT_VERSION,
        group = record(GROUP, None, ""),
        member = record(MEMBER, Some(GROUP), &format!("{}{NIGHT}", transform(7.0, 3.0))),
        root = record(ROOT, None, &transform(2.0, 0.0)),
        inner = record(INNER, Some(ROOT), ""),
        inner_member = record(
            INNER_MEMBER,
            Some(INNER),
            &format!("{}{NIGHT}", transform(1.0, 0.0))
        ),
        defs = DEFS_ID,
    );
    let level = Level::from_ron_str(LEVEL, &text).expect("level");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
    app.update();
    app
}

fn entity_of(app: &mut App, id: u128) -> Option<Entity> {
    let world = app.world_mut();
    let mut query = world.query::<(Entity, &EntityGuid)>();
    query.iter(world).find(|(_, g)| g.0 == guid(id)).map(|(e, _)| e)
}

fn world_position(app: &mut App, id: u128) -> Vec3 {
    let entity = entity_of(app, id).expect("alive");
    app.world()
        .get::<GlobalTransform>(entity)
        .map(GlobalTransform::translation)
        .expect("propagated")
}

fn request(app: &mut App, state: LayerState) {
    app.world_mut()
        .resource_mut::<LayerTable>()
        .request("night", state, false, Side::Local)
        .expect("known layer");
    app.update();
}

/// The rebase moves the origin by `OFFSET`: live roots with a `Transform` shift into the new local
/// space (what `regia_world`'s origin system does); the members are respawned afterwards.
fn rebase(app: &mut App) {
    app.insert_resource(regia_scene::streaming::OriginOffset(OFFSET));
    let root = entity_of(app, ROOT).expect("root");
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(root) {
        t.translation -= Vec3::new(OFFSET[0] as f32, OFFSET[1] as f32, OFFSET[2] as f32);
    }
    app.update();
}

#[test]
fn a_member_respawned_under_a_transformless_group_after_a_rebase_is_shifted() {
    let mut app = app();
    let before = world_position(&mut app, MEMBER);
    assert_eq!(before, Vec3::new(7.0, 0.0, 3.0));

    rebase(&mut app);
    request(&mut app, LayerState::Absent);
    assert_eq!(entity_of(&mut app, MEMBER), None, "night is absent");
    request(&mut app, LayerState::Live);

    let member = entity_of(&mut app, MEMBER).expect("respawned");
    let group = entity_of(&mut app, GROUP).expect("the group stayed");
    assert_eq!(
        app.world().get::<ChildOf>(member).map(ChildOf::parent),
        Some(group),
        "back under the live group"
    );
    let expected = Vec3::new(
        (7.0 - OFFSET[0]) as f32,
        (0.0 - OFFSET[1]) as f32,
        (3.0 - OFFSET[2]) as f32,
    );
    assert_eq!(
        world_position(&mut app, MEMBER),
        expected,
        "pre-rebase position minus the offset"
    );
    assert_eq!(
        app.world().get::<Transform>(group),
        None,
        "the group got no transform"
    );
}

#[test]
fn a_member_under_a_transformless_group_inside_a_transformed_parent_stays_relative() {
    let mut app = app();
    let before = world_position(&mut app, INNER_MEMBER);
    assert_eq!(before, Vec3::new(3.0, 0.0, 0.0));

    rebase(&mut app);
    request(&mut app, LayerState::Absent);
    request(&mut app, LayerState::Live);

    let inner_member = entity_of(&mut app, INNER_MEMBER).expect("respawned");
    assert_eq!(
        app.world().get::<Transform>(inner_member).map(|t| t.translation),
        Some(Vec3::new(1.0, 0.0, 0.0)),
        "parent-relative: the transformed ancestor is in local space already"
    );
    let expected = Vec3::new((3.0 - OFFSET[0]) as f32, 0.0, (0.0 - OFFSET[2]) as f32);
    assert_eq!(world_position(&mut app, INNER_MEMBER), expected);
}
