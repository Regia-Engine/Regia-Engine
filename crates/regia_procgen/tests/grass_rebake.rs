// SPDX-License-Identifier: MPL-2.0
//! Biome grass multipliers reach the host's grass (spec 65 § 8, ADR-7214): with a `GrassRebake`
//! resource, a change of `GrassMultipliers` re-bakes the mask and queues it on the layer's handle,
//! so a lower multiplier means fewer grass clumps. Headless, CPU only: the queued mask's density
//! total is what the GPU keeps clumps in proportion to (`GrassMask::density_total`).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_procgen::biome::ecs::GrassRebake;
use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGenTerrain, ProcGraphSource};
use regia_terrain::grass::{GrassMultipliers, MultiplierImage};
use regia_terrain::{GrassSet, TerrainGrassHandle, TerrainMap};

const CLASS: &str = "summer_grass";

fn app() -> (App, TerrainGrassHandle) {
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    let map = TerrainMap::from_heights(33, 33, vec![0.0; 33 * 33], 32.0, 1.0, 0.0).expect("map");
    app.insert_resource(ProcGenTerrain(map));
    let handle = TerrainGrassHandle::new_linked();
    let set = GrassSet::summer();
    assert_eq!(set.classes[0].name, CLASS);
    app.insert_resource(GrassRebake::new(set, handle.clone()));
    (app, handle)
}

fn multipliers(value: u8) -> GrassMultipliers {
    let mut by_class = BTreeMap::new();
    by_class.insert(
        CLASS.to_string(),
        MultiplierImage {
            side: 4,
            texels: vec![value; 16],
        },
    );
    GrassMultipliers { by_class }
}

fn rebakes(app: &App) -> u64 {
    app.world().resource::<GrassRebake>().rebakes
}

fn queued_total(handle: &TerrainGrassHandle) -> u64 {
    handle
        .pending_mask()
        .expect("a re-baked mask is queued")
        .density_total(0)
}

#[test]
fn changing_a_multiplier_changes_the_grass_clump_count() {
    let (mut app, handle) = app();
    app.update();
    assert_eq!(rebakes(&app), 1, "the first frame bakes without multipliers");
    let plain = queued_total(&handle);
    assert!(plain > 0, "flat ground grows grass");

    app.insert_resource(multipliers(255));
    app.update();
    assert_eq!(rebakes(&app), 2, "a multiplier change re-bakes");
    let full = queued_total(&handle);
    assert_eq!(full, plain, "a multiplier of 1 keeps the class's own mask");

    app.insert_resource(multipliers(64));
    app.update();
    assert_eq!(rebakes(&app), 3);
    let quarter = queued_total(&handle);
    assert!(
        quarter < full / 2,
        "a quarter multiplier leaves fewer clumps: {quarter} vs {full}"
    );

    // Unchanged frames do not re-bake.
    app.update();
    app.update();
    assert_eq!(rebakes(&app), 3);

    // Removing the multipliers (every biome generator cleared) bakes the plain mask again.
    app.world_mut().remove_resource::<GrassMultipliers>();
    app.update();
    assert_eq!(rebakes(&app), 4);
    assert_eq!(queued_total(&handle), plain);
}

#[test]
fn without_a_rebake_resource_nothing_is_baked() {
    let (mut app, handle) = app();
    app.world_mut().remove_resource::<GrassRebake>();
    app.insert_resource(multipliers(64));
    app.update();
    assert!(handle.pending_mask().is_none());
}

/// A 64 × 64 density map whose −X half is black (0) and +X half white (1).
fn half_black() -> regia_terrain::DensityMap {
    let side = 64_u32;
    let values = (0..side * side)
        .map(|k| if k % side < side / 2 { 0.0 } else { 1.0 })
        .collect();
    regia_terrain::DensityMap::new(side, side, values).expect("density map")
}

/// A class that names a density map re-bakes with the map the host supplied (ADR-7262): a half-black
/// map leaves about half the clumps, and the re-bake neither fails nor keeps the old mask.
#[test]
fn a_class_with_a_density_map_rebakes_with_it() {
    let (mut app, handle) = app();
    app.update();
    let plain = queued_total(&handle);
    assert!(plain > 0, "flat ground grows grass");

    // The same set, the class now naming a map the host loaded.
    let mut set = GrassSet::summer();
    set.classes[0].density_map = Some("maps/grass_density.png".into());
    let mut rebake = GrassRebake::new(set, handle.clone());
    rebake
        .density_maps
        .insert("maps/grass_density.png".into(), half_black());
    app.insert_resource(rebake);
    app.update();
    let world = app.world();
    let rebake = world.resource::<GrassRebake>();
    assert_eq!(rebake.last_error, None, "the map is supplied");
    assert_eq!(rebake.rebakes, 1, "the new resource re-bakes once");
    let half = queued_total(&handle);
    assert!(
        half * 100 >= plain * 40 && half * 100 <= plain * 60,
        "a half-black map leaves about half the clumps: {half} of {plain}"
    );
}

/// Guard: a class naming a map the host did not supply still fails with `last_error` and queues no
/// new mask (the pre-ADR-7262 behavior for an unsupplied map).
#[test]
fn an_unsupplied_density_map_is_still_an_error() {
    let (mut app, handle) = app();
    app.update();
    let plain = queued_total(&handle);
    let mut set = GrassSet::summer();
    set.classes[0].density_map = Some("maps/missing.png".into());
    app.insert_resource(GrassRebake::new(set, handle.clone()));
    app.update();
    let rebake = app.world().resource::<GrassRebake>();
    assert!(
        rebake
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("maps/missing.png")),
        "{:?}",
        rebake.last_error
    );
    assert_eq!(rebake.rebakes, 0);
    assert_eq!(queued_total(&handle), plain, "the old mask stays queued");
}
