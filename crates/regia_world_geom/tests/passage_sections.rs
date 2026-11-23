// SPDX-License-Identifier: MPL-2.0
//! Plan row W-08 (spec 121 § 5.6, § 5.7, § 9.2; ADR-8210): the cook cuts every `Passages` layer into sections, gives
//! each its zone, a blocker plug when it is collapsible, and portals at section cuts, open passage ends and mouths,
//! and cooks every skylight shaft as an open cut (DO27 known issue K8), all in the canonical section.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use regia_detsim::float::{det_clamp_f64, det_max_f64, det_min, det_min_f64};
use regia_detsim::math::{self, sqrt_f64};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::{SampledLayout, TunnelLayout};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, GroundInput, cook, quantize_mesh};
use regia_world_geom::cooked::{CookedZoneShape, TriKind, WorldGeomCooked, content_hash_of};
use regia_world_geom::meshlayers::SectionRule;
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::sections::{cut_sections, is_skylight_kind, open_skylight_shafts};
use regia_world_geom::{TriRef, WorldGeom};

const MAPS: [&str; 3] = ["switchbacks", "atacama_rail", "cinder_steps"];

/// Flat ground at -256 m (the Switchbacks canyon floor), as the Stage A sampler sees it.
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

fn cook_doc(map: &str, layer_extra: &str) -> CookOutput {
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
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    match run(&doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            let waived = waive_ramp_facets(&doc, &report).expect("waive ramp facets");
            run(&waived).unwrap_or_else(|e| panic!("{e} ({})", e.code()))
        }
        Err(e) => panic!("{e} ({})", e.code()),
    }
}

/// The layout as the cook meshes it: sampled on the canyon, skylight shafts opened.
fn sampled(layout: &TunnelLayout) -> SampledLayout {
    let mut s = layout.sample(Some(&canyon_terrain()));
    open_skylight_shafts(&mut s, &layout.openings);
    s
}

/// A canonical triangle: index, kind, section, world mm vertices.
type Tri = (u32, TriKind, Option<u16>, [[i32; 3]; 3]);

/// Every layer-0 canonical triangle, index order, world mm.
fn triangles(c: &WorldGeomCooked) -> Vec<Tri> {
    c.triangles_sorted()
        .into_iter()
        .filter(|(_, t)| t.layer == 0)
        .map(|(o, t)| {
            (
                t.index,
                t.kind,
                t.section,
                t.v.map(|p| [p[0] + o[0], p[1], p[2] + o[1]]),
            )
        })
        .collect()
}

/// Plan row W-08 (`switchbacks_cuts_35_sections`, the plan's name kept): the Switchbacks fixture on the canyon floor
/// cooks into the sections spec 121 § 5.6's rule gives, every canonical passage triangle belongs to exactly one of
/// them, and every section owns triangles. The plan expected 35 (from DO27's unmerged zone emitter, which cut on the
/// old hole rects); the spec's rule gives **37**: the tube 18 (two junction cuts and the 4 m chord rule over 1514 m of
/// bends), the gallery 7, and the four ramps 12 (each cut where its cover changes, the bent east ramp also by the chord
/// rule). The spec wins (ADR-8210); the rule was not tuned to the plan's number.
#[test]
fn switchbacks_cuts_35_sections() {
    let out = cook_doc("switchbacks", "");
    let c = &out.file.canonical;
    let per_passage: BTreeMap<&str, usize> = c.sections.iter().fold(BTreeMap::new(), |mut m, s| {
        *m.entry(s.source.as_str()).or_default() += 1;
        m
    });
    assert_eq!(
        c.sections.len(),
        37,
        "sections per passage: {per_passage:?} (covered: {:?})",
        c.sections.iter().map(|s| s.covered).collect::<Vec<_>>()
    );
    let tris = triangles(c);
    assert!(tris.iter().all(|t| t.2.is_some()), "a triangle has no section");
    let mut owned = BTreeSet::new();
    for s in &c.sections {
        assert!(!s.triangles.is_empty(), "section {} owns no triangle", s.id);
        for r in &s.triangles {
            for i in r.start..r.end {
                assert!(owned.insert(i), "triangle {i} is in two sections");
                assert_eq!(tris[i as usize].2, Some(s.id), "triangle {i}'s section field");
            }
        }
    }
    assert_eq!(owned.len(), tris.len());
    assert_eq!(out.report.sections.len(), c.sections.len());
    assert_eq!(out.report.layers[0].sections as usize, c.sections.len());
}

