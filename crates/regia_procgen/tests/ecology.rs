// SPDX-License-Identifier: MPL-2.0
//! ADR-2456 tests: plausible ecology for biomes. The shipped presets validate clean; implausible mixes
//! are flagged with the reason (a birch woodland has no oak or maple unless its soil is mesic); the
//! opt-in `biome.ecology` node keeps shade herbs under crowns, light-demanders out of deep shade and
//! thins crowded canopy, deterministically by seed, and leaves the starter graph's output unchanged.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{box_shape, hex};
use regia_core::id::Guid;
use regia_procgen::biome::ecology::{
    EcologyProfile, EcologyReport, MapSource, PRESETS, Role, Severity, Site, Species, SpeciesCatalog,
    SpeciesUse, Stratum, Succession, ecology_path_for, starter_text, validate_ecology,
};
use regia_procgen::biome::{
    AreaOutline, BiomeAreaSnap, BiomeDef, BiomeEntry, BiomeLayer, BiomeWorld, EntryFilter, EntrySpawn,
    STARTER_BIOMES_ECOLOGY_GRAPH, STARTER_BIOMES_GRAPH,
};
use regia_procgen::data::{AttrColumn, GroundData, ShapeData};
use regia_procgen::exec::{SelfInfo, StaticSnapshot};
use regia_procgen::{CancelToken, Evaluator, ProcGraph, ProcNodeRegistry, RunInputs, RunResult};
use regia_terrain::TerrainMap;

fn preset(name: &str) -> (BiomeDef, EcologyProfile) {
    let p = PRESETS
        .iter()
        .find(|p| p.path == format!("starter/biomes/{name}.biome.ron"))
        .unwrap_or_else(|| panic!("no preset {name}"));
    let def = BiomeDef::from_ron(p.biome).unwrap_or_else(|e| panic!("{name}: {e}"));
    let eco = EcologyProfile::from_ron(p.ecology).unwrap_or_else(|e| panic!("{name}: {e}"));
    (def, eco)
}

fn check(def: &BiomeDef, eco: Option<&EcologyProfile>) -> EcologyReport {
    let cat = eco.map_or_else(|| SpeciesCatalog::builtin().clone(), EcologyProfile::catalog);
    validate_ecology(def, eco, &cat)
}

fn codes_at<'a>(r: &'a EcologyReport, path: &str) -> Vec<&'a str> {
    r.issues
        .iter()
        .filter(|i| i.path == path)
        .map(|i| i.code.as_str())
        .collect()
}

fn add_entry(def: &mut BiomeDef, layer: &str, name: &str, weight: f64) {
    let mut e = BiomeEntry::new(name, EntrySpawn::Mesh(format!("trees/{name}.glb")));
    e.weight = weight;
    let l = def.layers.iter_mut().find(|l| l.name == layer).expect("layer");
    l.entries.push(e);
}

fn mesh_layer(name: &str, entries: &[(&str, f64)]) -> BiomeLayer {
    BiomeLayer {
        entries: entries
            .iter()
            .map(|(n, w)| {
                let mut e = BiomeEntry::new(n, EntrySpawn::Mesh(format!("plants/{n}.glb")));
                e.weight = *w;
                e
            })
            .collect(),
        ..BiomeLayer::new(name)
    }
}

fn biome(name: &str, layers: Vec<BiomeLayer>) -> BiomeDef {
    BiomeDef {
        name: name.to_string(),
        layers,
        ..BiomeDef::default()
    }
}

