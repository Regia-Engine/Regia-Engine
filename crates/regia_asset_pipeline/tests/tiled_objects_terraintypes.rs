// SPDX-License-Identifier: MPL-2.0
//! Tiled objects in world space on isometric, staggered and hexagonal maps, and Tiled 1.0's XML
//! `<terraintypes>` imported as the equivalent Wang corner set (`docs/specs/86-2d-toolkit.md`
//! § 13, ADR-2714). Every fixture is built in the test from the public Tiled format documentation.

use std::path::{Path, PathBuf};

use regia_2d::autotile::{Layout, TerrainBrush};
use regia_2d::tilemap::{MapLayer, MapObject, Orientation, TileLayer, TileMapAsset, TilesetDef};
use regia_asset_pipeline::tiled_import::import_tiled;
use regia_asset_pipeline::tiled_wang::import_wang_autotiles;
use regia_test_utils::unique_temp_dir;

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write fixture");
    path
}

/// A 4 x 3 map of 64 x 32 tiles in `orientation_attrs` with point objects.
fn map_xml(orientation_attrs: &str, tile: (u32, u32), objects: &[(u32, f32, f32)]) -> String {
    let objects: String = objects
        .iter()
        .map(|(id, x, y)| format!("<object id=\"{id}\" name=\"o{id}\" x=\"{x}\" y=\"{y}\"><point/></object>"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<map version="1.10" orientation="{orientation_attrs}" renderorder="right-down" width="4" height="3" tilewidth="{}" tileheight="{}" infinite="0" nextlayerid="3" nextobjectid="9">
 <tileset firstgid="1" name="tiles" tilewidth="{}" tileheight="{}" tilecount="4" columns="4">
  <image source="tiles.png" width="{}" height="{}"/>
 </tileset>
 <layer id="1" name="ground" width="4" height="3"><data encoding="csv">1,1,1,1,1,1,1,1,1,1,1,1</data></layer>
 <objectgroup id="2" name="things">{objects}</objectgroup>
</map>"#,
        tile.0,
        tile.1,
        tile.0,
        tile.1,
        tile.0 * 4,
        tile.1,
    )
}

fn world(map: &TileMapAsset, id: u32) -> [f32; 2] {
    let object: &MapObject = map
        .layers
        .iter()
        .find_map(|l| match l {
            MapLayer::Objects(o) => o.objects.iter().find(|o| o.id == id),
            MapLayer::Tiles(_) => None,
        })
        .expect("object imported");
    map.object_world_position(object)
}

fn imported(dir: &Path, name: &str, text: &str) -> TileMapAsset {
    let imported = import_tiled(&write(dir, name, text)).expect("imports");
    assert!(imported.warnings.is_empty(), "{name}: {:?}", imported.warnings);
    imported.map
}

#[test]
fn isometric_objects_land_on_the_cell_corners_they_name() {
    let dir = unique_temp_dir("iso-objects");
    // 4 x 3 diamonds of 64 x 32: the map box is 224 x 112. Object space is the grid in units of the
    // tile height (32), so (64, 32) is grid point (2, 1).
    let map = imported(
        &dir,
        "iso.tmx",
        &map_xml(
            "isometric",
            (64, 32),
            &[(1, 64.0, 32.0), (2, 0.0, 0.0), (3, 128.0, 96.0)],
        ),
    );
    assert_eq!(map.orientation, Orientation::Isometric);
    assert_eq!(map.pixel_size(), [224, 112]);
    // (2, 1): x = (2 − 1)·32 + 3·32 = 128, y = (2 + 1)·16 = 48 from the top → 64 up.
    assert_eq!(world(&map, 1), [128.0, 64.0]);
    // The grid origin is the top corner of cell (0, 0): the middle of its box, at the very top.
    assert_eq!(world(&map, 2), [96.0, 112.0]);
    // (4, 3): the far corner of the diamond: x = 32 + 96 = 128, y = 7·16 = 112 from the top → 0.
    assert_eq!(world(&map, 3), [128.0, 0.0]);
    // The top corner of cell (x, y) is the object point (x·th, y·th): same as the cell geometry.
    for (x, y) in [(0u32, 0u32), (1, 0), (0, 2), (3, 2)] {
        let [ox, oy] = map
            .orientation
            .object_to_map_pixels(map.grid_size(), [f64::from(x * 32), f64::from(y * 32)]);
        let [bx, by] = map.orientation.cell_origin(map.grid_size(), x, y);
        assert_eq!([ox, oy], [bx + 32.0, by], "cell ({x}, {y})");
        let back = map.orientation.map_pixels_to_object(map.grid_size(), [ox, oy]);
        assert!((back[0] - f64::from(x * 32)).abs() < 1e-9 && (back[1] - f64::from(y * 32)).abs() < 1e-9);
    }
}

#[test]
fn staggered_and_hexagonal_objects_are_map_pixels_flipped_to_world_up() {
    let dir = unique_temp_dir("stagger-objects");
    // Staggered diamonds, axis y, odd: the box is 4·64 + 32 = 288 wide and 2·16 + 32 = 64 tall.
    let staggered = imported(
        &dir,
        "stag.tmx",
        &map_xml(
            "staggered\" staggeraxis=\"y\" staggerindex=\"odd",
            (64, 32),
            &[(1, 50.0, 20.0), (2, 0.0, 64.0)],
        ),
    );
    assert!(matches!(staggered.orientation, Orientation::Staggered { .. }));
    assert_eq!(staggered.pixel_size(), [288, 64]);
    assert_eq!(world(&staggered, 1), [50.0, 44.0]);
    assert_eq!(world(&staggered, 2), [0.0, 0.0]);
    // Pointy hexagons (side 16, 32 x 32 boxes), axis y, even: the box is 4·32 + 16 = 144 wide and
    // 2·(32 + 16)/2 + 32 = 80 tall.
    let hex = imported(
        &dir,
        "hex.tmx",
        &map_xml(
            "hexagonal\" hexsidelength=\"16\" staggeraxis=\"y\" staggerindex=\"even",
            (32, 32),
            &[(1, 10.0, 10.0), (2, 144.0, 80.0)],
        ),
    );
    assert!(matches!(
        hex.orientation,
        Orientation::Hexagonal { side_length: 16, .. }
    ));
    assert_eq!(hex.pixel_size(), [144, 80]);
    assert_eq!(world(&hex, 1), [10.0, 70.0]);
    assert_eq!(world(&hex, 2), [144.0, 0.0]);
    // Flat hexagons, axis x, odd: 3·(32 + 16)/2 + 32 = 104 wide, 3·32 + 16 = 112 tall.
    let flat = imported(
        &dir,
        "flat.tmx",
        &map_xml(
            "hexagonal\" hexsidelength=\"16\" staggeraxis=\"x\" staggerindex=\"odd",
            (32, 32),
            &[(1, 100.0, 12.0)],
        ),
    );
    assert_eq!(flat.pixel_size(), [104, 112]);
    assert_eq!(world(&flat, 1), [100.0, 100.0]);
    // Orthogonal maps are unchanged: map pixels, +Y up.
    let ortho = imported(&dir, "o.tmx", &map_xml("orthogonal", (16, 16), &[(1, 5.0, 6.0)]));
    assert_eq!(ortho.pixel_size(), [64, 48]);
    assert_eq!(world(&ortho, 1), [5.0, 42.0]);
}

const TERRAIN_TILES: u32 = 16;

/// The corner masks of tile `m`: `[tl, tr, br, bl]` set.
fn corners(m: u32) -> [bool; 4] {
    [m & 1 != 0, m & 2 != 0, m & 4 != 0, m & 8 != 0]
}

/// A Tiled 1.0 tileset: terrain 0 ("Grass") with tile `m` (1..15) carrying the corners of mask `m`.
fn old_tileset(name: &str) -> String {
    let tiles: String = (1..TERRAIN_TILES)
        .map(|m| {
            let [tl, tr, br, bl] = corners(m);
            let c = |on: bool| if on { "0" } else { "" };
            format!(
                "<tile id=\"{m}\" terrain=\"{},{},{},{}\"/>",
                c(tl),
                c(tr),
                c(bl),
                c(br)
            )
        })
        .collect();
    format!(
        "<tileset version=\"1.0\" name=\"{name}\" tilewidth=\"16\" tileheight=\"16\" tilecount=\"16\" columns=\"4\">\
         <image source=\"wang.png\" width=\"64\" height=\"64\"/>\
         <terraintypes><terrain name=\"Grass\" tile=\"15\"/></terraintypes>{tiles}</tileset>"
    )
}

/// The same tileset as a Tiled 1.10 corner Wang set.
fn wang_tileset(name: &str) -> String {
    let tiles: String = (1..TERRAIN_TILES)
        .map(|m| {
            let [tl, tr, br, bl] = corners(m);
            let c = |on: bool| if on { "1" } else { "0" };
            // wangid: top, top-right, right, bottom-right, bottom, bottom-left, left, top-left.
            format!(
                "<wangtile tileid=\"{m}\" wangid=\"0,{},0,{},0,{},0,{}\"/>",
                c(tr),
                c(br),
                c(bl),
                c(tl)
            )
        })
        .collect();
    format!(
        "<tileset version=\"1.10\" name=\"{name}\" tilewidth=\"16\" tileheight=\"16\" tilecount=\"16\" columns=\"4\">\
         <image source=\"wang.png\" width=\"64\" height=\"64\"/>\
         <wangsets><wangset name=\"terrains\" type=\"corner\" tile=\"-1\">\
         <wangcolor name=\"Grass\" color=\"#00ff00\" tile=\"-1\" probability=\"1\"/>{tiles}</wangset></wangsets></tileset>"
    )
}

fn paint_block(set: &regia_2d::autotile::AutoTileSet) -> Vec<u32> {
    let mut map = TileMapAsset {
        version: regia_2d::TILEMAP_FORMAT_VERSION,
        orientation: Orientation::Orthogonal,
        width: 5,
        height: 5,
        tile_width: 16,
        tile_height: 16,
        background: None,
        tilesets: vec![TilesetDef {
            name: set.tileset.clone(),
            first_gid: 1,
            image: "wang.png".into(),
            image_width: 64,
            image_height: 64,
            tile_width: 16,
            tile_height: 16,
            columns: 4,
            tile_count: 16,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        }],
        layers: vec![MapLayer::Tiles(TileLayer {
            name: "g".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0; 25],
            properties: Default::default(),
        })],
        properties: Default::default(),
    };
    let cells: Vec<(u32, u32)> = (1..4).flat_map(|y| (1..4).map(move |x| (x, y))).collect();
    set.bind(&map)
        .expect("binds")
        .paint(&mut map, 0, TerrainBrush::Terrain(0), &cells)
        .expect("paints");
    match &map.layers[0] {
        MapLayer::Tiles(t) => t.data.clone(),
        MapLayer::Objects(_) => Vec::new(),
    }
}

