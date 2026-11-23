// SPDX-License-Identifier: MPL-2.0
//! Plan row W-06 (spec 121 § 3, § 9): the meshlayers formats, the `do27-tunnels/1` import, the
//! weld and trim cook steps, and the content-hash pins of spec 121 § 9.2.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::TunnelLayout;
use regia_world_geom::compat::pack::parse_tunnels;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{
    CookError, CookInputs, GroundInput, WorldTriangleMm, cook, quantize_mesh, trim, weld,
};
use regia_world_geom::cooked::{
    COOKED_MAGIC, CookedMeshLayers, CookedPlug, CookedZoneShape, CutBitmap, GeomLoadError,
    TRI_FLAG_NO_SUPPORT, TriKind, TriRange, VertexHeightMm, WorldGeomCooked, content_hash_of,
    content_hash_of_bytes, decode_canonical, encode_canonical, migrate_canonical, migrate_file, split_file,
};
use regia_world_geom::meshlayers::{
    GroundMode, MESH_LAYERS_FORMAT_VERSION, MeshLayersDocument, MeshLayersError,
};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::{GeomSettings, WorldGeom};

const MAPS: [&str; 3] = ["switchbacks", "atacama_rail", "cinder_steps"];

/// Flat ground at -256 m (the Switchbacks canyon floor), as the Stage A sampler sees it.
fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

/// The same flat ground as the sim lattice reads it.
fn canyon_ground() -> GroundInput {
    GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    }
}

const ROLLING_SIDE: usize = 65;

fn rolling_counts() -> Vec<u16> {
    (0..ROLLING_SIDE * ROLLING_SIDE)
        .map(|i| {
            let (x, z) = (i % ROLLING_SIDE, i / ROLLING_SIDE);
            (20_000 + (x * 37 + z * 91) % 640) as u16
        })
        .collect()
}

/// Rolling ground (the W-01 loader fixture).
fn rolling_terrain() -> TerrainSampler {
    TerrainSampler::new(
        HeightGrid::square(ROLLING_SIDE, rolling_counts()).unwrap(),
        1280.0,
    )
}

fn rolling_ground() -> GroundInput {
    let counts = rolling_counts();
    let centre = counts[(ROLLING_SIDE / 2) * ROLLING_SIDE + ROLLING_SIDE / 2];
    GroundInput {
        lattice_n: ROLLING_SIDE as u32,
        width_m: 1280.0,
        offset_m: -(centre as f32) / 32.0,
        step_m: 1.0 / 32.0,
        counts,
    }
}

/// A one-layer document over `passages.ron`.
fn document(extra: &str) -> String {
    format!(
        "(version: 1, ground_tile_cells: 16, {extra} layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"))])"
    )
}

fn cook_with(
    doc: &str,
    passages: &str,
    ground: &GroundInput,
    terrain: Option<&TerrainSampler>,
) -> Result<CookedMeshLayers, CookError> {
    let files: BTreeMap<String, Vec<u8>> =
        [("map.passages.ron".to_string(), passages.as_bytes().to_vec())].into();
    let read = |p: &str| files.get(p).cloned().ok_or_else(|| format!("{p}: not found"));
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground,
            terrain,
            read_file: &read,
        })
    };
    match run(doc) {
        Ok(out) => Ok(out.file),
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            let waived = waive_ramp_facets(doc, &report).map_err(CookError::Document)?;
            run(&waived).map(|out| out.file)
        }
        Err(e) => Err(e),
    }
}

/// The canonical triangles of a cooked file in world millimetres, `(layer, index)` order.
fn world_triangles(c: &WorldGeomCooked) -> Vec<WorldTriangleMm> {
    c.triangles_sorted()
        .into_iter()
        .map(|(o, t)| WorldTriangleMm {
            v: t.v.map(|p| [p[0] + o[0], p[1], p[2] + o[1]]),
            kind: t.kind,
            flags: t.flags,
        })
        .collect()
}

/// Spec 121 § 9.1 and the plan row's fail-first test: DO27's `do27-tunnels/1` export of every
/// fixture map, imported as `<map>.passages.ron` and cooked, gives the canonical mesh DO27's own
/// loader arms (`TunnelsFile::collision_mesh`, whose count is the one the export recorded) from the same sampled
/// block with its skylight shafts opened (W-08, K8), quantized with `m_to_mm`, then welded and trimmed; and the same canonical section (the same
/// `content_hash`) as the layout authored directly in Regia.
#[test]
fn do27_json_imports_to_the_same_canonical_mesh() {
    for (terrain, ground) in [
        (canyon_terrain(), canyon_ground()),
        (rolling_terrain(), rolling_ground()),
    ] {
        for map in MAPS {
            let layout = for_map(map).unwrap();
            let json = layout.export_json(Some(&terrain));
            let imported = PassagesDocument::import_do27_json(&json).unwrap();
            let ron_text = imported.to_ron().unwrap();
            let file = cook_with(&document(""), &ron_text, &ground, Some(&terrain)).unwrap();
            let geom = WorldGeom::from_cooked(&file.encode().unwrap()).unwrap();

            // What DO27 arms from the same JSON (its Stage A mesh has the count the export recorded) ...
            let baked = parse_tunnels(&json).unwrap();
            assert_eq!(
                baked.collision_mesh(&terrain).triangles.len() as u64,
                baked.triangle_count.unwrap(),
                "{map}"
            );
            // ... from the same sampled block with its skylight shafts cooked as open cuts (lane W-08, DO27's K8
            // answer: the cook cuts the lid in a shaft column; ADR-8210).
            let mut sampled = baked.sampled.clone();
            regia_world_geom::sections::open_skylight_shafts(&mut sampled, &baked.layout.openings);
            let mesh = sampled.mesh(&baked.layout.openings, Some(&terrain));
            let lattice = ground.lattice(16).unwrap();
            let mut expected = quantize_mesh(&mesh.triangles).unwrap();
            weld(&mut expected, Some(&ground), &lattice);
            trim(&mut expected, Some(&ground), &lattice);
            assert_eq!(world_triangles(&file.canonical), expected, "{map}");
            assert_eq!(geom.triangles().len(), expected.len(), "{map}");

            // The Regia-authored layout cooks to the same canonical bytes.
            let authored = PassagesDocument::from_layout(layout.clone()).to_ron().unwrap();
            let regia = cook_with(&document(""), &authored, &ground, Some(&terrain)).unwrap();
            assert_eq!(
                content_hash_of(&regia.canonical).unwrap(),
                geom.content_hash(),
                "{map}: the import and the authored layout cook alike"
            );
        }
    }
}