#[test]
fn every_shipped_preset_parses_maps_every_entry_and_is_plausible() {
    assert_eq!(PRESETS.len(), 6);
    for p in PRESETS {
        let def = BiomeDef::from_ron(p.biome).unwrap_or_else(|e| panic!("{}: {e}", p.path));
        let eco = EcologyProfile::from_ron(p.ecology).unwrap_or_else(|e| panic!("{}: {e}", p.path));
        let r = check(&def, Some(&eco));
        assert!(r.issues.is_empty(), "{}: {:#?}", p.path, r.issues);
        assert!(r.unmapped.is_empty(), "{}: unmapped {:?}", p.path, r.unmapped);
        assert!(
            r.species.iter().all(|m| m.source == MapSource::Profile),
            "{}",
            p.path
        );
        assert!(!r.site_inferred && r.site.is_some() && r.floor_light.is_some());
        assert!(r.species.iter().any(|m| m.role == Role::Dominant), "{}", p.path);
        // Canonical text round-trips; the starter lookup finds both files.
        assert_eq!(EcologyProfile::from_ron(&eco.to_ron()).as_ref(), Ok(&eco));
        assert_eq!(BiomeDef::from_ron(&def.to_ron()).as_ref(), Ok(&def));
        assert_eq!(starter_text(p.path), Some(p.biome));
        assert_eq!(starter_text(&ecology_path_for(p.path)), Some(p.ecology));
    }
}

#[test]
fn the_birch_preset_has_no_oak_or_maple_and_flags_them_with_the_reason() {
    let (mut def, eco) = preset("birch_woodland");
    let cat = eco.catalog();
    let r = check(&def, Some(&eco));
    for m in &r.species {
        let g = cat.get(&m.species).map(|s| s.group.as_str());
        assert!(!matches!(g, Some("oak" | "maple")), "{} is {g:?}", m.entry);
    }
    // A viewer's "birch forest" with oak and maple: the added entries are matched by name.
    add_entry(&mut def, "canopy", "oak", 1.0);
    add_entry(&mut def, "canopy", "maple", 1.0);
    let r = check(&def, Some(&eco));
    assert!(!r.plausible());
    let oak = codes_at(&r, "canopy/oak");
    assert!(oak.contains(&"ecology_moisture"), "{oak:?}");
    let maple = codes_at(&r, "canopy/maple");
    assert!(
        maple.contains(&"ecology_moisture") && maple.contains(&"ecology_reaction"),
        "{maple:?}"
    );
    let why = r
        .issues
        .iter()
        .find(|i| i.path == "canopy/oak" && i.code == "ecology_moisture")
        .map(|i| i.message.clone())
        .unwrap_or_default();
    assert!(
        why.contains("pedunculate oak") && why.contains("peaty gley"),
        "{why}"
    );
    assert!(r.issues.iter().all(|i| i.severity == Severity::Error));
}

#[test]
fn oak_and_maple_pass_with_birch_once_the_soil_is_mesic() {
    let (mut def, mut eco) = preset("birch_woodland");
    add_entry(&mut def, "canopy", "oak", 1.0);
    add_entry(&mut def, "canopy", "maple", 1.0);
    eco.site = Site {
        moisture: (5.0, 7.0),
        reaction: (4.0, 6.0),
        soil: "mesic brown earth".into(),
        ..eco.site.clone()
    };
    let r = check(&def, Some(&eco));
    for path in ["canopy/oak", "canopy/maple"] {
        let c = codes_at(&r, path);
        assert!(
            !c.contains(&"ecology_moisture") && !c.contains(&"ecology_reaction"),
            "{path}: {c:?}"
        );
    }
    // The peat moss of the wet wood no longer fits the drier site.
    assert!(codes_at(&r, "ground/peat_moss").contains(&"ecology_moisture"));
}

#[test]
fn an_unprofiled_birch_forest_infers_acid_soil_and_flags_maple_not_oak() {
    let def = biome(
        "Birch forest",
        vec![mesh_layer(
            "canopy",
            &[("birch", 6.0), ("oak", 2.0), ("maple", 2.0)],
        )],
    );
    let r = check(&def, None);
    assert!(r.site_inferred);
    let birch = r
        .species
        .iter()
        .find(|m| m.entry == "canopy/birch")
        .expect("birch mapped");
    assert_eq!(
        (birch.species.as_str(), birch.role),
        ("betula_pendula", Role::Dominant)
    );
    assert!(codes_at(&r, "canopy/oak").is_empty(), "{:?}", r.issues);
    let maple = r
        .issues
        .iter()
        .find(|i| i.path == "canopy/maple")
        .expect("maple flagged");
    assert_eq!(maple.code, "ecology_reaction");
    assert!(
        maple.message.contains("inferred from the dominant"),
        "{}",
        maple.message
    );
    // Without a name the largest canopy weight dominates.
    let unnamed = biome(
        "Stand",
        vec![mesh_layer("canopy", &[("oak", 6.0), ("birch", 4.0)])],
    );
    let r = check(&unnamed, None);
    let oak = r.species.iter().find(|m| m.entry == "canopy/oak").expect("oak");
    assert_eq!(oak.role, Role::Dominant);
    assert!(r.plausible(), "{:?}", r.issues);
}

