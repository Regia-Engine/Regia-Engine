// SPDX-License-Identifier: MPL-2.0
//! Inspector tests that drive the panel through egui input (ADR-7472): per-component Remove from
//! a component header, asset fields with `@AssetFilter` (picker choices, Content Browser drops).

use regia_ecs::reflect::AppTypeRegistry;
use regia_physics::Collider;
use regia_render::{MeshSource, PointLight};
use regia_scene::prelude::{Name, Transform};
use serde_json::json;

use super::*;
use crate::dnd::ContentDrag;

/// An app with the scene plugin and the three component types the tests place.
fn app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<MeshSource>();
    app.register_type::<PointLight>();
    app.register_type::<Collider>();
    app.init_resource::<UndoStack>();
    app
}

fn select(world: &mut World, entity: Entity) {
    let mut selection = Selection::default();
    selection.select(entity, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
}

/// One inspector frame with `events`; returns the AccessKit tree.
fn frame(
    ctx: &egui::Context,
    world: &mut World,
    events: Vec<egui::Event>,
    before: impl FnOnce(&egui::Context),
) -> egui::accesskit::TreeUpdate {
    ctx.enable_accesskit();
    // No animations: a header opened by a click lays its body out at once, so a drag in the next
    // frame hits the field instead of the body's still-growing clip rect.
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 1400.0),
        )),
        events,
        ..Default::default()
    };
    before(ctx);
    let mut output = ctx.run_ui(input, |ui| inspector_panel(ui, world));
    output.textures_delta.clear();
    output
        .platform_output
        .accesskit_update
        .unwrap_or_else(|| egui::accesskit::TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            focus: egui::accesskit::NodeId(0),
            tree_id: egui::accesskit::TreeId::ROOT,
        })
}

/// The centre of the node whose label (or value) is `text`.
fn find(update: &egui::accesskit::TreeUpdate, text: &str) -> Option<egui::Pos2> {
    update.nodes.iter().find_map(|(_, node)| {
        let named = node.label() == Some(text) || node.value() == Some(text);
        let bounds = node.bounds()?;
        named.then(|| {
            egui::pos2(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            )
        })
    })
}

/// Every label and value text of the tree (for failure messages and visible-reason checks).
fn texts(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
    update
        .nodes
        .iter()
        .flat_map(|(_, node)| [node.label(), node.value()])
        .flatten()
        .map(str::to_owned)
        .collect()
}

fn press(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

/// A click at `pos` over two frames.
fn click(ctx: &egui::Context, world: &mut World, pos: egui::Pos2, button: egui::PointerButton) {
    frame(
        ctx,
        world,
        vec![egui::Event::PointerMoved(pos), press(pos, button, true)],
        |_| {},
    );
    frame(ctx, world, vec![press(pos, button, false)], |_| {});
}

/// Item 2: with one entity selected, each component header has its own Remove; removing the
/// second removable component (not the first, which the old single button took) is one undo step,
/// and undo restores it with identical values.
#[test]
fn a_component_header_removes_its_own_component_and_undo_restores_it() {
    let mut app = app();
    let light = PointLight {
        position: [0.0, 0.0, 0.0],
        color: [1.0, 0.5, 0.25],
        intensity: 812.5,
        radius: 7.0,
        cast_shadows: true,
    };
    let collider = Collider {
        friction: 0.7,
        ..Collider::default()
    };
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Lamp"),
            Transform::default(),
            MeshSource::new("builtin:cube", ""),
            light.clone(),
            collider.clone(),
        ))
        .id();
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let header = find(&tree, "PointLight").unwrap_or_else(|| panic!("PointLight header: {:?}", texts(&tree)));
    // The header's own context menu.
    click(&ctx, world, header, egui::PointerButton::Secondary);
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let remove = find(&tree, "Remove PointLight")
        .unwrap_or_else(|| panic!("no Remove PointLight in the header's menu: {:?}", texts(&tree)));
    click(&ctx, world, remove, egui::PointerButton::Primary);
    assert!(world.get::<PointLight>(entity).is_none(), "PointLight removed");
    assert!(world.get::<MeshSource>(entity).is_some(), "MeshSource kept");
    assert_eq!(world.get::<Collider>(entity), Some(&collider), "Collider kept");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one undo step");
    assert!(crate::command::undo_world(world));
    assert_eq!(
        world.get::<PointLight>(entity),
        Some(&light),
        "undo restores identical values"
    );
}

