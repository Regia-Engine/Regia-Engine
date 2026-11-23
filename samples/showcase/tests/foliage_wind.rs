// SPDX-License-Identifier: MIT-0
//! `--foliage-wind` (spec 88 § 2.5, ADR-2304), without a GPU: the example scene's cone pines take the sway
//! and its rocks do not, the streamed field builds from the swaying library, and the app reports how many
//! materials sway. Off (the default) leaves the library as loaded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia::prelude::*;
use regia_app::enable_fixed_step;
use regia_render::FoliageSway;
use showcase::STEP_HZ;
use showcase::app::{Options, ShowcasePlugin, ShowcaseStats, build_scatter_field_with, swaying_library};
use showcase::scene::{SceneData, load_scene};

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")
}

#[test]
fn the_pines_sway_and_the_rocks_stand_still() {
    let scene = load_scene(&example_dir()).expect("the example scene loads");
    let (library, adopted) = swaying_library(&scene);
    assert_eq!(library.len(), scene.library.len());
    let pine = library.get("pine").expect("the example scatters pines");
    let sway = FoliageSway::of_material(&pine.material).expect("the pine sways");
    // The cone stand-in is 1 m tall at scale 1: the bend is full at its tip.
    assert!((sway.height_m - 1.0).abs() < 1e-4, "{sway:?}");
    assert!(adopted >= 1);
    for rock in ["rock_a", "rock_b"] {
        let asset = library.get(rock).expect("the example scatters rocks");
        assert!(
            FoliageSway::of_material(&asset.material).is_none(),
            "{rock} stands still"
        );
        assert!(asset.material.surface_graph.is_none());
    }
    let loaded = scene.library.get("pine").expect("pine");
    assert!(
        FoliageSway::of_material(&loaded.material).is_none(),
        "the scene's own library is untouched"
    );
    build_scatter_field_with(&scene, &library).expect("the field builds from the swaying library");
}

fn app(scene: SceneData, foliage_wind: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, STEP_HZ);
    app.add_plugins(ShowcasePlugin {
        scene: Arc::new(scene),
        options: Options {
            plain: true,
            foliage_wind,
            ..Options::default()
        },
    });
    app
}

#[test]
fn the_flag_reaches_the_streamed_field() {
    for on in [true, false] {
        let scene = load_scene(&example_dir()).expect("the example scene loads");
        let (_, adopted) = swaying_library(&scene);
        let mut app = app(scene, on);
        for _ in 0..3 {
            app.update();
        }
        let stats = app.world().resource::<ShowcaseStats>().clone();
        assert!(stats.scatter_streamed, "{:?}", stats.scatter_error);
        assert_eq!(stats.swaying_materials, if on { adopted } else { 0 });
        assert!(!on || stats.swaying_materials > 0);
    }
}
