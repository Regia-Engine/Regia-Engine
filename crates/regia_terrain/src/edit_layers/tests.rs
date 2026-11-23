// SPDX-License-Identifier: MPL-2.0
//! Edit-layer acceptance (P-236, ADR-7825): additive layers add up, toggles restore exactly,
//! reorder matters only for Set layers, undo is bit-exact, recompute is regional, the cook bakes
//! the hand-flattened map, the sidecar round-trips and refuses stale or hostile files.

use glam::Vec2;

use super::*;
use crate::brush::{Brush, BrushMode, BrushShape};
use crate::cdlod::CdlodConfig;
use crate::layers::TerrainLayers;

const N: u32 = 129;

fn terrain() -> Terrain {
    let heights = (0..N * N)
        .map(|i| 100.0 + ((i % N) as f32 * 0.07).sin() * 4.0 + ((i / N) as f32 * 0.05).cos() * 3.0)
        .collect();
    let map = TerrainMap::from_heights(N, N, heights, 128.0, 400.0, 0.0).expect("map");
    let heightfield = map.to_heightfield();
    Terrain::new(
        &heightfield,
        128.0,
        400.0,
        0.0,
        CdlodConfig::default(),
        4,
        TerrainLayers::default(),
    )
    .expect("terrain")
}

fn raise(amount: f32) -> Brush {
    Brush {
        shape: BrushShape::Circle { radius: 6.0 },
        mode: BrushMode::Raise { amount },
        hardness: 0.5,
        ..Brush::default()
    }
}

/// One stroke through the normal path, then absorbed into the active layer.
fn stroke(
    t: &mut Terrain,
    edits: &mut TerrainEdits,
    stack: &mut TerrainEditLayers,
    brush: &Brush,
    at: Vec2,
) -> (StrokeUndo, LayerStrokeUndo) {
    let undo = crate::apply_stroke(t, None, edits, brush, &[at]).expect("stroke");
    let StrokeUndo::Height(mut record) = undo else {
        panic!("height stroke");
    };
    let layer = stack.absorb_stroke(&mut record, &mut t.map).expect("absorb");
    (StrokeUndo::Height(record), layer)
}

fn bits(map: &TerrainMap) -> Vec<u32> {
    map.heights().iter().map(|h| h.to_bits()).collect()
}

/// Every layer's settings and per-sample value and coverage, as bits (bounds are conservative and
/// are not part of the undo promise).
type LayerBits = (u32, String, bool, bool, u32, Vec<u32>, Vec<u32>);

fn layer_bits(stack: &TerrainEditLayers) -> Vec<LayerBits> {
    let n = stack.base().len();
    stack
        .layers()
        .iter()
        .map(|l| {
            (
                l.id,
                l.name.clone(),
                l.enabled,
                l.locked,
                l.opacity.to_bits(),
                (0..n).map(|i| l.value(i).to_bits()).collect(),
                (0..n).map(|i| l.coverage(i).to_bits()).collect(),
            )
        })
        .collect()
}

fn centre() -> Vec2 {
    Vec2::new(0.0, 0.0)
}

fn centre_index(map: &TerrainMap) -> usize {
    let s = crate::brush::world_to_sample(map, 0.0, 0.0);
    s.y.round() as usize * N as usize + s.x.round() as usize
}

#[test]
fn a_crater_survives_a_layer_toggle() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("Hills", EditLayerBlend::Add).expect("layer");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let i = centre_index(&t.map);
    let base = stack.base()[i];
    let before = t.map.heights()[i];
    crate::apply_crater(
        &mut t.map,
        &crate::Crater {
            center: [0.0, 0.0],
            radius: 4.0,
            depth: 3.0,
            rim_height: 0.0,
        },
    )
    .expect("crater");
    let delta = t.map.heights()[i] - before;
    assert!(delta < -2.0);
    let mut world = World::new();
    let entity = world.spawn((t, edits, stack)).id();
    apply_op(
        &mut world,
        entity,
        &EditLayerOp::Set {
            id: a,
            patch: EditLayerPatch {
                enabled: Some(false),
                ..Default::default()
            },
        },
    )
    .expect("hide");
    let t = world.get::<Terrain>(entity).expect("terrain");
    assert_eq!(
        t.map.heights()[i],
        base + delta,
        "hiding a layer must preserve the crater in the base"
    );
}

