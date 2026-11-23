// SPDX-License-Identifier: MPL-2.0
//! Memory accounting on the agent protocol (`book/agents/memory.md`, ADR-539):
//! `memory.report` puts CPU heap bytes (the `regia_core::memory` ledger) and GPU bytes (the
//! wgpu allocator report) side by side per engine subsystem, with the process RSS and ECS
//! counts; `memory.mark` records a baseline that later reports diff against, so an agent can
//! load and unload a level a few times and see what stayed behind.

use regia_app::prelude::*;
use regia_core::memory::{self, MemorySnapshot, OwnerSnapshot};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The hint a report carries when the heap is not tracked.
const UNTRACKED_HINT: &str = "heap bytes per subsystem need regia_core::memory::TaggingAllocator \
                              as the host's #[global_allocator] (dev builds, tests, soaks)";

/// The baseline `memory.mark` recorded.
#[derive(Resource, Clone, Copy, Debug)]
pub(crate) struct MemoryMark {
    /// The heap ledger then.
    heap: MemorySnapshot,
    /// The named owners then (ADR-3351).
    owners: OwnerSnapshot,
    /// Spawned entities then.
    entities: u32,
    /// GPU allocator bytes then (`None` without a device or allocator report).
    gpu_allocated: Option<u64>,
}

/// ECS counts: spawned entities, archetypes, registered components.
fn ecs_counts(world: &World) -> (u32, usize, usize) {
    (
        world.entities().count_spawned(),
        world.archetypes().len(),
        world.components().len(),
    )
}

/// The GPU allocator report, when a render device exists.
fn gpu_report(world: &World) -> Option<regia_render::MemoryReport> {
    world
        .get_resource::<regia_render::RenderDevice>()
        .map(regia_render::memory_report)
}

/// `memory.mark`: records the current heap ledger, entity count and GPU total as the baseline
/// later `memory.report` calls diff against (`since_mark`). Replaces an earlier mark.
pub(crate) fn mark(world: &mut World) -> Result<Value, RpcError> {
    let heap = memory::snapshot();
    let gpu_allocated = gpu_report(world)
        .filter(|r| r.available)
        .map(|r| r.allocated_bytes);
    // Resources are entities (bevy_ecs 0.20): the first mark's own resource must not count as
    // growth, so the baseline is taken with it in place.
    world.insert_resource(MemoryMark {
        heap,
        owners: memory::owner_snapshot(),
        entities: 0,
        gpu_allocated,
    });
    let (entities, _, _) = ecs_counts(world);
    if let Some(mut mark) = world.get_resource_mut::<MemoryMark>() {
        mark.entities = entities;
    }
    Ok(json!({
        "heap_tracked": heap.tracking,
        "heap_live_bytes": heap.tracking.then_some(heap.live_bytes),
        "gpu_allocated_bytes": gpu_allocated,
        "entities": entities,
    }))
}

/// `memory.report`: CPU heap and GPU bytes per subsystem, process RSS, ECS counts, the largest GPU
/// allocations, and the growth since `memory.mark`.
pub(crate) fn report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let top = match params.get("top") {
        None | Some(Value::Null) => 10,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("top must be a non-negative integer"))?
            .min(200) as usize,
    };
    let heap = memory::snapshot();
    let owners = memory::owner_snapshot();
    let gpu = gpu_report(world);
    let inputs = ReportInputs {
        heap: &heap,
        owners: &owners,
        gpu: gpu.as_ref(),
        ecs: ecs_counts(world),
        rss: (
            regia_render::process_rss_bytes(),
            regia_render::process_peak_rss_bytes(),
        ),
        mark: world.get_resource::<MemoryMark>(),
        top,
        assets: world
            .get_resource::<regia_asset::AssetServer>()
            .map(regia_asset::AssetServer::memory_by_type),
    };
    Ok(report_json(&inputs))
}

/// What a report is built from (the testable seam of [`report`]).
struct ReportInputs<'a> {
    heap: &'a MemorySnapshot,
    owners: &'a OwnerSnapshot,
    gpu: Option<&'a regia_render::MemoryReport>,
    ecs: (u32, usize, usize),
    rss: (Option<u64>, Option<u64>),
    mark: Option<&'a MemoryMark>,
    top: usize,
    /// Loaded count and bytes per asset type (ADR-9740); `None` without an asset server.
    assets: Option<Vec<regia_asset::AssetTypeMemory>>,
}

