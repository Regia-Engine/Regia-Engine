// SPDX-License-Identifier: MPL-2.0
//! Spec 66 conformance (lane spec-27-66, ADR-5646): requirements that were built but had no test
//! of their own. Scheduler rules are driven through `GeneratorTiles::step` with synthetic anchors
//! (fixed steps, no clock); tier and reach rules through `TierPlan::build`. No GPU.

mod common;

use common::{compile, graph, node};
use regia_core::math::Vec3;
use regia_procgen::tiles::{
    AnchorSettings, AnchorSource, AnchorView, ApplyBudget, CellCoord, GeneratorTiles, ProcGenAnchor,
    SchedTier, TierPlan, TierReach, TileCell, TileKey, TileSettings, TileState, quality_reach_index,
};
use regia_render::quality::QualityTier;

fn key(tier: u8, i: i64, j: i64) -> TileKey {
    TileKey {
        tier,
        cell: CellCoord::new(i, j),
    }
}

fn one_tier_settings(build: f32) -> AnchorSettings {
    AnchorSettings {
        tiers: vec![TierReach {
            cell_m: 64.0,
            build_m: build,
            release_factor: 1.25,
            view_bias: 0.5,
            view_gate: None,
        }],
        max_new_per_frame: Some(10_000),
        max_running: Some(10_000),
        ..AnchorSettings::default()
    }
}

fn one_tier() -> GeneratorTiles {
    GeneratorTiles::new(
        [-10_000.0, -10_000.0, 10_000.0, 10_000.0],
        vec![SchedTier {
            index: 0,
            cell_m: Some(64.0),
        }],
    )
}

/// A fake host: every started cell finishes at once.
fn finish_started(g: &mut GeneratorTiles, plan: &regia_procgen::tiles::StepPlan) {
    for k in &plan.start {
        g.set_running(*k);
        g.finished(*k, 10, 1024, [0; 32], 0.0);
    }
}

// ------------------------------------------------------------------------------------------------
// § 4.2 priority, ties, reach scale
// ------------------------------------------------------------------------------------------------

/// § 4.2: `p = (d / build_m) · (1 − view_bias · facing)`, `facing = max(0, cos θ)` toward the cell
/// center. A cell straight ahead gets the facing discount, one to the side none, one behind none.
#[test]
fn s66_priority_is_normalized_distance_discounted_by_facing() {
    let mut s = one_tier_settings(400.0);
    // No slots: every wanted cell stays queued with its priority.
    s.max_new_per_frame = Some(0);
    let mut g = one_tier();
    let anchor = AnchorView {
        view_dir: Some(Vec3::X),
        ..AnchorView::at(Vec3::new(32.0, 0.0, 32.0))
    };
    let plan = g.step(&[anchor], &s, &mut ApplyBudget::default());
    assert!(plan.start.is_empty());
    let p = |k: TileKey| {
        g.entries
            .get(&k)
            .map(|e| e.priority)
            .unwrap_or_else(|| panic!("{k:?} not wanted"))
    };
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    // Ahead: (3, 0) spans x 192..256; d = 160, center straight ahead (facing 1).
    assert!(
        close(p(key(0, 3, 0)), 160.0 / 400.0 * (1.0 - 0.5)),
        "{}",
        p(key(0, 3, 0))
    );
    // Side: (0, 3) spans z 192..256; d = 160, center at 90° (facing 0).
    assert!(close(p(key(0, 0, 3)), 160.0 / 400.0), "{}", p(key(0, 0, 3)));
    // Behind: (-4, 0) spans x -256..-192; d = 224, facing clamps to 0.
    assert!(close(p(key(0, -4, 0)), 224.0 / 400.0), "{}", p(key(0, -4, 0)));
    // The anchor's own cell: d = 0.
    assert!(close(p(key(0, 0, 0)), 0.0));
    // Without a view direction facing is 0: the ahead cell loses its discount.
    let mut g = one_tier();
    g.step(
        &[AnchorView::at(Vec3::new(32.0, 0.0, 32.0))],
        &s,
        &mut ApplyBudget::default(),
    );
    let ahead = g.entries.get(&key(0, 3, 0)).map(|e| e.priority);
    assert_eq!(ahead, Some(160.0 / 400.0));
}

/// § 4.2: ties go coarser tier first, then cell `(i, j)` ascending.
#[test]
fn s66_equal_priorities_start_in_cell_order() {
    let mut s = one_tier_settings(10.0);
    s.max_new_per_frame = Some(2);
    let mut g = one_tier();
    // On the shared corner of four cells: every one is at distance 0.
    let a = [AnchorView::at(Vec3::new(64.0, 0.0, 64.0))];
    let first = g.step(&a, &s, &mut ApplyBudget::default());
    assert_eq!(first.start, vec![key(0, 0, 0), key(0, 0, 1)]);
    let second = g.step(&a, &s, &mut ApplyBudget::default());
    assert_eq!(second.start, vec![key(0, 1, 0), key(0, 1, 1)]);
}