#[test]
fn old_terraintypes_import_the_same_tiles_as_the_equivalent_wang_set() {
    let dir = unique_temp_dir("terraintypes");
    let old = import_wang_autotiles(&write(&dir, "old.tsx", &old_tileset("t"))).expect("imports");
    let new = import_wang_autotiles(&write(&dir, "new.tsx", &wang_tileset("t"))).expect("imports");
    assert_eq!((old.len(), new.len()), (1, 1));
    let (old, new) = (&old[0].set, &new[0].set);
    assert_eq!(old.terrains.len(), 1);
    assert_eq!(old.terrains[0].name, "Grass");
    assert!(matches!(old.terrains[0].layout, Layout::WangCorner(_)));
    assert_eq!(old.terrains, new.terrains, "the same terrain, layout and members");
    // And the same tiles on a painted block (corner masks 4 12 8 / 6 15 9 / 2 3 1).
    let (a, b) = (paint_block(old), paint_block(new));
    assert_eq!(a, b);
    let block: Vec<u32> = [4u32, 12, 8, 6, 15, 9, 2, 3, 1].iter().map(|m| m + 1).collect();
    let got: Vec<u32> = (1..4)
        .flat_map(|y| (1..4).map(move |x| (x, y)))
        .map(|(x, y)| a[y * 5 + x])
        .collect();
    assert_eq!(got, block, "tile ids follow the masks of the old terrain table");
}

