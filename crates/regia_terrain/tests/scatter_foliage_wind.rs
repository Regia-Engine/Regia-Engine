// SPDX-License-Identifier: MPL-2.0
//! Foliage sway on the scatter field (`docs/specs/88-foliage-wind.md` § 2.5, ADR-2304): a mesh
//! whose material sways draws swaying for seeded and painted instances, a brush with `wind` off
//! paints instances that draw the same surface at rest, the flag survives a save, and a mesh whose
//! material does not sway is drawn exactly as before.

use std::sync::Arc;

use regia_core::glam::Vec2;
use regia_render::{ExtractedMesh, FOLIAGE_SWAY_VERTEX, FoliageSway};
use regia_terrain::{
    BrushShape, PAINTED_FLAG_STILL, RockStyle, ScatterBrush, ScatterClass, ScatterField, ScatterLibrary,
    ScatterMeshAsset, ScatterMeshRef, ScatterSet, ScatterSource, ScatterView, Terrain, TerrainEdits,
    TerrainMap, TerrainScatterPaint, apply_scatter_stroke, procedural_rock,
};

fn map_arc() -> Arc<TerrainMap> {
    Arc::new(TerrainMap::from_heights(257, 257, vec![10.0; 257 * 257], 512.0, 1.0, 0.0).expect("map"))
}

fn class(name: &str, density: f32) -> ScatterClass {
    ScatterClass {
        name: name.into(),
        meshes: vec![ScatterMeshRef {
            mesh: "shrub".into(),
            weight: 1.0,
            min_scale: 1.0,
            max_scale: 2.0,
            scale_bias: 1.0,
        }],
        cell_size: 8.0,
        density,
        contact_ao: 0.0,
        fade_start: 400.0,
        fade_end: 500.0,
        ..ScatterClass::default()
    }
}

fn set() -> ScatterSet {
    ScatterSet {
        seed: 41,
        classes: vec![class("seeded", 0.5), class("painted", 0.0)],
        ..ScatterSet::default()
    }
}

fn sway() -> FoliageSway {
    FoliageSway {
        height_m: 1.5,
        ..FoliageSway::default()
    }
}

/// A procedural rock standing in for a shrub, its material swaying (or not).
fn shrub(swaying: bool) -> ScatterMeshAsset {
    let mut asset = procedural_rock(3, RockStyle::rounded([0.2, 0.4, 0.15]));
    if swaying {
        asset.material = sway().adopt(&asset.material).expect("plain material adopts");
    }
    asset
}

fn field(swaying: bool) -> ScatterField {
    let mut library = ScatterLibrary::new();
    library.insert("shrub".into(), shrub(swaying));
    let mut field = ScatterField::new(&set(), ScatterSource::new(map_arc()), &library, 0xB1).expect("field");
    field.set_background_streaming(false);
    field
}

