// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 18 test 1 (partition) and the recook guarantees of § 6.4.

use super::*;
use regia_core::id::Guid;
use regia_scene::cooked::Payload;
use regia_test_utils::unique_temp_dir;

fn guid(n: u128) -> Guid {
    Guid::from_u128(n)
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    register_cook_types(&mut registry);
    registry
}

const TRANSFORM: &str = "regia_scene::transform::Transform";

fn at(x: f32, y: f32, z: f32) -> String {
    format!("(translation: ({x}, {y}, {z}))")
}

/// Text of one entity record.
fn entity(id: u128, parent: Option<u128>, components: &[(&str, String)]) -> String {
    let parent = parent.map_or(String::new(), |p| format!("parent: Some(\"{}\"),", guid(p)));
    let body: String = components.iter().map(|(k, v)| format!("\"{k}\": {v},")).collect();
    format!(
        "(guid: \"{}\", name: \"e{id}\", {parent} components: {{ {body} }}),",
        guid(id)
    )
}

fn settings_text(extra: &str) -> String {
    format!(
        "\"regia_scene::streaming::LevelStreaming\": (enabled: true, cell_size_m: 100.0, load_radius_m: 150.0, \
         unload_radius_m: 200.0, hlod: None, {extra})"
    )
}

fn level(entities: &[String], extra: &str) -> SceneFile {
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ {} }})",
        entities.concat(),
        settings_text(extra)
    );
    SceneFile::from_ron_str(&text).expect("level")
}

fn settings() -> LevelStreaming {
    level_streaming(&level(&[], ""), &registry())
        .expect("settings")
        .expect("streamed")
}

fn split(scene: &SceneFile) -> Partition {
    partition(scene, settings(), &registry(), &()).expect("partition")
}

fn cell(x: i64, z: i64) -> CellCoord {
    CellCoord::new(x, z)
}

fn guids(cell: &PartitionedCell) -> Vec<Guid> {
    cell.entities.iter().map(|e| e.guid).collect()
}

fn lvl() -> LevelId {
    LevelId(guid(0xabc))
}

#[test]
fn an_entity_goes_to_the_cell_of_its_center_with_half_open_cells() {
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
            entity(2, None, &[(TRANSFORM, at(100.0, 0.0, 50.0))]),
            entity(3, None, &[(TRANSFORM, at(-0.5, 0.0, 99.9))]),
        ],
        "",
    );
    let p = split(&scene);
    assert_eq!(guids(&p.cells[&cell(0, 0)]), vec![guid(1)]);
    assert_eq!(guids(&p.cells[&cell(1, 0)]), vec![guid(2)]);
    assert_eq!(guids(&p.cells[&cell(-1, 0)]), vec![guid(3)]);
    assert_eq!(p.bounds, [cell(-1, 0), cell(1, 0)]);
}

#[test]
fn a_hierarchy_moves_with_its_root_root_first_and_contiguous() {
    let scene = level(
        &[
            // The child would land in cell 2 on its own.
            entity(9, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
            entity(2, Some(9), &[(TRANSFORM, at(10.0, 0.0, 0.0))]),
            entity(5, Some(2), &[(TRANSFORM, at(1.0, 0.0, 0.0))]),
            entity(1, Some(9), &[(TRANSFORM, at(2.0, 0.0, 0.0))]),
            entity(4, None, &[(TRANSFORM, at(60.0, 0.0, 50.0))]),
        ],
        "",
    );
    let p = split(&scene);
    assert_eq!(p.cells.len(), 1);
    // Hierarchies by root guid; each one root first, then depth first by guid.
    assert_eq!(
        guids(&p.cells[&cell(0, 0)]),
        vec![guid(4), guid(9), guid(1), guid(2), guid(5)]
    );
}

#[test]
fn a_forced_cell_wins_over_the_position() {
    let scene = level(
        &[entity(
            1,
            None,
            &[
                (TRANSFORM, at(50.0, 0.0, 50.0)),
                (
                    "regia_scene::streaming::StreamPlacement",
                    "Cell(x: 7, z: -3)".to_owned(),
                ),
            ],
        )],
        "",
    );
    assert!(split(&scene).cells.contains_key(&cell(7, -3)));
}

#[test]
fn pinned_persistent_shapeless_and_oversized_entities_stay_with_the_level() {
    let scene = level(
        &[
            entity(
                1,
                None,
                &[
                    (TRANSFORM, at(50.0, 0.0, 50.0)),
                    (
                        "regia_scene::streaming::StreamPlacement",
                        "AlwaysLoaded".to_owned(),
                    ),
                ],
            ),
            entity(
                2,
                None,
                &[
                    (TRANSFORM, at(50.0, 0.0, 50.0)),
                    ("regia_scene::level::Persistent", "()".to_owned()),
                ],
            ),
            // Game rules: no transform, no extent.
            entity(3, None, &[]),
            entity(
                4,
                None,
                &[
                    (TRANSFORM, at(50.0, 0.0, 50.0)),
                    (
                        "regia_physics::components::Collider",
                        "(shape: Box(half_extents: (150.0, 1.0, 1.0)))".to_owned(),
                    ),
                ],
            ),
            entity(5, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
        ],
        "",
    );
    let p = split(&scene);
    let always: Vec<Guid> = p.always.entities.iter().map(|e| e.guid).collect();
    assert_eq!(always, vec![guid(1), guid(2), guid(3), guid(4)]);
    assert_eq!(guids(&p.cells[&cell(0, 0)]), vec![guid(5)]);
    assert_eq!(p.warnings.len(), 1);
    assert_eq!(p.warnings[0].code, "stream_large_entity");
    assert!(p.warnings[0].message.contains("e4"), "{}", p.warnings[0].message);
}

/// The nav cook takes what the runtime's nav takes: a collider whose nearest body is static.
#[test]
fn nav_geometry_is_the_static_colliders_only() {
    const COLLIDER: &str = "regia_physics::components::Collider";
    const BODY: &str = "regia_physics::components::RigidBody";
    let block = "(shape: Box(half_extents: (1.0, 1.0, 1.0)))".to_owned();
    let scene = level(
        &[
            // A static body with a collider, and a bodyless collider on its child.
            entity(
                1,
                None,
                &[
                    (TRANSFORM, at(0.0, 0.0, 0.0)),
                    (BODY, "(kind: Static)".to_owned()),
                    (COLLIDER, block.clone()),
                ],
            ),
            entity(
                2,
                Some(1),
                &[(TRANSFORM, at(10.0, 0.0, 0.0)), (COLLIDER, block.clone())],
            ),
            // A dynamic body with a collider, and one on its child.
            entity(
                3,
                None,
                &[
                    (TRANSFORM, at(40.0, 0.0, 0.0)),
                    (BODY, "(kind: Dynamic)".to_owned()),
                    (COLLIDER, block.clone()),
                ],
            ),
            entity(
                4,
                Some(3),
                &[(TRANSFORM, at(10.0, 0.0, 0.0)), (COLLIDER, block.clone())],
            ),
            // A collider with no body at all: the runtime's nav does not take it either.
            entity(5, None, &[(TRANSFORM, at(80.0, 0.0, 0.0)), (COLLIDER, block)]),
        ],
        "",
    );
    let triangles = static_collider_triangles(&scene, &registry());
    assert_eq!(
        triangles.len(),
        12,
        "one box: the runtime's nav takes a collider with its own static body only"
    );
    let max_x = triangles.iter().flatten().map(|v| v.x).fold(f32::MIN, f32::max);
    assert_eq!(max_x, 1.0, "the static body's own box");
}

/// A static body under a moved group sits where the group puts it.
#[test]
fn nav_geometry_takes_the_world_pose_of_a_static_body() {
    const COLLIDER: &str = "regia_physics::components::Collider";
    const BODY: &str = "regia_physics::components::RigidBody";
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(100.0, 0.0, 0.0))]),
            entity(
                2,
                Some(1),
                &[
                    (TRANSFORM, at(10.0, 0.0, 0.0)),
                    (BODY, "(kind: Static)".to_owned()),
                    (COLLIDER, "(shape: Box(half_extents: (1.0, 1.0, 1.0)))".to_owned()),
                ],
            ),
        ],
        "",
    );
    let triangles = static_collider_triangles(&scene, &registry());
    assert_eq!(triangles.len(), 12);
    let max_x = triangles.iter().flatten().map(|v| v.x).fold(f32::MIN, f32::max);
    assert_eq!(max_x, 111.0);
}

/// A grid the runtime would refuse fails the cook.
#[test]
fn a_nav_grid_past_the_runtime_limit_fails_the_cook() {
    let scene = level(
        &[entity(
            1,
            None,
            &[
                (TRANSFORM, at(0.0, 0.0, 0.0)),
                (
                    "regia_physics::components::RigidBody",
                    "(kind: Static)".to_owned(),
                ),
                (
                    "regia_physics::components::Collider",
                    "(shape: Box(half_extents: (20000.0, 1.0, 20000.0)))".to_owned(),
                ),
            ],
        )],
        "",
    );
    let mut settings = settings();
    settings.nav = Some(regia_scene::streaming::StreamNavCook {
        cell_size: 0.25,
        ..regia_scene::streaming::StreamNavCook::default()
    });
    let mut split = partition(&scene, settings, &registry(), &()).expect("partition");
    let grid = split.settings.grid().expect("grid");
    let error = cook_nav_tiles(&scene, &mut split, &grid, &registry(), &[])
        .err()
        .expect("too large");
    assert!(error.to_string().contains("raise cell_size"), "{error}");
}

/// A light partitions by what it lights, not by its position alone (spec 78 § 5.3).
#[test]
fn a_lights_reach_counts_in_the_bounds() {
    const POINT: &str = "regia_render::lights::PointLight";
    let light = |radius: f32| {
        format!("(color: (1.0, 1.0, 1.0), intensity: 100.0, radius: {radius}, cast_shadows: false)")
    };
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(50.0, 5.0, 50.0)), (POINT, light(20.0))]),
            entity(
                2,
                None,
                &[(TRANSFORM, at(250.0, 5.0, 50.0)), (POINT, light(500.0))],
            ),
        ],
        "",
    );
    let p = split(&scene);
    let near = &p.cells[&cell(0, 0)];
    assert_eq!(guids(near), vec![guid(1)]);
    assert_eq!((near.min_y, near.max_y), (-15.0, 25.0), "the sphere's Y range");
    assert!(
        p.always.entities.iter().any(|e| e.guid == guid(2)),
        "too wide for a cell"
    );
    assert_eq!(p.warnings[0].code, "stream_large_entity");
}