#[test]
fn a_raw_patch_survives_direct_recompute_and_sidecar_reload() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("Hills", EditLayerBlend::Add).expect("layer");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let i = centre_index(&t.map);
    let rect = point_rect((i % N as usize) as u32, (i / N as usize) as u32);
    crate::edit::apply_height_patch(&mut t, &mut edits, rect, &[80.0]).expect("patch");
    stack.recompute(&mut t.map, rect).expect("recompute");
    assert_eq!(t.map.heights()[i], 80.0);
    let bytes = stack.to_bytes().expect("encode");
    let mut loaded = TerrainEditLayers::from_bytes(&bytes, &t.map).expect("reload");
    let bounds = loaded
        .set(
            a,
            &EditLayerPatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .expect("hide");
    loaded
        .recompute_terrain(&mut t, &mut edits, bounds)
        .expect("recompute");
    assert_eq!(t.map.heights()[i], stack.base()[i]);
}

#[test]
fn an_unlayered_edit_before_a_stroke_stays_in_the_base() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("Hills", EditLayerBlend::Add).expect("layer");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let i = centre_index(&t.map);
    let base = stack.base()[i];
    t.map.set_height(i, t.map.heights()[i] - 3.0);
    let (undo, layer) = stroke(&mut t, &mut edits, &mut stack, &raise(1.0), centre());
    let after = bits(&t.map);
    undo.restore(&mut t, None, None, &mut edits, true);
    assert!(layer.restore(&mut stack, true));
    // Recompute after undo must not mistake the undo itself for another unlayered write.
    stack
        .recompute(&mut t.map, point_rect(64, 64))
        .expect("recompute");
    undo.restore(&mut t, None, None, &mut edits, false);
    assert!(layer.restore(&mut stack, false));
    stack
        .recompute(&mut t.map, point_rect(64, 64))
        .expect("recompute");
    assert_eq!(bits(&t.map), after);
    let bounds = stack
        .set(
            a,
            &EditLayerPatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .expect("hide");
    stack
        .recompute_terrain(&mut t, &mut edits, bounds)
        .expect("recompute");
    assert_eq!(
        t.map.heights()[i],
        base - 3.0,
        "the unlayered edit must not become part of Hills"
    );
}

#[test]
fn a_set_layer_covers_a_base_edit_without_erasing_it_or_absorbing_it_twice() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("Plateau", EditLayerBlend::Set).expect("layer");
    let brush = Brush {
        mode: BrushMode::Assign { value: 90.0 },
        hardness: 1.0,
        alpha: 1.0,
        ..raise(0.0)
    };
    stroke(&mut t, &mut edits, &mut stack, &brush, centre());
    let i = centre_index(&t.map);
    let base = stack.base()[i];
    let before = t.map.heights()[i];
    t.map.set_height(i, before - 5.0);
    stack
        .recompute(&mut t.map, point_rect(64, 64))
        .expect("recompute");
    assert_eq!(t.map.heights()[i], before, "the Set layer still covers the base");
    for enabled in [false, true, false] {
        let rect = stack
            .set(
                a,
                &EditLayerPatch {
                    enabled: Some(enabled),
                    ..Default::default()
                },
            )
            .expect("toggle");
        stack
            .recompute_terrain(&mut t, &mut edits, rect)
            .expect("recompute");
        assert_eq!(t.map.heights()[i], if enabled { before } else { base - 5.0 });
    }
}

#[test]
fn a_layered_runtime_patch_sends_the_composite_to_the_linked_draw() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    stack.create("Plateau", EditLayerBlend::Set).expect("layer");
    stroke(
        &mut t,
        &mut edits,
        &mut stack,
        &Brush {
            mode: BrushMode::Assign { value: 90.0 },
            hardness: 1.0,
            alpha: 1.0,
            ..raise(0.0)
        },
        centre(),
    );
    let mut drawn = t.map.clone();
    let queue = crate::TerrainHeightPatches::linked(&t.map);
    queue
        .request(crate::HeightPatch {
            col: 64,
            row: 64,
            width: 1,
            height: 1,
            heights: vec![80.0],
        })
        .expect("patch");
    let mut world = World::new();
    world.insert_resource(queue.clone());
    world.init_resource::<Messages<crate::TerrainHeightsChanged>>();
    // A loaded stack may not have an editor dirty tracker yet.
    let e = world.spawn((t, stack)).id();
    let mut schedule = Schedule::default();
    schedule.add_systems(crate::apply_terrain_height_patches);
    schedule.run(&mut world);
    let terrain = world.get::<Terrain>(e).expect("terrain");
    assert_eq!(terrain.map.heights()[64 * N as usize + 64], 90.0);
    for band in queue.take_draw_bands() {
        assert!(band.apply(&mut drawn).is_some());
    }
    assert_eq!(drawn.to_heightfield(), terrain.map.to_heightfield());
    assert!(
        !world
            .get::<TerrainEdits>(e)
            .expect("dirty tracker")
            .unsaved_tiles()
            .is_empty()
    );
    assert_eq!(
        world.get::<TerrainEditLayers>(e).expect("stack").flatten(),
        terrain.map.heights()
    );
}

