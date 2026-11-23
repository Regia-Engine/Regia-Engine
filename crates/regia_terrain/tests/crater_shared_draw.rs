// SPDX-License-Identifier: MPL-2.0
//! A crater applied through `TerrainSharedDraw::update_heights` reaches the draw's own map and
//! min/max pyramid (ADR-2301), so `rendered_heights` and node selection see it without a
//! rebuild. Before this, only the GPU texture and its mips changed.

use std::sync::OnceLock;

use regia_render::RenderDevice;
use regia_terrain::deform::{Crater, apply_crater};
use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap, TerrainSharedDraw};

const WORLD_M: f32 = 512.0;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

fn flat_map() -> TerrainMap {
    let n = 129u32;
    TerrainMap::from_heights(n, n, vec![10.0; (n * n) as usize], WORLD_M, 1.0, 0.0).expect("map")
}

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"
        TerrainLayers(
            version: 1,
            wetness_default: 0.0,
            layers: [
                LayerDesc(
                    name: "dust",
                    albedo: (0.62, 0.40, 0.27),
                    roughness: 0.9,
                    tiling: 4.0,
                    slope_min: 0.0,
                    slope_max: 1.6,
                    altitude_min: -1000.0,
                    altitude_max: 5000.0,
                    blend_sharpness: 0.15,
                    triplanar: false,
                ),
            ],
        )
        "#,
    )
    .expect("layers")
}

#[test]
fn a_crater_updates_the_draws_own_heights_and_leaves_far_samples_alone() {
    let map = flat_map();
    let mut draw =
        TerrainSharedDraw::new(device(), map.clone(), &layers(), CdlodConfig::default(), 4).expect("draw");
    let before = draw.rendered_heights(&[[0.0, 0.0], [200.0, 200.0]]);
    assert!((before[0] - 10.0).abs() < 1e-3 && (before[1] - 10.0).abs() < 1e-3);

    let mut edited = map;
    let crater = Crater {
        center: [0.0, 0.0],
        radius: 24.0,
        depth: 6.0,
        rim_height: 1.0,
    };
    let dirty = apply_crater(&mut edited, &crater).expect("crater");
    assert!(!dirty.is_empty());
    draw.update_heights(&edited, &dirty);

    let after = draw.rendered_heights(&[[0.0, 0.0], [200.0, 200.0]]);
    let want = edited.sample_bilinear(0.0, 0.0);
    assert!(want < 8.0, "the crater dug the centre: {want}");
    assert!(
        (after[0] - want).abs() < 1e-3,
        "draw sees {} want {want}",
        after[0]
    );
    assert!((after[1] - 10.0).abs() < 1e-3, "far sample untouched");
}

#[test]
fn a_map_of_another_size_is_uploaded_but_leaves_the_draws_map_alone() {
    let mut draw =
        TerrainSharedDraw::new(device(), flat_map(), &layers(), CdlodConfig::default(), 4).expect("draw");
    let n = 65u32;
    let other =
        TerrainMap::from_heights(n, n, vec![50.0; (n * n) as usize], WORLD_M, 1.0, 0.0).expect("other");
    let dirty = regia_terrain::deform::DirtyRect {
        col_start: 0,
        col_end: 8,
        row_start: 0,
        row_end: 8,
    };
    draw.update_heights(&other, &dirty);
    let h = draw.rendered_heights(&[[-250.0, -250.0]]);
    assert!((h[0] - 10.0).abs() < 1e-3, "kept its own map: {}", h[0]);
}

/// Updating the whole map with the heights the draw already holds is a no-op (ADR-2450): the
/// shadow and content keys (cached cascades, held screen-space gathers, the probes' heightfield
/// generation) stay put. A real edit still moves them.
#[test]
fn a_whole_map_update_with_the_same_heights_changes_no_key() {
    use regia_render::SharedPassDraw;
    let map = flat_map();
    let mut draw =
        TerrainSharedDraw::new(device(), map.clone(), &layers(), CdlodConfig::default(), 4).expect("draw");
    let whole = regia_terrain::deform::DirtyRect {
        col_start: 0,
        col_end: map.width(),
        row_start: 0,
        row_end: map.height(),
    };
    let (shadow, content) = (draw.shadow_key(), draw.content_key());
    draw.update_heights(&map, &whole);
    assert_eq!(draw.shadow_key(), shadow, "a no-op update moved the shadow key");
    assert_eq!(
        draw.content_key(),
        content,
        "a no-op update moved the content key"
    );

    let mut edited = map;
    let crater = Crater {
        center: [0.0, 0.0],
        radius: 24.0,
        depth: 6.0,
        rim_height: 1.0,
    };
    apply_crater(&mut edited, &crater).expect("crater");
    draw.update_heights(&edited, &whole);
    assert_ne!(draw.content_key(), content, "a crater left the content key");
    // The same edited map again changes nothing more.
    let after = draw.content_key();
    draw.update_heights(&edited, &whole);
    assert_eq!(draw.content_key(), after);
}
