// SPDX-License-Identifier: MPL-2.0
//! The Regia-side pin for the runtime tunnel loader (plan row W-01, spec 121 § 13.1).
//!
//! DO27's pin `the_runtime_loader_arms_every_map_byte_identically` runs against DO27's committed map
//! packages and real heightfields, so it runs only in DO27. This is the same proof on data this crate
//! owns: every fixture layout is exported to a baked tunnel file in a temporary package root, and the
//! loader (`compat::pack::load_tunnels`) must arm each map from that file byte-identically to the spline
//! description: the same collision mesh (triangles in the same order), the same nav overlay, the same
//! description and the same sampled centrelines. A map without a baked file must arm through the spline
//! fallback, and a drifted file must be ignored rather than armed.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::{SampledLayout, TunnelLayout};
use regia_world_geom::compat::mesh::{TunnelCollisionMesh, TunnelNavOverlay};
use regia_world_geom::compat::pack::{
    MAX_TUNNEL_SAMPLES, load_tunnels, load_tunnels_file, map_dir, parse_tunnels,
};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};

const MAPS: [&str; 3] = ["switchbacks", "atacama_rail", "cinder_steps"];

/// What a host arms from the tunnel inputs: the four things the sim reads.
#[derive(Debug, PartialEq)]
struct Armed {
    mesh: TunnelCollisionMesh,
    nav: TunnelNavOverlay,
    layout: TunnelLayout,
    sampled: SampledLayout,
}

/// The host's spline path (DO27's `TunnelsState::from_layout`, mesh half).
fn from_layout(layout: TunnelLayout, terrain: Option<&TerrainSampler>) -> Armed {
    let sampled = layout.sample(terrain);
    let mesh = TunnelCollisionMesh::new(sampled.triangles(&layout.openings, terrain));
    let nav = TunnelNavOverlay::bake(&mesh);
    Armed {
        mesh,
        nav,
        layout,
        sampled,
    }
}

/// The host's baked-file path (DO27's `TunnelsState::from_baked`, mesh half).
fn from_baked(file: &regia_world_geom::compat::pack::TunnelsFile, terrain: &TerrainSampler) -> Armed {
    let mesh = file.collision_mesh(terrain);
    let nav = TunnelNavOverlay::bake(&mesh);
    Armed {
        mesh,
        nav,
        layout: file.layout.clone(),
        sampled: file.sampled.clone(),
    }
}

fn spline(id: &str, terrain: Option<&TerrainSampler>) -> Option<Armed> {
    for_map(id).map(|layout| from_layout(layout, terrain))
}

/// Flat ground at -256 m (the Switchbacks canyon floor the mouths open onto).
fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