#[test]
fn collider_and_mesh_bounds_pick_the_cell_and_the_y_range() {
    struct Mesh;
    impl StreamAssets for Mesh {
        fn cost(&self, id: AssetId) -> Option<AssetCost> {
            (id.0 == guid(0xdead)).then_some(AssetCost {
                ram_bytes: 100,
                vram_bytes: 40,
                bounds: Some([[0.0, 0.0, 0.0], [120.0, 30.0, 0.0]]),
            })
        }
    }
    let scene = level(
        &[entity(
            1,
            None,
            &[
                (TRANSFORM, at(10.0, 5.0, 50.0)),
                ("my_game::MeshUser", format!("(mesh: \"{}\")", guid(0xdead))),
            ],
        )],
        "",
    );
    let p = partition(&scene, settings(), &registry(), &Mesh).expect("partition");
    // Position 10 is in cell 0, the box center 70 too; the Y range comes from the mesh.
    let c = &p.cells[&cell(0, 0)];
    assert_eq!((c.min_y, c.max_y), (5.0, 35.0));
    assert!(c.deps.contains(&AssetId(guid(0xdead))));

    // Far enough along X that the center of the bounds crosses a cell edge.
    let scene = level(
        &[entity(
            1,
            None,
            &[
                (TRANSFORM, at(60.0, 0.0, 50.0)),
                ("my_game::MeshUser", format!("(mesh: \"{}\")", guid(0xdead))),
            ],
        )],
        "",
    );
    let p = partition(&scene, settings(), &registry(), &Mesh).expect("partition");
    assert!(p.cells.contains_key(&cell(1, 0)), "{:?}", p.cells.keys());
}

#[test]
fn y_ranges_union_the_members() {
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(10.0, -4.0, 10.0))]),
            entity(2, None, &[(TRANSFORM, at(20.0, 9.0, 10.0))]),
        ],
        "",
    );
    let p = split(&scene);
    assert_eq!(
        (p.cells[&cell(0, 0)].min_y, p.cells[&cell(0, 0)].max_y),
        (-4.0, 9.0)
    );
}

#[test]
fn file_order_does_not_change_the_result() {
    let records = vec![
        entity(3, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
        entity(1, None, &[(TRANSFORM, at(60.0, 0.0, 50.0))]),
        entity(2, Some(1), &[(TRANSFORM, at(1.0, 0.0, 0.0))]),
    ];
    let forward = level(&records, "");
    let mut backward = records;
    backward.reverse();
    let backward = level(&backward, "");
    let bytes = |scene: &SceneFile| {
        let dir = unique_temp_dir("stream_cook_order");
        cook_streamed_level(lvl(), scene, &registry(), &(), Some(&dir), "a.level.ron")
            .expect("cook")
            .expect("streamed");
        std::fs::read(dir.join(cooked_file_name(cell_blob_id(lvl(), cell(0, 0))))).expect("blob")
    };
    assert_eq!(bytes(&forward), bytes(&backward));
}

#[test]
fn a_priority_volume_sets_the_record_of_every_overlapped_cell() {
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
            entity(2, None, &[(TRANSFORM, at(150.0, 0.0, 50.0))]),
            entity(
                3,
                None,
                &[
                    (TRANSFORM, at(160.0, 0.0, 50.0)),
                    (
                        "regia_scene::streaming::StreamPriorityVolume",
                        "(half_extents: (5.0, 1.0, 5.0), priority: 4)".to_owned(),
                    ),
                ],
            ),
        ],
        "",
    );
    let p = split(&scene);
    assert_eq!(p.cells[&cell(0, 0)].priority, 0);
    assert_eq!(p.cells[&cell(1, 0)].priority, 4);
}

const PRIORITY_VOLUME: &str = "regia_scene::streaming::StreamPriorityVolume";

/// Audit asset_pipeline1 #3: a volume on a child sits where its parent puts it. The child's local
/// translation is 0; its world position is x = 550, in cell 5, where the hierarchy goes.
#[test]
fn a_child_priority_volume_covers_the_cell_of_its_world_position() {
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(550.0, 0.0, 50.0))]),
            entity(
                2,
                Some(1),
                &[
                    (TRANSFORM, at(0.0, 0.0, 0.0)),
                    (
                        PRIORITY_VOLUME,
                        "(half_extents: (10.0, 10.0, 10.0), priority: 5)".to_owned(),
                    ),
                ],
            ),
            // A hierarchy in the cell the local translation names, which the volume must not reach.
            entity(3, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
        ],
        "",
    );
    let p = split(&scene);
    assert_eq!(p.cells[&cell(5, 0)].priority, 5, "the child's world cell");
    assert_eq!(
        p.cells[&cell(0, 0)].priority,
        0,
        "not the cell of its local translation"
    );
}

/// Audit asset_pipeline1 #3: a yawed volume covers its rotated box. 1 x 20 m local, turned 90°
/// about Y, it is 40 m wide along X and reaches from cell 0 into cell 1.
#[test]
fn a_rotated_priority_volume_covers_its_turned_box() {
    let yawed = "(translation: (95.0, 0.0, 50.0), rotation: (0.0, 0.70710677, 0.0, 0.70710677))".to_owned();
    let scene = level(
        &[
            entity(
                1,
                None,
                &[
                    (TRANSFORM, yawed),
                    (
                        PRIORITY_VOLUME,
                        "(half_extents: (1.0, 1.0, 20.0), priority: 3)".to_owned(),
                    ),
                ],
            ),
            entity(2, None, &[(TRANSFORM, at(150.0, 0.0, 50.0))]),
        ],
        "",
    );
    let p = split(&scene);
    assert_eq!(p.cells[&cell(0, 0)].priority, 3);
    assert_eq!(p.cells[&cell(1, 0)].priority, 3, "the turned box crosses x = 100");
}

/// Audit asset_pipeline1 #3: the old loop inserted one map entry per covered cell, so a 2000 km
/// volume on the 8 m minimum grid (6.25e10 cells) hung the cook. Only cells that hold entities
/// read a priority now, so this partitions at once.
#[test]
fn a_priority_volume_over_billions_of_cells_partitions_without_a_cell_map() {
    let scene = level(
        &[
            entity(
                1,
                None,
                &[
                    (TRANSFORM, at(0.0, 0.0, 0.0)),
                    (
                        PRIORITY_VOLUME,
                        "(half_extents: (1000000.0, 1.0, 1000000.0), priority: 7)".to_owned(),
                    ),
                ],
            ),
            entity(2, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
            entity(3, None, &[(TRANSFORM, at(-900000.0, 0.0, 900000.0))]),
        ],
        "",
    );
    let mut settings = settings();
    settings.cell_size_m = 8.0;
    let p = partition(&scene, settings, &registry(), &()).expect("partition");
    assert_eq!(p.cells[&cell(6, 6)].priority, 7);
    assert_eq!(p.cells[&cell(-112500, 112500)].priority, 7);
    assert_eq!(p.cells[&cell(0, 0)].priority, 7, "the volume's own entity");
}

/// Audit asset_pipeline1 #3: a volume past the addressable cell range was skipped and the cook
/// still succeeded with its priorities unset. It is a cook error that names the entity now.
#[test]
fn a_priority_volume_past_the_cell_range_fails_the_partition() {
    let scene = level(
        &[entity(
            4,
            None,
            &[
                (TRANSFORM, at(50.0, 0.0, 50.0)),
                (
                    PRIORITY_VOLUME,
                    "(half_extents: (1e30, 1.0, 1.0), priority: 2)".to_owned(),
                ),
            ],
        )],
        "",
    );
    let error = partition(&scene, settings(), &registry(), &()).expect_err("out of range");
    let text = error.to_string();
    assert!(
        text.contains("e4") && text.contains("StreamPriorityVolume"),
        "{text}"
    );
}

/// Audit asset_pipeline2 #1: 1e5 m half extents on an 8 m grid cover about 25 000 cells a side
/// (6.25e8 cells), and the old loop stamped one map entry per cell, so the cook hung and ran out
/// of memory. The lane first refused such a volume against a `1 << 22` cell budget; the merged
/// rectangle lookup (audit asset_pipeline1 #3, ADR-5668) needs no budget, so the cook finishes
/// and the volume's own cell reads its priority.
#[test]
fn a_priority_volume_larger_than_the_old_cell_budget_cooks_without_a_cell_map() {
    let scene = {
        let text = format!(
            "(format_version: 1, entities: [{}], resources: {{ {} }})",
            entity(
                1,
                None,
                &[
                    (TRANSFORM, at(0.0, 0.0, 0.0)),
                    (
                        "regia_scene::streaming::StreamPriorityVolume",
                        "(half_extents: (1.0e5, 1.0, 1.0e5), priority: 4)".to_owned(),
                    ),
                ],
            ),
            "\"regia_scene::streaming::LevelStreaming\": (enabled: true, cell_size_m: 8.0, \
             load_radius_m: 150.0, unload_radius_m: 200.0, hlod: None)"
        );
        SceneFile::from_ron_str(&text).expect("level")
    };
    let settings = level_streaming(&scene, &registry())
        .expect("settings")
        .expect("streamed");
    let p = partition(&scene, settings, &registry(), &()).expect("partition");
    assert_eq!(p.cells[&cell(0, 0)].priority, 4);
}

#[test]
fn a_child_priority_volume_stamps_the_cells_of_its_world_position() {
    // The parent stands at (160, 0, 50); the child's local translation is the origin, so the
    // volume covers the cell under the parent's position, not the cell of the local origin.
    // A root in cell (0, 0) makes that cell exist in the partition.
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
            entity(2, None, &[(TRANSFORM, at(160.0, 0.0, 50.0))]),
            entity(
                3,
                Some(2),
                &[
                    (TRANSFORM, at(0.0, 0.0, 0.0)),
                    (
                        "regia_scene::streaming::StreamPriorityVolume",
                        "(half_extents: (5.0, 1.0, 5.0), priority: 4)".to_owned(),
                    ),
                ],
            ),
        ],
        "",
    );
    let p = split(&scene);
    assert_eq!(p.cells[&cell(0, 0)].priority, 0);
    assert_eq!(p.cells[&cell(1, 0)].priority, 4);
}