#[test]
fn a_biome_named_after_a_tree_that_does_not_dominate_is_flagged() {
    let def = biome(
        "Oak forest",
        vec![mesh_layer("canopy", &[("birch", 6.0), ("pine", 4.0)])],
    );
    let r = check(&def, None);
    assert_eq!(codes_at(&r, "name"), vec!["ecology_name_mismatch"]);
    let (def, mut eco) = preset("oak_birch_woodland");
    for u in &mut eco.species {
        if u.species == "betula_pendula" {
            u.role = Role::Associated;
        }
    }
    let r = check(&def, Some(&eco));
    assert!(
        r.issues
            .iter()
            .any(|i| i.code == "ecology_name_mismatch" && i.message.contains("not a dominant"))
    );
}

#[test]
fn species_that_never_share_a_site_are_flagged() {
    let def = biome(
        "Moor",
        vec![mesh_layer(
            "ground",
            &[("peat_moss", 1.0), ("reindeer_lichen", 1.0)],
        )],
    );
    let r = check(&def, None);
    let i = r
        .issues
        .iter()
        .find(|i| i.code == "ecology_pair_exclusive")
        .expect("pair");
    assert!(
        i.message.contains("peat moss") && i.message.contains("reindeer lichen"),
        "{}",
        i.message
    );
}

#[test]
fn heather_under_beech_and_a_birch_beech_codominance_are_flagged() {
    let (mut def, eco) = preset("beech_forest");
    add_entry(&mut def, "field", "heather", 1.0);
    let r = check(&def, Some(&eco));
    let c = codes_at(&r, "field/heather");
    assert!(c.contains(&"ecology_understory_light"), "{c:?}");
    let floor = r.floor_light.expect("trees");
    assert!(floor < 4.0, "beech leaves little light: {floor}");

    let (def, mut eco) = preset("beech_forest");
    // Recast the maple slot as a co-dominant silver birch.
    let mut def = def;
    def.layers[0].entries[1].name = "silver_birch".into();
    for u in &mut eco.species {
        if u.entry == "canopy/sycamore_maple" {
            u.entry = "canopy/silver_birch".into();
            u.species = "betula_pendula".into();
            u.role = Role::Dominant;
        }
    }
    let r = check(&def, Some(&eco));
    let i = r
        .issues
        .iter()
        .find(|i| i.code == "ecology_competition")
        .expect("competition");
    assert!(
        i.message.contains("silver birch") && i.message.contains("European beech"),
        "{}",
        i.message
    );
}

#[test]
fn proportions_must_match_the_weights_and_put_dominants_first() {
    let (mut def, eco) = preset("oak_hornbeam_forest");
    def.layers[0].entries[0].weight = 9.0;
    let r = check(&def, Some(&eco));
    assert!(
        codes_at(&r, "canopy/pedunculate_oak").contains(&"ecology_share_mismatch"),
        "{:?}",
        r.issues
    );

    let (def, mut eco) = preset("oak_hornbeam_forest");
    eco.species[2].share = Some(0.3); // lime: the canopy shares now sum to 1.2
    let r = check(&def, Some(&eco));
    assert!(
        codes_at(&r, "canopy").contains(&"ecology_share_sum"),
        "{:?}",
        r.issues
    );

    let (def, mut eco) = preset("oak_hornbeam_forest");
    eco.species.push(SpeciesUse {
        entry: "canopy/elm".into(),
        species: "ulmus_glabra".into(),
        role: Role::Associated,
        share: None,
    });
    let r = check(&def, Some(&eco));
    assert!(r.issues.iter().any(|i| i.code == "ecology_entry_unknown"));
    let mut def2 = def.clone();
    add_entry(&mut def2, "canopy", "elm", 0.0);
    let r = check(&def2, Some(&eco));
    assert!(
        r.issues.iter().any(|i| i.code == "ecology_species_unknown"),
        "{:?}",
        r.issues
    );

    let (def, mut eco) = preset("oak_hornbeam_forest");
    for u in &mut eco.species {
        if u.role == Role::Dominant {
            u.role = Role::Associated;
        }
    }
    let r = check(&def, Some(&eco));
    assert!(r.issues.iter().any(|i| i.code == "ecology_dominant_missing"));
}

