// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/tests.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 718-767 (`two_hulls_pass_each_other_in_the_tube`, `a_group_goes_through_a_junction_without_wedging`) and
// 768-786 (`tunnel_runs_give_the_same_hash`), mark: requirements (Stage B, A-10). No DO27 code: the requirements are
// re-expressed on the layered bake's plans (two head-on hulls in every passage, a group of eight through every
// junction, a two-sided run hash), stepped by a fixed integer mover instead of DO27's sim. The file is byte-identical
// between regia-absorb-2 (e6c84ba6d), regia-absorb-3 and regia-absorb-4.

//! The deterministic layered runtime (plan row W-20, ADR-8486): plans on the layered bake of the
//! cooked Switchbacks and Cinder Steps fixtures, the blocker and ground-edit feed, the bake's bytes
//! and the pins.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::OnceLock;

use regia_detsim::golden::{self, DetRun, DetScenario, GoldenMode, REPORT_PREFIX, StateHasher};
use regia_detsim::tick::TickRate;
use regia_nav::det::layers::{LayerAgent, LayerPlan, LayerScratch, LayerState, LayeredBake, fnv1a64, mm};
use regia_nav::layered::geom::{LayeredGeomSettings, cook_layered, sync_layers};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::{Passage, TunnelLayout};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookInputs, GroundInput, cook};
use regia_world_geom::edit::GroundEdit;
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::{BlockerId, GeomState, SectionId, WorldGeom};

/// The hull radius of the requirements (DO27's battle hull), meters.
const HULL_R: f32 = 4.5;
/// DO27's passing bar: centers at least `2 r - 0.5` m apart.
const PASS_TOL_MM: i64 = 500;

const GALLERY: &str = "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0)]";

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

/// The fixture `map` cooked with `layer_extra` on its one `Passages` layer, loaded.
fn cook_geom(map: &str, layer_extra: &str) -> WorldGeom {
    let passages = PassagesDocument::from_layout(for_map(map).unwrap())
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
    let run = |doc: &str| {
        cook(&CookInputs {
            document: doc.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // ADR-8359/ADR-8436: a test cook waives the known Switchbacks ramp facets by hash and recooks (the
    // waiver leaves the cooked bytes alone; merged with W-09's checks by native-acceptance-suite).
    let out = match run(&doc) {
        Err(regia_world_geom::cook::CookError::Checks(report))
            if regia_world_geom::checks::only_ramp_facet_failures(&report) =>
        {
            run(&regia_world_geom::checks::waive_ramp_facets(&doc, &report).unwrap())
        }
        other => other,
    }
    .unwrap();
    WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap()
}

/// The bake bounds of a layout: its passages' points, padded by the widest passage.
fn bounds(layout: &TunnelLayout) -> [f32; 4] {
    let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    let mut pad = 0.0f32;
    for p in &layout.passages {
        pad = regia_detsim::float::det_max(pad, p.width_m);
        for q in &p.points {
            b[0] = regia_detsim::float::det_min(b[0], q[0]);
            b[1] = regia_detsim::float::det_min(b[1], q[2]);
            b[2] = regia_detsim::float::det_max(b[2], q[0]);
            b[3] = regia_detsim::float::det_max(b[3], q[2]);
        }
    }
    [b[0] - pad, b[1] - pad, b[2] + pad, b[3] + pad]
}

struct Map {
    layout: TunnelLayout,
    geom: WorldGeom,
    bake: LayeredBake,
}

fn baked(map: &str, layer_extra: &str) -> Map {
    let layout = for_map(map).unwrap();
    let geom = cook_geom(map, layer_extra);
    let state = geom.new_state();
    let settings = LayeredGeomSettings::hull(HULL_R, bounds(&layout));
    let (_, bake) = cook_layered(&geom, &state, &settings).unwrap();
    Map { layout, geom, bake }
}

fn switchbacks() -> &'static Map {
    static M: OnceLock<Map> = OnceLock::new();
    M.get_or_init(|| baked("switchbacks", ""))
}

fn switchbacks_gallery_plug() -> &'static Map {
    static M: OnceLock<Map> = OnceLock::new();
    M.get_or_init(|| baked("switchbacks", GALLERY))
}

fn cinder() -> &'static Map {
    static M: OnceLock<Map> = OnceLock::new();
    M.get_or_init(|| baked("cinder_steps", ""))
}

