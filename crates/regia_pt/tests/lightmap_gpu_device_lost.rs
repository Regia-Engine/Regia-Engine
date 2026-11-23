// SPDX-License-Identifier: MPL-2.0
//! A GPU lightmap bake whose device is lost mid-bake ends with `lightmap_gpu_device_lost` and no
//! map, instead of hanging, panicking or returning wrong light (ADR-2125). Its own test binary:
//! the lost flag (`regia_render::gpu_health::device_lost`) is process-wide and stays set. Runs on
//! lavapipe; the loss is a `Device::destroy`, which wgpu reports through the same callback as a
//! driver reset.

#![cfg(feature = "lightmap-gpu")]

use regia_pt::lightmap::{
    LightmapBakeSettings, LightmapGpuControl, LightmapGpuOptions, LightmapScene, MIN_GPU_TILE_RAYS,
    MeshLightmapRequest, bake_lightmap_gpu_with, lightmap_gpu_error_code, mesh_layout, quad_box_scene,
};
use regia_render::RenderDevice;

#[test]
fn a_device_lost_mid_bake_ends_the_bake_with_its_code() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    // What `regia bake-lightmaps --gpu` installs on its device.
    device.device.set_device_lost_callback(|reason, message| {
        regia_render::gpu_health::note_device_lost(reason, &message);
    });
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let settings = LightmapBakeSettings {
        samples: 32,
        threads: 1,
        ..LightmapBakeSettings::default()
    };
    let options = LightmapGpuOptions {
        tile_rays: MIN_GPU_TILE_RAYS,
        ..LightmapGpuOptions::default()
    };
    assert!(!regia_render::gpu_health::device_lost());
    let mut tiles = 0u64;
    let result = bake_lightmap_gpu_with(
        &device,
        &scene,
        mesh_layout(&scene, &request).expect("layout"),
        &[qb.floor],
        &settings,
        &options,
        |p| {
            tiles = p.tiles_done;
            if p.tiles_done == 2 {
                device.device.destroy();
            }
            LightmapGpuControl::Continue
        },
    );
    let e = result.map(|_| ()).expect_err("the device was lost");
    assert_eq!(lightmap_gpu_error_code(&e), "lightmap_gpu_device_lost", "{e}");
    assert!(regia_render::gpu_health::device_lost());
    assert!(tiles >= 2, "{tiles}");
    // A second bake on the lost device refuses at once.
    let again = bake_lightmap_gpu_with(
        &device,
        &scene,
        mesh_layout(&scene, &request).expect("layout"),
        &[qb.floor],
        &settings,
        &options,
        |_| LightmapGpuControl::Continue,
    );
    let e = again.map(|_| ()).expect_err("still lost");
    assert_eq!(lightmap_gpu_error_code(&e), "lightmap_gpu_device_lost", "{e}");
}
