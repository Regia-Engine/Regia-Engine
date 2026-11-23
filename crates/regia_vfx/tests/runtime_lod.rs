// SPDX-License-Identifier: MPL-2.0
//! LOD on the runtime path follows the camera the frame was rendered with.
//!
//! A host that renders through `regia_render`'s frame hook without extracting ECS cameras
//! (a game shell with its own camera code) never wrote `VfxView`, so the LOD measured every
//! effect from the default view near the world origin: smoke 80 m from the real camera but
//! 850 m from the origin dropped to rate 0 and never drew.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameQuality, FrameScene, RenderDevice, RuntimeRenderer,
};
use regia_vfx::{VfxPlugin, VfxRuntimePlugin, VfxScene};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

#[test]
fn smoke_80m_from_the_rendered_camera_is_scheduled() {
    let device = device();
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app.insert_resource(device.clone());
    app.init_resource::<RuntimeRenderer>();
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(regia_vfx::showcase::smoke()).expect("smoke");
    app.insert_resource(VfxScene::new(lib));
    app.add_plugins((VfxPlugin, VfxRuntimePlugin));
    let smoke_at = Vec3::new(600.0, 0.0, 600.0);
    app.world_mut()
        .resource_mut::<VfxScene>()
        .spawn("smoke", smoke_at, Vec3::Z, 7)
        .expect("spawn smoke");
    let eye = smoke_at + Vec3::new(0.0, 2.0, 80.0);
    for _ in 0..40 {
        app.update();
        // The host renders its own camera through the frame hook (no ECS camera).
        let frame = FrameScene {
            width: 64,
            height: 36,
            eye: eye.to_array(),
            target: (smoke_at + Vec3::Y * 4.0).to_array(),
            fov_y: 0.9,
            near: 0.1,
            far: 2000.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 15.0,
            ambient_lux: 20_000.0,
            double_sided: false,
            post: None,
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 100_000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes: Vec::new(),
        };
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .render_offscreen(&device, &frame)
            .expect("frame");
    }
    let live = app.world().resource::<VfxScene>().estimated_live();
    assert!(
        live >= 20,
        "smoke 80 m from the rendered camera was LOD-culled ({live} live particles)"
    );
}