/// § 3 scheduling order and § 4.2 ties across tiers: a fine cell starts only once the coarse cell
/// containing it is resident, and among equal priorities the coarser tier starts first.
#[test]
fn s66_fine_cells_wait_for_their_resident_coarse_parent() {
    let s = AnchorSettings {
        tiers: vec![
            TierReach {
                cell_m: 128.0,
                build_m: 100.0,
                ..TierReach::default()
            },
            TierReach {
                cell_m: 64.0,
                build_m: 100.0,
                ..TierReach::default()
            },
        ],
        max_new_per_frame: Some(10_000),
        max_running: Some(10_000),
        ..AnchorSettings::default()
    };
    let mut g = GeneratorTiles::new(
        [-10_000.0, -10_000.0, 10_000.0, 10_000.0],
        vec![
            SchedTier {
                index: 0,
                cell_m: Some(128.0),
            },
            SchedTier {
                index: 1,
                cell_m: Some(64.0),
            },
        ],
    );
    let a = [AnchorView::at(Vec3::ZERO)];
    let first = g.step(&a, &s, &mut ApplyBudget::default());
    // The four coarse cells around the origin, in cell order; no fine cell yet.
    assert_eq!(
        first.start,
        vec![key(0, -1, -1), key(0, -1, 0), key(0, 0, -1), key(0, 0, 0)]
    );
    finish_started(&mut g, &first);
    let mut fine_started = 0;
    for step in 0..10 {
        let plan = g.step(&a, &s, &mut ApplyBudget::default());
        for k in plan.start.iter().filter(|k| k.tier == 1) {
            let r = k.cell.rect(64.0);
            let parent = TileKey {
                tier: 0,
                cell: CellCoord::containing((r[0] + r[2]) * 0.5, (r[1] + r[3]) * 0.5, 128.0),
            };
            assert_eq!(
                g.state(parent),
                Some(TileState::Resident),
                "step {step}: {k:?} started before its parent {parent:?}"
            );
            fine_started += 1;
        }
        finish_started(&mut g, &plan);
    }
    // Fine cells within 100 m of the origin: x and z in -128..128 → 4 × 4.
    assert_eq!(fine_started, 16);
}

/// § 4.2: distance is divided by the anchor's `reach_scale`.
#[test]
fn s66_reach_scale_divides_the_distance() {
    let s = one_tier_settings(100.0);
    let far = key(0, 3, 0); // d = 160 from (32, 32)
    let mut plain = one_tier();
    plain.step(
        &[AnchorView::at(Vec3::new(32.0, 0.0, 32.0))],
        &s,
        &mut ApplyBudget::default(),
    );
    assert_eq!(plain.state(far), None, "160 m is beyond build 100 m");
    let mut scaled = one_tier();
    scaled.step(
        &[AnchorView {
            reach_scale: 2.0,
            ..AnchorView::at(Vec3::new(32.0, 0.0, 32.0))
        }],
        &s,
        &mut ApplyBudget::default(),
    );
    assert!(
        scaled.state(far).is_some(),
        "160 m / 2 = 80 m is within build 100 m"
    );
}

// ------------------------------------------------------------------------------------------------
// § 4.2 untiled anchored generator, § 5 pause
// ------------------------------------------------------------------------------------------------

/// § 4.2: an untiled `Anchored` generator is one cell, wanted within `base_reach_m` of its bounds and
/// released beyond `base_reach_m × 1.25`.
#[test]
fn s66_untiled_anchored_generator_is_one_cell_with_a_quarter_hysteresis() {
    let s = AnchorSettings {
        base_reach_m: 100.0,
        ..AnchorSettings::default()
    };
    let mut g = GeneratorTiles::new(
        [0.0, 0.0, 100.0, 100.0],
        vec![SchedTier {
            index: 0,
            cell_m: None,
        }],
    );
    let only = key(0, 0, 0);
    let at = |x: f32| [AnchorView::at(Vec3::new(x, 0.0, 50.0))];
    let plan = g.step(&at(-150.0), &s, &mut ApplyBudget::default());
    assert!(
        plan.start.is_empty() && g.entries.is_empty(),
        "150 m away: not wanted"
    );
    for _ in 0..3 {
        let plan = g.step(&at(-90.0), &s, &mut ApplyBudget::default());
        finish_started(&mut g, &plan);
    }
    assert_eq!(g.state(only), Some(TileState::Resident));
    assert_eq!(g.entries.len(), 1, "one cell for the whole bounds");
    // 120 m: beyond build (100) but within release (125): it stays.
    let plan = g.step(&at(-120.0), &s, &mut ApplyBudget::default());
    assert!(plan.release.is_empty());
    assert_eq!(g.state(only), Some(TileState::Resident));
    // 130 m: beyond 125 m: released.
    let plan = g.step(&at(-130.0), &s, &mut ApplyBudget::default());
    assert_eq!(plan.release, vec![only]);
}