/// Item 2: the body of every removable component carries a Remove button too.
#[test]
fn each_removable_component_has_a_remove_button() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Lamp"),
            Transform::default(),
            MeshSource::new("builtin:cube", ""),
            PointLight::default(),
            Collider::default(),
        ))
        .id();
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    // PointLight sorts after Collider and MeshSource: the old single button never offered it.
    let header = find(&tree, "PointLight").unwrap_or_else(|| panic!("PointLight header: {:?}", texts(&tree)));
    click(&ctx, world, header, egui::PointerButton::Primary);
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let remove = find(&tree, "Remove PointLight").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    assert!(
        find(&tree, "Remove Transform").is_none(),
        "Transform is not removable"
    );
    assert!(find(&tree, "Remove Name").is_none(), "Name is not removable");
    click(&ctx, world, remove, egui::PointerButton::Primary);
    assert!(world.get::<PointLight>(entity).is_none());
    assert!(world.get::<Collider>(entity).is_some());
    assert!(world.get::<MeshSource>(entity).is_some());
}

/// Item 1: the runtime asset-path fields carry `@AssetFilter`.
#[test]
fn mesh_source_fields_carry_asset_filters() {
    let app = app();
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let info = registry
        .get_with_type_path("regia_render::runtime::MeshSource")
        .expect("MeshSource registered")
        .type_info();
    let regia_ecs::reflect::TypeInfo::Struct(info) = info else {
        panic!("MeshSource is a struct");
    };
    let filter = |field: &str| {
        let field = info.field(field).expect("field");
        regia_ecs::attrs::EditorAttrs::from_custom(field.custom_attributes()).asset_filter
    };
    assert_eq!(filter("mesh"), Some("mesh"));
    assert_eq!(filter("material"), Some("material"));
}

/// Item 1: a material field lists graphs and instances; a mesh field lists only meshes (and the
/// built-in primitives).
#[test]
fn material_fields_list_graphs_and_instances_and_mesh_fields_only_meshes() {
    use crate::dnd::ContentKind::{Mesh, Other};
    let assets = [
        ("assets/props/crate.gltf".to_owned(), Mesh),
        ("assets/props/rock.glb".to_owned(), Mesh),
        ("assets/materials/crate.material.ron".to_owned(), Other),
        ("assets/materials/wet.matgraph.ron".to_owned(), Other),
        ("assets/materials/wet_red.matinst.ron".to_owned(), Other),
        ("assets/materials/noise.matfunc.ron".to_owned(), Other),
        ("assets/sounds/hit.ogg".to_owned(), Other),
    ];
    for path in [
        "assets/materials/crate.material.ron",
        "assets/materials/wet.matgraph.ron",
        "assets/materials/wet_red.matinst.ron",
    ] {
        assert!(impls::asset_matches("material", path, Other), "{path}");
    }
    assert!(!impls::asset_matches(
        "material",
        "assets/materials/noise.matfunc.ron",
        Other
    ));
    assert!(!impls::asset_matches("material", "assets/props/crate.gltf", Mesh));
    let material = impls::asset_choices("material", "", &assets);
    assert_eq!(
        material,
        [
            "assets/materials/crate.material.ron",
            "assets/materials/wet.matgraph.ron",
            "assets/materials/wet_red.matinst.ron",
        ]
    );
    let mesh = impls::asset_choices("mesh", "", &assets);
    assert_eq!(
        mesh,
        [
            MeshSource::BUILTIN_CUBE,
            MeshSource::BUILTIN_PLANE,
            MeshSource::BUILTIN_SPHERE,
            "assets/props/crate.gltf",
            "assets/props/rock.glb",
        ]
    );
    assert_eq!(
        impls::asset_choices("material", "RED", &assets),
        ["assets/materials/wet_red.matinst.ron"]
    );
    assert_eq!(
        impls::asset_choices("mesh", "rock", &assets),
        ["assets/props/rock.glb"]
    );
}

/// Drags `path` from the Content Browser and releases it over the field labelled `field`.
fn drop_on_field(
    ctx: &egui::Context,
    world: &mut World,
    field: &str,
    path: &str,
) -> egui::accesskit::TreeUpdate {
    let tree = frame(ctx, world, Vec::new(), |_| {});
    let at = find(&tree, field).unwrap_or_else(|| panic!("no field {field}: {:?}", texts(&tree)));
    let drag = ContentDrag {
        path: path.to_owned(),
        kind: crate::dnd::kind_of_path(path),
    };
    frame(
        ctx,
        world,
        vec![
            egui::Event::PointerMoved(at),
            press(at, egui::PointerButton::Primary, true),
        ],
        move |ctx| egui::DragAndDrop::set_payload(ctx, drag),
    );
    frame(
        ctx,
        world,
        vec![press(at, egui::PointerButton::Primary, false)],
        |_| {},
    );
    frame(ctx, world, Vec::new(), |_| {})
}

