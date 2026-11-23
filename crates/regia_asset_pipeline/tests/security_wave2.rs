// SPDX-License-Identifier: MPL-2.0
//! Threat-model findings 19, 20 and 25 (ported from the threat-model review's reproducers, ADR-3206)
//! and the path helpers routed through the shared rule (`regia_core::project_path`, ADR-4922). Each
//! finding test failed before its fix.

use std::path::Path;

use regia_test_utils::unique_temp_dir;

/// A glTF with one triangle whose buffer lies at `uri`.
fn gltf_with_buffer(uri: &str, len: usize) -> String {
    format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
        "nodes":[{{"name":"sim","mesh":0}}],
        "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
        "accessors":[
          {{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
          {{"bufferView":1,"componentType":5123,"count":3,"type":"SCALAR"}}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":6}}],
        "buffers":[{{"uri":"{uri}","byteLength":{len}}}]}}"#
    )
}

/// 3 positions then 3 u16 indices: bytes that decode as a valid triangle.
fn triangle_bytes() -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    for v in [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for c in v {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    for i in [0_u16, 1, 2] {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    bytes
}

fn cloth_source(mesh: &str) -> String {
    format!(r#"(format_version: 1, name: "c", sim_mesh: (source: "{mesh}", node: "sim"))"#)
}

/// Finding 19: the cloth cook hands a project's glTF to `gltf::import_buffers` with no URI check and
/// joins `sim_mesh.source` unconfined, where every other import refuses a URI that leaves the
/// project. A `.cloth.ron` can pull any file of the cook host into cooked output as positions.
#[test]
fn finding_19_the_cloth_cook_refuses_a_buffer_outside_the_project() {
    let base = unique_temp_dir("threat-cloth");
    let project = base.join("project");
    std::fs::create_dir_all(&project).expect("project");
    let secret = triangle_bytes();
    std::fs::write(base.join("outside.bin"), &secret).expect("outside file");
    std::fs::write(
        project.join("sim.gltf"),
        gltf_with_buffer("../outside.bin", secret.len()),
    )
    .expect("gltf");
    let cloth = project.join("a.cloth.ron");
    std::fs::write(&cloth, cloth_source("sim.gltf")).expect("cloth");
    let result = regia_asset_pipeline::cloth_cook::cook_cloth(&cloth);
    let message = match &result {
        Ok(_) => String::new(),
        Err(e) => e.to_string(),
    };
    // Whatever else a real cook needs, the buffer outside the project must not be what stopped or
    // fed it: only a refusal that names the URI counts.
    assert!(
        message.to_ascii_lowercase().contains("uri") || message.contains("outside"),
        "the buffer outside the project was read: {message:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Finding 19, the mesh reference itself: `sim_mesh.source` may not climb out of the folder the
/// cloth source is cooked in (no project around it) nor reach out through a symlink.
#[test]
fn finding_19_the_cloth_mesh_reference_stays_inside() {
    let base = unique_temp_dir("threat-cloth-ref");
    let project = base.join("project");
    let outside = base.join("outside");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&outside).expect("outside");
    let bytes = triangle_bytes();
    // The outside glTF carries its buffer inline (a `data:` URI), so the URI check has nothing to
    // refuse: only the mesh reference's own bound can keep it out.
    let inline = {
        use base64::Engine as _;
        format!(
            "data:application/octet-stream;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        )
    };
    std::fs::write(outside.join("sim.gltf"), gltf_with_buffer(&inline, bytes.len())).expect("gltf");
    let cloth = project.join("a.cloth.ron");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, project.join("link")).expect("link");
    // The symlink case exists on Unix only; Windows checks the relative escape alone.
    let refs: Vec<String> = std::iter::once("../outside/sim.gltf".to_string())
        .chain(cfg!(unix).then(|| "link/sim.gltf".to_string()))
        .collect();
    for r in refs {
        std::fs::write(&cloth, cloth_source(&r)).expect("cloth");
        let err = regia_asset_pipeline::cloth_cook::cook_cloth(&cloth)
            .err()
            .unwrap_or_else(|| panic!("{r}: the mesh outside was cooked"));
        assert_eq!(err.code(), "cloth_cook_gltf", "{r}: {err}");
        assert!(err.to_string().contains("leaves"), "{r}: {err}");
    }
    // The same glTF inside the folder still cooks as far as the cloth rules (a lone triangle is a
    // valid sim mesh or a cloth-rule error, never a path refusal).
    std::fs::write(project.join("sim.bin"), &bytes).expect("bin");
    std::fs::write(project.join("sim.gltf"), gltf_with_buffer("sim.bin", bytes.len())).expect("gltf");
    std::fs::write(&cloth, cloth_source("sim.gltf")).expect("cloth");
    if let Err(e) = regia_asset_pipeline::cloth_cook::cook_cloth(&cloth) {
        assert!(!e.to_string().contains("leaves"), "{e}");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Finding 20: `.tmx` has no cell cap (the `.tmj` reader has one), and the capacity is an unchecked
/// `u32` product: this map panics on overflow in a debug build and wraps in a release one.
#[test]
fn finding_20_a_tmx_claiming_too_many_tiles_is_refused() {
    let dir = unique_temp_dir("threat-tmx");
    let map = dir.join("big.tmx");
    std::fs::write(
        &map,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<map version="1.9" orientation="orthogonal" renderorder="right-down" width="65536" height="65536" tilewidth="8" tileheight="8" infinite="0">
 <layer id="1" name="ground" width="65536" height="65536"><data encoding="csv">0</data></layer>
</map>"#,
    )
    .expect("map");
    let result = std::panic::catch_unwind(|| regia_asset_pipeline::tiled_import::import_tiled(&map).is_err());
    assert_eq!(
        result.ok(),
        Some(true),
        "the map must be an error, not a panic or a success"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding 20, the edge of the cap: a map at the cap is not refused for its size, one tile over is.
#[test]
fn finding_20_the_tmx_cap_is_the_tmj_cap() {
    use regia_asset_pipeline::tiled_import::{MAX_TILED_CELLS, import_tiled};
    let dir = unique_temp_dir("threat-tmx-edge");
    let side = 1u32 << 13;
    assert_eq!(side as usize * side as usize, MAX_TILED_CELLS);
    for (w, h, over) in [(side, side, false), (side, side + 1, true)] {
        let map = dir.join(format!("m{w}x{h}.tmx"));
        std::fs::write(
            &map,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<map version="1.9" orientation="orthogonal" renderorder="right-down" width="{w}" height="{h}" tilewidth="8" tileheight="8" infinite="0">
</map>"#
            ),
        )
        .expect("map");
        let result = import_tiled(&map);
        let refused = result
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string().contains("tiles per layer"));
        assert_eq!(refused, over, "{w}x{h}: {:?}", result.err());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Finding 25: a `command` generator inherits the whole environment of whoever runs
/// `regia gen run|verify` (a CI step holds its secrets there); a `[[tool]]` starts with an empty one.
#[cfg(unix)]
#[test]
fn finding_25_a_generator_does_not_inherit_the_callers_secrets() {
    let project = unique_temp_dir("threat-gen");
    let gens = project.join("generators");
    std::fs::create_dir_all(&gens).expect("generators");
    std::fs::write(project.join("Regia.toml"), "").expect("manifest");
    std::fs::write(
        gens.join("leak.toml"),
        r#"format_version = 1
[generator]
kind = "command"
seed = 1
command = ['/bin/sh', '-c', 'mkdir -p "$REGIA_GEN_OUT/assets/gen/leak"; env > "$REGIA_GEN_OUT/assets/gen/leak/env.txt"']
outputs = ["assets/gen/leak/"]
"#,
    )
    .expect("generator");
    regia_asset_pipeline::generators::run(&project, "leak").expect("the generator runs");
    let seen = std::fs::read_to_string(project.join("assets/gen/leak/env.txt")).expect("its output");
    assert!(
        seen.contains("REGIA_GEN_SEED="),
        "the generator's own variables: {seen}"
    );
    // `cargo test` sets this for the test process: a stand-in for a CI step's secret.
    assert!(
        !seen.contains("CARGO_MANIFEST_DIR="),
        "the generator saw the caller's environment"
    );
    // What it does get: the allow-list, so tools on `PATH` still start.
    for name in seen.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)) {
        assert!(
            regia_asset_pipeline::generators::GENERATOR_ENV_ALLOW
                .iter()
                .any(|a| a.eq_ignore_ascii_case(name))
                || name.starts_with("REGIA_")
                // Set by the shell itself, not inherited.
                || ["PWD", "SHLVL", "_", "OLDPWD"].contains(&name),
            "{name} reached the generator"
        );
    }
    if std::env::var_os("PATH").is_some() {
        assert!(seen.lines().any(|l| l.starts_with("PATH=")), "PATH: {seen}");
    }
    let _ = std::fs::remove_dir_all(&project);
}

fn heightfield_generator(output: &str) -> String {
    format!(
        "format_version = 1\n[generator]\nkind = \"heightfield\"\nseed = 7\noutputs = [\"{output}\"]\n\
         [generator.heightfield]\nwidth = 9\nheight = 9\noctaves = 2\n"
    )
}

/// The generator's paths take the shared rule: names Windows reads as a device, a drive or a
/// stream are refused on every host.
#[test]
fn generator_outputs_take_the_shared_path_rule() {
    let project = unique_temp_dir("threat-gen-paths");
    let gens = project.join("generators");
    std::fs::create_dir_all(&gens).expect("generators");
    std::fs::write(project.join("Regia.toml"), "").expect("manifest");
    // The control.
    std::fs::write(gens.join("ok.toml"), heightfield_generator("assets/gen/ok.r16")).expect("ok");
    regia_asset_pipeline::generators::run(&project, "ok").expect("the control runs");
    assert!(project.join("assets/gen/ok.r16").is_file());
    for bad in [
        "assets/nul.r16",
        "assets/con/x.r16",
        "C:x/out.r16",
        "assets/a:b.r16",
        "assets/.../x.r16",
    ] {
        std::fs::write(gens.join("bad.toml"), heightfield_generator(bad)).expect("generator");
        let err = regia_asset_pipeline::generators::run(&project, "bad")
            .err()
            .unwrap_or_else(|| panic!("{bad}: accepted"));
        assert_eq!(err.code(), "gen_invalid", "{bad}: {err}");
        assert!(!project.join(bad).exists(), "{bad}: written");
    }
    let _ = std::fs::remove_dir_all(&project);
}

/// A project with an `.obj` in `models/` and a secret beside the project.
fn obj_project(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let top = unique_temp_dir(tag);
    let project = top.join("project");
    let models = project.join("models");
    std::fs::create_dir_all(&models).expect("models");
    std::fs::write(project.join("Regia.toml"), "").expect("manifest");
    std::fs::write(top.join("secret.mtl"), "newmtl secret\nKd 1 0 0\n").expect("secret");
    (top, models)
}

const OBJ_TRIANGLE: &str = "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";

/// The warnings of converting an OBJ in `models` whose `mtllib` names `mtllib`: a library the
/// importer refused or did not find is a `mtllib … not found` warning.
fn mtllib_warnings(models: &Path, mtllib: &str) -> Vec<String> {
    let obj = format!("mtllib {mtllib}\nusemtl secret\n{OBJ_TRIANGLE}");
    let settings = regia_asset_pipeline::obj_import::ObjImportSettings::default();
    let conversion =
        regia_asset_pipeline::obj_import::convert_obj_to_glb(obj.as_bytes(), Some(models), &settings)
            .expect("converts");
    conversion
        .report
        .warnings
        .into_iter()
        .filter(|w| w.contains("mtllib"))
        .collect()
}

/// The OBJ importer's `mtllib` reference: no `..`, no device name, no symlink out of the project.
#[test]
fn obj_mtllib_stays_inside_the_project() {
    let (top, models) = obj_project("threat-obj");
    // The control: a material file beside the OBJ is read.
    std::fs::write(models.join("ok.mtl"), "newmtl secret\nKd 1 0 0\n").expect("ok");
    let warnings = mtllib_warnings(&models, "ok.mtl");
    assert!(
        warnings.is_empty(),
        "the control library was not read: {warnings:?}"
    );
    let mut refused = vec!["../../secret.mtl".to_string()];
    // On Linux these are plain names that exist beside the OBJ; Windows opens a device, a drive or
    // a stream instead. The rule refuses them on every host.
    for planted in ["nul.mtl", "con.mtl", "x.mtl:alt", "aux"] {
        let _ = std::fs::write(models.join(planted), "newmtl secret\nKd 1 0 0\n");
        refused.push(planted.to_string());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(top.join("secret.mtl"), models.join("link.mtl")).expect("link");
        refused.push("link.mtl".to_string());
    }
    for name in refused {
        assert!(
            !mtllib_warnings(&models, &name).is_empty(),
            "{name}: the library was read"
        );
    }
    // Outside any project there is no project bound, but the reference itself still may not
    // climb: only the lookup's own rule refuses `..` there.
    let loose = top.join("loose");
    std::fs::create_dir_all(loose.join("models")).expect("loose");
    std::fs::write(loose.join("secret.mtl"), "newmtl secret\nKd 1 0 0\n").expect("loose secret");
    assert!(
        !mtllib_warnings(&loose.join("models"), "../secret.mtl").is_empty(),
        "outside a project the library above the OBJ was read"
    );
    let _ = std::fs::remove_dir_all(&top);
}

const TSX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tileset version="1.10" name="secret" tilewidth="8" tileheight="8" tilecount="1" columns="1">
 <image source="t.png" width="8" height="8"/>
</tileset>"#;

fn tmx_with_tileset(source: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<map version="1.10" orientation="orthogonal" renderorder="right-down" width="1" height="1" tilewidth="8" tileheight="8" infinite="0">
 <tileset firstgid="1" source="{source}"/>
 <layer id="1" name="g" width="1" height="1"><data encoding="csv">1</data></layer>
</map>"#
    )
}

/// The `.tmx` importer's external tilesets (read by the `tiled` crate through the importer's
/// reader): no absolute path, no `..` out of the project, no symlink out of it.
#[test]
fn tmx_external_tilesets_stay_inside_the_project() {
    use regia_asset_pipeline::tiled_import::import_tiled;
    let top = unique_temp_dir("threat-tmx-ts");
    let project = top.join("project");
    let maps = project.join("maps");
    std::fs::create_dir_all(&maps).expect("maps");
    std::fs::write(project.join("Regia.toml"), "").expect("manifest");
    std::fs::write(top.join("outside.tsx"), TSX).expect("outside");
    std::fs::write(maps.join("own.tsx"), TSX).expect("own");
    std::fs::create_dir_all(project.join("sets")).expect("sets");
    std::fs::write(project.join("sets/shared.tsx"), TSX).expect("shared");
    let map = maps.join("a.tmx");
    // The controls: beside the map, and a sibling folder of the project.
    for ok in ["own.tsx", "../sets/shared.tsx"] {
        std::fs::write(&map, tmx_with_tileset(ok)).expect("map");
        let imported = import_tiled(&map).unwrap_or_else(|e| panic!("{ok}: {e}"));
        assert_eq!(imported.map.tilesets.len(), 1, "{ok}");
    }
    let absolute = top.join("outside.tsx").display().to_string();
    #[cfg(unix)]
    std::os::unix::fs::symlink(top.join("outside.tsx"), maps.join("link.tsx")).expect("link");
    let refused: Vec<String> = ["../../outside.tsx".to_string(), absolute]
        .into_iter()
        .chain(cfg!(unix).then(|| "link.tsx".to_string()))
        .collect();
    for source in refused {
        std::fs::write(&map, tmx_with_tileset(&source)).expect("map");
        let result = import_tiled(&map);
        assert!(result.is_err(), "{source}: the tileset outside was read");
    }
    let _ = std::fs::remove_dir_all(&top);
}

/// Every importer that resolves a reference against its file's folder (`join_portable`) refuses the
/// Windows-only names on every host (here: a tilemap's tileset image).
#[test]
fn tilemap_images_take_the_shared_rule() {
    use regia_2d::tilemap::{Orientation, TILEMAP_FORMAT_VERSION, TileMapAsset, TilesetDef};
    use regia_asset_pipeline::tiled_import::cook_tilemap;
    let dir = unique_temp_dir("threat-tilemap-image");
    std::fs::write(dir.join("Regia.toml"), "").expect("manifest");
    for image in ["nul.png", "a/con.png", "t.png:alt", "a/.../t.png"] {
        // On Linux each is a plain file name: plant it so only the rule can refuse it.
        let planted = dir.join(image);
        if let Some(parent) = planted.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&planted, b"not an image");
        let map = TileMapAsset {
            version: TILEMAP_FORMAT_VERSION,
            orientation: Orientation::Orthogonal,
            width: 1,
            height: 1,
            tile_width: 8,
            tile_height: 8,
            background: None,
            tilesets: vec![TilesetDef {
                name: "s".into(),
                first_gid: 1,
                image: image.into(),
                image_width: 8,
                image_height: 8,
                tile_width: 8,
                tile_height: 8,
                columns: 1,
                tile_count: 1,
                spacing: 0,
                margin: 0,
                tiles: Vec::new(),
            }],
            layers: Vec::new(),
            properties: Default::default(),
        };
        let err = cook_tilemap(map, &dir)
            .err()
            .unwrap_or_else(|| panic!("{image}: accepted"))
            .to_string();
        // Refused by the rule, not by the image decoder reading the planted file.
        assert!(err.contains("path_"), "{image}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn tmx_layer(w: u32, h: u32, data: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<map version="1.10" orientation="orthogonal" renderorder="right-down" width="{w}" height="{h}" tilewidth="8" tileheight="8" infinite="0">
 <layer id="1" name="g" width="{w}" height="{h}">
  {data}
 </layer>
</map>"#
    )
}

/// Finding 20, the layer data: the `tiled` crate indexes a layer's tiles by its declared size, so a
/// layer holding fewer tiles than `width * height` panicked the importer (in a release build too);
/// and it inflates zlib and gzip data with no limit. Both are errors now, checked before the crate
/// decodes the file.
#[test]
fn finding_20_tmx_layer_data_must_match_the_layer_size() {
    use base64::Engine as _;
    use regia_asset_pipeline::tiled_import::import_tiled;
    use std::io::Write as _;
    let dir = unique_temp_dir("threat-tmx-data");
    let map = dir.join("m.tmx");
    let zlib = |bytes: &[u8]| {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(bytes).expect("compress");
        base64::engine::general_purpose::STANDARD.encode(enc.finish().expect("compress"))
    };
    // The control: 2 x 2, four tiles, as CSV and as zlib.
    let ok_csv = r#"<data encoding="csv">0,0,
0,0</data>"#
        .to_string();
    let ok_zlib = format!(
        r#"<data encoding="base64" compression="zlib">{}</data>"#,
        zlib(&[0u8; 16])
    );
    for data in [ok_csv, ok_zlib] {
        std::fs::write(&map, tmx_layer(2, 2, &data)).expect("map");
        import_tiled(&map).unwrap_or_else(|e| panic!("{data}: {e}"));
    }
    let short_csv = r#"<data encoding="csv">0</data>"#.to_string();
    let short_raw = format!(
        r#"<data encoding="base64">{}</data>"#,
        base64::engine::general_purpose::STANDARD.encode([0u8; 4])
    );
    // 64 MiB of zeros compress to about 64 KiB: a bomb against a 2 x 2 layer.
    let bomb = format!(
        r#"<data encoding="base64" compression="zlib">{}</data>"#,
        zlib(&vec![0u8; 64 << 20])
    );
    for data in [short_csv, short_raw, bomb] {
        std::fs::write(&map, tmx_layer(2, 2, &data)).expect("map");
        let shown: String = data.chars().take(60).collect();
        let result = std::panic::catch_unwind(|| import_tiled(&map).map_err(|e| e.to_string()));
        match result {
            Ok(Err(e)) => assert!(e.contains("layer `g`"), "{shown}: {e}"),
            Ok(Ok(_)) => panic!("{shown}: accepted"),
            Err(_) => panic!("{shown}: panicked"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A 16-bit grayscale PNG of `width x height`.
fn height_png(width: u32, height: u32) -> Vec<u8> {
    use image::ImageEncoder as _;
    let samples = vec![0u8; (width * height * 2) as usize];
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&samples, width, height, image::ExtendedColorType::L16)
        .expect("png");
    out
}

/// Finding 33, the cook side: a plain 16-bit PNG cooks to a single `Heightfield`, which the loader
/// refuses past `HEIGHTFIELD_MAX_EDGE`, so the cook refuses it too (a larger terrain cooks through
/// the tiled terrain path); one at the cap cooks.
#[test]
fn finding_33_the_heightfield_cook_refuses_what_the_loader_would() {
    let edge = regia_asset::HEIGHTFIELD_MAX_EDGE;
    let root = unique_temp_dir("threat-heightfield-cook");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join("Regia.toml"), "").expect("manifest");
    std::fs::write(root.join("assets/at.png"), height_png(edge, 2)).expect("at");
    std::fs::write(root.join("assets/over.png"), height_png(edge + 1, 2)).expect("over");
    let report =
        regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(root.clone()))
            .expect("the cook runs");
    let failed: Vec<&str> = report.errors.iter().map(|(path, _)| path.as_str()).collect();
    assert!(
        failed.iter().any(|p| p.ends_with("over.png")),
        "{:?}",
        report.errors
    );
    assert!(
        !failed.iter().any(|p| p.ends_with("at.png")),
        "{:?}",
        report.errors
    );
    let _ = std::fs::remove_dir_all(&root);
}
