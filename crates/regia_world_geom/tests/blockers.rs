// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/tests.rs and ir-sim/src/pathing/layer.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines tests.rs 1195-1238 and layer.rs 305-324, mark: requirements (Stage B).
// Edits: the collapse proof re-expressed on the cooked section graph (sections as nav cells, W-08's portals) instead of
// the centreline tunnel nodes; the closed set is read from the blocker bits; no DO27 code or identifiers kept.
//! Section blocker plugs (spec 121 § 5.3 rule 6, § 5.7, § 13.3 "Blocker generation"; plan row W-13).
//!
//! The Switchbacks fixture on the canyon floor, cooked with a collapsible run of the gallery (and, for the two-peer
//! test, a second run of the tube). `set_blocker` flips one bit of `GeomState`; the query candidates (`support`,
//! `floors_at`, `segment`, `push_disc`), the nav cells (`section_open`, `cell_route`) and the sound occlusion
//! (`rock_between`) all change at the generation it returns, and revert at the next one.
#![cfg(all(feature = "stage_b", feature = "fixtures"))]
#![allow(clippy::unwrap_used)]

use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::cooked::{CookedPlug, TriKind};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::{
    Band, BlockerId, EditStats, FaceMask, Generation, GeomEditError, GeomState, Push, SectionId, SegmentHit,
    SpanBuf, SurfaceKind, WorldGeom,
};

const GALLERY: &str = "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0)]";
/// The gallery run and a tube run between the two junctions: together they cut every underground way from the west
/// ramps to the east ones.
const BETWEEN: &str = "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0), (passage: \"tube\", from_s_m: 780.0, to_s_m: 810.0)]";
const TWO: &str = "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0), (passage: \"tube\", from_s_m: 200.0, to_s_m: 230.0)]";

fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

fn canyon_ground() -> GroundInput {
    GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    }
}

/// The Switchbacks fixture cooked with `layer_extra` on its one `Passages` layer, loaded.
fn cook_geom(layer_extra: &str) -> WorldGeom {
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let doc = format!(
        "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"), {layer_extra})])"
    );
    let read = |p: &str| {
        if p == "map.passages.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{p}: not found"))
        }
    };
    let terrain = canyon_terrain();
    let ground = canyon_ground();
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // ADR-8359: a test cook waives the two known Switchbacks ramp facets by hash and recooks.
    let out = match run(&doc) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&doc, &report).unwrap())
        }
        other => other,
    }
    .unwrap();
    WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap()
}

/// Points around one plug: the centroid of its top (an up-facing triangle), and two points on the tube axis at the
/// end faces' mid height, one metre outside either end face.
#[derive(Clone, Copy, Debug)]
struct Probe {
    top: [f32; 3],
    a: [f32; 3],
    b: [f32; 3],
}

fn probe(geom: &WorldGeom, plug: &CookedPlug) -> Probe {
    let tris: Vec<_> = geom
        .triangles()
        .into_iter()
        .filter(|t| {
            t.tri.layer == plug.triangles.layer
                && (plug.triangles.start..plug.triangles.end).contains(&t.tri.triangle)
        })
        .collect();
    assert_eq!(tris.len(), 6, "the plug is two end faces and a top");
    assert!(tris.iter().all(|t| t.kind == TriKind::Blocker));
    let centre = |vs: &[[f32; 3]]| {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for v in vs {
            for k in 0..3 {
                lo[k] = regia_detsim::float::det_min(lo[k], v[k]);
                hi[k] = regia_detsim::float::det_max(hi[k], v[k]);
            }
        }
        [
            (lo[0] + hi[0]) * 0.5,
            (lo[1] + hi[1]) * 0.5,
            (lo[2] + hi[2]) * 0.5,
        ]
    };
    let face = |i: usize| centre(&[tris[i].v, tris[i + 1].v].concat());
    let (e0, e1) = (face(0), face(2));
    let top = {
        let v = tris[4].v;
        [
            (v[0][0] + v[1][0] + v[2][0]) / 3.0,
            (v[0][1] + v[1][1] + v[2][1]) / 3.0,
            (v[0][2] + v[1][2] + v[2][2]) / 3.0,
        ]
    };
    let d = [e1[0] - e0[0], e1[1] - e0[1], e1[2] - e0[2]];
    let len = regia_detsim::math::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
    assert!(len > 1.0, "the plug is {len} m long");
    let u = d.map(|c| c / len);
    Probe {
        top,
        a: [e0[0] - u[0], e0[1] - u[1], e0[2] - u[2]],
        b: [e1[0] + u[0], e1[1] + u[1], e1[2] + u[2]],
    }
}