#[test]
fn two_layers_raising_the_same_spot_add_up() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let i = centre_index(&t.map);
    let base = t.map.heights()[i];
    stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let after_a = t.map.heights()[i];
    let b = stack.create("B", EditLayerBlend::Add).expect("B");
    stroke(&mut t, &mut edits, &mut stack, &raise(3.0), centre());
    let after_b = t.map.heights()[i];
    let a_delta = stack.layers()[0].value(i);
    let b_delta = stack.layer(b).expect("b").value(i);
    assert!(
        a_delta > 0.0 && b_delta > 0.0,
        "both layers hold a raise: {a_delta} {b_delta}"
    );
    assert!(((after_a - base) - a_delta).abs() < 1e-4);
    assert!(
        ((after_b - base) - (a_delta + b_delta)).abs() < 1e-4,
        "the raises add up"
    );
    assert_eq!(
        after_b.to_bits(),
        ((f64::from(base) + f64::from(a_delta) + f64::from(b_delta)) as f32).to_bits(),
        "the map holds the canonical fold"
    );
    assert_eq!(stack.flatten(), t.map.heights(), "the map is the fold everywhere");
}

#[test]
fn toggling_one_off_restores_the_others_result_exactly() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let original = bits(&t.map);
    let a = stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    stroke(&mut t, &mut edits, &mut stack, &raise(1.0), Vec2::new(10.0, 4.0));
    let only_a = bits(&t.map);
    let b = stack.create("B", EditLayerBlend::Add).expect("B");
    stroke(&mut t, &mut edits, &mut stack, &raise(3.0), Vec2::new(3.0, -2.0));
    assert_ne!(bits(&t.map), only_a);
    let region = stack
        .set(
            b,
            &EditLayerPatch {
                enabled: Some(false),
                ..EditLayerPatch::default()
            },
        )
        .expect("off");
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    assert_eq!(bits(&t.map), only_a, "B off gives A's result bit for bit");
    let region = stack
        .set(
            a,
            &EditLayerPatch {
                enabled: Some(false),
                ..EditLayerPatch::default()
            },
        )
        .expect("off");
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    assert_eq!(bits(&t.map), original, "both off gives the base back");
}

#[test]
fn reorder_is_a_no_op_for_add_layers_and_matters_for_a_set_layer() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let b = stack.create("B", EditLayerBlend::Add).expect("B");
    stroke(&mut t, &mut edits, &mut stack, &raise(0.37), Vec2::new(2.0, 1.0));
    let before = bits(&t.map);
    let region = stack.reorder(b, 0).expect("reorder");
    assert!(region.is_some());
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    assert_eq!(bits(&t.map), before, "Add layers commute bit for bit");
    assert_eq!(stack.layers()[0].id, b);

    // A Set layer that flattens the spot to one height: on top it wins, under A it is raised.
    let s = stack.create("Plateau", EditLayerBlend::Set).expect("set");
    let flatten = Brush {
        shape: BrushShape::Circle { radius: 6.0 },
        mode: BrushMode::Assign { value: 90.0 },
        hardness: 1.0,
        alpha: 1.0,
        ..Brush::default()
    };
    stroke(&mut t, &mut edits, &mut stack, &flatten, centre());
    let i = centre_index(&t.map);
    assert!(
        (t.map.heights()[i] - 90.0).abs() < 1e-4,
        "the Set layer on top sets the height: {}",
        t.map.heights()[i]
    );
    let region = stack.reorder(s, 0).expect("reorder");
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    let raised = t.map.heights()[i];
    let set_to = stack.layer(s).expect("s").value(i);
    let adds = stack.layer(a).expect("a").value(i) + stack.layer(b).expect("b").value(i);
    assert!(
        (raised - (set_to + adds)).abs() < 1e-4,
        "under the Add layers the plateau is raised by them: {raised}"
    );
    assert!((raised - set_to).abs() > 1e-3, "order matters for a Set layer");
    assert_eq!(stack.flatten(), t.map.heights());
}

