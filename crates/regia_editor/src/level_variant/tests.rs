// SPDX-License-Identifier: MPL-2.0
use regia_app::prelude::App;
use regia_scene::prelude::EntityGuid;
use regia_scene::transform::Transform;

use super::*;
use crate::command::{SetTransform, undo_world};

const A: u128 = 0xa1;
const B: u128 = 0xa2;
const C: u128 = 0xa3;

fn g(n: u128) -> Guid {
    Guid::from_u128(n)
}

fn project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets/levels")).expect("dirs");
    dir
}

fn app(dir: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    app.world_mut().insert_resource(UndoStack::new());
    app
}

/// The base level: three named entities in a row; `C` sits at `c_x`.
fn write_base(dir: &Path, c_x: f32) -> PathBuf {
    let path = dir.join("assets/levels/arena.level.ron");
    let e = |n: u128, name: &str, x: f32| {
        format!(
            r#"(guid: "{}", name: "{name}", components: {{ "regia_scene::transform::Transform": (translation: ({x:?}, 0.0, 0.0)) }}),"#,
            g(n)
        )
    };
    let text = format!(
        "SceneFile(format_version: {}, entities: [{}{}{}])",
        regia_scene::format::SCENE_FORMAT_VERSION,
        e(A, "A", 0.0),
        e(B, "B", 1.0),
        e(C, "C", c_x)
    );
    regia_test_utils::write_atomic(&path, text.as_bytes()).expect("base");
    path
}

fn entity(world: &World, guid: u128) -> Option<Entity> {
    crate::hierarchy_ops::guid_entity(world, g(guid))
}

fn translation_x(world: &World, guid: u128) -> Option<f32> {
    entity(world, guid)
        .and_then(|e| world.get::<Transform>(e))
        .map(|t| t.translation.x)
}

fn push(world: &mut World, mut command: Box<dyn EditorCommand>) {
    command.apply(world);
    let mut stack = world.resource_mut::<UndoStack>();
    stack.push(command);
    stack.seal();
}

fn move_x(world: &mut World, guid: u128, x: f32) {
    let e = entity(world, guid).expect("entity");
    let mut t = world.get::<Transform>(e).copied().unwrap_or(Transform::IDENTITY);
    t.translation.x = x;
    let command = SetTransform::capture(world, e, t);
    push(world, Box::new(command));
}

fn save(world: &mut World) -> String {
    let open = world.resource::<OpenLevel>().clone();
    let path = open.path.clone().expect("path");
    crate::level_io::save_level_file(world, &open, &path).expect("save");
    std::fs::read_to_string(&path).expect("read")
}

fn new_variant(world: &mut World, dir: &Path) -> (PathBuf, Guid) {
    let base = dir.join("assets/levels/arena.level.ron");
    let path = dir.join("assets/levels/arena_night.levelvariant.ron");
    create_level_variant(&base, &path).expect("create");
    open_level_variant(world, &path).expect("open");
    let base_id = world.resource::<LevelVariantSession>().base;
    (path, base_id)
}