#[test]
fn entry_filters_outside_a_species_altitude_or_slope_are_flagged() {
    let mut def = biome("Stand", vec![mesh_layer("canopy", &[("oak", 1.0)])]);
    def.layers[0].entries[0].filters = vec![EntryFilter::Altitude {
        min: 1500.0,
        max: 2500.0,
        feather: 0.0,
    }];
    let r = check(&def, None);
    assert!(
        codes_at(&r, "canopy/oak").contains(&"ecology_altitude"),
        "{:?}",
        r.issues
    );
    let mut def = biome("Carr", vec![mesh_layer("canopy", &[("alder", 1.0)])]);
    def.layers[0].entries[0].filters = vec![EntryFilter::Slope {
        min_deg: 30.0,
        max_deg: 60.0,
        feather: 0.0,
    }];
    let r = check(&def, None);
    assert!(
        codes_at(&r, "canopy/alder").contains(&"ecology_slope"),
        "{:?}",
        r.issues
    );
}

#[test]
fn custom_species_extend_the_table() {
    let (mut def, mut eco) = preset("oak_hornbeam_forest");
    add_entry(&mut def, "canopy", "field_maple", 0.0);
    eco.custom_species.push(Species {
        id: "acer_campestre".into(),
        common_name: "field maple".into(),
        group: "maple".into(),
        aliases: vec!["field_maple".into()],
        stratum: Stratum::Subcanopy,
        moisture: (3.0, 6.0),
        reaction: (6.0, 9.0),
        indicator: (5.0, 7.0),
        light: 5.0,
        shade_tolerance: 3.2,
        casts_shade: 3.0,
        altitude_m: (0.0, 800.0),
        max_slope_deg: 35.0,
        succession: Succession::Mid,
        vernal: false,
    });
    let r = check(&def, Some(&eco));
    let m = r
        .species
        .iter()
        .find(|m| m.entry == "canopy/field_maple")
        .expect("mapped");
    assert_eq!(
        (m.species.as_str(), m.source),
        ("acer_campestre", MapSource::Name)
    );
    assert!(r.plausible(), "{:?}", r.issues);
}

