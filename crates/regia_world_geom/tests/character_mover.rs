// SPDX-License-Identifier: MPL-2.0
//! The character mover's acceptance (plan row W-30, spec 121 § 5.4 and § 13.3, ADR-8323).
//!
//! Original tests. Requirements: DO27's walker slide and step (`pilot_collide.rs` 774-930, 216), the
//! skylight prop (`ir-sim/src/tunnels.rs` 71-88) and the T3 handover's known issue K3 (the rubble
//! pile at the mid-east skylight). The fixtures build their own cones and rocks: no game data.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]

pub mod native_support;

use std::sync::OnceLock;

use native_support::{FIXTURES, hex, native, native_mesh, read};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::{ON_FOOT_PARAMS, for_map};
use regia_world_geom::compat::mesh::{TunnelFaceKind, synthetic_gallery};
use regia_world_geom::compat::onfoot::mesh::{GltfMemory, load_gltf_grid_on};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;
use regia_world_geom::compat::terrain_patch::TerrainPatch;
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, PROP_LAYER_ID, cook};
use regia_world_geom::mover::{CapsuleMove, CapsuleParams, MoveStop, dressing_grid, move_capsule};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::{Band, GeomState, QUERY_BITS_VERSION, SurfaceKind, WorldGeom};

/// The walker of the on-foot fixtures (radius 0.35 m, 1.85 m tall), one 0.55 m step up and down.
const WALKER: CapsuleParams = CapsuleParams {
    radius_m: ON_FOOT_PARAMS.capsule_radius_m,
    height_m: ON_FOOT_PARAMS.height_m,
    step_up_m: 0.55,
    step_down_m: 0.55,
    substep_m: 0.2,
};
/// A walker's run, metres per 60 Hz tick (about 7.8 m/s, the on-foot grid's 0.13 m).
const RUN: f32 = 0.13;
/// Where the fixtures draw the mid-east skylight's rubble: a third of the way up the ramp, on its
/// centreline. The fixture's mid-east opening is a `skylight`, so the cook opens this climb to the sky
/// (K8, `regia_world_geom::sections::open_skylight_shafts`): no lid over the pile.
const PILE: [f32; 2] = [16.0, -126.0];
/// The same spot mirrored onto `ramp_mid_west`, a `fracture_ramp` that K8 leaves covered: a pile under a
/// lid (the scene W-30 first drew at [`PILE`], before K8 joined the cook).
const LIDDED_PILE: [f32; 2] = [-16.0, -126.0];

/// The flat canyon floor at -256 m (the T3 reference terrain).
fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

/// A `*_skylight` patch over the mid-east ramp: `profile(d)` metres over the ramp floor at distance
/// `d` from [`PILE`], 12 m across at `spacing`. The ramp floor comes from the Stage A query (the data
/// the art is drawn on), as the on-foot fixtures do.
fn ramp_patch(spacing: f32, profile: impl Fn(f32) -> f32) -> TerrainPatch {
    ramp_patch_at(PILE, "mid_east_skylight", spacing, profile)
}

/// [`ramp_patch`] centred on `center` of any Switchbacks ramp, as patch `id` (a `*_skylight` id makes it a
/// prop-layer patch).
fn ramp_patch_at(center: [f32; 2], id: &str, spacing: f32, profile: impl Fn(f32) -> f32) -> TerrainPatch {
    let canyon = canyon();
    let tunnels = TunnelsState::<()>::for_layout(for_map("switchbacks").unwrap(), Some(&canyon));
    let [cx, cz] = center;
    let n = (12.0 / spacing) as usize + 1;
    let origin = [cx - 6.0, cz - 6.0];
    let ramp_y = tunnels.move_floor(Some(&canyon), cx, cz, -270.0).point[1];
    let heights = (0..n * n)
        .map(|i| {
            let (x, z) = (
                origin[0] + (i % n) as f32 * spacing,
                origin[1] + (i / n) as f32 * spacing,
            );
            let floor = tunnels.move_floor(Some(&canyon), x, z, ramp_y + 1.0).point[1];
            let d = regia_detsim::math::sqrt((x - cx) * (x - cx) + (z - cz) * (z - cz));
            floor + profile(d)
        })
        .collect();
    TerrainPatch {
        id: id.into(),
        origin,
        spacing,
        cols: n,
        rows: n,
        heights,
    }
}