/// The cross-section portal planes: (point, normal, bbox lo, bbox hi), metres.
type Plane = ([f64; 3], [f64; 3], [f64; 3], [f64; 3]);

fn portal_plane(poly: &[[i32; 3]]) -> Plane {
    let p: Vec<[f64; 3]> = poly.iter().map(|q| q.map(|c| f64::from(c) / 1000.0)).collect();
    let (u, w) = (
        [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]],
        [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]],
    );
    let n = [
        u[1] * w[2] - u[2] * w[1],
        u[2] * w[0] - u[0] * w[2],
        u[0] * w[1] - u[1] * w[0],
    ];
    let len = sqrt_f64(n[0] * n[0] + n[1] * n[1] + n[2] * n[2]);
    let n = n.map(|c| c / len);
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for q in &p {
        for i in 0..3 {
            lo[i] = det_min_f64(lo[i], q[i]);
            hi[i] = det_max_f64(hi[i], q[i]);
        }
    }
    (p[0], n, lo, hi)
}

/// Whether a point lies in a portal's cut: on its plane (within 0.2 m) and inside its XZ box padded 4 m (the open-cut
/// lips reach 3 m past the walls), at any height (an open cut's lips run from the rim to the ground).
fn on_portal(q: [f64; 3], (o, n, lo, hi): &Plane) -> bool {
    let d = (q[0] - o[0]) * n[0] + (q[1] - o[1]) * n[1] + (q[2] - o[2]) * n[2];
    d.abs() <= 0.2 && q[0] >= lo[0] - 4.0 && q[0] <= hi[0] + 4.0 && q[2] >= lo[2] - 4.0 && q[2] <= hi[2] + 4.0
}

