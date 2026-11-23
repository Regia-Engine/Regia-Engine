// SPDX-License-Identifier: MPL-2.0
//! Generator output follows data layers (spec 62 § 7.3, spec 63 § 13.1, ADR-7214): hiding the
//! generator's layer hides its spawned entities and instanced output, showing it restores them,
//! and `spawn.mesh` `target_layer` puts one instance set in its own layer. Headless app with the
//! scene and generator plugins, inline evaluation, fixed frame counts.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::draw::{ProcDrawStats, ProcMeshLibrary, extract_proc_instances};
use regia_procgen::world::{
    ProcBounds, ProcGenMode, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
    ProcLayerFollow, ProcOutput, ProcOutputRoot, generator_presence,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_render::{
    CpuMaterial, CpuMesh, ExtractedCamera, ExtractedFrame, ExtractedTarget, ExtractedView, PostSettings,
};
use regia_scene::components::{EntityGuid, Generated, Visibility};
use regia_scene::layers::{
    LayerDefs, LayerDefsLibrary, LayerMembership, LayerParked, LayerState, LayerTable, Side,
    apply_layer_switches,
};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::spawn::{SnapshotFilter, snapshot_scene};
use regia_scene::{GlobalTransform, Transform};

fn n(ty: &str, s: &[(&str, &str)]) -> NodeEntry {
    let mut e = NodeEntry::new(ty);
    for (k, v) in s {
        e = e.with(k, SettingValue::parse(v).expect("setting"));
    }
    e
}

fn g(nodes: Vec<(u32, NodeEntry)>, edges: &[(u32, &str, u32, &str)]) -> String {
    ProcGraph {
        nodes: nodes.into_iter().collect(),
        edges: edges
            .iter()
            .map(|(a, ap, b, bp)| Edge::new(*a, ap, *b, bp))
            .collect(),
        ..ProcGraph::default()
    }
    .to_ron()
}

fn files() -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    // Three rocks, three crates (prefab entities) and three fences in the `fences` layer, which
    // also collide.
    f.insert(
        "layered.pgraph.ron".to_string(),
        g(
            vec![
                (1, n("gen.grid", &[("count", "(3.0, 1.0, 1.0)")])),
                (2, n("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
                (3, n("spawn.prefab", &[("prefab", "\"props/crate.prefab.ron\"")])),
                (
                    4,
                    n(
                        "spawn.mesh",
                        &[
                            ("entries", "[(mesh: \"fence.glb\", collision: Simple)]"),
                            ("target_layer", "\"fences\""),
                            ("write_attr", "\"\""),
                        ],
                    ),
                ),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in"), (1, "out", 4, "in")],
        ),
    );
    f
}

const LEVEL: LevelId = LevelId(Guid(0x1e));
const DEFS: Guid = Guid(0xd0);
const GENERATOR: Guid = Guid(0x6e6);

/// An app with a level holding one `OnLoad` generator in the layer `quest`; the level binds the
/// layers `quest` and `fences`, both `Live`.
fn app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    let mut lib = ProcMeshLibrary::default();
    for mesh in ["rock.glb", "fence.glb"] {
        lib.insert(
            mesh,
            Arc::new(CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            }),
            Arc::new(CpuMaterial::default()),
        );
    }
    app.insert_resource(lib);
    app.init_resource::<ProcDrawStats>();

    // The level: a snapshot of a generator entity, with its membership, plus the layer binding.
    let mut scratch = App::new();
    scratch.add_plugins(regia_scene::ScenePlugin);
    scratch.add_plugins(ProcGenPlugin);
    scratch.world_mut().spawn((
        ProcGenerator {
            mode: ProcGenMode::OnLoad,
            bounds: ProcBounds::Box {
                half_extents: Vec3::new(20.0, 20.0, 20.0),
            },
            ..ProcGenerator::new("layered.pgraph.ron")
        },
        LayerMembership {
            layers: vec!["quest".into()],
        },
        EntityGuid(GENERATOR),
        Transform::default(),
        GlobalTransform::default(),
    ));
    let mut scene = snapshot_scene(scratch.world_mut(), &SnapshotFilter::default()).expect("snapshot");
    assert_eq!(scene.entities.len(), 1, "the generator record");
    let binding = format!(
        "(defs: [\"asset:{DEFS}\"], entries: [\
            (layer: \"quest\", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone),\
            (layer: \"fences\", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone)])"
    );
    scene.resources.insert(
        "regia_scene::layers::LevelLayers".to_string(),
        ron::value::RawValue::from_boxed_ron(binding.into_boxed_str()).expect("raw ron"),
    );
    let defs = LayerDefs::from_ron_str(
        r#"LayerDefs(format_version: 1, layers: [
            (name: "quest", kind: Runtime, debug_color: (0.2, 0.2, 0.2), parent: None),
            (name: "fences", kind: Runtime, debug_color: (0.4, 0.4, 0.4), parent: None),
        ])"#,
    )
    .expect("defs");
    app.world_mut()
        .resource_mut::<LayerDefsLibrary>()
        .insert(DEFS, defs);
    app.world_mut()
        .resource_mut::<LevelLibrary>()
        .insert(Level::from_scene_file(LEVEL, scene));
    open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
    for _ in 0..4 {
        app.update();
    }
    app
}