/// The 1.8 m rubble cone, 6 m across (handover K3; the on-foot fixtures' cone).
fn cone_profile(d: f32) -> f32 {
    1.8 * (1.0 - regia_detsim::float::det_min(d / 6.0, 1.0))
}

/// A drawn rock: a 2.5 m plateau 1.5 m in radius with a 0.5 m steep skirt.
fn rock_profile(d: f32) -> f32 {
    if d <= 1.5 {
        2.5
    } else if d < 2.0 {
        2.5 * (1.0 - (d - 1.5) / 0.5)
    } else {
        0.0
    }
}
/// The rock's drawn radius at `h` metres over the ramp floor.
fn rock_radius_at(h: f32) -> f32 {
    1.5 + 0.5 * (1.0 - h / 2.5)
}

/// The Switchbacks fixture cooked on `terrain` (its mouths cut, as the canyon vector fixture cooks).
fn switchbacks(terrain: &TerrainSampler) -> (WorldGeom, regia_world_geom::cook::CookReport) {
    let layout = for_map("switchbacks").unwrap();
    let fills: Vec<_> = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(terrain)))
        .iter()
        .map(|m| format!("{:?}", m.id))
        .collect();
    let document = format!(
        "(version:1,layers:[(id:\"rings\",source:Passages(path:\"map.passages.ron\"),fills:[{}])])",
        fills.join(",")
    );
    let passages = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let ground = GroundInput {
        lattice_n: 4096,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 4096 * 4096],
    };
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(terrain),
            read_file: &|_| Ok(passages.as_bytes().to_vec()),
        })
    };
    // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks do (ADR-8436).
    let out = match run(&document) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&document, &report).unwrap()).unwrap()
        }
        Err(e) => panic!("{e} ({})", e.code()),
    };
    let bytes = out.file.encode().unwrap();
    (WorldGeom::from_cooked(&bytes).unwrap(), out.report)
}

struct Fixture {
    geom: WorldGeom,
    report: regia_world_geom::cook::CookReport,
}
fn plain() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let (geom, report) = switchbacks(&canyon());
        Fixture { geom, report }
    })
}
fn coned() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let (geom, report) = switchbacks(&canyon().with_patches(vec![ramp_patch(0.5, cone_profile)]));
        Fixture { geom, report }
    })
}
/// The rubble cone drawn at [`LIDDED_PILE`], on the covered mid-west climb.
fn lidded() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let patch = ramp_patch_at(LIDDED_PILE, "mid_west_skylight", 0.5, cone_profile);
        let (geom, report) = switchbacks(&canyon().with_patches(vec![patch]));
        Fixture { geom, report }
    })
}
fn rocked() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let (geom, report) = switchbacks(&canyon().with_patches(vec![ramp_patch(0.25, rock_profile)]));
        Fixture { geom, report }
    })
}

/// The ramp floor under `(x, z)` near `y` from the native query.
fn floor_at(geom: &WorldGeom, state: &GeomState, x: f32, z: f32, y: f32) -> f32 {
    geom.support(state, x, z, y).floor.y
}

/// What a drawn rubble pile has over its summit.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Over {
    /// An open skylight climb (K8): no lid, in the plain cook as on the pile.
    Sky,
    /// A covered climb: the ramp's lid, in the plain cook as on the pile.
    Lid,
}

/// Acceptance 1 (plan § 4.3 row "Rubble pile", handover K3, T3 addendum § 1): a walker on the
/// mid-east skylight cone stands on the cone through `support`, with no prop hook. The cook's prop
/// layer makes the drawn rubble canonical floor triangles. Control: the same map cooked without the
/// drawn patch has the ramp floor there.
///
/// The fixture's mid-east opening is a `skylight`, so the cook opens its ramp's climb to the sky (K8,
/// W-08): the summit has no lid, in the plain cook as on the pile. W-30 drew this scene before K8 joined
/// the cook and asserted the ramp's lid here; a pile under a lid is
/// [`a_lidded_rubble_pile_is_a_sim_floor`].
#[test]
fn rubble_pile_is_a_sim_floor() {
    let rings = plain()
        .report
        .layers
        .iter()
        .find(|l| l.id == "rings")
        .expect("rings layer");
    assert!(
        rings.skylight_segments_opened > 0,
        "K8 opened the skylight climbs: {rings:?}"
    );
    pile_is_a_sim_floor(coned(), PILE, 1.0, Over::Sky);
}