/// W-21's VisCell input: with every portal closed, a flood fill over the canonical triangles (through shared edges)
/// from each section reaches no other section; with them open, the portal graph joins every section and the outdoors
/// (the Switchbacks network is one cave system).
#[test]
fn portals_close_every_section() {
    for map in MAPS {
        let out = cook_doc(map, "");
        let c = &out.file.canonical;
        assert!(
            !c.sections.is_empty() && !c.portals.is_empty(),
            "{map}: no sections or portals"
        );
        let planes: Vec<Plane> = c
            .portals
            .iter()
            .filter(|p| p.sides.iter().all(Option::is_some))
            .map(|p| portal_plane(&p.polygon_mm))
            .collect();
        let tris = triangles(c);
        let key = |a: [i32; 3], b: [i32; 3]| if a <= b { (a, b) } else { (b, a) };
        let mut edges: BTreeMap<([i32; 3], [i32; 3]), Vec<usize>> = BTreeMap::new();
        for (i, t) in tris.iter().enumerate() {
            let [a, b, cc] = t.3;
            for (p, q) in [(a, b), (b, cc), (cc, a)] {
                edges.entry(key(p, q)).or_default().push(i);
            }
        }
        let m = |p: [i32; 3]| p.map(|v| f64::from(v) / 1000.0);
        let mut crossings = 0usize;
        let mut open_edges = Vec::new();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); tris.len()];
        for ((p, q), users) in &edges {
            let closed = planes
                .iter()
                .any(|pl| on_portal(m(*p), pl) && on_portal(m(*q), pl));
            if !closed && users.iter().any(|&a| tris[a].2 != tris[users[0]].2) {
                open_edges.push((
                    p,
                    q,
                    users
                        .iter()
                        .map(|&a| (tris[a].0, tris[a].1, tris[a].2))
                        .collect::<Vec<_>>(),
                ));
            }
            for &a in users {
                for &b in users {
                    if a == b {
                        continue;
                    }
                    if tris[a].2 != tris[b].2 {
                        crossings += 1;
                        if closed {
                            continue;
                        }
                    }
                    adj[a].push(b);
                }
            }
        }
        assert!(
            crossings > 0,
            "{map}: no two sections share an edge (the test would be empty)"
        );
        assert!(
            open_edges.is_empty(),
            "{map}: section edges no portal closes: {:?}",
            &open_edges[..open_edges.len().min(6)]
        );
        for start in 0..tris.len() {
            let own = tris[start].2;
            let mut seen = vec![false; tris.len()];
            let mut stack = vec![start];
            seen[start] = true;
            while let Some(t) = stack.pop() {
                assert_eq!(
                    tris[t].2, own,
                    "{map}: section {own:?} leaks into section {:?} at triangle {t} with every portal closed",
                    tris[t].2
                );
                for &n in &adj[t] {
                    if !seen[n] {
                        seen[n] = true;
                        stack.push(n);
                    }
                }
            }
        }
        // Open: the portal graph (outdoors = node `sections.len()`) is connected.
        let outdoor = c.sections.len();
        let mut graph: Vec<Vec<usize>> = vec![Vec::new(); outdoor + 1];
        for p in &c.portals {
            let side = |s: Option<u16>| s.map_or(outdoor, usize::from);
            let (a, b) = (side(p.sides[0]), side(p.sides[1]));
            graph[a].push(b);
            graph[b].push(a);
        }
        let mut seen = vec![false; outdoor + 1];
        let mut stack = vec![outdoor];
        seen[outdoor] = true;
        while let Some(n) = stack.pop() {
            for &m in &graph[n] {
                if !seen[m] {
                    seen[m] = true;
                    stack.push(m);
                }
            }
        }
        assert!(
            seen.iter().all(|s| *s),
            "{map}: open portals leave a section unreachable: {seen:?}"
        );
        let mut pairs = BTreeSet::new();
        for p in c.portals.iter().filter(|p| p.sides.iter().all(Option::is_some)) {
            assert!(
                pairs.insert((p.sides, p.nav_mm)),
                "{map}: portal {} duplicates another",
                p.id
            );
        }
        for s in &c.sections {
            for &p in &s.portals {
                assert!(
                    c.portals[usize::from(p)].sides.contains(&Some(s.id)),
                    "{map}: section {} lists portal {p}",
                    s.id
                );
            }
        }
    }
}

/// XZ convex polygons overlap by more than `eps` metres (separating-axis test on every edge normal).
fn overlap(a: &[[f64; 2]], b: &[[f64; 2]], eps: f64) -> bool {
    for poly in [a, b] {
        for i in 0..poly.len() {
            let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
            let n = [-(q[1] - p[1]), q[0] - p[0]];
            let len = sqrt_f64(n[0] * n[0] + n[1] * n[1]);
            if len == 0.0 {
                continue;
            }
            let n = [n[0] / len, n[1] / len];
            let proj = |pts: &[[f64; 2]]| {
                pts.iter()
                    .map(|v| v[0] * n[0] + v[1] * n[1])
                    .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| {
                        (det_min_f64(lo, x), det_max_f64(hi, x))
                    })
            };
            let ((alo, ahi), (blo, bhi)) = (proj(a), proj(b));
            if ahi - blo <= eps || bhi - alo <= eps {
                return false;
            }
        }
    }
    true
}

/// One footprint quad of a shaft: its XZ corners and the lowest floor height under it, metres.
type Quad = ([[f64; 2]; 4], f64);