#[test]
fn a_non_positive_or_non_finite_priority_volume_warns_and_covers_no_cells() {
    for extents in ["(-1.0, 1.0, 5.0)", "(NaN, 1.0, 5.0)", "(inf, 1.0, 5.0)"] {
        let scene = level(
            &[
                entity(1, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]),
                entity(
                    2,
                    None,
                    &[
                        (TRANSFORM, at(150.0, 0.0, 50.0)),
                        (
                            "regia_scene::streaming::StreamPriorityVolume",
                            format!("(half_extents: {extents}, priority: 4)"),
                        ),
                    ],
                ),
            ],
            "",
        );
        let p = split(&scene);
        assert_eq!(
            p.warnings.iter().map(|w| w.code).collect::<Vec<_>>(),
            vec!["stream_priority_volume_invalid"],
            "{extents}"
        );
        assert!(p.cells.values().all(|c| c.priority == 0), "{extents}");
    }
}

#[test]
fn cylinder_bounds_follow_their_long_axis() {
    const COLLIDER: &str = "regia_physics::components::Collider";
    // Y axis: the XZ size is the 2 m diameter, well inside the 200 m limit, and the Y range is
    // ± half_height (not half_height + radius on every axis).
    let y_up = level(
        &[entity(
            1,
            None,
            &[
                (TRANSFORM, at(50.0, 0.0, 50.0)),
                (
                    COLLIDER,
                    "(shape: Cylinder(half_height: 150.0, radius: 1.0, axis: Y))".to_owned(),
                ),
            ],
        )],
        "",
    );
    let p = split(&y_up);
    assert_eq!(guids(&p.cells[&cell(0, 0)]), vec![guid(1)]);
    assert!(p.warnings.iter().all(|w| w.code != "stream_large_entity"));
    assert_eq!(
        (p.cells[&cell(0, 0)].min_y, p.cells[&cell(0, 0)].max_y),
        (-150.0, 150.0)
    );
    // X axis: the X size is the 300 m length, over the limit; it warns and stays with the level.
    let x_axis = level(
        &[entity(
            2,
            None,
            &[
                (TRANSFORM, at(50.0, 0.0, 50.0)),
                (
                    COLLIDER,
                    "(shape: Cylinder(half_height: 150.0, radius: 1.0, axis: X))".to_owned(),
                ),
            ],
        )],
        "",
    );
    let p = split(&x_axis);
    assert_eq!(
        p.always.entities.iter().map(|e| e.guid).collect::<Vec<_>>(),
        vec![guid(2)]
    );
    assert_eq!(
        p.warnings.iter().map(|w| w.code).collect::<Vec<_>>(),
        vec!["stream_large_entity"]
    );
}

#[test]
fn a_reference_across_cells_warns_and_one_to_the_always_set_does_not() {
    let scene = level(
        &[
            entity(
                1,
                None,
                &[
                    (TRANSFORM, at(50.0, 0.0, 50.0)),
                    (
                        "my_game::Link",
                        format!("(to: \"{}\", keep: \"{}\")", guid(2), guid(3)),
                    ),
                ],
            ),
            entity(2, None, &[(TRANSFORM, at(150.0, 0.0, 50.0))]),
            entity(
                3,
                None,
                &[
                    (TRANSFORM, at(50.0, 0.0, 50.0)),
                    (
                        "regia_scene::streaming::StreamPlacement",
                        "AlwaysLoaded".to_owned(),
                    ),
                ],
            ),
        ],
        "",
    );
    let p = split(&scene);
    let codes: Vec<_> = p.warnings.iter().map(|w| w.code).collect();
    assert_eq!(codes, vec!["stream_cross_cell_ref"]);
}

#[test]
fn root_transforms_are_stored_relative_to_the_anchor() {
    // 50 km out: the anchor's cell center is subtracted, so the blob keeps millimeters.
    let scene = level(
        &[
            entity(1, None, &[(TRANSFORM, at(50_000.5, 2.0, -30_000.25))]),
            entity(2, Some(1), &[(TRANSFORM, at(1.5, 0.0, 0.0))]),
        ],
        "",
    );
    let p = split(&scene);
    let (coord, cell) = p.cells.iter().next().expect("cell");
    assert_eq!(*coord, super::tests::cell(500, -301));
    assert_eq!(cell.anchor, [49, 0, -29]);
    let types = registry();
    let mut codec = ReflectCodec::new(&types);
    let root = decode::<Transform>(
        &mut codec,
        &types,
        cell.entities[0].components[TRANSFORM].get_ron(),
    )
    .expect("root");
    assert_eq!(root.translation.x, 50_000.5 - 49.0 * 1024.0);
    assert_eq!(root.translation.y, 2.0);
    assert_eq!(root.translation.z, -30_000.25 + 29.0 * 1024.0);
    let child = decode::<Transform>(
        &mut codec,
        &types,
        cell.entities[1].components[TRANSFORM].get_ron(),
    )
    .expect("child");
    assert_eq!(child.translation.x, 1.5, "children stay relative to their parent");
}

#[test]
fn a_level_without_streaming_cooks_nothing() {
    let dir = unique_temp_dir("stream_cook_plain");
    let plain = SceneFile::from_ron_str("(format_version: 1, entities: [])").expect("scene");
    assert!(
        cook_streamed_level(lvl(), &plain, &registry(), &(), Some(&dir), "a.level.ron")
            .expect("cook")
            .is_none()
    );
    let off = SceneFile::from_ron_str(
        "(format_version: 1, resources: { \"regia_scene::streaming::LevelStreaming\": (enabled: false) })",
    )
    .expect("scene");
    assert!(
        cook_streamed_level(lvl(), &off, &registry(), &(), Some(&dir), "a.level.ron")
            .expect("cook")
            .is_none()
    );
    assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 0);
}

#[test]
fn invalid_settings_are_a_cook_error_naming_the_field() {
    let dir = unique_temp_dir("stream_cook_bad");
    let scene = SceneFile::from_ron_str(
        "(format_version: 1, resources: { \"regia_scene::streaming::LevelStreaming\": (enabled: true, \
         load_radius_m: 900.0, unload_radius_m: 100.0) })",
    )
    .expect("scene");
    let error =
        cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron").expect_err("bad");
    assert!(error.to_string().contains("stream_settings_invalid"), "{error}");
    assert!(error.to_string().contains("unload_radius_m"), "{error}");
}

fn world() -> Vec<String> {
    let mut records = Vec::new();
    for x in 0..4u32 {
        for z in 0..3u32 {
            let id = u128::from(1000 + x * 10 + z);
            records.push(entity(
                id,
                None,
                &[(
                    TRANSFORM,
                    at(x as f32 * 100.0 + 10.0, 0.0, z as f32 * 100.0 + 10.0),
                )],
            ));
        }
    }
    records
}

#[test]
fn the_cooked_world_reads_back_through_the_runtime_formats() {
    let dir = unique_temp_dir("stream_cook_read");
    let scene = level(&world(), "");
    let output = cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "maps/a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert_eq!(output.report.cells, 12);
    assert_eq!(output.report.entities_max, 1);
    assert_eq!(output.manifest.len(), 14);
    assert!(
        output
            .manifest
            .iter()
            .any(|m| m.path == "maps/a.level.ron#cells/3_2")
    );

    let bytes = std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    assert_eq!(index.cells.len(), 12);
    assert_eq!(index.bounds, [cell(0, 0), cell(3, 2)]);
    assert_eq!(index.content_hash, WorldIndex::hash_cells(&index.cells));
    for record in &index.cells {
        let file = std::fs::read(dir.join(cooked_file_name(record.blob))).expect("blob");
        assert_eq!(file.len() as u64, record.own_bytes);
        let blob = CellBlob::from_bytes(&file, record.blob, &record.blob_hash).expect("blob");
        assert_eq!(blob.coord, record.coord);
        assert_eq!(blob.scene.entities.len(), 1);
        assert!(matches!(
            blob.scene.entities[0].components[0].1,
            Payload::Postcard { .. }
        ));
    }
    let always = std::fs::read(dir.join(cooked_file_name(always_loaded_id(lvl())))).expect("always");
    let always = regia_world::format::always_loaded_from_bytes(&always, lvl()).expect("always");
    assert!(always.entities.is_empty());
    assert_eq!(always.resources.len(), 1, "the settings stay with the level");
}

#[test]
fn a_recook_of_an_untouched_level_is_byte_identical_and_writes_nothing() {
    let dir = unique_temp_dir("stream_cook_again");
    let scene = level(&world(), "");
    let read_all = |dir: &Path| -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).expect("file"),
                )
            })
            .collect()
    };
    let first = cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert_eq!(first.report.written, 12);
    let before = read_all(&dir);
    let again = cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert_eq!((again.report.written, again.report.unchanged), (0, 12));
    assert_eq!(read_all(&dir), before);
}

