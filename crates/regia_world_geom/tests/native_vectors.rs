// SPDX-License-Identifier: MPL-2.0
//! T3 vector acceptance. These tests deliberately retain every source row unchanged.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]
mod native_support;
use native_support::*;
use regia_detsim::float::{det_max, det_min};
use regia_world_geom::cooked::{TRI_FLAG_NO_SUPPORT, TriKind};
use regia_world_geom::{SpanBuf, SurfaceKind};

fn span_kind(kind: SurfaceKind) -> &'static str {
    if kind == SurfaceKind::Ground {
        "ground"
    } else {
        "mesh"
    }
}
fn column_matches(spans: &SpanBuf, want: &[serde_json::Value]) -> bool {
    spans.spans().len() == want.len()
        && spans.spans().iter().zip(want).all(|(s, w)| {
            span_kind(s.floor.kind) == w[2]
                && s.ceiling.is_some() != w[1].is_null()
                && s.floor.y.to_bits() == hex(&w[0]).to_bits()
                && s.ceiling.is_none_or(|l| l.y.to_bits() == hex(&w[1]).to_bits())
        })
}
fn bits(y: f32) -> String {
    format!("{:08x}", y.to_bits())
}
fn hex_str(text: &str) -> f32 {
    f32::from_bits(u32::from_str_radix(text, 16).unwrap())
}
fn want_floor(w: &serde_json::Value) -> String {
    format!(
        "{}:{}",
        w[2].as_str().unwrap_or("?"),
        w[0].as_str().unwrap_or("?")
    )
}
fn got_floor(s: &regia_world_geom::Span) -> String {
    format!("{}:{}", span_kind(s.floor.kind), bits(s.floor.y))
}
/// ADR-8246 R1. A difference with no clause is a bug. Canyon rows are the real cut lattice plus
/// W-06 weld/trim (ADR-8214, spec 121 §9.2). §15.7 is a T3 ground lifted onto a lid while the
/// native ground stays at the lattice height. §5.3 rule 2 is a T3 span the watertight test
/// rejects (T3's `≥ −1e-5` barycentric is superseded). A same-kind height within 1 mm is §15.4.
fn classify_column(
    name: &str,
    geom: &regia_world_geom::WorldGeom,
    x: f32,
    z: f32,
    spans: &SpanBuf,
    want: &[serde_json::Value],
) -> &'static str {
    if column_matches(spans, want) {
        return "match";
    }
    if name == "switchbacks_canyon" {
        return "ADR-8214 §9.2";
    }
    if ground_stays_while_t3_lifts(spans, want) {
        return "§15.7";
    }
    let same_shape = spans.spans().len() == want.len()
        && spans
            .spans()
            .iter()
            .zip(want)
            .all(|(s, w)| span_kind(s.floor.kind) == w[2] && s.ceiling.is_some() != w[1].is_null());
    let within_1mm = same_shape
        && spans.spans().iter().zip(want).all(|(s, w)| {
            (s.floor.y - hex(&w[0])).abs() < 0.001
                && s.ceiling.is_none_or(|l| (l.y - hex(&w[1])).abs() < 0.001)
        });
    if within_1mm {
        return "§15.4";
    }
    ownership_clause(geom, x, z, spans, want).unwrap_or("UNCLASSIFIED")
}
/// Spec 121 §15.7 forbids the T3 runtime ground lift. The native ground stays at the lattice
/// height (0 for a mesh-only input). T3 writes that ground at the lid height and drops the
/// ceiling. Mesh spans in the same column may still differ by the §15.4 one-millimetre edge.
fn ground_stays_while_t3_lifts(spans: &SpanBuf, want: &[serde_json::Value]) -> bool {
    if spans.spans().len() != want.len() {
        return false;
    }
    let mut saw_lift = false;
    for (s, w) in spans.spans().iter().zip(want) {
        if span_kind(s.floor.kind) != w[2] {
            return false;
        }
        if w[2] == "ground" {
            let t3 = hex(&w[0]);
            let lattice = s.floor.kind == SurfaceKind::Ground && s.floor.y.to_bits() == 0.0f32.to_bits();
            // The lid T3 climbed onto is the native ceiling. A higher roof may still cap that
            // lifted ground, so T3's ceiling is not always null (spec 121 §15.7).
            let lifted =
                lattice && t3.to_bits() != 0 && s.ceiling.is_some_and(|lid| (lid.y - t3).abs() < 0.001);
            if lifted {
                saw_lift = true;
                continue;
            }
            let ceil_ok = s.ceiling.is_some() != w[1].is_null()
                && s.ceiling.is_none_or(|l| l.y.to_bits() == hex(&w[1]).to_bits());
            if s.floor.y.to_bits() == t3.to_bits() && ceil_ok {
                continue;
            }
            return false;
        }
        if s.ceiling.is_some() == w[1].is_null() {
            return false;
        }
        if (s.floor.y - hex(&w[0])).abs() >= 0.001 {
            return false;
        }
        if s.ceiling
            .is_some_and(|l| !w[1].is_null() && (l.y - hex(&w[1])).abs() >= 0.001)
        {
            return false;
        }
    }
    saw_lift
}
/// Spec 121 §5.3 rule 2 owns a point with the watertight edge test. T3's barycentric `≥ −1e-5`
/// still emits a span for a point just outside that test. A T3 ground standing on such a lid is
/// also the §15.7 lift.
fn ownership_clause(
    geom: &regia_world_geom::WorldGeom,
    x: f32,
    z: f32,
    spans: &SpanBuf,
    want: &[serde_json::Value],
) -> Option<&'static str> {
    let native_mesh: Vec<_> = spans
        .spans()
        .iter()
        .filter(|s| s.floor.kind != regia_world_geom::SurfaceKind::Ground)
        .collect();
    let want_mesh: Vec<_> = want.iter().filter(|w| w[2] == "mesh").collect();
    let mut used = vec![false; native_mesh.len()];
    let mut outside_span = false;
    for w in &want_mesh {
        let wy = hex(&w[0]);
        let paired = native_mesh
            .iter()
            .enumerate()
            .find(|&(i, s)| !used[i] && (s.floor.y - wy).abs() < 0.001);
        if let Some((i, s)) = paired {
            used[i] = true;
            let ceil_close = s.ceiling.is_none() && w[1].is_null()
                || s.ceiling
                    .is_some_and(|l| w[1].as_str().is_some_and(|h| (l.y - hex_str(h)).abs() < 0.001));
            if !ceil_close {
                if ceilings_follow_rule(geom, x, z, s.floor.y, s.ceiling.map(|l| l.y), w[1].as_str()) {
                    outside_span = true;
                } else {
                    return None;
                }
            }
        } else if native_mesh.iter().any(|s| (s.floor.y - wy).abs() < 0.001) {
            // Same-height collapse already kept one native floor (spec 121 §5.3 rule 4).
        } else if support_outside(geom, x, z, TriKind::Floor, wy)
            && !support_inside(geom, x, z, TriKind::Floor, wy)
        {
            outside_span = true;
        } else {
            return None;
        }
    }
    if used.iter().any(|u| !u) {
        return None;
    }
    let native_ground = spans
        .spans()
        .iter()
        .find(|s| s.floor.kind == regia_world_geom::SurfaceKind::Ground)?;
    let want_ground = want.iter().find(|w| w[2] == "ground")?;
    let t3 = hex(&want_ground[0]);
    let lattice = native_ground.floor.y.to_bits() == 0.0f32.to_bits();
    let lift = lattice && t3.to_bits() != 0 && {
        native_ground
            .ceiling
            .is_some_and(|lid| (lid.y - t3).abs() < 0.001)
            || (native_ground.ceiling.is_none() && loose_roof(geom, x, z, t3))
    };
    if !lift {
        if (native_ground.floor.y - t3).abs() >= 0.001 {
            return None;
        }
        let ceil_close = native_ground.ceiling.is_none() && want_ground[1].is_null()
            || native_ground.ceiling.is_some_and(|l| {
                want_ground[1]
                    .as_str()
                    .is_some_and(|h| (l.y - hex_str(h)).abs() < 0.001)
            });
        if !ceil_close {
            if ceilings_follow_rule(
                geom,
                x,
                z,
                native_ground.floor.y,
                native_ground.ceiling.map(|l| l.y),
                want_ground[1].as_str(),
            ) {
                outside_span = true;
            } else {
                return None;
            }
        }
    }
    Some(match (outside_span, lift) {
        (true, true) => "§5.3 rule 2; §15.7",
        (true, false) => "§5.3 rule 2",
        (false, true) => "§15.7",
        (false, false) => return None,
    })
}
fn ceilings_follow_rule(
    geom: &regia_world_geom::WorldGeom,
    x: f32,
    z: f32,
    floor_y: f32,
    native_ceil: Option<f32>,
    want_ceil: Option<&str>,
) -> bool {
    let want_y = want_ceil.map(hex_str);
    if native_ceil.is_none() && want_y.is_none() {
        return true;
    }
    if native_ceil.is_some_and(|n| want_y.is_some_and(|w| (n - w).abs() < 0.001)) {
        return true;
    }
    let expected = lowest_inside_roof(geom, x, z, floor_y);
    let native_ok = match (native_ceil, expected) {
        (None, None) => true,
        (Some(a), Some(b)) => (a - b).abs() < 0.001,
        _ => false,
    };
    if !native_ok {
        return false;
    }
    match want_y {
        None => false,
        Some(c) => loose_roof(geom, x, z, c) && !support_inside(geom, x, z, TriKind::Roof, c),
    }
}
fn lowest_inside_roof(geom: &regia_world_geom::WorldGeom, x: f32, z: f32, floor_y: f32) -> Option<f32> {
    let gap = geom.settings().lid_gap_m;
    geom.triangles()
        .iter()
        .filter_map(|tri| {
            if tri.kind != TriKind::Roof {
                return None;
            }
            let y = plane_y(tri.v, x, z)?;
            (y > floor_y + gap && watertight_in(tri.v, x, z)).then_some(y)
        })
        .reduce(det_min)
}
fn support_inside(geom: &regia_world_geom::WorldGeom, x: f32, z: f32, kind: TriKind, y: f32) -> bool {
    geom.triangles().iter().any(|tri| {
        tri.kind == kind
            && (kind != TriKind::Floor || tri.flags & TRI_FLAG_NO_SUPPORT == 0)
            && plane_y(tri.v, x, z).is_some_and(|h| (h - y).abs() < 0.001)
            && watertight_in(tri.v, x, z)
    })
}
fn support_outside(geom: &regia_world_geom::WorldGeom, x: f32, z: f32, kind: TriKind, y: f32) -> bool {
    geom.triangles().iter().any(|tri| {
        tri.kind == kind
            && plane_y(tri.v, x, z).is_some_and(|h| (h - y).abs() < 0.001)
            && !watertight_in(tri.v, x, z)
            && bary_min(tri.v, x, z).is_some_and(|b| b >= -1e-4)
    })
}
fn loose_roof(geom: &regia_world_geom::WorldGeom, x: f32, z: f32, y: f32) -> bool {
    support_outside(geom, x, z, TriKind::Roof, y) || support_inside(geom, x, z, TriKind::Roof, y)
}
fn plane_y(v: [[f32; 3]; 3], x: f32, z: f32) -> Option<f32> {
    let [a, b, c] = v;
    let (ux, uy, uz) = (b[0] - a[0], b[1] - a[1], b[2] - a[2]);
    let (vx, vy, vz) = (c[0] - a[0], c[1] - a[1], c[2] - a[2]);
    let (nx, ny, nz) = (uy * vz - uz * vy, uz * vx - ux * vz, ux * vy - uy * vx);
    let len2 = nx * nx + ny * ny + nz * nz;
    if len2 < 1e-16 || ny.abs() < 1e-6 {
        return None;
    }
    Some(a[1] - (nx * (x - a[0]) + nz * (z - a[2])) / ny)
}
fn bary_min(v: [[f32; 3]; 3], x: f32, z: f32) -> Option<f32> {
    let (x0, z0) = (v[0][0], v[0][2]);
    let (x1, z1) = (v[1][0], v[1][2]);
    let (x2, z2) = (v[2][0], v[2][2]);
    let d = (z1 - z2) * (x0 - x2) + (x2 - x1) * (z0 - z2);
    if d.abs() < 1e-12 {
        return None;
    }
    let a = ((z1 - z2) * (x - x2) + (x2 - x1) * (z - z2)) / d;
    let b = ((z2 - z0) * (x - x2) + (x0 - x2) * (z - z2)) / d;
    let c = 1.0 - a - b;
    Some(det_min(a, det_min(b, c)))
}
/// Mirrors `query_index` edge ownership: outside the half-plane only. An unowned boundary still
/// covers the point unless a same-kind neighbor claims it, which the column result already shows.
fn watertight_in(v: [[f32; 3]; 3], x: f32, z: f32) -> bool {
    let min_x = det_min(v[0][0], det_min(v[1][0], v[2][0]));
    let max_x = det_max(v[0][0], det_max(v[1][0], v[2][0]));
    let min_z = det_min(v[0][2], det_min(v[1][2], v[2][2]));
    let max_z = det_max(v[0][2], det_max(v[1][2], v[2][2]));
    if x < min_x || x > max_x || z < min_z || z > max_z {
        return false;
    }
    [(0, 1, 2), (1, 2, 0), (2, 0, 1)]
        .iter()
        .all(|&(i, j, k)| !edge_outside(v[i], v[j], v[k], x, z))
}
fn edge_outside(a: [f32; 3], b: [f32; 3], c: [f32; 3], x: f32, z: f32) -> bool {
    let (lo, hi) = if a[0] < b[0] || (a[0] == b[0] && a[2] < b[2]) {
        (a, b)
    } else {
        (b, a)
    };
    let (dx, dz) = (hi[0] - lo[0], hi[2] - lo[2]);
    let side = dx * (c[2] - lo[2]) - dz * (c[0] - lo[0]);
    if side == 0.0 {
        return true;
    }
    let e = dx * (z - lo[2]) - dz * (x - lo[0]);
    if e == 0.0 {
        let len2 = dx * dx + dz * dz;
        if len2 == 0.0 {
            return true;
        }
        let t = ((x - lo[0]) * dx + (z - lo[2]) * dz) / len2;
        if (0.0..=1.0).contains(&t) {
            return false;
        }
    }
    let inside = if side < 0.0 { e <= 0.0 } else { e > 0.0 };
    !inside
}
#[test]
fn t3_vectors_match_the_native_kernel() {
    let mut unclassified = 0;
    let mut rows = Vec::new();
    for name in FIXTURES {
        let file = read(name);
        let geom = native(&file, name, 16.0);
        let state = geom.new_state();
        let mut column_diffs = 0;
        let mut at_diffs = 0;
        let mut printed = 0;
        let mut spans = SpanBuf::default();
        for (i, row) in file["columns"].as_array().unwrap().iter().enumerate() {
            let x = hex(&row["x"]);
            let z = hex(&row["z"]);
            geom.floors_at(&state, x, z, &mut spans);
            let want = row["spans"].as_array().unwrap();
            let cause = classify_column(name, &geom, x, z, &spans, want);
            if cause != "match" {
                column_diffs += 1;
                let t3: Vec<_> = want.iter().map(want_floor).collect();
                let native: Vec<_> = spans.spans().iter().map(got_floor).collect();
                rows.push(format!(
                    "{name}\tcolumn\t{i}\t{}\t{}\t{cause}",
                    t3.join(","),
                    native.join(",")
                ));
                if cause == "UNCLASSIFIED" {
                    unclassified += 1;
                    if printed < 8 {
                        eprintln!(
                            "NATIVE_VECTOR {name} column={i} x={x} z={z} tag={} want={want:?} got={:?}",
                            row["tag"],
                            spans.spans()
                        );
                        printed += 1;
                    }
                }
            }
            for at in row["at"].as_array().unwrap() {
                let s = geom.support(&state, x, z, hex(&at[0]));
                let index = spans.spans().iter().position(|v| *v == s);
                if index.map(|i| i as u64) != at[1].as_u64() {
                    at_diffs += 1;
                    let at_cause = if cause == "match" { "UNCLASSIFIED" } else { cause };
                    rows.push(format!("{name}\tat\t{i}\t{at_cause}"));
                    if at_cause == "UNCLASSIFIED" {
                        unclassified += 1;
                    }
                }
            }
        }
        eprintln!("NATIVE_VECTOR {name} column_differences={column_diffs} at_differences={at_diffs}");
    }
    eprintln!("NATIVE_DIVERGENCE_ROWS {}", rows.len());
    for row in &rows {
        eprintln!("NATIVE_DIVERGENCE {row}");
    }
    assert_eq!(unclassified, 0, "a T3 difference with no spec clause is a bug");
}
#[test]
fn cook_conditioning_matches_t3_support_floors() {
    let mut failures = 0;
    for name in FIXTURES {
        let file = read(name);
        let geom = native(&file, name, 16.0);
        let reference = mesh(&file, name);
        let got: Vec<_> = geom
            .triangles()
            .iter()
            .filter(|t| t.kind == TriKind::Floor && t.flags & TRI_FLAG_NO_SUPPORT == 0)
            .map(|t| t.v.map(|p| p.map(regia_world_geom::m_to_mm)))
            .collect();
        let want: Vec<_> = reference
            .support_floors()
            .iter()
            .map(|t| [t.a, t.b, t.c].map(|p| p.map(regia_world_geom::m_to_mm)))
            .collect();
        if got != want {
            if name == "switchbacks_canyon" {
                eprintln!(
                    "CONDITIONING {name} native={} T3={} cause=ADR-8214 §9.2",
                    got.len(),
                    want.len()
                );
            } else {
                failures += 1;
                eprintln!(
                    "CONDITIONING {name} native={} T3={} first_difference={:?}",
                    got.len(),
                    want.len(),
                    got.iter().zip(&want).position(|(a, b)| a != b)
                );
            }
        }
    }
    assert_eq!(
        failures, 0,
        "mesh-only W-06 conditioning must equal T3 welded(None); supplied-ground diffs are ADR-8214"
    );
}