fn set_layer(app: &mut App, layer: &str, state: LayerState) {
    app.world_mut()
        .resource_mut::<LayerTable>()
        .request(layer, state, false, Side::Local)
        .expect("known layer");
    for _ in 0..3 {
        app.update();
    }
}

fn the_generator(app: &mut App) -> Option<Entity> {
    let world = app.world_mut();
    world
        .query::<(Entity, &EntityGuid, &ProcGenerator)>()
        .iter(world)
        .find(|(_, g, _)| g.0 == GENERATOR)
        .map(|(e, ..)| e)
}

fn roots(app: &mut App) -> Vec<Entity> {
    let world = app.world_mut();
    world
        .query_filtered::<Entity, With<ProcOutputRoot>>()
        .iter(world)
        .collect()
}

/// Instances the draw extracts this frame.
fn drawn(app: &mut App) -> usize {
    let world = app.world_mut();
    let mut frame = ExtractedFrame::default();
    frame.views.push(ExtractedView {
        camera: ExtractedCamera {
            eye: [0.0; 3],
            target: [0.0, 0.0, -1.0],
            fov_y: 1.0,
            near: 0.1,
            far: 1000.0,
        },
        viewport: [0.0, 0.0, 1.0, 1.0],
        order: 0,
        clear: true,
        target: ExtractedTarget::Window,
        layer_mask: u32::MAX,
        post: PostSettings::default(),
    });
    world.insert_resource(frame);
    use regia_ecs::prelude::bevy_ecs::system::RunSystemOnce;
    world
        .run_system_once(extract_proc_instances)
        .expect("draw system runs");
    world.resource::<ProcDrawStats>().drawn
}

fn is_parked(app: &App, e: Entity) -> bool {
    app.world().get::<LayerParked>(e).is_some()
}

#[test]
fn hiding_the_generators_layer_parks_its_output_and_showing_it_restores_it() {
    let mut app = app();
    let generator = the_generator(&mut app).expect("the level spawned the generator");
    assert!(
        app.world().get::<ProcOutput>(generator).is_some(),
        "the OnLoad run published"
    );
    assert_eq!(drawn(&mut app), 6, "rocks and fences draw");
    let root = roots(&mut app)[0];
    let crates: Vec<Entity> = {
        let world = app.world_mut();
        world
            .query_filtered::<(Entity, &ChildOf), With<Generated>>()
            .iter(world)
            .filter(|(_, c)| c.parent() == root)
            .map(|(e, _)| e)
            .collect()
    };
    assert_eq!(crates.len(), 4, "three crates and the fence collider");
    assert_eq!(
        generator_presence(app.world(), generator),
        (vec!["quest".to_string()], LayerState::Live)
    );

    set_layer(&mut app, "quest", LayerState::Parked);
    assert!(is_parked(&app, generator), "the switch parked the member");
    assert!(is_parked(&app, root), "its output root follows it");
    assert_eq!(app.world().get::<Visibility>(root), Some(&Visibility::Hidden));
    assert!(
        crates.iter().all(|&e| is_parked(&app, e)),
        "and so do the spawned entities"
    );
    assert_eq!(drawn(&mut app), 0, "no instance of a parked generator draws");
    assert_eq!(generator_presence(app.world(), generator).1, LayerState::Parked);

    set_layer(&mut app, "quest", LayerState::Live);
    assert!(!is_parked(&app, root));
    assert_ne!(app.world().get::<Visibility>(root), Some(&Visibility::Hidden));
    assert!(crates.iter().all(|&e| !is_parked(&app, e)));
    assert_eq!(drawn(&mut app), 6, "showing the layer restores the instances");
}