#[test]
fn undo_and_redo_of_a_layered_stroke_are_bit_exact() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(1.5), centre());
    let (map0, stack0) = (bits(&t.map), stack.clone());
    let (undo, layer) = stroke(&mut t, &mut edits, &mut stack, &raise(0.8), Vec2::new(1.0, 1.0));
    let (map1, stack1) = (bits(&t.map), stack.clone());
    for _ in 0..3 {
        undo.restore(&mut t, None, None, &mut edits, true);
        assert!(layer.restore(&mut stack, true));
        assert_eq!(bits(&t.map), map0, "undo restores the map");
        assert!(
            layer_bits(&stack) == layer_bits(&stack0),
            "undo restores the layer"
        );
        undo.restore(&mut t, None, None, &mut edits, false);
        assert!(layer.restore(&mut stack, false));
        assert_eq!(bits(&t.map), map1, "redo restores the map");
        assert!(
            layer_bits(&stack) == layer_bits(&stack1),
            "redo restores the layer"
        );
    }
}

#[test]
fn a_toggle_recomputes_only_the_layers_region() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(1.0), centre());
    let bounds = stack.layer(a).and_then(EditLayer::bounds).expect("bounds");
    let area = u64::from(bounds.col_end - bounds.col_start) * u64::from(bounds.row_end - bounds.row_start);
    let before = stack.recomputed_samples();
    let region = stack
        .set(
            a,
            &EditLayerPatch {
                enabled: Some(false),
                ..EditLayerPatch::default()
            },
        )
        .expect("off");
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    let cost = stack.recomputed_samples() - before;
    assert_eq!(cost, area, "the toggle recomputes the layer's bounds");
    assert!(cost * 10 < u64::from(N * N), "{cost} samples, not the map");
    // A rename changes nothing on the map.
    let region = stack
        .set(
            a,
            &EditLayerPatch {
                name: Some("Hills".into()),
                ..EditLayerPatch::default()
            },
        )
        .expect("rename");
    assert_eq!(region, None);
}

#[test]
fn the_cooked_map_equals_a_hand_flattened_map() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let base: Vec<f32> = t.map.heights().to_vec();
    stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    stack.create("B", EditLayerBlend::Add).expect("B");
    stroke(&mut t, &mut edits, &mut stack, &raise(1.0), Vec2::new(-8.0, 5.0));
    // By hand: base + A + B in f64, rounded once.
    let (la, lb) = (&stack.layers()[0], &stack.layers()[1]);
    let hand: Vec<f32> = base
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let sum = f64::from(la.value(i)) + f64::from(lb.value(i));
            if la.value(i) == 0.0 && lb.value(i) == 0.0 {
                *h
            } else {
                (f64::from(*h) + sum) as f32
            }
        })
        .collect();
    let hand_map = TerrainMap::from_heights(N, N, hand, 128.0, 400.0, 0.0).expect("hand");
    let cooked = crate::cook_all_tiles(&t.map, 32).expect("cook");
    let cooked_hand = crate::cook_all_tiles(&hand_map, 32).expect("cook hand");
    assert_eq!(cooked, cooked_hand, "the cook bakes the flattened heights");
}

#[test]
fn flatten_into_base_keeps_every_bit_and_removes_enabled_layers() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let off = stack.create("Off", EditLayerBlend::Add).expect("off");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), Vec2::new(20.0, 0.0));
    let region = stack
        .set(
            off,
            &EditLayerPatch {
                enabled: Some(false),
                ..EditLayerPatch::default()
            },
        )
        .expect("off");
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    let before = bits(&t.map);
    assert_eq!(stack.flatten_into_base().expect("flatten"), 1);
    assert_eq!(stack.layers().len(), 1, "the off layer stays");
    let flat: Vec<u32> = stack.flatten().iter().map(|h| h.to_bits()).collect();
    assert_eq!(flat, before, "flattening does not move a bit");
}

#[test]
fn locked_and_hidden_layers_refuse_strokes_with_codes() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("A", EditLayerBlend::Add).expect("A");
    stack
        .set(
            a,
            &EditLayerPatch {
                locked: Some(true),
                ..EditLayerPatch::default()
            },
        )
        .expect("lock");
    let undo = crate::apply_stroke(&mut t, None, &mut edits, &raise(1.0), &[centre()]).expect("stroke");
    let StrokeUndo::Height(mut record) = undo else {
        panic!("height");
    };
    let err = stack.absorb_stroke(&mut record, &mut t.map).expect_err("locked");
    assert_eq!(err.code(), "edit_layer_locked");
    assert_eq!(stack.delete(a).expect_err("locked").code(), "edit_layer_locked");
    assert_eq!(
        stack.flatten_into_base().expect_err("locked").code(),
        "edit_layer_locked"
    );
    stack
        .set(
            a,
            &EditLayerPatch {
                locked: Some(false),
                opacity: Some(0.0),
                ..EditLayerPatch::default()
            },
        )
        .expect("transparent");
    assert_eq!(
        stack
            .absorb_stroke(&mut record, &mut t.map)
            .expect_err("hidden")
            .code(),
        "edit_layer_hidden"
    );
    assert_eq!(
        stack.set(99, &EditLayerPatch::default()).expect_err("x").code(),
        "edit_layer_not_found"
    );
    assert_eq!(
        stack.create("", EditLayerBlend::Add).expect_err("x").code(),
        "edit_layer_bad_name"
    );
    let bad = EditLayerPatch {
        opacity: Some(1.5),
        ..EditLayerPatch::default()
    };
    assert_eq!(
        stack.set(a, &bad).expect_err("x").code(),
        "edit_layer_bad_opacity"
    );
}