/// The footprint of every skylight opening: its ramp's floor quads over the climb (from where the ramp leaves its
/// base passage to the top), metres. The shaft column is everything over those floors.
fn skylight_footprints(layout: &TunnelLayout) -> Vec<(String, Vec<Quad>)> {
    let raw = layout.sample(Some(&canyon_terrain()));
    let mut out = Vec::new();
    for o in layout.openings.iter().filter(|o| is_skylight_kind(&o.kind)) {
        let p = raw.passage(&o.ramp).unwrap();
        let y0 = p.samples[0].p[1];
        let half = f64::from(p.width_m) * 0.5;
        let corner = |k: usize, sign: f64| {
            let s = &p.samples[k];
            [
                f64::from(s.p[0]) + f64::from(s.left[0]) * half * sign,
                f64::from(s.p[2]) + f64::from(s.left[1]) * half * sign,
            ]
        };
        let quads: Vec<Quad> = (0..p.samples.len() - 1)
            .filter(|&k| p.samples[k + 1].p[1] != y0)
            .map(|k| {
                (
                    [
                        corner(k, 1.0),
                        corner(k, -1.0),
                        corner(k + 1, -1.0),
                        corner(k + 1, 1.0),
                    ],
                    f64::from(det_min(p.samples[k].p[1], p.samples[k + 1].p[1])),
                )
            })
            .collect();
        out.push((o.id.clone(), quads));
    }
    out
}

/// Lids in a shaft column: over a footprint quad in XZ and higher than its floor (a passage under the shaft keeps its
/// own lid).
fn lids_over(tris: &[[[i32; 3]; 3]], quads: &[Quad]) -> usize {
    tris.iter()
        .filter(|t| {
            let xz: Vec<[f64; 2]> = t
                .iter()
                .map(|p| [f64::from(p[0]) / 1000.0, f64::from(p[2]) / 1000.0])
                .collect();
            let top = t
                .iter()
                .map(|p| f64::from(p[1]) / 1000.0)
                .fold(f64::NEG_INFINITY, det_max_f64);
            quads
                .iter()
                .any(|(q, floor)| top > *floor + 0.05 && overlap(&xz, q, 0.05))
        })
        .count()
}

/// K8 (DO27 handover § 8; its answer: the cook cuts the lid in shaft columns): every skylight opening's footprint (its
/// ramp's climb) has no lid triangle in the canonical set, the shaft's sections are open cuts with a sky portal that
/// names the opening, so a body falling in from the plain meets no lid (W-11's `falling_into_a_skylight_shaft_never_clamps`
/// on this cook). The Stage A mesher on the same layout (the base) does put lids there.
#[test]
fn skylight_shaft_lid_is_cut() {
    for map in MAPS {
        let layout = for_map(map).unwrap();
        let shafts = skylight_footprints(&layout);
        assert!(!shafts.is_empty(), "{map}: no skylight opening");
        let out = cook_doc(map, "");
        let c = &out.file.canonical;
        let roofs: Vec<[[i32; 3]; 3]> = triangles(c)
            .into_iter()
            .filter(|t| t.1 == TriKind::Roof)
            .map(|t| t.3)
            .collect();
        let raw = layout.sample(Some(&canyon_terrain()));
        let base: Vec<[[i32; 3]; 3]> =
            quantize_mesh(&raw.triangles(&layout.openings, Some(&canyon_terrain())))
                .unwrap()
                .into_iter()
                .filter(|t| t.kind == TriKind::Roof)
                .map(|t| t.v)
                .collect();
        let mut base_lids = 0;
        for (opening, quads) in &shafts {
            assert!(!quads.is_empty(), "{map}/{opening}: an empty footprint");
            let over: Vec<_> = triangles(c)
                .into_iter()
                .filter(|t| t.1 == TriKind::Roof && lids_over(&[t.3], quads) > 0)
                .map(|t| (t.0, t.2, t.3))
                .take(6)
                .collect();
            assert_eq!(
                lids_over(&roofs, quads),
                0,
                "{map}/{opening}: a lid over the skylight shaft: {over:?}"
            );
            base_lids += lids_over(&base, quads);
            assert!(
                c.portals
                    .iter()
                    .any(|p| p.mouth.as_deref() == Some(opening.as_str()) && p.sides[1].is_none()),
                "{map}/{opening}: no mouth portal"
            );
        }
        if map == "switchbacks" {
            // The acceptance fixture: the base (Stage A) mesh does have lids over both shafts on the canyon.
            assert!(
                base_lids > 0,
                "{map}: the Stage A mesh has no lid over any shaft (the test would pass on the base)"
            );
            assert!(out.report.layers[0].skylight_segments_opened > 0, "{map}");
        }
    }
}