/// Item 1: dropping a `.matgraph.ron` on `MeshSource.material` is one undo step and gives the
/// same component bytes as `editor.set_property`.
#[test]
fn dropping_a_material_graph_on_the_material_field_matches_set_property() {
    let graph = "assets/materials/wet.matgraph.ron";
    let mut dropped = app();
    let entity = dropped
        .world_mut()
        .spawn((
            Name::new("Crate"),
            Transform::default(),
            MeshSource::new("builtin:cube", ""),
        ))
        .id();
    select(dropped.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = dropped.world_mut();
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let header = find(&tree, "MeshSource").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, header, egui::PointerButton::Primary);
    drop_on_field(&ctx, world, "MeshSource.material", graph);
    assert_eq!(
        world.get::<MeshSource>(entity).map(|m| m.material.as_str()),
        Some(graph)
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one undo step");

    let mut agent = app();
    let other = agent
        .world_mut()
        .spawn((
            Name::new("Crate"),
            Transform::default(),
            MeshSource::new("builtin:cube", ""),
        ))
        .id();
    crate::agent_ext::test_support::set_property(
        agent.world_mut(),
        &json!({ "entity": other.to_bits(), "component": "MeshSource", "path": "material", "value": graph }),
    )
    .expect("set_property");
    let bytes = |world: &World, entity: Entity| {
        let value = world.get::<MeshSource>(entity).expect("mesh source").clone();
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let serializer = regia_ecs::reflect::serde::TypedReflectSerializer::new(&value, &registry);
        ron::to_string(&serializer).expect("serialize")
    };
    assert_eq!(bytes(world, entity), bytes(agent.world(), other));
    assert!(crate::command::undo_world(world));
    assert_eq!(
        world.get::<MeshSource>(entity).map(|m| m.material.as_str()),
        Some("")
    );
}

/// Item 1: a wrong-kind drop (a mesh on a material field) is refused and the reason is shown.
#[test]
fn a_wrong_kind_drop_is_refused_with_a_visible_reason() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Crate"),
            Transform::default(),
            MeshSource::new("builtin:cube", ""),
        ))
        .id();
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let header = find(&tree, "MeshSource").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, header, egui::PointerButton::Primary);
    let tree = drop_on_field(&ctx, world, "MeshSource.material", "assets/props/crate.gltf");
    assert_eq!(
        world.get::<MeshSource>(entity).map(|m| m.material.as_str()),
        Some("")
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0, "nothing recorded");
    let shown = texts(&tree);
    assert!(
        shown
            .iter()
            .any(|t| t.contains("assets/props/crate.gltf") && t.contains("not a material")),
        "the refusal is visible: {shown:?}"
    );
    assert_eq!(
        impls::asset_drop_verdict(
            "material",
            "assets/props/crate.gltf",
            crate::dnd::ContentKind::Mesh
        ),
        Err("assets/props/crate.gltf is not a material (material)".to_owned())
    );
}