#[test]
fn half_opacity_halves_an_add_layer_and_strokes_still_land_where_painted() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let i = centre_index(&t.map);
    let base = t.map.heights()[i];
    let a = stack.create("A", EditLayerBlend::Add).expect("A");
    stack
        .set(
            a,
            &EditLayerPatch {
                opacity: Some(0.5),
                ..EditLayerPatch::default()
            },
        )
        .expect("half");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let painted = t.map.heights()[i] - base;
    assert!(painted > 0.5, "the stroke shows as painted: {painted}");
    let region = stack
        .set(
            a,
            &EditLayerPatch {
                opacity: Some(1.0),
                ..EditLayerPatch::default()
            },
        )
        .expect("full");
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    let full = t.map.heights()[i] - base;
    assert!(
        (full - painted * 2.0).abs() < 1e-3,
        "full opacity doubles it: {full} vs {painted}"
    );
}

#[test]
fn the_sidecar_round_trips_and_refuses_stale_newer_and_hostile_files() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    let s = stack.create("Plateau", EditLayerBlend::Set).expect("set");
    let flatten = Brush {
        shape: BrushShape::Circle { radius: 4.0 },
        mode: BrushMode::Assign { value: 95.0 },
        hardness: 0.5,
        alpha: 1.0,
        ..Brush::default()
    };
    stroke(&mut t, &mut edits, &mut stack, &flatten, Vec2::new(30.0, -30.0));
    stack
        .set(
            s,
            &EditLayerPatch {
                locked: Some(true),
                opacity: Some(0.75),
                ..EditLayerPatch::default()
            },
        )
        .map(|r| stack.recompute_terrain(&mut t, &mut edits, r))
        .expect("set")
        .expect("recompute");
    let bytes = stack.to_bytes().expect("encode");
    assert_eq!(edit_layers_file_version(&bytes), Some(EDIT_LAYERS_FORMAT_VERSION));
    let back = TerrainEditLayers::from_bytes(&bytes, &t.map).expect("decode");
    assert_eq!(back.layers().len(), 2);
    for (x, y) in back.layers().iter().zip(stack.layers()) {
        assert_eq!(
            (x.id, &x.name, x.enabled, x.locked, x.opacity, x.blend),
            (y.id, &y.name, y.enabled, y.locked, y.opacity, y.blend)
        );
    }
    assert_eq!(back.active(), stack.active());
    assert_eq!(
        back.flatten(),
        stack.flatten(),
        "the decoded stack folds to the same map"
    );
    // Dense storage would be the base, two value arrays and one coverage array.
    let dense = (N * N * 4 * 4) as usize;
    assert!(bytes.len() < dense, "sparse: {} bytes of {dense}", bytes.len());

    // The map changed outside the layers: the file is stale.
    let mut other = t.map.clone();
    other.set_height(centre_index(&t.map), 0.0);
    assert_eq!(
        TerrainEditLayers::from_bytes(&bytes, &other)
            .expect_err("stale")
            .code(),
        "edit_layer_file_stale"
    );
    // A newer file is refused before its body. Version 2 is current; 3 is the next one.
    let mut newer = postcard::to_allocvec(&3u32).expect("v");
    newer.extend_from_slice(&bytes[1..]);
    assert_eq!(
        TerrainEditLayers::from_bytes(&newer, &t.map)
            .expect_err("newer")
            .code(),
        "edit_layer_file_version"
    );
    // A hostile grid is refused before allocating.
    let hostile = postcard::to_allocvec(&(1u32, 1_000_000u32, 1_000_000u32)).expect("h");
    assert_eq!(
        TerrainEditLayers::from_bytes(&hostile, &t.map)
            .expect_err("hostile")
            .code(),
        "edit_layer_file_invalid"
    );
    // Every truncation is an error, never a panic.
    for cut in 0..bytes.len() {
        assert!(TerrainEditLayers::from_bytes(&bytes[..cut], &t.map).is_err());
    }
}