/// Rolling ground: a deterministic relief, so the lips, sills and cover tests see slopes.
fn rolling_terrain() -> TerrainSampler {
    let side = 65usize;
    let heights = (0..side * side)
        .map(|i| {
            let (x, z) = (i % side, i / side);
            (20_000 + (x * 37 + z * 91) % 640) as u16
        })
        .collect();
    TerrainSampler::new(HeightGrid::square(side, heights).unwrap(), 1280.0)
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A fresh package root per call (pid + counter), never the source tree.
fn temp_root(tag: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia_world_geom-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes the map's export as its baked tunnel file (atomically: tmp + rename).
fn write_baked(root: &Path, id: &str, terrain: &TerrainSampler) {
    let layout = for_map(id).unwrap();
    let dir = map_dir(root, id);
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join("tunnels.json.tmp");
    std::fs::write(&tmp, layout.export_json(Some(terrain))).unwrap();
    std::fs::rename(&tmp, dir.join("tunnels.json")).unwrap();
}

fn assert_armed_alike(id: &str, from_loader: &Armed, from_spline: &Armed) {
    assert_eq!(from_loader.mesh, from_spline.mesh, "{id}: same collision mesh");
    assert_eq!(from_loader.nav, from_spline.nav, "{id}: same nav overlay");
    assert_eq!(from_loader.layout, from_spline.layout, "{id}: same description");
    assert_eq!(
        from_loader.sampled, from_spline.sampled,
        "{id}: same sampled centrelines"
    );
    let bits = |m: &TunnelCollisionMesh| -> Vec<u32> {
        m.triangles
            .iter()
            .flat_map(|t| [t.a, t.b, t.c])
            .flatten()
            .map(f32::to_bits)
            .collect()
    };
    assert_eq!(
        bits(&from_loader.mesh),
        bits(&from_spline.mesh),
        "{id}: vertex bits"
    );
    assert!(from_loader.mesh.triangles.len() > 100, "{id}: meshed");
    assert!(
        from_loader.mesh.triangles.iter().any(|t| t.portal),
        "{id}: portal floors"
    );
}

/// Switchbacks ships its baked file, the other maps do not yet: Switchbacks arms from the file, the
/// others through the spline fallback, and every map equals the spline description bit for bit.
#[test]
fn the_runtime_loader_arms_every_map_byte_identically() {
    for (name, terrain) in [("canyon", canyon_terrain()), ("rolling", rolling_terrain())] {
        let root = temp_root(name);
        write_baked(&root, "switchbacks", &terrain);
        for id in MAPS {
            assert_eq!(
                load_tunnels_file(&root, id).is_some(),
                id == "switchbacks",
                "{name}/{id}: baked file"
            );
            let from_loader = load_tunnels(&root, id, Some(&terrain), from_baked, spline)
                .unwrap_or_else(|| panic!("{name}/{id} arms"));
            let from_spline = spline(id, Some(&terrain)).unwrap();
            assert_armed_alike(&format!("{name}/{id}"), &from_loader, &from_spline);
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Every map baked: each one arms from its own file, still bit-identical to the splines.
#[test]
fn every_baked_map_arms_byte_identically_to_its_splines() {
    let terrain = rolling_terrain();
    let root = temp_root("all-baked");
    for id in MAPS {
        write_baked(&root, id, &terrain);
    }
    for id in MAPS {
        let mut used_file = false;
        let from_loader = load_tunnels(
            &root,
            id,
            Some(&terrain),
            |file, t| {
                used_file = true;
                from_baked(file, t)
            },
            spline,
        )
        .unwrap();
        assert!(used_file, "{id}: armed from the baked file");
        assert_armed_alike(id, &from_loader, &spline(id, Some(&terrain)).unwrap());
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Without a terrain the baked file is not used (it meshes against the ground): the loader takes the
/// spline path, and a map with no layout and no file does not arm.
#[test]
fn no_terrain_takes_the_spline_path_and_an_unknown_map_does_not_arm() {
    let terrain = canyon_terrain();
    let root = temp_root("no-terrain");
    write_baked(&root, "switchbacks", &terrain);
    let mut used_file = false;
    let armed = load_tunnels(
        &root,
        "switchbacks",
        None,
        |file, t| {
            used_file = true;
            from_baked(file, t)
        },
        spline,
    )
    .unwrap();
    assert!(!used_file, "no terrain: the spline path");
    assert_eq!(armed, spline("switchbacks", None).unwrap());
    assert!(load_tunnels(&root, "bore_twelve", Some(&terrain), from_baked, spline).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

/// A drifted or truncated baked file is ignored (the loader falls back to the splines) and the parser
/// names what is wrong; it never arms a half-read mesh.
#[test]
fn a_drifted_baked_file_falls_back_to_the_splines() {
    let terrain = canyon_terrain();
    let root = temp_root("drifted");
    write_baked(&root, "switchbacks", &terrain);
    let path = map_dir(&root, "switchbacks").join("tunnels.json");
    let text = std::fs::read_to_string(&path).unwrap();
    for (what, bad) in [
        ("truncated", text[..text.len() / 2].to_string()),
        (
            "wrong format",
            text.replacen("do27-tunnels/1", "do27-tunnels/9", 1),
        ),
        ("no sampled block", {
            let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
            v.as_object_mut().unwrap().remove("sampled");
            v.to_string()
        }),
        (
            "non-finite",
            text.replacen("\"sample_step_m\": 5.0", "\"sample_step_m\": NaN", 1),
        ),
    ] {
        assert_ne!(bad, text, "{what}: the mutation applied");
        assert!(parse_tunnels(&bad).is_err(), "{what}: parses");
        std::fs::write(&path, &bad).unwrap();
        assert!(
            load_tunnels_file(&root, "switchbacks").is_none(),
            "{what}: loaded"
        );
        let mut used_file = false;
        let armed = load_tunnels(
            &root,
            "switchbacks",
            Some(&terrain),
            |file, t| {
                used_file = true;
                from_baked(file, t)
            },
            spline,
        )
        .unwrap();
        assert!(!used_file, "{what}: armed from a bad file");
        assert_eq!(armed, spline("switchbacks", Some(&terrain)).unwrap(), "{what}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Review rev-b L6: a baked file with one finite but far (or overflowing) number, a field that would make
/// the mesher index past a list, or counts past the caps must be refused by `parse_tunnels`, so the loader
/// falls back to the splines. Before the bounds, `1e17` in one sample passed the parser, and building the
/// collision mesh asked for a multi-exabyte bucket grid (SIGABRT, the whole process), `1e39` narrowed to an
/// `f32` infinity and overflowed the bucket count, and a short `covered` list indexed out of bounds.
#[test]
fn a_baked_file_with_a_far_coordinate_is_refused() {
    use serde_json::{Value, json};
    let terrain = canyon_terrain();
    let root = temp_root("far");
    write_baked(&root, "switchbacks", &terrain);
    let path = map_dir(&root, "switchbacks").join("tunnels.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let good: Value = serde_json::from_str(&text).unwrap();
    assert!(parse_tunnels(&text).is_ok(), "the export parses");
    let edit = |f: &dyn Fn(&mut Value)| -> String {
        let mut v = good.clone();
        f(&mut v);
        v.to_string()
    };
    let cases: Vec<(&str, &str, String)> = vec![
        // The probe of review rev-b: a multi-exabyte allocation, SIGABRT on the base.
        (
            "far sample",
            "sampled.passages[0].samples[0].p",
            edit(&|v| v["sampled"]["passages"][0]["samples"][0]["p"][0] = json!(1e17)),
        ),
        (
            "f32 overflow",
            "sampled.passages[0].samples[0].p",
            edit(&|v| v["sampled"]["passages"][0]["samples"][0]["p"][0] = json!(1e39)),
        ),
        (
            "inside the number cap, outside the map",
            "sampled.passages[0].samples[0].p",
            edit(&|v| v["sampled"]["passages"][0]["samples"][0]["p"][2] = json!(-5e5)),
        ),
        (
            "long side normal",
            "sampled.passages[0].samples[0].left",
            edit(&|v| v["sampled"]["passages"][0]["samples"][0]["left"][0] = json!(1e5)),
        ),
        (
            "wide passage",
            "sampled.passages[0].width_m",
            edit(&|v| v["sampled"]["passages"][0]["width_m"] = json!(1e5)),
        ),
        (
            "short covered list",
            "sampled.passages[0].covered",
            edit(&|v| {
                v["sampled"]["passages"][0]["covered"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }),
        ),
        (
            "short wall list",
            "sampled.passages[0].wall_right",
            edit(&|v| {
                v["sampled"]["passages"][0]["wall_right"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }),
        ),
        (
            "far control point",
            "layout.passages[0].points",
            edit(&|v| v["layout"]["passages"][0]["points"][0][1] = json!(-1e17)),
        ),
        (
            "far opening",
            "layout.openings[0].top",
            edit(&|v| v["layout"]["openings"][0]["top"][0] = json!(2e6)),
        ),
        (
            "far hole",
            "holes[0].center",
            edit(&|v| v["holes"][0]["center"][1] = json!(1e30)),
        ),
        (
            "triangle count",
            "triangle_count",
            edit(&|v| v["triangle_count"] = json!(1_u64 << 40)),
        ),
        (
            "too many samples",
            "sampled samples",
            edit(&|v| {
                let p = &mut v["sampled"]["passages"][0];
                let first = p["samples"][0].clone();
                let flag = p["covered"][0].clone();
                let n = MAX_TUNNEL_SAMPLES + 1;
                p["samples"] = Value::Array(vec![first; n]);
                for list in ["covered", "wall_left", "wall_right"] {
                    p[list] = Value::Array(vec![flag.clone(); n - 1]);
                }
            }),
        ),
    ];
    for (what, field, bad) in cases {
        assert_ne!(bad, text, "{what}: the mutation applied");
        // The host path first: on the base this armed the mesh and aborted or panicked.
        std::fs::write(&path, &bad).unwrap();
        let mut used_file = false;
        let armed = load_tunnels(
            &root,
            "switchbacks",
            Some(&terrain),
            |file, t| {
                used_file = true;
                from_baked(file, t)
            },
            spline,
        )
        .unwrap();
        assert!(!used_file, "{what}: armed from a bad file");
        assert_eq!(armed, spline("switchbacks", Some(&terrain)).unwrap(), "{what}");
        let err = parse_tunnels(&bad).expect_err(what);
        assert!(err.contains(field), "{what}: the error names `{field}`: {err}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
