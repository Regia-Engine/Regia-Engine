// SPDX-License-Identifier: MPL-2.0
//! Contract tests for preservation and refusal behavior in cooked world-index migration.

use regia_asset::AssetId;
use regia_core::id::Guid;
use regia_scene::hlod::{CellProxy, HlodConfig};
use regia_scene::level::LevelId;
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    HLOD_ATLAS_MAX_PX_DEFAULT, HlodSettings, LevelStreaming, OriginSettings, StreamBudgets, StreamNavCook,
    StreamNavLayout, StreamTuning,
};
use regia_world::format::{
    CellRecord, HlodIndex, WORLD_INDEX_FORMAT_VERSION, WorldFormatError, WorldIndex,
    migrate_world_index_file, world_index_file_version,
};

fn id(value: u128) -> AssetId {
    AssetId(Guid::from_u128(value))
}

fn expected_index(nav: Option<StreamNavLayout>, hlod_present: bool) -> WorldIndex {
    let level = Guid::from_u128(0xfeed_1234);
    let cells = vec![
        CellRecord {
            coord: CellCoord::new(-4, -2),
            blob: id(101),
            blob_hash: [0x11; 32],
            entity_count: 0,
            own_bytes: 0,
            dep_bytes: 19,
            vram_bytes: 31,
            min_y: -48.25,
            max_y: -1.0,
            priority: -7,
            deps: vec![id(501), id(502)],
            nav_tiles: 0,
            has_terrain_collision: true,
        },
        CellRecord {
            coord: CellCoord::new(-1, 3),
            blob: id(102),
            blob_hash: [0x22; 32],
            entity_count: 13,
            own_bytes: 4097,
            dep_bytes: 0,
            vram_bytes: 800,
            min_y: 2.5,
            max_y: 19.75,
            priority: 0,
            deps: vec![],
            nav_tiles: 5,
            has_terrain_collision: false,
        },
        CellRecord {
            coord: CellCoord::new(2, -5),
            blob: id(103),
            blob_hash: [0x33; 32],
            entity_count: 2,
            own_bytes: 128,
            dep_bytes: 64,
            vram_bytes: 0,
            min_y: -0.125,
            max_y: 0.125,
            priority: 41,
            deps: vec![id(503)],
            nav_tiles: 1,
            has_terrain_collision: true,
        },
        CellRecord {
            coord: CellCoord::new(5, 7),
            blob: id(104),
            blob_hash: [0x44; 32],
            entity_count: 1,
            own_bytes: 65_536,
            dep_bytes: 300,
            vram_bytes: 12_345,
            min_y: 100.0,
            max_y: 800.0,
            priority: 3,
            deps: vec![id(504), id(505), id(506)],
            nav_tiles: 0,
            has_terrain_collision: false,
        },
    ];
    let mut hasher = blake3::Hasher::new();
    for cell in &cells {
        hasher.update(&cell.blob_hash);
    }

    WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION,
        level,
        content_hash: *hasher.finalize().as_bytes(),
        settings: LevelStreaming {
            version: 1,
            enabled: true,
            cell_size_m: 256.0,
            load_radius_m: 901.5,
            unload_radius_m: 1200.0,
            max_entity_extent_cells: 3.5,
            hlod: Some(HlodSettings {
                screen_size_threshold: Some(0.0375),
                fov_y_rad: 1.12,
                merge_factor: 3,
                max_levels: 4,
                proxy_byte_ratio: 0.23,
                leaf_triangle_ratio: 0.31,
                parent_triangle_ratio: 0.42,
                min_triangles: 7,
            }),
            nav: Some(StreamNavCook {
                cell_size: 0.3,
                cell_height: 0.17,
                border_size: 2.25,
                tile_size: 48,
                agent_radius: 0.61,
                agent_height: 1.85,
                agent_max_climb: 0.72,
                agent_max_slope: 0.88,
            }),
            origin: OriginSettings {
                enabled: true,
                cell_size_m: 2048.0,
                threshold_m: 6100.0,
            },
            budgets: Some(StreamBudgets {
                max_loads_per_step: 5,
                max_inflight: 7,
                max_pending_bytes: 17_000_001,
                apply_budget_ms: 3.25,
                apply_budget_entities: 87,
                memory_budget_bytes: 2_300_000_003,
                vram_budget_bytes: 901_000_007,
                hlod_budget_bytes: 81_000_009,
            }),
            tuning: StreamTuning {
                plan_interval_steps: 11,
                retry_after_steps: 93,
                max_retries: 6,
                guard_margin_m: 2.75,
                wait_on_open: false,
                hlod_fade_frames: Some(13),
                lockstep_latency_steps: 5,
                lockstep_wait_cap_ms: 2300,
            },
            // The only v1-to-v2 defaults: v1 has no atlas settings or node atlases.
            hlod_atlas_px: 0,
            hlod_atlas_max_px: HLOD_ATLAS_MAX_PX_DEFAULT,
        },
        cells,
        bounds: [CellCoord::new(-4, -5), CellCoord::new(5, 7)],
        hlod: hlod_present.then(|| HlodIndex {
            config: HlodConfig {
                fov_y_rad: 1.12,
                screen_size_threshold: 0.0375,
                merge_factor: 3,
                max_levels: 4,
                proxy_byte_ratio: 0.23,
            },
            proxies: vec![
                CellProxy {
                    cell: CellCoord::new(-4, -2),
                    min_y: -48.25,
                    max_y: -1.0,
                    bytes: 700,
                },
                CellProxy {
                    cell: CellCoord::new(5, 7),
                    min_y: 100.0,
                    max_y: 800.0,
                    bytes: 99_001,
                },
            ],
            node_proxies: vec![Some(id(801)), None, Some(id(803))],
            node_anchors: vec![[1, -3, 5], [0, 0, 0], [-8, 13, 21]],
            node_colors: vec![Some([0.12, 0.34, 0.56, 0.78]), None, Some([1.0, 0.0, 0.25, 0.5])],
            node_atlases: vec![None, None, None],
        }),
        replicated: vec![
            (Guid::from_u128(0x1001), CellCoord::new(-4, -2)),
            (Guid::from_u128(0x1002), CellCoord::new(2, -5)),
            (Guid::from_u128(0x1003), CellCoord::new(5, 7)),
        ],
        nav,
    }
}

