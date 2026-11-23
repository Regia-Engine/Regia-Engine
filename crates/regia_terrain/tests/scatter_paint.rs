// SPDX-License-Identifier: MPL-2.0
//! Painted mesh instances end to end (`docs/specs/82-terrain-editing.md` § 13, ADR-1936): a brush
//! stroke reaches the frame through the scatter field, one stroke touches only its tiles, the
//! seeded instances stay where they were, the terrain edit journal undoes and redoes it, a sculpt
//! re-seats what it moved, and painted rocks are drawn on the GPU path.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::glam::{Mat4, Vec2, Vec4};
use regia_render::{
    DirectionalLightUniform, ExtractedCamera, ExtractedFrame, ExtractedMesh, ExtractedTarget, ExtractedView,
    FrameMesh, FrameQuality, FrameRenderer, LabScene, LodViewport, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset,
};
use regia_terrain::{
    Brush, BrushMode, BrushShape, ImpostorLayer, ImpostorSettings, PaintedScatter, RockStyle, RuntimeScatter,
    ScatterBrush, ScatterClass, ScatterField, ScatterLibrary, ScatterMeshRef, ScatterPaintInputs,
    ScatterRuntimePlugin, ScatterSet, ScatterSource, ScatterView, StrokeUndo, Terrain, TerrainEdits,
    TerrainMap, TerrainScatterPaint, apply_scatter_stroke, apply_stroke, procedural_rock,
    reseat_painted_scatter, scatter_stroke,
};

const FOV: f32 = 0.9;

fn map_arc() -> Arc<TerrainMap> {
    Arc::new(TerrainMap::from_heights(257, 257, vec![10.0; 257 * 257], 512.0, 1.0, 0.0).expect("map"))
}

fn class(name: &str, density: f32) -> ScatterClass {
    ScatterClass {
        name: name.into(),
        meshes: vec![
            ScatterMeshRef {
                mesh: "rock_a".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 2.0,
                scale_bias: 1.0,
            },
            ScatterMeshRef {
                mesh: "rock_b".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 2.0,
                scale_bias: 1.0,
            },
        ],
        cell_size: 8.0,
        density,
        contact_ao: 0.0,
        fade_start: 400.0,
        fade_end: 500.0,
        ..ScatterClass::default()
    }
}

fn set() -> ScatterSet {
    ScatterSet {
        seed: 41,
        classes: vec![class("seeded", 0.5), class("painted", 0.0)],
        ..ScatterSet::default()
    }
}

fn library() -> ScatterLibrary {
    let grey = [0.42, 0.36, 0.31];
    let mut library = ScatterLibrary::new();
    library.insert("rock_a".into(), procedural_rock(1, RockStyle::angular(grey)));
    library.insert("rock_b".into(), procedural_rock(2, RockStyle::rounded(grey)));
    library
}

fn field() -> ScatterField {
    let mut field =
        ScatterField::new(&set(), ScatterSource::new(map_arc()), &library(), 0xB1).expect("field");
    field.set_background_streaming(false);
    field
}

fn brush(radius: f32, density: f32) -> ScatterBrush {
    ScatterBrush {
        shape: BrushShape::Circle { radius },
        hardness: 1.0,
        density,
        ..ScatterBrush::for_class(&class("painted", 0.0))
    }
}