/// § 4.2a / § 5: `ProcGenSettings::paused` freezes scheduling; resident cells stay, nothing new is
/// queued or released.
#[test]
fn s66_paused_scheduling_keeps_resident_cells_and_queues_nothing() {
    let s = one_tier_settings(100.0);
    let mut g = one_tier();
    let home = [AnchorView::at(Vec3::new(32.0, 0.0, 32.0))];
    for _ in 0..3 {
        let plan = g.step(&home, &s, &mut ApplyBudget::default());
        finish_started(&mut g, &plan);
    }
    let resident: Vec<TileKey> = g.entries.keys().copied().collect();
    assert!(!resident.is_empty());
    g.paused = true;
    let away = [AnchorView::at(Vec3::new(5_000.0, 0.0, 5_000.0))];
    for _ in 0..5 {
        let plan = g.step(&away, &s, &mut ApplyBudget::default());
        assert_eq!(plan, regia_procgen::tiles::StepPlan::default());
    }
    assert_eq!(g.entries.keys().copied().collect::<Vec<_>>(), resident);
    assert!(
        g.entries.values().all(|e| e.state == TileState::Resident),
        "paused cells stay resident"
    );
    g.paused = false;
    let plan = g.step(&away, &s, &mut ApplyBudget::default());
    assert_eq!(plan.release.len(), resident.len(), "unpaused, the far cells go");
}

// ------------------------------------------------------------------------------------------------
// § 4.2 tier_reach_order, § 2 declared reach and halo, § 2 cell size
// ------------------------------------------------------------------------------------------------

/// `in.self → sample.region → tiles.tier(256) → tiles.tier(32) → out.points`, base 64 m.
fn two_tier_plan() -> TierPlan {
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (2, node("sample.region", &[("spacing_m", "10.0")])),
            (3, node("tiles.tier", &[("cell_m", "Meters(256.0)")])),
            (4, node("tiles.tier", &[("cell_m", "Meters(32.0)")])),
            (5, node("out.points", &[("name", "\"fine\"")])),
        ],
        &[
            (1, "out", 2, "region"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 5, "in"),
        ],
    );
    TierPlan::build(&compile(&g), 64.0).unwrap_or_else(|e| panic!("{:?}", e.0))
}

fn reach(cell_m: f32, build_m: f32) -> TierReach {
    TierReach {
        cell_m,
        build_m,
        ..TierReach::default()
    }
}

/// § 4.2: the validation warns `tier_reach_order` when a finer tier builds farther than a coarser
/// one; the right order (or equal radii) warns nothing.
#[test]
fn s66_tier_reach_order_warns_when_a_fine_tier_outreaches_its_parent() {
    let plan = two_tier_plan();
    let sizes: Vec<Option<f64>> = plan.tiers.iter().map(|t| t.cell_m).collect();
    assert_eq!(sizes, vec![Some(256.0), Some(32.0)]);
    let wrong = AnchorSettings {
        tiers: vec![reach(256.0, 300.0), reach(32.0, 500.0)],
        ..AnchorSettings::default()
    };
    let warnings = plan.check_reach(&wrong);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].0, "tier_reach_order");
    let right = AnchorSettings {
        tiers: vec![reach(256.0, 600.0), reach(32.0, 120.0)],
        ..AnchorSettings::default()
    };
    assert!(plan.check_reach(&right).is_empty());
    let equal = AnchorSettings {
        tiers: vec![reach(256.0, 200.0), reach(32.0, 200.0)],
        ..AnchorSettings::default()
    };
    assert!(plan.check_reach(&equal).is_empty());
    // A fine tier without an entry uses base_reach_m (256 by default), farther than 100 m.
    let fallback = AnchorSettings {
        tiers: vec![reach(256.0, 100.0)],
        ..AnchorSettings::default()
    };
    assert_eq!(plan.check_reach(&fallback).len(), 1);
}