/// What every consumer sees at one state, around one plug.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    /// `floors_at` under the plug's top has a `Blocker` floor.
    column_has_plug: bool,
    /// `support` just over the plug's top: its floor kind and height.
    support: (SurfaceKind, f32),
    /// `segment(a -> b, SOLID)`.
    segment: Option<SegmentHit>,
    /// `rock_between(a, b)`: the sound occlusion (DO27 `rock_between`, spec 121 § 5.5).
    rock: Option<f32>,
    /// `line_of_sight(a, b)`.
    sight: bool,
    /// A 0.5 m disc on the axis, 0.3 m before the first end face.
    push: Push,
    /// The section is open to nav, and its closed set.
    open: bool,
    closed: Vec<SectionId>,
}

fn observe(geom: &WorldGeom, state: &GeomState, plug: &CookedPlug, p: &Probe) -> Seen {
    let mut spans = SpanBuf::default();
    geom.floors_at(state, p.top[0], p.top[2], &mut spans);
    let s = geom.support(state, p.top[0], p.top[2], p.top[1] + 0.05);
    let dir = [p.b[0] - p.a[0], p.b[2] - p.a[2]];
    let dl = regia_detsim::math::sqrt(dir[0] * dir[0] + dir[1] * dir[1]);
    let at = [p.a[0] + dir[0] / dl * 0.7, p.a[2] + dir[1] / dl * 0.7];
    let mut closed = Vec::new();
    geom.closed_sections(state, &mut closed);
    Seen {
        column_has_plug: spans.spans().iter().any(|s| s.floor.kind == SurfaceKind::Blocker),
        support: (s.floor.kind, s.floor.y),
        segment: geom.segment(state, p.a, p.b, FaceMask::SOLID),
        rock: geom.rock_between(state, p.a, p.b),
        sight: geom.line_of_sight(state, p.a, p.b),
        push: geom.push_disc(
            state,
            at[0],
            at[1],
            0.5,
            Band {
                lo: p.a[1] - 0.5,
                hi: p.a[1] + 0.5,
            },
        ),
        open: geom.section_open(state, SectionId(plug.section)),
        closed,
    }
}