/// `regia terrain passages import` then `export` writes DO27's file back byte for byte, and the
/// `<map>.passages.ron` text round trips with every float's bits.
#[test]
fn do27_json_round_trips_byte_identically() {
    let terrain = rolling_terrain();
    for map in MAPS {
        let layout = for_map(map).unwrap();
        let json = layout.export_json(Some(&terrain));
        let doc = PassagesDocument::import_do27_json(&json).unwrap();
        assert_eq!(doc.to_do27_json().unwrap(), json, "{map}");
        let back = PassagesDocument::from_ron(&doc.to_ron().unwrap()).unwrap();
        assert_eq!(back, doc, "{map}");
        assert_eq!(back.to_do27_json().unwrap(), json, "{map}: through RON");
        assert_eq!(
            doc.export_do27_json_on(Some(&terrain)),
            json,
            "{map}: re-baked on the same terrain"
        );
    }
    let authored = PassagesDocument::from_layout(for_map("switchbacks").unwrap());
    assert_eq!(
        authored.to_do27_json().map_err(|e| e.code()),
        Err("passages_unsampled")
    );
    assert_eq!(
        PassagesDocument::import_do27_json("{\"format\": \"other/1\"}").map_err(|e| e.code()),
        Err("passages_import")
    );
}

/// An import baked on one terrain and cooked on another is refused, never cooked silently.
#[test]
fn a_drifted_import_fails_the_cook() {
    let layout = for_map("switchbacks").unwrap();
    let json = layout.export_json(Some(&canyon_terrain()));
    let ron_text = PassagesDocument::import_do27_json(&json)
        .unwrap()
        .to_ron()
        .unwrap();
    let err = cook_with(
        &document(""),
        &ron_text,
        &rolling_ground(),
        Some(&rolling_terrain()),
    );
    assert_eq!(err.map_err(|e| e.code()).err(), Some("passages_drift"));
}

fn switchbacks_cooked() -> CookedMeshLayers {
    let ron_text = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    cook_with(
        &document(""),
        &ron_text,
        &canyon_ground(),
        Some(&canyon_terrain()),
    )
    .unwrap()
}

#[test]
fn the_cooked_file_round_trips_and_loads() {
    let file = switchbacks_cooked();
    let bytes = file.encode().unwrap();
    assert_eq!(&bytes[..4], &COOKED_MAGIC);
    assert_eq!(CookedMeshLayers::decode(&bytes).unwrap(), file);
    assert_eq!(file.encode().unwrap(), bytes, "encoding is byte-stable");
    let geom = WorldGeom::from_cooked(&bytes).unwrap();
    // ADR-8246 R7: the loaded section drops repeated tile-border counts. The file keeps them.
    let loaded = geom.cooked();
    assert_eq!(loaded.tiles, file.canonical.tiles);
    assert_eq!(loaded.lattice, file.canonical.lattice);
    assert_eq!(loaded.tile_levels, file.canonical.tile_levels);
    assert_eq!(loaded.hanging_overrides, file.canonical.hanging_overrides);
    assert_eq!(loaded.ring_pins, file.canonical.ring_pins);
    assert_eq!(loaded.cut_bitmaps, file.canonical.cut_bitmaps);
    assert_eq!(loaded.settings, file.canonical.settings);
    assert_eq!(loaded.ground, file.canonical.ground);
    assert_eq!(loaded.sections, file.canonical.sections);
    assert_eq!(loaded.portals, file.canonical.portals);
    assert_eq!(loaded.zones, file.canonical.zones);
    assert_eq!(loaded.plugs, file.canonical.plugs);
    let file_counts: usize = file.canonical.ground_counts.iter().map(|b| b.len()).sum();
    let runtime_counts: usize = loaded.ground_counts.iter().map(|b| b.len()).sum();
    // One tile owns its own border, so the counts stay equal. A multi-tile map drops them.
    assert!(
        runtime_counts <= file_counts,
        "runtime {runtime_counts} file {file_counts}"
    );
    let pinned: std::collections::BTreeSet<u32> = loaded
        .ring_pins
        .iter()
        .chain(&loaded.hanging_overrides)
        .map(|v| v.vertex)
        .collect();
    let lattice = regia_world_geom::ground::GroundLattice::new(loaded);
    let per = loaded.lattice.tiles_per_side();
    let n = loaded.lattice.lattice_n;
    for (t, block) in file.canonical.ground_counts.iter().enumerate() {
        let level = file.canonical.tile_levels[t];
        let (a, b) = ((t as u32) % per, (t as u32) / per);
        let (x0, x1) = loaded.lattice.tile_cells(a);
        let (z0, _z1) = loaded.lattice.tile_cells(b);
        let step = 1u32 << u32::from(level);
        let cols = (x1 - x0) / step + 1;
        for (k, &count) in block.iter().enumerate() {
            let di = (k as u32) % cols;
            let dj = (k as u32) / cols;
            let (vi, vj) = (x0 + di * step, z0 + dj * step);
            if pinned.contains(&(vj * n + vi)) {
                continue;
            }
            let want = loaded.lattice.offset_m() + count as f32 * loaded.lattice.step_m();
            assert_eq!(
                lattice.vertex_height_in_tile(t as u32, vi, vj).map(f32::to_bits),
                Some(want.to_bits()),
                "tile {t} vertex {vi},{vj}"
            );
        }
    }
    assert_eq!(*geom.settings(), GeomSettings::default());
    assert!(geom.resident_bytes() > 0);
    // Version 2 since ADR-8816 (the canonical section's fixed-width counts) and ADR-8872.
    assert_eq!(
        regia_world_geom::cooked::cooked_file_version(&bytes),
        Ok(Some(regia_world_geom::cooked::COOKED_FORMAT_VERSION))
    );
    assert_eq!(regia_world_geom::cooked::COOKED_FORMAT_VERSION, 2);
    assert_eq!(
        regia_world_geom::cooked::cooked_file_version(b"(version: 1)"),
        Ok(None)
    );
    // TriRef order and the f32 metres come from the pinned conversion.
    let tris = geom.triangles();
    assert!(tris.windows(2).all(|w| w[0].tri < w[1].tri));
    let first = file.canonical.triangles_sorted()[0];
    let (o, t) = first;
    assert_eq!(
        tris[0].v[0][0].to_bits(),
        regia_world_geom::mm_to_m(o[0], t.v[0][0]).to_bits()
    );
}

