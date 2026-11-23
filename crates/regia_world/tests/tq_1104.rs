// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep 11-04 (ADR-2705): the pure parts of `regia_world` (origin conversions, the HLOD fade rule, id
//! derivation, file-format validation, error codes, the in-memory source) plus a few runtime contracts that
//! the scheduling tests leave implicit. Lockstep and counters only: no clocks, no sleeps.

mod common;

use std::collections::BTreeSet;

use common::*;
use regia_asset::AssetId;
use regia_core::id::Guid;
use regia_scene::level::LevelId;
use regia_scene::level::LoadMode;
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::StreamedOpen;
use regia_scene::streaming::{OriginSettings, Readiness, StreamBudgets};
use regia_world::format::{
    CellRecord, WorldFormatError, WorldIndex, always_loaded_from_bytes, always_loaded_to_bytes, cell_blob_id,
    cell_blob_schema_fingerprint, hlod_proxy_id, world_index_id, world_index_schema_fingerprint,
};
use regia_world::hlod_fade::{Drawable, FadeState};
use regia_world::{
    CancelToken, CellReadError, CellState, PinId, PinWait, StreamOpenError, StreamRuntimeError, StreamTiming,
    TraceKind, WorldOrigin, WorldStreaming, open_if_cooked, open_streamed_from, open_streamed_level,
};

fn c(x: i64, z: i64) -> CellCoord {
    CellCoord::new(x, z)
}

fn streaming(app: &mut regia_app::App) -> regia_ecs::prelude::Mut<'_, WorldStreaming> {
    app.world_mut().resource_mut::<WorldStreaming>()
}

// ---- origin ----