fn terrain() -> Terrain {
    let map = map_arc();
    let heights = regia_asset::Heightfield {
        width: map.width(),
        height: map.height(),
        samples: vec![10_000; 257 * 257],
    };
    Terrain::new(
        &heights,
        map.world_width(),
        65.535,
        0.0,
        regia_terrain::CdlodConfig::default(),
        4,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain")
}

fn stroke(
    terrain: &Terrain,
    paint: &mut TerrainScatterPaint,
    edits: &mut TerrainEdits,
    b: &ScatterBrush,
    at: [f32; 2],
) -> StrokeUndo {
    let path = [Vec2::new(at[0], at[1])];
    apply_scatter_stroke(terrain, paint, edits, &set(), b, &path).expect("stroke")
}

fn view() -> ScatterView {
    ScatterView::look_at(
        [0.0, 120.0, 150.0],
        [0.0, 10.0, 0.0],
        FOV,
        16.0 / 9.0,
        0.1,
        2000.0,
    )
}

fn emitted(field: &mut ScatterField) -> Vec<ExtractedMesh> {
    field.update(view().eye);
    let mut out = Vec::new();
    field.extract_meshes(&view(), &mut out);
    out
}

fn keys(meshes: &[ExtractedMesh]) -> Vec<u64> {
    let mut keys: Vec<u64> = meshes.iter().map(|m| m.object_key_xor).collect();
    keys.sort_unstable();
    keys
}

#[test]
fn painted_instances_reach_the_frame_instanced_with_the_seeded_ones() {
    let mut field = field();
    let seeded = emitted(&mut field);
    assert!(!seeded.is_empty());

    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    stroke(&t, &mut paint, &mut edits, &brush(40.0, 0.5), [0.0, 0.0]);
    let painted = paint.0.report()[0].instances as usize;
    assert!(painted > 1000, "{painted}");
    field.set_painted(&paint.0);
    let both = emitted(&mut field);
    assert_eq!(
        both.len(),
        seeded.len() + painted,
        "every painted instance is emitted"
    );

    let list = keys(&both);
    assert!(list.windows(2).all(|w| w[0] != w[1]), "object keys are unique");
    // The instancing path: runs of one (mesh, material), not one mesh per instance.
    let mut shared: Vec<*const regia_render::CpuMesh> = both.iter().map(|m| Arc::as_ptr(&m.mesh)).collect();
    shared.sort_unstable();
    shared.dedup();
    assert!(shared.len() <= 4 * 4 * 2, "{} distinct meshes", shared.len());
}

#[test]
fn the_field_places_the_same_tiles_as_a_fresh_one() {
    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    let mut live = field();
    emitted(&mut live);
    for at in [[0.0, 0.0], [90.0, 20.0], [-70.0, -60.0]] {
        stroke(&t, &mut paint, &mut edits, &brush(30.0, 0.4), at);
        live.set_painted(&paint.0);
        emitted(&mut live);
    }
    let mut fresh = field();
    fresh.set_painted(&paint.0);
    assert_eq!(keys(&emitted(&mut live)), keys(&emitted(&mut fresh)));
}

/// A class's field tile is a whole number of its cells, so it need not be 64 m: 65 m, 100 m. Every
/// instance is drawn once, however the storage tiles and field tiles overlap.
#[test]
fn field_tiles_of_any_side_draw_each_instance_once() {
    for cell_size in [5.0, 8.0, 12.0, 100.0] {
        let mut set = set();
        set.classes[0].density = 0.0;
        set.classes[1].cell_size = cell_size;
        let mut field =
            ScatterField::new(&set, ScatterSource::new(map_arc()), &library(), 0xB1).expect("field");
        field.set_background_streaming(false);
        // Above the middle of the map: every tile is in range and in view.
        let eye = [0.0, 300.0, 0.0];
        let above = ScatterView::look_at(eye, [0.0, 0.0, -0.01], 1.6, 1.0, 0.1, 2000.0);
        field.update(eye);

        let t = terrain();
        let mut paint = TerrainScatterPaint::default();
        let mut edits = TerrainEdits::default();
        for at in [[0.0, 0.0], [64.0, -64.0], [-130.0, 40.0]] {
            let b = brush(70.0, 0.3);
            apply_scatter_stroke(&t, &mut paint, &mut edits, &set, &b, &[Vec2::new(at[0], at[1])])
                .expect("stroke");
            field.set_painted(&paint.0);
            field.update(eye);
            assert_eq!(field.len(), paint.0.len(), "cell size {cell_size}: resident");
            let mut out = Vec::new();
            field.extract_meshes(&above, &mut out);
            let list = keys(&out);
            assert!(
                list.windows(2).all(|w| w[0] != w[1]),
                "cell size {cell_size}: drawn twice"
            );
        }
        let erase = ScatterBrush {
            shape: BrushShape::Circle { radius: 40.0 },
            hardness: 1.0,
            ..ScatterBrush::eraser(None)
        };
        apply_scatter_stroke(&t, &mut paint, &mut edits, &set, &erase, &[Vec2::new(64.0, 0.0)])
            .expect("erase");
        field.set_painted(&paint.0);
        field.update(eye);
        assert_eq!(field.len(), paint.0.len(), "cell size {cell_size}: erased");
    }
}

#[test]
fn a_stroke_places_only_its_tiles_again() {
    let mut field = field();
    emitted(&mut field);
    let settled = field.update(view().eye);
    assert_eq!(settled.placed_tiles, 0);

    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    // Inside one storage tile.
    stroke(&t, &mut paint, &mut edits, &brush(8.0, 1.0), [32.0, 32.0]);
    field.set_painted(&paint.0);
    let stats = field.update(view().eye);
    assert_eq!(stats.placed_tiles, 1, "{stats:?}");

    // The same store again costs nothing.
    field.set_painted(&paint.0);
    assert_eq!(field.update(view().eye).placed_tiles, 0);
}

#[test]
fn seeded_instances_stay_put_and_the_journal_undoes_and_redoes() {
    let mut terrain = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    let mut field = field();
    let empty = keys(&emitted(&mut field));

    let undo = stroke(&terrain, &mut paint, &mut edits, &brush(40.0, 0.5), [0.0, 0.0]);
    assert!(!undo.is_empty());
    assert!(edits.unsaved_scatter());
    let bytes = paint.0.to_bytes().expect("bytes");
    field.set_painted(&paint.0);
    let with_paint = keys(&emitted(&mut field));
    assert!(with_paint.len() > empty.len());
    assert!(
        empty.iter().all(|k| with_paint.binary_search(k).is_ok()),
        "seeded keys kept"
    );

    undo.restore(&mut terrain, None, Some(&mut paint), &mut edits, true);
    assert!(paint.0.is_empty());
    field.set_painted(&paint.0);
    assert_eq!(keys(&emitted(&mut field)), empty);

    undo.restore(&mut terrain, None, Some(&mut paint), &mut edits, false);
    assert_eq!(paint.0.to_bytes().expect("bytes"), bytes);
    field.set_painted(&paint.0);
    assert_eq!(keys(&emitted(&mut field)), with_paint);

    // The eraser is a journal entry too.
    let erase = stroke(
        &terrain,
        &mut paint,
        &mut edits,
        &ScatterBrush {
            shape: BrushShape::Circle { radius: 15.0 },
            hardness: 1.0,
            ..ScatterBrush::eraser(Some("painted"))
        },
        [0.0, 0.0],
    );
    assert!(!erase.is_empty());
    field.set_painted(&paint.0);
    let thinned = keys(&emitted(&mut field));
    assert_eq!(thinned.len(), with_paint.len() - erase.len());
    erase.restore(&mut terrain, None, Some(&mut paint), &mut edits, true);
    field.set_painted(&paint.0);
    assert_eq!(keys(&emitted(&mut field)), with_paint);
    assert_eq!(paint.0.to_bytes().expect("bytes"), bytes);
}

/// A placement queued before an edit read the old store: it must not land after it. The ring queued
/// around the first eye is painted over, then the camera moves onto it.
#[test]
fn stale_placements_cannot_land_after_an_edit() {
    let mut near = set();
    near.classes[1].fade_start = 40.0;
    near.classes[1].fade_end = 60.0;
    let streamed = || {
        let mut field =
            ScatterField::new(&near, ScatterSource::new(map_arc()), &library(), 0xB1).expect("field");
        field.set_class_enabled("seeded", false);
        field
    };
    let (first, second) = ([-100.0, 40.0, 0.0], [0.0, 40.0, 0.0]);
    let ahead = ScatterView::look_at(second, [60.0, 10.0, 0.0], FOV, 16.0 / 9.0, 0.1, 2000.0);

    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    let mut live = streamed();
    live.update(first);
    let whole = brush(240.0, 0.05);
    apply_scatter_stroke(&t, &mut paint, &mut edits, &near, &whole, &[Vec2::ZERO]).expect("stroke");
    live.set_painted(&paint.0);
    // Blocks on whatever is still queued, so a stale placement lands if one survived.
    live.settle_streaming(second);

    let mut fresh = streamed();
    fresh.set_painted(&paint.0);
    fresh.settle_streaming(second);
    let mut out = Vec::new();
    live.extract_meshes(&ahead, &mut out);
    let mut expected = Vec::new();
    fresh.extract_meshes(&ahead, &mut expected);
    assert!(!expected.is_empty());
    assert_eq!(keys(&out), keys(&expected));
}

#[test]
fn a_class_or_mesh_the_set_dropped_is_kept_and_not_drawn() {
    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    stroke(&t, &mut paint, &mut edits, &brush(30.0, 0.5), [0.0, 0.0]);

    let mut without_rock_b = set();
    without_rock_b.classes[1].meshes.truncate(1);
    let mut only_a =
        ScatterField::new(&without_rock_b, ScatterSource::new(map_arc()), &library(), 0xB1).expect("field");
    only_a.set_background_streaming(false);
    let base = emitted(&mut only_a).len();
    only_a.set_painted(&paint.0);
    let drawn = emitted(&mut only_a).len() - base;
    let of_a = paint
        .0
        .instances("painted")
        .filter(|i| paint.0.mesh_path("painted", i) == Some("rock_a"))
        .count();
    assert_eq!(drawn, of_a);
    assert!(of_a > 0 && of_a < paint.0.len());

    let mut no_class = set();
    no_class.classes.truncate(1);
    let mut seeded =
        ScatterField::new(&no_class, ScatterSource::new(map_arc()), &library(), 0xB1).expect("field");
    seeded.set_background_streaming(false);
    let base = emitted(&mut seeded).len();
    seeded.set_painted(&paint.0);
    assert_eq!(emitted(&mut seeded).len(), base);
    assert!(!paint.0.is_empty());
}

/// An app with the scatter on the ECS frame and a terrain with painted instances.
fn app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(ScatterRuntimePlugin);
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(LodViewport {
        width: 1280,
        height: 720,
    });
    app.insert_resource(RuntimeScatter::new(field()));
    app.add_systems(Update, reseat_painted_scatter);
    let view = ScatterView::look_at(
        [0.0, 120.0, 150.0],
        [0.0, 10.0, 0.0],
        FOV,
        16.0 / 9.0,
        0.1,
        2000.0,
    );
    app.world_mut()
        .resource_mut::<ExtractedFrame>()
        .views
        .push(ExtractedView {
            camera: ExtractedCamera {
                eye: view.eye,
                target: [0.0, 10.0, 0.0],
                fov_y: FOV,
                near: 0.1,
                far: 2000.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: PostSettings::default(),
        });
    let entity = app
        .world_mut()
        .spawn((terrain(), TerrainScatterPaint::default(), TerrainEdits::default()))
        .id();
    (app, entity)
}

fn frame_meshes(app: &mut App) -> usize {
    app.world_mut().resource_mut::<ExtractedFrame>().meshes.clear();
    app.world_mut().run_schedule(Render);
    app.world().resource::<ExtractedFrame>().meshes.len()
}

fn paint_in(world: &mut World, entity: Entity, b: &ScatterBrush, at: [f32; 2]) -> StrokeUndo {
    let mut query = world.query::<(&Terrain, &mut TerrainScatterPaint, &mut TerrainEdits)>();
    let (terrain, mut paint, mut edits) = query.get_mut(world, entity).expect("terrain");
    stroke(terrain, &mut paint, &mut edits, b, at)
}

#[test]
fn the_ecs_frame_draws_a_stroke_on_the_next_frame() {
    let (mut app, entity) = app();
    let before = frame_meshes(&mut app);
    assert!(before > 0);
    let undo = paint_in(app.world_mut(), entity, &brush(30.0, 0.5), [0.0, 0.0]);
    let after = frame_meshes(&mut app);
    assert_eq!(after, before + undo.len());
    assert_eq!(frame_meshes(&mut app), after, "a held camera keeps them");
}

#[test]
fn a_sculpt_moves_the_painted_instances_and_undoing_it_puts_them_back() {
    let (mut app, entity) = app();
    paint_in(app.world_mut(), entity, &brush(60.0, 0.2), [0.0, 0.0]);
    app.world_mut().run_schedule(Update);
    let before = app
        .world()
        .get::<TerrainScatterPaint>(entity)
        .expect("paint")
        .clone();
    app.world_mut()
        .get_mut::<TerrainEdits>(entity)
        .expect("edits")
        .take_unsaved_scatter();

    let raise = Brush {
        shape: BrushShape::Circle { radius: 30.0 },
        mode: BrushMode::Raise { amount: 6.0 },
        hardness: 1.0,
        ..Brush::default()
    };
    let mut query = app.world_mut().query::<(&mut Terrain, &mut TerrainEdits)>();
    let world = app.world_mut();
    let (mut terrain, mut edits) = query.get_mut(world, entity).expect("terrain");
    let sculpt = apply_stroke(&mut terrain, None, &mut edits, &raise, &[Vec2::ZERO]).expect("sculpt");
    assert!(edits.scatter_reseat().is_some());
    app.world_mut().run_schedule(Update);

    let world = app.world();
    let paint = world.get::<TerrainScatterPaint>(entity).expect("paint");
    let terrain = world.get::<Terrain>(entity).expect("terrain");
    assert_ne!(*paint, before, "the sculpt moved them");
    for i in paint.0.instances("painted") {
        assert_eq!(
            i.position[1],
            terrain.map.sample_bilinear(i.position[0], i.position[2])
        );
    }
    assert!(
        world
            .get::<TerrainEdits>(entity)
            .expect("edits")
            .unsaved_scatter()
    );

    let mut query = app.world_mut().query::<(&mut Terrain, &mut TerrainEdits)>();
    let world = app.world_mut();
    let (mut terrain, mut edits) = query.get_mut(world, entity).expect("terrain");
    sculpt.restore(&mut terrain, None, None, &mut edits, true);
    app.world_mut().run_schedule(Update);
    assert_eq!(
        *app.world().get::<TerrainScatterPaint>(entity).expect("paint"),
        before,
        "bit for bit"
    );
}

/// Serializes device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A painted edit changes tiles under the same ids, which the impostor layer's per-tile upload
/// cannot see: without the re-upload it keeps the old instances (lavapipe).
#[test]
fn the_impostor_layer_follows_a_painted_edit() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let settings = ImpostorSettings {
        grid: 4,
        cell: 16,
        ..ImpostorSettings::default()
    };
    let eye = view().eye;
    let mut field = field();
    field.set_class_enabled("seeded", false);
    field.set_impostors(Some(settings));
    field.settle_streaming(eye);
    let atlas = field.bake_impostors(Some(settings));
    let mut layer = ImpostorLayer::new(&device.device, &device.queue, &atlas);
    assert_eq!(layer.update(&device.queue, &field), 0);

    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    stroke(&t, &mut paint, &mut edits, &brush(40.0, 0.2), [0.0, 0.0]);
    field.set_painted(&paint.0);
    field.settle_streaming(eye);
    assert_eq!(layer.update(&device.queue, &field) as usize, paint.0.len());

    let eraser = ScatterBrush {
        shape: BrushShape::Circle { radius: 20.0 },
        hardness: 1.0,
        ..ScatterBrush::eraser(None)
    };
    let erase = stroke(&t, &mut paint, &mut edits, &eraser, [0.0, 0.0]);
    assert!(!erase.is_empty());
    field.set_painted(&paint.0);
    field.settle_streaming(eye);
    assert_eq!(layer.update(&device.queue, &field) as usize, paint.0.len());
}

