// SPDX-License-Identifier: MPL-2.0
//! The Terrain panel's edit layers in the editor (P-236): tool strokes write the active layer and
//! undo bit-exactly, panel and agent changes are undo steps, the panel works from the keyboard,
//! and the sidecar is written beside the level.

use regia_core::math::Vec2;
use regia_terrain::cdlod::CdlodConfig;
use regia_terrain::layers::TerrainLayers;
use regia_terrain::{EditLayerOp, TerrainEditLayers, TerrainMap};

use super::*;
use crate::terrain_tool::{TerrainTool, begin_stroke, drain_stroke_log, end_stroke, pump};

fn world_with_terrain() -> (World, Entity) {
    let mut world = World::new();
    let n = 129u32;
    let heights = (0..n * n)
        .map(|i| 20.0 + ((i % n) as f32 * 0.09).sin() * 2.0 + ((i / n) as f32 * 0.07).cos() * 1.5)
        .collect();
    let map = TerrainMap::from_heights(n, n, heights, 128.0, 100.0, 0.0).expect("map");
    let terrain = Terrain::new(
        &map.to_heightfield(),
        128.0,
        100.0,
        0.0,
        CdlodConfig::default(),
        4,
        TerrainLayers::default(),
    )
    .expect("terrain");
    let e = world.spawn(terrain).id();
    world.init_resource::<UndoStack>();
    (world, e)
}

fn bits(world: &World, e: Entity) -> Vec<u32> {
    world
        .get::<Terrain>(e)
        .expect("terrain")
        .map
        .heights()
        .iter()
        .map(|h| h.to_bits())
        .collect()
}

fn tool_stroke(world: &mut World, e: Entity, at: Vec2) {
    assert!(begin_stroke(world, e, at, false), "the stroke starts");
    end_stroke(world);
    pump(world, true);
}

fn op(world: &mut World, e: Entity, op: EditLayerOp) {
    apply_op(world, e, &op).expect("op");
    drain_layer_log(world);
}

#[test]
fn tool_strokes_write_the_active_layer_and_undo_bit_exactly() {
    let (mut world, e) = world_with_terrain();
    world.init_resource::<TerrainTool>();
    let original = bits(&world, e);
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "A".into(),
            blend: EditLayerBlend::Add,
        },
    );
    tool_stroke(&mut world, e, Vec2::new(0.0, 0.0));
    let after_a = bits(&world, e);
    assert_ne!(after_a, original);
    let stack = world.get::<TerrainEditLayers>(e).expect("stack");
    assert!(
        stack.layers()[0].bounds().is_some(),
        "the stroke landed in layer A"
    );
    assert_eq!(
        stack.flatten().iter().map(|h| h.to_bits()).collect::<Vec<_>>(),
        after_a,
        "the map is the fold of the layers"
    );
    let layer_a = stack.layers()[0].clone();
    // Undo walks back the stroke, then the layer's creation; redo comes back bit for bit.
    let undo_steps = world.resource::<UndoStack>().undo_depth();
    assert_eq!(undo_steps, 2, "create + stroke");
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.undo(world));
    });
    assert_eq!(bits(&world, e), original, "undoing the stroke restores the map");
    let stack = world.get::<TerrainEditLayers>(e).expect("stack");
    assert!(
        (0..stack.base().len()).all(|i| stack.layers()[0].value(i) == 0.0),
        "and empties the layer"
    );
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.redo(world));
    });
    assert_eq!(bits(&world, e), after_a, "redo is bit-exact");
    assert_eq!(
        world.get::<TerrainEditLayers>(e).expect("stack").layers()[0],
        layer_a
    );
}

#[test]
fn a_toggle_from_the_panel_path_is_one_undo_step_that_restores_exactly() {
    let (mut world, e) = world_with_terrain();
    world.init_resource::<TerrainTool>();
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "A".into(),
            blend: EditLayerBlend::Add,
        },
    );
    tool_stroke(&mut world, e, Vec2::new(0.0, 0.0));
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "B".into(),
            blend: EditLayerBlend::Add,
        },
    );
    tool_stroke(&mut world, e, Vec2::new(4.0, 2.0));
    let both = bits(&world, e);
    let b = world.get::<TerrainEditLayers>(e).expect("stack").layers()[1].id;
    op(
        &mut world,
        e,
        EditLayerOp::Set {
            id: b,
            patch: EditLayerPatch {
                enabled: Some(false),
                ..EditLayerPatch::default()
            },
        },
    );
    let hidden = bits(&world, e);
    assert_ne!(hidden, both);
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.undo(world));
    });
    assert_eq!(
        bits(&world, e),
        both,
        "undoing the toggle shows B again, bit for bit"
    );
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.redo(world));
    });
    assert_eq!(bits(&world, e), hidden);
}