/// Acceptance 1 under a lid: the same cone drawn on the mid-west climb (a `fracture_ramp`, which K8 leaves
/// covered) is a prop-layer floor, and `support` over its summit still answers the ramp's lid. The walk is
/// the mirror of the mid-east one.
#[test]
fn a_lidded_rubble_pile_is_a_sim_floor() {
    pile_is_a_sim_floor(lidded(), LIDDED_PILE, -1.0, Over::Lid);
}

/// The summit, control and walk checks of acceptance 1 for the cone drawn at `center` in `fixture`,
/// walking along `dir` (+1 or -1) times x.
fn pile_is_a_sim_floor(fixture: &Fixture, center: [f32; 2], dir: f32, over: Over) {
    let geom = &fixture.geom;
    let state = geom.new_state();
    let [cx, cz] = center;
    let props = fixture.report.layers.iter().position(|l| l.id == PROP_LAYER_ID);
    let props = props.expect("the cook wrote the prop layer") as u16;
    assert!(
        fixture.report.layers[usize::from(props)].triangles > 100,
        "{:?}",
        fixture.report.layers
    );

    let plain_state = plain().geom.new_state();
    let ramp_y = floor_at(&plain().geom, &plain_state, cx, cz, -270.0);
    assert!(
        plain().report.layers.iter().all(|l| l.id != PROP_LAYER_ID),
        "no patch, no prop layer"
    );
    // The summit: support at one step over the top answers the cone, a prop triangle.
    let top = geom.support(&state, cx, cz, ramp_y + 1.8 + 0.57);
    assert!(
        (top.floor.y - (ramp_y + 1.8)).abs() < 0.03,
        "{top:?} over ramp {ramp_y}"
    );
    assert_eq!(top.floor.kind, SurfaceKind::Floor);
    assert_eq!(top.floor.tri.map(|t| t.layer), Some(props), "{top:?}");
    // Control: without the drawn patch the same point stands on the ramp.
    let bare = plain().geom.support(&plain_state, cx, cz, ramp_y + 1.8 + 0.57);
    assert!((bare.floor.y - ramp_y).abs() < 0.03, "{bare:?}");
    match over {
        Over::Sky => {
            assert!(
                top.ceiling.is_none(),
                "the skylight climb is open to the sky (K8): {top:?}"
            );
            assert!(bare.ceiling.is_none(), "open without the pile too: {bare:?}");
        }
        Over::Lid => {
            assert!(top.ceiling.is_some(), "the cone is under the ramp's lid: {top:?}");
            assert_eq!(
                top.ceiling.map(|c| c.y),
                bare.ceiling.map(|c| c.y),
                "the pile keeps the ramp's lid: {top:?} vs {bare:?}"
            );
        }
    }

    // The walk: from the ramp beside the pile straight over its summit and down the far side.
    let start_x = cx - 6.5 * dir;
    let mut feet = [start_x, floor_at(geom, &state, start_x, cz, ramp_y + 1.0), cz];
    let (mut on_cone, mut max_rise, mut max_drop) = (0, 0.0f32, 0.0f32);
    for tick in 0..100 {
        let m = move_capsule(geom, &state, &WALKER, feet, [RUN * dir, 0.0], true);
        assert_eq!(m.stop, MoveStop::None, "tick {tick}: {m:?}");
        assert!(!m.airborne, "tick {tick}: {m:?}");
        let plain_y = floor_at(
            &plain().geom,
            &plain_state,
            m.feet[0],
            m.feet[2],
            m.feet[1] + 0.57,
        );
        let drawn = plain_y + cone_profile((m.feet[0] - cx).abs());
        if drawn - plain_y > 0.1 {
            // On the pile the feet are on the drawn cone (the capsule's rim may lift them a little).
            assert!(
                m.feet[1] >= drawn - 0.06 && m.feet[1] <= drawn + 0.08,
                "tick {tick}: feet {:?}, drawn {drawn}",
                m.feet
            );
            on_cone += 1;
        }
        max_rise = regia_detsim::float::det_max(max_rise, m.feet[1] - feet[1]);
        max_drop = regia_detsim::float::det_max(max_drop, feet[1] - m.feet[1]);
        feet = m.feet;
    }
    eprintln!(
        "RUBBLE {over:?} on_cone {on_cone} max_rise {max_rise:.4} max_drop {max_drop:.4} top {:.3}",
        top.floor.y - ramp_y
    );
    assert!(on_cone >= 70, "the walk crosses the pile: {on_cone} ticks on it");
    // K3: no 1.4-1.65 m drop in a tick; every tick is well inside one step.
    assert!(
        max_rise <= 0.1 && max_drop <= 0.1,
        "rise {max_rise}, drop {max_drop}"
    );
}

