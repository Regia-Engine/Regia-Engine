// SPDX-License-Identifier: MPL-2.0
//! `FrameRenderer::host_checklist_report` flags exactly what a host changed from the
//! look-dev harness's frame (the DO27 shell diffs its frame against it).

use regia_render::{
    DirectionalLightUniform, FrameRenderer, LabScene, OutdoorParams, RenderSettings, SharedFrameInfo,
    SharedPass, SharedPassDraw, SharedUpload,
};
use regia_scene::level::{AtmospherePreset, LevelSettings, SkyBody};

struct NoDraw;
impl SharedPassDraw for NoDraw {
    fn prepare(&mut self, _info: &SharedFrameInfo, _upload: &mut SharedUpload<'_>) {}
    fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: SharedPass) {}
}

fn harness_like() -> (LabScene, regia_render::FrameQuality) {
    let settings = RenderSettings::default();
    let mut post = settings.resolve_post();
    post.auto_exposure = true;
    let lab = LabScene {
        width: 64,
        height: 36,
        eye: [0.0, 2.0, 5.0],
        target: [0.0; 3],
        fov_y: 1.0,
        near: 0.1,
        far: 12_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.3], [1.0; 3], 45_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    (lab, settings.quality())
}

/// The harness's outdoor state: the level's sun strength on the planet (`apply_level`).
fn harness_outdoor(renderer: &mut FrameRenderer) {
    let mut outdoor = OutdoorParams::default();
    outdoor.planet.sun_illuminance = 45_000.0;
    renderer.set_outdoor(outdoor);
}

#[test]
fn the_harness_frame_has_no_differences_and_a_shell_frame_shows_its_own() {
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(NoDraw)));
    harness_outdoor(&mut renderer);
    let (lab, quality) = harness_like();
    let report = renderer.host_checklist_report(&lab.as_frame_scene_with_quality(quality));
    assert!(report.differences().is_empty(), "{report}");

    // A shell on MSAA with a pre-exposed sun and its own forward fog on top.
    let (mut lab, mut quality) = harness_like();
    quality.msaa_samples = 4;
    lab.directional = DirectionalLightUniform::new([0.0, -1.0, 0.3], [1.0; 3], 3.0);
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.fog.enabled = true;
    let report = renderer.host_checklist_report(&frame);
    let diffs: Vec<&str> = report.differences().iter().map(|c| c.item).collect();
    assert_eq!(
        diffs,
        vec![
            "quality.msaa_samples",
            "scene.fog.enabled",
            "directional.illuminance (lux)",
            "MSAA with the shared terrain draw / planet sky",
            "outdoor.planet.sun_illuminance = directional.illuminance (±5 %)",
        ],
        "{report}"
    );
    // MSAA with terrain is the one frame-breaking difference, and prints first.
    let errors: Vec<&str> = report.errors().iter().map(|c| c.item).collect();
    assert_eq!(
        errors,
        vec!["MSAA with the shared terrain draw / planet sky"],
        "{report}"
    );
    assert!(
        report
            .to_string()
            .lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("  ERROR")),
        "{report}"
    );
}

#[test]
fn an_airless_map_without_its_sky_body_and_a_foreign_exposure_band_are_flagged() {
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(NoDraw)));
    let mut level = LevelSettings::default();
    level.atmosphere.preset = AtmospherePreset::Airless;
    level.atmosphere.sun_illuminance_lux = Some(45_000.0);
    level.look.min_ev100 = Some(11.9);
    level.look.max_ev100 = Some(12.1);
    let mut outdoor = OutdoorParams::default();
    outdoor.apply_level(&level.atmosphere, false);
    renderer.set_outdoor(outdoor.clone());
    let (lab, quality) = harness_like();
    let report = renderer.host_checklist_report_for_level(&lab.as_frame_scene_with_quality(quality), &level);
    let diffs: Vec<&str> = report.differences().iter().map(|c| c.item).collect();
    assert_eq!(
        diffs,
        vec![
            "outdoor.sky_body on an airless planet (planet-shine fill)",
            "post exposure band = level.look (min, max EV100)",
        ],
        "{report}"
    );
    assert!(report.errors().is_empty(), "{report}");

    // The shell sets Jupiter with its fill and applies the level look: clean.
    level.atmosphere.sky_body = Some(SkyBody {
        fill_lux: Some(1600.0),
        ..SkyBody::jupiter_from_europa()
    });
    outdoor.apply_level(&level.atmosphere, false);
    renderer.set_outdoor(outdoor);
    let (mut lab, quality) = harness_like();
    if let Some(post) = lab.post.as_mut() {
        regia_render::apply_level_look(post, &level.look);
    }
    let report = renderer.host_checklist_report_for_level(&lab.as_frame_scene_with_quality(quality), &level);
    assert!(report.differences().is_empty(), "{report}");
}