#[test]
fn the_shipped_sample_biomes_are_plausible() {
    for (name, text) in [
        (
            "pine_forest",
            include_str!("../../../samples/procgen_biomes/assets/pine_forest.biome.ron"),
        ),
        (
            "meadow",
            include_str!("../../../samples/procgen_biomes/assets/meadow.biome.ron"),
        ),
        (
            "riverbed",
            include_str!("../../../samples/procgen_biomes/assets/riverbed.biome.ron"),
        ),
        (
            "open_world_forest",
            include_str!("../../../samples/procgen_open_world/assets/forest.biome.ron"),
        ),
    ] {
        let def = BiomeDef::from_ron(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let r = check(&def, None);
        assert!(r.plausible(), "{name}: {:#?}", r.issues);
    }
}

// ---------------------------------------------------------------------------------------------
// The `biome.ecology` node.
// ---------------------------------------------------------------------------------------------

const GEN_GUID: Guid = Guid(0x00ec_0100_0000_0000_0000_0000_0000_0001);

fn flat_map() -> TerrainMap {
    TerrainMap::from_heights(129, 129, vec![2.0; 129 * 129], 256.0, 1.0, 0.0).expect("flat map")
}

fn area(guid: u128, center: [f32; 2], half: f32, path: &str, def: BiomeDef) -> BiomeAreaSnap {
    BiomeAreaSnap {
        guid: Guid(guid),
        name: format!("area{guid}"),
        outline: AreaOutline::Shape(box_shape([center[0], 0.0, center[1]], [half, 100.0, half], 0.0)),
        blend_m: 0.0,
        blend_noise: None,
        priority_offset: 0,
        preview_alone: false,
        biomes: vec![(path.to_string(), Arc::new(def))],
    }
}

/// A beech forest west, a boreal pine forest east, on flat ground.
fn eco_world(with_profiles: bool, thinning_override: Option<f64>) -> BiomeWorld {
    let mut w = BiomeWorld::default();
    for (guid, x, name) in [(1u128, -55.0f32, "beech_forest"), (2, 55.0, "boreal_pine_forest")] {
        let (def, mut eco) = preset(name);
        let path = format!("starter/biomes/{name}.biome.ron");
        if let (Some(n), Some(t)) = (thinning_override, eco.competition.self_thinning.as_mut()) {
            t.max_stems_per_ha = n;
        }
        if with_profiles {
            w.ecology.insert(path.clone(), Arc::new(eco));
        }
        w.defs.insert(path.clone(), Arc::new(def.clone()));
        w.areas.push(area(guid, [x, 0.0], 50.0, &path, def));
    }
    w.sort();
    w
}

fn snapshot(world: BiomeWorld) -> Arc<StaticSnapshot> {
    let bounds: ShapeData = box_shape([0.0; 3], [110.0, 100.0, 55.0], 0.0);
    let (lo, hi) = bounds.world_aabb();
    let g = GroundData::capture(&flat_map(), [lo[0], lo[2], hi[0], hi[2]], 2);
    Arc::new(StaticSnapshot {
        self_info: Some(SelfInfo {
            guid: GEN_GUID,
            name: "gen".to_string(),
            bounds,
        }),
        ground: Some(Arc::new(g)),
        biome: Some(Arc::new(world)),
        ..StaticSnapshot::default()
    })
}

fn run(graph: &str, world: BiomeWorld, seed: u32) -> RunResult {
    let g = ProcGraph::from_ron(graph).expect("graph parses");
    let c = match g.compile(&ProcNodeRegistry::with_builtins()) {
        Ok(c) => c,
        Err(e) => panic!("compile failed: {:?}", e.0),
    };
    let mut inputs = RunInputs::new(snapshot(world), seed);
    inputs.keep_debug = true;
    match Evaluator::new().run(&c, &inputs, &CancelToken::new()) {
        Ok(r) => r,
        Err(e) => panic!("run failed: {e}"),
    }
}

fn counter_sum(r: &RunResult, suffix: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for n in &r.report.nodes {
        for (k, v) in &n.counters {
            if let Some(name) = k.strip_suffix(suffix) {
                *out.entry(name.to_string()).or_default() += v;
            }
        }
    }
    out
}

/// `(entry, x, z, spread, scale)` of node `id`'s output.
fn points(r: &RunResult, id: u32) -> Vec<(String, f32, f32, f32, f32)> {
    let mut out = Vec::new();
    let Some(b) = r.debug.as_ref().and_then(|d| d.get(&id)).and_then(|v| v.first()) else {
        return out;
    };
    for item in &b.0 {
        let Some(p) = item.as_points() else { continue };
        let names = match p.attrs.get("entry") {
            Some(AttrColumn::Name(v)) => v.to_vec(),
            _ => vec![String::new(); p.len()],
        };
        let spreads = match p.attrs.get("spread_m") {
            Some(AttrColumn::Float(v)) => v.to_vec(),
            _ => vec![0.0; p.len()],
        };
        for i in 0..p.len() {
            out.push((
                names[i].clone(),
                p.position[i][0],
                p.position[i][2],
                spreads[i] as f32,
                p.scale[i][1],
            ));
        }
    }
    out
}

#[test]
fn the_ecology_graph_keeps_shade_herbs_under_crowns_and_is_deterministic_by_seed() {
    let a = run(STARTER_BIOMES_ECOLOGY_GRAPH, eco_world(true, None), 7);
    let b = run(STARTER_BIOMES_ECOLOGY_GRAPH, eco_world(true, None), 7);
    assert_eq!(hex(&a.output_hash), hex(&b.output_hash), "same seed, same output");
    let c = run(STARTER_BIOMES_ECOLOGY_GRAPH, eco_world(true, None), 8);
    assert_ne!(
        hex(&a.output_hash),
        hex(&c.output_hash),
        "the seed changes the output"
    );

    let unshaded = counter_sum(&a, "|ecology_unshaded_out");
    eprintln!(
        "ecology graph seed 7: unshaded {unshaded:?}; shaded {:?}; thinned {:?}; placed {:?}",
        counter_sum(&a, "|ecology_shaded_out"),
        counter_sum(&a, "|ecology_thinned"),
        counter_sum(&a, "|placed")
    );
    for herb in [
        "Beech forest/field/woodruff",
        "Beech forest/field/dogs_mercury",
        "Beech forest/field/male_fern",
    ] {
        assert!(
            unshaded.get(herb).copied().unwrap_or(0) > 0,
            "{herb}: {unshaded:?}"
        );
    }
    assert!(
        !unshaded.contains_key("Beech forest/field/wood_anemone"),
        "spring plants are exempt"
    );
    // Every shade herb left stands under a beech-forest crown.
    let out = points(&a, 9);
    let crowns: Vec<_> = out
        .iter()
        .filter(|p| p.0.starts_with("Beech forest/canopy/"))
        .collect();
    assert!(!crowns.is_empty());
    let herbs: Vec<_> = out
        .iter()
        .filter(|p| p.0 == "Beech forest/field/woodruff")
        .collect();
    assert!(!herbs.is_empty(), "some woodruff survives under the crowns");
    for h in &herbs {
        let covered = crowns
            .iter()
            .any(|c| (c.1 - h.1).powi(2) + (c.2 - h.2).powi(2) < c.3 * c.3);
        assert!(covered, "woodruff at ({}, {}) has no crown over it", h.1, h.2);
    }
    // Pine heath plants keep their light: nothing of the pine forest is shaded out.
    let shaded = counter_sum(&a, "|ecology_shaded_out");
    assert!(
        !shaded.keys().any(|k| k.starts_with("Boreal pine forest/")),
        "{shaded:?}"
    );
}

#[test]
fn self_thinning_removes_small_stems_first() {
    let r = run(STARTER_BIOMES_ECOLOGY_GRAPH, eco_world(true, Some(60.0)), 7);
    let thinned = counter_sum(&r, "|ecology_thinned");
    let total: u64 = thinned.values().sum();
    assert!(total > 0, "{thinned:?}");
    assert!(
        thinned.keys().all(|k| k.contains("/canopy/")),
        "only trees thin: {thinned:?}"
    );
    // The kept canopy is larger on average than the input to the node.
    let before = points(&r, 5);
    let after = points(&r, 9);
    let mean = |v: &[(String, f32, f32, f32, f32)]| {
        let c: Vec<f32> = v
            .iter()
            .filter(|p| p.0.contains("/canopy/"))
            .map(|p| p.4)
            .collect();
        c.iter().sum::<f32>() / c.len().max(1) as f32
    };
    assert!(
        mean(&after) > mean(&before),
        "{} vs {}",
        mean(&after),
        mean(&before)
    );
}

#[test]
fn the_starter_graph_ignores_ecology_profiles() {
    let with = run(STARTER_BIOMES_GRAPH, eco_world(true, None), 7);
    let without = run(STARTER_BIOMES_GRAPH, eco_world(false, None), 7);
    assert_eq!(hex(&with.output_hash), hex(&without.output_hash));
    // The ecology graph without profiles still applies the shade rules by species name.
    let named = run(STARTER_BIOMES_ECOLOGY_GRAPH, eco_world(false, None), 7);
    assert!(counter_sum(&named, "|ecology_unshaded_out").values().sum::<u64>() > 0);
    assert_eq!(
        counter_sum(&named, "|ecology_thinned").values().sum::<u64>(),
        0,
        "no thinning without a profile"
    );
}

/// Determinism audit finding 3: the self-thinning load is `detmath::pow`, not the platform libm's
/// `f64::powf`. The stop test slack is absolute (1e-9), far above the f64 libm gap at tree
/// scales, so these sizes (found by scanning the f32 range) make the gap cross the slack: glibc
/// `powf` sits ~2e-7 below `detmath::pow` on each, and a budget midway between the two sums
/// thins the smallest stem under detmath but keeps both under libm — the platform-dependent
/// outcome the finding is about.
#[test]
fn competition_pass_load_uses_detmath_pow() {
    use regia_procgen::biome::ecology::{CompOut, CompPoint, SelfThinning, Stratum, competition_pass};
    use regia_procgen::detmath;

    let exp = 1.605f64;
    // Sizes where glibc `powf(x, 1.605)` is ~2e-7 below `detmath::pow` (probe, f32-range scan):
    // bits 0x47878243 and 0x47882d3b. The test recomputes the gap instead of trusting it.
    let s_small = f32::from_bits(0x4787_8243);
    let s_big = f32::from_bits(0x4788_2d3b);
    let mk = |size: f32, seed: u32| CompPoint {
        x: 0.0,
        z: seed as f32,
        spread_m: 0.0,
        size,
        seed,
        stratum: Some(Stratum::Canopy),
        light: 0.0,
        casts_shade: 0.0,
        vernal: false,
        shade_rules: false,
        thinning: Some((
            3,
            SelfThinning {
                cell_m: 25.0,
                max_stems_per_ha: 1.0, // set from the budget below
                exponent: exp,
            },
        )),
    };
    let load = |s: f32| detmath::pow(f64::from(s), exp);
    let a_small = load(s_small);
    let a_big = load(s_big);
    let p_sum = f64::from(s_small).powf(exp) + f64::from(s_big).powf(exp);
    let a_sum = a_small + a_big;
    let gap = a_sum - p_sum;
    assert!(
        gap > 1e-7,
        "the probe sizes must diverge by more than the 1e-9 slack: gap {gap:e}"
    );
    let budget = a_sum - gap * 0.5;
    // budget = max_stems_per_ha * cell_m^2 / 10_000 with cell_m = 25.
    let max_stems = budget * 10_000.0 / (25.0f64 * 25.0);
    let mut pts = vec![mk(s_big, 2), mk(s_small, 1)];
    for p in &mut pts {
        p.thinning = p.thinning.map(|(g, mut t)| {
            t.max_stems_per_ha = max_stems;
            (g, t)
        });
    }
    let verdict = competition_pass(&pts);
    // The detmath reference: thin the smallest while the sum is over budget + slack.
    let mut expect = vec![None, None];
    let mut total = a_sum;
    if total > budget + 1e-9 {
        total -= a_small;
        expect[1] = Some(CompOut::Thinned);
    }
    assert!(total <= budget + 1e-9, "the reference stops after one stem");
    assert_eq!(verdict, expect, "the load must decide by detmath, not libm powf");

    // The audit's non-cube case at tree scale: scales (1.1, 1.2, 1.3). Same product, any order of
    // the factors: one size, ties broken by seed; a slightly larger product loses its stem.
    let size_of =
        |a: f32, b: f32, c: f32| detmath::pow(f64::from(a.abs() * b.abs() * c.abs()), 1.0 / 3.0) as f32;
    let s = size_of(1.1, 1.2, 1.3);
    assert_eq!(
        s,
        size_of(1.3, 1.1, 1.2),
        "the geometric mean is factor-order independent"
    );
    let mut same = vec![mk(s, 9), mk(s, 4)];
    for p in &mut same {
        p.thinning = p.thinning.map(|(g, mut t)| {
            t.max_stems_per_ha = 0.0; // budget 0: everything thin-able goes
            (g, t)
        });
    }
    let verdict = competition_pass(&same);
    assert_eq!(
        verdict,
        vec![Some(CompOut::Thinned), Some(CompOut::Thinned)],
        "equal sizes in one cell: both over the zero budget go, in seed order"
    );
}