/// The passage's centerline length, meters.
fn length(p: &Passage) -> f32 {
    p.points
        .windows(2)
        .map(|w| {
            let (dx, dz) = (w[1][0] - w[0][0], w[1][2] - w[0][2]);
            regia_detsim::math::sqrt(dx * dx + dz * dz)
        })
        .sum()
}

/// The centerline point `s` meters from the passage's first point (clamped), millimeters.
fn along(p: &Passage, s: f32) -> [i32; 3] {
    let mut left = regia_detsim::float::det_max(s, 0.0);
    for w in p.points.windows(2) {
        let (dx, dy, dz) = (w[1][0] - w[0][0], w[1][1] - w[0][1], w[1][2] - w[0][2]);
        let l = regia_detsim::math::sqrt(dx * dx + dz * dz);
        if left <= l || l <= 0.0 {
            let t = if l > 0.0 { left / l } else { 0.0 };
            return [mm(w[0][0] + dx * t), mm(w[0][1] + dy * t), mm(w[0][2] + dz * t)];
        }
        left -= l;
    }
    let q = p.points[p.points.len() - 1];
    [mm(q[0]), mm(q[1]), mm(q[2])]
}

/// The arc position, meters, of the centerline point nearest `q` (mm).
fn arc_of(p: &Passage, q: [i32; 3]) -> f32 {
    let (x, z) = (q[0] as f32 / 1000.0, q[2] as f32 / 1000.0);
    let mut best = (f32::MAX, 0.0f32);
    let mut base = 0.0f32;
    for w in p.points.windows(2) {
        let (dx, dz) = (w[1][0] - w[0][0], w[1][2] - w[0][2]);
        let l2 = dx * dx + dz * dz;
        let l = regia_detsim::math::sqrt(l2);
        let t = if l2 > 0.0 {
            ((x - w[0][0]) * dx + (z - w[0][2]) * dz) / l2
        } else {
            0.0
        };
        let t = regia_detsim::float::det_clamp(t, 0.0, 1.0);
        let (px, pz) = (w[0][0] + dx * t, w[0][2] + dz * t);
        let d = (x - px) * (x - px) + (z - pz) * (z - pz);
        if d < best.0 {
            best = (d, base + l * t);
        }
        base += l;
    }
    best.1
}

/// The plan's corners resampled every 0.5 m (xz millimeters).
fn samples(plan: &LayerPlan) -> Vec<[i32; 3]> {
    let mut out = Vec::new();
    for w in plan.waypoints.windows(2) {
        let (a, b) = (w[0], w[1]);
        let d = (((i64::from(b[0] - a[0])).pow(2) + (i64::from(b[2] - a[2])).pow(2)) as u64).isqrt() as i64;
        let n = (d / 500).max(1);
        for k in 0..n {
            out.push([
                a[0] + ((i64::from(b[0] - a[0]) * k) / n) as i32,
                a[1] + ((i64::from(b[1] - a[1]) * k) / n) as i32,
                a[2] + ((i64::from(b[2] - a[2]) * k) / n) as i32,
            ]);
        }
    }
    if let Some(l) = plan.waypoints.last() {
        out.push(*l);
    }
    out
}

fn dist_mm(a: [i32; 3], b: [i32; 3]) -> i64 {
    ((i64::from(a[0] - b[0]).pow(2) + i64::from(a[2] - b[2]).pow(2)) as u64).isqrt() as i64
}

/// `q` with its height moved to the nearest floor of the map's world geometry: a centerline height
/// interpolated between layout points runs under a cooked ramp's real floor, and would land on the
/// passage under it.
fn on_floor(map: &Map, q: [i32; 3]) -> [i32; 3] {
    let mut spans = regia_world_geom::SpanBuf::default();
    map.geom.floors_at(
        &map.geom.new_state(),
        q[0] as f32 / 1000.0,
        q[2] as f32 / 1000.0,
        &mut spans,
    );
    let y = q[1] as f32 / 1000.0;
    spans
        .spans()
        .iter()
        .map(|s| s.floor.y)
        .min_by(|a, b| (a - y).abs().total_cmp(&(b - y).abs()))
        .map_or(q, |f| [q[0], mm(f), q[2]])
}