/// Spec 121 § 5.7, § 5.3 rule 6: a section marked collapsible gets exactly one plug, whose triangles come after the
/// section's own (and after every passage triangle of the layer), are `Blocker` triangles of that section, lie inside
/// the section's bounds, start inactive unless the source says so, and close the section's portals.
#[test]
fn collapsible_section_gets_one_plug() {
    for (extra, active) in [
        (
            "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0)]",
            false,
        ),
        (
            "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0, initially_active: true)]",
            true,
        ),
    ] {
        let out = cook_doc("switchbacks", extra);
        let c = &out.file.canonical;
        assert_eq!(c.plugs.len(), 1, "{extra}");
        let plug = c.plugs[0];
        assert_eq!(plug.initially_active, active);
        let section = &c.sections[usize::from(plug.section)];
        assert_eq!(section.blocker, Some(plug.id));
        assert_eq!(c.sections.iter().filter(|s| s.blocker.is_some()).count(), 1);
        assert!(section.source.ends_with("/gallery"));
        let last_own = section.triangles.iter().map(|r| r.end).max().unwrap();
        assert!(
            plug.triangles.start >= last_own,
            "plug before the section's own triangles"
        );
        let tris = triangles(c);
        let passage_count = tris.iter().filter(|t| t.1 != TriKind::Blocker).count() as u32;
        assert_eq!(
            plug.triangles.start, passage_count,
            "plugs follow every passage triangle"
        );
        assert_eq!(plug.triangles.end as usize, tris.len());
        let [lo, hi] = section.bounds_mm;
        for i in plug.triangles.start..plug.triangles.end {
            let t = &tris[i as usize];
            assert_eq!((t.1, t.2), (TriKind::Blocker, Some(section.id)));
            for v in t.3 {
                for a in 0..3 {
                    assert!(
                        v[a] >= lo[a] - 1 && v[a] <= hi[a] + 1,
                        "plug vertex {v:?} outside {lo:?}..{hi:?}"
                    );
                }
            }
        }
        assert!(plug.triangles.end - plug.triangles.start >= 4);
        assert!(!section.portals.is_empty());
        for &p in &section.portals {
            assert_eq!(c.portals[usize::from(p)].blocker, Some(plug.id));
        }
        let geom = WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap();
        assert_eq!(geom.blockers().len(), 1);
        assert!(geom.triangles().iter().any(|t| t.tri
            == TriRef {
                layer: 0,
                triangle: plug.triangles.start
            }
            && t.kind == TriKind::Blocker));
    }
    // A run on an unknown passage, or crossing a junction, is refused by name.
    for bad in [
        "blockers: [(passage: \"nope\", from_s_m: 0.0, to_s_m: 6.0)]",
        "blockers: [(passage: \"tube\", from_s_m: 0.0, to_s_m: 1500.0)]",
    ] {
        let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
            .to_ron()
            .unwrap();
        let doc = format!(
            "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"m.ron\"), {bad})])"
        );
        let read = |_: &str| Ok(passages.as_bytes().to_vec());
        let err = cook(&CookInputs {
            document: doc.as_bytes(),
            ground: &canyon_ground(),
            terrain: Some(&canyon_terrain()),
            read_file: &read,
        })
        .err()
        .unwrap();
        assert_eq!(err.code(), "meshlayers_invalid", "{bad}: {err}");
    }
}