/// Item 1 format guard: `@AssetFilter` is editor metadata, not part of a component's serialized
/// shape. A level holding tagged runtime fields saves, reopens in a fresh world and saves again to
/// the same bytes, and each field is written as its plain string.
#[test]
fn a_level_with_tagged_asset_fields_round_trips_byte_identically() {
    use regia_core::id::Guid;
    use regia_framework::cloth::ClothSimSource;
    use regia_matgraph::MaterialGraphRef;

    fn level_app() -> regia_app::App {
        let mut app = app();
        app.register_type::<MaterialGraphRef>();
        app.register_type::<ClothSimSource>();
        app
    }
    let dir = regia_test_utils::unique_temp_dir("inspector-asset-filter-level");
    std::fs::create_dir_all(dir.join("assets/levels")).expect("dir");
    let first = dir.join("assets/levels/tagged.level.ron");
    let second = dir.join("assets/levels/tagged-again.level.ron");

    let mut app = level_app();
    let open = crate::level_io::new_level(app.world_mut());
    let graph = MaterialGraphRef {
        path: "assets/materials/wet.matgraph.ron".to_owned(),
    };
    let cloth = ClothSimSource {
        asset: "assets/cloth/flag.cloth.ron".to_owned(),
        ..Default::default()
    };
    let world = app.world_mut();
    let mut command =
        crate::command::SpawnEntities::new_entity(world, Guid::new_random(), "Tagged", Transform::default())
            .and_then(|c| {
                c.with_component(
                    world,
                    &MeshSource::new("assets/props/crate.gltf", "assets/materials/wet.matgraph.ron"),
                )
            })
            .and_then(|c| c.with_component(world, &graph))
            .and_then(|c| c.with_component(world, &cloth))
            .expect("spawn command");
    crate::command::EditorCommand::apply(&mut command, world);
    crate::level_io::save_level_file(world, &open, &first).expect("save");
    let saved = std::fs::read_to_string(&first).expect("read");
    for written in [
        "mesh: \"assets/props/crate.gltf\"",
        "material: \"assets/materials/wet.matgraph.ron\"",
        "path: \"assets/materials/wet.matgraph.ron\"",
        "asset: \"assets/cloth/flag.cloth.ron\"",
    ] {
        assert!(saved.contains(written), "{written} in {saved}");
    }

    let mut again = level_app();
    let reopened = crate::level_io::open_level_file(again.world_mut(), &first).expect("open");
    crate::level_io::save_level_file(again.world_mut(), &reopened, &second).expect("save again");
    assert_eq!(
        std::fs::read_to_string(&second).expect("read again"),
        saved,
        "the same bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// rev-claude-59 inspector-forms finding 6: `DestructibleSource.asset`'s filter takes the forms its
/// doc lists that a path can show: a `.destructible.ron` and a manifest sub-asset
/// (`assets/wall.glb#fracture/destructible`). An asset id is typed, never picked.
#[test]
fn the_destructible_source_filter_takes_its_manifest_form() {
    use crate::dnd::ContentKind::{Mesh, Other};
    let mut app = app();
    app.register_type::<regia_physics::destruction::DestructibleSource>();
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let info = registry
        .get_with_type_path("regia_physics::destruction::source::DestructibleSource")
        .expect("DestructibleSource registered")
        .type_info();
    let regia_ecs::reflect::TypeInfo::Struct(info) = info else {
        panic!("DestructibleSource is a struct");
    };
    let field = info.field("asset").expect("field");
    let filter = regia_ecs::attrs::EditorAttrs::from_custom(field.custom_attributes())
        .asset_filter
        .expect("asset is tagged");
    assert!(impls::asset_matches(
        filter,
        "assets/wall.destructible.ron",
        Other
    ));
    assert!(
        impls::asset_matches(filter, "assets/wall.glb#fracture/destructible", Mesh),
        "{filter}: the manifest sub-asset form"
    );
    assert!(
        impls::asset_drop_verdict(filter, "assets/wall.glb#fracture/destructible", Mesh).is_ok(),
        "{filter}: a drop of the manifest form is taken"
    );
    assert!(!impls::asset_matches(filter, "assets/wall.glb", Mesh), "{filter}");
    assert!(!impls::asset_matches(filter, "assets/t.png", Other), "{filter}");
}

/// The root viewport's repaint delay after one inspector frame.
fn repaint_delay(ctx: &egui::Context, world: &mut World) -> std::time::Duration {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 1400.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| inspector_panel(ui, world));
    output.textures_delta.clear();
    output
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .map_or(std::time::Duration::MAX, |v| v.repaint_delay)
}

/// rev-claude-59 inspector-forms finding 8: an asset field whose picture is on its way keeps the
/// inspector repainting (the Content Browser need not be open), and an idle one does not.
#[test]
fn an_asset_field_repaints_while_its_picture_is_pending() {
    let dir = regia_test_utils::unique_temp_dir("inspector-thumb-repaint");
    std::fs::create_dir_all(dir.join("assets/props")).expect("dir");
    std::fs::write(
        dir.join("assets/props/rock.glb"),
        b"glTF not parsed by the picture worker",
    )
    .expect("glb");
    let mut app = app();
    let world = app.world_mut();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let entity = world.spawn((Name::new("Rock"), MeshSource::new("", ""))).id();
    select(world, entity);
    let ctx = egui::Context::default();
    // Components other than the transform start collapsed: open MeshSource so its fields draw.
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let header = find(&tree, "MeshSource").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, header, egui::PointerButton::Primary);
    for _ in 0..4 {
        repaint_delay(&ctx, world);
    }
    assert_eq!(
        repaint_delay(&ctx, world),
        std::time::Duration::MAX,
        "no picture is pending: no repaint"
    );
    world.get_mut::<MeshSource>(entity).expect("mesh source").mesh = "assets/props/rock.glb".to_owned();
    // The frame that asks for the picture leaves it queued or in flight (only a pump collects it).
    assert_eq!(
        repaint_delay(&ctx, world),
        std::time::Duration::ZERO,
        "a pending picture repaints the inspector"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A GLB with one triangle (`POSITION` only).
fn triangle_glb() -> Vec<u8> {
    let positions: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": "tri", "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 } }] }],
        "accessors": [{ "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
                        "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] }],
        "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": positions.len() }],
        "buffers": [{ "byteLength": positions.len() }]
    });
    let mut text = serde_json::to_vec(&doc).expect("json");
    while !text.len().is_multiple_of(4) {
        text.push(b' ');
    }
    let total = 12 + 8 + text.len() + 8 + positions.len();
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(total).expect("small").to_le_bytes());
    out.extend_from_slice(&u32::try_from(text.len()).expect("small").to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&text);
    out.extend_from_slice(&u32::try_from(positions.len()).expect("small").to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&positions);
    out
}

