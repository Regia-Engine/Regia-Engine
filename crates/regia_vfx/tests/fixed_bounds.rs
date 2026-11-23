// SPDX-License-Identifier: MPL-2.0
//! Authored fixed bounds (`.vfx.ron` v8 fold, P-225, ADR-8105 row (h)): the culling volume of an
//! instance when set. CPU only, fixed steps, no clock.

use regia_core::math::Vec3;
use regia_vfx::VfxCamera;
use regia_vfx::VfxScene;
use regia_vfx::format::{FixedBoundsDef, VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::scene::VfxLibrary;
use regia_vfx::significance::{SignificanceCaps, SignificanceState, SignificanceView};
use regia_vfx::sim::LodInput;

const DT: f32 = 1.0 / 64.0;
const LOD: LodInput = LodInput {
    camera_pos: Vec3::ZERO,
};

/// A small looping puff emitter (computed bound well under 2 m), with `bounds` as authored.
fn puffs(name: &str, bounds: &str) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "{name}",
    system: (budget: 4000{bounds}),
    emitters: [
        (
            name: "puffs",
            spawn: (mode: Rate(64.0), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.2, max: 0.4)),
            render: (mode: Billboard),
        ),
    ],
)"#
    ))
    .expect("fixture parses")
}

/// Acceptance (P-225 (1)): an effect with fixed bounds is culled by them, not by its computed
/// bound. The camera sits at the origin looking down -Z with off-screen pausing on.
/// - Behind the camera, an instance whose fixed box reaches 20-30 m in front of it stays in view
///   (its computed sphere at the origin would be paused).
/// - In front of the camera, an instance whose fixed box sits 100 m to the side is paused (its
///   computed sphere would be in view).
#[test]
fn an_effect_with_fixed_bounds_culls_by_them() {
    let mut library = VfxLibrary::new();
    for def in [
        puffs("plain", ""),
        puffs(
            "ahead",
            ", bounds: Some((min: (-1.0, -1.0, -60.0), max: (1.0, 1.0, -50.0)))",
        ),
        puffs(
            "aside",
            ", bounds: Some((min: (100.0, -1.0, -1.0), max: (101.0, 1.0, 1.0)))",
        ),
    ] {
        library.add(def).expect("valid fixture");
    }
    assert_eq!(
        library.def(library.index("ahead").expect("ahead")).system.bounds,
        Some(FixedBoundsDef {
            min: [-1.0, -1.0, -60.0],
            max: [1.0, 1.0, -50.0],
        })
    );
    let mut scene = VfxScene::new(library);
    scene.significance_mut().set_caps(SignificanceCaps {
        pause_offscreen: true,
        ..SignificanceCaps::default()
    });
    scene
        .significance_mut()
        .set_view(Some(SignificanceView::from_camera(
            &VfxCamera::looking_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -10.0)),
            1280,
            720,
        )));
    let behind = Vec3::new(0.0, 0.0, 30.0);
    let front = Vec3::new(0.0, 0.0, -10.0);
    let plain_behind = scene.spawn("plain", behind, Vec3::Z, 1).expect("spawn");
    let plain_front = scene.spawn("plain", front, Vec3::Z, 2).expect("spawn");
    let ahead = scene.spawn("ahead", behind, Vec3::Z, 3).expect("spawn");
    let aside = scene.spawn("aside", front, Vec3::Z, 4).expect("spawn");
    for frame in 0..8 {
        scene.update(DT, &LOD, 100_000);
        let state = |id| scene.significance().state_of(id);
        // The controls: the computed sphere decides.
        assert_eq!(
            state(plain_behind),
            Some(SignificanceState::Paused),
            "frame {frame}"
        );
        assert_eq!(state(plain_front), Some(SignificanceState::Full), "frame {frame}");
        // The fixed boxes decide.
        assert_eq!(
            state(ahead),
            Some(SignificanceState::Full),
            "frame {frame}: fixed box ahead"
        );
        assert_eq!(
            state(aside),
            Some(SignificanceState::Paused),
            "frame {frame}: fixed box aside"
        );
    }
    assert!(scene.instance(ahead).expect("live").live_particles() > 0);
    assert_eq!(scene.instance(aside).expect("live").live_particles(), 0);
}

/// The loader refuses a fixed box that is not finite or has `min > max` on an axis; `None` is not
/// written back (a v8 file without bounds re-saves byte for byte the same shape).
#[test]
fn fixed_bounds_are_validated_and_omitted_when_unset() {
    let bad = [
        ", bounds: Some((min: (1.0, 0.0, 0.0), max: (0.0, 1.0, 1.0)))",
        ", bounds: Some((min: (0.0, 0.0, 0.0), max: (inf, 1.0, 1.0)))",
        ", bounds: Some((min: (NaN, 0.0, 0.0), max: (1.0, 1.0, 1.0)))",
    ];
    for b in bad {
        let text = format!(
            r#"(version: {VFX_FORMAT_VERSION}, name: "x", system: (budget: 8{b}), emitters: [(name: "e",
            spawn: (mode: Burst(1), lifetime: (min: 1.0, max: 1.0)), init: (size: (min: 0.1, max: 0.1)), render: (mode: Billboard))])"#
        );
        let err = VfxSystemDef::from_ron(&text).expect_err(b);
        assert!(err.to_string().contains("bounds"), "{b}: {err}");
    }
    let plain = puffs("plain", "");
    assert!(!plain.to_ron().expect("ron").contains("bounds"));
    let mut fixed = plain.clone();
    fixed.system.bounds = Some(FixedBoundsDef {
        min: [-1.0, 0.0, -1.0],
        max: [1.0, 3.5, 1.0],
    });
    let text = fixed.to_ron().expect("ron");
    assert!(text.contains("bounds"), "{text}");
    assert_eq!(VfxSystemDef::from_ron(&text).expect("round trip"), fixed);
}