fn terrain() -> Terrain {
    let map = map_arc();
    let heights = regia_asset::Heightfield {
        width: map.width(),
        height: map.height(),
        samples: vec![10_000; 257 * 257],
    };
    Terrain::new(
        &heights,
        map.world_width(),
        65.535,
        0.0,
        regia_terrain::CdlodConfig::default(),
        4,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain")
}

fn view() -> ScatterView {
    ScatterView::look_at(
        [0.0, 120.0, 150.0],
        [0.0, 10.0, 0.0],
        0.9,
        16.0 / 9.0,
        0.1,
        2000.0,
    )
}

fn emitted(field: &mut ScatterField) -> Vec<ExtractedMesh> {
    field.update(view().eye);
    let mut out = Vec::new();
    field.extract_meshes(&view(), &mut out);
    out
}

fn swaying(mesh: &ExtractedMesh) -> bool {
    mesh.material
        .surface_graph
        .as_deref()
        .is_some_and(|g| g.vertex_wgsl.as_deref() == Some(FOLIAGE_SWAY_VERTEX))
}

/// Paints the `painted` class: a calm brush on the left, a windy one on the right.
fn paint_both_halves() -> TerrainScatterPaint {
    let t = terrain();
    let mut paint = TerrainScatterPaint::default();
    let mut edits = TerrainEdits::default();
    let brush = |wind: bool| ScatterBrush {
        shape: BrushShape::Circle { radius: 20.0 },
        hardness: 1.0,
        density: 0.3,
        wind,
        ..ScatterBrush::for_class(&class("painted", 0.0))
    };
    for (wind, x) in [(false, -40.0), (true, 40.0)] {
        apply_scatter_stroke(
            &t,
            &mut paint,
            &mut edits,
            &set(),
            &brush(wind),
            &[Vec2::new(x, 0.0)],
        )
        .expect("stroke");
    }
    paint
}

#[test]
fn the_brush_writes_the_still_flag_only_with_wind_off() {
    assert!(
        ScatterBrush::default().wind,
        "painting follows the wind by default"
    );
    let paint = paint_both_halves();
    let mut still = 0;
    let mut windy = 0;
    for i in paint.0.instances("painted") {
        if i.flags & PAINTED_FLAG_STILL != 0 {
            assert!(i.position[0] < 0.0, "only the calm stroke paints still instances");
            still += 1;
        } else {
            assert!(i.position[0] > 0.0);
            windy += 1;
        }
        assert_eq!(i.flags & !PAINTED_FLAG_STILL, 0, "reserved bits stay 0");
    }
    assert!(still > 50 && windy > 50, "{still} still, {windy} windy");
}

#[test]
fn seeded_and_windy_painted_instances_sway_and_still_ones_rest() {
    let paint = paint_both_halves();
    let mut field = field(true);
    let seeded = emitted(&mut field);
    assert!(!seeded.is_empty());
    assert!(seeded.iter().all(swaying), "seeded instances follow the material");
    field.set_painted(&paint.0);
    let both = emitted(&mut field);
    let rest: Vec<&ExtractedMesh> = both.iter().filter(|m| !swaying(m)).collect();
    let still_painted = paint
        .0
        .instances("painted")
        .filter(|i| i.flags & PAINTED_FLAG_STILL != 0)
        .count();
    assert_eq!(
        rest.len(),
        still_painted,
        "exactly the still instances draw at rest"
    );
    for mesh in &rest {
        let graph = mesh
            .material
            .surface_graph
            .as_deref()
            .expect("same graph surface");
        assert!(graph.vertex_wgsl.is_none());
        assert_eq!(FoliageSway::of_material(&mesh.material), None);
    }
    // The still copy is shared per tint variant, not made per instance.
    let mut materials: Vec<*const regia_render::LabMaterial> =
        rest.iter().map(|m| Arc::as_ptr(&m.material)).collect();
    materials.sort_unstable();
    materials.dedup();
    assert!(materials.len() <= 4, "{} still materials", materials.len());
    // The surface is the same at rest and swaying: same texture bytes, same parameters but the
    // sway's own slots.
    let a = both.iter().find(|m| swaying(m)).expect("a swaying draw");
    let (ga, gr) = (
        a.material.surface_graph.as_deref().expect("graph"),
        rest[0].material.surface_graph.as_deref().expect("graph"),
    );
    assert_eq!(ga.wgsl, gr.wgsl);
}

#[test]
fn a_material_that_does_not_sway_ignores_the_flag() {
    let paint = paint_both_halves();
    let mut field = field(false);
    field.set_painted(&paint.0);
    let both = emitted(&mut field);
    assert!(!both.is_empty());
    assert!(
        both.iter().all(|m| m.material.surface_graph.is_none()),
        "plain rocks stay plain"
    );
}

#[test]
fn the_flag_survives_a_save_and_a_load() {
    let paint = paint_both_halves();
    let bytes = paint.0.to_bytes().expect("save");
    let loaded = regia_terrain::PaintedScatter::from_bytes(&bytes).expect("load");
    let flags = |p: &regia_terrain::PaintedScatter| {
        let mut f: Vec<(u64, u16)> = p.instances("painted").map(|i| (i.id, i.flags)).collect();
        f.sort_unstable();
        f
    };
    assert_eq!(flags(&loaded), flags(&paint.0));
    assert!(flags(&loaded).iter().any(|&(_, f)| f == PAINTED_FLAG_STILL));
}

#[test]
fn a_swaying_mesh_bakes_the_same_impostor_cards_as_the_plain_one() {
    // The CPU impostor bake reads the standard texture slots; an adopted material's maps live in
    // its graph, so the field bakes from the maps given back (`FoliageSway::plain`).
    let settings = regia_terrain::ImpostorSettings {
        grid: 4,
        cell: 16,
        ..regia_terrain::ImpostorSettings::default()
    };
    let swaying = field(true).bake_impostors(Some(settings));
    let plain = field(false).bake_impostors(Some(settings));
    assert!(!plain.cards.is_empty());
    assert_eq!(swaying.cards, plain.cards);
}
