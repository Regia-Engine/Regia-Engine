// SPDX-License-Identifier: MPL-2.0
//! Spec 65 in the world: `BiomeArea` capture (shape areas, definitions through the file source,
//! tagged exclusions), the starter graph through the generator lifecycle, and the grass multiplier
//! hand-off into `regia_terrain::grass::GrassMultipliers` (headless app, inline evaluation).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::biome::STARTER_BIOMES_GRAPH;
use regia_procgen::biome::ecs::{AreaShape, BiomeArea, biome_world_for};
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenRunState, ProcGenSettings, ProcGenTerrain, ProcGenerator,
    ProcGraphSource, ProcOutput, run_now,
};
use regia_scene::components::{EntityGuid, Name, Tags};
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::grass::GrassMultipliers;

const MEADOW: &str = r#"BiomeDef(
    name: "Meadow",
    priority: 10,
    layers: [(
        name: "flowers",
        source: Ground(spacing_m: 2.0, jitter: 0.5),
        entries: [(name: "flower", spawn: Mesh("flower.glb"), footprint: (core_m: Some(0.1), spread_m: Some(0.4)))],
    )],
    grass: [(class: "meadow", density: 0.8)],
)"#;

fn app(extra: &[(&str, &str)]) -> App {
    let mut files = BTreeMap::new();
    files.insert("biomes.pgraph.ron".to_string(), STARTER_BIOMES_GRAPH.to_string());
    files.insert("meadow.biome.ron".to_string(), MEADOW.to_string());
    for (k, v) in extra {
        files.insert((*k).to_string(), (*v).to_string());
    }
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(AppTypeRegistry::default());
    let map = regia_terrain::TerrainMap::from_heights(129, 129, vec![1.0; 129 * 129], 128.0, 1.0, 0.0)
        .expect("map");
    app.insert_resource(ProcGenTerrain(map));
    app
}

fn generator(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(60.0, 20.0, 60.0),
                },
                ..ProcGenerator::new("biomes.pgraph.ron")
            },
            EntityGuid(Guid(0x77)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id()
}

/// A meadow area over x in [-60, 0], z in [-60, 60] (a unit box scaled 60 × 10 × 120).
fn meadow_area(app: &mut App, biome: &str) -> Entity {
    app.world_mut()
        .spawn((
            BiomeArea {
                blend_m: 4.0,
                ..BiomeArea::new(biome)
            },
            EntityGuid(Guid(0x100)),
            Name::new("meadow area"),
            boxed(Vec3::new(-30.0, 0.0, 0.0), Vec3::new(60.0, 10.0, 120.0)),
        ))
        .id()
}

fn boxed(at: Vec3, size: Vec3) -> Transform {
    Transform {
        scale: size,
        ..Transform::from_translation(at)
    }
}

