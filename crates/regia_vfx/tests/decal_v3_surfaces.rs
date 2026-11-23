// SPDX-License-Identifier: MPL-2.0
//! ADR-8829: decal v3 integration. Damage-atlas cells register as v3 custom images, and a v3
//! prefab's footstep `surface` follows its decal through every placement path. No GPU.

use regia_core::math::{Quat, Vec3};
use regia_vfx::{
    DAMAGE_DECAL_ATLAS_SIZE, DAMAGE_DECAL_CELL, DECAL_ATLAS_LAYERS, DECAL_ATLAS_SIZE,
    DECAL_FOOTSTEP_NAMES_MAX, Decal, DecalLibrary, DecalPool, DecalPrefab, DecalSurface,
    damage_decal_cell_image, damage_decal_cell_key, generate_damage_decal_atlas,
};

fn prefab(name: &str, extra: &str) -> DecalPrefab {
    DecalPrefab::from_ron(&format!(
        "(version: 3, name: {name:?}, half_size: (1.0, 1.0, 0.3){extra})"
    ))
    .expect("v3 prefab")
}

/// An up-facing ground decal (local +Z along world +Y) at `at`, with `priority`.
fn ground(at: Vec3, priority: i16) -> (Decal, DecalSurface) {
    let (decal, mut surface) = prefab("g", "")
        .instantiate(at, Vec3::Y, 0.0, None)
        .expect("place");
    surface.priority = priority;
    (decal, surface)
}

#[test]
fn damage_atlas_cells_register_as_v3_custom_images() {
    let atlas = generate_damage_decal_atlas();
    let mut lib = DecalLibrary::builtin();
    let builtins = lib.texture_atlas().rgba().to_vec();
    let page = (DECAL_ATLAS_SIZE * DECAL_ATLAS_SIZE * 4) as usize;
    assert_eq!(builtins.len(), DECAL_ATLAS_LAYERS as usize * page);

    lib.add_damage_cell(prefab("hole", ""), &atlas, 0)
        .expect("cell 0");
    let hole = lib.get("hole").expect("registered");
    assert_eq!(hole.texture.as_deref(), Some("damage-atlas/cell-00"));
    assert_eq!(damage_decal_cell_key(15), "damage-atlas/cell-15");
    assert_eq!(lib.texture_atlas().layers(), DECAL_ATLAS_LAYERS + 1);
    let (_, surface) = hole.instantiate(Vec3::ZERO, Vec3::Y, 0.0, None).expect("place");
    assert_eq!(surface.atlas_layer, DECAL_ATLAS_LAYERS as i32);

    // The custom page is the cell, nearest-sampled at texel centres; the built-ins keep their bytes.
    let rgba = lib.texture_atlas().rgba();
    assert_eq!(&rgba[..builtins.len()], &builtins[..], "built-in pages unchanged");
    let cell = damage_decal_cell_image(&atlas, 0).expect("cell image");
    assert_eq!((cell.width, cell.height), (DAMAGE_DECAL_CELL, DAMAGE_DECAL_CELL));
    let base = DECAL_ATLAS_LAYERS as usize * page;
    for (x, y) in [(0u32, 0u32), (32, 32), (17, 40), (63, 63)] {
        let s = |v: u32| ((v * 2 + 1) * DAMAGE_DECAL_CELL / (DECAL_ATLAS_SIZE * 2)) as usize;
        let src = (s(y) * DAMAGE_DECAL_CELL as usize + s(x)) * 4;
        let dst = base + ((y * DECAL_ATLAS_SIZE + x) * 4) as usize;
        assert_eq!(&rgba[dst..dst + 4], &cell.rgba8[src..src + 4], "texel ({x}, {y})");
    }
    // The cell is the atlas's own bytes: cell 15 starts at (768, 768).
    let rivets = damage_decal_cell_image(&atlas, 15).expect("cell 15");
    let side = DAMAGE_DECAL_ATLAS_SIZE as usize;
    let at = ((768 + 128) * side + 768 + 22) * 4;
    assert_eq!(&rivets.rgba8[(128 * 256 + 22) * 4..][..4], &atlas[at..at + 4]);

    // The same cell shares its layer; another cell takes the next one.
    lib.add_damage_cell(prefab("hole_b", ""), &atlas, 0)
        .expect("cell 0 again");
    assert_eq!(lib.texture_atlas().layers(), DECAL_ATLAS_LAYERS + 1);
    lib.add_damage_cell(prefab("rivet", ""), &atlas, 15)
        .expect("cell 15");
    assert_eq!(lib.texture_atlas().layers(), DECAL_ATLAS_LAYERS + 2);

    // Refusals change nothing.
    let before = std::sync::Arc::clone(lib.texture_atlas());
    assert!(lib.add_damage_cell(prefab("x", ""), &atlas, 16).is_err());
    assert!(
        lib.add_damage_cell(prefab("y", ""), &atlas[..atlas.len() - 4], 1)
            .is_err()
    );
    assert!(damage_decal_cell_image(&atlas, 16).is_none());
    assert!(std::sync::Arc::ptr_eq(&before, lib.texture_atlas()));
    assert!(lib.get("x").is_none() && lib.get("y").is_none());
}