/// ADR-8246 R7: a two-by-two tile lattice must not keep the shared east and north borders twice.
/// The runtime count length is the distinct lattice vertices. The file still stores the borders.
#[test]
fn runtime_drops_repeated_tile_borders() {
    let n = 33u32;
    let ground = GroundInput {
        lattice_n: n,
        width_m: 32.0,
        offset_m: 0.0,
        step_m: 1.0 / 32.0,
        counts: (0..n * n).map(|i| (i % 1000) as u16).collect(),
    };
    let cooked = cook(&CookInputs {
        document: b"(version:1, ground_tile_cells:16, layers:[])",
        ground: &ground,
        terrain: None,
        read_file: &|_| Err("no source".into()),
    })
    .unwrap()
    .file
    .canonical;
    let file_counts: usize = cooked.ground_counts.iter().map(|b| b.len()).sum();
    let geom = WorldGeom::from_canonical_bytes(&encode_canonical(&cooked).unwrap()).unwrap();
    let runtime: usize = geom.cooked().ground_counts.iter().map(|b| b.len()).sum();
    assert!(
        runtime < file_counts,
        "runtime {runtime} still repeats the file borders {file_counts}"
    );
    assert_eq!(runtime, (n as usize) * (n as usize));
    let estimate = regia_world_geom::pitch::sim_ground_bytes(geom.cooked());
    let actual = geom.resident_bytes();
    eprintln!("BORDER_DEDUP file={file_counts} runtime={runtime} estimate={estimate} resident={actual}");
}

/// Spec 121 § 9.1/9.2: a newer source document, a newer cooked file and a newer canonical section
/// are refused as newer, before anything else is decoded.
#[test]
fn newer_versions_are_refused() {
    let err = MeshLayersDocument::from_ron("(version: 2, layers: [], future: 1)").map_err(|e| e.code());
    assert_eq!(err, Err("meshlayers_newer_version"));
    let passages = format!(
        "(version: {}, layout: (), future: 1)",
        MESH_LAYERS_FORMAT_VERSION + 1
    );
    assert_eq!(
        PassagesDocument::from_ron(&passages).map_err(|e| e.code()),
        Err("meshlayers_newer_version")
    );
    let file = switchbacks_cooked();
    let mut newer = file.clone();
    newer.header.version = regia_world_geom::cooked::COOKED_FORMAT_VERSION + 1;
    // The rest is still current bytes; the version alone decides.
    let bytes = newer.encode().unwrap();
    assert_eq!(
        WorldGeom::from_cooked(&bytes).map_err(|e| e.code()).err(),
        Some("geom_newer_version")
    );
    let mut canonical = file.clone();
    canonical.canonical.canonical_version = 3;
    let bytes = canonical.encode().unwrap();
    assert_eq!(
        WorldGeom::from_cooked(&bytes).err(),
        Some(GeomLoadError::NewerVersion {
            found: 3,
            supported: 2
        })
    );
    // A newer file with a body this engine cannot read is still "newer", not "corrupt".
    let mut garbage = COOKED_MAGIC.to_vec();
    garbage.extend_from_slice(&[9, 0xff, 0xff, 0xff]);
    assert_eq!(
        WorldGeom::from_cooked(&garbage).map_err(|e| e.code()).err(),
        Some("geom_newer_version")
    );
}

/// Spec 121 § 9.2 (ADR-8084, review W1): each `GeomSettings` field changed by one ulp or one unit
/// moves `content_hash`, and the header copies follow (a file whose copies differ is refused).
#[test]
fn settings_change_moves_content_hash() {
    let file = switchbacks_cooked();
    let base = WorldGeom::from_cooked(&file.encode().unwrap())
        .unwrap()
        .content_hash();
    const FIELDS: [&str; 10] = [
        "index_cell_m",
        "wall_pad_m",
        "same_height_m",
        "lid_gap_m",
        "hull_fit_m",
        "step_bound_mm",
        "max_floors",
        "min_cover_mm",
        "pitch_error_m",
        "fine_band_m",
    ];
    fn up(v: f32) -> f32 {
        f32::from_bits(v.to_bits() + 1)
    }
    fn edit(field: &str, s: &mut GeomSettings) {
        match field {
            "index_cell_m" => s.index_cell_m = up(s.index_cell_m),
            "wall_pad_m" => s.wall_pad_m = up(s.wall_pad_m),
            "same_height_m" => s.same_height_m = up(s.same_height_m),
            "lid_gap_m" => s.lid_gap_m = up(s.lid_gap_m),
            "hull_fit_m" => s.hull_fit_m = up(s.hull_fit_m),
            "step_bound_mm" => s.step_bound_mm += 1,
            "max_floors" => s.max_floors += 1,
            "min_cover_mm" => s.min_cover_mm += 1,
            "pitch_error_m" => s.pitch_error_m = up(s.pitch_error_m),
            "fine_band_m" => s.fine_band_m = up(s.fine_band_m),
            other => panic!("no edit for {other}"),
        }
    }
    let mut seen = std::collections::BTreeSet::from([base]);
    for field in FIELDS {
        let mut changed = file.clone();
        edit(field, &mut changed.canonical.settings);
        changed.header.settings = changed.canonical.settings;
        let geom = WorldGeom::from_cooked(&changed.encode().unwrap()).unwrap();
        assert!(
            seen.insert(geom.content_hash()),
            "{field}: content_hash did not move"
        );
        // The header copy alone never decides: a stale copy is refused.
        let mut stale = changed.clone();
        stale.header.settings = file.header.settings;
        assert_eq!(
            WorldGeom::from_cooked(&stale.encode().unwrap())
                .map_err(|e| e.code())
                .err(),
            Some("geom_corrupt"),
            "{field}"
        );
    }
}

