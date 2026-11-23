// SPDX-License-Identifier: MIT-0
//! Which runtime-path goldens the dense-scene default would move (ADR-3149).
//!
//! The default-look goldens are the goldens that render through `RenderSettings` (the shipped
//! Desktop High row), so they are the ones a preset default could reach; the lab goldens render
//! at the pinned `FrameQuality::LAB`, which has the path off whatever the settings say. Each shot
//! draws twice on fresh renderers that can build clusters for every mesh: with the switch off and
//! with it on. With it on, the gate must keep the frame on the classic path (the shots are far
//! under the measured thresholds) and the frame must equal the switch-off frame byte for byte, so
//! the golden cannot move. No golden image is read or written.
//!
//! Beside the assertion it prints, per shot, what the path would change if it drew every frame
//! (`select` always): the list `book/agents/visbuffer.md` gives the owner for that choice.

use std::sync::Arc;

use lookdev::default_look::{LookOptions, LookShot, render_shot_with};
use regia_render::visbuffer::{MeshletBuilder, MeshletMesh};
use regia_render::{
    FrameRenderer, LabVertex, RenderDevice, RenderProfilePreset, RenderSettings, VisibilityDefault,
    VisibilityDefaultState, VisibilitySelect,
};

/// One cluster level for every mesh: the path can take any mesh the gate hands it.
fn builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

/// (pixels that differ, largest channel difference).
fn diff(a: &[u8], b: &[u8]) -> (usize, u8) {
    let mut count = 0;
    let mut worst = 0u8;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        count += usize::from(d > 0);
        worst = worst.max(d);
    }
    (count, worst)
}

#[test]
fn the_dense_default_moves_no_default_look_golden() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    let options = LookOptions::golden();
    // The goldens' row (`RenderSettings::default` on a desktop).
    let off = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    let mut on = off.clone();
    assert_eq!(
        on.set_visibility_default(VisibilityDefault::Dense),
        VisibilityDefaultState::Applied
    );
    let mut always = on.clone();
    always.profile.visibility_path.select = VisibilitySelect::Always;
    let mut failures = Vec::new();
    for shot in LookShot::ALL {
        let draw = |settings: &RenderSettings| {
            let mut renderer = FrameRenderer::new();
            renderer.set_meshlet_builder(Some(builder()));
            let pixels = render_shot_with(&device, &mut renderer, shot, options, settings, &|_| {})
                .expect("render shot");
            (pixels, renderer)
        };
        let (classic, _) = draw(&off);
        let (dense, renderer) = draw(&on);
        let report = renderer.visbuffer_dense_report();
        let (differ, worst) = diff(&classic, &dense);
        println!(
            "{:>16}: {} triangles, {} meshes; dense {}, active {}; {differ} pixels differ (worst {worst})",
            shot.name(),
            report.density.triangles,
            report.density.instances,
            report.dense,
            renderer.visbuffer_active(),
        );
        if report.density.instances == 0 {
            failures.push(format!("{}: the gate counted no mesh", shot.name()));
        }
        if report.dense || renderer.visbuffer_active() || report.entered != 0 {
            failures.push(format!("{}: the default took the path: {report:?}", shot.name()));
        }
        if differ != 0 {
            failures.push(format!(
                "{}: {differ} pixels moved with the default on (worst {worst})",
                shot.name()
            ));
        }
        // What `select: always` would do to the golden (printed, not asserted: that is a choice
        // for the owner, not a property of the default).
        let (every_frame, renderer) = draw(&always);
        let (differ, worst) = diff(&classic, &every_frame);
        println!(
            "{:>16}: with the path on every frame: {} instances on the path, {differ} pixels differ (worst {worst}); \
             golden would move: {}",
            shot.name(),
            renderer.visbuffer_stats().instances,
            differ != 0,
        );
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