#[test]
fn one_moved_entity_rewrites_only_its_old_and_new_cells() {
    let dir = unique_temp_dir("stream_cook_moved");
    let mut records = world();
    let first = level(&records, "");
    cook_streamed_level(lvl(), &first, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    // Entity 1000 moves from cell (0, 0) into (1, 0), joining entity 1010.
    records[0] = entity(1000, None, &[(TRANSFORM, at(150.0, 0.0, 50.0))]);
    let moved = level(&records, "");
    let output = cook_streamed_level(lvl(), &moved, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert_eq!(output.report.cells, 11);
    assert_eq!(
        output.report.written, 1,
        "the old cell is empty now, the new one changed"
    );
}

#[test]
fn a_report_only_cook_touches_no_file() {
    let scene = level(&world(), "");
    let output = cook_streamed_level(lvl(), &scene, &registry(), &(), None, "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert_eq!(output.report.cells, 12);
    assert_eq!(output.report.written, 0);
}

#[test]
fn hlod_settings_build_a_tree_whose_leaves_switch_inside_the_load_radius() {
    let dir = unique_temp_dir("stream_cook_hlod");
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 400.0, unload_radius_m: 500.0) }})",
        world().concat()
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let output = cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert!(
        output
            .report
            .warnings
            .iter()
            .all(|w| w.code != "stream_hlod_switch_beyond_load"),
        "{:?}",
        output.report.warnings
    );
    let bytes = std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    let hlod = index.hlod.expect("hlod");
    let grid = index.settings.grid().expect("grid");
    let tree = hlod.tree(&grid).expect("tree");
    assert_eq!(tree.leaf_count(), 12);
    assert_eq!(hlod.node_proxies.len(), tree.nodes().len());
    for leaf in tree.nodes().iter().filter(|n| n.level == 0) {
        assert!(leaf.switch_distance <= 400.0, "{}", leaf.switch_distance);
    }
}

#[test]
fn every_cell_with_meshes_gets_a_proxy_and_parents_merge_their_children() {
    let dir = unique_temp_dir("stream_cook_proxy");
    let mut records = Vec::new();
    for x in 0..4u32 {
        for z in 0..4u32 {
            for i in 0..40u32 {
                records.push(entity(
                    u128::from(1000 + x * 1000 + z * 100 + i),
                    None,
                    &[
                        (
                            TRANSFORM,
                            at(
                                x as f32 * 100.0 + 5.0 + i as f32 * 2.0,
                                0.0,
                                z as f32 * 100.0 + 5.0,
                            ),
                        ),
                        (
                            "regia_render::runtime::MeshSource",
                            "(mesh: \"builtin:cube\", material: \"\")".to_owned(),
                        ),
                    ],
                ));
            }
        }
    }
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 400.0, unload_radius_m: 500.0) }})",
        records.concat()
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let output = cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let bytes = std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    let hlod = index.hlod.expect("hlod");
    let tree = hlod.tree(&index.settings.grid().expect("grid")).expect("tree");
    assert_eq!(output.report.proxies, tree.nodes().len());
    assert!(
        output
            .manifest
            .iter()
            .any(|m| m.path.starts_with("a.level.ron#hlod/"))
    );
    let mut triangles = vec![0usize; tree.nodes().len()];
    for (i, id) in hlod.node_proxies.iter().enumerate() {
        let id = id.expect("every node has content");
        let file = std::fs::read(dir.join(cooked_file_name(id))).expect("proxy");
        let mesh = regia_asset::load_mesh_bytes(&file).expect("load").mesh;
        triangles[i] = mesh.indices.len() / 3;
        assert!(triangles[i] > 0);
    }
    // A cell holds 40 cubes of 12 triangles: the proxy keeps the minimum of 64.
    let leaf = tree.nodes().iter().position(|n| n.level == 0).expect("leaf");
    assert!(triangles[leaf] <= 64 + 12, "{}", triangles[leaf]);
    assert!(triangles[leaf] < 480);
    // Vertices are relative to the anchor: a proxy of the far corner stays near zero at its anchor.
    let far = tree.leaf_for(cell(3, 3)).expect("leaf") as usize;
    let file = std::fs::read(dir.join(cooked_file_name(hlod.node_proxies[far].expect("id")))).expect("proxy");
    let mesh = regia_asset::load_mesh_bytes(&file).expect("load").mesh;
    let anchor = hlod.node_anchors[far];
    let world_x = anchor[0] as f64 * 1024.0 + f64::from(mesh.positions[0][0]);
    assert!((300.0..400.0).contains(&world_x), "{world_x}");
}

#[test]
fn mesh_bounds_decide_the_cell_for_a_builtin_mesh() {
    // A 60 m wide cube whose position is in cell 0 and whose center stays there: the Y range sees it.
    let scene = level(
        &[entity(
            1,
            None,
            &[
                (
                    TRANSFORM,
                    "(translation: (40.0, 0.0, 50.0), scale: (60.0, 10.0, 1.0))".to_owned(),
                ),
                (
                    "regia_render::runtime::MeshSource",
                    "(mesh: \"builtin:cube\", material: \"\")".to_owned(),
                ),
            ],
        )],
        "",
    );
    let p = split(&scene);
    let c = &p.cells[&cell(0, 0)];
    assert_eq!((c.min_y, c.max_y), (-5.0, 5.0));
    assert_eq!(c.meshes.len(), 1);
}

#[test]
fn an_authored_threshold_that_switches_too_far_warns() {
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 150.0, unload_radius_m: 200.0, hlod: Some((screen_size_threshold: \
         Some(0.01)))) }})",
        world().concat()
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    let output = cook_streamed_level(lvl(), &scene, &registry(), &(), None, "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert!(
        output
            .report
            .warnings
            .iter()
            .any(|w| w.code == "stream_hlod_switch_beyond_load"),
        "{:?}",
        output.report.warnings
    );
}

#[test]
fn levels_with_settings_are_discovered_in_both_forms() {
    let dir = unique_temp_dir("stream_cook_discover");
    let assets = dir.join("assets");
    std::fs::create_dir_all(assets.join("maps/big.level.d")).expect("dirs");
    std::fs::write(assets.join("maps/plain.level.ron"), "(format_version: 1)").expect("write");
    std::fs::write(
        assets.join("maps/open.level.ron"),
        "(format_version: 1, resources: { \"regia_scene::streaming::LevelStreaming\": () })",
    )
    .expect("write");
    std::fs::write(
        assets.join("maps/big.level.d/level.ron"),
        "LevelDirFile(resources: { \"regia_scene::streaming::LevelStreaming\": () })",
    )
    .expect("write");
    assert_eq!(
        discover_streamed_levels(&assets),
        vec![
            assets.join("maps/big.level.d"),
            assets.join("maps/open.level.ron")
        ]
    );
}

