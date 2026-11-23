// SPDX-License-Identifier: MPL-2.0
//! Splat weight packs (ADR-1402, `TerrainGpu::set_splat_weights`): valid packs install, removing them works, and
//! packs that disagree in size or hold the wrong number of bytes are refused with an error (no panic, no GPU
//! validation error). The rendered effect is measured end to end by the showcase sample's
//! `splat_packs_paint_each_half_of_the_map_with_its_own_layer` (lavapipe).

use regia_terrain::gpu::TerrainGpu;
use regia_terrain::{CdlodConfig, LayerDesc, Rgba8Image, TerrainLayers, TerrainMap};

fn pack(side: u32, texel: [u8; 4]) -> Rgba8Image {
    Rgba8Image::solid(side, side, texel)
}

#[test]
fn splat_packs_install_remove_and_refuse_bad_sizes() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip splat weight test: {err}");
            return;
        }
    };
    let map = TerrainMap::from_heights(65, 65, vec![5.0; 65 * 65], 64.0, 8.0, 0.0).expect("flat map");
    let layers = TerrainLayers {
        layers: vec![
            LayerDesc {
                name: "a".into(),
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "b".into(),
                ..LayerDesc::ground()
            },
        ],
        ..Default::default()
    };
    let config = CdlodConfig::default();
    let mut gpu = TerrainGpu::new(&device, &map, &layers, &config, 4, None, None).expect("gpu");

    let a = pack(32, [255, 0, 0, 0]);
    let b = pack(32, [0, 0, 0, 0]);
    gpu.set_splat_weights(Some([&a, &b]))
        .expect("matching packs install");
    // Non-square packs of the same size are fine.
    let wide0 = Rgba8Image::solid(64, 16, [0, 255, 0, 0]);
    let wide1 = Rgba8Image::solid(64, 16, [0, 0, 0, 0]);
    gpu.set_splat_weights(Some([&wide0, &wide1]))
        .expect("non-square packs install");

    let small = pack(16, [0, 0, 0, 0]);
    assert!(
        gpu.set_splat_weights(Some([&a, &small])).is_err(),
        "packs of two sizes are refused"
    );
    let short = Rgba8Image {
        width: 32,
        height: 32,
        data: vec![0; 10],
    };
    assert!(
        gpu.set_splat_weights(Some([&a, &short])).is_err(),
        "a pack whose bytes do not match its size is refused"
    );
    let empty = Rgba8Image {
        width: 0,
        height: 0,
        data: Vec::new(),
    };
    assert!(
        gpu.set_splat_weights(Some([&empty, &empty])).is_err(),
        "an empty pack is refused"
    );

    gpu.set_splat_weights(None).expect("packs are removed");
}