#[test]
fn the_default_origin_is_cell_zero_and_local_equals_world() {
    let o = WorldOrigin::default();
    assert_eq!(o.to_world([1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
    assert_eq!(o.from_world([1.0, 2.0, 3.0]), Ok([1.0, 2.0, 3.0]));
}

#[test]
fn an_origin_at_a_cell_offsets_by_whole_cells_exactly() {
    let settings = OriginSettings {
        enabled: true,
        cell_size_m: 1024.0,
        threshold_m: 5000.0,
    };
    let o = WorldOrigin::at_cell(&settings, [3, -2, 1]).expect("valid");
    assert_eq!(o.to_world([0.5, 0.0, -0.5]), [3072.5, -2048.0, 1023.5]);
    // The far point comes back as a small local offset, exactly.
    assert_eq!(o.from_world([3072.5, -2048.0, 1023.5]), Ok([0.5, 0.0, -0.5]));
    // And a world point a long way off the origin still round-trips to the same world point.
    let far = [5.0e6, 10.0, -3.0e6];
    let local = o.from_world(far).expect("in range");
    let back = o.to_world(local);
    for a in 0..3 {
        assert!(
            (back[a] - far[a]).abs() < 1.0,
            "axis {a}: {} vs {}",
            back[a],
            far[a]
        );
    }
}

#[test]
fn bad_origin_settings_are_refused() {
    for size in [0.0, -1024.0, f64::NAN, f64::INFINITY] {
        let settings = OriginSettings {
            enabled: true,
            cell_size_m: size,
            threshold_m: 5000.0,
        };
        assert!(WorldOrigin::from_settings(&settings).is_err(), "cell size {size}");
    }
    let o = WorldOrigin::default();
    assert!(o.from_world([f64::NAN, 0.0, 0.0]).is_err());
}

// ---- HLOD fade rule ----

fn set(items: &[Drawable]) -> BTreeSet<Drawable> {
    items.iter().copied().collect()
}

#[test]
fn fade_length_zero_and_the_first_frame_fade_nothing() {
    let mut s = FadeState::default();
    assert!(
        s.step(&set(&[Drawable::Proxy(1)]), 8).is_empty(),
        "first frame has no previous cut"
    );
    let mut off = FadeState::default();
    off.step(&set(&[Drawable::Proxy(1)]), 0);
    assert!(
        off.step(&set(&[Drawable::Proxy(2)]), 0).is_empty(),
        "no fade length authored: instant"
    );
}

#[test]
fn a_switch_dissolves_with_complementary_shares_then_ends() {
    let mut s = FadeState::default();
    let proxy = Drawable::Proxy(1);
    let full = Drawable::Full(c(0, 0));
    s.step(&set(&[proxy]), 4);
    let mut shares = Vec::new();
    // proxy -> full: the full cell fades in, the proxy fades out for the same 4 frames.
    for _ in 0..4 {
        let out = s.step(&set(&[full]), 4);
        shares.push((out.get(&full).copied(), out.get(&proxy).copied()));
    }
    assert_eq!(shares[0], (Some(0.25), Some(-0.25)));
    assert_eq!(shares[1], (Some(0.5), Some(-0.5)));
    assert_eq!(shares[2], (Some(0.75), Some(-0.75)));
    assert_eq!(shares[3], (None, None), "the fade is over on frame 4");
    assert!(s.step(&set(&[full]), 4).is_empty());
}

#[test]
fn a_reversal_mid_fade_continues_from_the_share_it_had() {
    let mut s = FadeState::default();
    let a = Drawable::Proxy(1);
    let b = Drawable::Proxy(2);
    s.step(&set(&[a]), 8);
    s.step(&set(&[b]), 8); // b in, a out, frame 1
    s.step(&set(&[b]), 8); // frame 2
    let back = s.step(&set(&[a]), 8); // a returns while it was fading out at 2/8
    let share_a = back.get(&a).copied().expect("a fades in again");
    assert!(share_a > 0.0 && share_a < 1.0);
    // a left at frame 2, so coming back it has the 6 frames it had not yet faded and one step on.
    assert!((share_a - 7.0 / 8.0).abs() < 1e-6, "a came back at {share_a}");
    assert!(back.get(&b).is_some_and(|v| *v < 0.0), "b now fades out");
}

#[test]
fn clear_forgets_fades_and_the_started_flag() {
    let mut s = FadeState::default();
    s.step(&set(&[Drawable::Proxy(1)]), 4);
    assert!(!s.step(&set(&[Drawable::Proxy(2)]), 4).is_empty());
    s.clear();
    assert_eq!(s, FadeState::default());
    assert!(
        s.step(&set(&[Drawable::Proxy(3)]), 4).is_empty(),
        "after clear the next frame is a first frame"
    );
}

// ---- ids and the file format ----

fn lvl(n: u128) -> LevelId {
    LevelId(Guid::from_u128(n))
}

#[test]
fn asset_ids_depend_on_the_level_and_the_exact_cell_and_node() {
    let a = lvl(1);
    let b = lvl(2);
    assert_ne!(world_index_id(a), world_index_id(b));
    assert_ne!(cell_blob_id(a, c(0, 0)), cell_blob_id(b, c(0, 0)));
    assert_ne!(cell_blob_id(a, c(0, 1)), cell_blob_id(a, c(1, 0)));
    assert_ne!(
        cell_blob_id(a, c(-1, 0)),
        cell_blob_id(a, c(1, 0)),
        "negative coordinates are distinct cells"
    );
    assert_ne!(hlod_proxy_id(a, 3), cell_blob_id(a, c(3, 0)));
    assert_eq!(hlod_proxy_id(a, u32::MAX), hlod_proxy_id(a, u32::MAX));
}

#[test]
fn the_content_hash_depends_on_cell_order_and_hashes() {
    let app = app();
    let world = build(&app, 2, 2, 1, 1, settings(40.0, 60.0));
    let cells = world.index.cells.clone();
    assert_eq!(WorldIndex::hash_cells(&cells), world.index.content_hash);
    let mut reversed = cells.clone();
    reversed.reverse();
    assert_ne!(WorldIndex::hash_cells(&reversed), world.index.content_hash);
    assert_eq!(WorldIndex::hash_cells(&[]), *blake3::hash(b"").as_bytes());
    let mut changed = cells.clone();
    changed[0].blob_hash[0] ^= 1;
    assert_ne!(WorldIndex::hash_cells(&changed), world.index.content_hash);
}

#[test]
fn validate_refuses_duplicate_cells_and_accepts_an_empty_world() {
    let app = app();
    let mut world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    let dup: CellRecord = world.index.cells[0].clone();
    world.index.cells.insert(1, dup);
    assert!(
        matches!(world.index.validate(), Err(WorldFormatError::Corrupt(_))),
        "equal coordinates are not sorted-unique"
    );
    let mut empty = build(&app, 1, 1, 0, 1, settings(40.0, 60.0)).index;
    empty.cells.clear();
    empty.validate().expect("a world with no cells is valid");
}

#[test]
fn cell_reasons_are_the_documented_codes() {
    assert_eq!(
        WorldFormatError::Corrupt("x".into()).cell_reason(),
        "cell_corrupt"
    );
    assert_eq!(WorldFormatError::Decode("x".into()).cell_reason(), "cell_decode");
    assert_eq!(
        WorldFormatError::Newer {
            found: 2,
            supported: 1
        }
        .cell_reason(),
        "cell_format_newer"
    );
    assert_eq!(CellReadError::Missing.reason(), "cell_missing");
    assert_eq!(CellReadError::Io("e".into()).reason(), "cell_corrupt");
    assert_eq!(
        CellReadError::Unavailable("c".into()).reason(),
        "cell_unavailable"
    );
}

#[test]
fn the_always_loaded_set_is_bound_to_its_level() {
    let bytes = always_loaded_to_bytes(lvl(5), &empty_scene()).expect("encode");
    assert!(always_loaded_from_bytes(&bytes, lvl(5)).is_ok());
    assert!(
        always_loaded_from_bytes(&bytes, lvl(6)).is_err(),
        "another level's set is refused"
    );
    assert!(always_loaded_from_bytes(&bytes[..bytes.len() / 2], lvl(5)).is_err());
    assert!(always_loaded_from_bytes(&[], lvl(5)).is_err());
}

#[test]
fn schema_fingerprints_are_stable_and_distinct() {
    let index = world_index_schema_fingerprint();
    let cell = cell_blob_schema_fingerprint();
    assert_eq!(index, world_index_schema_fingerprint());
    assert_ne!(index, cell);
    assert!(index.contains("content_hash") && index.contains("bounds"));
    assert!(cell.contains("anchor"));
}

// ---- state and error vocabulary ----

#[test]
fn cell_states_know_what_they_have_reached() {
    let all = [
        (CellState::Unloaded, "unloaded", false, false, false),
        (CellState::Queued, "queued", false, false, false),
        (CellState::Loading, "loading", false, false, false),
        (CellState::Applying, "applying", false, false, false),
        (CellState::Spawned, "spawned", true, false, false),
        (CellState::SimReady, "sim_ready", true, true, false),
        (CellState::Visible, "visible", true, true, true),
        (CellState::Unloading, "unloading", false, false, false),
        (
            CellState::Failed("cell_missing".into()),
            "failed",
            false,
            false,
            false,
        ),
    ];
    let mut names = BTreeSet::new();
    for (state, name, spawned, sim, visible) in all {
        assert_eq!(state.as_str(), name);
        assert_eq!(state.is_spawned(), spawned, "{name}");
        assert_eq!(state.reached(Readiness::SimReady), sim, "{name}");
        assert_eq!(state.reached(Readiness::Visible), visible, "{name}");
        names.insert(name);
    }
    assert_eq!(names.len(), 9);
}

#[test]
fn timing_and_trace_names_are_distinct_and_lockstep_runs_in_the_fixed_step() {
    assert_eq!(StreamTiming::Async.as_str(), "async");
    assert!(!StreamTiming::Async.in_fixed_step());
    let lockstep = StreamTiming::Lockstep { latency_steps: 3 };
    assert_ne!(lockstep.as_str(), StreamTiming::Async.as_str());
    assert!(lockstep.in_fixed_step());
    let kinds = [
        TraceKind::Queued,
        TraceKind::Loading,
        TraceKind::Applied,
        TraceKind::SimReady,
        TraceKind::Visible,
        TraceKind::Unloading,
        TraceKind::Unloaded,
        TraceKind::Cancelled,
        TraceKind::Failed,
        TraceKind::Rebase,
    ];
    let names: BTreeSet<_> = kinds.iter().map(|k| k.as_str()).collect();
    assert_eq!(names.len(), kinds.len());
}

#[test]
fn runtime_errors_carry_their_codes() {
    let cases = [
        (StreamRuntimeError::NotEnabled, "stream_not_enabled"),
        (StreamRuntimeError::UnknownCell(c(1, 2)), "stream_unknown_cell"),
        (StreamRuntimeError::UnknownPin(9), "stream_unknown_pin"),
        (StreamRuntimeError::SourceLimit, "stream_source_limit"),
        (StreamRuntimeError::BadBudget("b".into()), "stream_bad_budget"),
        (
            StreamRuntimeError::FormatNewer {
                found: 2,
                supported: 1,
            },
            "stream_format_newer",
        ),
        (
            StreamRuntimeError::BadIndex("i".into()),
            "stream_settings_invalid",
        ),
        (StreamRuntimeError::NonFinite("center"), "stream_settings_invalid"),
        (
            StreamRuntimeError::WaitAreaRefused {
                radius_m: 1.0e9,
                detail: "d".into(),
            },
            "stream_wait_area_refused",
        ),
    ];
    for (err, code) in cases {
        assert_eq!(err.reason(), code, "{err}");
        assert!(
            code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
        assert!(!err.to_string().is_empty());
    }
}

// ---- pins and the runtime API without a level ----

#[test]
fn with_no_level_open_the_runtime_refuses_cleanly() {
    let mut ws = WorldStreaming::default();
    assert!(!ws.is_active());
    assert_eq!(ws.first_level(), Err(StreamRuntimeError::NotEnabled));
    assert_eq!(ws.prepare_teleport([0.0; 3]), Err(StreamRuntimeError::NotEnabled));
    assert_eq!(ws.retry(level(), c(0, 0)), Err(StreamRuntimeError::NotEnabled));
    assert_eq!(
        ws.set_budgets(level(), StreamBudgets::default()),
        Err(StreamRuntimeError::NotEnabled)
    );
    assert!(!ws.close_level(level()), "closing an unknown level is a no-op");
    assert_eq!(ws.state(level(), c(0, 0)), None);
    assert!(ws.entities(level(), c(0, 0)).is_empty());
    assert_eq!(ws.measured_bytes(level(), c(0, 0)), 0);
    assert_eq!(ws.focus(), None);
}

#[test]
fn pins_validate_their_numbers_and_ids_are_never_reused() {
    let mut ws = WorldStreaming::default();
    assert_eq!(
        ws.pin("n", [f64::NAN, 0.0, 0.0], 1.0, 0, None),
        Err(StreamRuntimeError::NonFinite("center"))
    );
    for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(
            ws.pin("s", [0.0; 3], scale, 0, None),
            Err(StreamRuntimeError::NonFinite("radius_scale")),
            "scale {scale}"
        );
    }
    for scale in [0.05, 8.1, 100.0] {
        let err = ws.pin("s", [0.0; 3], scale, 0, None).expect_err("range");
        assert_eq!(err.reason(), "stream_settings_invalid", "scale {scale}: {err}");
        assert!(err.to_string().contains("0.1..=8"), "{err}");
    }
    assert_eq!(ws.pins().count(), 0, "refused pins leave nothing behind");
    let edge = ws
        .pin("edge", [0.0; 3], 0.1, 0, None)
        .expect("0.1 is inside 0.1..=8");
    let edge_hi = ws
        .pin("edge-hi", [0.0; 3], 8.0, 0, None)
        .expect("8 is inside 0.1..=8");
    assert!(ws.unpin(edge) && ws.unpin(edge_hi));
    let a = ws.pin("a", [1.0, 0.0, 2.0], 2.0, 5, Some(30)).expect("ok");
    assert!(ws.unpin(a));
    assert!(!ws.unpin(a), "second unpin reports nothing removed");
    let b = ws.pin("b", [0.0; 3], 1.0, 0, None).expect("ok");
    assert_ne!(a, b, "an id is not reused");
    let info = ws.pin_info(b).expect("info");
    assert_eq!(
        (info.name.as_str(), info.priority, info.ttl_steps),
        ("b", 0, None)
    );
    assert_eq!(ws.set_pin_wait(a, None), Err(StreamRuntimeError::UnknownPin(a.0)));
    assert_eq!(
        ws.set_pin_wait(PinId(9999), None)
            .expect_err("must fail")
            .reason(),
        "stream_unknown_pin"
    );
}

#[test]
fn the_pin_limit_is_exact_and_frees_with_unpin() {
    let mut ws = WorldStreaming::default();
    let mut ids = Vec::new();
    for i in 0..regia_world::runtime::MAX_STREAM_SOURCES {
        ids.push(
            ws.pin("p", [i as f64, 0.0, 0.0], 1.0, 0, None)
                .expect("within the limit"),
        );
    }
    assert_eq!(
        ws.pin("one more", [0.0; 3], 1.0, 0, None),
        Err(StreamRuntimeError::SourceLimit)
    );
    assert!(ws.unpin(ids[10]));
    assert!(ws.pin("fits again", [0.0; 3], 1.0, 0, None).is_ok());
}

// ---- runtime contracts with a level ----

#[test]
fn a_bad_budget_is_refused_and_leaves_the_old_one() {
    let mut app = app();
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    let before = *streaming(&mut app).budgets(level()).expect("budgets");
    for bad in [
        StreamBudgets {
            max_inflight: 0,
            ..before
        },
        StreamBudgets {
            apply_budget_entities: 0,
            ..before
        },
        StreamBudgets {
            apply_budget_ms: f32::NAN,
            ..before
        },
        StreamBudgets {
            apply_budget_ms: -1.0,
            ..before
        },
    ] {
        let err = streaming(&mut app)
            .set_budgets(level(), bad)
            .expect_err("refused");
        assert_eq!(err.reason(), "stream_bad_budget");
    }
    assert_eq!(*streaming(&mut app).budgets(level()).expect("budgets"), before);
}

#[test]
fn retry_of_a_cell_that_is_not_failed_changes_nothing_and_unknown_cells_error() {
    let mut app = app();
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 6);
    let now = state(&app, c(0, 0)).expect("cell");
    assert!(now.is_spawned());
    assert_eq!(streaming(&mut app).retry(level(), c(0, 0)), Ok(now.clone()));
    assert_eq!(state(&app, c(0, 0)), Some(now));
    assert_eq!(
        streaming(&mut app)
            .retry(level(), c(40, 40))
            .expect_err("must fail")
            .reason(),
        "stream_unknown_cell"
    );
}

#[test]
fn a_cell_with_no_blob_fails_with_the_source_reason_and_a_set_error_is_reversible() {
    let mut app = app();
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    let id: AssetId = cell_id(c(0, 0));
    world
        .source
        .set_error(id, Some(CellReadError::Unavailable("chunk".into())));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 6);
    assert_eq!(
        state(&app, c(0, 0)),
        Some(CellState::Failed("cell_unavailable".into()))
    );
    assert!(stats(&app).failures >= 1);
    world.source.set_error(id, None);
    streaming(&mut app).retry(level(), c(0, 0)).expect("retry");
    steps(&mut app, 8);
    assert!(
        state(&app, c(0, 0)).is_some_and(|s| s.is_spawned()),
        "{:?}",
        state(&app, c(0, 0))
    );
}

#[test]
fn a_teleport_pin_counts_its_wait_and_the_wait_clears_when_the_ground_is_ready() {
    let mut app = app();
    let world = build(&app, 6, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    let pin = streaming(&mut app)
        .prepare_teleport([450.0, 0.0, 50.0])
        .expect("pin");
    steps(&mut app, 1);
    assert_eq!(stats(&app).pending_waits, 1, "the far ground is not ready yet");
    steps(&mut app, 10);
    assert_eq!(stats(&app).pending_waits, 0);
    assert!(streaming(&mut app).unpin(pin));
    // A wait can be cleared without removing the pin.
    let again = streaming(&mut app)
        .pin("hold", [50.0, 0.0, 50.0], 1.0, 0, None)
        .expect("pin");
    streaming(&mut app)
        .set_pin_wait(
            again,
            Some(PinWait {
                radius_m: 10.0,
                readiness: Readiness::Visible,
            }),
        )
        .expect("wait");
    streaming(&mut app).set_pin_wait(again, None).expect("cleared");
    assert!(streaming(&mut app).pin_info(again).expect("pin").wait.is_none());
}

#[test]
fn waiting_near_reports_cells_in_the_grid_order_and_honors_its_limit() {
    let mut app = app();
    let world = build(&app, 4, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    // Nothing is loaded yet: every cell around the point waits.
    let gates = app.world().resource::<regia_world::StreamGates>().clone();
    let ws = app.world().resource::<WorldStreaming>();
    let all = ws.waiting_near(&gates, [150.0, 0.0, 50.0], 120.0, Readiness::SimReady, 100);
    assert!(all.len() >= 2, "{all:?}");
    let one = ws.waiting_near(&gates, [150.0, 0.0, 50.0], 120.0, Readiness::SimReady, 1);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0], all[0], "the limit keeps the first of the same order");
    assert!(!ws.ready_near(&gates, [150.0, 0.0, 50.0], 120.0, Readiness::SimReady));
}

#[test]
fn cancel_tokens_are_shared_between_clones() {
    let t = CancelToken::new();
    let u = t.clone();
    assert!(!u.is_cancelled());
    t.cancel();
    assert!(u.is_cancelled());
}

// ---- opening a level ----

#[test]
fn opening_without_the_pieces_it_needs_fails_with_a_named_error() {
    let mut bare = regia_ecs::prelude::World::new();
    let level = lvl(1);
    assert!(matches!(
        open_streamed_level(&mut bare, level, LoadMode::Replace),
        Err(StreamOpenError::NoAssets)
    ));
    let app = app();
    let world = build(&app, 1, 1, 1, 1, settings(40.0, 60.0));
    let err = open_streamed_from(
        &mut bare,
        level,
        LoadMode::Replace,
        world.index.clone(),
        &empty_scene(),
        std::sync::Arc::new(world.source.clone()),
    )
    .expect_err("no runtime resource");
    assert!(matches!(err, StreamOpenError::NoRuntime));
    assert_eq!(err.reason(), "stream_not_enabled");
    assert!(err.to_string().contains("streaming runtime"));
}

#[test]
fn a_level_with_no_cooked_index_opens_the_ordinary_way() {
    let mut bare = regia_ecs::prelude::World::new();
    assert_eq!(
        open_if_cooked(&mut bare, lvl(9), LoadMode::Replace),
        StreamedOpen::NotCooked
    );
}

#[test]
fn open_error_codes_follow_the_spec_vocabulary() {
    let newer = StreamOpenError::Format(WorldFormatError::Newer {
        found: 3,
        supported: 1,
    });
    assert_eq!(newer.reason(), "stream_format_newer");
    let runtime = StreamOpenError::Runtime(StreamRuntimeError::SourceLimit);
    assert_eq!(runtime.reason(), "stream_source_limit");
    assert_eq!(StreamOpenError::NoAssets.reason(), "stream_no_assets");
    assert_eq!(
        StreamOpenError::Format(WorldFormatError::Corrupt("settings: unload".into())).reason(),
        "stream_settings_invalid"
    );
    assert_eq!(
        StreamOpenError::Format(WorldFormatError::Corrupt("cell records are not sorted".into())).reason(),
        "stream_index_corrupt"
    );
    assert_eq!(
        StreamOpenError::Format(WorldFormatError::Decode("postcard".into())).reason(),
        "stream_index_decode"
    );
    assert_eq!(StreamOpenError::NoRuntime.reason(), "stream_not_enabled");
    // ADR-8812: no cooked index (never cooked, or a stopped cook removed the old one).
    assert_eq!(
        StreamOpenError::NotCooked { chunk: None }.reason(),
        "stream_not_cooked"
    );
}

#[test]
fn an_index_that_fails_validation_is_refused_before_anything_opens() {
    let mut app = app();
    let mut world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    world.index.cells.reverse(); // not sorted
    let err = open_streamed_from(
        app.world_mut(),
        level(),
        LoadMode::Replace,
        world.index.clone(),
        &empty_scene(),
        std::sync::Arc::new(world.source.clone()),
    )
    .expect_err("unsorted cells");
    // Unsorted cells are not a settings failure: the open reports `stream_index_corrupt`, not
    // "streaming is off".
    assert!(
        matches!(err, StreamOpenError::Format(WorldFormatError::Corrupt(_))),
        "{err}"
    );
    assert_eq!(err.reason(), "stream_index_corrupt");
    assert!(!streaming(&mut app).is_active(), "nothing was opened");
    assert_eq!(streaming(&mut app).levels().count(), 0);
    steps(&mut app, 3);
    assert_eq!(count_members(&mut app), 0);
}

#[test]
fn a_dedicated_server_runtime_is_ready_at_sim_ready_without_render_gates() {
    let mut app = app();
    streaming(&mut app).set_sim_only(true);
    assert!(streaming(&mut app).is_sim_only());
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 8);
    let s = state(&app, c(0, 0)).expect("cell");
    assert!(s.reached(Readiness::SimReady), "{s:?}");
    streaming(&mut app).set_sim_only(false);
    assert!(!streaming(&mut app).is_sim_only());
}

#[test]
fn closing_a_level_twice_reports_it_once_and_frees_its_cells() {
    let mut app = app();
    let world = build(&app, 2, 1, 1, 1, settings(40.0, 60.0));
    open(&mut app, &world, 1);
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 6);
    assert!(count_members(&mut app) > 0);
    assert!(streaming(&mut app).close_level(level()));
    assert!(!streaming(&mut app).close_level(level()), "already closed");
    assert!(!streaming(&mut app).is_active());
    assert_eq!(state(&app, c(0, 0)), None);
    assert_eq!(
        streaming(&mut app).first_level(),
        Err(StreamRuntimeError::NotEnabled)
    );
}