/// Acceptance 2 (plan § 4.3 row "Mid-east ramp", spec 121 § 13.3): a drawn solid on the mid-east
/// ramp stops the capsule within 0.5 m of its drawn surface at body height, from every direction.
/// The drawn rock is a 2.5 m plateau with a steep skirt; on the base the capsule walks through it.
#[test]
fn drawn_solid_within_half_meter_of_wall() {
    let geom = &rocked().geom;
    let state = geom.new_state();
    let [cx, cz] = PILE;
    let body_h = 1.0;
    let drawn_r = rock_radius_at(body_h);
    let dirs: [[f32; 2]; 8] = [
        [1.0, 0.0],
        [0.707_106_77, 0.707_106_77],
        [0.0, 1.0],
        [-0.707_106_77, 0.707_106_77],
        [-1.0, 0.0],
        [-0.707_106_77, -0.707_106_77],
        [0.0, -1.0],
        [0.707_106_77, -0.707_106_77],
    ];
    for (k, [ux, uz]) in dirs.into_iter().enumerate() {
        let (sx, sz) = (cx + ux * 6.0, cz + uz * 6.0);
        let mut feet = [sx, floor_at(geom, &state, sx, sz, -270.0), sz];
        let mut stopped = None;
        for tick in 0..60 {
            let m: CapsuleMove = move_capsule(geom, &state, &WALKER, feet, [-ux * RUN, -uz * RUN], true);
            if m.stop != MoveStop::None && stopped.is_none() {
                stopped = Some((tick, m.stop));
            }
            feet = m.feet;
        }
        let d = regia_detsim::math::sqrt((feet[0] - cx) * (feet[0] - cx) + (feet[2] - cz) * (feet[2] - cz));
        let gap = d - WALKER.radius_m - drawn_r;
        eprintln!("DRAWN_SOLID direction {k} gap {gap:.3} m stop {stopped:?}");
        assert!(
            (-0.05..=0.5).contains(&gap),
            "direction {k}: capsule surface {gap} m from the drawn rock at body height (feet {feet:?}, stop {stopped:?})"
        );
        assert_eq!(stopped.map(|s| s.1), Some(MoveStop::Ledge), "direction {k}");
    }
}

/// One capsule vector row: inputs and the `push_capsule` answer, as f32 hex bits.
fn capsule_row(
    name: &str,
    geom: &WorldGeom,
    state: &GeomState,
    x: f32,
    z: f32,
    r: f32,
    band: Band,
) -> String {
    let (p, b) = geom.push_capsule(state, x, z, r, band);
    let h = |v: f32| format!("{:08x}", v.to_bits());
    format!(
        "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        h(x),
        h(z),
        h(r),
        h(band.lo),
        h(band.hi),
        h(p.xz[0]),
        h(p.xz[1]),
        p.contacts,
        h(b.lo),
        h(b.hi)
    )
}

/// The committed capsule vector table: the four-deck stack (every fifth T3 column, a walker's band
/// just under and just over each floor of the column, two radii, two offsets across the deck edges) and the low gallery
/// (a 9 x 9 point grid, four feet heights, two radii).
fn capsule_table() -> Vec<String> {
    let mut rows = Vec::new();
    let file = read("four_decks_mesh");
    let geom = native(&file, "four_decks_mesh", 16.0);
    let state = geom.new_state();
    for row in file["columns"].as_array().unwrap().iter().step_by(5) {
        let (x, z) = (hex(&row["x"]), hex(&row["z"]));
        let mut spans = regia_world_geom::SpanBuf::default();
        geom.floors_at(&state, x, z, &mut spans);
        for span in spans.spans() {
            let y = span.floor.y;
            for r in [0.35f32, 0.8] {
                for off in [0.0f32, 0.3] {
                    for lo in [y - 0.2, y + 0.1] {
                        let band = Band { lo, hi: lo + 1.85 };
                        rows.push(capsule_row("four_decks", &geom, &state, x + off, z, r, band));
                    }
                }
            }
        }
    }
    let gallery = synthetic_gallery(10.0, 10.0, 0.0, 2.0, false);
    let geom = native_mesh(&gallery.triangles, 16.0);
    let state = geom.new_state();
    for i in 0..9 {
        for j in 0..9 {
            let (x, z) = (-9.6 + i as f32 * 2.4, -9.6 + j as f32 * 2.4);
            for lo in [-0.1f32, 0.0, 0.1, 0.3] {
                for r in [0.35f32, 0.8] {
                    let band = Band { lo, hi: lo + 1.85 };
                    rows.push(capsule_row("gallery", &geom, &state, x, z, r, band));
                }
            }
        }
    }
    rows
}

