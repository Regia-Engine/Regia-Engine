// SPDX-License-Identifier: MPL-2.0
//! CPU-ordered emitters on mobile (`specs/26` § 3, ADR-7210) on a headless adapter (lavapipe in
//! the gate): a profile without the alpha sort draws the alpha list emitter by emitter, back to
//! front for the current view, so of two overlapping opaque-ish alpha emitters the nearer one is
//! on top from either side and in either spawn order. No golden image: the test reads which
//! colour wins at the overlap.

use std::sync::OnceLock;

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxCamera, VfxProfile};

const W: u32 = 96;
const H: u32 = 96;
const DT: f32 = 1.0 / 60.0;
/// Height of the cards and the eye: above the backdrop's ground plane (y = 0).
const Y: f32 = 2.5;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

/// Mobile High without the soft layer (the test reads the full-resolution composite).
fn mobile() -> VfxProfile {
    let p = VfxProfile {
        particle_capacity: 1024,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        ..VfxProfile::mobile_high()
    };
    assert!(!p.sort, "mobile has no alpha sort");
    p
}

/// One still, opaque alpha card of `color`, 3 m across.
fn card(name: &str, color: &str) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r##"(
    version: {VFX_FORMAT_VERSION},
    name: "{name}",
    emitters: [(
        name: "card",
        spawn: (mode: Burst(1), lifetime: (min: 30.0, max: 30.0)),
        init: (size: (min: 3.0, max: 3.0), color: [(t: 0.0, color: "{color}")]),
        render: (mode: Billboard, sprite: HardDisc, blend: Alpha),
    )],
)"##
    ))
    .expect("test card parses")
}

/// Plays red at `z = -2` and blue at `z = +2` (spawned in `order`) seen from `eye` on the Z
/// axis; returns the centre pixel.
fn centre_pixel(eye_z: f32, red_first: bool) -> [u8; 4] {
    let mut library = VfxLibrary::new();
    library.add(card("red", "#ff0000")).expect("red");
    library.add(card("blue", "#0000ff")).expect("blue");
    let mut scene = VfxScene::new(library);
    let spawns = [("red", Vec3::new(0.0, Y, -2.0)), ("blue", Vec3::new(0.0, Y, 2.0))];
    let ordered: Vec<_> = if red_first {
        spawns.to_vec()
    } else {
        spawns.iter().rev().copied().collect()
    };
    for (name, at) in ordered {
        scene.spawn(name, at, Vec3::Y, 1).expect("spawned");
    }
    let camera = VfxCamera::looking_at(Vec3::new(0.0, Y, eye_z), Vec3::new(0.0, Y, 0.0));
    let mut harness = VfxHarness::new(device(), mobile()).expect("harness");
    let mut last = None;
    for _ in 0..4 {
        scene.update(
            DT,
            &LodInput {
                camera_pos: camera.eye,
            },
            1024,
        );
        scene.upload(harness.gpu());
        last = Some(harness.render(device(), &camera, W, H, DT).expect("frame"));
    }
    let frame = last.expect("frames ran");
    assert_eq!(frame.counters[1], 2, "two alpha particles");
    let i = ((H / 2 * W + W / 2) * 4) as usize;
    [
        frame.pixels[i],
        frame.pixels[i + 1],
        frame.pixels[i + 2],
        frame.pixels[i + 3],
    ]
}

fn assert_on_top(eye_z: f32, red_first: bool, red_wins: bool) {
    let [r, _, b, _] = centre_pixel(eye_z, red_first);
    let (near, won) = if red_wins { ("red", r > b) } else { ("blue", b > r) };
    assert!(
        won && r.abs_diff(b) > 64,
        "eye z {eye_z}, red spawned first {red_first}: the nearer {near} card must be on top, got r {r} b {b}"
    );
}

#[test]
fn mobile_draws_the_nearer_emitter_on_top_from_the_front() {
    // Eye at z = -10: red (z = -2) is nearer.
    assert_on_top(-10.0, true, true);
    assert_on_top(-10.0, false, true);
}

#[test]
fn mobile_draws_the_nearer_emitter_on_top_from_behind() {
    // Eye at z = +10: blue (z = +2) is nearer.
    assert_on_top(10.0, true, false);
    assert_on_top(10.0, false, false);
}

/// The ranks themselves: the CPU orders emitter slots back to front per view.
#[test]
fn mobile_emitter_ranks_follow_view_depth() {
    let mut library = VfxLibrary::new();
    library.add(card("red", "#ff0000")).expect("red");
    library.add(card("blue", "#0000ff")).expect("blue");
    let mut scene = VfxScene::new(library);
    scene
        .spawn("red", Vec3::new(0.0, Y, -2.0), Vec3::Y, 1)
        .expect("red");
    scene
        .spawn("blue", Vec3::new(0.0, Y, 2.0), Vec3::Y, 1)
        .expect("blue");
    let mut harness = VfxHarness::new(device(), mobile()).expect("harness");
    let mut ranks = |eye_z: f32| {
        let camera = VfxCamera::looking_at(Vec3::new(0.0, Y, eye_z), Vec3::new(0.0, Y, 0.0));
        scene.update(
            DT,
            &LodInput {
                camera_pos: camera.eye,
            },
            1024,
        );
        scene.upload(harness.gpu());
        harness.render(device(), &camera, W, H, DT).expect("frame");
        let gpu = harness.gpu();
        // The scene allocates emitter slots from the top: the first instance's slot is the
        // highest one in use.
        let top = regia_vfx::gpu::layout::MAX_EMITTERS - 1;
        (
            gpu.emitter_rank(top).expect("slot"),
            gpu.emitter_rank(top - 1).expect("slot"),
        )
    };
    let (red, blue) = ranks(-10.0);
    assert!(
        red > blue,
        "from the front red is nearer: drawn later (red {red}, blue {blue})"
    );
    let (red, blue) = ranks(10.0);
    assert!(
        red < blue,
        "from behind blue is nearer: drawn later (red {red}, blue {blue})"
    );
}