/// rev-claude-59 inspector-forms finding 8: with only the Inspector drawn (no Content Browser
/// grid), a mesh field's picture is rendered on the GPU and becomes ready.
#[test]
fn a_mesh_field_gets_its_picture_without_the_content_browser_grid() {
    let Ok(device) = crate::render::headless_device() else {
        eprintln!("skip: no GPU adapter (lavapipe) for the mesh picture");
        return;
    };
    let dir = regia_test_utils::unique_temp_dir("inspector-thumb-mesh");
    std::fs::create_dir_all(dir.join("assets/props")).expect("dir");
    std::fs::write(dir.join("assets/props/tri.glb"), triangle_glb()).expect("glb");
    let mut app = app();
    let world = app.world_mut();
    world.insert_resource(device);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let rel = "assets/props/tri.glb";
    let entity = world.spawn((Name::new("Tri"), MeshSource::new(rel, ""))).id();
    select(world, entity);
    let ctx = egui::Context::default();
    // Opening the MeshSource header draws the field, which asks for the picture; the worker
    // answers that it needs the GPU.
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let header = find(&tree, "MeshSource").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, header, egui::PointerButton::Primary);
    world.resource_mut::<crate::thumb_cache::ThumbCache>().finish();
    assert_eq!(
        world.resource::<crate::thumb_cache::ThumbCache>().state(rel),
        crate::thumb_cache::ThumbState::NeedsGpu,
        "the worker hands a model to the GPU"
    );
    // The next inspector frame renders it (one per frame).
    repaint_delay(&ctx, world);
    world.resource_mut::<crate::thumb_cache::ThumbCache>().finish();
    let thumbs = world.resource::<crate::thumb_cache::ThumbCache>();
    assert_eq!(thumbs.gpu_waiting(), 0, "nothing waits for the GPU any more");
    assert_eq!(
        thumbs.state(rel),
        crate::thumb_cache::ThumbState::Ready,
        "the Inspector rendered the mesh picture"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-7744 (w9-levels-variants Known issue): a prefab instance's override of a field is marked in
/// that field's row, beside its label, and nothing says `Override ...` under the component.
#[test]
fn a_prefab_override_is_marked_beside_its_field() {
    let app = app();
    let ctx = egui::Context::default();
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let draw = |overrides: &[&str]| {
        let mut light = PointLight {
            position: [0.0; 3],
            color: [1.0; 3],
            intensity: 800.0,
            radius: 6.0,
            cast_shadows: false,
        };
        let mut context = InspectorContext::new("regia_render::lights::PointLight")
            .with_overrides(overrides.iter().copied());
        ctx.enable_accesskit();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let registry = registry.read();
            InspectorUi::new(&registry, &mut context).ui_for_reflect_with_options(
                &mut light,
                ui,
                egui::Id::new("override-marker-test"),
                &(),
            );
        });
        output.textures_delta.clear();
        output.platform_output.accesskit_update.expect("accesskit")
    };
    // Two frames: the first lays the grid out.
    draw(&["intensity"]);
    let update = draw(&["intensity"]);
    let label = find(&update, "intensity").expect("the intensity label");
    let marker = find(&update, "●").expect("an override marker");
    assert!(
        (label.y - marker.y).abs() < 4.0 && marker.x > label.x,
        "the marker sits in the intensity row, right of its label: {label:?} {marker:?}"
    );
    let radius = find(&update, "radius").expect("the radius label");
    assert!((radius.y - marker.y).abs() > 4.0, "no marker on the other rows");
    // Every marker node (the accessibility tree may hold the dot twice) is in the intensity row.
    let marker_rows: Vec<f64> = update
        .nodes
        .iter()
        .filter(|(_, node)| node.label() == Some("●") || node.value() == Some("●"))
        .filter_map(|(_, node)| node.bounds().map(|b| (b.y0 + b.y1) / 2.0))
        .collect();
    assert!(!marker_rows.is_empty(), "a marker");
    assert!(
        marker_rows.iter().all(|y| (*y as f32 - label.y).abs() < 4.0),
        "{marker_rows:?} vs {label:?}"
    );
    assert!(
        !texts(&update).iter().any(|t| t.starts_with("Override ")),
        "{:?}",
        texts(&update)
    );

    // No overrides: no marker. An override inside a field marks the field hollow.
    draw(&[]);
    assert!(find(&draw(&[]), "●").is_none());
    let mut context = InspectorContext::new("T").with_overrides([".color.x", "radius"]);
    assert_eq!(context.override_mark("radius"), Some(true));
    assert_eq!(
        context.override_mark("color"),
        Some(false),
        "something inside is overridden"
    );
    assert_eq!(context.override_mark("intensity"), None);
    assert!(context.take_override_requests().is_empty());
    let whole = InspectorContext::new("T").with_overrides([".", ""]);
    assert_eq!(
        whole.override_mark("color"),
        None,
        "a whole-component entry marks no field"
    );
}