fn plan(map: &Map, state: &LayerState, from: [i32; 3], to: [i32; 3]) -> LayerPlan {
    let (from, to) = (on_floor(map, from), on_floor(map, to));
    let agent = LayerAgent::for_bake(&map.bake);
    map.bake
        .plan(state, from, to, &agent, &mut LayerScratch::default())
        .unwrap_or_else(|e| panic!("plan {from:?} -> {to:?}: {e}"))
}

#[test]
fn the_switchbacks_bake_has_its_passages() {
    let m = switchbacks();
    assert!(m.bake.polys.len() > 50, "{} polys", m.bake.polys.len());
    let in_passage = m.bake.polys.iter().filter(|p| p.in_passage()).count();
    assert!(
        in_passage * 10 >= m.bake.polys.len() * 9,
        "{in_passage} passage polys"
    );
    let sectioned = m
        .bake
        .polys
        .iter()
        .filter(|p| p.section != regia_nav::det::layers::NO_SECTION)
        .count();
    assert!(
        sectioned * 2 >= m.bake.polys.len(),
        "{sectioned} of {} polys with a section",
        m.bake.polys.len()
    );
    println!(
        "switchbacks: {} polys, {} edges, {} layers, hash {:#018x}",
        m.bake.polys.len(),
        m.bake.edges.len(),
        m.bake.layer_count(),
        m.bake.content_hash()
    );
}

/// Requirement `two_hulls_pass_each_other_in_the_tube`, in every passage: two 4.5 m hulls set off
/// head-on from either end of a passage (10 % and 90 % of its centerline); where both plans are in
/// the passage's middle (30-70 %), their centers stay at least `2 r - 0.5` m apart.
#[test]
fn two_hulls_pass_in_every_passage() {
    let m = switchbacks();
    let state = LayerState::new(&m.bake);
    let bar = 2 * i64::from(mm(HULL_R)) - PASS_TOL_MM;
    let mut report = Vec::new();
    for p in &m.layout.passages {
        let len = length(p);
        // The far end: 90 % of the centerline, or the farthest of 80-60 % the bake reaches (a ramp's
        // top may end in its open cut; the report says which).
        let agent = LayerAgent::for_bake(&m.bake);
        let a = on_floor(m, along(p, len * 0.1));
        let mut far = None;
        for f in [0.9f32, 0.8, 0.7, 0.6] {
            let b = on_floor(m, along(p, len * f));
            let mut sc = LayerScratch::default();
            if let (Ok(e), Ok(w)) = (
                m.bake.plan(&state, a, b, &agent, &mut sc),
                m.bake.plan(&state, b, a, &agent, &mut sc),
            ) {
                far = Some((f, e, w));
                break;
            }
        }
        let (f, east, west) =
            far.unwrap_or_else(|| panic!("{}: no plan from 10 % to 60 % of the passage", p.id));
        let (lo, hi) = (len * 0.3, len * (f - 0.2));
        let window = |pl: &LayerPlan| -> Vec<[i32; 3]> {
            samples(pl)
                .into_iter()
                .filter(|q| {
                    let s = arc_of(p, *q);
                    s >= lo && s <= hi
                })
                .collect()
        };
        let (ea, wa) = (window(&east), window(&west));
        assert!(
            !ea.is_empty() && !wa.is_empty(),
            "{}: both plans cross the middle",
            p.id
        );
        let mut closest = i64::MAX;
        let mut at = ([0; 3], [0; 3]);
        for q in &ea {
            for r in &wa {
                let d = dist_mm(*q, *r);
                if d < closest {
                    closest = d;
                    at = (*q, *r);
                }
            }
        }
        let center = |q: [i32; 3]| dist_mm(q, along(p, arc_of(p, q)));
        report.push(format!(
            "{} (to {:.0} %) {closest} at {:?} (arc {:.1} m, {} mm off center) / {:?} ({} mm off center); {} + {} waypoints",
            p.id,
            f * 100.0,
            at.0,
            arc_of(p, at.0),
            center(at.0),
            at.1,
            center(at.1),
            east.waypoints.len(),
            west.waypoints.len()
        ));
        assert!(
            closest >= bar,
            "{}: {closest} mm apart, the bar is {bar} mm ({report:?})",
            p.id
        );
    }
    println!("passing: {report:?}");
}