/// Opening and saving a variant nobody edited writes no differences: the base's values, spelled
/// differently in its file than a snapshot spells them, are not overrides.
#[test]
fn a_new_variant_saves_with_no_differences() {
    let dir = project("level-variant-empty");
    write_base(&dir, 2.0);
    let mut app = app(&dir);
    let world = app.world_mut();
    let (path, base_id) = new_variant(world, &dir);
    assert!(
        regia_asset::meta_path_for(&path).is_file(),
        "the variant has a sidecar"
    );
    assert!(
        regia_asset::meta_path_for(&dir.join("assets/levels/arena.level.ron")).is_file(),
        "the base got a sidecar holding the id it already loaded under"
    );
    assert_eq!(translation_x(world, C), Some(2.0));
    let text = save(world);
    assert_eq!(text, LevelVariantFile::new(base_id).to_ron_string().expect("ron"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The acceptance flow: move one entity, remove one, add one; the save holds only those three
/// differences, a change to the base's third entity flows in on reopen, and Revert to base on the
/// moved entity drops its override.
#[test]
fn edit_save_follow_the_base_and_revert() {
    let dir = project("level-variant-edit");
    write_base(&dir, 2.0);
    let mut app = app(&dir);
    let world = app.world_mut();
    let (path, base_id) = new_variant(world, &dir);

    move_x(world, A, 5.0);
    let b = entity(world, B).expect("b");
    push(
        world,
        Box::new(DespawnEntities {
            entities: vec![b.to_bits()],
            snapshot: None,
        }),
    );
    let spawn = SpawnEntities::new_entity(
        world,
        g(0xd4),
        "D",
        Transform::from_translation(regia_core::math::Vec3::new(9.0, 0.0, 0.0)),
    )
    .expect("spawn");
    push(world, Box::new(spawn));

    let text = save(world);
    let file = LevelVariantFile::from_ron_str(&text).expect("parses");
    assert_eq!(file.base_guid(), base_id);
    assert_eq!(file.removed_entities, vec![g(B)]);
    assert_eq!(
        file.added_entities.iter().map(|e| e.guid).collect::<Vec<_>>(),
        vec![g(0xd4)]
    );
    assert_eq!(file.overrides.len(), 1, "{text}");
    assert_eq!(file.overrides[0].target, g(A));
    assert_eq!(file.overrides[0].path, ".translation");
    assert_eq!(file.len(), 3, "{text}");
    let state = describe(world).expect("state");
    assert_eq!(state["differences"], 3);
    assert_eq!(state["unsaved"], false);

    // The base moves C; reopening the variant shows it.
    write_base(&dir, 7.0);
    open_level_variant(world, &path).expect("reopen");
    assert_eq!(translation_x(world, C), Some(7.0));
    assert_eq!(translation_x(world, A), Some(5.0));
    assert!(entity(world, B).is_none());
    assert!(entity(world, 0xd4).is_some());

    // Revert to base on A: one undo step, and the save forgets the override.
    assert_eq!(
        revert(
            world,
            g(A),
            Some("regia_scene::transform::Transform"),
            Some(".translation")
        ),
        Ok(1)
    );
    assert_eq!(translation_x(world, A), Some(0.0));
    let text = save(world);
    assert!(
        LevelVariantFile::from_ron_str(&text)
            .expect("parses")
            .overrides
            .is_empty(),
        "{text}"
    );
    assert!(undo_world(world));
    assert_eq!(translation_x(world, A), Some(5.0), "the revert undoes");
    // Reverting the added entity deletes it; reverting the removed one brings it back.
    assert_eq!(revert(world, g(0xd4), None, None), Ok(1));
    assert!(entity(world, 0xd4).is_none());
    assert_eq!(revert(world, g(B), None, None), Ok(1));
    assert_eq!(translation_x(world, B), Some(1.0));
    assert_eq!(
        revert(world, g(C), None, None).map_err(|e| e.reason),
        Err("nothing_to_revert")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tiny deterministic generator (no wall clock, no crate).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n
    }
}

/// 50 random edits (moves, deletions, additions) through the undo stack, then a full undo: the
/// save writes the variant's original bytes again.
#[test]
fn fifty_random_edits_and_a_full_undo_restore_the_original_bytes() {
    let dir = project("level-variant-undo");
    write_base(&dir, 2.0);
    let mut app = app(&dir);
    let world = app.world_mut();
    let (path, _) = new_variant(world, &dir);
    move_x(world, A, 3.0);
    let original = save(world);
    open_level_variant(world, &path).expect("reopen");
    let mut rng = Lcg(0x5eed);
    let mut next = 0xe000u128;
    let mut pushed = 0;
    for _ in 0..50 {
        let mut live: Vec<Guid> = {
            let mut query = world.query::<&EntityGuid>();
            query.iter(world).map(|guid| guid.0).collect()
        };
        live.sort();
        match rng.next(3) {
            0 if !live.is_empty() => {
                let guid = live[rng.next(live.len())];
                move_x(world, guid.as_u128(), rng.next(50) as f32);
            }
            1 if !live.is_empty() => {
                let guid = live[rng.next(live.len())];
                let e = entity(world, guid.as_u128()).expect("live");
                push(
                    world,
                    Box::new(DespawnEntities {
                        entities: vec![e.to_bits()],
                        snapshot: None,
                    }),
                );
            }
            _ => {
                next += 1;
                let x = rng.next(50) as f32;
                let spawn = SpawnEntities::new_entity(
                    world,
                    g(next),
                    "N",
                    Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, 0.0)),
                )
                .expect("spawn");
                push(world, Box::new(spawn));
            }
        }
        pushed += 1;
    }
    assert_eq!(pushed, 50);
    assert_ne!(save(world), original, "the edits changed the variant");
    while undo_world(world) {}
    assert_eq!(save(world), original, "a full undo writes the original bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_methods_create_read_and_revert() {
    let dir = project("level-variant-agent");
    write_base(&dir, 2.0);
    let mut app = app(&dir);
    let world = app.world_mut();
    let handlers: std::collections::HashMap<&str, Handler> = methods()
        .into_iter()
        .map(|(schema, handler)| (schema.name, handler))
        .collect();
    let call = |world: &mut World, name: &str, params: Value| handlers[name](world, &params);
    let reason = |err: RpcError| err.data.and_then(|d| d["reason"].as_str().map(str::to_string));

    let state = call(world, "editor.level_variant_state", json!({})).expect("state");
    assert_eq!(state["open"], false);
    let created = call(
        world,
        "editor.new_level_variant",
        json!({ "base": "assets/levels/arena.level.ron", "path": "levels/night.levelvariant.ron" }),
    )
    .expect("create");
    assert_eq!(created["chain"].as_array().map(Vec::len), Some(2));
    move_x(world, A, 4.0);
    let state = call(world, "editor.level_variant_state", json!({})).expect("state");
    assert_eq!(state["open"], true);
    assert_eq!(state["overrides"], 1);
    assert_eq!(state["unsaved"], true);
    let a = state["entities"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["guid"] == g(A).to_string()))
        .cloned()
        .expect("row");
    assert_eq!(a["mark"], "overridden");
    assert_eq!(a["fields"][0]["path"], ".translation");

    let reverted = call(
        world,
        "editor.level_variant_revert",
        json!({ "entity": g(A).to_string() }),
    )
    .expect("revert");
    assert_eq!(reverted["reverted"], 1);
    assert_eq!(reverted["differences"], 0);

    let err = call(
        world,
        "editor.level_variant_revert",
        json!({ "entity": g(A).to_string() }),
    )
    .expect_err("again");
    assert_eq!(reason(err).as_deref(), Some("nothing_to_revert"));
    let err = call(world, "editor.level_variant_revert", json!({ "entity": "x" })).expect_err("bad guid");
    assert_eq!(reason(err).as_deref(), Some("entity_missing"));
    let err = call(
        world,
        "editor.level_variant_revert",
        json!({ "entity": g(A).to_string(), "path": ".translation" }),
    )
    .expect_err("path without component");
    assert_eq!(reason(err).as_deref(), Some("bad_field"));
    let err = call(
        world,
        "editor.new_level_variant",
        json!({ "base": "assets/levels/arena.level.ron", "path": "levels/night.levelvariant.ron" }),
    )
    .expect_err("exists");
    assert_eq!(reason(err).as_deref(), Some("variant_exists"));
    let err = call(
        world,
        "editor.new_level_variant",
        json!({ "base": "assets/levels/arena.level.ron", "path": "levels/x.level.ron" }),
    )
    .expect_err("suffix");
    assert_eq!(reason(err).as_deref(), Some("bad_path"));
    let err = call(
        world,
        "editor.new_level_variant",
        json!({ "base": "assets/levels/none.level.ron", "path": "levels/y.levelvariant.ron" }),
    )
    .expect_err("no base");
    assert_eq!(reason(err).as_deref(), Some("base_missing"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two variants naming each other: opening one is refused with `variant_loop`.
#[test]
fn a_variant_loop_is_refused_on_open() {
    let dir = project("level-variant-loop");
    let mut app = app(&dir);
    let world = app.world_mut();
    let one = dir.join("assets/levels/one.levelvariant.ron");
    let two = dir.join("assets/levels/two.levelvariant.ron");
    for (path, id, base) in [(&one, 0xc1, 0xc2), (&two, 0xc2, 0xc1)] {
        write_meta(path, LEVEL_VARIANT_META_KIND, g(id)).expect("meta");
        let text = LevelVariantFile::new(g(base)).to_ron_string().expect("text");
        std::fs::write(path, text).expect("variant");
    }
    assert_eq!(
        open_level_variant(world, &one).map(|_| ()).map_err(|e| e.reason),
        Err("variant_loop")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_default_variant_path_sits_next_to_the_base() {
    assert_eq!(
        default_variant_path(Path::new("assets/levels/arena.level.ron")),
        PathBuf::from("assets/levels/arena_variant.levelvariant.ron")
    );
    assert_eq!(
        default_variant_path(Path::new("assets/x.levelvariant.ron")),
        PathBuf::from("assets/x_variant.levelvariant.ron")
    );
}

/// Accessibility: the Inspector's Revert entity to base is a focusable button, reached with Tab and
/// pressed with Enter (no mouse).
#[test]
fn revert_entity_to_base_is_reachable_from_the_keyboard() {
    let dir = project("level-variant-keys");
    write_base(&dir, 2.0);
    let mut app = app(&dir);
    let world = app.world_mut();
    new_variant(world, &dir);
    move_x(world, A, 6.0);
    let target = entity(world, A).expect("a");
    let ctx = egui::Context::default();
    let key = |key| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let frame = |world: &mut World, events: Vec<egui::Event>| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| inspector_entity_header(ui, world, target));
        output.textures_delta.clear();
    };
    frame(world, Vec::new());
    assert_eq!(entity_mark(world, target), Some(EntityMark::Overridden));
    frame(world, vec![key(egui::Key::Tab)]);
    frame(world, Vec::new());
    frame(world, vec![key(egui::Key::Enter)]);
    frame(world, Vec::new());
    assert_eq!(
        translation_x(world, A),
        Some(0.0),
        "Enter on the focused button reverted the entity"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_variant_with_unsaved_changes_needs_a_second_click() {
    let mut world = World::new();
    assert!(confirm_new_variant(&mut world, false), "nothing unsaved: go");
    assert!(!confirm_new_variant(&mut world, true), "first click arms");
    assert!(confirm_new_variant(&mut world, true), "second click discards");
    assert!(!confirm_new_variant(&mut world, true), "and disarms");
}