#[test]
fn pool_answers_the_footstep_surface_on_top_at_a_point() {
    let mut pool = DecalPool::new(8);
    let (d, s) = ground(Vec3::ZERO, 0);
    let low = pool.spawn_with(d, s);
    assert!(pool.set_footstep_surface(low, Some("mud")));
    assert_eq!(pool.footstep_surface(low), Some("mud"));
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.9, 0.0, -0.9)), Some("mud"));
    assert_eq!(
        pool.footstep_surface_at(Vec3::new(1.1, 0.0, 0.0)),
        None,
        "outside the box"
    );
    assert_eq!(
        pool.footstep_surface_at(Vec3::new(0.0, 0.4, 0.0)),
        None,
        "above the depth"
    );

    // A decal with no surface on top changes nothing: footsteps keep the covering surface.
    let (d, s) = ground(Vec3::ZERO, 5);
    let plain = pool.spawn_with(d, s);
    assert_eq!(pool.footstep_surface(plain), None);
    assert_eq!(pool.footstep_surface_at(Vec3::ZERO), Some("mud"));

    // An older decal of higher priority beats a newer one; on a tie the newest wins.
    let (d, s) = ground(Vec3::new(0.5, 0.0, 0.0), 2);
    let high = pool.spawn_with(d, s);
    pool.set_footstep_surface(high, Some("oil"));
    let (d, s) = ground(Vec3::new(0.5, 0.0, 0.0), 1);
    let newer_low = pool.spawn_with(d, s);
    pool.set_footstep_surface(newer_low, Some("puddle"));
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.5, 0.0, 0.0)), Some("oil"));
    let (d, s) = ground(Vec3::new(0.5, 0.0, 0.0), 2);
    let newer_tie = pool.spawn_with(d, s);
    pool.set_footstep_surface(newer_tie, Some("blood"));
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.5, 0.0, 0.0)), Some("blood"));
    assert_eq!(pool.footstep_surface_at(Vec3::new(-0.8, 0.0, 0.0)), Some("mud"));

    // Removing decals keeps the others' surfaces on the right decals.
    assert!(pool.remove(newer_tie));
    assert!(pool.remove(plain));
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.5, 0.0, 0.0)), Some("oil"));
    assert_eq!(pool.footstep_surface(newer_low), Some("puddle"));
    assert_eq!(pool.footstep_surface(low), Some("mud"));
    assert!(!pool.set_footstep_surface(newer_tie, Some("x")), "a retired id");
    assert!(pool.set_footstep_surface(high, None));
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.5, 0.0, 0.0)), Some("puddle"));
}