fn instances(app: &App, e: Entity) -> Vec<[f32; 3]> {
    app.world()
        .get::<ProcOutput>(e)
        .map(|o| {
            o.0.instance_sets
                .iter()
                .flat_map(|s| s.instances.iter().map(|i| i.translation))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_shape_area_places_inside_itself_and_skips_tagged_exclusions() {
    let mut app = app(&[]);
    let g = generator(&mut app);
    meadow_area(&mut app, "meadow.biome.ron");
    app.world_mut().spawn((
        Tags::from_iter_str(["procgen_exclude"]),
        EntityGuid(Guid(0x200)),
        boxed(Vec3::new(-30.0, 0.0, 0.0), Vec3::new(10.0, 10.0, 10.0)),
    ));
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    let pts = instances(&app, g);
    assert!(pts.len() > 200, "{}", pts.len());
    for p in &pts {
        assert!(
            p[0] >= -60.0 && p[0] <= 0.0 && p[2].abs() <= 60.0,
            "outside the area: {p:?}"
        );
        let inside_zone = (p[0] + 30.0).abs() < 5.0 && p[2].abs() < 5.0;
        assert!(!inside_zone, "inside the exclusion: {p:?}");
    }
    // The captured world names the area and its definition.
    let (bw, problems) = biome_world_for(app.world_mut(), g);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(bw.areas.len(), 1);
    assert_eq!(bw.areas[0].biomes[0].1.name, "Meadow");
    assert_eq!(bw.exclusions.len(), 1);
    let c = bw.claims_at(-30.0, 30.0).owner.expect("owner");
    assert_eq!(c.weight, 1.0);
    assert!(bw.claims_at(30.0, 0.0).owner.is_none());
}

#[test]
fn grass_multipliers_reach_the_terrain_resource() {
    let mut app = app(&[]);
    let g = generator(&mut app);
    meadow_area(&mut app, "meadow.biome.ron");
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    app.update();
    let m = app
        .world()
        .get_resource::<GrassMultipliers>()
        .expect("multipliers written");
    let img = m.by_class.get("meadow").expect("meadow class");
    // Inside the area beyond the band: 0.8; outside every area: 1 (Keep).
    assert!((img.sample(-30.0, 0.0, 128.0) - 0.8).abs() < 0.01);
    assert_eq!(img.sample(30.0, 0.0, 128.0), 1.0);
    // Unchanged output: the resource is not rewritten.
    let before = app.world().resource::<GrassMultipliers>().clone();
    app.update();
    assert_eq!(*app.world().resource::<GrassMultipliers>(), before);
}

#[test]
fn bad_areas_are_reported_and_skipped() {
    let mut app = app(&[(
        "broken.biome.ron",
        "(name: \"b\", layers: [(name: \"l\", entries: [(name: \"e\", children: [(entry: \"nope\")])])])",
    )]);
    let g = generator(&mut app);
    meadow_area(&mut app, "broken.biome.ron");
    app.world_mut().spawn((
        BiomeArea {
            shape: AreaShape::FromSpline,
            ..BiomeArea::new("meadow.biome.ron")
        },
        EntityGuid(Guid(0x300)),
        Transform::default(),
    ));
    app.world_mut().spawn((
        BiomeArea::new("missing.biome.ron"),
        EntityGuid(Guid(0x400)),
        Transform::default(),
    ));
    let (bw, problems) = biome_world_for(app.world_mut(), g);
    assert!(bw.areas.is_empty());
    let mut codes: Vec<&str> = problems.iter().map(|p| p.code.as_str()).collect();
    codes.sort_unstable();
    assert_eq!(codes, ["biome_area_open", "biome_entry_unknown", "biome_unknown"]);
    // The run still succeeds with nothing placed.
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    assert!(instances(&app, g).is_empty());
}

/// Spec 65 § 12 test 10: a biome imported from a `.scatter.ron` places at exactly the scatter
/// positions over the same map, with the same mesh frequencies within ±1 %.
#[test]
fn t10_imported_scatter_places_at_the_scatter_positions() {
    use regia_terrain::scatter::placement::{PlacementInputs, place_set};
    use regia_terrain::scatter::{ScatterClass, ScatterMeshRef, ScatterSet};
    let mesh = |m: &str, w: f32| ScatterMeshRef {
        mesh: m.into(),
        weight: w,
        min_scale: 0.8,
        max_scale: 1.2,
        scale_bias: 1.0,
    };
    let set = ScatterSet {
        seed: 99,
        classes: vec![ScatterClass {
            name: "stones".into(),
            meshes: vec![mesh("props/stone_a.glb", 3.0), mesh("props/stone_b.glb", 1.0)],
            cell_size: 2.0,
            density: 6.0,
            slope_max: 1.5,
            ..ScatterClass::default()
        }],
        ..ScatterSet::default()
    };
    let set_text = set.to_ron().expect("scatter text");
    let def =
        regia_procgen::biome::import::biome_from_scatter_text(&set_text, "stones.scatter.ron", "Stones")
            .expect("import");
    assert_eq!(def.layers.len(), 1);
    assert_eq!(def.layers[0].entries.len(), 2);
    assert_eq!(def.layers[0].entries[0].name, "stone_a");
    let def_text = def.to_ron();
    let mut app = app(&[("stones.scatter.ron", &set_text), ("stones.biome.ron", &def_text)]);
    let g = generator(&mut app);
    app.world_mut().spawn((
        BiomeArea {
            blend_m: 0.0,
            ..BiomeArea::new("stones.biome.ron")
        },
        EntityGuid(Guid(0x500)),
        boxed(Vec3::ZERO, Vec3::new(120.0, 10.0, 120.0)),
    ));
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    // The scatter set's own placement over the same map, inside the area.
    let map = app.world().resource::<ProcGenTerrain>().0.clone();
    let placed = place_set(
        &set,
        PlacementInputs {
            map: &map,
            holes: None,
            layers: None,
            density_maps: None,
        },
    )
    .expect("place");
    let inside = |p: &[f32; 3]| p[0].abs() < 60.0 && p[2].abs() < 60.0;
    let mut want: Vec<[u32; 3]> = placed
        .iter()
        .filter(|p| inside(&p.position))
        .map(|p| p.position.map(f32::to_bits))
        .collect();
    let mut got: Vec<[u32; 3]> = instances(&app, g).iter().map(|p| p.map(f32::to_bits)).collect();
    want.sort_unstable();
    got.sort_unstable();
    assert!(want.len() > 20_000, "{}", want.len());
    assert_eq!(got, want, "exactly the scatter positions");
    // Mesh frequencies: the biome's pick vs the scatter set's pick.
    let total = want.len() as f64;
    let scatter_a = placed
        .iter()
        .filter(|p| inside(&p.position) && p.mesh == 0)
        .count() as f64
        / total;
    let out = app.world().get::<ProcOutput>(g).expect("output");
    let biome_a = out
        .0
        .instance_sets
        .iter()
        .filter(|s| s.mesh == "props/stone_a.glb")
        .map(|s| s.instances.len())
        .sum::<usize>() as f64
        / total;
    eprintln!(
        "t10: {} points; stone_a {biome_a:.4} (biome) vs {scatter_a:.4} (scatter)",
        want.len()
    );
    assert!((biome_a - scatter_a).abs() <= 0.01);
}

/// A scatter class with a density map places through the map the host supplied, and is reported
/// (not placed) when the host supplied none.
#[test]
fn scatter_classes_with_density_maps_read_the_hosts_map() {
    use regia_procgen::biome::ecs::{ProcDensityMaps, capture_biomes};
    use regia_terrain::scatter::placement::DensityMap;
    use regia_terrain::scatter::{ScatterClass, ScatterMeshRef, ScatterSet};
    let set = ScatterSet {
        seed: 3,
        classes: vec![ScatterClass {
            name: "stones".into(),
            meshes: vec![ScatterMeshRef {
                mesh: "props/stone_a.glb".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 1.0,
                scale_bias: 1.0,
            }],
            cell_size: 2.0,
            density: 6.0,
            slope_max: 1.5,
            density_map: Some("stones_density.png".into()),
            ..ScatterClass::default()
        }],
        ..ScatterSet::default()
    };
    let set_text = set.to_ron().expect("scatter text");
    let def =
        regia_procgen::biome::import::biome_from_scatter_text(&set_text, "stones.scatter.ron", "Stones")
            .expect("import")
            .to_ron();
    let mut app = app(&[("stones.scatter.ron", &set_text), ("stones.biome.ron", &def)]);
    let g = generator(&mut app);
    app.world_mut().spawn((
        BiomeArea {
            blend_m: 0.0,
            ..BiomeArea::new("stones.biome.ron")
        },
        EntityGuid(Guid(0x500)),
        boxed(Vec3::ZERO, Vec3::new(120.0, 10.0, 120.0)),
    ));
    // No map supplied: the capture names the problem and the run places nothing.
    let mut deps = Vec::new();
    let (_, problems) = capture_biomes(app.world_mut(), [-60.0, -60.0, 60.0, 60.0], &mut deps);
    assert!(
        problems.iter().any(|p| p.message.contains("stones_density.png")),
        "{problems:?}"
    );
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    assert!(instances(&app, g).is_empty());
    // A map that is 1 for x > 0 and 0 for x < 0 (row-major, 2 × 2 is too coarse for a hard edge).
    let n = 257u32;
    let values: Vec<f32> = (0..n * n)
        .map(|i| if i % n > n / 2 { 1.0 } else { 0.0 })
        .collect();
    let mut maps = ProcDensityMaps::default();
    maps.0.insert(
        "stones_density.png".into(),
        DensityMap::new(n, n, values).expect("map"),
    );
    app.insert_resource(maps);
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    let placed = instances(&app, g);
    assert!(placed.len() > 1000, "{}", placed.len());
    assert!(placed.iter().all(|p| p[0] > -2.0), "only the map's bright half");
    let st = app
        .world()
        .get::<regia_procgen::world::ProcGenState>(g)
        .expect("state");
    assert!(
        st.deps.iter().any(|d| matches!(
            d,
            regia_procgen::world::ProcDependency::File { path } if path == "stones_density.png"
        )),
        "the map is a dependency"
    );
}

#[test]
fn area_edits_and_definition_edits_mark_the_generator_stale() {
    use regia_procgen::world::ProcGenState;
    let mut app = app(&[]);
    let g = generator(&mut app);
    let a = meadow_area(&mut app, "meadow.biome.ron");
    // One frame first: spawning counts as a change.
    app.update();
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    app.update();
    let stale = |app: &App| app.world().get::<ProcGenState>(g).is_some_and(|s| s.stale);
    assert!(!stale(&app));
    // Editing the area component (not its transform) marks the reader stale.
    app.world_mut().get_mut::<BiomeArea>(a).expect("area").blend_m = 2.0;
    app.update();
    assert!(stale(&app), "a BiomeArea edit");
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    app.update();
    assert!(!stale(&app));
    // A definition the run read.
    assert_eq!(
        regia_procgen::biome::ecs::mark_readers_stale(app.world_mut(), "other.biome.ron"),
        0
    );
    assert_eq!(
        regia_procgen::biome::ecs::mark_readers_stale(app.world_mut(), "meadow.biome.ron"),
        1
    );
    assert!(stale(&app));
}

#[test]
fn spline_areas_and_tagged_spline_and_shape_exclusions() {
    use regia_scene::shape::{Shape, ShapeKind};
    use regia_scene::spline::{Spline, TangentMode};
    let mut app = app(&[]);
    let g = generator(&mut app);
    // A closed square spline area over x, z in [-40, 0] (linear segments).
    let mut square = Spline::through([
        Vec3::new(-40.0, 0.0, -40.0),
        Vec3::new(0.0, 0.0, -40.0),
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(-40.0, 0.0, 0.0),
    ])
    .closed();
    for p in &mut square.points {
        p.mode = TangentMode::Linear;
    }
    app.world_mut().spawn((
        BiomeArea {
            shape: AreaShape::FromSpline,
            blend_m: 0.0,
            ..BiomeArea::new("meadow.biome.ron")
        },
        square,
        EntityGuid(Guid(0x600)),
        Transform::default(),
    ));
    // An open-spline area is refused.
    app.world_mut().spawn((
        BiomeArea {
            shape: AreaShape::FromSpline,
            ..BiomeArea::new("meadow.biome.ron")
        },
        Spline::through([Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0)]),
        EntityGuid(Guid(0x601)),
        Transform::default(),
    ));
    // Exclusions: a Shape sphere and an open road-like spline 4 m wide (scale.x).
    app.world_mut().spawn((
        Tags::from_iter_str(["procgen_exclude"]),
        Shape::new(ShapeKind::Sphere { radius: 5.0 }),
        EntityGuid(Guid(0x700)),
        Transform::from_translation(Vec3::new(-30.0, 0.0, -30.0)),
    ));
    let mut road = Spline::through([Vec3::new(-50.0, 0.0, -10.0), Vec3::new(10.0, 0.0, -10.0)]);
    for p in &mut road.points {
        p.scale.x = 4.0;
    }
    app.world_mut().spawn((
        Tags::from_iter_str(["procgen_exclude"]),
        road,
        EntityGuid(Guid(0x701)),
        Transform::default(),
    ));
    let (bw, problems) = biome_world_for(app.world_mut(), g);
    assert_eq!(bw.areas.len(), 1);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].code, "biome_area_open");
    assert_eq!(bw.exclusions.len(), 2);
    assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
    let pts = instances(&app, g);
    assert!(pts.len() > 100, "{}", pts.len());
    for p in &pts {
        assert!(
            p[0] > -40.0 && p[0] < 0.0 && p[2] > -40.0 && p[2] < 0.0,
            "inside the spline: {p:?}"
        );
        let (dx, dz) = (p[0] + 30.0, p[2] + 30.0);
        assert!(dx * dx + dz * dz > 25.0, "outside the sphere: {p:?}");
        assert!((p[2] + 10.0).abs() > 2.0, "off the road: {p:?}");
    }
}