/// A deterministic integer mover: each tick, in index order, a hull moves `step_mm` along its
/// plan unless that move would bring it within `2 r - 0.5` m of a hull with a lower index (the
/// right of way) and closer to it.
/// Returns the tick every hull reached its goal, or `None` (with the positions) at `max_ticks`.
fn step_group(plans: &[LayerPlan], step_mm: i64, max_ticks: u32) -> (Option<u32>, Vec<[i32; 3]>, u32) {
    let bar = 2 * i64::from(mm(HULL_R)) - PASS_TOL_MM;
    let mut pos: Vec<[i32; 3]> = plans.iter().map(|p| p.waypoints[0]).collect();
    let mut next: Vec<usize> = vec![1; plans.len()];
    let mut idle = 0u32;
    let mut longest_idle = 0u32;
    for tick in 0..max_ticks {
        if (0..plans.len()).all(|i| next[i] >= plans[i].waypoints.len()) {
            return (Some(tick), pos, longest_idle);
        }
        let mut moved = false;
        for i in 0..plans.len() {
            let mut budget = step_mm;
            let mut cand = pos[i];
            let mut k = next[i];
            while budget > 0 && k < plans[i].waypoints.len() {
                let w = plans[i].waypoints[k];
                let d = dist_mm(cand, w);
                if d <= budget {
                    cand = w;
                    budget -= d;
                    k += 1;
                } else {
                    cand = [
                        cand[0] + ((i64::from(w[0] - cand[0]) * budget) / d) as i32,
                        cand[1] + ((i64::from(w[1] - cand[1]) * budget) / d) as i32,
                        cand[2] + ((i64::from(w[2] - cand[2]) * budget) / d) as i32,
                    ];
                    budget = 0;
                }
            }
            let blocked = (0..i)
                .any(|j| dist_mm(cand, pos[j]) < bar && dist_mm(cand, pos[j]) < dist_mm(pos[i], pos[j]));
            if !blocked && cand != pos[i] {
                pos[i] = cand;
                next[i] = k;
                moved = true;
            } else if !blocked {
                next[i] = k;
            }
        }
        if moved {
            idle = 0;
        } else {
            idle += 1;
            longest_idle = longest_idle.max(idle);
        }
    }
    (None, pos, longest_idle)
}

/// Eight hulls in a column `s0 + 10 k` m back along `from` from the junction, to slots `30 + 10 k`
/// m out along `to` (the front hull takes the farthest slot).
fn group_through(
    m: &Map,
    junction: &str,
    from: &str,
    from_dir: f32,
    to: &str,
    to_dir: f32,
) -> (Option<u32>, u32) {
    let j = m.layout.junctions.iter().find(|j| j.id == junction).unwrap();
    let pa = m.layout.passages.iter().find(|p| p.id == from).unwrap();
    let pb = m.layout.passages.iter().find(|p| p.id == to).unwrap();
    let jat = [mm(j.at[0]), mm(j.at[1]), mm(j.at[2])];
    let (sa, sb) = (arc_of(pa, jat), arc_of(pb, jat));
    let state = LayerState::new(&m.bake);
    let plans: Vec<LayerPlan> = (0..8)
        .map(|k| {
            let start = along(pa, sa + from_dir * (40.0 + 12.0 * k as f32));
            let goal = along(pb, sb + to_dir * (30.0 + 12.0 * (7 - k) as f32));
            plan(m, &state, start, goal)
        })
        .collect();
    let (arrived, at, idle) = step_group(&plans, 400, 6_000);
    assert!(arrived.is_some(), "{junction}: the group came through: {at:?}");
    (arrived, idle)
}

