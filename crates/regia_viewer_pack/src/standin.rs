// SPDX-License-Identifier: MPL-2.0
//! Stand-in fixtures (`regia viewer standin -o <dir>`): a generic part kit, three generic piece
//! recipes, a stand-in material library and synthetic option records, until real kits exist.
//!
//! Everything here is generic: parts from simple solids, gem cuts from published proportions,
//! placeholder houses `house-a` and `house-b`, no supplier names, SKUs or designs. Material values
//! are rounded public reference numbers marked `estimate`.
//!
//! ```text
//! <dir>/kits/shared/standin-parts/{kit.json, standin-parts.glb}
//! <dir>/recipes/house-a/standin-marquise-bezel-end.json
//! <dir>/recipes/house-b/standin-round-prong-end.json
//! <dir>/recipes/house-a/standin-dangle-ring.json
//! <dir>/materials/standin/library.json
//! <dir>/optiongraph/<house>/<slug>.json
//! ```

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::geom::{self, Detail};
use crate::mesh::MeshData;
use crate::{PackError, canonical_json};

/// The stand-in kit id.
pub const KIT_ID: &str = "standin-parts";

/// What the stand-in generator wrote.
#[derive(Clone, Debug)]
pub struct StandinSet {
    /// The kit folder.
    pub kit_dir: PathBuf,
    /// Every recipe file.
    pub recipes: Vec<PathBuf>,
    /// The material library.
    pub library: PathBuf,
    /// The option records, one per recipe, same order.
    pub option_graphs: Vec<PathBuf>,
}

struct Part {
    id: &'static str,
    kind: &'static str,
    mesh: MeshData,
    sockets: Value,
}

fn parts() -> Vec<Part> {
    // A cut's length runs along +y (the kit's gem-local frame), so the bezel's outline does too.
    let mq = |l: f32, w: f32| -> Vec<glam::Vec2> {
        geom::outline("marquise", l, w)
            .unwrap_or_default()
            .into_iter()
            .map(|p| glam::Vec2::new(-p.y, p.x))
            .collect()
    };
    let seat =
        |d: f32| json!([{"socket_id": "gem_seat", "kind": "gem_seat", "origin_mm": [0, 0, 0], "size_mm": d}]);
    let pin = json!([{"socket_id": "thread", "kind": "pin", "origin_mm": [0, 0, 0], "size_mm": 0.8}]);
    vec![
        Part {
            id: "post_1.2x8",
            kind: "post",
            mesh: geom::post("post_1.2x8", 1.2, 8.0, Detail::Cheap),
            sockets: pin.clone(),
        },
        Part {
            id: "threadless_pin_0.8x6",
            kind: "threadless_pin",
            mesh: geom::post("threadless_pin_0.8x6", 0.8, 6.0, Detail::Cheap),
            sockets: pin,
        },
        Part {
            id: "end_base_2.5",
            kind: "end_base",
            mesh: geom::end_base("end_base_2.5", 2.5, 0.6, Detail::Fine),
            sockets: json!([]),
        },
        Part {
            id: "ball_3.0",
            kind: "bead",
            mesh: geom::ball("ball_3.0", 3.0, Detail::Fine),
            sockets: json!([]),
        },
        Part {
            id: "prong_head_4_2.25",
            kind: "prong_head",
            mesh: geom::prong_head("prong_head_4_2.25", 2.25, 4, Detail::Fine),
            sockets: seat(2.25),
        },
        Part {
            id: "prong_head_4_3.0",
            kind: "prong_head",
            mesh: geom::prong_head("prong_head_4_3.0", 3.0, 4, Detail::Fine),
            sockets: seat(3.0),
        },
        Part {
            id: "bezel_marquise_3x1.5",
            kind: "bezel",
            mesh: geom::bezel_cup(
                "bezel_marquise_3x1.5",
                &mq(3.0, 1.5),
                0.25,
                0.9,
                0.25,
                Detail::Fine,
            ),
            sockets: seat(3.0),
        },
        Part {
            id: "bezel_marquise_4x2",
            kind: "bezel",
            mesh: geom::bezel_cup("bezel_marquise_4x2", &mq(4.0, 2.0), 0.3, 1.1, 0.3, Detail::Fine),
            sockets: seat(4.0),
        },
        Part {
            id: "seam_ring_8x1.2",
            kind: "hoop",
            mesh: geom::ring("seam_ring_8x1.2", 8.0, 1.2, Detail::Fine),
            sockets: json!([]),
        },
        Part {
            id: "chain_link_3x2",
            kind: "link",
            mesh: geom::chain_link("chain_link_3x2", 3.0, 2.0, 0.4, Detail::Fine),
            sockets: json!([]),
        },
    ]
}