/// Painted rocks on the GPU path: the same frame with and without the stroke, on the renderer's
/// instanced path (lavapipe).
#[test]
fn painted_rocks_render_where_the_brush_put_them() {
    const W: u32 = 240;
    const H: u32 = 136;
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let mut field = field();
    field.set_class_enabled("seeded", false);
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: false,
        ..PostSettings::default()
    };
    let eye = [0.0, 30.0, 60.0];
    let target = [0.0, 10.0, 0.0];
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.render_scale_permille = 1000;
    let view =
        ScatterView::look_at(eye, target, FOV, W as f32 / H as f32, 0.1, 2000.0).with_sun([-0.55, 0.45, 0.7]);

    let mut draw = |field: &mut ScatterField| -> (Vec<u8>, u32, u32) {
        field.update(eye);
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: FOV,
            near: 0.1,
            far: 2000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new([0.55, -0.45, -0.7], [1.0, 0.95, 0.88], 90_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
        let stats = field.collect(&view, &mut meshes);
        scene.meshes.extend(meshes);
        let result = renderer
            .render_offscreen(&device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        (result.pixels, stats.emitted, renderer.draw_stats().drawn as u32)
    };

    let (empty, none, _) = draw(&mut field);
    assert_eq!(none, 0);

    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    let b = ScatterBrush {
        scale: [1.5, 2.5],
        ..brush(14.0, 0.3)
    };
    stroke(&t, &mut paint, &mut edits, &b, [0.0, 0.0]);
    field.set_painted(&paint.0);
    let (painted, emitted, drawn) = draw(&mut field);
    assert_eq!(emitted as usize, paint.0.len());
    assert!(drawn > 0);

    // Every instance covering a few pixels changes the frame there.
    let vp = Mat4::from_cols_array_2d(&view.view_proj);
    let focal = (H as f32 * 0.5) / (FOV * 0.5).tan();
    let (mut expected, mut found) = (0, 0);
    for (center, radius) in field.instance_spheres() {
        let clip = vp * Vec4::new(center[0], center[1], center[2], 1.0);
        if clip.w < 1.0 {
            continue;
        }
        let ndc = clip.truncate() / clip.w;
        if ndc.x.abs() > 0.95 || ndc.y.abs() > 0.95 || radius * focal / clip.w < 2.0 {
            continue;
        }
        let x = ((ndc.x * 0.5 + 0.5) * W as f32) as usize;
        let y = ((0.5 - ndc.y * 0.5) * H as f32) as usize;
        let i = (y.min(H as usize - 1) * W as usize + x.min(W as usize - 1)) * 4;
        expected += 1;
        if (0..3).any(|k| painted[i + k] != empty[i + k]) {
            found += 1;
        }
    }
    assert!(expected > 10, "{expected}");
    assert!(found * 10 >= expected * 9, "{found} of {expected}");
    // Nothing outside the painted patch and its shadows: the far left of the frame is untouched.
    let row = (H / 2) as usize * W as usize;
    assert_eq!(painted[row * 4..(row + 4) * 4], empty[row * 4..(row + 4) * 4]);
}

/// Numbers for the lane report, not a budget:
/// `cargo test --release -p regia_terrain --test scatter_paint bench -- --ignored --nocapture`.
#[test]
#[ignore = "measurement"]
fn bench_scatter_paint() {
    use std::time::Instant;
    let map = Arc::new(
        TerrainMap::from_heights(1025, 1025, vec![10.0; 1025 * 1025], 2048.0, 1.0, 0.0).expect("map"),
    );
    let mut field =
        ScatterField::new(&set(), ScatterSource::new(Arc::clone(&map)), &library(), 0xB1).expect("field");
    field.set_background_streaming(false);
    let mut store = PaintedScatter::new();
    let inputs = ScatterPaintInputs {
        map: &map,
        holes: None,
        layers: None,
        seed: 41,
    };
    let set = set();
    let run = |store: &mut PaintedScatter, b: &ScatterBrush, at: [f32; 2]| {
        let start = Instant::now();
        let stroke = scatter_stroke(store, &set, inputs, b, &[Vec2::new(at[0], at[1])]).expect("stroke");
        (start.elapsed().as_secs_f64() * 1e3, stroke.len())
    };
    let (ms, n) = run(&mut store, &brush(900.0, 0.5), [0.0, 0.0]);
    println!("whole map stroke: {n} instances, {ms:.1} ms");
    field.update(view().eye);
    let start = Instant::now();
    field.set_painted(&store);
    let set_ms = start.elapsed().as_secs_f64() * 1e3;
    let stats = field.update(view().eye);
    let update_ms = start.elapsed().as_secs_f64() * 1e3 - set_ms;
    println!(
        "first sync: set_painted {set_ms:.2} ms, update {update_ms:.1} ms, {} tiles",
        stats.placed_tiles
    );

    for (radius, density) in [(8.0, 1.0), (32.0, 1.0)] {
        let (ms, n) = run(
            &mut store,
            &ScatterBrush {
                seed: 3,
                ..brush(radius, density)
            },
            [32.0, 32.0],
        );
        let start = Instant::now();
        field.set_painted(&store);
        let set_ms = start.elapsed().as_secs_f64() * 1e3;
        let stats = field.update(view().eye);
        let update_ms = start.elapsed().as_secs_f64() * 1e3 - set_ms;
        println!(
            "stroke r {radius} on {} instances: {n} added, {ms:.2} ms; set_painted {set_ms:.3} ms, update {update_ms:.2} ms, {} tiles",
            store.len(),
            stats.placed_tiles
        );
    }
    let start = Instant::now();
    field.set_painted(&store);
    println!("idle set_painted: {:.4} ms", start.elapsed().as_secs_f64() * 1e3);
    let start = Instant::now();
    let bytes = store.to_bytes().expect("save");
    let save_ms = start.elapsed().as_secs_f64() * 1e3;
    let start = Instant::now();
    let back = PaintedScatter::from_bytes(&bytes).expect("load");
    println!(
        "file: {} bytes ({:.1} per instance), save {save_ms:.1} ms, load {:.1} ms",
        bytes.len(),
        bytes.len() as f64 / back.len() as f64,
        start.elapsed().as_secs_f64() * 1e3
    );
}