#[test]
fn terraintypes_inside_a_tmx_and_an_external_tsx_import_too() {
    let dir = unique_temp_dir("terraintypes-tmx");
    write(&dir, "ext.tsx", &old_tileset("ext"));
    let embedded = old_tileset("emb").replace("<tileset ", "<tileset firstgid=\"1\" ");
    let tmx = format!(
        "<?xml version=\"1.0\"?><map version=\"1.0\" orientation=\"orthogonal\" width=\"1\" height=\"1\" tilewidth=\"16\" tileheight=\"16\">\
         <tileset firstgid=\"1\" source=\"ext.tsx\"/>{}\
         <layer id=\"1\" name=\"g\" width=\"1\" height=\"1\"><data encoding=\"csv\">1</data></layer></map>",
        embedded.replace("firstgid=\"1\"", "firstgid=\"17\"")
    );
    let sets = import_wang_autotiles(&write(&dir, "m.tmx", &tmx)).expect("imports");
    let names: Vec<&str> = sets.iter().map(|s| s.set.tileset.as_str()).collect();
    assert_eq!(names, ["ext", "emb"]);
    for imported in &sets {
        assert!(matches!(imported.set.terrains[0].layout, Layout::WangCorner(_)));
    }
}

#[test]
fn a_tileset_without_terraintypes_or_with_garbage_ones_is_not_an_autotile() {
    let dir = unique_temp_dir("terraintypes-none");
    let plain = "<tileset name=\"p\" tilewidth=\"16\" tileheight=\"16\" tilecount=\"16\" columns=\"4\">\
                 <image source=\"w.png\" width=\"64\" height=\"64\"/></tileset>";
    assert!(
        import_wang_autotiles(&write(&dir, "p.tsx", plain))
            .expect("imports")
            .is_empty()
    );
    let junk = plain.replace(
        "</tileset>",
        "<terraintypes><terrain name=\"A\" tile=\"x\"/></terraintypes><tile id=\"2\" terrain=\"1,2\"/><tile id=\"1\" terrain=\"9,,,\"/></tileset>",
    );
    // A terrain with no usable tiles gives nothing, and never panics.
    let sets = import_wang_autotiles(&write(&dir, "j.tsx", &junk)).expect("imports");
    assert!(sets.is_empty(), "{sets:?}");
}