fn report_json(inputs: &ReportInputs<'_>) -> Value {
    let heap = inputs.heap;
    let tracked = heap.tracking;
    let gpu = inputs.gpu.filter(|r| r.available);
    let gpu_by_subsystem = gpu.map(|r| {
        regia_render::memory::subsystem_bytes_of(r.labels.iter().map(|l| (l.label.as_str(), l.bytes)))
    });
    let subsystems: Vec<Value> = heap
        .subsystems
        .iter()
        .map(|(subsystem, m)| {
            json!({
                "name": subsystem.name(),
                "heap_bytes": tracked.then_some(m.live_bytes),
                "heap_peak_bytes": tracked.then_some(m.peak_bytes),
                "heap_allocations": tracked.then_some(m.live_allocations),
                "gpu_bytes": gpu_by_subsystem.map(|g| g[*subsystem as usize]),
            })
        })
        .collect();
    let top_gpu: Vec<Value> = gpu
        .map(|r| {
            r.labels
                .iter()
                .take(inputs.top)
                .map(|l| {
                    json!({
                        "label": l.label,
                        "subsystem": regia_render::memory::gpu_subsystem(&l.label).name(),
                        "category": l.category.name(),
                        "bytes": l.bytes,
                        "count": l.count,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // Named owners inside the subsystems (ADR-3351), largest first; their bytes are also in their
    // subsystem's row.
    let mut owner_rows: Vec<_> = inputs.owners.iter().filter(|_| tracked).collect();
    owner_rows.sort_by(|a, b| {
        b.memory
            .live_bytes
            .cmp(&a.memory.live_bytes)
            .then_with(|| a.name.cmp(b.name))
    });
    let owners: Vec<Value> = owner_rows
        .iter()
        .map(|o| {
            json!({
                "name": o.name,
                "subsystem": o.subsystem.name(),
                "heap_bytes": o.memory.live_bytes,
                "heap_peak_bytes": o.memory.peak_bytes,
                "heap_allocations": o.memory.live_allocations,
            })
        })
        .collect();
    let (entities, archetypes, components) = inputs.ecs;
    let since_mark = inputs.mark.map(|mark| {
        let owners_by: serde_json::Map<String, Value> = inputs
            .owners
            .iter()
            .filter(|_| tracked)
            .filter_map(|o| {
                let then = mark.owners.get(o.name).map(|b| b.memory).unwrap_or_default();
                let bytes = o.memory.live_bytes as i64 - then.live_bytes as i64;
                let allocations = o.memory.live_allocations as i64 - then.live_allocations as i64;
                (bytes != 0 || allocations != 0).then(|| {
                    (
                        o.name.to_owned(),
                        json!({ "heap_bytes": bytes, "heap_allocations": allocations }),
                    )
                })
            })
            .collect();
        let growth = heap.growth_since(&mark.heap);
        let by: serde_json::Map<String, Value> = growth
            .subsystems
            .iter()
            .filter(|(_, bytes, allocations)| *bytes != 0 || *allocations != 0)
            .map(|(subsystem, bytes, allocations)| {
                (
                    subsystem.name().to_owned(),
                    json!({ "heap_bytes": bytes, "heap_allocations": allocations }),
                )
            })
            .collect();
        let gpu_now = gpu.map(|r| r.allocated_bytes);
        json!({
            "heap_bytes": tracked.then_some(growth.live_bytes),
            "largest": growth.largest().filter(|_| tracked).map(|(s, _)| s.name()),
            "subsystems": by,
            "owners": owners_by,
            "gpu_bytes": gpu_now.zip(mark.gpu_allocated).map(|(now, then)| now as i64 - then as i64),
            "entities": i64::from(entities) - i64::from(mark.entities),
        })
    });
    json!({
        "heap": {
            "tracked": tracked,
            "live_bytes": tracked.then_some(heap.live_bytes),
            "peak_bytes": tracked.then_some(heap.peak_bytes),
            "hint": (!tracked).then_some(UNTRACKED_HINT),
        },
        "gpu": inputs.gpu.map(|r| json!({
            "available": r.available,
            "backend": r.backend,
            "allocated_bytes": r.allocated_bytes,
            "reserved_bytes": r.reserved_bytes,
        })),
        "process": { "rss_bytes": inputs.rss.0, "rss_peak_bytes": inputs.rss.1 },
        "ecs": { "entities": entities, "archetypes": archetypes, "components": components },
        "subsystems": subsystems,
        "owners": owners,
        "top_gpu": top_gpu,
        "since_mark": since_mark,
        "assets_by_type": inputs.assets.as_ref().map(|rows| {
            rows.iter()
                .map(|r| json!({ "type": r.type_name, "loaded": r.loaded, "bytes": r.bytes }))
                .collect::<Vec<Value>>()
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::memory::{Subsystem, SubsystemMemory};

    fn tracked_heap(scene_bytes: u64) -> MemorySnapshot {
        let mut heap = MemorySnapshot {
            tracking: true,
            live_bytes: 10_000 + scene_bytes,
            peak_bytes: 50_000,
            ..MemorySnapshot::default()
        };
        heap.subsystems = std::array::from_fn(|i| (Subsystem::ALL[i], SubsystemMemory::default()));
        heap.subsystems[Subsystem::Scene as usize].1 = SubsystemMemory {
            live_bytes: scene_bytes,
            peak_bytes: scene_bytes,
            live_allocations: scene_bytes / 100,
            total_allocations: scene_bytes / 100,
            total_bytes: scene_bytes,
        };
        heap
    }

    #[test]
    fn report_lists_every_subsystem_with_heap_and_gpu_bytes() {
        let heap = tracked_heap(4_000);
        let gpu = regia_render::MemoryReport::from_allocations(
            "Vulkan",
            [("terrain heights", 400), ("frame hdr", 100), ("vfx atlas", 7)],
            1024,
        );
        let v = report_json(&ReportInputs {
            heap: &heap,
            owners: &OwnerSnapshot::default(),
            gpu: Some(&gpu),
            ecs: (12, 3, 40),
            rss: (Some(1 << 20), None),
            mark: None,
            top: 2,
            assets: None,
        });
        assert_eq!(v["heap"]["tracked"], true);
        assert_eq!(v["heap"]["live_bytes"], 14_000);
        assert!(v["heap"]["hint"].is_null());
        let subsystems = v["subsystems"].as_array().expect("array");
        assert_eq!(subsystems.len(), Subsystem::COUNT);
        let get = |name: &str| {
            subsystems
                .iter()
                .find(|s| s["name"] == name)
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(get("scene")["heap_bytes"], 4_000);
        assert_eq!(get("scene")["heap_allocations"], 40);
        assert_eq!(get("terrain")["gpu_bytes"], 400);
        assert_eq!(get("render")["gpu_bytes"], 100);
        assert_eq!(get("vfx")["gpu_bytes"], 7);
        assert_eq!(v["top_gpu"].as_array().map(Vec::len), Some(2));
        assert_eq!(v["top_gpu"][0]["subsystem"], "terrain");
        assert_eq!(v["gpu"]["allocated_bytes"], 507);
        assert_eq!(v["ecs"]["entities"], 12);
        assert_eq!(v["process"]["rss_bytes"], 1 << 20);
        assert!(v["since_mark"].is_null());
    }

    #[test]
    fn untracked_heap_reports_nulls_and_a_hint() {
        let heap = MemorySnapshot::default();
        let v = report_json(&ReportInputs {
            heap: &heap,
            owners: &OwnerSnapshot::default(),
            gpu: None,
            ecs: (0, 1, 0),
            rss: (None, None),
            mark: None,
            top: 10,
            assets: None,
        });
        assert_eq!(v["heap"]["tracked"], false);
        assert!(v["heap"]["live_bytes"].is_null());
        assert!(
            v["heap"]["hint"]
                .as_str()
                .is_some_and(|h| h.contains("TaggingAllocator"))
        );
        assert!(v["gpu"].is_null());
        assert!(v["subsystems"][0]["heap_bytes"].is_null());
        assert!(v["subsystems"][0]["gpu_bytes"].is_null());
    }

    #[test]
    fn since_mark_names_what_grew() {
        let then = tracked_heap(1_000);
        let now = tracked_heap(9_000);
        let mark = MemoryMark {
            heap: then,
            owners: OwnerSnapshot::default(),
            entities: 100,
            gpu_allocated: None,
        };
        let v = report_json(&ReportInputs {
            heap: &now,
            owners: &OwnerSnapshot::default(),
            gpu: None,
            ecs: (164, 3, 40),
            rss: (None, None),
            mark: Some(&mark),
            top: 10,
            assets: None,
        });
        assert_eq!(v["since_mark"]["heap_bytes"], 8_000);
        assert_eq!(v["since_mark"]["largest"], "scene");
        assert_eq!(v["since_mark"]["subsystems"]["scene"]["heap_bytes"], 8_000);
        assert_eq!(v["since_mark"]["subsystems"]["scene"]["heap_allocations"], 80);
        assert!(v["since_mark"]["subsystems"].get("render").is_none());
        assert_eq!(v["since_mark"]["entities"], 64);
        assert!(v["since_mark"]["gpu_bytes"].is_null());
    }

    #[test]
    fn report_lists_named_owners_largest_first_and_their_growth_since_the_mark() {
        use regia_core::memory::OwnerMemory;
        let row = |name, live: u64| OwnerMemory {
            name,
            subsystem: Subsystem::Render,
            memory: SubsystemMemory {
                live_bytes: live,
                peak_bytes: live + 5,
                live_allocations: live / 10,
                ..SubsystemMemory::default()
            },
        };
        let heap = tracked_heap(1_000);
        let then = OwnerSnapshot::from_rows(&[row("render.pipelines", 700)]);
        let now =
            OwnerSnapshot::from_rows(&[row("render.pipelines", 700), row("render.shader_modules", 3_000)]);
        let mark = MemoryMark {
            heap,
            owners: then,
            entities: 0,
            gpu_allocated: None,
        };
        let v = report_json(&ReportInputs {
            heap: &heap,
            owners: &now,
            gpu: None,
            ecs: (0, 0, 0),
            rss: (None, None),
            mark: Some(&mark),
            top: 10,
            assets: None,
        });
        assert_eq!(v["owners"][0]["name"], "render.shader_modules");
        assert_eq!(v["owners"][0]["subsystem"], "render");
        assert_eq!(v["owners"][0]["heap_bytes"], 3_000);
        assert_eq!(v["owners"][0]["heap_peak_bytes"], 3_005);
        assert_eq!(v["owners"][0]["heap_allocations"], 300);
        assert_eq!(v["owners"][1]["name"], "render.pipelines");
        let grew = &v["since_mark"]["owners"];
        assert_eq!(grew["render.shader_modules"]["heap_bytes"], 3_000);
        assert!(
            grew.get("render.pipelines").is_none(),
            "unchanged owners are left out"
        );
        // Untracked: no owner rows.
        let untracked = MemorySnapshot::default();
        let v = report_json(&ReportInputs {
            heap: &untracked,
            owners: &now,
            gpu: None,
            ecs: (0, 0, 0),
            rss: (None, None),
            mark: None,
            top: 10,
            assets: None,
        });
        assert_eq!(v["owners"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn mark_then_report_through_the_world() {
        let mut world = World::new();
        let marked = mark(&mut world).expect("mark");
        assert!(marked["entities"].is_u64());
        world.spawn_empty();
        let v = report(&mut world, &json!({ "top": 5 })).expect("report");
        assert_eq!(v["since_mark"]["entities"], 1);
        assert!(v["gpu"].is_null(), "no render device in a bare world");
        assert!(report(&mut world, &json!({ "top": "lots" })).is_err());
        assert!(v["assets_by_type"].is_null(), "no asset server, no per-type rows");
    }

    /// ADR-9740: with an asset server, `assets_by_type` lists every asset type with its loaded
    /// count and bytes, and the report's `ecs.entities` is the world's spawned-entity count (the
    /// number the editor Profiler's entity view shows).
    #[test]
    fn report_lists_assets_by_type_and_entity_counts() {
        let root = regia_test_utils::unique_temp_dir("agent-memory-assets");
        let mut world = World::new();
        world.insert_resource(
            regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            })
            .expect("open"),
        );
        for _ in 0..3 {
            world.spawn_empty();
        }
        let v = report(&mut world, &json!({})).expect("report");
        let types: Vec<&str> = v["assets_by_type"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| r["type"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(types, ["mesh", "image", "texture", "material"]);
        assert_eq!(v["assets_by_type"][0]["loaded"], 0);
        assert_eq!(v["ecs"]["entities"], u64::from(world.entities().count_spawned()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