/// Requirement `a_group_goes_through_a_junction_without_wedging`, at each junction of the
/// Switchbacks and at the Cinder Steps' south junction: eight hulls in a column come through from
/// one passage into the other and every hull reaches its slot; no tick passes where every waiting
/// hull is stuck for more than 10 ticks running (a wedge).
#[test]
fn junction_never_wedges() {
    let s = switchbacks();
    let mut seen = Vec::new();
    // West junction: the tube from the west into the gallery; east: the gallery into the tube eastward.
    seen.push(("sw-west", group_through(s, "west", "tube", -1.0, "gallery", 1.0)));
    seen.push(("sw-east", group_through(s, "east", "gallery", -1.0, "tube", 1.0)));
    let c = cinder();
    // The core ends 57 m south of the south junction: the column comes down the core from the north.
    seen.push((
        "cinder-south",
        group_through(c, "south", "core", 1.0, "strand_w", 1.0),
    ));
    for (name, (arrived, idle)) in &seen {
        assert!(
            *idle <= 10,
            "{name}: {idle} ticks with no hull moving (arrived {arrived:?})"
        );
    }
    println!("junctions: {seen:?}");
}

#[test]
fn layered_bake_is_byte_identical_twice() {
    let a = baked("switchbacks", "");
    let b = baked("switchbacks", "");
    assert_eq!(a.bake.to_bytes(), b.bake.to_bytes());
    assert_eq!(a.bake.content_hash(), switchbacks().bake.content_hash());
    let c = baked("cinder_steps", "");
    assert_eq!(c.bake.to_bytes(), cinder().bake.to_bytes());
    println!(
        "bake hashes: switchbacks {:#018x} ({} bytes), cinder_steps {:#018x} ({} bytes)",
        a.bake.content_hash(),
        a.bake.to_bytes().len(),
        c.bake.content_hash(),
        c.bake.to_bytes().len()
    );
}

/// Requirement 4 (W-13): `set_blocker` closes the plug section's polygons at the generation it
/// returns; a plan through the gallery goes round by the tube, and clearing reopens it at the next.
#[test]
fn a_blocker_closes_its_polys_in_the_same_generation() {
    let m = switchbacks_gallery_plug();
    let plug = m.geom.blockers()[0];
    let gallery = m.layout.passages.iter().find(|p| p.id == "gallery").unwrap();
    let len = length(gallery);
    let (a, b) = (along(gallery, len * 0.05), along(gallery, len * 0.5));
    let mut geom_state: GeomState = m.geom.new_state();
    let mut layers = LayerState::new(&m.bake);
    let section_polys: Vec<u32> = (0..m.bake.polys.len() as u32)
        .filter(|&i| m.bake.polys[i as usize].section == plug.section)
        .collect();
    assert!(!section_polys.is_empty(), "the plug's section has polygons");
    let open = plan(m, &layers, a, b);
    assert!(
        open.polys.iter().any(|p| section_polys.contains(p)),
        "the open plan uses the gallery run"
    );

    let generation = geom_state.set_blocker(&m.geom, BlockerId(plug.id), true).unwrap();
    let flipped = sync_layers(&m.bake, &mut layers, &m.geom, &geom_state, &[]);
    assert_eq!(layers.generation(), generation.0, "the same generation");
    assert_eq!(flipped, section_polys, "exactly the plug section's polygons flip");
    assert_eq!(layers.closed_sections(), &[plug.section]);
    assert!(!m.geom.section_open(&geom_state, SectionId(plug.section)));
    let round = plan(m, &layers, a, b);
    assert_eq!(round.generation, generation.0);
    assert!(
        round.polys.iter().all(|p| !section_polys.contains(p)),
        "the closed run is never entered"
    );
    assert!(
        round.cost > open.cost,
        "the way round costs more ({} vs {})",
        round.cost,
        open.cost
    );

    let generation2 = geom_state
        .set_blocker(&m.geom, BlockerId(plug.id), false)
        .unwrap();
    let back = sync_layers(&m.bake, &mut layers, &m.geom, &geom_state, &[]);
    assert_eq!(back, section_polys);
    assert_eq!(layers.generation(), generation2.0);
    let again = plan(m, &layers, a, b);
    assert_eq!(again.polys, open.polys, "reopened at the next generation");
    assert_eq!(again.waypoints, open.waypoints);
}