/// Spec 121 § 5.6: one zone per section, in section order, and each zone's box holds its section's free space: every
/// centreline sample, at the floor, mid-height and the lid, and the floor and lid edges.
#[test]
fn zones_contain_their_samples() {
    for map in MAPS {
        let layout = for_map(map).unwrap();
        let out = cook_doc(map, "");
        let c = &out.file.canonical;
        let s = sampled(&layout);
        let cut = cut_sections(&s, &layout, &SectionRule::default(), &[]).unwrap();
        assert_eq!(cut.len(), c.sections.len(), "{map}");
        assert_eq!(c.zones.len(), c.sections.len(), "{map}");
        for (i, (section, zone)) in cut.iter().zip(&c.zones).enumerate() {
            assert_eq!(usize::from(zone.section), i);
            assert!(!zone.reverb_preset.is_empty() && zone.reverb_size_m > 0.0);
            let CookedZoneShape::OrientedBox {
                center_mm,
                half_mm,
                yaw,
            } = zone.shape
            else {
                panic!("{map}: zone {i} is not a box");
            };
            let (sin, cos) = (f64::from(math::sin(yaw)), f64::from(math::cos(yaw)));
            let p = &s.passages[section.passage];
            let half_w = f64::from(p.width_m) * 0.5;
            for q in &p.samples[section.start..=section.end] {
                for (side, up) in [
                    (0.0, 0.0),
                    (0.0, 0.5),
                    (0.0, 1.0),
                    (1.0, 0.0),
                    (-1.0, 0.0),
                    (1.0, 1.0),
                    (-1.0, 1.0),
                ] {
                    let x = f64::from(q.p[0]) + f64::from(q.left[0]) * half_w * side;
                    let z = f64::from(q.p[2]) + f64::from(q.left[1]) * half_w * side;
                    let y = f64::from(q.p[1]) + f64::from(p.ceiling_m) * up;
                    let (dx, dy, dz) = (
                        x - f64::from(center_mm[0]) / 1000.0,
                        y - f64::from(center_mm[1]) / 1000.0,
                        z - f64::from(center_mm[2]) / 1000.0,
                    );
                    let (u, v) = (dx * cos + dz * sin, -dx * sin + dz * cos);
                    let h = half_mm.map(|c| f64::from(c) / 1000.0);
                    assert!(
                        u.abs() <= h[0] && dy.abs() <= h[1] && v.abs() <= h[2],
                        "{map}: zone {i} misses ({x}, {y}, {z})"
                    );
                }
            }
            let covered = c.sections[i].covered;
            assert_eq!(zone.reverb_preset == "open", !covered, "{map}: zone {i}");
        }
    }
}

/// Spec 121 § 9.2: sections, portals, zones and plugs are canonical output, so two cooks give the same bytes and the
/// report is the same.
#[test]
fn cook_twice_gives_identical_bytes() {
    let extra = "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0)]";
    for map in MAPS {
        let extra = if map == "switchbacks" { extra } else { "" };
        let (a, b) = (cook_doc(map, extra), cook_doc(map, extra));
        let c = &a.file.canonical;
        assert!(
            !c.sections.is_empty() && !c.portals.is_empty() && !c.zones.is_empty(),
            "{map}"
        );
        if map == "switchbacks" {
            assert_eq!(c.plugs.len(), 1);
        }
        assert_eq!(a.file.encode().unwrap(), b.file.encode().unwrap(), "{map}");
        assert_eq!(a.report, b.report, "{map}");
    }
}