/// The gem cuts the stand-in kit offers: (cut type, length, width).
pub const CUTS: [(&str, f32, f32); 4] = [
    ("round_brilliant", 2.25, 2.25),
    ("round_brilliant", 3.0, 3.0),
    ("marquise", 3.0, 1.5),
    ("marquise", 4.0, 2.0),
];

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), PackError> {
    let io = |e: std::io::Error| PackError::new("pack_io", format!("{}: {e}", path.display()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

fn kit_json(parts: &[Part]) -> Value {
    let part_rows: Vec<Value> = parts
        .iter()
        .map(|p| {
            let (lo, hi) = p.mesh.bounds();
            json!({
                "part_id": p.id, "kind": p.kind, "params": [],
                "glb": "standin-parts.glb", "node": p.id,
                "sockets": p.sockets, "material_slots": ["body"], "lods": [],
                "bbox_mm": [lo.to_array(), hi.to_array()], "tri_count": p.mesh.triangles(),
                "edge_radius_min_mm": 0.1, "source": "parametric", "accuracy": "plausible",
                "front_axis": [0, 0, 1], "provenance": {"by": "regia viewer standin", "generic": true},
            })
        })
        .collect();
    // The full facet sets of `regia_looks`, the same cuts its look board draws, so a stand-in piece shows the look
    // the owner approves. A kit entry is `None` only for a concave cut, and CUTS has none.
    let cuts: Vec<Value> = CUTS
        .iter()
        .filter_map(|&(t, l, w)| {
            let cut = regia_looks::CutType::parse(t)?;
            regia_looks::GemCut::new(cut, [l, w]).to_kit_entry(&format!("{t}-{l}x{w}"))
        })
        .collect();
    json!({
        "schema": "jewelry.part_kit/1", "kit_id": KIT_ID, "kit_version": 1,
        "regia_min_version": env!("CARGO_PKG_VERSION"), "house": "shared",
        "parts": part_rows, "gem_cuts": cuts,
    })
}

fn view_range(spin: &str) -> Value {
    json!({"front_axis": [0, 0, 1], "tilt_max_deg": 30, "pan_bounds_mm": [[-3, 3], [-3, 3]], "spin": spin,
           "rock_max_deg": null, "max_px_per_mm": 400, "back": "plausible"})
}

fn wear() -> Value {
    json!({"handed": "none", "mirror_pair": null,
           "presets": [{"preset_id": "default", "label": "Default", "rotation_deg": [0, 0, 0], "mirror": false}]})
}

/// A `jewelry.option_graph/1` record for a stand-in: the `Gem` group is a required add-on, every other group
/// an axis, and every axis combination is a sold variant except titanium with the larger gem size, so the
/// configurator has a refusal to test. Every variant carries a `cost` the cook must strip.
fn option_record(product_key: &str, groups: &[(&str, Vec<&str>)]) -> Value {
    let house = product_key.split('/').next().unwrap_or_default();
    let values = |group: &str, labels: &[&str]| -> Vec<Value> {
        labels
            .iter()
            .map(|l| json!({"value_key": format!("{group}={l}"), "label": l, "available": true}))
            .collect()
    };
    // A manual-line add-on value is only acceptable with a code to put on the order line.
    let addon_values = |group: &str, labels: &[&str]| -> Vec<Value> {
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| json!({"value_key": format!("{group}={l}"), "label": l, "available": true, "code": format!("STANDIN-{group}-{i}")}))
            .collect()
    };
    let axes: Vec<&(&str, Vec<&str>)> = groups.iter().filter(|(g, _)| *g != "Gem").collect();
    let mut combos: Vec<serde_json::Map<String, Value>> = vec![serde_json::Map::new()];
    for (group, labels) in &axes {
        combos = combos
            .iter()
            .flat_map(|c| {
                labels.iter().map(move |l| {
                    let mut next = c.clone();
                    next.insert((*group).to_string(), json!(l));
                    next
                })
            })
            .collect();
    }
    combos.retain(|c| {
        !(c.get("Material") == Some(&json!("Titanium"))
            && ["4x2mm", "3mm"]
                .iter()
                .any(|s| c.get("Gem Size") == Some(&json!(s))))
    });
    let variants: Vec<Value> = combos
        .into_iter()
        .enumerate()
        .map(|(i, picks)| json!({"variant_key": format!("{product_key}:{i}"), "sku": format!("STANDIN-{i}"), "picks": picks, "cost": 10.0, "price": 40.0}))
        .collect();
    json!({
        "schema": "jewelry.option_graph/1", "snapshot_id": "2026-11-04-000000000000",
        "product_key": product_key, "design_key": product_key, "house": house, "status": "orderable",
        "title": "Stand-in", "cart": {"mode": "manual_line", "platform": "custom"}, "gaps": [],
        "axes": axes.iter().map(|(g, v)| {
            let mut axis = json!({"group": g, "kind": "other", "values": values(g, v)});
            // Metal and gauge do not change the shape (the geometry key, option graph schema 1.1.0).
            if matches!(*g, "Material" | "Gauge") {
                axis["geometry_role"] = json!("excluded");
            }
            axis
        }).collect::<Vec<_>>(),
        // The stand-ins' Gem group is only the stone's color; its size is an axis of its own.
        "addons": groups.iter().filter(|(g, _)| *g == "Gem").map(|(g, v)| json!({"group": g, "kind": "gem", "geometry_role": "excluded", "required": true, "multi": false, "values": addon_values(g, v)})).collect::<Vec<_>>(),
        "variants": variants,
    })
}

fn recipes() -> Vec<(String, String, Value, Value)> {
    let metals = ["14K Yellow Gold", "14K Rose Gold", "14K White Gold", "Titanium"];
    let gems = [
        "Clear CZ",
        "Blue Sapphire (synthetic)",
        "Green Spinel (synthetic)",
    ];
    let og = |product_key: &str, groups: Vec<(&str, Vec<&str>)>| option_record(product_key, &groups);
    let a = json!({
        "schema": "jewelry.piece_recipe/1", "house": "house-a", "design_key": "house-a/STANDIN-MQBEZ",
        "product_keys": ["house-a/standin-marquise-bezel-end"], "recipe_version": 1,
        "modelling_class": "end-single-gem", "units": "mm", "values": {"mq_l": 3.0, "mq_w": 1.5},
        "dims_mm": [{"name": "length", "value": 3.0, "source": "option", "confidence": 1.0}],
        "parts": [
            {"instance_id": "setting", "part_ref": "shared/bezel_marquise_3x1.5@1", "params": {}, "at": {"origin_mm": [0, 0, 0]}},
            {"instance_id": "base", "part_ref": "shared/threadless_pin_0.8x6@1", "params": {}, "at": {"origin_mm": [0, 0, -0.9]}}
        ],
        "gem_slots": [{"slot_id": "s1", "role": "center", "cut": "marquise", "size_mm": ["$mq_l", "$mq_w"], "count": 1,
                       "pattern": null, "setting": {"kind": "bezel", "variant": "closed"},
                       "at": {"origin_mm": [0, 0, 0], "z_axis": [0, 0, 1], "up": [0, 1, 0]}, "material_slot": "gem:s1"}],
        "material_zones": [{"zone": "body", "kind": "metal"}, {"zone": "gem:s1", "kind": "gem"}],
        "drives": [
            {"group": "Material", "to": {"material_zone": "body"}},
            {"group": "Gem", "to": {"material_zone": "gem:s1"}},
            {"group": "Gem Size", "to": {"value": "mq_l", "map": {"3x1.5mm": 3.0, "4x2mm": 4.0}}},
            {"group": "Gem Size", "to": {"value": "mq_w", "map": {"3x1.5mm": 1.5, "4x2mm": 2.0}}},
            {"group": "Gem Size", "to": {"part_swap": "setting", "map": {"3x1.5mm": "shared/bezel_marquise_3x1.5@1", "4x2mm": "shared/bezel_marquise_4x2@1"}}},
            {"group": "Threading", "to": {"part_swap": "base", "map": {"Threadless": "shared/threadless_pin_0.8x6@1", "Threaded": "shared/post_1.2x8@1"}}}
        ],
        "no_visual": ["Gauge"], "wear": wear(), "view_range": view_range("none"),
        "camera_default": {"distance_mm": 60, "focal_length_mm": 100, "f_stop": 11},
        "fidelity": {"front": "plausible", "back_sides": "plausible", "height_source": "estimate"},
        "provenance": {"by": "regia viewer standin", "generic": true},
    });
    let a_og = og(
        "house-a/standin-marquise-bezel-end",
        vec![
            ("Material", metals.to_vec()),
            ("Gem", gems.to_vec()),
            ("Gem Size", vec!["3x1.5mm", "4x2mm"]),
            ("Threading", vec!["Threadless", "Threaded"]),
            ("Gauge", vec!["18g", "16g"]),
        ],
    );
    let b = json!({
        "schema": "jewelry.piece_recipe/1", "house": "house-b", "design_key": "house-b/standin-round-prong",
        "product_keys": ["house-b/standin-round-prong-end"], "recipe_version": 1,
        "modelling_class": "end-single-gem", "units": "mm", "values": {"d": 2.25},
        "parts": [
            {"instance_id": "head", "part_ref": "shared/prong_head_4_2.25@1", "params": {}, "at": {"origin_mm": [0, 0, 0]}},
            {"instance_id": "base", "part_ref": "shared/threadless_pin_0.8x6@1", "params": {}, "at": {"origin_mm": [0, 0, -1.0]}}
        ],
        "gem_slots": [{"slot_id": "s1", "role": "center", "cut": "round_brilliant", "size_mm": ["$d", "$d"], "count": 1,
                       "pattern": null, "setting": {"kind": "prong", "variant": "4"},
                       "at": {"origin_mm": [0, 0, 0], "z_axis": [0, 0, 1], "up": [0, 1, 0]}, "material_slot": "gem:s1"}],
        "material_zones": [{"zone": "body", "kind": "metal"}, {"zone": "gem:s1", "kind": "gem"}],
        "drives": [
            {"group": "Material", "to": {"material_zone": "body"}},
            {"group": "Gem", "to": {"material_zone": "gem:s1"}},
            {"group": "Gem Size", "to": {"value": "d", "map": {"2.25mm": 2.25, "3mm": 3.0}}},
            {"group": "Gem Size", "to": {"part_swap": "head", "map": {"2.25mm": "shared/prong_head_4_2.25@1", "3mm": "shared/prong_head_4_3.0@1"}}},
            {"group": "Thread", "to": {"part_swap": "base", "map": {"Threadless": "shared/threadless_pin_0.8x6@1", "Threaded": "shared/post_1.2x8@1"}}}
        ],
        "no_visual": [], "wear": wear(), "view_range": view_range("none"),
        "camera_default": {"distance_mm": 60, "focal_length_mm": 100, "f_stop": 11},
        "provenance": {"by": "regia viewer standin", "generic": true},
    });
    let b_og = og(
        "house-b/standin-round-prong-end",
        vec![
            ("Material", metals.to_vec()),
            ("Gem", gems.to_vec()),
            ("Gem Size", vec!["2.25mm", "3mm"]),
            ("Thread", vec!["Threadless", "Threaded"]),
        ],
    );
    let c = json!({
        "schema": "jewelry.piece_recipe/1", "house": "house-a", "design_key": "house-a/STANDIN-DANGLE",
        "product_keys": ["house-a/standin-dangle-ring"], "recipe_version": 1,
        "modelling_class": "ring-dangle", "units": "mm", "values": {},
        "parts": [
            {"instance_id": "ring", "part_ref": "shared/seam_ring_8x1.2@1", "params": {}, "at": {"origin_mm": [0, 0, 0]}},
            {"instance_id": "link", "part_ref": "shared/chain_link_3x2@1", "params": {}, "at": {"origin_mm": [0, -5.6, 0]}},
            {"instance_id": "bead", "part_ref": "shared/ball_3.0@1", "params": {}, "at": {"origin_mm": [0, -8.2, 0]}}
        ],
        "gem_slots": [],
        "material_zones": [{"zone": "body", "kind": "metal"}],
        "drives": [{"group": "Material", "to": {"material_zone": "body"}}],
        "no_visual": ["Gauge"], "wear": wear(), "view_range": view_range("about_front_axis"),
        "camera_default": {"distance_mm": 90, "focal_length_mm": 100, "f_stop": 11},
        "provenance": {"by": "regia viewer standin", "generic": true},
    });
    let c_og = og(
        "house-a/standin-dangle-ring",
        vec![("Material", metals.to_vec()), ("Gauge", vec!["16g", "14g"])],
    );
    vec![
        ("house-a".into(), "standin-marquise-bezel-end".into(), a, a_og),
        ("house-b".into(), "standin-round-prong-end".into(), b, b_og),
        ("house-a".into(), "standin-dangle-ring".into(), c, c_og),
    ]
}

/// The stand-in material library. Linear-RGB F0 of the metals are rounded from published
/// reference tables (Hoffman 2013; Akenine-Moller et al. 2018, table 9.2); karat golds and
/// anodized colors are eyeballed estimates; gem indices are textbook refractive indices.
pub fn library() -> Value {
    let metal = |id: &str, label: &str, f0: [f32; 3], rough: f32| {
        json!({"material_id": id, "kind": "conductor", "display_name": label,
               "catalogue_facets": [format!("Material={label}")],
               "conductor": {"f0_rgb_linear": f0, "f82_tint_rgb": [1.0, 1.0, 1.0], "plating": "none"}, "roughness": rough,
               "status": "estimate", "source": {"dataset": "stand-in", "citation": "rounded public F0 tables", "date": "2026-11-04"},
               "calibration": {"method": "none"}})
    };
    let gem = |id: &str, label: &str, n: f32, tint: [f32; 3]| {
        json!({"material_id": id, "kind": "gem_dielectric", "display_name": label,
               "catalogue_facets": [format!("Gem={label}")],
               "gem_dielectric": {"n_d": n, "abbe_vd": 30.0, "attenuation_rgb_per_mm": tint, "scatter": 0.0,
                                  "surface_roughness": 0.0, "ior_family": format!("n{:.2}", n)},
               "status": "estimate",
               "source": {"dataset": "stand-in", "citation": "textbook refractive indices", "date": "2026-11-04"},
               "calibration": {"method": "none"}})
    };
    json!({
        "schema": "jewelry.material_library/1", "library_version": "standin-1",
        "materials": [
            metal("au14-yellow", "14K Yellow Gold", [0.94, 0.72, 0.38], 0.18),
            metal("au14-rose", "14K Rose Gold", [0.94, 0.62, 0.50], 0.18),
            metal("au14-white", "14K White Gold", [0.82, 0.80, 0.76], 0.16),
            metal("ti", "Titanium", [0.54, 0.50, 0.45], 0.22),
            gem("cz-clear", "Clear CZ", 2.16, [0.0, 0.0, 0.0]),
            gem("sapphire-blue", "Blue Sapphire (synthetic)", 1.77, [1.6, 0.9, 0.1]),
            gem("spinel-green", "Green Spinel (synthetic)", 1.72, [1.2, 0.15, 1.0]),
        ],
    })
}

/// The static demo page (`web/demo/index.html`): it drives the web player's own JS API
/// (`RegiaViewer` from the `regia_web` wasm build) and, without that build, lists the package's
/// options from its files.
pub const DEMO_PAGE: &str = include_str!("../web/demo/index.html");

/// Write the static demo page as `dir/demo/index.html`. Serve `dir` and open
/// `demo/index.html?product=../packages/<name>`; the wasm player goes in `demo/pkg/`.
pub fn write_demo(dir: &Path) -> Result<PathBuf, PackError> {
    let demo = dir.join("demo");
    write_file(&demo.join("index.html"), DEMO_PAGE.as_bytes())?;
    Ok(demo)
}

/// Write the stand-in set under `dir` (created if needed; existing stand-in files are replaced).
pub fn write(dir: &Path) -> Result<StandinSet, PackError> {
    let _span = tracing::info_span!("viewer_standin").entered();
    let parts = parts();
    let kit_dir = dir.join("kits/shared").join(KIT_ID);
    let meshes: Vec<MeshData> = parts.iter().map(|p| p.mesh.clone()).collect();
    let glb = crate::glb::write(&meshes, "regia viewer standin")?;
    write_file(&kit_dir.join("standin-parts.glb"), &glb)?;
    write_file(
        &kit_dir.join("kit.json"),
        canonical_json(&kit_json(&parts)).as_bytes(),
    )?;
    let library = dir.join("materials/standin/library.json");
    write_file(&library, canonical_json(&self::library()).as_bytes())?;
    let mut set = StandinSet {
        kit_dir,
        recipes: Vec::new(),
        library,
        option_graphs: Vec::new(),
    };
    for (house, slug, recipe, mut og) in recipes() {
        og["design_key"] = recipe["design_key"].clone();
        let r = dir.join("recipes").join(&house).join(format!("{slug}.json"));
        write_file(&r, canonical_json(&recipe).as_bytes())?;
        let o = dir.join("optiongraph").join(&house).join(format!("{slug}.json"));
        write_file(&o, canonical_json(&og).as_bytes())?;
        set.recipes.push(r);
        set.option_graphs.push(o);
    }
    Ok(set)
}