/// Requirement 4 (W-12): a crater marks the nav tiles under its dirty rectangle, in tile order, at
/// the edit's generation; a plan crossing them says so until the host clears them.
#[test]
fn a_ground_edit_marks_its_nav_tiles_dirty() {
    let m = switchbacks();
    let mut geom_state = m.geom.new_state();
    let mut layers = LayerState::new(&m.bake);
    let tube = m.layout.passages.iter().find(|p| p.id == "tube").unwrap();
    let len = length(tube);
    let (a, b) = (along(tube, len * 0.2), along(tube, len * 0.8));
    let clean = plan(m, &layers, a, b);
    assert!(!clean.crosses_dirty);
    // The fixture's lattice is 4 x 4 (426 m cells): a crater on the vertex nearest the tube's middle.
    let lattice = m.geom.cooked().lattice;
    let at = along(tube, len * 0.5);
    let near = |v: i32| {
        (0..lattice.lattice_n)
            .min_by_key(|&i| (lattice.vertex_mm(i) - i64::from(v)).abs())
            .unwrap()
    };
    let (vx, vz) = (
        lattice.vertex_mm(near(at[0])) as i32,
        lattice.vertex_mm(near(at[2])) as i32,
    );
    let rect = geom_state
        .apply_ground_edit(
            &m.geom,
            &GroundEdit::Crater {
                x_mm: vx,
                z_mm: vz,
                radius_mm: 20_000,
                depth_mm: 1_000,
            },
        )
        .unwrap();
    assert!(!rect.is_empty());
    sync_layers(&m.bake, &mut layers, &m.geom, &geom_state, &[rect]);
    assert_eq!(layers.generation(), geom_state.generation().0);
    let tiles = layers.dirty_tiles().to_vec();
    assert!(!tiles.is_empty(), "the edit dirtied tiles");
    assert!(
        tiles.windows(2).all(|w| w[0] < w[1]),
        "ascending, no duplicates: {tiles:?}"
    );
    let mut expect = Vec::new();
    m.bake.tiles_in_rect(
        regia_nav::layered::geom::dirty_rect_mm(&m.geom, rect),
        &mut expect,
    );
    expect.sort_unstable();
    expect.dedup();
    assert_eq!(tiles, expect);
    let stale = plan(m, &layers, a, b);
    assert!(stale.crosses_dirty, "the plan crosses the edited tiles");
    assert_eq!(
        stale.polys, clean.polys,
        "a dirty tile stays walkable until it is re-baked"
    );
    layers.clear_dirty(&tiles);
    assert!(!plan(m, &layers, a, b).crosses_dirty);
}

/// The run of `layered_tunnel_runs_give_the_same_hash`: four hulls from each end of the tube to
/// the other, stepped up to 4000 ticks; the hash of every position and plan.
fn tunnel_run(m: &Map) -> (u64, u32) {
    let tube = m.layout.passages.iter().find(|p| p.id == "tube").unwrap();
    let len = length(tube);
    let state = LayerState::new(&m.bake);
    let mut plans = Vec::new();
    for k in 0..4 {
        let off = 6.0 + 12.0 * k as f32;
        plans.push(plan(m, &state, along(tube, off), along(tube, len - 60.0 + off)));
        plans.push(plan(m, &state, along(tube, len - off), along(tube, 60.0 - off)));
    }
    let (arrived, pos, _) = step_group(&plans, 400, 4_000);
    let mut words: Vec<u8> = Vec::new();
    for p in &plans {
        for w in &p.waypoints {
            for c in w {
                words.extend_from_slice(&c.to_le_bytes());
            }
        }
        words.extend_from_slice(&p.cost.to_le_bytes());
    }
    for p in &pos {
        for c in p {
            words.extend_from_slice(&c.to_le_bytes());
        }
    }
    words.extend_from_slice(&m.bake.content_hash().to_le_bytes());
    (fnv1a64(&words), arrived.unwrap_or(u32::MAX))
}