/// The canonical bytes (sections, portals, zones, plugs included) are host-independent: the `content_hash` of each
/// fixture on the canyon is pinned, identical on x86-64 and aarch64. Re-pinned by lane v06-section-yaw-canon
/// (ADR-8579): zone half extents cross through `m_to_mm` (round half to even, was the ceiling) and moved every
/// fixture's zones; the canonical zone yaw (the lockstep `atan2`) alone moved none of them.
#[test]
fn section_bytes_are_pinned_across_hosts() {
    const PINS: [(&str, &str); 3] = [
        (
            "switchbacks",
            "f22b2ff16f82d31d79a1b5ac816ce6d36c6db585ad3ff9ca55cde0eca98d4947",
        ),
        (
            "atacama_rail",
            "9530dcd9c4760f3f10f6b1fa51fceec615af6d34de3ff784ad86d774497d74d1",
        ),
        (
            "cinder_steps",
            "16b55b1937e2842066572aad40ffa1fd49f04e4cb90a6d263f0649872f528536",
        ),
    ];
    // `content_hash` in the version 2 section layout (ADR-8816, lane w11-index-build: the BLAKE3 of
    // the hashed head, which holds the hashes of the count body). The definition of the hash moved
    // with the format; the cooked value did not: `PINS` above are the BLAKE3 of the same value's
    // version 1 section (`content_hash` through v0.6.0) and still hold. Recorded on x86-64 Linux
    // (the build server, rustc 1.99.0).
    const V2_PINS: [&str; 3] = [
        "5731e1b3893dd0eff32da82fe2d01a09c9c6c7c9d95e545d4c97c25f1a13d0c7",
        "c48954fafee0e0644587091e223265e37869ceaf7755f6bdc2cbd5005a445e08",
        "32b0888648d3d791a2bb4a7eb3e7072e32433943d4399aef9598ce85ec1c4a0a",
    ];
    let hex = |h: [u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut got = Vec::new();
    let mut got_v2 = Vec::new();
    for (map, _) in PINS {
        let extra = if map == "switchbacks" {
            "blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0)]"
        } else {
            ""
        };
        let out = cook_doc(map, extra);
        let mut v1 = out.file.canonical.clone();
        v1.canonical_version = 1;
        got.push((
            map,
            hex(*blake3::hash(&postcard::to_allocvec(&v1).unwrap()).as_bytes()),
        ));
        got_v2.push(hex(content_hash_of(&out.file.canonical).unwrap()));
    }
    for ((map, want), (_, have)) in PINS.iter().zip(&got) {
        assert_eq!(have, want, "{map}: the canonical bytes moved (all: {got:?})");
    }
    assert_eq!(got_v2, V2_PINS, "content_hash (version 2 layout)");
}

/// The chord rule alone (spec 121 § 5.6): a straight covered tube is one section; a bent one is cut, and in every
/// section every sample lies within `section_chord_m` of the chord between the section's ends, while joining any
/// section to the next sample would break that (the cut is as late as the rule allows).
#[test]
fn chord_rule_cuts_only_where_the_centreline_leaves_the_chord() {
    use regia_world_geom::compat::layout::{LAYOUT_VERSION, Passage, PassageKind, SAMPLE_STEP_M};
    let tube = |points: Vec<[f32; 3]>| TunnelLayout {
        map_id: "chord".into(),
        version: LAYOUT_VERSION,
        sample_step_m: SAMPLE_STEP_M,
        passages: vec![Passage {
            id: "t".into(),
            kind: PassageKind::Tube,
            width_m: 12.0,
            ceiling_m: 8.0,
            points,
        }],
        openings: Vec::new(),
        junctions: Vec::new(),
        crawlways: Vec::new(),
    };
    let rule = SectionRule::default();
    let straight = tube(vec![[-100.0, -40.0, 0.0], [0.0, -40.0, 0.0], [100.0, -40.0, 0.0]]);
    let s = straight.sample(None);
    let cut = cut_sections(&s, &straight, &rule, &[]).unwrap();
    assert_eq!(cut.len(), 1, "{cut:?}");
    let bent = tube(vec![
        [-100.0, -40.0, 0.0],
        [0.0, -40.0, 0.0],
        [40.0, -40.0, 60.0],
        [0.0, -40.0, 140.0],
        [-80.0, -40.0, 160.0],
    ]);
    let s = bent.sample(None);
    let samples = &s.passages[0].samples;
    let cut = cut_sections(&s, &bent, &rule, &[]).unwrap();
    assert!(cut.len() >= 3, "{cut:?}");
    let off = |a: usize, b: usize| {
        let (pa, pb) = (samples[a].p.map(f64::from), samples[b].p.map(f64::from));
        samples[a + 1..b]
            .iter()
            .map(|q| {
                let q = q.p.map(f64::from);
                let ab = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
                let aq = [q[0] - pa[0], q[1] - pa[1], q[2] - pa[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = det_clamp_f64((aq[0] * ab[0] + aq[1] * ab[1] + aq[2] * ab[2]) / l2, 0.0, 1.0);
                let d = [aq[0] - ab[0] * t, aq[1] - ab[1] * t, aq[2] - ab[2] * t];
                sqrt_f64(d[0] * d[0] + d[1] * d[1] + d[2] * d[2])
            })
            .fold(0.0, det_max_f64)
    };
    assert_eq!((cut[0].start, cut[cut.len() - 1].end), (0, samples.len() - 1));
    for (i, c) in cut.iter().enumerate() {
        assert!(off(c.start, c.end) <= 4.0, "section {i} {c:?}");
        if i + 1 < cut.len() {
            assert_eq!(cut[i + 1].start, c.end);
            assert!(
                off(c.start, c.end + 1) > 4.0,
                "section {i} could have taken one more sample"
            );
        }
    }
}

/// Two passages that start at one point open into each other once: the baker's fixture (DO27 `test_tunnel_bake.py`
/// data) starts its tube and its ramp at (-60, -40, 0), and the cook writes one portal between their sections there,
/// not one per passage end.
#[test]
fn two_passages_from_one_point_share_one_portal() {
    let desc = r#"{"passages": [
        {"id": "main", "kind": "tube", "width_m": 22.0, "ceiling_m": 12.0, "points": [[-60.0, -40.0, 0.0], [0.0, -40.0, 40.0], [60.0, -40.0, 0.0]]},
        {"id": "spur", "kind": "gallery", "width_m": 18.0, "ceiling_m": 10.0, "points": [[0.0, -40.0, 40.0], [0.0, -40.0, 110.0]]},
        {"id": "ramp_w", "kind": "ramp", "width_m": 20.0, "ceiling_m": 12.0, "points": [[-60.0, -40.0, 0.0], [-95.0, -40.0, 25.0], [-110.0, -5.0, 45.0]]}],
        "openings": [{"id": "west", "kind": "fracture_ramp", "ramp": "ramp_w", "top": [-110.0, -5.0, 45.0]}],
        "junctions": [{"id": "core", "at": [0.0, -40.0, 40.0], "passages": ["main", "spur"]}],
        "sample_step_m": 5.0}"#;
    let passages = PassagesDocument::import_description(desc, "bore")
        .unwrap()
        .to_ron()
        .unwrap();
    let doc =
        "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"b.ron\"))])";
    let read = |_: &str| Ok(passages.as_bytes().to_vec());
    // Flat ground at -5 m, as `regia terrain cook --ground-y -5 --width-m 512` builds it.
    let count = ((-5.0f32 + 512.0) * 32.0) as u16;
    let ground = GroundInput {
        lattice_n: 2,
        width_m: 512.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![count; 4],
    };
    let flat =
        TerrainSampler::new(HeightGrid::square(2, vec![count; 4]).unwrap(), 512.0).with_zero_counts(16_384);
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&flat),
            read_file: &read,
        })
    };
    let out = match run(doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            let waived = waive_ramp_facets(doc, &report).expect("waive ramp facets");
            run(&waived).unwrap_or_else(|e| panic!("{e} ({})", e.code()))
        }
        Err(e) => panic!("{e} ({})", e.code()),
    };
    let c = &out.file.canonical;
    let at_start: Vec<_> = c
        .portals
        .iter()
        .filter(|p| p.nav_mm[0] == -60_000 && p.nav_mm[2] == 0)
        .collect();
    assert_eq!(at_start.len(), 1, "{at_start:?}");
    let mouth: Vec<_> = c
        .portals
        .iter()
        .filter(|p| p.mouth.as_deref() == Some("west"))
        .collect();
    assert_eq!(mouth.len(), 1);
}