/// Spec 121 § 9.2 (ADR-8084, review W1): the version, each lattice constant, the ground mode, one
/// tile level, one ring pin, one hanging override and one cut bit, each changed alone, move the
/// hash of the canonical section bytes.
#[test]
fn every_canonical_field_moves_content_hash() {
    let mut base = switchbacks_cooked().canonical;
    base.ring_pins = vec![VertexHeightMm {
        vertex: 5,
        height_mm: -256_000,
    }];
    base.hanging_overrides = vec![VertexHeightMm {
        vertex: 6,
        height_mm: -256_000,
    }];
    base.cut_bitmaps = vec![CutBitmap {
        tile: 0,
        bits: vec![0b10],
    }];
    // Lane W-08: sections, portals and zones are cooked; one plug on section 0 (its range is not checked here).
    assert!(!base.sections.is_empty() && !base.portals.is_empty() && !base.zones.is_empty());
    base.plugs = vec![CookedPlug {
        id: 0,
        section: 0,
        initially_active: false,
        triangles: TriRange {
            layer: 0,
            start: 0,
            end: 1,
        },
    }];
    base.sections[0].blocker = Some(0);
    base.check().unwrap();
    let h0 = content_hash_of(&base).unwrap();
    const EDITS: [&str; 24] = [
        "canonical_version",
        "lattice_n",
        "width_mm",
        "ground_tile_cells",
        "offset_m",
        "step_m",
        "ground mode",
        "tile level",
        "ring pin",
        "hanging override",
        "cut bit",
        "a triangle vertex",
        "a ground count",
        "a section's covered flag",
        "a section's bounds",
        "a section's triangle range",
        "a section's portal list",
        "a portal's polygon",
        "a portal's nav node",
        "a portal's mouth",
        "a zone's shape",
        "a zone's exposure",
        "a zone's reverb preset",
        "a plug's initial state",
    ];
    fn edit(what: &str, c: &mut WorldGeomCooked) {
        match what {
            "canonical_version" => c.canonical_version += 1,
            "lattice_n" => c.lattice.lattice_n += 1,
            "width_mm" => c.lattice.width_mm += 1,
            "ground_tile_cells" => c.lattice.ground_tile_cells *= 2,
            "offset_m" => c.lattice.offset_m_bits += 1,
            "step_m" => c.lattice.step_m_bits += 1,
            "ground mode" => c.ground = GroundMode::Nested { max_level: 0 },
            "tile level" => c.tile_levels[0] = 1,
            "ring pin" => c.ring_pins[0].height_mm += 1,
            "hanging override" => c.hanging_overrides[0].height_mm += 1,
            "cut bit" => c.cut_bitmaps[0].bits[0] ^= 1,
            "a triangle vertex" => c.tiles[0].triangles[0].v[0][1] += 1,
            "a ground count" => c.ground_counts[0][0] += 1,
            "a section's covered flag" => c.sections[0].covered ^= true,
            "a section's bounds" => c.sections[0].bounds_mm[1][1] += 1,
            "a section's triangle range" => c.sections[0].triangles[0].end += 1,
            "a section's portal list" => c.sections[0].portals.clear(),
            "a portal's polygon" => c.portals[0].polygon_mm[0][0] += 1,
            "a portal's nav node" => c.portals[0].nav_mm[1] += 1,
            "a portal's mouth" => c.portals[0].mouth = Some("x".into()),
            "a zone's shape" => {
                if let CookedZoneShape::OrientedBox { half_mm, .. } = &mut c.zones[0].shape {
                    half_mm[0] += 1;
                }
            }
            "a zone's exposure" => {
                c.zones[0].exposure[0] = f32::from_bits(c.zones[0].exposure[0].to_bits() + 1)
            }
            "a zone's reverb preset" => c.zones[0].reverb_preset.push('x'),
            "a plug's initial state" => c.plugs[0].initially_active = true,
            other => panic!("no edit for {other}"),
        }
    }
    let mut seen = std::collections::BTreeSet::from([h0]);
    for what in EDITS {
        let mut c = base.clone();
        edit(what, &mut c);
        assert!(
            seen.insert(content_hash_of(&c).unwrap()),
            "{what}: content_hash did not move"
        );
    }
}

/// Spec 121 § 9.2 (ADR-8084, review W1): a file with its header and presentation section zeroed,
/// then the header copies refreshed, loads to the same `WorldGeom`: the run time reads only the
/// canonical section. (The query table joins this comparison when lane W-11 adds the queries.)
#[test]
fn runtime_reads_only_the_canonical_section() {
    let mut file = switchbacks_cooked();
    file.presentation = vec![7; 64];
    let geom = WorldGeom::from_cooked(&file.encode().unwrap()).unwrap();
    let mut zeroed = file.clone();
    zeroed.header.input_hash = [0; 32];
    zeroed.presentation = vec![0; 64];
    zeroed.header.settings = zeroed.canonical.settings;
    zeroed.header.ground = zeroed.canonical.ground;
    let other = WorldGeom::from_cooked(&zeroed.encode().unwrap()).unwrap();
    assert_eq!(other.content_hash(), geom.content_hash());
    assert_eq!(other.cooked(), geom.cooked());
    let bits = |g: &WorldGeom| {
        g.triangles()
            .iter()
            .map(|t| (t.tri, t.v.map(|p| p.map(f32::to_bits)), t.kind, t.flags))
            .collect::<Vec<_>>()
    };
    assert_eq!(bits(&other), bits(&geom));
    assert_eq!(other.resident_bytes(), geom.resident_bytes());
    // And the canonical bytes alone (what a headless server keeps) give the same geometry.
    let parts = split_file(&zeroed.encode().unwrap()).unwrap();
    let alone = WorldGeom::from_canonical_bytes(&parts.canonical).unwrap();
    assert_eq!(alone.content_hash(), geom.content_hash());
}