#[test]
fn the_sidecar_sits_beside_the_level() {
    assert_eq!(
        edit_layers_path(Path::new("levels/arena.level.ron")),
        Path::new("levels/arena.terrain.editlayers")
    );
}

#[test]
fn the_sidecar_reads_against_the_quantized_cooked_map() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    stack.create("Hills", EditLayerBlend::Add).expect("layer");
    stroke(&mut t, &mut edits, &mut stack, &raise(0.123_456), centre());
    let bytes = stack.to_bytes().expect("sidecar");
    let cooked = crate::cook_all_tiles(&t.map, 64).expect("cook");
    let hf = crate::stitch_tiled_heightfield(&cooked).expect("stitch");
    let mut loaded = TerrainMap::new(
        &hf,
        t.map.world_width(),
        t.map.height_scale(),
        t.map.height_offset(),
    )
    .expect("reload cooked heights");
    assert_ne!(bits(&loaded), bits(&t.map), "the cook quantizes edited heights");
    let mut back = TerrainEditLayers::from_bytes(&bytes, &loaded).expect("reload sidecar after cook");
    assert_eq!(
        back.flatten(),
        stack.flatten(),
        "the sidecar preserves the authored fold"
    );
    back.recompute(
        &mut loaded,
        DirtyRect {
            col_start: 0,
            col_end: N,
            row_start: 0,
            row_end: N,
        },
    )
    .expect("recompute");
    assert_eq!(
        bits(&loaded),
        bits(&t.map),
        "quantization is not an unlayered edit"
    );
}

#[test]
fn writing_the_base_with_no_active_layer_keeps_layers_on_top() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    let a = stack.create("A", EditLayerBlend::Add).expect("A");
    stroke(&mut t, &mut edits, &mut stack, &raise(2.0), centre());
    stack.set_active(None).expect("base");
    let (_, layer) = stroke(&mut t, &mut edits, &mut stack, &raise(1.0), centre());
    assert_eq!(layer.layer, None, "the stroke went into the base");
    let with_a = bits(&t.map);
    let region = stack
        .set(
            a,
            &EditLayerPatch {
                enabled: Some(false),
                ..EditLayerPatch::default()
            },
        )
        .expect("off");
    stack
        .recompute_terrain(&mut t, &mut edits, region)
        .expect("recompute");
    assert_ne!(bits(&t.map), with_a, "A still sits on top of the edited base");
    assert_eq!(stack.flatten(), t.map.heights());
}

fn small_map() -> TerrainMap {
    TerrainMap::from_heights(4, 4, vec![8.0; 16], 4.0, 10.0, 0.0).expect("map")
}

#[test]
fn weight_layers_renormalize_to_one() {
    let map = small_map();
    let mut stack = TerrainEditLayers::new(&map);
    let index = 5usize;
    let base = stack.folded_weights(index);
    assert_eq!(base[0].to_bits(), 1.0f32.to_bits());
    assert!(base[1..].iter().all(|c| c.to_bits() == 0.0f32.to_bits()));

    let low = stack.create("Low", EditLayerBlend::Add).expect("low");
    let mut delta = [0.0; crate::layers::MAX_LAYERS];
    delta[1] = 1.0;
    let folded = stack.paint_weights(Some(low), index, &delta).expect("low paint");
    assert_eq!(folded[0].to_bits(), 0.5f32.to_bits());
    assert_eq!(folded[1].to_bits(), 0.5f32.to_bits());
    assert!(folded[2..].iter().all(|c| c.to_bits() == 0.0f32.to_bits()));
    assert_eq!(folded.iter().copied().sum::<f32>().to_bits(), 1.0f32.to_bits());

    let high = stack.create("High", EditLayerBlend::Add).expect("high");
    stack
        .set(
            high,
            &EditLayerPatch {
                opacity: Some(0.5),
                ..EditLayerPatch::default()
            },
        )
        .expect("opacity");
    let mut delta = [0.0; crate::layers::MAX_LAYERS];
    delta[0] = -2.0;
    delta[2] = 2.0;
    let folded = stack
        .paint_weights(Some(high), index, &delta)
        .expect("high paint");
    assert_eq!(folded[0].to_bits(), 0.0f32.to_bits());
    assert_eq!(folded[1].to_bits(), 0.5f32.to_bits());
    assert_eq!(folded[2].to_bits(), 0.5f32.to_bits());
    assert!(folded[3..].iter().all(|c| c.to_bits() == 0.0f32.to_bits()));
    assert_eq!(folded.iter().copied().sum::<f32>().to_bits(), 1.0f32.to_bits());

    let mut stack = TerrainEditLayers::new(&map);
    let folded = stack
        .paint_weights(None, index, &[0.0; crate::layers::MAX_LAYERS])
        .expect("zero");
    assert_eq!(folded[0].to_bits(), 1.0f32.to_bits());
    assert!(folded[1..].iter().all(|c| c.to_bits() == 0.0f32.to_bits()));
    let stored = stack
        .base_weights
        .get(&index)
        .copied()
        .expect("explicit zero is stored");
    assert!(stored.iter().all(|c| c.to_bits() == 0.0f32.to_bits()));
}