/// Review finding S2 of ADR-7744: an override whose field has no row of its own (a list element)
/// gets only the hollow marker, which has no menu, so it must keep its `Override ...` row with
/// Revert and Apply under the component. The overridden field that drew its marker with the menu
/// gets no second row.
#[test]
fn an_override_without_its_own_field_row_keeps_its_revert_row() {
    use regia_scene::prefab::{OverrideEntry, PrefabInstance, PrefabLink};
    let mut app = app();
    let component = "regia_render::lights::PointLight";
    let entry = |path: &str, value: &str| OverrideEntry {
        target: 1,
        component: component.into(),
        path: path.into(),
        value: value.into(),
    };
    let world = app.world_mut();
    let root = world
        .spawn(PrefabInstance {
            prefab: 0x7744,
            overrides: vec![entry(".intensity", "900.0"), entry(".color[0]", "0.5")],
            ..Default::default()
        })
        .id();
    let light = world
        .spawn((
            Name::new("Lamp"),
            PointLight {
                position: [0.0; 3],
                color: [0.5, 1.0, 1.0],
                intensity: 900.0,
                radius: 6.0,
                cast_shadows: false,
            },
            PrefabLink { local_id: 1 },
            ChildOf(root),
        ))
        .id();
    select(world, light);
    let ctx = egui::Context::default();
    frame(&ctx, world, Vec::new(), |_| {});
    let tree = frame(&ctx, world, Vec::new(), |_| {});
    let all = texts(&tree);
    // Open the PointLight header if it is folded.
    let tree = if find(&tree, "intensity").is_none() {
        let header = find(&tree, "PointLight").unwrap_or_else(|| panic!("PointLight header: {all:?}"));
        click(&ctx, world, header, egui::PointerButton::Primary);
        frame(&ctx, world, Vec::new(), |_| {});
        frame(&ctx, world, Vec::new(), |_| {})
    } else {
        tree
    };
    let all = texts(&tree);
    let bits = light.to_bits();
    assert!(
        all.iter().any(|t| t == &format!("Override {bits} · .color[0]")),
        "the list-element override keeps its row: {all:?}"
    );
    assert!(
        !all.iter()
            .any(|t| t.starts_with("Override ") && t.ends_with(".intensity")),
        "the marked field has its marker, not a second row: {all:?}"
    );
    assert!(find(&tree, "●").is_some(), "the intensity marker: {all:?}");
    assert!(
        find(&tree, "Revert").is_some() && find(&tree, "Apply").is_some(),
        "the row's Revert and Apply: {all:?}"
    );
}