/// Determinism audit finding 2: with several `Terrain` entities, every biome read — the shoreline
/// level, the terrain layers, the painted weights, the scatter placement and the grass-multiplier
/// grid — binds to the lowest-guid terrain, like the ground capture (`capture_inner`), not to
/// whichever terrain the ECS query yields first (query order follows spawn and archetype history,
/// which a save/load cycle changes).
#[test]
fn capture_biomes_follows_lowest_terrain_guid() {
    use regia_procgen::biome::ecs::capture_biomes;
    use regia_terrain::{Terrain, TerrainLayers, WaterPlane};

    let flat_map = |width: f32| {
        regia_terrain::TerrainMap::from_heights(129, 129, vec![1.0; 129 * 129], width, 1.0, 0.0).expect("map")
    };
    let sloped_map = |width: f32| {
        let h: Vec<f32> = (0..129 * 129).map(|i| (i % 129) as f32 * 0.5).collect();
        regia_terrain::TerrainMap::from_heights(129, 129, h, width, 1.0, 0.0).expect("map")
    };
    let terrain_at = |map: &regia_terrain::TerrainMap, width: f32, wet: f32, water: Option<f32>| {
        let layers = TerrainLayers {
            wetness_default: wet,
            ..TerrainLayers::default()
        };
        let mut t = Terrain::new(
            &map.to_heightfield(),
            width,
            1.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            4,
            layers,
        )
        .expect("terrain");
        t.water = water.map(|level| WaterPlane {
            level,
            ..WaterPlane::default()
        });
        t
    };
    let paint = |side: u32| {
        let mut p = regia_terrain::LayerWeightMap::new(side, side, 1).expect("paint");
        p.set(0, 0, 0, 200);
        regia_terrain::TerrainPaint(p)
    };

    let small = |water: Option<f32>| (terrain_at(&flat_map(128.0), 128.0, 0.25, water), paint(129));
    let big = |water: Option<f32>| (terrain_at(&sloped_map(1024.0), 1024.0, 0.75, water), paint(65));

    for swapped in [false, true] {
        for (low_water, high_water, expect_level) in [
            (Some(-2.0f32), Some(7.0f32), Some(-2.0f32)),
            (None, Some(7.0f32), None),
        ] {
            let mut app = app(&[]);
            let (t_low, p_low) = small(low_water);
            let (t_high, p_high) = big(high_water);
            // Spawn the higher guid first on purpose; `swapped` spawns the lower one first.
            if swapped {
                app.world_mut().spawn((t_low, p_low, EntityGuid(Guid(0x1000))));
                app.world_mut().spawn((t_high, p_high, EntityGuid(Guid(0x2000))));
            } else {
                app.world_mut().spawn((t_high, p_high, EntityGuid(Guid(0x2000))));
                app.world_mut().spawn((t_low, p_low, EntityGuid(Guid(0x1000))));
            }
            let mut deps = Vec::new();
            let (bw, problems) = capture_biomes(app.world_mut(), [-60.0, -60.0, 60.0, 60.0], &mut deps);
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(
                bw.water.levels.first().map(|l| l.level),
                expect_level,
                "swapped {swapped}: the shoreline is the lowest-guid terrain's"
            );
            assert!(
                (bw.terrain_layers.as_ref().expect("layers").wetness_default - 0.25).abs() < 1e-6,
                "swapped {swapped}: the terrain layers are the lowest-guid terrain's"
            );
            let painted = bw.painted_layers.as_ref().expect("painted layers");
            assert_eq!(
                (painted.weights.width(), painted.weights.height()),
                (129, 129),
                "swapped {swapped}: the painted weights are the lowest-guid terrain's"
            );
        }
    }
}