#[test]
fn corrupt_files_are_refused_with_codes() {
    let file = switchbacks_cooked();
    let bytes = file.encode().unwrap();
    let code = |b: &[u8]| WorldGeom::from_cooked(b).map_err(|e| e.code()).err();
    assert_eq!(code(b"nope"), Some("geom_corrupt"));
    assert_eq!(code(&bytes[..bytes.len() - 1]), Some("geom_corrupt"));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(code(&trailing), Some("geom_corrupt"));
    let mut mode = file.clone();
    mode.header.ground = GroundMode::Nested { max_level: 1 };
    assert_eq!(code(&mode.encode().unwrap()), Some("geom_corrupt"));
    let mut far = file.clone();
    far.canonical.tiles[0].triangles[0].v[0][1] = 1 << 24;
    assert_eq!(code(&far.encode().unwrap()), Some("geom_extent_too_large"));
    let mut unsorted = file.clone();
    unsorted.canonical.ring_pins = vec![
        VertexHeightMm {
            vertex: 3,
            height_mm: 0,
        },
        VertexHeightMm {
            vertex: 2,
            height_mm: 0,
        },
    ];
    assert_eq!(code(&unsorted.encode().unwrap()), Some("geom_corrupt"));
    let mut level = file.clone();
    level.canonical.tile_levels[0] = 1;
    assert_eq!(
        code(&level.encode().unwrap()),
        Some("geom_corrupt"),
        "a level in Full mode"
    );
    let mut flag = file;
    flag.canonical.tiles[0].triangles[0].flags = 0x80;
    assert_eq!(code(&flag.encode().unwrap()), Some("geom_corrupt"));
    // Truncation at every prefix length never panics.
    for n in 0..bytes.len().min(512) {
        assert!(WorldGeom::from_cooked(&bytes[..n]).is_err());
    }
    let canonical = encode_canonical(&switchbacks_cooked().canonical).unwrap();
    assert!(decode_canonical(&canonical).is_ok());
}

/// The cook names what it cannot cook yet, and a missing input is an error, never an empty pass.
#[test]
fn the_cook_refuses_what_it_does_not_build() {
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let ground = canyon_ground();
    let code = |doc: &str| {
        cook_with(doc, &passages, &ground, None)
            .map_err(|e| e.code())
            .err()
    };
    // W-10 builds the Nested ground: it cooks now (lattice_ground.rs pins what it cooks). It is
    // cooked on the canyon terrain like `switchbacks_cooked`: without it the passages sit off the
    // ground and the bake checks refuse both modes (ADR-8436).
    let nested = cook_with(
        &document("ground: Nested(max_level: 1),"),
        &passages,
        &ground,
        Some(&canyon_terrain()),
    );
    assert_eq!(nested.map_err(|e| e.code()).err(), None);
    let gltf = "(version: 1, layers: [(id: \"a\", source: Gltf(path: \"a.glb\"))])";
    assert_eq!(code(gltf), Some("cook_not_built"));
    let missing = "(version: 1, layers: [(id: \"a\", source: Passages(path: \"missing.passages.ron\"))])";
    assert_eq!(code(missing), Some("input_missing"));
    // Blocker plugs are cooked since lane W-08 (ADR-8210); a run on a passage the layer does not have is refused.
    let blockers = "(version: 1, layers: [(id: \"a\", source: Passages(path: \"map.passages.ron\"), blockers: [(passage: \"x\", from_s_m: 0.0, to_s_m: 6.0)])])";
    assert_eq!(code(blockers), Some("meshlayers_invalid"));
    assert_eq!(code("(version: 3, layers: [])"), Some("meshlayers_newer_version"));
    let bad_ground = GroundInput {
        counts: vec![0; 3],
        ..canyon_ground()
    };
    assert_eq!(
        cook_with(&document(""), &passages, &bad_ground, None)
            .map_err(|e| e.code())
            .err(),
        Some("ground_invalid")
    );
    // An empty document cooks: settings, lattice and ground, no triangles.
    let empty = cook_with(
        "(version: 1, ground_tile_cells: 16, layers: [])",
        &passages,
        &ground,
        None,
    )
    .unwrap();
    assert!(empty.canonical.tiles.is_empty());
    assert_eq!(empty.canonical.ground_counts, vec![vec![8192u16; 16]]);
    assert!(matches!(
        MeshLayersDocument::from_ron("not ron"),
        Err(MeshLayersError::Parse(_))
    ));
}

fn floor(a: [i32; 3], b: [i32; 3], c: [i32; 3]) -> WorldTriangleMm {
    WorldTriangleMm {
        v: [a, b, c],
        kind: TriKind::Floor,
        flags: 0,
    }
}

/// A quad strip of two floor triangles, `w` wide in z and 20 m long in x, at height `y` (mm).
fn strip(x0: i32, z0: i32, w: i32, y: i32) -> [WorldTriangleMm; 2] {
    let (a, b, c, d) = (
        [x0, y, z0],
        [x0 + 20_000, y, z0],
        [x0, y, z0 + w],
        [x0 + 20_000, y, z0 + w],
    );
    [floor(a, c, b), floor(b, c, d)]
}