#[test]
fn v1_world_index_migration_preserves_the_contract_matrix() {
    let nav = Some(StreamNavLayout {
        cook: StreamNavCook {
            cell_size: 0.3,
            cell_height: 0.17,
            border_size: 2.25,
            tile_size: 48,
            agent_radius: 0.61,
            agent_height: 1.85,
            agent_max_climb: 0.72,
            agent_max_slope: 0.88,
        },
        origin: [-201.5, -17.0, 93.25],
        width: 117,
        height: 203,
    });

    for (case_name, expected) in [
        ("with-nav-and-hlod", expected_index(nav, true)),
        ("without-nav-or-hlod", expected_index(None, false)),
    ] {
        let v1 = expected.to_v1_bytes().expect("encode v1 fixture");
        assert_eq!(world_index_file_version(&v1).expect("version probe"), Some(1));

        let dir = regia_test_utils::unique_temp_dir("world-index-migration-matrix");
        let fixture_path = dir.join(format!("{case_name}.wstream"));
        regia_test_utils::write_atomic(&fixture_path, &v1).expect("write v1 fixture atomically");
        let fixture = std::fs::read(&fixture_path).expect("read v1 fixture");

        let direct = WorldIndex::from_bytes(&fixture, LevelId(expected.level)).expect("direct v1 decode");
        assert_eq!(direct, expected, "direct legacy read: {case_name}");

        let migrated = migrate_world_index_file(&fixture)
            .expect("migrate v1 fixture")
            .expect("v1 is rewritten");
        assert_eq!(
            world_index_file_version(&migrated).expect("migrated version"),
            Some(2)
        );
        let upgraded =
            WorldIndex::from_bytes(&migrated, LevelId(expected.level)).expect("decode migrated current file");
        assert_eq!(upgraded, expected, "migrated meaning: {case_name}");
        assert_eq!(upgraded, direct, "direct and rewritten paths: {case_name}");
        assert_eq!(upgraded.settings.hlod_atlas_px, 0);
        assert_eq!(upgraded.settings.hlod_atlas_max_px, HLOD_ATLAS_MAX_PX_DEFAULT);
        if let Some(index) = &upgraded.hlod {
            assert!(index.node_atlases.iter().all(Option::is_none));
            assert_eq!(index.node_atlases.len(), index.node_proxies.len());
        }

        assert_eq!(
            migrate_world_index_file(&migrated).expect("repeat migration"),
            None
        );
        let current_bytes = expected.to_bytes().expect("current encode");
        assert_eq!(
            migrate_world_index_file(&current_bytes).expect("current version is unchanged"),
            None
        );
        let decoded_current =
            WorldIndex::from_bytes(&current_bytes, LevelId(expected.level)).expect("decode current encoding");
        assert_eq!(
            decoded_current.to_bytes().expect("re-encode current"),
            current_bytes,
            "current encoding is deterministic: {case_name}"
        );

        std::fs::remove_dir_all(dir).expect("remove fixture directory");
    }
}

#[test]
fn world_index_migration_keeps_existing_refusal_classes() {
    let expected = expected_index(None, true);
    let v1 = expected.to_v1_bytes().expect("encode v1");
    let (mut header, body) = regia_asset::decode_cooked_file(&v1).expect("decode valid v1 wrapper");
    let truncated_body = &body[..body.len() - 1];
    header.payload_len = truncated_body.len() as u64;
    let truncated = regia_asset::encode_cooked_file(&header, truncated_body).expect("encode truncated body");
    assert!(matches!(
        migrate_world_index_file(&truncated),
        Err(WorldFormatError::Decode(_))
    ));

    let future = WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION + 1,
        ..expected.clone()
    }
    .to_bytes()
    .expect("encode future version");
    assert!(matches!(
        migrate_world_index_file(&future),
        Err(WorldFormatError::Newer { found, supported })
            if found == WORLD_INDEX_FORMAT_VERSION + 1 && supported == WORLD_INDEX_FORMAT_VERSION
    ));

    // A valid cooked file of another type is skipped by the migration probe.
    let other_type = regia_world::format::always_loaded_to_bytes(
        LevelId(expected.level),
        &regia_scene::cooked::CookedScene {
            format_version: regia_scene::cooked::COOKED_SCENE_VERSION,
            strings: vec![],
            entities: vec![],
            resources: vec![],
        },
    )
    .expect("encode a different cooked type");
    assert_eq!(
        world_index_file_version(&other_type).expect("probe other type"),
        None
    );
    assert_eq!(
        migrate_world_index_file(&other_type).expect("skip other type"),
        None
    );

    // The fixture's header names the same level as its payload, guarding against accidental
    // acceptance of a type-correct but structurally invalid current index.
}