/// Acceptance 3 (spec 121 § 5.4: "W-30 settles it with its vectors"): `push_capsule`'s answers
/// on the four-deck stack and the gallery equal the committed table bit for bit (x86-64 and
/// aarch64). Set `REGIA_WRITE_CAPSULE_VECTORS=<path>` to write the table this build computes.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "REGIA_WRITE_CAPSULE_VECTORS only names where the computed table is written for review; the check is the same (ADR-8367)"
)]
fn capsule_vertical_rule_vectors() {
    let rows = capsule_table();
    let text = format!(
        "# push_capsule vectors, QUERY_BITS_VERSION {QUERY_BITS_VERSION} (ADR-8323). fixture x z radius lo hi -> x z contacts lo hi (f32 hex)\n{}\n",
        rows.join("\n")
    );
    if let Some(path) = std::env::var_os("REGIA_WRITE_CAPSULE_VECTORS") {
        std::fs::write(path, &text).unwrap();
    }
    // The rule does something on both fixtures: feet lifted onto a deck edge, heads under the lid.
    let lifted = rows.iter().filter(|r| {
        let c: Vec<&str> = r.split('\t').collect();
        c[4] != c[9]
    });
    let lowered = rows.iter().filter(|r| {
        let c: Vec<&str> = r.split('\t').collect();
        c[5] != c[10]
    });
    assert!(
        lifted.count() > 0 && lowered.count() > 0,
        "the vertical rule moved nothing"
    );
    // Embedded, so a device run (Android, the Mac) needs no source tree.
    let want = include_str!("fixtures/regia/capsule-vectors.tsv");
    let mismatches: Vec<_> = want
        .lines()
        .zip(text.lines())
        .enumerate()
        .filter(|(_, (w, g))| w != g)
        .take(8)
        .collect();
    assert!(
        mismatches.is_empty() && want.lines().count() == text.lines().count(),
        "{} rows want, {} got; first differences: {mismatches:?}",
        want.lines().count(),
        text.lines().count()
    );
}

/// The capsule's vertical rule by hand on the low gallery (floor 0, lid 2.0): feet just under the floor
/// rise onto it; a head inside one radius of the lid is held under it; bad input is returned as is.
#[test]
fn capsule_vertical_rule_by_hand() {
    let gallery = synthetic_gallery(10.0, 10.0, 0.0, 2.0, false);
    let geom = native_mesh(&gallery.triangles, 16.0);
    let state = geom.new_state();
    let (_, b) = geom.push_capsule(&state, 0.0, 0.0, 0.35, Band { lo: -0.1, hi: 1.75 });
    assert_eq!(b, Band { lo: 0.0, hi: 1.75 });
    let (_, b) = geom.push_capsule(&state, 0.0, 0.0, 0.35, Band { lo: 0.3, hi: 2.15 });
    assert_eq!(b, Band { lo: 0.3, hi: 2.0 });
    let b = geom.clamp_capsule(&state, 0.0, 0.0, 0.35, Band { lo: 0.1, hi: 1.6 });
    assert_eq!(b, Band { lo: 0.1, hi: 1.6 }, "nothing to meet");
    let (p, b) = geom.push_capsule(&state, 0.0, 0.0, f32::NAN, Band { lo: 0.0, hi: 1.0 });
    assert_eq!((p.contacts, b), (0, Band { lo: 0.0, hi: 1.0 }));
}