#[test]
fn a_locked_active_layer_refuses_a_tool_stroke() {
    let (mut world, e) = world_with_terrain();
    world.init_resource::<TerrainTool>();
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "A".into(),
            blend: EditLayerBlend::Add,
        },
    );
    let id = world.get::<TerrainEditLayers>(e).expect("stack").layers()[0].id;
    op(
        &mut world,
        e,
        EditLayerOp::Set {
            id,
            patch: EditLayerPatch {
                locked: Some(true),
                ..EditLayerPatch::default()
            },
        },
    );
    let before = bits(&world, e);
    assert!(!begin_stroke(&mut world, e, Vec2::new(0.0, 0.0), false));
    assert_eq!(bits(&world, e), before);
    assert_eq!(
        check_stroke_target(&world, e).expect_err("locked").code(),
        "edit_layer_locked"
    );
}

#[test]
fn agent_strokes_from_the_stroke_log_land_in_the_active_layer() {
    let (mut world, e) = world_with_terrain();
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "A".into(),
            blend: EditLayerBlend::Add,
        },
    );
    // A height edit logged by another writer (an import) is absorbed when the editor drains it.
    let brush = regia_terrain::Brush {
        shape: regia_terrain::BrushShape::Circle { radius: 5.0 },
        mode: regia_terrain::BrushMode::Raise { amount: 1.0 },
        ..regia_terrain::Brush::default()
    };
    let undo = {
        let mut query = world.query::<(&mut Terrain, &mut regia_terrain::TerrainEdits)>();
        let (mut t, mut edits) = query.get_mut(&mut world, e).expect("terrain");
        regia_terrain::apply_stroke(&mut t, None, &mut edits, &brush, &[Vec2::new(0.0, 0.0)]).expect("stroke")
    };
    world.init_resource::<regia_terrain::TerrainStrokeLog>();
    world
        .resource_mut::<regia_terrain::TerrainStrokeLog>()
        .push(e, undo);
    drain_stroke_log(&mut world);
    let stack = world.get::<TerrainEditLayers>(e).expect("stack");
    assert!(
        stack.layers()[0].bounds().is_some(),
        "the logged stroke is in layer A"
    );
    assert_eq!(
        stack.flatten().iter().map(|h| h.to_bits()).collect::<Vec<_>>(),
        bits(&world, e)
    );
}

#[test]
fn the_panel_is_operable_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let (mut world, e) = world_with_terrain();
    let ctx = egui::Context::default();
    let size = [700.0, 600.0];
    let mut draw = |ui: &mut egui::Ui| {
        egui::CentralPanel::default().show(ui, |ui| layers_ui(ui, &mut world, Some(e)));
    };
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "Add layer", false, 30, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    let stack = world
        .get::<TerrainEditLayers>(e)
        .expect("Enter on Add layer made a stack");
    assert_eq!(
        stack.layers().len(),
        1,
        "exactly one layer, even if egui ran the panel twice"
    );
    assert_eq!(stack.layers()[0].name, "Layer 1");
    // Hide it from the keyboard too.
    let mut draw = |ui: &mut egui::Ui| {
        egui::CentralPanel::default().show(ui, |ui| layers_ui(ui, &mut world, Some(e)));
    };
    tab_to(&ctx, size, "Hide", false, 40, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    assert!(
        !world.get::<TerrainEditLayers>(e).expect("stack").layers()[0].enabled,
        "Enter on Hide hid the layer"
    );
}

