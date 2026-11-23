// SPDX-License-Identifier: MPL-2.0
//! Input-only native fixtures. Expected answers always come from the unchanged T3 files.
use regia_world_geom::compat::mesh::{TunnelCollisionMesh, TunnelFaceKind, TunnelTriangle};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookInputs, GroundInput, cook, quantize_mesh, trim, weld};
use regia_world_geom::cooked::{CookedTile, CookedTriangle, encode_canonical};
use regia_world_geom::{GeomSettings, WorldGeom};
use serde_json::Value;
use std::collections::BTreeMap;

/// The four pinned T3 input files.
pub const FIXTURES: [&str; 4] = [
    "switchbacks_mesh",
    "cinder_steps_mesh",
    "four_decks_mesh",
    "switchbacks_canyon",
];
/// Decode a source f32 bit pattern without numeric parsing.
pub fn hex(v: &Value) -> f32 {
    f32::from_bits(u32::from_str_radix(v.as_str().unwrap(), 16).unwrap())
}
/// Read a committed source vector file.
#[expect(
    clippy::disallowed_methods,
    reason = "test support: REGIA_WORLD_GEOM_FIXTURES names the vector directory, read before any query (ADR-8367)"
)]
pub fn read(name: &str) -> Value {
    let root = std::env::var_os("REGIA_WORLD_GEOM_FIXTURES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/do27/t3-vectors")
        });
    let path = root.join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
/// Decode the source integer-mm triangles.
pub fn triangles(file: &Value) -> Vec<TunnelTriangle> {
    file["triangles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let p = |i: usize| std::array::from_fn(|j| row[i + j].as_i64().unwrap() as i32 as f32 / 1000.0);
            TunnelTriangle {
                a: p(1),
                b: p(4),
                c: p(7),
                kind: match row[0].as_str().unwrap() {
                    "F" => TunnelFaceKind::Floor,
                    "C" => TunnelFaceKind::Ceiling,
                    "W" => TunnelFaceKind::Wall,
                    _ => panic!("unknown triangle kind"),
                },
                portal: false,
                pilot_only: false,
            }
        })
        .collect()
}
/// The T3 reference terrain, absent for mesh-only inputs.
pub fn terrain(name: &str) -> Option<TerrainSampler> {
    (name == "switchbacks_canyon").then(|| {
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
    })
}
/// The pinned compat mesh, conditioned by T3.
pub fn mesh(file: &Value, name: &str) -> TunnelCollisionMesh {
    TunnelCollisionMesh::new(triangles(file)).welded(terrain(name).as_ref())
}
/// W-06 conditioning and canonical loading of the same source triangles.
///
/// Mesh-only fixtures pass no ground into weld and trim (ADR-8246 R2). The canyon fixture keeps
/// W-10's cut lattice (mouth cuts, ring pins, full 4096 pitch) and welds the T3 triangles against
/// that supplied ground (ADR-8246 R1).
pub fn native(file: &Value, name: &str, cell: f32) -> WorldGeom {
    if name == "switchbacks_canyon" {
        return canyon_native(file, cell);
    }
    // No terrain was supplied. Conditioning must not invent the flat lattice as a weld target.
    native_mesh(&triangles(file), cell)
}
fn canyon_native(file: &Value, cell: f32) -> WorldGeom {
    use regia_world_geom::compat::fixtures::for_map;
    use regia_world_geom::passages::PassagesDocument;
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let layout = for_map("switchbacks").unwrap();
    let fills: Vec<_> = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(&terrain)))
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
    let read = |_: &str| -> Result<Vec<u8>, String> { Ok(passages.as_bytes().to_vec()) };
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // ADR-8359: a test cook waives the two known Switchbacks ramp facets by hash and recooks.
    let mut c = match run(&document) {
        Err(regia_world_geom::cook::CookError::Checks(report))
            if regia_world_geom::checks::only_ramp_facet_failures(&report) =>
        {
            run(&regia_world_geom::checks::waive_ramp_facets(&document, &report).unwrap())
        }
        other => other,
    }
    .unwrap()
    .file
    .canonical;
    let tris = quantize_mesh(&triangles(file)).unwrap();
    place_triangles(&mut c, tris, Some(&ground), cell)
}
fn place_triangles(
    c: &mut regia_world_geom::cooked::WorldGeomCooked,
    mut tris: Vec<regia_world_geom::cook::WorldTriangleMm>,
    ground: Option<&GroundInput>,
    cell: f32,
) -> WorldGeom {
    c.settings = GeomSettings {
        index_cell_m: cell,
        ..c.settings
    };
    weld(&mut tris, ground, &c.lattice);
    trim(&mut tris, ground, &c.lattice);
    let mut tiles: BTreeMap<(i32, i32), Vec<CookedTriangle>> = BTreeMap::new();
    for (i, t) in tris.into_iter().enumerate() {
        let ox = t.v[0][0].div_euclid(1_024_000) * 1_024_000;
        let oz = t.v[0][2].div_euclid(1_024_000) * 1_024_000;
        tiles.entry((oz, ox)).or_default().push(CookedTriangle {
            v: t.v.map(|p| [p[0] - ox, p[1], p[2] - oz]),
            kind: t.kind,
            flags: t.flags,
            layer: 0,
            index: i as u32,
            section: None,
            material: 0,
        });
    }
    c.tiles = tiles
        .into_iter()
        .map(|((oz, ox), triangles)| CookedTile {
            origin_mm: [ox, oz],
            triangles,
        })
        .collect();
    WorldGeom::from_canonical_bytes(&encode_canonical(c).unwrap()).unwrap()
}
/// A mesh-only native geometry from Stage A triangles (no ground supplied to weld and trim), for
/// synthetic fixtures such as the gallery (lane W-30).
pub fn native_mesh(tris: &[TunnelTriangle], cell: f32) -> WorldGeom {
    let ground = GroundInput {
        lattice_n: 65,
        width_m: 1280.0,
        offset_m: 0.0,
        step_m: 0.03125,
        counts: vec![0; 65 * 65],
    };
    let mut c = cook(&CookInputs {
        document: b"(version:1, layers:[])",
        ground: &ground,
        terrain: None,
        read_file: &|_| Err("no source".into()),
    })
    .unwrap()
    .file
    .canonical;
    place_triangles(&mut c, quantize_mesh(tris).unwrap(), None, cell)
}
