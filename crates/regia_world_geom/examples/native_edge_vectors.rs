// SPDX-License-Identifier: MPL-2.0
//! Regenerate only exact shared-edge rows permitted by spec 121 §15.4.
#![allow(clippy::unwrap_used)]
#[path = "../tests/native_support/mod.rs"]
pub mod native_support;
use native_support::*;
use regia_world_geom::{SpanBuf, SurfaceKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[expect(
    clippy::disallowed_methods,
    reason = "host tool: the output directory argument, read once before any query (ADR-8367)"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os()
        .nth(1)
        .ok_or("usage: native_edge_vectors OUTPUT_DIRECTORY")?;
    let output = std::path::PathBuf::from(output);
    std::fs::create_dir_all(&output)?;
    for name in &FIXTURES[..3] {
        let mut file = read(name);
        let geom = native(&file, name, 16.0);
        let state = geom.new_state();
        let reference = mesh(&file, name);
        let conditioned: Vec<_> = geom
            .triangles()
            .iter()
            .filter(|t| {
                t.kind == regia_world_geom::cooked::TriKind::Floor
                    && t.flags & regia_world_geom::cooked::TRI_FLAG_NO_SUPPORT == 0
            })
            .map(|t| t.v.map(|p| p.map(regia_world_geom::m_to_mm)))
            .collect();
        let source: Vec<_> = reference
            .support_floors()
            .iter()
            .map(|t| [t.a, t.b, t.c].map(|p| p.map(regia_world_geom::m_to_mm)))
            .collect();
        let roofs: Vec<_> = geom
            .triangles()
            .iter()
            .filter(|t| t.kind == regia_world_geom::cooked::TriKind::Roof)
            .map(|t| t.v.map(|p| p.map(regia_world_geom::m_to_mm)))
            .collect();
        let source_roofs: Vec<_> = reference
            .triangles
            .iter()
            .filter(|t| t.kind == regia_world_geom::compat::mesh::TunnelFaceKind::Ceiling)
            .map(|t| [t.a, t.b, t.c].map(|p| p.map(regia_world_geom::m_to_mm)))
            .collect();

        let mut edges = BTreeMap::new();
        for tri in geom.triangles() {
            if !matches!(
                tri.kind,
                regia_world_geom::cooked::TriKind::Floor | regia_world_geom::cooked::TriKind::Roof
            ) {
                continue;
            }
            for i in 0..3 {
                let a = tri.v[i].map(regia_world_geom::m_to_mm);
                let b = tri.v[(i + 1) % 3].map(regia_world_geom::m_to_mm);
                let key = if a < b { (a, b) } else { (b, a) };
                *edges.entry(key).or_insert(0usize) += 1;
            }
        }
        let shared: Vec<_> = edges
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|((a, b), _)| {
                let (a, b) = (a.map(|v| v as f32 / 1000.0), b.map(|v| v as f32 / 1000.0));
                if a[0] < b[0] || (a[0] == b[0] && a[2] < b[2]) {
                    (a, b)
                } else {
                    (b, a)
                }
            })
            .collect();
        let mut changed = Vec::new();
        let mut reasons = Vec::new();
        for (index, row) in file["columns"].as_array().unwrap().iter().enumerate() {
            let (x, z) = (hex(&row["x"]), hex(&row["z"]));
            let on_shared = shared.iter().any(|(a, b)| {
                let (dx, dz) = (b[0] - a[0], b[2] - a[2]);
                (dx != 0.0 || dz != 0.0)
                    && dx * (z - a[2]) - dz * (x - a[0]) == 0.0
                    && x >= regia_detsim::float::det_min(a[0], b[0])
                    && x <= regia_detsim::float::det_max(a[0], b[0])
                    && z >= regia_detsim::float::det_min(a[2], b[2])
                    && z <= regia_detsim::float::det_max(a[2], b[2])
            });
            if !on_shared {
                continue;
            }
            let near = |tri: &&[[i32; 3]; 3]| {
                let mut lo = [f32::INFINITY; 2];
                let mut hi = [f32::NEG_INFINITY; 2];
                for p in *tri {
                    for (k, axis) in [0, 2].into_iter().enumerate() {
                        let v = p[axis] as f32 / 1000.0;
                        lo[k] = regia_detsim::float::det_min(lo[k], v);
                        hi[k] = regia_detsim::float::det_max(hi[k], v);
                    }
                }
                x >= lo[0] - 0.25 && x <= hi[0] + 0.25 && z >= lo[1] - 0.25 && z <= hi[1] + 0.25
            };
            if conditioned.iter().filter(near).ne(source.iter().filter(near))
                || roofs.iter().filter(near).ne(source_roofs.iter().filter(near))
            {
                continue;
            }
            let mut spans = SpanBuf::default();
            geom.floors_at(&state, x, z, &mut spans);
            let want = row["spans"].as_array().unwrap();
            if spans.overflowed || spans.spans().len() != want.len() {
                continue;
            }
            let permitted = spans.spans().iter().zip(want).all(|(s, w)| {
                let kind = if s.floor.kind == SurfaceKind::Ground {
                    "ground"
                } else {
                    "mesh"
                };
                kind == w[2]
                    && (kind != "ground" || s.floor.y.to_bits() == hex(&w[0]).to_bits())
                    && s.ceiling.is_some() != w[1].is_null()
                    && (s.floor.y - hex(&w[0])).abs() <= 0.001
                    && s.ceiling.is_none_or(|l| (l.y - hex(&w[1])).abs() <= 0.001)
            });
            if !permitted {
                continue;
            }
            let mut replacement = row.clone();
            replacement["spans"] = json!(
                spans
                    .spans()
                    .iter()
                    .map(|s| json!([
                        format!("{:08x}", s.floor.y.to_bits()),
                        s.ceiling.map(|l| format!("{:08x}", l.y.to_bits())),
                        if s.floor.kind == SurfaceKind::Ground {
                            "ground"
                        } else {
                            "mesh"
                        }
                    ]))
                    .collect::<Vec<_>>()
            );
            for at in replacement["at"].as_array_mut().unwrap() {
                let answer = geom.support(&state, x, z, hex(&at[0]));
                let selected = spans
                    .spans()
                    .iter()
                    .position(|s| *s == answer)
                    .ok_or("support absent from column")?;
                at[1] = json!(selected);
            }
            if replacement != *row {
                reasons.push(json!({"original_column":index,"reason":"exact shared edge; canonical ownership; every height changes by at most 1 mm"}));
                changed.push(replacement);
            }
        }
        eprintln!("REGIA_EDGE {name} changed_columns={}", changed.len());
        file["columns"] = Value::Array(changed);
        file["source"] = json!("regia: native canonical shared-edge ownership; ADR-8246");
        file["provenance"] = json!("regia");
        file["query_bits_version"] = json!(regia_world_geom::QUERY_BITS_VERSION);
        file["regia_changes"] = Value::Array(reasons);
        let path = output.join(format!("{name}.json"));
        let temporary = output.join(format!(".{name}-{}.tmp", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec_pretty(&file)?)?;
        std::fs::rename(temporary, path)?;
    }
    Ok(())
}