/// ADR-8861 (review w7 finding 4): the staged copy of a level directory that an interrupted
/// `regia upgrade-project --write` left (`.<name>.<pid>.stage/<name>.level.d`) is not a second
/// level for the cook: the streamed-level scan and the source scan both skip it.
#[test]
fn a_stale_upgrade_stage_copy_is_not_a_second_level() {
    let dir = unique_temp_dir("stream_cook_stale_stage");
    let assets = dir.join("assets");
    let streaming = "LevelDirFile(resources: { \"regia_scene::streaming::LevelStreaming\": () })";
    for level in [
        assets.join("maps/big.level.d"),
        assets.join("maps/.big.level.d.4242.stage/big.level.d"),
    ] {
        std::fs::create_dir_all(&level).expect("dirs");
        std::fs::write(level.join("level.ron"), streaming).expect("write");
    }
    assert_eq!(
        discover_streamed_levels(&assets),
        vec![assets.join("maps/big.level.d")],
        "exactly one level"
    );
    let sources = crate::db::AssetDatabase::discover_sources(&assets);
    assert_eq!(
        sources,
        vec![assets.join("maps/big.level.d/level.ron")],
        "no source inside the staging directory"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_level_with_a_heightfield_marks_its_cells_as_waiting_for_terrain() {
    let dir = unique_temp_dir("stream_cook_terrain");
    let terrain = entity(
        9,
        None,
        &[
            (TRANSFORM, at(500.0, 0.0, 500.0)),
            (
                "regia_physics::components::Collider",
                "(shape: HeightField(nx: 2, nz: 2, heights: [0.0, 0.0, 0.0, 0.0], scale: (1000.0, 1.0, 1000.0)))"
                    .to_owned(),
            ),
        ],
    );
    let prop = entity(1, None, &[(TRANSFORM, at(50.0, 0.0, 50.0))]);
    let scene = level(&[terrain, prop], "");
    cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let bytes = std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    assert!(index.cells.iter().all(|c| c.has_terrain_collision));
}

#[test]
fn a_mesh_source_path_feeds_the_cell_dependencies_and_their_costs_are_shared() {
    struct Library;
    impl StreamAssets for Library {
        fn cost(&self, id: AssetId) -> Option<AssetCost> {
            (id.0 == guid(0xfeed)).then_some(AssetCost {
                ram_bytes: 900,
                vram_bytes: 300,
                bounds: None,
            })
        }
        fn assets_of(&self, path: &str) -> Vec<AssetId> {
            if path == "assets/props/crate.gltf" {
                vec![AssetId(guid(0xfeed))]
            } else {
                Vec::new()
            }
        }
    }
    let prop = |id: u128, x: f32| {
        entity(
            id,
            None,
            &[
                (TRANSFORM, at(x, 0.0, 50.0)),
                (
                    "regia_render::runtime::MeshSource",
                    "(mesh: \"assets/props/crate.gltf\", material: \"\")".to_owned(),
                ),
            ],
        )
    };
    let dir = unique_temp_dir("stream_cook_deps");
    let scene = level(&[prop(1, 50.0), prop(2, 150.0), prop(3, 250.0)], "");
    cook_streamed_level(lvl(), &scene, &registry(), &Library, Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let bytes = std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    for record in &index.cells {
        assert_eq!(record.deps, vec![AssetId(guid(0xfeed))]);
        assert_eq!((record.dep_bytes, record.vram_bytes), (300, 100));
    }
}

/// ADR-4993: a built-in primitive names no asset, but the renderer still uploads its mesh. The cell's
/// VRAM estimate counts each distinct shape's vertex and index bytes once, shared by the cells that
/// draw it, so `vram_budget_bytes` and `vram_over_budget` see a world built from primitives.
#[test]
fn built_in_primitives_count_in_the_cell_vram_estimate_once_per_shape() {
    const PRIMITIVE: &str = "regia_render::primitives::Primitive";
    let cuboid = |size: f32, tint: f32| {
        format!(
            "(format_version: 1, shape: Cuboid(size: ({size}, {size}, {size})), color: ({tint}, 0.5, 0.5, 1.0), \
             roughness: 0.5, metallic: 0.0)"
        )
    };
    let prop = |id: u128, x: f32, primitive: String| {
        entity(id, None, &[(TRANSFORM, at(x, 0.0, 50.0)), (PRIMITIVE, primitive)])
    };
    let scene = level(
        &[
            // Cell (0, 0): one shape twice (another tint), and a shape no other cell draws.
            prop(1, 20.0, cuboid(1.0, 0.2)),
            prop(2, 40.0, cuboid(1.0, 0.8)),
            prop(3, 60.0, cuboid(3.0, 0.2)),
            // Cell (1, 0): the shared shape only.
            prop(4, 150.0, cuboid(1.0, 0.2)),
            // Cell (2, 0): no primitive.
            entity(5, None, &[(TRANSFORM, at(250.0, 0.0, 50.0))]),
        ],
        "",
    );
    let dir = unique_temp_dir("stream_cook_primitive_vram");
    cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let bytes = std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    let vram = |x: i64| {
        index
            .cells
            .iter()
            .find(|c| c.coord == cell(x, 0))
            .map(|c| c.vram_bytes)
            .expect("cell")
    };
    let mesh = regia_render::primitives::PrimitiveShape::Cuboid {
        size: Vec3::splat(1.0),
    }
    .mesh()
    .expect("cuboid");
    let cube = (mesh.vertices.len() * std::mem::size_of::<regia_render::LabVertex>()
        + mesh.indices.len() * std::mem::size_of::<u32>()) as u64;
    assert!(cube > 0);
    assert_eq!(
        vram(1),
        cube / 2,
        "the shared shape is split between its two cells"
    );
    assert_eq!(
        vram(0),
        cube / 2 + cube,
        "the shared shape's half and the cell's own shape"
    );
    assert_eq!(vram(2), 0);
    assert!(
        index.cells.iter().all(|c| c.deps.is_empty()),
        "no asset dependency is made up"
    );
}

#[test]
fn guids_are_found_in_component_text() {
    let a = guid(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    let text = format!("(mesh: \"{a}\", other: 12, tail: \"not-a-guid\")");
    assert_eq!(guids_in(&text), vec![a]);
    assert!(guids_in("").is_empty());
}

/// ADR-2022: a proxy's flat color is the area-weighted mean of the `Primitive` colors it merges;
/// a cell of `MeshSource` meshes, whose materials the cook does not read, has none; a parent mixes
/// its children by area.
#[test]
fn proxies_carry_the_area_weighted_color_of_their_primitives() {
    const PRIMITIVE: &str = "regia_render::primitives::Primitive";
    let dir = unique_temp_dir("stream_cook_proxy_color");
    let mut records = Vec::new();
    for i in 0..4u32 {
        // Cell (0, 0): red cubes. Cell (1, 0): blue cubes. Cell (2, 0): mesh references only.
        let x = 10.0 + i as f32 * 5.0;
        records.push(entity(
            u128::from(100 + i),
            None,
            &[
                (TRANSFORM, at(x, 0.0, 10.0)),
                (
                    PRIMITIVE,
                    "(format_version: 1, shape: Cuboid(size: (2.0, 2.0, 2.0)), color: (1.0, 0.0, 0.0, 1.0), \
                     roughness: 0.5, metallic: 0.0)"
                        .to_owned(),
                ),
            ],
        ));
        records.push(entity(
            u128::from(200 + i),
            None,
            &[
                (TRANSFORM, at(110.0 + i as f32 * 5.0, 0.0, 10.0)),
                (
                    PRIMITIVE,
                    "(format_version: 1, shape: Cuboid(size: (2.0, 2.0, 2.0)), color: (0.0, 0.0, 1.0, 1.0), \
                     roughness: 0.5, metallic: 0.0)"
                        .to_owned(),
                ),
            ],
        ));
        records.push(entity(
            u128::from(300 + i),
            None,
            &[
                (TRANSFORM, at(210.0 + i as f32 * 5.0, 0.0, 10.0)),
                (
                    "regia_render::runtime::MeshSource",
                    "(mesh: \"builtin:cube\", material: \"\")".to_owned(),
                ),
            ],
        ));
    }
    let text = format!(
        "(format_version: 1, entities: [{}], resources: {{ \"regia_scene::streaming::LevelStreaming\": (enabled: \
         true, cell_size_m: 100.0, load_radius_m: 400.0, unload_radius_m: 500.0) }})",
        records.concat()
    );
    let scene = SceneFile::from_ron_str(&text).expect("scene");
    cook_streamed_level(lvl(), &scene, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let bytes = std::fs::read(dir.join(cooked_file_name(world_index_id(lvl())))).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    let hlod = index.hlod.expect("hlod");
    let tree = hlod.tree(&index.settings.grid().expect("grid")).expect("tree");
    assert_eq!(hlod.node_colors.len(), tree.nodes().len());
    let color_of = |x: i64| hlod.node_colors[tree.leaf_for(cell(x, 0)).expect("leaf") as usize];
    assert_eq!(color_of(0), Some([1.0, 0.0, 0.0, 1.0]));
    assert_eq!(color_of(1), Some([0.0, 0.0, 1.0, 1.0]));
    assert_eq!(color_of(2), None, "no source named a color");
    // A parent over red and blue (and perhaps the uncolored cell) holds a mix, never a source color.
    for (index, node) in tree.nodes().iter().enumerate() {
        if node.level == 0 {
            continue;
        }
        let color = hlod.node_colors[index].expect("a parent of colored children has a color");
        assert!(color[0] > 0.0 && color[2] > 0.0, "{color:?}");
        assert!((color[3] - 1.0).abs() < 1e-6);
    }
}

/// The nav cook paints the level's `NavAreaZone`s into the cooked cells (ADR-2133): a streamed
/// cache loading them carries the zone's area id under the zone and plain ground elsewhere.
#[test]
fn nav_area_zones_are_painted_into_the_cooked_tiles() {
    const ZONE: &str = "regia_framework::nav_world::NavAreaZone";
    let floor = entity(
        1,
        None,
        &[
            (TRANSFORM, at(0.0, -0.5, 0.0)),
            (
                "regia_physics::components::RigidBody",
                "(kind: Static)".to_owned(),
            ),
            (
                "regia_physics::components::Collider",
                "(shape: Box(half_extents: (12.0, 0.5, 12.0)))".to_owned(),
            ),
        ],
    );
    // A zone under a moved parent: it paints at its world pose.
    let group = entity(2, None, &[(TRANSFORM, at(4.0, 0.0, 0.0))]);
    let zone = entity(
        3,
        Some(2),
        &[
            (TRANSFORM, at(1.0, 0.0, 5.0)),
            (ZONE, "(half_extents: (2.0, 2.0, 2.0), area: 7)".to_owned()),
        ],
    );
    let scene = level(&[floor.clone(), group.clone(), zone], "");
    let volumes = nav_area_volumes(&scene, &registry());
    assert_eq!(volumes.len(), 1);
    assert_eq!(volumes[0].area, 7);

    let cook = |scene: &SceneFile| {
        let mut settings = settings();
        settings.nav = Some(regia_scene::streaming::StreamNavCook {
            cell_size: 0.25,
            ..regia_scene::streaming::StreamNavCook::default()
        });
        let mut split = partition(scene, settings, &registry(), &()).expect("partition");
        let grid = split.settings.grid().expect("grid");
        cook_nav_tiles(scene, &mut split, &grid, &registry(), &[])
            .expect("cooks")
            .expect("nav tiles")
    };
    let painted = cook(&scene);
    let plain = cook(&level(&[floor, group], ""));
    assert_eq!(painted.layout, plain.layout, "zones do not move the grid");
    assert_ne!(
        painted.cells.values().map(|c| &c.0).collect::<Vec<_>>(),
        plain.cells.values().map(|c| &c.0).collect::<Vec<_>>(),
        "the painted ids are in the cooked bytes"
    );

    let layout = painted.layout;
    let cook_settings = layout.cook;
    let params = regia_nav::NavTileCacheParams {
        build: regia_nav::NavMeshBuildParams {
            cell_size: cook_settings.cell_size,
            cell_height: cook_settings.cell_height,
            border_size: cook_settings.border_size,
            agent: regia_nav::NavAgentParams {
                radius: cook_settings.agent_radius,
                height: cook_settings.agent_height,
                max_climb: cook_settings.agent_max_climb,
                max_slope: cook_settings.agent_max_slope,
            },
        },
        tile_size: cook_settings.tile_size,
    };
    let mut cache = regia_nav::NavTileCache::streamed(
        params,
        regia_nav::NavTileLayout {
            origin: Vec3::from_array(layout.origin),
            width: layout.width as usize,
            height: layout.height as usize,
        },
    )
    .expect("streamed");
    for (i, (bytes, _)) in painted.cells.values().enumerate() {
        cache.load_cell(i as u64, bytes).expect("loads");
    }
    cache.update();
    let area_at = |p: Vec3| {
        let (poly, _) = cache
            .query()
            .find_nearest(p, Vec3::splat(1.0))
            .expect("on the mesh");
        cache.mesh().polys[poly as usize].area
    };
    assert_eq!(area_at(Vec3::new(5.0, 0.0, 5.0)), 7, "under the zone");
    assert_eq!(area_at(Vec3::new(-6.0, 0.0, -6.0)), 0, "plain ground");
}

/// A level's `NavAreaCostTable` is a reflected resource the cook registry reads.
#[test]
fn the_nav_area_cost_table_is_a_level_resource() {
    let text = "(format_version: 1, entities: [], resources: { \
                \"regia_framework::nav_world::NavAreaCostTable\": (areas: [(area: 3, cost: 4.0, name: \"mud\")]) })";
    let scene = SceneFile::from_ron_str(text).expect("level");
    let registry = registry();
    let mut codec = ReflectCodec::new(&registry);
    let raw = scene
        .resources
        .get("regia_framework::nav_world::NavAreaCostTable")
        .expect("stored");
    let table: regia_framework::nav_world::NavAreaCostTable =
        decode(&mut codec, &registry, raw.get_ron()).expect("decodes");
    assert_eq!(table.areas[0].name, "mud");
    assert_eq!(table.costs().expect("valid").get(3), 4.0);
}

#[test]
fn nav_heightfield_cells_use_the_collider_diagonal() {
    // A saddle: samples (0,0) and (1,1) high, (1,0) and (0,1) low. The collider splits a cell
    // along p10-p01 (the two low samples); the nav triangles must share that diagonal.
    let collider = Collider {
        shape: ColliderShape::HeightField {
            nx: 2,
            nz: 2,
            heights: vec![1.0, 0.0, 0.0, 1.0], // heights[i + j * nx]
            scale: Vec3::ONE,
        },
        ..Collider::default()
    };
    let mut out = Vec::new();
    let far = Vec3::splat(f32::MAX);
    regia_framework::nav_world::collider_triangles_at(&collider, Affine3A::IDENTITY, -far, far, &mut out);
    assert_eq!(out.len(), 2);
    for tri in &out {
        let low = tri.iter().filter(|v| v.y == 0.0).count();
        assert_eq!(low, 2, "each triangle holds both low corners: {tri:?}");
        assert!(tri.iter().any(|v| v.y == 1.0));
    }
}

#[test]
fn failed_rename_leaves_no_temp_file() {
    let dir = unique_temp_dir("stream_cook_tmp");
    let target = dir.join("cell.bin");
    std::fs::create_dir(&target).expect("dir at the destination");
    assert!(write_if_changed(&target, b"bytes").is_err());
    let names: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("cell.bin")], "{names:?}");
}

/// A flat terrain of `n` × `n` samples, `width` meters across, centered on the origin, at `y`.
fn flat_terrain(n: u32, width: f32, y: f32) -> NavTerrain {
    NavTerrain {
        map: regia_terrain::TerrainMap::from_heights(n, n, vec![y; (n * n) as usize], width, 1.0, 0.0)
            .expect("a map"),
        holes: None,
    }
}

fn nav_settings() -> LevelStreaming {
    let mut settings = settings();
    settings.nav = Some(regia_scene::streaming::StreamNavCook {
        cell_size: 0.5,
        tile_size: 16,
        ..regia_scene::streaming::StreamNavCook::default()
    });
    settings
}

/// The streamed cache of a cooked nav grid with every cooked cell loaded.
fn loaded_cache(nav: &CookedNav) -> regia_nav::NavTileCache {
    let cook = nav.layout.cook;
    let params = regia_nav::NavTileCacheParams {
        build: regia_nav::NavMeshBuildParams {
            cell_size: cook.cell_size,
            cell_height: cook.cell_height,
            border_size: cook.border_size,
            agent: regia_nav::NavAgentParams {
                radius: cook.agent_radius,
                height: cook.agent_height,
                max_climb: cook.agent_max_climb,
                max_slope: cook.agent_max_slope,
            },
        },
        tile_size: cook.tile_size,
    };
    let mut cache = regia_nav::NavTileCache::streamed(
        params,
        regia_nav::NavTileLayout {
            origin: Vec3::from_array(nav.layout.origin),
            width: nav.layout.width as usize,
            height: nav.layout.height as usize,
        },
    )
    .expect("streamed");
    for (i, (bytes, _)) in nav.cells.values().enumerate() {
        cache.load_cell(i as u64, bytes).expect("loads");
    }
    cache.update();
    cache
}

/// Spec 78 § 13.2: a level whose ground is a `Terrain` (no collider in the level file) gets walkable
/// nav tiles over it when the cook's caller names the terrain; cells with only terrain under them
/// join the world for their tiles.
#[test]
fn a_terrain_only_level_cooks_walkable_nav_tiles_over_the_terrain() {
    let scene = level(&[], "");
    let grid = |split: &Partition| split.settings.grid().expect("grid");

    let mut bare = partition(&scene, nav_settings(), &registry(), &()).expect("partition");
    let g = grid(&bare);
    assert!(
        cook_nav_tiles(&scene, &mut bare, &g, &registry(), &[])
            .expect("cooks")
            .is_none()
    );
    assert!(bare.warnings.iter().any(|w| w.code == "stream_nav_no_geometry"));

    let terrain = [flat_terrain(33, 64.0, 2.0)];
    let mut split = partition(&scene, nav_settings(), &registry(), &()).expect("partition");
    let g = grid(&split);
    let nav = cook_nav_tiles(&scene, &mut split, &g, &registry(), &terrain)
        .expect("cooks")
        .expect("nav tiles over the terrain");
    assert!(split.warnings.iter().all(|w| w.code != "stream_nav_no_geometry"));
    // The 64 m terrain around the origin straddles the four 100 m cells there.
    let cells: Vec<CellCoord> = nav.cells.keys().copied().collect();
    assert_eq!(cells, vec![cell(-1, -1), cell(-1, 0), cell(0, -1), cell(0, 0)]);
    assert!(
        cells.iter().all(|c| split.cells.contains_key(c)),
        "terrain cells join the world"
    );

    let cache = loaded_cache(&nav);
    for p in [
        Vec3::new(-20.0, 2.0, -20.0),
        Vec3::new(20.0, 2.0, 20.0),
        Vec3::ZERO,
    ] {
        let (_, on) = cache
            .query()
            .find_nearest(p, Vec3::new(1.0, 4.0, 1.0))
            .expect("walkable over the terrain");
        assert!((on.y - 2.0).abs() < 0.5, "on the terrain's surface: {on}");
    }
    let path = cache
        .query()
        .find_path(
            Vec3::new(-20.0, 2.0, -20.0),
            Vec3::new(20.0, 2.0, 20.0),
            Vec3::new(1.0, 4.0, 1.0),
        )
        .expect("a path across the terrain");
    assert!(path.waypoints.len() >= 2);

    // The hook goes through `StreamAssets`: the wrapper hands the cook the same terrain.
    let with = WithNavTerrain {
        assets: &(),
        terrains: terrain.to_vec(),
    };
    assert_eq!(with.nav_terrains().len(), 1);
    let mut again = partition(&scene, nav_settings(), &registry(), &with).expect("partition");
    let g = grid(&again);
    let through = cook_nav_tiles(&scene, &mut again, &g, &registry(), with.nav_terrains())
        .expect("cooks")
        .expect("tiles");
    assert_eq!(
        through.cells, nav.cells,
        "deterministic and identical through the wrapper"
    );
}

/// The cook's terrain triangles are the run-time nav geometry's (`level_triangles_in` over a
/// `Terrain`), holes cut: a region rebuild over a cooked tile keeps what the tile held.
#[test]
fn the_cooks_terrain_triangles_are_the_runtimes() {
    let n = 9;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| (i % n) as f32 * 0.25 + (i / n) as f32 * 0.5)
        .collect();
    let holes = regia_terrain::HolesMask::new(vec![regia_terrain::HoleEllipse {
        center: [0.0, 0.0],
        radius: [1.5, 1.5],
        rotation_radians: 0.0,
    }])
    .expect("a hole");
    let terrain = NavTerrain {
        map: regia_terrain::TerrainMap::from_heights(n, n, heights, 16.0, 1.0, 0.0).expect("a map"),
        holes: Some(holes),
    };
    let mut cooked = Vec::new();
    terrain.triangles(&mut cooked);
    let mut runtime = Vec::new();
    regia_framework::nav_world::terrain_triangles_in(
        &terrain.map,
        terrain.holes.as_ref(),
        Vec3::splat(-1.0e6),
        Vec3::splat(1.0e6),
        &mut runtime,
    );
    assert_eq!(cooked, runtime);
    assert!(cooked.len() < 2 * 8 * 8, "the hole is cut");
    assert!(!cooked.is_empty());
}

/// Gap row "Streaming runtime remainder" (5): a static collider under a moved, turned parent lands
/// at the same place in the cook and in the run-time nav geometry, and so does a heightfield.
#[test]
fn cook_and_runtime_nav_geometry_agree_on_nested_static_bodies() {
    const COLLIDER: &str = "regia_physics::components::Collider";
    const BODY: &str = "regia_physics::components::RigidBody";
    let turned = "(translation: (100.0, 2.0, -30.0), rotation: (0.0, 0.38268343, 0.0, 0.9238795))".to_owned();
    let child = "(translation: (10.0, 0.5, 4.0), scale: (2.0, 1.0, 1.0))".to_owned();
    let block =
        "(shape: Box(half_extents: (1.0, 0.5, 3.0)), offset: (translation: (0.0, 1.0, 0.0)))".to_owned();
    let ground = "(shape: HeightField(nx: 3, nz: 4, heights: [0.0, 0.5, 0.0, 1.0, 0.2, 0.0, 0.0, 0.7, 0.1, 0.3, 0.0, 0.4], \
                  scale: (24.0, 2.0, 16.0)))"
        .to_owned();
    let records = [
        entity(1, None, &[(TRANSFORM, turned.clone())]),
        entity(
            2,
            Some(1),
            &[
                (TRANSFORM, child.clone()),
                (BODY, "(kind: Static)".to_owned()),
                (COLLIDER, block.clone()),
            ],
        ),
        entity(
            3,
            Some(1),
            &[
                (TRANSFORM, at(-5.0, 0.0, 0.0)),
                (BODY, "(kind: Static)".to_owned()),
                (COLLIDER, ground.clone()),
            ],
        ),
    ];
    let scene = level(&records, "");
    let cooked = static_collider_triangles(&scene, &registry());
    assert_eq!(cooked.len(), 12 + 2 * 2 * 3, "a box and a 3 × 4 heightfield");

    // The same entities in a world, decoded from the same text.
    let registry = registry();
    let mut codec = ReflectCodec::new(&registry);
    let mut world = World::new();
    let pose: Transform = decode(&mut codec, &registry, &turned).expect("transform");
    let parent = world.spawn(pose).id();
    let body: RigidBody = decode(&mut codec, &registry, "(kind: Static)").expect("body");
    let block: Collider = decode(&mut codec, &registry, &block).expect("box");
    let ground: Collider = decode(&mut codec, &registry, &ground).expect("ground");
    let child: Transform = decode(&mut codec, &registry, &child).expect("transform");
    let shifted: Transform = decode(&mut codec, &registry, &at(-5.0, 0.0, 0.0)).expect("transform");
    world.spawn((child, body.clone(), block, ChildOf(parent)));
    world.spawn((shifted, body, ground, ChildOf(parent)));
    let mut runtime = Vec::new();
    regia_framework::nav_world::level_triangles_in(
        &world,
        Vec3::splat(-1.0e6),
        Vec3::splat(1.0e6),
        &mut runtime,
    );
    let key = |tris: &[[Vec3; 3]]| {
        let mut flat: Vec<[u32; 9]> = tris
            .iter()
            .map(|t| std::array::from_fn(|k| t[k / 3].to_array()[k % 3].to_bits()))
            .collect();
        flat.sort_unstable();
        flat
    };
    assert_eq!(key(&runtime), key(&cooked), "the same triangles, bit for bit");
    // And not where the child's local transform alone would put the box.
    let max_x = cooked.iter().flatten().map(|v| v.x).fold(f32::MIN, f32::max);
    assert!(max_x > 90.0, "{max_x}");
}

/// A terrain entity with a tiled collider of `tile_m` (spec 78 § 13.1).
fn tiled_terrain(tile_m: f32) -> String {
    entity(
        9,
        None,
        &[
            (TRANSFORM, at(500.0, 0.0, 500.0)),
            (
                "regia_physics::components::Collider",
                "(shape: HeightField(nx: 2, nz: 2, heights: [0.0, 0.0, 0.0, 0.0], scale: (1000.0, 1.0, 1000.0)))"
                    .to_owned(),
            ),
            (
                "regia_scene::streaming::TerrainTileCollision",
                format!("(tile_m: {tile_m:?}, view_radius_m: 100.0, max_tiles_per_step: 4)"),
            ),
        ],
    )
}

#[test]
fn terrain_tiles_that_do_not_divide_the_cells_warn() {
    // Cells are 100 m: 50 m and 200 m tiles line up, 75 m tiles do not.
    let codes = |tile_m: f32| -> Vec<&'static str> {
        let scene = level(&[tiled_terrain(tile_m)], "");
        let split = split(&scene);
        link_warnings(&split.always, &split.settings, &registry(), &())
            .iter()
            .map(|w| w.code)
            .collect()
    };
    assert!(codes(50.0).is_empty());
    assert!(codes(200.0).is_empty());
    assert_eq!(codes(75.0), vec!["stream_terrain_misaligned"]);
    // The cook's report carries it.
    let scene = level(&[tiled_terrain(75.0)], "");
    let out = cook_streamed_level(lvl(), &scene, &registry(), &(), None, "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert!(
        out.report
            .warnings
            .iter()
            .any(|w| w.code == "stream_terrain_misaligned")
    );
}

#[test]
fn a_relevance_reach_beyond_the_load_radius_warns() {
    let scene = level(&[], "");
    let split = split(&scene);
    let codes = |radius_m: f64| -> Vec<&'static str> {
        let assets = WithNetRelevance {
            assets: &(),
            radius_m,
        };
        link_warnings(&split.always, &split.settings, &registry(), &assets)
            .iter()
            .map(|w| w.code)
            .collect()
    };
    // load_radius_m is 150.
    assert!(codes(150.0).is_empty());
    assert_eq!(codes(150.5), vec!["stream_relevance_radius"]);
    // Without a reach there is nothing to check.
    assert!(link_warnings(&split.always, &split.settings, &registry(), &()).is_empty());
}