#[test]
fn footstep_surfaces_follow_rotation_retirement_and_recycling() {
    // A long strip turned 90 degrees around world Y: its across axis runs along world Z.
    let mut pool = DecalPool::new(2);
    let (mut d, s) = ground(Vec3::ZERO, 0);
    d.half_size = Vec3::new(3.0, 0.5, 0.3);
    d.rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2) * d.rotation;
    d.lifetime = Some(1.0);
    let strip = pool.spawn_with(d, s);
    pool.set_footstep_surface(strip, Some("gravel"));
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.0, 0.0, 2.5)), Some("gravel"));
    assert_eq!(pool.footstep_surface_at(Vec3::new(2.5, 0.0, 0.0)), None);

    // Lifetime retirement drops the surface with the decal.
    pool.update(0.5);
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.0, 0.0, 2.5)), Some("gravel"));
    pool.update(0.6);
    assert!(!pool.contains(strip));
    assert_eq!(pool.footstep_surface_at(Vec3::new(0.0, 0.0, 2.5)), None);

    // A full pool recycles the oldest decal; the new one does not inherit its surface.
    let (d, s) = ground(Vec3::ZERO, 0);
    let a = pool.spawn_with(d, s);
    pool.set_footstep_surface(a, Some("sand"));
    let (d, s) = ground(Vec3::new(5.0, 0.0, 0.0), 0);
    let b = pool.spawn_with(d, s);
    pool.set_footstep_surface(b, Some("snow"));
    let (d, s) = ground(Vec3::ZERO, 0);
    let c = pool.spawn_with(d, s);
    assert!(!pool.contains(a), "the oldest was recycled");
    assert_eq!(pool.footstep_surface(c), None);
    assert_eq!(pool.footstep_surface_at(Vec3::ZERO), None);
    assert_eq!(pool.footstep_surface_at(Vec3::new(5.0, 0.0, 0.0)), Some("snow"));
}

#[test]
fn footstep_surface_names_are_bounded() {
    let mut pool = DecalPool::new(1);
    let (d, s) = ground(Vec3::ZERO, 0);
    let id = pool.spawn_with(d, s);
    for i in 0..DECAL_FOOTSTEP_NAMES_MAX {
        assert!(pool.set_footstep_surface(id, Some(&format!("s{i}"))));
    }
    assert!(
        pool.set_footstep_surface(id, Some("s0")),
        "a known name still sets"
    );
    assert!(
        !pool.set_footstep_surface(id, Some("one_too_many")),
        "a new name past the cap is refused"
    );
    assert_eq!(pool.footstep_surface(id), Some("s0"), "a refusal changes nothing");
}

#[test]
fn every_placement_path_records_the_prefab_surface() {
    use regia_app::App;
    use regia_vfx::{PlaceDecal, StaticDecal, VfxPlugin, VfxSettings, place_decal};
    let dir = regia_test_utils::TempDir::new("decal-surface-app");
    let write = |surface: &str| {
        let text = prefab("puddle", &format!(", surface: Some({surface:?})"))
            .to_ron()
            .expect("ron");
        let tmp = dir.join("puddle.decal.tmp");
        std::fs::write(&tmp, text).expect("prefab");
        std::fs::rename(&tmp, dir.join("puddle.decal.ron")).expect("publish");
    };
    write("puddle");
    let mut app = App::new();
    app.insert_resource(VfxSettings {
        library_dir: Some(dir.to_path_buf()),
        ..VfxSettings::default()
    });
    app.add_plugins(VfxPlugin);
    app.update();
    app.world_mut().spawn((
        StaticDecal {
            prefab: "puddle".into(),
            ..StaticDecal::default()
        },
        regia_scene::Transform::default(),
    ));
    let request = PlaceDecal {
        position: Vec3::new(10.0, 0.0, 0.0),
        normal: Vec3::Y,
        yaw: 0.0,
        prefab: "puddle".into(),
        lifetime: None,
    };
    let placed = place_decal(app.world_mut(), &request).expect("placed");
    let pool = app.world().resource::<DecalPool>();
    assert_eq!(pool.len(), 2);
    assert_eq!(
        pool.footstep_surface_at(Vec3::ZERO),
        Some("puddle"),
        "static decal"
    );
    assert_eq!(pool.footstep_surface(placed), Some("puddle"), "place_decal");

    // A prefab reload re-instantiates static decals in place with the new surface; a decal
    // placed once keeps what it was placed with.
    write("oil");
    app.world_mut()
        .write_message(regia_asset::SourceAssetChanged::new(dir.join("puddle.decal.ron")));
    app.update();
    let pool = app.world().resource::<DecalPool>();
    assert_eq!(pool.len(), 2);
    assert_eq!(
        pool.footstep_surface_at(Vec3::ZERO),
        Some("oil"),
        "reloaded static decal"
    );
    assert_eq!(pool.footstep_surface(placed), Some("puddle"));
}