/// E6: the weld step moves floor vertices up to 0.15 m over the ground onto it (and every triangle
/// that shares them), and leaves higher or lower ones.
#[test]
fn weld_puts_near_floor_vertices_on_the_ground() {
    let ground = canyon_ground();
    let lattice = ground.lattice(16).unwrap();
    let g = -256_000;
    let mut tris = vec![
        floor([0, g + 100, 0], [1000, g + 150, 0], [0, g + 151, 1000]),
        floor([0, g - 50, 5000], [1000, g, 5000], [0, g + 3000, 6000]),
        WorldTriangleMm {
            v: [[0, g + 100, 0], [0, g + 4000, 0], [1000, g + 4000, 0]],
            kind: TriKind::Wall,
            flags: 0,
        },
    ];
    assert_eq!(weld(&mut tris, Some(&ground), &lattice), 2);
    assert_eq!(tris[0].v.map(|p| p[1]), [g, g, g + 151]);
    assert_eq!(tris[1].v.map(|p| p[1]), [g - 50, g, g + 3000]);
    assert_eq!(
        tris[2].v[0][1], g,
        "the wall vertex at the welded position moves with it"
    );
}

/// ADR-8246 R2: a mesh-only cook has no ground. A floor 100 mm above y = 0 must not weld to an
/// invented flat plane, and a narrow strip near that plane must not be trimmed. The same vertices
/// do weld when a ground at y = 0 is supplied.
#[test]
fn mesh_only_conditioning_receives_no_ground() {
    let ground = GroundInput {
        lattice_n: 17,
        width_m: 32.0,
        offset_m: 0.0,
        step_m: 0.03125,
        counts: vec![0; 17 * 17],
    };
    let lattice = ground.lattice(16).unwrap();
    let mut open = vec![floor([0, 100, 0], [1000, 100, 0], [0, 100, 1000])];
    assert_eq!(weld(&mut open, None, &lattice), 0);
    assert_eq!(open[0].v[0][1], 100, "no invented ground to weld against");
    assert_eq!(weld(&mut open, Some(&ground), &lattice), 3);
    assert_eq!(open[0].v.map(|p| p[1]), [0, 0, 0]);
    let mut strip_only = strip(0, 0, 1_500, 300).to_vec();
    assert_eq!(trim(&mut strip_only, None, &lattice), 0);
    assert!(strip_only.iter().all(|t| t.flags & TRI_FLAG_NO_SUPPORT == 0));
}

/// E6: a floor strip under 2 m wide lying within 0.5 m of another floor carries nothing; a wider
/// strip, or a narrow one far from any other floor, keeps carrying.
#[test]
fn trim_takes_narrow_strips_near_another_floor_out_of_support() {
    let ground = canyon_ground();
    let lattice = ground.lattice(16).unwrap();
    let g = -256_000;
    let mut tris = Vec::new();
    tris.extend(strip(0, 0, 1_500, g + 300)); // narrow, 0.3 m over the ground: trimmed
    tris.extend(strip(0, 10_000, 3_000, g + 300)); // wide: kept
    tris.extend(strip(0, 20_000, 1_500, g + 5_000)); // narrow, 5 m up, nothing near: kept
    tris.extend(strip(-5_000, 30_000, 30_000, g + 8_000)); // a deck
    tris.extend(strip(0, 40_000, 1_000, g + 8_400)); // narrow, 0.4 m over the deck: trimmed
    assert_eq!(trim(&mut tris, Some(&ground), &lattice), 4);
    let flagged: Vec<bool> = tris.iter().map(|t| t.flags & TRI_FLAG_NO_SUPPORT != 0).collect();
    assert_eq!(
        flagged,
        [true, true, false, false, false, false, false, false, true, true]
    );
}

/// The settings in a document reach the canonical section (E6).
#[test]
fn document_settings_are_cooked_into_the_canonical_section() {
    let passages = PassagesDocument::from_layout(for_map("cinder_steps").unwrap())
        .to_ron()
        .unwrap();
    let file = cook_with(
        &document("geom: (max_floors: 5, wall_pad_m: -0.0),"),
        &passages,
        &canyon_ground(),
        Some(&canyon_terrain()),
    )
    .unwrap();
    assert_eq!(file.canonical.settings.max_floors, 5);
    assert_eq!(
        file.canonical.settings.wall_pad_m.to_bits(),
        0,
        "-0.0 is written as +0.0"
    );
    assert_eq!(file.header.settings, file.canonical.settings);
    let layout: TunnelLayout = for_map("cinder_steps").unwrap();
    assert!(!layout.passages.is_empty());
}

/// ADR-8576: the chunk section follows the presentation section only when it is not empty. A file
/// without chunks keeps the bytes it had before the section existed, a file with chunks round-trips
/// them, and a stored empty chunk section (a single `0` length byte) is refused.
#[test]
fn the_chunk_section_is_stored_only_when_not_empty() {
    let file = switchbacks_cooked();
    assert!(file.chunks.is_empty(), "the canonical cook writes no chunks");
    let plain = file.encode().unwrap();
    let parts = split_file(&plain).unwrap();
    assert!(parts.chunks.is_empty());
    let with = CookedMeshLayers {
        chunks: vec![1, 2, 3],
        ..file.clone()
    };
    let bytes = with.encode().unwrap();
    assert_eq!(&bytes[..plain.len()], &plain[..], "the chunk section is appended");
    assert_eq!(&bytes[plain.len()..], &[3, 1, 2, 3], "postcard(Vec<u8>)");
    assert_eq!(split_file(&bytes).unwrap().chunks, vec![1, 2, 3]);
    assert_eq!(CookedMeshLayers::decode(&bytes).unwrap(), with);
    let geom = WorldGeom::from_cooked(&bytes).unwrap();
    assert_eq!(
        geom.content_hash(),
        WorldGeom::from_cooked(&plain).unwrap().content_hash(),
        "chunks are not hashed into content_hash"
    );
    let mut empty = plain.clone();
    empty.push(0);
    assert_eq!(
        split_file(&empty).map_err(|e| e.code()).err(),
        Some("geom_corrupt")
    );
}

/// A ground of `n` samples a side in 16-cell tiles, with distinct counts and no layers.
fn ground_only_canonical(n: u32) -> WorldGeomCooked {
    let ground = GroundInput {
        lattice_n: n,
        width_m: (n - 1) as f32,
        offset_m: 0.0,
        step_m: 1.0 / 32.0,
        counts: (0..n * n).map(|i| (i * 7 % 60_000) as u16).collect(),
    };
    cook(&CookInputs {
        document: b"(version:1, ground_tile_cells:16, layers:[])",
        ground: &ground,
        terrain: None,
        read_file: &|_| Err("no source".into()),
    })
    .unwrap()
    .file
    .canonical
}