/// A static box collider entity.
fn static_box(id: u128, at_: String, half: &str, extra: &[(&str, String)]) -> String {
    let mut components = vec![
        (TRANSFORM, at_),
        (
            "regia_physics::components::RigidBody",
            "(kind: Static)".to_owned(),
        ),
        (
            "regia_physics::components::Collider",
            format!("(shape: Box(half_extents: {half}))"),
        ),
    ];
    components.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
    entity(id, None, &components)
}

fn cook_nav_cells(scene: &SceneFile) -> Result<CookedNav, StreamCookError> {
    let mut settings = settings();
    settings.nav = Some(regia_scene::streaming::StreamNavCook {
        cell_size: 0.25,
        ..regia_scene::streaming::StreamNavCook::default()
    });
    let mut split = partition(scene, settings, &registry(), &()).expect("partition");
    let grid = split.settings.grid().expect("grid");
    cook_nav_tiles(scene, &mut split, &grid, &registry(), &[]).map(|nav| nav.expect("nav tiles"))
}

/// The streamed cache a client builds from a cook's cells.
fn load_cooked_cache(cooked: &CookedNav) -> regia_nav::NavTileCache {
    let cook = cooked.layout.cook;
    let params = regia_nav::NavTileCacheParams {
        build: regia_nav::NavMeshBuildParams {
            cell_size: cook.cell_size,
            cell_height: cook.cell_height,
            border_size: cook.border_size,
            agent: regia_nav::NavAgentParams {
                radius: cook.agent_radius,
                height: cook.agent_height,
                max_climb: cook.agent_max_climb,
                max_slope: cook.agent_max_slope,
            },
        },
        tile_size: cook.tile_size,
    };
    let mut cache = regia_nav::NavTileCache::streamed(
        params,
        regia_nav::NavTileLayout {
            origin: Vec3::from_array(cooked.layout.origin),
            width: cooked.layout.width as usize,
            height: cooked.layout.height as usize,
        },
    )
    .expect("streamed");
    for (i, (bytes, _)) in cooked.cells.values().enumerate() {
        cache.load_cell(i as u64, bytes).expect("loads");
    }
    cache.update();
    cache
}

