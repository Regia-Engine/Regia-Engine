// SPDX-License-Identifier: MPL-2.0
//! `regia cook` writes a tiled heightfield and terrain index. The runtime
//! mounts that pak and draws the stitched map through the shared frame.

use std::path::PathBuf;
use std::process::Command;

use regia_asset::{MetaSidecar, cooked_cache_dir, meta_path_for};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    SharedPassDraw,
};
use regia_terrain::{CdlodConfig, PakTileSource, TerrainLayers, TerrainSharedDraw, TileKey, TileSource};

// Same locator as `lookdev::workspace_bin`. This test cannot depend on the sample (lookdev
// already depends on regia_terrain), and `CARGO_BIN_EXE_regia` is set only inside
// `regia_cli`. `CARGO_MANIFEST_DIR` inside the included file is this crate's, which still
// walks up to the workspace.
#[path = "../../../samples/lookdev/src/workspace_bin.rs"]
mod workspace_bin;

/// The `regia` CLI for this test's cargo target directory and profile.
///
/// `CARGO_BIN_EXE_regia` exists only inside `regia_cli`. The binary is
/// `<target>/<profile>/regia` next to this test (`deps/`). When it is absent it is built
/// once with `cargo build -p regia_cli --bin regia` into that directory.
///
/// # Panics
///
/// Panics when that build fails. The message names the missing binary and the build command.
fn regia_bin() -> PathBuf {
    match workspace_bin::ensure_regia_cli() {
        Ok(path) => path,
        Err(message) => panic!("{message}"),
    }
}

fn gray16_png(samples: &[u16], width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Sixteen);
    let mut writer = encoder.write_header().expect("png header");
    let mut be = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        be.extend_from_slice(&sample.to_be_bytes());
    }
    writer.write_image_data(&be).expect("png data");
    writer.finish().expect("png finish");
    bytes
}

fn rgba_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    let pixels = rgba.repeat((width * height) as usize);
    writer.write_image_data(&pixels).expect("png data");
    writer.finish().expect("png finish");
    bytes
}

fn pixel(pixels: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

#[test]
fn cooked_pak_heightfield_renders_in_the_frame() {
    let bin = regia_bin();
    assert!(
        bin.is_file(),
        "build the cook CLI first: cargo build -p regia_cli --bin regia (missing {})",
        bin.display()
    );
    let root = std::env::temp_dir().join(format!("regia-terrain-cook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let assets = root.join("assets");
    std::fs::create_dir_all(assets.join("detail/ground")).expect("assets");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"terrain-cook\"\nid = \"terrain-cook\"\nversion = \"0.1.0\"\nengine = \"0.1\"\n",
    )
    .expect("toml");
    let layers_src = include_str!("fixtures/ground.terrain.layers.ron");
    let layers_path = assets.join("ground.terrain.layers.ron");
    std::fs::write(&layers_path, layers_src).expect("layers");
    let width = 64u32;
    let height = 64u32;
    let samples: Vec<u16> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x * 3 + y) as u16))
        .collect();
    std::fs::write(
        assets.join("heightfield.png"),
        gray16_png(&samples, width, height),
    )
    .expect("hf");
    std::fs::write(
        assets.join("heightfield.json"),
        r#"{ "size_m": 32.0, "origin": [0.0, 0.0, 0.0] }"#,
    )
    .expect("extents");
    std::fs::write(
        assets.join("detail/ground/albedo.png"),
        // Tiled cooks require a square power of two that the 256 px tile divides.
        rgba_png(256, 256, [40, 90, 30, 255]),
    )
    .expect("albedo");

    let output = Command::new(&bin)
        .args([
            "cook",
            "--project",
            &root.to_string_lossy(),
            "--platform",
            "linux",
            "--quality",
            "fast",
        ])
        .output()
        .expect("spawn regia cook");
    assert!(
        output.status.success(),
        "regia cook failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let meta = MetaSidecar::from_ron(&std::fs::read(meta_path_for(&layers_path)).expect("meta bytes"))
        .expect("meta");
    let pak_dir = root.join("build/paks/linux");
    let cache = cooked_cache_dir(&root, "linux");
    let source = PakTileSource::new(meta.id, Some(&pak_dir), cache);
    let layers = TerrainLayers::from_ron(layers_src).expect("layers ron");
    let device = RenderDevice::headless().expect("gpu");
    let (draw, index) = TerrainSharedDraw::from_cooked(
        &device,
        &source,
        &layers,
        32.0,
        1.0,
        0.0,
        CdlodConfig::default(),
        3,
    )
    .expect("from cooked pak");
    assert_eq!(index.map_id, meta.id);
    assert_eq!(index.world_size_m, Some(32.0));
    assert_eq!(
        index.heightfield.as_ref().map(|h| (h.width, h.height)),
        Some((width, height))
    );
    assert_eq!(draw.cooked_samples(), Some(samples.as_slice()));
    assert!(
        !index.slots.is_empty(),
        "the layer albedo must cook as a tiled texture"
    );
    let slot = index.texture(0).expect("slot");
    let tile = source
        .load(
            &slot,
            TileKey {
                texture: 0,
                mip: 0,
                x: 0,
                y: 0,
            },
        )
        .expect("cooked texture tile");
    assert!(tile.len() > 32, "tile payload is empty");

    let mut lab = LabScene {
        width: 96,
        height: 54,
        eye: [0.0, 3.0, 6.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 80.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.9, -0.15], [1.0, 0.98, 0.92], 110_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    lab.post = Some(post.clone());
    let mut quality = FrameQuality::LAB;
    quality.taau = true;
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    frame.ibl = true;
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let image = renderer.render_offscreen(&device, &frame).expect("cooked frame");
    let mut greens = 0u32;
    for y in 0..image.height {
        for x in 0..image.width {
            let p = pixel(&image.pixels, image.width, x, y);
            let (r, g) = (u16::from(p[0]), u16::from(p[1]));
            if g > r + 8 && g > 30 {
                greens += 1;
            }
        }
    }
    assert!(greens > 20, "cooked terrain did not draw ({greens} green pixels)");
    let _ = std::fs::remove_dir_all(&root);
}