/// The A-10 re-pin of `tunnel_runs_give_the_same_hash`: the old value is the W-04 twin
/// `tunnel_plans_give_the_same_hash`'s `f67d91d885e0e6bf` (the centerline layer and the navmesh, which
/// stays as the Stage A pin); the new value `4d9791c86c636bcc` is the layered runtime at
/// `LAYERED_NAV_BITS_VERSION` 1 (every hull arrives at tick 3351). Equal twice, and pinned.
#[test]
fn layered_tunnel_runs_give_the_same_hash() {
    let m = switchbacks();
    let a = tunnel_run(m);
    assert_eq!(a, tunnel_run(m));
    assert!(a.1 < u32::MAX, "every hull arrived");
    println!("layered_tunnel_runs: hash {:#018x}, arrived at tick {}", a.0, a.1);
    assert_eq!(
        a.0, LAYERED_TUNNEL_RUN_PIN,
        "a layered plan moved: bump LAYERED_NAV_BITS_VERSION and re-pin (A-10)"
    );
}
const LAYERED_TUNNEL_RUN_PIN: u64 = 0x4d97_91c8_6c63_6bcc;

/// The golden scenario: plans between every pair of passage points, both directions, one plan per
/// tick.
struct LayeredPlans {
    pairs: Vec<([i32; 3], [i32; 3])>,
    last: u64,
    acc: u64,
}

impl DetScenario for LayeredPlans {
    fn name(&self) -> &str {
        "layered_plans"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0a20, self.pairs.len() as u32, TickRate::DEFAULT)
    }

    fn reset(&mut self, _run: &DetRun) {
        self.last = 0;
        self.acc = 0;
    }

    fn step(&mut self, tick: u32) {
        let m = switchbacks();
        let state = LayerState::new(&m.bake);
        let (a, b) = self.pairs[tick as usize % self.pairs.len()];
        let agent = LayerAgent::for_bake(&m.bake);
        let mut words = Vec::new();
        match m.bake.plan(&state, a, b, &agent, &mut LayerScratch::default()) {
            Ok(p) => {
                for w in &p.waypoints {
                    for c in w {
                        words.extend_from_slice(&c.to_le_bytes());
                    }
                }
                words.extend_from_slice(&p.cost.to_le_bytes());
                words.extend_from_slice(&p.expansions.to_le_bytes());
            }
            Err(e) => words.extend_from_slice(e.code().as_bytes()),
        }
        self.last = fnv1a64(&words);
        self.acc = self.acc.rotate_left(5) ^ self.last;
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("plan", self.last);
        h.u64("acc", self.acc);
        h.u64("bake", switchbacks().bake.content_hash());
    }
}

fn layered_plans() -> LayeredPlans {
    let m = switchbacks();
    let mut pts = Vec::new();
    for p in &m.layout.passages {
        let len = length(p);
        pts.push(along(p, len * 0.25));
        pts.push(along(p, len * 0.75));
    }
    let mut pairs = Vec::new();
    for (i, a) in pts.iter().enumerate() {
        for b in pts.iter().skip(i + 1) {
            pairs.push((*a, *b));
            pairs.push((*b, *a));
        }
    }
    LayeredPlans {
        pairs,
        last: 0,
        acc: 0,
    }
}

/// F-02 golden: the layered plans (and the bake hash) on every arch. `REGIA_BLESS=1` records this
/// target's line. Prints one `regia-det-golden {json}` line.
#[test]
fn layered_plans_pinned_on_every_arch() {
    let dir = match std::env::var_os(golden::GOLDEN_DIR_ENV) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
    };
    let mut scenario = layered_plans();
    let run = scenario.default_run();
    let trace = golden::record(&mut scenario, &run);
    let again = golden::record(&mut layered_plans(), &run);
    assert_eq!(
        golden::first_divergence(&trace.tick_hashes, &again.tick_hashes),
        None
    );
    let report = golden::check_trace(
        &trace,
        &golden::golden_path(&dir, "layered_plans"),
        &golden::current_target(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
}