fn owned_world() -> (World, Entity, u32) {
    let map = small_map();
    let terrain = Terrain::new(
        &map.to_heightfield(),
        4.0,
        10.0,
        0.0,
        CdlodConfig::default(),
        1,
        TerrainLayers::default(),
    )
    .expect("terrain");
    let mut world = World::new();
    let entity = world.spawn((terrain, TerrainEdits::default())).id();
    let id = apply_op(
        &mut world,
        entity,
        &EditLayerOp::Create {
            name: "Hills".to_owned(),
            blend: EditLayerBlend::Add,
        },
    )
    .expect("create")
    .layer
    .expect("id");
    (world, entity, id)
}

#[test]
fn brush_on_owned_layer_is_refused_until_detached() {
    let (mut world, entity, id) = owned_world();
    let target = ProcgenLayerTarget {
        graph: "assets/hills.pgraph.ron".to_owned(),
        output: "height".to_owned(),
    };
    apply_op(
        &mut world,
        entity,
        &EditLayerOp::SetProcgen {
            id,
            target: Some(target.clone()),
        },
    )
    .expect("attach");
    let err = world
        .get::<TerrainEditLayers>(entity)
        .expect("stack")
        .check_target(Some(id))
        .expect_err("owned");
    assert_eq!(err.code(), "edit_layer_owned");
    let text = err.to_string();
    assert!(text.contains(&target.graph), "{text}");
    assert!(text.contains(&target.output), "{text}");
    let undo = {
        let mut query = world.query::<(&mut Terrain, &mut TerrainEdits)>();
        let (mut terrain, mut edits) = query.get_mut(&mut world, entity).expect("terrain");
        crate::apply_stroke(&mut terrain, None, &mut edits, &raise(1.0), &[centre()]).expect("stroke")
    };
    let err = absorb_world_stroke(&mut world, entity, Some(Some(id)), undo).expect_err("stroke");
    assert_eq!(err.code(), "edit_layer_owned");
    assert!(err.to_string().contains(&target.graph), "{err}");
    apply_op(&mut world, entity, &EditLayerOp::SetProcgen { id, target: None }).expect("detach");
    world
        .get::<TerrainEditLayers>(entity)
        .expect("stack")
        .check_target(Some(id))
        .expect("a detached layer accepts a brush");
    let before = {
        let log = world.resource::<TerrainEditLayerLog>();
        log.entries.iter().rev().find_map(|entry| match entry {
            EditLayerLogEntry::Stack { before, label, .. } if label == "Detach terrain layer procgen" => {
                before.clone()
            }
            _ => None,
        })
    }
    .expect("detach logged the owned stack");
    install_stack(&mut world, entity, None, Some(*before)).expect("undo detach");
    let stack = world.get::<TerrainEditLayers>(entity).expect("stack");
    let err = stack.check_target(Some(id)).expect_err("undo restores ownership");
    assert_eq!(err.code(), "edit_layer_owned");
    assert_eq!(stack.layer_for_procgen(&target.graph, &target.output), Some(id));
}