fn nav_area_at(cache: &regia_nav::NavTileCache, p: Vec3) -> u8 {
    let (poly, _) = cache
        .query()
        .find_nearest(p, Vec3::splat(1.0))
        .expect("on the mesh");
    cache.mesh().polys[poly as usize].area
}

/// A collider's `NavSurface` paints its own triangles (ADR-2788): the strip's ground carries the id and the rest
/// stays plain, and a cook of the same level twice is byte-identical.
#[test]
fn a_nav_surface_paints_its_colliders_triangles_into_the_cooked_tiles() {
    const SURFACE: &str = "regia_framework::nav_world::NavSurface";
    let floor = static_box(1, at(0.0, -0.5, 0.0), "(12.0, 0.5, 12.0)", &[]);
    // A raised strip on the floor: its top face is the walkable surface and carries area 5.
    let strip = static_box(
        2,
        at(5.0, 0.05, 0.0),
        "(2.0, 0.1, 12.0)",
        &[(SURFACE, "(area: 5)".to_owned())],
    );
    let scene = level(&[floor, strip], "");
    let (triangles, areas) = static_collider_surface(&scene, &registry());
    assert_eq!(triangles.len(), areas.len());
    assert!(areas.contains(&5) && areas.contains(&0));
    assert_eq!(
        areas.iter().filter(|&&a| a == 5).count(),
        12,
        "a box has 12 triangles"
    );

    let cooked = cook_nav_cells(&scene).expect("cooks");
    let cache = load_cooked_cache(&cooked);
    assert_eq!(nav_area_at(&cache, Vec3::new(5.0, 0.2, 0.0)), 5, "on the strip");
    assert_eq!(nav_area_at(&cache, Vec3::new(-6.0, 0.0, 0.0)), 0, "plain ground");
    let again = cook_nav_cells(&scene).expect("cooks");
    assert_eq!(cooked.cells, again.cells, "cook twice is byte-identical");
}

/// A level with no painted surface cooks the bytes the plain build makes: an absent component, a zero
/// area and the reference `NavTileCache::build` agree.
#[test]
fn an_unpainted_nav_cook_is_byte_identical_to_the_plain_build() {
    const SURFACE: &str = "regia_framework::nav_world::NavSurface";
    let bare = level(&[static_box(1, at(0.0, -0.5, 0.0), "(12.0, 0.5, 12.0)", &[])], "");
    let zero = level(
        &[static_box(
            1,
            at(0.0, -0.5, 0.0),
            "(12.0, 0.5, 12.0)",
            &[(SURFACE, "(area: 0)".to_owned())],
        )],
        "",
    );
    let (plain, with_zero) = (
        cook_nav_cells(&bare).expect("bare"),
        cook_nav_cells(&zero).expect("zero"),
    );
    assert_eq!(plain.cells, with_zero.cells);

    // The pre-change path: `NavTileCache::build` over the same triangles.
    let cook = plain.layout.cook;
    let params = regia_nav::NavTileCacheParams {
        build: regia_nav::NavMeshBuildParams {
            cell_size: cook.cell_size,
            cell_height: cook.cell_height,
            border_size: cook.border_size,
            agent: regia_nav::NavAgentParams {
                radius: cook.agent_radius,
                height: cook.agent_height,
                max_climb: cook.agent_max_climb,
                max_slope: cook.agent_max_slope,
            },
        },
        tile_size: cook.tile_size,
    };
    let triangles = static_collider_triangles(&bare, &registry());
    let reference = regia_nav::NavTileCache::build(params, &triangles).expect("plain build");
    let layout = reference.layout();
    let (tiles_x, tiles_z) = reference.tile_grid();
    let tile_m = f64::from(cook.tile_size) * f64::from(cook.cell_size);
    let mut settings = settings();
    settings.nav = Some(cook);
    let grid = settings.grid().expect("grid");
    let mut per_cell: BTreeMap<CellCoord, Vec<(u32, u32)>> = BTreeMap::new();
    for tz in 0..tiles_z {
        for tx in 0..tiles_x {
            let x = f64::from(layout.origin.x) + (f64::from(tx) + 0.5) * tile_m;
            let z = f64::from(layout.origin.z) + (f64::from(tz) + 0.5) * tile_m;
            per_cell
                .entry(grid.cell_of(x, z).expect("cell"))
                .or_default()
                .push((tx, tz));
        }
    }
    assert_eq!(per_cell.len(), plain.cells.len());
    for (cell, tiles) in per_cell {
        let want = reference.cook_cell(&tiles).expect("reference cook");
        assert_eq!(
            plain.cells[&cell].0, want,
            "cell {cell:?} equals the plain build's bytes"
        );
    }
}

/// An id past 63 fails the cook with a message, not a panic.
#[test]
fn a_nav_surface_past_the_last_area_id_fails_the_cook() {
    const SURFACE: &str = "regia_framework::nav_world::NavSurface";
    let scene = level(
        &[static_box(
            1,
            at(0.0, -0.5, 0.0),
            "(12.0, 0.5, 12.0)",
            &[(SURFACE, "(area: 64)".to_owned())],
        )],
        "",
    );
    let err = cook_nav_cells(&scene).err().expect("fails");
    assert!(err.to_string().contains("nav surface"), "{err}");
}