#[test]
fn the_sidecar_is_written_beside_the_level_and_reads_back() {
    let (mut world, e) = world_with_terrain();
    let dir = regia_test_utils::unique_temp_dir("terrain-editlayers");
    let level = dir.join("arena.level.ron");
    assert_eq!(save_edit_layers(&mut world, &level, true).expect("nothing"), None);
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "A".into(),
            blend: EditLayerBlend::Add,
        },
    );
    world.init_resource::<TerrainTool>();
    tool_stroke(&mut world, e, Vec2::new(0.0, 0.0));
    let file = save_edit_layers(&mut world, &level, true)
        .expect("save")
        .expect("written");
    assert_eq!(file, dir.join("arena.terrain.editlayers"));
    assert!(!world.get::<TerrainEditLayers>(e).expect("stack").unsaved());
    let terrain = world.get::<Terrain>(e).expect("terrain");
    let back = load_edit_layers(&file, &terrain.map)
        .expect("read")
        .expect("some");
    assert_eq!(back.layers().len(), 1);
    assert_eq!(
        back.flatten(),
        world.get::<TerrainEditLayers>(e).expect("stack").flatten()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saving_after_a_crater_absorbs_it_even_after_the_stack_was_saved() {
    let (mut world, e) = world_with_terrain();
    let dir = regia_test_utils::unique_temp_dir("terrain-layer-crater-save");
    let level = dir.join("arena.level.ron");
    let map_id = regia_asset::AssetId::new_random();
    world
        .entity_mut(e)
        .insert(crate::terrain_tool::TerrainCookTarget {
            map_id,
            cache_dir: dir.join("cache"),
        });
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "Hills".into(),
            blend: EditLayerBlend::Set,
        },
    );
    world.init_resource::<TerrainTool>();
    tool_stroke(&mut world, e, Vec2::ZERO);
    save_edit_layers(&mut world, &level, true).expect("initial save");
    {
        let mut terrain = world.get_mut::<Terrain>(e).expect("terrain");
        regia_terrain::apply_crater(
            &mut terrain.map,
            &regia_terrain::Crater {
                center: [0.0, 0.0],
                radius: 4.0,
                depth: 3.0,
                rim_height: 0.0,
            },
        )
        .expect("crater");
    }
    let saved = crate::terrain_tool::save_terrain_files(&mut world, &level).expect("save all terrain files");
    assert!(saved.tiles_recooked > 0);
    let file = edit_layers_path(&level);
    let terrain = world.get::<Terrain>(e).expect("terrain");
    let back = load_edit_layers(&file, &terrain.map)
        .expect("read")
        .expect("stack");
    assert_eq!(back.flatten(), terrain.map.heights());
    let cooked_path = dir.join("cache").join(regia_asset::cooked_file_name(
        regia_asset_pipeline::terrain_data::heightfield_asset_id(map_id),
    ));
    let cooked =
        regia_terrain::decode_cooked_tiled_heightfield(&std::fs::read(cooked_path).expect("cooked bytes"))
            .expect("cooked terrain");
    assert_eq!(
        cooked,
        regia_terrain::cook_all_tiles(&terrain.map, cooked.tile_size).expect("reference cook")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_base_edit_survives_undo_and_redo_of_an_earlier_layer_toggle() {
    let (mut world, e) = world_with_terrain();
    world.init_resource::<TerrainTool>();
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "Hills".into(),
            blend: EditLayerBlend::Add,
        },
    );
    tool_stroke(&mut world, e, Vec2::ZERO);
    let layer = world.get::<TerrainEditLayers>(e).expect("stack").layers()[0].id;
    let i = 64 * 129 + 64;
    let shown = world.get::<Terrain>(e).expect("terrain").map.heights()[i];
    op(
        &mut world,
        e,
        EditLayerOp::Set {
            id: layer,
            patch: EditLayerPatch {
                enabled: Some(false),
                ..Default::default()
            },
        },
    );
    let base = world.get::<Terrain>(e).expect("terrain").map.heights()[i];
    {
        let mut terrain = world.get_mut::<Terrain>(e).expect("terrain");
        regia_terrain::edit::write_height_samples(
            &mut terrain.map,
            regia_terrain::DirtyRect {
                col_start: 64,
                col_end: 65,
                row_start: 64,
                row_end: 65,
            },
            &[base - 3.0],
        );
    }
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.undo(world));
    });
    assert_eq!(
        world.get::<Terrain>(e).expect("terrain").map.heights()[i],
        shown - 3.0
    );
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.redo(world));
    });
    assert_eq!(
        world.get::<Terrain>(e).expect("terrain").map.heights()[i],
        base - 3.0
    );
}