/// FNV-1a-64 of a sequence of u64 words (little-endian), for the pins below.
fn fnv(words: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Spec 121 § 13.3 "Blocker generation" (W-13): after `set_blocker(s, true)` the plug's triangles are `support`,
/// `floors_at`, `segment` and `push_disc` candidates, the section is closed to nav, and the sound occlusion sees rock,
/// all at the generation the call returns; `set_blocker(s, false)` reverts all of them at the next generation. Fails on
/// the base: the plug was never a candidate and `set_blocker` did not exist.
#[test]
fn a_blocker_closes_and_reopens_in_one_generation() {
    let geom = cook_geom(GALLERY);
    let plug = geom.blockers()[0];
    let id = BlockerId(plug.id);
    let p = probe(&geom, &plug);
    let mut state = geom.new_state();
    assert!(!state.blocker_active(id));
    let listed = |state: &GeomState| state.blockers(&geom).collect::<Vec<_>>();
    let status = |active| {
        vec![regia_world_geom::BlockerStatus {
            id,
            section: SectionId(plug.section),
            active,
        }]
    };
    assert_eq!(listed(&state), status(false));
    let open = observe(&geom, &state, &plug, &p);
    println!("open:   {open:?}");
    assert!(!open.column_has_plug && open.support.0 != SurfaceKind::Blocker);
    assert_eq!(
        open.segment, None,
        "the tube axis is clear while the plug is clear"
    );
    assert_eq!(open.rock, None);
    assert!(open.sight && open.open && open.closed.is_empty());
    assert_eq!(open.push.contacts, 0);
    let h0 = state.state_hash();

    assert_eq!(state.set_blocker(&geom, id, true), Ok(Generation(1)));
    assert_eq!(state.generation(), Generation(1));
    assert!(state.blocker_active(id));
    assert_eq!(
        listed(&state),
        status(true),
        "the agent view (terrain.blockers) reads the same bit"
    );
    let shut = observe(&geom, &state, &plug, &p);
    println!("closed: {shut:?}");
    assert!(shut.column_has_plug, "the plug's top is a floor");
    assert_eq!(shut.support.0, SurfaceKind::Blocker, "support stands on the plug");
    let hit = shut.segment.expect("the segment meets the plug");
    assert_eq!(hit.kind, SurfaceKind::Blocker);
    assert!(
        hit.tri
            .is_some_and(|t| (plug.triangles.start..plug.triangles.end).contains(&t.triangle))
    );
    assert!(shut.rock.is_some(), "the sound occlusion sees rock");
    assert!(!shut.sight);
    assert!(shut.push.contacts > 0, "the end face is a wall");
    assert!(!shut.open);
    assert_eq!(shut.closed, vec![SectionId(plug.section)]);
    for &port in &geom.sections()[usize::from(plug.section)].portals {
        assert!(!geom.portal_open(&state, regia_world_geom::PortalId(port)));
    }
    let h1 = state.state_hash();

    // A no-op flip changes nothing.
    assert_eq!(state.set_blocker(&geom, id, true), Ok(Generation(1)));
    assert_eq!(state.state_hash(), h1);

    assert_eq!(state.set_blocker(&geom, id, false), Ok(Generation(2)));
    let back = observe(&geom, &state, &plug, &p);
    assert_eq!(back, open, "every consumer reverts at the next generation");
    let h2 = state.state_hash();
    assert!(
        h0 != h1 && h1 != h2 && h0 != h2,
        "the generation is part of the hash"
    );

    // An unknown id is an error with its agent code, and changes nothing.
    let before = state.clone();
    let err = state.set_blocker(&geom, BlockerId(7), true).unwrap_err();
    assert_eq!(err, GeomEditError::UnknownBlocker(BlockerId(7)));
    assert_eq!(err.code(), "blocker_unknown");
    assert_eq!(state, before);

    // Pins (spec 121 § 14 rule 1), recorded on x86-64 Linux: the three state hashes and the closed answers' bits.
    let bits = [
        h0,
        h1,
        h2,
        u64::from(shut.support.1.to_bits()),
        u64::from(hit.t.to_bits()),
        u64::from(shut.rock.unwrap().to_bits()),
        u64::from(shut.push.xz[0].to_bits()) << 32 | u64::from(shut.push.xz[1].to_bits()),
    ];
    println!("pins: {bits:#018x?} fnv {:#018x}", fnv(&bits));
    assert_eq!(h0, regia_world_geom::edit::FRESH_STATE_HASH);
    assert_eq!(
        fnv(&bits),
        PIN_CLOSE_REOPEN,
        "a blocker answer moved: a § 14 bit change"
    );
}
const PIN_CLOSE_REOPEN: u64 = 0x593ff98f5970744c;

/// A query table over the plugs' probes and a coarse grid of the map: every column (`floors_at`), `support` at three
/// heights, the segment, rock and push answers, folded to one hash.
fn table(geom: &WorldGeom, state: &GeomState, probes: &[Probe]) -> u64 {
    let mut words = Vec::new();
    let mut pts: Vec<[f32; 3]> = Vec::new();
    for p in probes {
        pts.extend([p.top, p.a, p.b]);
    }
    for i in 0..24 {
        for j in 0..24 {
            pts.push([-500.0 + i as f32 * 41.7, -250.0, -500.0 + j as f32 * 41.7]);
        }
    }
    let mut spans = SpanBuf::default();
    for q in &pts {
        geom.floors_at(state, q[0], q[2], &mut spans);
        for s in spans.spans() {
            words.push(u64::from(s.floor.y.to_bits()) << 8 | s.floor.kind as u64);
        }
        for dy in [-1.0, 0.05, 1.0] {
            let s = geom.support(state, q[0], q[2], q[1] + dy);
            words.push(u64::from(s.floor.y.to_bits()) << 8 | s.floor.kind as u64);
        }
    }
    for p in probes {
        let seg = geom.segment(state, p.a, p.b, FaceMask::SOLID);
        words.push(seg.map_or(u64::MAX, |h| u64::from(h.t.to_bits())));
        words.push(
            geom.rock_between(state, p.a, p.b)
                .map_or(u64::MAX, |t| u64::from(t.to_bits())),
        );
        let push = geom.push_disc(
            state,
            p.a[0],
            p.a[2],
            0.5,
            Band {
                lo: p.a[1] - 0.5,
                hi: p.a[1] + 0.5,
            },
        );
        words.push(u64::from(push.xz[0].to_bits()) << 32 | u64::from(push.xz[1].to_bits()));
    }
    fnv(&words)
}

/// Two peers fed the same blocker flips at the same ticks have equal `state_hash` every tick and equal query tables
/// after, though each lists a tick's flips in its own order: `apply_blocker_flips` applies them in ascending section.
#[test]
fn two_peers_checksum_equal_across_a_close_and_reopen() {
    let geom = cook_geom(TWO);
    assert_eq!(geom.blockers().len(), 2);
    let (b0, b1) = (BlockerId(0), BlockerId(1));
    assert!(
        geom.blockers()[0].section < geom.blockers()[1].section,
        "ids follow sections"
    );
    let probes: Vec<Probe> = geom.blockers().iter().map(|p| probe(&geom, p)).collect();
    let fresh = geom.new_state();
    let fresh_table = table(&geom, &fresh, &probes);
    // Tick 11 lists b0 twice: `false` sorts before `true`, so it ends active whichever way round it was listed.
    let schedule: [&[(BlockerId, bool)]; 14] = [
        &[],
        &[],
        &[(b1, true), (b0, true)],
        &[],
        &[],
        &[(b0, false)],
        &[],
        &[(b1, false), (b0, true)],
        &[],
        &[(b0, false), (b1, true)],
        &[(b1, false)],
        &[(b0, true), (b0, false)],
        &[(b0, false)],
        &[],
    ];
    let (mut a, mut b, mut c) = (fresh.clone(), fresh.clone(), fresh.clone());
    let mut hashes = Vec::new();
    for (tick, flips) in schedule.iter().enumerate() {
        let mut fa = flips.to_vec();
        let mut fb: Vec<_> = flips.iter().rev().copied().collect();
        a.apply_blocker_flips(&geom, &mut fa).unwrap();
        b.apply_blocker_flips(&geom, &mut fb).unwrap();
        // The fixed order by hand: ascending section, then id, then false before true.
        let mut sorted = flips.to_vec();
        sorted.sort_by_key(|f| (geom.blockers()[usize::from(f.0.0)].section, f.0, f.1));
        for (id, on) in sorted {
            c.set_blocker(&geom, id, on).unwrap();
        }
        assert_eq!(a.state_hash(), b.state_hash(), "tick {tick}");
        assert_eq!(a.state_hash(), c.state_hash(), "tick {tick}");
        assert_eq!(a, b, "tick {tick}");
        hashes.push(a.state_hash());
    }
    assert_eq!(a.generation(), Generation(10));
    assert!(!a.blocker_active(b0) && !a.blocker_active(b1));
    let (ta, tb) = (table(&geom, &a, &probes), table(&geom, &b, &probes));
    assert_eq!(ta, tb);
    assert_eq!(ta, fresh_table, "every plug clear again: the fresh answers");
    // Mid-schedule both closed: the tables agree and differ from the fresh one.
    let mut both = fresh.clone();
    both.apply_blocker_flips(&geom, &mut [(b1, true), (b0, true)])
        .unwrap();
    let mut both2 = fresh.clone();
    both2
        .apply_blocker_flips(&geom, &mut [(b0, true), (b1, true)])
        .unwrap();
    assert_eq!(table(&geom, &both, &probes), table(&geom, &both2, &probes));
    assert_ne!(table(&geom, &both, &probes), fresh_table);
    // An unknown id anywhere in the tick applies nothing of it.
    let before = a.clone();
    assert_eq!(
        a.apply_blocker_flips(&geom, &mut [(b0, true), (BlockerId(9), true)]),
        Err(GeomEditError::UnknownBlocker(BlockerId(9)))
    );
    assert_eq!(a, before);
    println!(
        "hashes: {hashes:#018x?} fnv {:#018x} table {fresh_table:#018x}",
        fnv(&hashes)
    );
    assert_eq!(
        fnv(&hashes),
        PIN_TWO_PEERS,
        "the blocker hash sequence moved: a § 14 rule 5 change"
    );
}
const PIN_TWO_PEERS: u64 = 0x39e0944768d96e04;

/// The surface penalty of the route below (DO27's planner charges the open map more than a passage).
const OUTDOOR_PCT: u32 = 300;

/// E4: a flip touches one blocker word and one hash term, however many plugs or flips; `state_hash` itself reads the
/// running accumulator (O(1)). The snapshot of a closed state restores the same bits and answers.
#[test]
fn blocker_flip_is_o1_in_state_hash() {
    let geom = cook_geom(TWO);
    let mut state = geom.new_state();
    let mut total = EditStats::default();
    for k in 0..10_000u32 {
        let id = BlockerId((k % 2) as u16);
        let before = state.blocker_words().to_vec();
        let mut stats = EditStats::default();
        let on = !state.blocker_active(id);
        state.set_blocker_with_stats(&geom, id, on, &mut stats).unwrap();
        assert_eq!(stats.hash_terms, 1, "flip {k}");
        let changed = before
            .iter()
            .zip(state.blocker_words())
            .filter(|(x, y)| x != y)
            .count();
        assert_eq!(changed, 1, "flip {k}: one word");
        total.hash_terms += stats.hash_terms;
    }
    assert_eq!(total.hash_terms, 10_000);
    assert_eq!(state.generation(), Generation(10_000));
    // An even number of flips of each: the bits are fresh, so the hash is the fresh accumulator at this generation.
    assert_eq!(state.blocker_words(), geom.new_state().blocker_words());
    let mut stats = EditStats::default();
    state
        .set_blocker_with_stats(&geom, BlockerId(1), true, &mut stats)
        .unwrap();
    let mut bytes = Vec::new();
    state.encode(&mut bytes);
    let back = GeomState::decode(&geom, &bytes).unwrap();
    assert_eq!(back, state);
    assert_eq!(back.state_hash(), state.state_hash());
    let p = probe(&geom, &geom.blockers()[1]);
    assert_eq!(
        observe(&geom, &back, &geom.blockers()[1], &p),
        observe(&geom, &state, &geom.blockers()[1], &p)
    );
}

/// DO27's collapse proof (requirements, `pathing/tests.rs` 1195-1238), Stage B form on the cooked fixture. DO27 closes
/// every tube node and the planner goes round on the surface; clearing reopens the passage. Here: the route from the
/// west ramp's foot to the east ramp's foot runs underground; with the plugs of the gallery and of the tube between the
/// junctions active, those sections are closed (`closed_sections` is the whole closed list, ascending, as DO27's
/// `closed()`) and the route goes round on the surface; cleared, the same underground route is back. A gallery plug
/// alone sends the route round underground, through the tube. Run on the cooked section graph (sections as nav cells,
/// W-08's portals): neither W-20's layered nav nor S-06's navmesh is on this line yet.
#[test]
fn a_collapse_closes_its_passage_and_clearing_reopens_it_stage_b() {
    let cells = |r: &[regia_world_geom::CellStep]| r.iter().map(|c| c.cell).collect::<Vec<_>>();
    // The foot of a ramp: its section with a portal into the tube.
    let foot = |geom: &WorldGeom, ramp: &str| {
        let s = geom
            .sections()
            .iter()
            .find(|s| {
                s.source.ends_with(ramp)
                    && s.portals.iter().any(|&p| {
                        geom.portals()[usize::from(p)]
                            .sides
                            .iter()
                            .flatten()
                            .any(|&o| geom.sections()[usize::from(o)].source.ends_with("/tube"))
                    })
            })
            .unwrap();
        SectionId(s.id)
    };

    // Both plugs: no way underground.
    let geom = cook_geom(BETWEEN);
    assert_eq!(geom.blockers().len(), 2);
    let (from, to) = (foot(&geom, "/ramp_west"), foot(&geom, "/ramp_east"));
    let mut state = geom.new_state();
    let first = geom
        .cell_route(&state, Some(from), Some(to), OUTDOOR_PCT)
        .expect("a route");
    println!("open route: {:?}", cells(&first));
    assert!(
        first.iter().all(|c| c.cell.is_some()),
        "through the passage first"
    );
    let plugged: Vec<SectionId> = geom.blockers().iter().map(|p| SectionId(p.section)).collect();
    state
        .apply_blocker_flips(&geom, &mut [(BlockerId(1), true), (BlockerId(0), true)])
        .unwrap();
    let mut closed = Vec::new();
    geom.closed_sections(&state, &mut closed);
    let mut want = plugged.clone();
    want.sort_unstable();
    assert_eq!(closed, want, "whole-list, sorted");
    let round = geom
        .cell_route(&state, Some(from), Some(to), OUTDOOR_PCT)
        .expect("a way round");
    println!("closed route: {:?}", cells(&round));
    assert!(
        round.iter().all(|c| c.cell.is_none_or(|s| !plugged.contains(&s))),
        "never through the rubble"
    );
    assert!(
        round.iter().any(|c| c.cell.is_none()),
        "no way underground: round on the surface"
    );
    for &s in &plugged {
        assert_eq!(
            geom.cell_route(&state, Some(s), Some(to), OUTDOOR_PCT),
            None,
            "a closed cell is not entered or left"
        );
    }
    state
        .apply_blocker_flips(&geom, &mut [(BlockerId(0), false), (BlockerId(1), false)])
        .unwrap();
    geom.closed_sections(&state, &mut closed);
    assert!(closed.is_empty(), "blown clear");
    assert_eq!(
        geom.cell_route(&state, Some(from), Some(to), OUTDOOR_PCT),
        Some(first),
        "through the passage again"
    );

    // The gallery plug alone: the route between the gallery sections either side goes round through the tube.
    let geom = cook_geom(GALLERY);
    let plug = geom.blockers()[0];
    let s = SectionId(plug.section);
    let neighbours: Vec<SectionId> = geom.sections()[usize::from(s.0)]
        .portals
        .iter()
        .flat_map(|&p| geom.portals()[usize::from(p)].sides)
        .flatten()
        .map(SectionId)
        .filter(|&n| n != s)
        .collect();
    assert_eq!(neighbours.len(), 2, "{neighbours:?}");
    let mut state = geom.new_state();
    let direct = geom
        .cell_route(&state, Some(neighbours[0]), Some(neighbours[1]), OUTDOOR_PCT)
        .unwrap();
    assert_eq!(
        cells(&direct),
        vec![Some(neighbours[0]), Some(s), Some(neighbours[1])]
    );
    state.set_blocker(&geom, BlockerId(plug.id), true).unwrap();
    let detour = geom
        .cell_route(&state, Some(neighbours[0]), Some(neighbours[1]), OUTDOOR_PCT)
        .unwrap();
    assert!(
        detour.iter().all(|c| c.cell.is_some() && c.cell != Some(s)),
        "{:?}",
        cells(&detour)
    );
    assert!(detour.iter().any(|c| {
        c.cell
            .is_some_and(|c| geom.sections()[usize::from(c.0)].source.ends_with("/tube"))
    }));
    state.set_blocker(&geom, BlockerId(plug.id), false).unwrap();
    assert_eq!(
        geom.cell_route(&state, Some(neighbours[0]), Some(neighbours[1]), OUTDOOR_PCT),
        Some(direct)
    );
}