#[test]
fn a_v1_file_migrates_with_identical_heights() {
    let mut t = terrain();
    let mut edits = TerrainEdits::default();
    let mut stack = TerrainEditLayers::new(&t.map);
    stack.create("Hills", EditLayerBlend::Add).expect("layer");
    stroke(&mut t, &mut edits, &mut stack, &raise(1.5), centre());
    let current = stack.to_bytes().expect("v2");
    let file: EditLayersFile = postcard::from_bytes(&current).expect("decode v2");
    let v1 = EditLayersFileV1 {
        version: 1,
        width: file.width,
        height: file.height,
        chunk: file.chunk,
        active: file.active,
        next_id: file.next_id,
        base: file.base.clone(),
        layers: file
            .layers
            .iter()
            .map(|layer| EditLayerFileV1 {
                id: layer.id,
                name: layer.name.clone(),
                enabled: layer.enabled,
                locked: layer.locked,
                opacity: layer.opacity,
                blend: layer.blend,
                values: layer.values.clone(),
                coverage: layer.coverage.clone(),
            })
            .collect(),
    };
    let v1_bytes = postcard::to_allocvec(&v1).expect("encode v1");
    assert_eq!(edit_layers_file_version(&v1_bytes), Some(1));
    let migrated = migrate_edit_layers_bytes(&v1_bytes).expect("migrate");
    assert_eq!(
        edit_layers_file_version(&migrated),
        Some(EDIT_LAYERS_FORMAT_VERSION)
    );
    assert_eq!(migrate_edit_layers_bytes(&migrated).expect("again"), migrated);
    assert_eq!(migrate_edit_layers_bytes(&current).expect("v2 stays"), current);
    let back: EditLayersFile = postcard::from_bytes(&migrated).expect("v2 body");
    assert!(back.base_weights.is_empty());
    for (layer, original) in back.layers.iter().zip(&file.layers) {
        assert!(layer.weights.is_empty());
        assert!(layer.procgen.is_none());
        for (chunk, was) in layer.values.iter().zip(&original.values) {
            let now: Vec<u32> = chunk.values.iter().map(|v| v.to_bits()).collect();
            let then: Vec<u32> = was.values.iter().map(|v| v.to_bits()).collect();
            assert_eq!(now, then);
        }
    }
    for (chunk, was) in back.base.iter().zip(&file.base) {
        let now: Vec<u32> = chunk.values.iter().map(|v| v.to_bits()).collect();
        let then: Vec<u32> = was.values.iter().map(|v| v.to_bits()).collect();
        assert_eq!(now, then);
    }
    let loaded = TerrainEditLayers::from_bytes(&v1_bytes, &t.map).expect("v1 loads");
    assert_eq!(loaded.flatten(), stack.flatten());
    assert!(loaded.layers().iter().all(|layer| layer.procgen().is_none()));
    let hostile = postcard::to_allocvec(&(0u32, 4u32, 4u32)).expect("v0");
    assert_eq!(
        migrate_edit_layers_bytes(&hostile).expect_err("v0").code(),
        "edit_layer_file_version"
    );
}

#[test]
fn weights_and_procgen_target_save_reload_and_undo() {
    let (mut world, entity, id) = owned_world();
    let channels = [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    apply_op(
        &mut world,
        entity,
        &EditLayerOp::PaintWeights {
            layer: Some(id),
            index: 6,
            channels,
        },
    )
    .expect("paint");
    let target = ProcgenLayerTarget {
        graph: "assets/hills.pgraph.ron".to_owned(),
        output: "height".to_owned(),
    };
    apply_op(
        &mut world,
        entity,
        &EditLayerOp::SetProcgen {
            id,
            target: Some(target.clone()),
        },
    )
    .expect("attach");
    let saved = world
        .get::<TerrainEditLayers>(entity)
        .expect("stack")
        .to_bytes()
        .expect("save");
    let map = world.get::<Terrain>(entity).expect("terrain").map.clone();
    let loaded = TerrainEditLayers::from_bytes(&saved, &map).expect("reload");
    assert_eq!(loaded.layer(id).expect("layer").weight_delta(6), channels);
    assert_eq!(loaded.layer(id).expect("layer").procgen(), Some(&target));
    assert_eq!(loaded.to_bytes().expect("resave"), saved);
    let owned = loaded;
    apply_op(&mut world, entity, &EditLayerOp::SetProcgen { id, target: None }).expect("detach");
    apply_op(
        &mut world,
        entity,
        &EditLayerOp::PaintWeights {
            layer: Some(id),
            index: 6,
            channels: [0.0; 8],
        },
    )
    .expect("clear");
    let cleared = world.get::<TerrainEditLayers>(entity).expect("cleared").clone();
    assert!(cleared.layer(id).expect("layer").procgen().is_none());
    assert_eq!(cleared.layer(id).expect("layer").weight_delta(6), [0.0; 8]);
    install_stack(&mut world, entity, Some(&cleared), Some(owned)).expect("undo");
    let back = world.get::<TerrainEditLayers>(entity).expect("restored");
    assert_eq!(back.layer(id).expect("layer").procgen(), Some(&target));
    assert_eq!(back.layer(id).expect("layer").weight_delta(6), channels);
    assert_eq!(
        back.folded_weights(6).iter().copied().sum::<f32>().to_bits(),
        1.0f32.to_bits()
    );
}