/// A two-by-two-tile ground: the smallest map whose tiles share borders.
fn four_tile_canonical() -> WorldGeomCooked {
    ground_only_canonical(33)
}

/// The v0.6.0 (version 1) canonical section of a value: `postcard(WorldGeomCooked)` in field
/// order with `canonical_version` 1, every count a varint.
fn v1_section(canonical: &WorldGeomCooked) -> Vec<u8> {
    let mut v1 = canonical.clone();
    v1.canonical_version = 1;
    postcard::to_allocvec(&v1).unwrap()
}

/// The v0.6.0 (version 1) file of a value: the same parts with the header at version 1 and the
/// version 1 canonical section.
fn v1_file(file: &CookedMeshLayers) -> Vec<u8> {
    let mut header = file.header.clone();
    header.version = 1;
    let mut out = COOKED_MAGIC.to_vec();
    out.extend_from_slice(&postcard::to_allocvec(&header).unwrap());
    out.extend_from_slice(&postcard::to_allocvec(&v1_section(&file.canonical)).unwrap());
    out.extend_from_slice(&postcard::to_allocvec(&file.presentation).unwrap());
    if !file.chunks.is_empty() {
        out.extend_from_slice(&postcard::to_allocvec(&file.chunks).unwrap());
    }
    out
}

/// Spec 121 § 9.2 as of version 2 (ADR-8816): after the version come a little-endian `u32` head
/// length, the head, and the counts at two bytes each with no length prefix; `content_hash` is the
/// BLAKE3 of the bytes before the count body, and the head's hashes tie the body to it.
#[test]
fn canonical_v2_stores_fixed_width_counts_after_a_hashed_head() {
    for canonical in [four_tile_canonical(), switchbacks_cooked().canonical] {
        let bytes = encode_canonical(&canonical).unwrap();
        assert_eq!(bytes[0], 2, "the version is the first byte");
        let head_len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
        let counts: usize = canonical.ground_counts.iter().map(|b| b.len()).sum();
        assert_eq!(bytes.len(), 5 + head_len + 2 * counts);
        let body = &bytes[5 + head_len..];
        let mut stored = body.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p));
        for block in &canonical.ground_counts {
            for &count in block {
                assert_eq!(stored.next(), Some(count));
            }
        }
        let hash = content_hash_of_bytes(&bytes);
        assert_eq!(hash, *blake3::hash(&bytes[..5 + head_len]).as_bytes());
        assert_eq!(hash, content_hash_of(&canonical).unwrap());
        assert_eq!(
            WorldGeom::from_canonical_bytes(&bytes).unwrap().content_hash(),
            hash
        );
        assert_eq!(decode_canonical(&bytes).unwrap(), canonical);
        // The last count of the map moves the hash too: the head carries the body's hashes.
        let mut other = canonical.clone();
        *other.ground_counts.last_mut().unwrap().last_mut().unwrap() ^= 1;
        assert_ne!(content_hash_of(&other).unwrap(), hash);
    }
}

/// ADR-8816: the count body is covered by the hash through the head's block hashes, so the loader
/// refuses a body byte that changed (in v1 it loaded with another `content_hash`). One byte in
/// the first, a middle and the last block of a 24 x 24 tile ground's body, through both loaders.
#[test]
fn a_changed_count_byte_is_refused_at_load() {
    let bytes = encode_canonical(&ground_only_canonical(385)).unwrap();
    let head_len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
    let body = 5 + head_len;
    let block = regia_world_geom::cooked::COUNT_BLOCK_BYTES;
    assert!(bytes.len() - body > 3 * block, "the body spans several blocks");
    for at in [body, body + (bytes.len() - body) / 2, bytes.len() - 1] {
        let mut bad = bytes.clone();
        bad[at] ^= 0x40;
        let err = WorldGeom::from_canonical_bytes(&bad).err();
        assert_eq!(
            err.as_ref().map(GeomLoadError::code),
            Some("geom_corrupt"),
            "byte {at}"
        );
        let which = (at - body) / block;
        assert!(
            err.is_some_and(|e| e.to_string().contains(&format!("block {which} "))),
            "byte {at} names block {which}"
        );
        assert_eq!(
            decode_canonical(&bad).map_err(|e| e.code()).err(),
            Some("geom_corrupt")
        );
    }
    // A changed hash, a missing count and an extra count are refused as well.
    let mut hash = bytes.clone();
    hash[body - 1] ^= 1;
    assert_eq!(
        WorldGeom::from_canonical_bytes(&hash).map_err(|e| e.code()).err(),
        Some("geom_corrupt")
    );
    assert!(WorldGeom::from_canonical_bytes(&bytes[..bytes.len() - 2]).is_err());
    let mut longer = bytes.clone();
    longer.extend_from_slice(&[0, 0]);
    assert!(WorldGeom::from_canonical_bytes(&longer).is_err());
    // Truncation at every length of a small section never panics and never loads.
    let small = encode_canonical(&four_tile_canonical()).unwrap();
    for n in 0..small.len() {
        assert!(decode_canonical(&small[..n]).is_err(), "prefix {n}");
        assert!(
            WorldGeom::from_canonical_bytes(&small[..n]).is_err(),
            "prefix {n}"
        );
    }
}