/// A glTF of world-space triangles (one mesh), in memory.
fn tris_gltf(tris: &[[[f32; 3]; 3]]) -> GltfMemory {
    let mut bin = Vec::new();
    for t in tris {
        for p in t {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
    }
    let pos_len = bin.len();
    for i in 0..(tris.len() * 3) as u32 {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let text = format!(
        r#"{{"buffers":[{{"uri":"shell.bin","byteLength":{total}}}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{pos_len}}},{{"buffer":0,"byteOffset":{pos_len},"byteLength":{idx_len}}}],
        "accessors":[{{"bufferView":0,"componentType":5126,"type":"VEC3","count":{verts}}},{{"bufferView":1,"componentType":5125,"type":"SCALAR","count":{verts}}}],
        "meshes":[{{"name":"Shell","primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
        "nodes":[{{"mesh":0}}]}}"#,
        total = bin.len(),
        idx_len = bin.len() - pos_len,
        verts = tris.len() * 3,
    );
    GltfMemory::new("shell.gltf", text).with_buffer("shell.bin", bin)
}

/// An axis box as twelve triangles (the crate a dresser drops in the gallery).
fn box_tris(min: [f32; 3], max: [f32; 3]) -> Vec<[[f32; 3]; 3]> {
    let p = |i: usize| {
        [
            if i & 1 == 0 { min[0] } else { max[0] },
            if i & 2 == 0 { min[1] } else { max[1] },
            if i & 4 == 0 { min[2] } else { max[2] },
        ]
    };
    let quads = [
        [0, 1, 3, 2],
        [4, 6, 7, 5],
        [0, 4, 5, 1],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 5, 7, 3],
    ];
    quads
        .iter()
        .flat_map(|q| [[p(q[0]), p(q[1]), p(q[2])], [p(q[0]), p(q[2]), p(q[3])]])
        .collect()
}

/// Acceptance 4 (plan row W-30): the on-foot grid built for a map with layers holds no canonical
/// triangle. The set piece is the gallery's own shell (as the art draws the tunnel lining over the sim
/// triangles) plus a 1 m crate; the dressing grid keeps the crate and nothing of the shell. Control:
/// the Stage A loader on the same glTF stamps the shell's walls.
#[test]
fn tunnel_shells_never_reach_the_dressing_grid() {
    let gallery = synthetic_gallery(10.0, 10.0, -5.0, 1.0, false);
    let geom = native_mesh(&gallery.triangles, 16.0);
    let state = geom.new_state();
    let mut drawn: Vec<[[f32; 3]; 3]> = gallery.triangles.iter().map(|t| [t.a, t.b, t.c]).collect();
    let shell_walls = gallery
        .triangles
        .iter()
        .filter(|t| t.kind == TunnelFaceKind::Wall)
        .count();
    assert!(shell_walls > 0);
    drawn.extend(box_tris([2.0, -5.0, 2.0], [3.0, -4.0, 3.0]));
    let gltf = tris_gltf(&drawn);
    let terrain = TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 40.0);

    let grid = dressing_grid(&ON_FOOT_PARAMS, &gltf, Some(&terrain), &geom, &state).unwrap();
    assert!(!grid.cells.is_empty(), "the crate is dressing and stays");
    for (&(i, k), cell) in &grid.cells {
        let (x, z) = ((i as f32 + 0.5) * 0.75, (k as f32 + 0.5) * 0.75);
        assert!(
            (1.0..=4.0).contains(&x) && (1.0..=4.0).contains(&z),
            "cell ({i}, {k}) at ({x}, {z}) is shell, not the crate: {cell:?}"
        );
    }
    // Control: the Stage A loader keeps the shell's walls along the gallery's sides.
    let stage_a = load_gltf_grid_on::<()>(&ON_FOOT_PARAMS, &gltf, true, Some(&terrain), None).unwrap();
    let shell_cells = stage_a
        .cells
        .iter()
        .filter(|((i, _), c)| c.has_wall() && (*i as f32 * 0.75).abs() > 8.0)
        .count();
    assert!(
        shell_cells > 10,
        "the Stage A grid stamps the shell: {shell_cells}"
    );
}

/// The mover pin (A-10): a scripted walk on each fixture, every answer's bits hashed.
fn mover_hash(geom: &WorldGeom, start: [f32; 3], steer: impl Fn(u32) -> [f32; 2], ticks: u32) -> String {
    let state = geom.new_state();
    let mut h = blake3::Hasher::new();
    let mut feet = start;
    for tick in 0..ticks {
        let m = move_capsule(geom, &state, &WALKER, feet, steer(tick), true);
        for v in [
            m.feet[0],
            m.feet[1],
            m.feet[2],
            m.band.lo,
            m.band.hi,
            m.support.floor.y,
        ] {
            h.update(&v.to_bits().to_le_bytes());
        }
        h.update(&m.contacts.to_le_bytes());
        h.update(&[u8::from(m.airborne), m.stop as u8]);
        feet = m.feet;
    }
    h.finalize().to_hex().to_string()
}

/// The mover pins, recorded on x86-64 Linux at `QUERY_BITS_VERSION` 7 on lane W-30, which integ-v06r-det
/// numbered 8 (ADR-8531: W-13 took 7; W-30's own answers did not move).
const MOVER_PINS: [(&str, &str); 3] = [
    (
        "gallery_box",
        "1c4ca6049fa6d27f403188854423dc9e5fe709f348f3823403ae308f975776c5",
    ),
    (
        "four_decks",
        "72d7c6e75a0c9d9e3508af502449099523f2d6013d63e1186112d90104a64e6d",
    ),
    (
        "switchbacks_cone",
        "5310b841b90aa99f0767d6c909ddadd30dabf9e56464af6a48a5dc7f27ecc9a3",
    ),
];

/// Acceptance 5 (A-10, the new mover pin): a walker boxing the gallery's walls, crossing the four-deck
/// stack and climbing the mid-east rubble cone hashes the same on every arch.
#[test]
fn mover_walks_are_pinned_on_every_arch() {
    assert_eq!(
        QUERY_BITS_VERSION, 8,
        "QUERY_BITS_VERSION moved: re-record the mover pins"
    );
    let gallery = native_mesh(&synthetic_gallery(10.0, 10.0, -5.0, 1.0, false).triangles, 16.0);
    let decks_file = read(FIXTURES[2]);
    let decks = native(&decks_file, FIXTURES[2], 16.0);
    let deck0 = {
        let row = &decks_file["columns"][0];
        let (x, z) = (hex(&row["x"]), hex(&row["z"]));
        let y = hex(&row["at"][0][0]);
        [x, decks.support(&decks.new_state(), x, z, y).floor.y, z]
    };
    let cone = &coned().geom;
    let [cx, cz] = PILE;
    let cone_start = [
        cx - 6.5,
        floor_at(cone, &cone.new_state(), cx - 6.5, cz, -270.0),
        cz,
    ];
    let got = [
        (
            "gallery_box",
            mover_hash(
                &gallery,
                [-8.0, -5.0, -8.0],
                |t| match (t / 80) % 4 {
                    0 => [RUN * 2.0, 0.3],
                    1 => [-0.3, RUN * 2.0],
                    2 => [-RUN * 2.0, -0.3],
                    _ => [0.3, -RUN * 2.0],
                },
                320,
            ),
        ),
        (
            "four_decks",
            mover_hash(
                &decks,
                deck0,
                |t| [RUN, if t % 50 < 25 { RUN } else { -RUN }],
                240,
            ),
        ),
        (
            "switchbacks_cone",
            mover_hash(cone, cone_start, |t| [RUN, if t < 60 { 0.0 } else { RUN }], 160),
        ),
    ];
    for (name, hash) in &got {
        eprintln!("MOVER_PIN {name} {hash}");
    }
    let moved: Vec<_> = got
        .iter()
        .zip(MOVER_PINS)
        .filter(|((_, g), (_, w))| g != w)
        .map(|((n, g), _)| (*n, g.clone()))
        .collect();
    assert!(moved.is_empty(), "mover pins moved: {moved:?}");
}

/// The probe (the agent read of W-30): in the low gallery a walker on the floor is covered and fits;
/// one standing 0.3 m up has its head under the 2.0 m lid and does not fit; bad input does not fit.
#[test]
fn body_probe_reads_band_and_support() {
    let gallery = synthetic_gallery(10.0, 10.0, 0.0, 2.0, false);
    let geom = native_mesh(&gallery.triangles, 16.0);
    let state = geom.new_state();
    let p = regia_world_geom::mover::body_probe(&geom, &state, &WALKER, [0.0, 0.0, 0.0]);
    assert!(p.covered && p.fits, "{p:?}");
    assert_eq!(p.support.floor.y, 0.0);
    assert_eq!(p.band, Band { lo: 0.0, hi: 1.85 });
    let p = regia_world_geom::mover::body_probe(&geom, &state, &WALKER, [0.0, 0.3, 0.0]);
    assert!(!p.fits && p.band.hi == 2.0, "{p:?}");
    let p = regia_world_geom::mover::body_probe(&geom, &state, &WALKER, [f32::NAN, 0.0, 0.0]);
    assert!(!p.fits, "{p:?}");
}