#[test]
fn a_height_import_writes_the_base_and_keeps_layered_undo_exact() {
    let (mut world, e) = world_with_terrain();
    world.init_resource::<TerrainTool>();
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "Hills".into(),
            blend: EditLayerBlend::Add,
        },
    );
    tool_stroke(&mut world, e, Vec2::ZERO);
    let i = 64 * 129 + 64;
    let layer = world.get::<TerrainEditLayers>(e).expect("stack").layers()[0].clone();
    let before = bits(&world, e);
    let mut hf = world.get::<Terrain>(e).expect("terrain").map.to_heightfield();
    hf.samples.fill(12_000);
    let dir = regia_test_utils::unique_temp_dir("terrain-layer-base-import");
    std::fs::create_dir_all(&dir).expect("directory");
    let path = dir.join("heights.r16");
    let endian = regia_terrain::RawEndian::Little;
    std::fs::write(&path, regia_terrain::encode_r16(&hf, endian)).expect("heightmap");
    crate::terrain_panel::import_heightmap(
        &mut world,
        e,
        &path,
        crate::terrain_panel::HeightmapFormat::R16(endian),
        None,
    )
    .expect("import");
    let imported = bits(&world, e);
    assert_ne!(imported, before);
    assert_eq!(
        world
            .get::<TerrainEditLayers>(e)
            .expect("stack")
            .layer(layer.id)
            .expect("layer")
            .value(i),
        layer.value(i)
    );
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.undo(world));
    });
    assert_eq!(bits(&world, e), before);
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.redo(world));
    });
    assert_eq!(bits(&world, e), imported);
    for enabled in [false, true] {
        op(
            &mut world,
            e,
            EditLayerOp::Set {
                id: layer.id,
                patch: EditLayerPatch {
                    enabled: Some(enabled),
                    ..Default::default()
                },
            },
        );
    }
    assert_eq!(bits(&world, e), imported);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_crater_survives_undo_and_redo_of_an_earlier_layer_stroke() {
    for base_target in [false, true] {
        let (mut world, e) = world_with_terrain();
        world.init_resource::<TerrainTool>();
        op(
            &mut world,
            e,
            EditLayerOp::Create {
                name: "Hills".into(),
                blend: EditLayerBlend::Add,
            },
        );
        if base_target {
            regia_terrain::set_active_layer(&mut world, e, None).expect("base target");
        }
        let i = 64 * 129 + 64;
        let base = world.get::<Terrain>(e).expect("terrain").map.heights()[i];
        tool_stroke(&mut world, e, Vec2::ZERO);
        let shown = world.get::<Terrain>(e).expect("terrain").map.heights()[i];
        {
            let mut terrain = world.get_mut::<Terrain>(e).expect("terrain");
            regia_terrain::apply_crater(
                &mut terrain.map,
                &regia_terrain::Crater {
                    center: [0.0, 0.0],
                    radius: 4.0,
                    depth: 3.0,
                    rim_height: 0.0,
                },
            )
            .expect("crater");
        }
        world.resource_scope::<UndoStack, ()>(|world, mut stack| {
            assert!(stack.undo(world));
        });
        assert_eq!(
            world.get::<Terrain>(e).expect("terrain").map.heights()[i],
            base - 3.0
        );
        world.resource_scope::<UndoStack, ()>(|world, mut stack| {
            assert!(stack.redo(world));
        });
        assert_eq!(
            world.get::<Terrain>(e).expect("terrain").map.heights()[i],
            shown - 3.0
        );
    }
}

#[test]
fn a_base_edit_survives_redo_of_the_first_layer_creation() {
    let (mut world, e) = world_with_terrain();
    op(
        &mut world,
        e,
        EditLayerOp::Create {
            name: "Hills".into(),
            blend: EditLayerBlend::Add,
        },
    );
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.undo(world));
    });
    let i = 64 * 129 + 64;
    let changed = world.get::<Terrain>(e).expect("terrain").map.heights()[i] - 3.0;
    {
        let mut terrain = world.get_mut::<Terrain>(e).expect("terrain");
        regia_terrain::edit::write_height_samples(
            &mut terrain.map,
            regia_terrain::DirtyRect {
                col_start: 64,
                col_end: 65,
                row_start: 64,
                row_end: 65,
            },
            &[changed],
        );
    }
    world.resource_scope::<UndoStack, ()>(|world, mut stack| {
        assert!(stack.redo(world));
    });
    assert_eq!(
        world.get::<Terrain>(e).expect("terrain").map.heights()[i],
        changed
    );
    assert_eq!(
        world.get::<TerrainEditLayers>(e).expect("stack").flatten()[i],
        changed
    );
    assert!(
        !world
            .get::<regia_terrain::TerrainEdits>(e)
            .expect("dirty tracker")
            .unsaved_tiles()
            .is_empty()
    );
}