/// ADR-8816, the migrator: a v0.6.0 (version 1) section and file load as the same value, the same
/// run-time geometry and the same `content_hash` as the map cooked at version 2, and
/// `migrate_canonical` / `migrate_file` write exactly the version 2 bytes.
#[test]
fn a_v0_6_0_file_migrates_to_the_same_value_and_hash() {
    for file in [
        switchbacks_cooked(),
        CookedMeshLayers {
            canonical: four_tile_canonical(),
            ..switchbacks_cooked()
        },
    ] {
        let canonical = &file.canonical;
        let v2 = encode_canonical(canonical).unwrap();
        let v1 = v1_section(canonical);
        assert_eq!(v1[0], 1);
        assert!(v1.len() != v2.len());
        assert_eq!(decode_canonical(&v1).unwrap(), *canonical);
        assert_eq!(migrate_canonical(&v1).unwrap(), v2);
        assert_eq!(
            migrate_canonical(&v2).unwrap(),
            v2,
            "current bytes migrate to themselves"
        );
        let old = WorldGeom::from_canonical_bytes(&v1).unwrap();
        let new = WorldGeom::from_canonical_bytes(&v2).unwrap();
        assert_eq!(old.content_hash(), new.content_hash());
        // The run-time form (owned counts, ADR-8246 R7): the one-pass version 2 load equals the
        // v0.6.0 path (decode, then drop the borders).
        assert_eq!(old.cooked(), new.cooked());
        assert_eq!(old.resident_bytes(), new.resident_bytes());
        // A re-encoded run-time section (owned counts) still loads to the same geometry.
        let again = WorldGeom::from_canonical_bytes(&encode_canonical(new.cooked()).unwrap()).unwrap();
        assert_eq!(again.cooked(), new.cooked());

        let current = file.encode().unwrap();
        let legacy = v1_file(&file);
        assert_eq!(
            regia_world_geom::cooked::cooked_file_version(&legacy),
            Ok(Some(1))
        );
        assert_eq!(CookedMeshLayers::decode(&legacy).unwrap(), file);
        assert_eq!(migrate_file(&legacy).unwrap(), current);
        assert_eq!(migrate_file(&current).unwrap(), current);
        let loaded = WorldGeom::from_cooked(&legacy).unwrap();
        assert_eq!(loaded.content_hash(), new.content_hash());
        assert_eq!(loaded.cooked(), new.cooked());
        // The two versions move together: a version 1 file with a version 2 section (and the
        // reverse) is refused.
        let mut header = file.header.clone();
        header.version = 1;
        for (header_version, section) in [(1u32, &v2), (2, &v1)] {
            header.version = header_version;
            let mut mixed = COOKED_MAGIC.to_vec();
            mixed.extend_from_slice(&postcard::to_allocvec(&header).unwrap());
            mixed.extend_from_slice(&postcard::to_allocvec(section).unwrap());
            mixed.extend_from_slice(&postcard::to_allocvec(&file.presentation).unwrap());
            assert_eq!(
                WorldGeom::from_cooked(&mixed).map_err(|e| e.code()).err(),
                Some("geom_corrupt")
            );
            assert_eq!(
                CookedMeshLayers::decode(&mixed).map_err(|e| e.code()).err(),
                Some("geom_corrupt")
            );
        }
    }
    // A value still marked version 1 is never written in the version 2 layout.
    let mut marked = four_tile_canonical();
    marked.canonical_version = 1;
    assert_eq!(
        encode_canonical(&marked).map_err(|e| e.code()).err(),
        Some("geom_corrupt")
    );
}

/// ADR-8816: a load with the count-body check deferred gives the same geometry and
/// `content_hash` as the checked load, does not look at the body's hashes, and
/// `check_count_body` on the same bytes is the check it left out.
#[test]
fn a_deferred_body_check_is_the_callers_and_finds_the_same_change() {
    use regia_world_geom::cooked::{CountBodyCheck, check_count_body};
    let bytes = encode_canonical(&ground_only_canonical(385)).unwrap();
    let checked = WorldGeom::from_canonical_bytes(&bytes).unwrap();
    let deferred = WorldGeom::from_canonical_bytes_with(&bytes, CountBodyCheck::Deferred).unwrap();
    assert_eq!(deferred.cooked(), checked.cooked());
    assert_eq!(deferred.content_hash(), checked.content_hash());
    assert_eq!(check_count_body(&bytes), Ok(()));
    assert_eq!(
        WorldGeom::from_canonical_bytes_with(&bytes, CountBodyCheck::AtLoad)
            .unwrap()
            .cooked(),
        checked.cooked()
    );
    // One changed count byte: the checked load refuses it; the deferred load takes it (with the
    // unchanged head's hash) and the owed check refuses it, naming the same block.
    let mut bad = bytes.clone();
    let at = bad.len() - 3;
    bad[at] ^= 1;
    let refused = WorldGeom::from_canonical_bytes(&bad).unwrap_err();
    let loaded = WorldGeom::from_canonical_bytes_with(&bad, CountBodyCheck::Deferred).unwrap();
    assert_eq!(loaded.content_hash(), checked.content_hash());
    assert_ne!(loaded.cooked(), checked.cooked());
    assert_eq!(check_count_body(&bad), Err(refused));
    // The deferred load still refuses everything else: a changed head, a short body.
    let head_len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
    let mut head = bytes.clone();
    head[5 + head_len - 1] ^= 1;
    assert_eq!(check_count_body(&head).map_err(|e| e.code()), Err("geom_corrupt"));
    assert!(
        WorldGeom::from_canonical_bytes_with(&bytes[..bytes.len() - 2], CountBodyCheck::Deferred).is_err()
    );
    assert!(check_count_body(&bytes[..bytes.len() - 2]).is_err());
    // A whole file, and a v0.6.0 section (nothing is deferred there: its load reads every count).
    let file = CookedMeshLayers {
        canonical: ground_only_canonical(33),
        ..switchbacks_cooked()
    };
    let current = file.encode().unwrap();
    let from_file = WorldGeom::from_cooked_with(&current, CountBodyCheck::Deferred).unwrap();
    assert_eq!(
        from_file.content_hash(),
        WorldGeom::from_cooked(&current).unwrap().content_hash()
    );
    let parts = regia_world_geom::cooked::split_file_ref(&current).unwrap();
    assert_eq!(check_count_body(parts.canonical), Ok(()));
    assert_eq!(
        parts.canonical,
        split_file(&current).unwrap().canonical.as_slice()
    );
    assert_eq!(check_count_body(&v1_section(&file.canonical)), Ok(()));
}