/// The grass-multiplier handoff sizes its image from the lowest-guid terrain as well.
#[test]
fn grass_handoff_sizes_from_the_lowest_terrain_guid() {
    use regia_terrain::{Terrain, TerrainLayers};
    let map = |width: f32| {
        regia_terrain::TerrainMap::from_heights(129, 129, vec![1.0; 129 * 129], width, 1.0, 0.0).expect("map")
    };
    let terrain_at = |width: f32| {
        Terrain::new(
            &map(width).to_heightfield(),
            width,
            1.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    };
    for swapped in [false, true] {
        let mut app = app(&[]);
        let g = generator(&mut app);
        meadow_area(&mut app, "meadow.biome.ron");
        if swapped {
            app.world_mut()
                .spawn((terrain_at(128.0), EntityGuid(Guid(0x1000))));
            app.world_mut()
                .spawn((terrain_at(1024.0), EntityGuid(Guid(0x2000))));
        } else {
            app.world_mut()
                .spawn((terrain_at(1024.0), EntityGuid(Guid(0x2000))));
            app.world_mut()
                .spawn((terrain_at(128.0), EntityGuid(Guid(0x1000))));
        }
        assert_eq!(run_now(app.world_mut(), g), Some(ProcGenRunState::Ready));
        app.update();
        let m = app
            .world()
            .get_resource::<GrassMultipliers>()
            .expect("multipliers written");
        let img = m.by_class.get("meadow").expect("meadow class");
        assert_eq!(
            img.side, 128,
            "swapped {swapped}: the multiplier grid sides with the lowest-guid terrain's width"
        );
    }
}

/// Part of determinism audit finding 2: `ScatterClass` placement binds to the lowest-guid
/// terrain's map as well (slope filter: the flat low-guid map places, the sloped high-guid one
/// does not — the old first-hit query order picked whichever terrain spawned first).
#[test]
fn capture_scatter_follows_lowest_terrain_guid() {
    use regia_procgen::biome::ecs::capture_biomes;
    use regia_procgen::biome::import::biome_from_scatter_text;
    use regia_terrain::scatter::{ScatterClass, ScatterMeshRef, ScatterSet};
    use regia_terrain::{Terrain, TerrainLayers};

    let set = ScatterSet {
        seed: 41,
        classes: vec![ScatterClass {
            name: "stones".into(),
            meshes: vec![ScatterMeshRef {
                mesh: "props/stone_a.glb".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 1.0,
                scale_bias: 1.0,
            }],
            cell_size: 4.0,
            density: 2.0,
            slope_max: 0.5,
            ..ScatterClass::default()
        }],
        ..ScatterSet::default()
    };
    let set_text = set.to_ron().expect("scatter text");
    let def = biome_from_scatter_text(&set_text, "flat.scatter.ron", "Stones")
        .expect("import")
        .to_ron();
    let map = |width: f32, sloped: bool| {
        let h: Vec<f32> = (0..129 * 129)
            .map(|i| if sloped { (i % 129) as f32 * 5.0 } else { 1.0 })
            .collect();
        regia_terrain::TerrainMap::from_heights(129, 129, h, width, 1.0, 0.0).expect("map")
    };
    let terrain_at = |width: f32, sloped: bool| {
        Terrain::new(
            &map(width, sloped).to_heightfield(),
            width,
            1.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    };
    let scatter_bits = |app: &mut App| {
        let mut deps = Vec::new();
        let (bw, problems) = capture_biomes(app.world_mut(), [-60.0, -60.0, 60.0, 60.0], &mut deps);
        assert!(problems.is_empty(), "{problems:?}");
        let mut v: Vec<[u32; 3]> = bw
            .scatter
            .values()
            .flat_map(|list| {
                list.iter().map(|c| {
                    [
                        c.position[0].to_bits(),
                        c.position[1].to_bits(),
                        c.position[2].to_bits(),
                    ]
                })
            })
            .collect();
        v.sort_unstable();
        v
    };
    let build = |swapped: bool, only: Option<bool>| {
        let mut app = app(&[("flat.scatter.ron", &set_text), ("stones.biome.ron", &def)]);
        // low guid 0x1000: flat 128 m; high guid 0x2000: sloped 1024 m.
        let low = (terrain_at(128.0, false), EntityGuid(Guid(0x1000)));
        let high = (terrain_at(1024.0, true), EntityGuid(Guid(0x2000)));
        match (swapped, only) {
            (_, Some(true)) => app.world_mut().spawn(low),
            (_, Some(false)) => app.world_mut().spawn(high),
            (false, None) => {
                app.world_mut().spawn(high);
                app.world_mut().spawn(low)
            }
            (true, None) => {
                app.world_mut().spawn(low);
                app.world_mut().spawn(high)
            }
        };
        app.world_mut().spawn((
            BiomeArea {
                blend_m: 0.0,
                ..BiomeArea::new("stones.biome.ron")
            },
            EntityGuid(Guid(0x3000)),
            boxed(Vec3::ZERO, Vec3::new(120.0, 10.0, 120.0)),
        ));
        app
    };

    let low_only = scatter_bits(&mut build(false, Some(true)));
    assert!(!low_only.is_empty(), "the flat map places");
    assert_eq!(
        scatter_bits(&mut build(false, None)),
        low_only,
        "high guid first: scatter still follows the low-guid terrain"
    );
    assert_eq!(
        scatter_bits(&mut build(true, None)),
        low_only,
        "low guid first: same scatter"
    );
    assert_ne!(
        scatter_bits(&mut build(false, Some(false))),
        low_only,
        "the sloped high-guid map alone places differently (the test can tell them apart)"
    );
}