/// `in.self → sample.region → <node> → out.points` (or `in.self → sample.poisson → out.points`).
fn reach_of(ty: &str, settings: &[(&str, &str)]) -> f64 {
    let g = if ty == "sample.poisson" {
        graph(
            vec![
                (1, node("in.self", &[])),
                (2, node(ty, settings)),
                (3, node("out.points", &[("name", "\"p\"")])),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in")],
        )
    } else {
        graph(
            vec![
                (1, node("in.self", &[])),
                (2, node("sample.region", &[("spacing_m", "10.0")])),
                (3, node(ty, settings)),
                (4, node("out.points", &[("name", "\"p\"")])),
            ],
            &[(1, "out", 2, "region"), (2, "out", 3, "in"), (3, "out", 4, "in")],
        )
    };
    TierPlan::build(&compile(&g), 64.0)
        .unwrap_or_else(|e| panic!("{ty}: {:?}", e.0))
        .reach_m
}

/// § 2 declared reach: `points.neighbors` → `radius_m`; `sample.poisson` → `2 · min_dist_m`;
/// `points.prune` → `2 × max_footprint_m` (the bound on the half-diagonals it sees);
/// `biome.resolve` → `2 × max_core_m` (ADR-2003 (12)); most nodes 0. The default halo is twice the
/// reach, at least 4 m; an explicit `halo_m` wins.
#[test]
fn s66_declared_reach_per_node_and_the_default_halo() {
    assert_eq!(reach_of("points.neighbors", &[("radius_m", "7.0")]), 7.0);
    assert_eq!(reach_of("sample.poisson", &[("min_dist_m", "3.0")]), 6.0);
    assert_eq!(reach_of("points.prune", &[("max_footprint_m", "4.0")]), 8.0);
    assert_eq!(reach_of("biome.resolve", &[("max_core_m", "5.0")]), 10.0);
    assert_eq!(reach_of("points.jitter", &[]), 0.0);
    // Defaults: prune 8 m footprint → 16 m reach.
    assert_eq!(reach_of("points.prune", &[]), 16.0);
    let auto = TileSettings::default();
    assert_eq!(auto.halo(7.0), 14.0, "twice the reach");
    assert_eq!(auto.halo(1.0), 4.0, "at least 4 m");
    assert_eq!(auto.halo(0.0), 4.0);
    let fixed = TileSettings {
        halo_m: Some(3.0),
        ..TileSettings::default()
    };
    assert_eq!(fixed.halo(7.0), 3.0, "an explicit halo wins");
}

/// § 2: `cell_m` defaults to 128 and is a power of two in meters between 8 and 4096
/// (`tile_cell_invalid` otherwise); a non-finite or negative halo is refused too.
#[test]
fn s66_cell_size_must_be_a_power_of_two_between_8_and_4096() {
    let d = TileSettings::default();
    assert_eq!((d.cell_m, d.halo_m, d.runtime.is_none()), (128.0, None, true));
    let with = |cell_m: f32| TileSettings {
        cell_m,
        ..TileSettings::default()
    };
    for ok in [8.0, 16.0, 64.0, 128.0, 1024.0, 4096.0] {
        assert!(with(ok).validate().is_ok(), "{ok}");
    }
    for bad in [4.0, 8192.0, 96.0, 100.0, 0.5, 0.0, -64.0, f32::NAN, f32::INFINITY] {
        let err = with(bad).validate().map_err(|e| e.0);
        assert_eq!(err, Err("tile_cell_invalid".to_string()), "{bad}");
    }
    for bad in [f32::NAN, -1.0] {
        let t = TileSettings {
            halo_m: Some(bad),
            ..TileSettings::default()
        };
        assert!(t.validate().is_err(), "halo {bad}");
    }
}

// ------------------------------------------------------------------------------------------------
// § 4.1 / § 4.2 / § 4.3 / § 5 defaults
// ------------------------------------------------------------------------------------------------

/// § 4.1: `ProcGenAnchor::default()` is reach scale 1, view direction on, format version 1.
#[test]
fn s66_anchor_component_defaults() {
    let a = ProcGenAnchor::default();
    assert_eq!(
        (a.reach_scale, a.use_view_direction, a.format_version),
        (1.0, true, 1)
    );
}

/// § 4.2, § 4.3, § 5 and ADR-1585: the documented defaults, exactly. The platform defaults are
/// the desktop ones on every target but Android.
#[test]
fn s66_anchor_settings_defaults_match_the_spec() {
    let a = AnchorSettings::default();
    assert_eq!(a.anchors, AnchorSource::PrimaryView);
    assert_eq!(a.base_reach_m, 256.0);
    assert!(a.tiers.is_empty());
    assert_eq!(a.pool_cells, 64);
    assert!(a.wait_on_load);
    assert_eq!(a.quality_reach, [0.5, 0.75, 1.0, 1.25]);
    assert_eq!(a.apply_budget_elements, None);
    if cfg!(target_os = "android") {
        assert_eq!((a.max_new(), a.max_running(), a.resident_mb()), (4, 2, 128));
        assert_eq!(a.apply_ms(), 0.5);
    } else {
        assert_eq!((a.max_new(), a.max_running(), a.resident_mb()), (8, 4, 512));
        assert_eq!(a.apply_ms(), 1.0);
    }
    let explicit = AnchorSettings {
        apply_budget_ms: Some(2.5),
        max_resident_mb: Some(64),
        ..AnchorSettings::default()
    };
    assert_eq!((explicit.apply_ms(), explicit.resident_mb()), (2.5, 64));
    let r = TierReach::default();
    assert_eq!((r.release_factor, r.view_bias, r.view_gate), (1.25, 0.5, None));
    // Release factor clamps to ≥ 1.1 where it is read.
    let low = TierReach {
        release_factor: 1.0,
        ..TierReach::default()
    };
    assert!((low.release_factor() - 1.1).abs() < 1e-6);
    // ADR-1585: the seven quality tiers onto the four reach slots.
    for (tier, slot) in [
        (QualityTier::MobileLow, 0),
        (QualityTier::MobileHigh, 0),
        (QualityTier::Low, 0),
        (QualityTier::Deck, 1),
        (QualityTier::Medium, 1),
        (QualityTier::High, 2),
        (QualityTier::Ultra, 3),
    ] {
        assert_eq!(quality_reach_index(tier), slot, "{tier:?}");
    }
    assert_eq!(a.quality_scale(QualityTier::Deck), 0.75);
}

/// § 4.2: the view gate's release margin is clamped to at least its build margin, so a gate whose
/// release margin is smaller does not release a cell it just built.
#[test]
fn s66_a_view_gate_release_margin_below_the_build_margin_is_clamped() {
    use regia_core::math::Frustum;
    let eye = Vec3::new(0.0, 2.0, 0.0);
    let view = regia_core::glam::camera::rh::view::look_to_mat4(eye, Vec3::X, Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(60f32.to_radians(), 1.0, 0.1, 5000.0);
    let cam = AnchorView {
        pos: eye,
        view_dir: Some(Vec3::X),
        frustum: Some(Frustum::from_view_projection(&(proj * view))),
        reach_scale: 1.0,
    };
    let mut s = one_tier_settings(400.0);
    s.tiers[0].view_gate = Some(regia_procgen::tiles::ViewGate {
        build_margin_deg: 20.0,
        release_margin_deg: 0.0,
    });
    let mut g = one_tier();
    let plan = g.step(&[cam], &s, &mut ApplyBudget::default());
    finish_started(&mut g, &plan);
    for step in 0..5 {
        let plan = g.step(&[cam], &s, &mut ApplyBudget::default());
        assert!(plan.release.is_empty(), "step {step}: {:?}", plan.release);
        finish_started(&mut g, &plan);
    }
}

/// § 2: terrain edits are not allowed in tiled generators with anchors (no bake step owns the
/// terrain); a baked tiled generator without anchors keeps the ordinary spec 64 § 7.3 gate.
#[test]
fn s66_anchored_tiled_generators_may_not_edit_terrain() {
    use regia_procgen::exec::TerrainEditGate;
    use regia_procgen::world::{ProcGenMode, ProcGenSettings, ProcGenerator, terrain_edit_gate};
    let settings = ProcGenSettings {
        terrain_edits: true,
        ..ProcGenSettings::default()
    };
    let tiled = |runtime: Option<AnchorSettings>| ProcGenerator {
        mode: ProcGenMode::Baked,
        allow_terrain_edits: true,
        tiles: Some(TileSettings {
            runtime,
            ..TileSettings::default()
        }),
        ..ProcGenerator::new("g.pgraph.ron")
    };
    assert_eq!(
        terrain_edit_gate(&tiled(None), &settings),
        TerrainEditGate::Allowed,
        "baked tiled, no anchors: allowed by the generator and the project"
    );
    assert_eq!(
        terrain_edit_gate(&tiled(Some(AnchorSettings::default())), &settings),
        TerrainEditGate::NotBaked,
        "baked tiled streaming around anchors"
    );
    let anchored = ProcGenerator {
        mode: ProcGenMode::Anchored,
        ..tiled(Some(AnchorSettings::default()))
    };
    assert_eq!(terrain_edit_gate(&anchored, &settings), TerrainEditGate::NotBaked);
}