/// One cooked blob of `len` bytes, named for `id`.
fn write_blob(dir: &std::path::Path, id: AssetId, len: usize) {
    regia_test_utils::write_atomic(&dir.join(regia_asset::cooked_file_name(id)), &vec![7u8; len])
        .expect("blob");
}

fn manifest_entry(path: &str, id: AssetId, type_path: &str) -> regia_asset::ManifestEntry {
    regia_asset::ManifestEntry {
        path: path.to_owned(),
        id,
        type_path: type_path.to_owned(),
    }
}

/// N copies of one mesh, an empty material, and a manifest of M unrelated paths stay a linear
/// number of manifest walks, and the cell still depends on that mesh's ids only.
#[test]
fn shared_mesh_paths_do_not_rescan_the_manifest() {
    const N: u64 = 32;
    const M: u64 = 200;
    let mesh_path = "meshes/prop.gltf";
    let mesh_id = AssetId(guid(0xA11));
    let sub_id = AssetId(guid(0xA12));
    let decoy_id = AssetId(guid(0xA13));
    let dir = unique_temp_dir("stream_manifest_index");
    let mut entries = Vec::with_capacity(M as usize + 3);
    for i in 0..M {
        entries.push(manifest_entry(
            &format!("unrelated/{i}.bin"),
            AssetId(guid(0x1_0000 + i as u128)),
            "regia_asset::CookedImage",
        ));
    }
    entries.push(manifest_entry(mesh_path, mesh_id, "regia_asset::CookedMesh"));
    entries.push(manifest_entry(
        &format!("{mesh_path}#mesh/0"),
        sub_id,
        "regia_asset::CookedMesh",
    ));
    // A path that shares a string prefix but not a `#` boundary must not match.
    entries.push(manifest_entry(
        &format!("{mesh_path}-extra"),
        decoy_id,
        "regia_asset::CookedMesh",
    ));
    let manifest_len = entries.len() as u64;
    regia_asset::AssetManifest { entries }
        .write_to_cache(&dir)
        .expect("manifest");
    write_blob(&dir, mesh_id, 16);
    write_blob(&dir, sub_id, 32);

    let assets = CacheAssets::for_platform(&dir, "linux");
    let records: Vec<String> = (1..=N)
        .map(|i| {
            entity(
                i as u128,
                None,
                &[
                    (TRANSFORM, at(20.0, 0.0, 20.0)),
                    (
                        "regia_render::runtime::MeshSource",
                        format!("(mesh: \"{mesh_path}\", material: \"\")"),
                    ),
                ],
            )
        })
        .collect();
    let scene = level(&records, "");
    let part = split_with(&scene, &assets);
    let cell = part.cells.values().next().expect("one cell");
    assert_eq!(
        cell.deps,
        BTreeSet::from([mesh_id, sub_id]),
        "the decoy and the unrelated paths stay out of the cell"
    );
    let walks = assets.manifest_walks();
    let bound = (N + manifest_len) * 2;
    eprintln!("manifest_walks={walks} bound={bound} manifest={manifest_len}");
    assert!(
        walks <= bound,
        "manifest walks {walks} exceeded the O(N + M) bound {bound} (N={N}, manifest={manifest_len})"
    );

    let before = assets.manifest_walks();
    for i in 0..50u64 {
        let ids = assets.assets_of(&format!("unrelated/{i}.bin"));
        assert_eq!(ids, vec![AssetId(guid(0x1_0000 + i as u128))]);
    }
    let extra = assets.manifest_walks() - before;
    eprintln!("distinct_path_walks={extra}");
    assert!(
        extra <= 50 * 2,
        "distinct path lookups walked {extra} manifest entries"
    );
    assert!(assets.assets_of("").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

fn split_with(scene: &SceneFile, assets: &CacheAssets) -> Partition {
    partition(scene, settings(), &registry(), assets).expect("partition")
}

/// Many entities that name one cooked file stat it once, and the cell records that file's length.
#[test]
fn a_cooked_file_is_stated_once_for_many_entities() {
    const N: u64 = 24;
    const FILE_LEN: u64 = 128;
    let mesh_path = "meshes/crate.gltf";
    let mesh_id = AssetId(guid(0xB20));
    let dir = unique_temp_dir("stream_cost_memo");
    let out = unique_temp_dir("stream_cost_memo_out");
    regia_asset::AssetManifest {
        entries: vec![manifest_entry(mesh_path, mesh_id, "regia_asset::CookedMesh")],
    }
    .write_to_cache(&dir)
    .expect("manifest");
    write_blob(&dir, mesh_id, FILE_LEN as usize);

    let assets = CacheAssets::for_platform(&dir, "linux");
    let records: Vec<String> = (1..=N)
        .map(|i| {
            entity(
                i as u128,
                None,
                &[
                    (TRANSFORM, at(10.0, 0.0, 10.0)),
                    (
                        "regia_render::runtime::MeshSource",
                        format!("(mesh: \"{mesh_path}\", material: \"\")"),
                    ),
                ],
            )
        })
        .collect();
    let scene = level(&records, "");
    let part = split_with(&scene, &assets);
    assert_eq!(part.cells.len(), 1);
    assert!(part.cells.values().next().expect("cell").deps.contains(&mesh_id));

    cook_partition(
        lvl(),
        &scene,
        part,
        &registry(),
        &assets,
        Some(&out),
        "a.level.ron",
    )
    .expect("cook");
    let index = regia_world::format::WorldIndex::from_bytes(
        &std::fs::read(out.join(regia_asset::cooked_file_name(world_index_id(lvl())))).expect("index"),
        lvl(),
    )
    .expect("world index");
    assert_eq!(index.cells.len(), 1);
    assert_eq!(index.cells[0].dep_bytes, FILE_LEN);
    assert_eq!(index.cells[0].vram_bytes, FILE_LEN);
    eprintln!("metadata_calls_after_cook={}", assets.metadata_calls());
    assert_eq!(
        assets.metadata_calls(),
        1,
        "each entity restated the same cooked file"
    );
    assert_eq!(assets.cost(mesh_id).expect("cost").ram_bytes, FILE_LEN);
    assert_eq!(assets.metadata_calls(), 1, "a repeated cost still does not stat");
    let missing = AssetId(guid(0xDEAD));
    assert!(assets.cost(missing).is_none());
    assert!(assets.cost(missing).is_none());
    assert_eq!(assets.metadata_calls(), 2, "a missing file is stated once");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&out);
}

/// ADR-8812: a cook stopped after its first changed blob leaves no index naming the new blobs: the
/// old index is removed before that blob is written, so the level reads as not cooked
/// (`stream_not_cooked`), never as corrupt; the next complete cook loads as before, and a recook of
/// an unchanged level writes nothing (not even the index).
#[test]
fn a_cook_stopped_after_its_first_changed_blob_reads_as_not_cooked() {
    let root = unique_temp_dir("stream_cook_stopped");
    let dir = regia_asset::cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&dir).expect("cache dir");
    let mut records = world();
    let first = level(&records, "");
    cook_streamed_level(lvl(), &first, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let index_path = dir.join(cooked_file_name(world_index_id(lvl())));
    assert!(index_path.is_file(), "the first cook writes the index");

    // An unchanged recook writes no file at all (the index included).
    let writes = test_hooks::file_writes();
    cook_streamed_level(lvl(), &first, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    assert_eq!(
        test_hooks::file_writes() - writes,
        0,
        "an unchanged recook rewrites nothing"
    );
    assert!(index_path.is_file());

    // Entity 1000 moves into (1, 0): that cell's blob changes. Stop right after it is written.
    records[0] = entity(1000, None, &[(TRANSFORM, at(150.0, 0.0, 50.0))]);
    let moved = level(&records, "");
    test_hooks::stop_after_writes(Some(1));
    let stopped = cook_streamed_level(lvl(), &moved, &registry(), &(), Some(&dir), "a.level.ron");
    test_hooks::stop_after_writes(None);
    assert!(stopped.is_err(), "the hook stops the cook");
    assert!(
        !index_path.exists(),
        "the old index must not survive over the new blob it does not describe"
    );
    // The open's reason code (`None`: it opened), from a world with only an asset server.
    let open_reason = || -> Option<&'static str> {
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open the cache");
        let mut world = World::new();
        world.insert_resource(server);
        regia_world::open_streamed_level(&mut world, lvl(), regia_scene::level::LoadMode::Additive)
            .err()
            .map(|e| e.reason())
    };
    assert_eq!(
        open_reason(),
        Some("stream_not_cooked"),
        "a stopped cook is not a cooked level"
    );

    // The next complete cook writes the index again and the level opens past the index read.
    cook_streamed_level(lvl(), &moved, &registry(), &(), Some(&dir), "a.level.ron")
        .expect("cook")
        .expect("streamed");
    let bytes = std::fs::read(&index_path).expect("index");
    let index = WorldIndex::from_bytes(&bytes, lvl()).expect("decode");
    for record in &index.cells {
        let file = std::fs::read(dir.join(cooked_file_name(record.blob))).expect("blob");
        CellBlob::from_bytes(&file, record.blob, &record.blob_hash).expect("every blob matches the index");
    }
    assert_eq!(
        open_reason(),
        Some("stream_not_enabled"),
        "the index and the always set read; this world has no streaming runtime"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn cook_review_notes_never_reach_streamed_cells() {
    let scene = level(&[
        entity(1, None, &[(TRANSFORM, at(0.0, 0.0, 0.0))]),
        entity(2, Some(1), &[("regia_editor::review_notes::ReviewNote", "(text: \"private review\", author: \"Reviewer\", created: 1, resolved: false, thread: [])".into())]),
    ], "");
    let partition = split(&scene);
    let count = partition.always.entities.len()
        + partition
            .cells
            .values()
            .map(|cell| cell.entities.len())
            .sum::<usize>();
    assert_eq!(count, 1, "review marker must be stripped before cooking");
}