#[test]
fn an_absent_layer_takes_the_output_and_a_live_one_brings_it_back() {
    let mut app = app();
    let generator = the_generator(&mut app).expect("generator");
    let hash = app
        .world()
        .get::<ProcGenState>(generator)
        .and_then(|s| s.output_hash)
        .expect("published");
    assert_eq!(roots(&mut app).len(), 1);

    set_layer(&mut app, "quest", LayerState::Absent);
    assert!(
        the_generator(&mut app).is_none(),
        "the absent member is despawned"
    );
    assert!(roots(&mut app).is_empty(), "its output root went with it");
    let generated = {
        let world = app.world_mut();
        world
            .query_filtered::<Entity, With<Generated>>()
            .iter(world)
            .count()
    };
    assert_eq!(generated, 0, "no spawned entity is left behind");
    assert_eq!(drawn(&mut app), 0);

    set_layer(&mut app, "quest", LayerState::Live);
    for _ in 0..3 {
        app.update();
    }
    let back = the_generator(&mut app).expect("respawned from the level");
    assert_eq!(
        app.world().get::<ProcGenState>(back).and_then(|s| s.output_hash),
        Some(hash),
        "the respawned generator ran again to the same output"
    );
    assert_eq!(roots(&mut app).len(), 1);
    assert_eq!(drawn(&mut app), 6);
}

#[test]
fn target_layer_puts_an_instance_set_in_that_layer() {
    let mut app = app();
    let generator = the_generator(&mut app).expect("generator");
    let layers: Vec<Option<String>> = app
        .world()
        .get::<ProcOutput>(generator)
        .expect("output")
        .0
        .instance_sets
        .iter()
        .map(|s| s.layer.clone())
        .collect();
    assert!(layers.contains(&Some("fences".to_string())), "{layers:?}");
    assert!(layers.contains(&None), "{layers:?}");
    let follower = {
        let world = app.world_mut();
        world
            .query::<(Entity, &ProcLayerFollow)>()
            .iter(world)
            .map(|(e, f)| (e, f.layer.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(follower.len(), 1, "the fences' collider follows its layer");
    assert_eq!(follower[0].1, "fences");
    assert_eq!(drawn(&mut app), 6);

    set_layer(&mut app, "fences", LayerState::Parked);
    assert_eq!(drawn(&mut app), 3, "only the rocks draw");
    assert!(is_parked(&app, follower[0].0), "the fence collider is parked");
    assert!(!is_parked(&app, generator), "the generator is not in `fences`");

    set_layer(&mut app, "fences", LayerState::Live);
    assert_eq!(drawn(&mut app), 6);
    assert!(!is_parked(&app, follower[0].0));
}

#[test]
fn layer_switches_alone_do_not_rerun_the_generator() {
    let mut app = app();
    let generator = the_generator(&mut app).expect("generator");
    let runs = app.world().get::<ProcGenState>(generator).map(|s| s.evaluations);
    set_layer(&mut app, "quest", LayerState::Parked);
    set_layer(&mut app, "quest", LayerState::Live);
    set_layer(&mut app, "fences", LayerState::Parked);
    assert_eq!(
        app.world().get::<ProcGenState>(generator).map(|s| s.evaluations),
        runs
    );
    // `apply_layer_switches` with nothing requested is idle.
    apply_layer_switches(app.world_mut());
}
