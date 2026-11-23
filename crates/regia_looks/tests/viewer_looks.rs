// SPDX-License-Identifier: MPL-2.0
//! The looks on the product viewer's renderer: every looks hook compiles for WebGL2 inside the
//! viewer's prelude and entry points, and on a device (lavapipe) the board renders bit-stable with
//! no pipeline created by a material swap.
#![cfg(feature = "viewer")]

use glam::Affine3A;
use regia_looks::{CutType, GemCut, Library, Studio, viewer};
use regia_viewer::{ShaderFamily, ViewerCamera, ViewerConfig, ViewerRenderer, capture_still};

#[test]
fn every_looks_hook_compiles_for_webgl2_in_the_viewer() {
    let hooks = viewer::hooks();
    for family in ShaderFamily::ALL {
        let source = hooks.family_source(family);
        let module = naga::front::wgsl::parse_str(&source)
            .unwrap_or_else(|e| panic!("{family:?}: {}", e.emit_to_string(&source)));
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{family:?}: {e:?}"));
        for (entry, stage) in [
            ("vs_main", naga::ShaderStage::Vertex),
            ("fs_main", naga::ShaderStage::Fragment),
        ] {
            let options = naga::back::glsl::Options {
                version: naga::back::glsl::Version::Embedded {
                    version: 300,
                    is_webgl: true,
                },
                ..Default::default()
            };
            let pipeline = naga::back::glsl::PipelineOptions {
                shader_stage: stage,
                entry_point: entry.into(),
                multiview: None,
            };
            let mut out = String::new();
            let mut writer = naga::back::glsl::Writer::new(
                &mut out,
                &module,
                &info,
                &options,
                &pipeline,
                naga::proc::BoundsCheckPolicies::default(),
            )
            .unwrap_or_else(|e| panic!("{family:?}/{entry}: {e:?}"));
            writer
                .write()
                .unwrap_or_else(|e| panic!("{family:?}/{entry}: {e:?}"));
            assert!(out.contains("#version 300 es"));
        }
    }
}

fn renderer() -> Option<ViewerRenderer> {
    match regia_viewer::gpu::headless_device() {
        Ok((_, device, queue)) => {
            let mut config = ViewerConfig::new(wgpu::TextureFormat::Rgba8UnormSrgb);
            config.hooks = viewer::hooks();
            let mut r = ViewerRenderer::new(device, queue, config).expect("renderer");
            r.set_environment(&viewer::studio_env(&Studio::default()).expect("studio"));
            Some(r)
        }
        Err(e) => {
            assert!(
                std::env::var_os("REGIA_LOOKS_REQUIRE_GPU").is_none(),
                "no GPU: {e}"
            );
            eprintln!("skipped: {e}");
            None
        }
    }
}

#[test]
fn gem_swaps_on_the_viewer_create_no_pipeline_and_stills_are_stable() {
    let Some(mut r) = renderer() else { return };
    let lib = Library::builtin().expect("library");
    let cut = GemCut::new(CutType::RoundBrilliant, [6.0, 6.0]);
    let mesh = r.upload_mesh(&viewer::mesh_data(&cut.mesh())).expect("mesh");
    let first = viewer::create_material(
        &mut r,
        &lib.look("diamond-white", None).expect("d"),
        cut.stone_size(),
    )
    .expect("m");
    let tilt = Affine3A::from_rotation_x(-0.3);
    let draw = r.add_draw(mesh, first, &[tilt]).expect("draw");
    r.set_camera(&ViewerCamera::front(40.0, 100.0, 24.0))
        .expect("camera");
    let a = capture_still(&mut r, 96, 96).expect("still a");
    let b = capture_still(&mut r, 96, 96).expect("still b");
    assert_eq!(a.hash(), b.hash(), "the same view renders the same bytes");
    let pipelines = r.stats().pipelines_created;
    for id in [
        "sapphire-blue",
        "ruby-red",
        "cz-frosted",
        "titanium-anodized-blue",
        "opal-white",
    ] {
        let m = viewer::create_material(
            &mut r,
            &lib.look(id, Some("finish-polish"))
                .or_else(|_| lib.look(id, None))
                .expect("look"),
            cut.stone_size(),
        )
        .expect("material");
        r.set_draw_material(draw, m).expect("swap");
        capture_still(&mut r, 96, 96).expect("still");
    }
    assert_eq!(r.stats().pipelines_created, pipelines, "swaps write blocks only");
    // A colored stone differs from the colorless one.
    let sapphire = capture_still(&mut r, 96, 96).expect("still");
    assert_ne!(sapphire.hash(), a.hash());
}

/// The corrections of ADR-2587 hold on the renderer the web player and the Android viewer use, not only
/// on the standalone one: blasted steel is a dark gray, yellow gold is warm, and the step family gives a
/// princess table a pattern instead of one flat pane.
#[test]
fn the_look_corrections_hold_on_the_viewer() {
    let Some(mut r) = looks_renderer_with_keys() else {
        return;
    };
    let lib = Library::builtin().expect("library");
    let ball = regia_looks::LookMesh::ball(6.0, 64, 32);
    let mesh = r.upload_mesh(&viewer::mesh_data(&ball)).expect("mesh");
    let steel = viewer::create_material(
        &mut r,
        &lib.look("steel", Some("finish-sandblast")).expect("steel"),
        regia_looks::StoneSize::default(),
    )
    .expect("material");
    let draw = r.add_draw(mesh, steel, &[Affine3A::IDENTITY]).expect("draw");
    r.set_camera(&ViewerCamera::front(40.0, 100.0, 19.3))
        .expect("camera");
    let px = pixels(&capture_still(&mut r, 256, 256).expect("still"));
    // The ball's own pixels: inside its silhouette in this framing.
    let on_ball: Vec<[f32; 3]> = (0..256usize * 256)
        .filter(|i| {
            let (x, y) = ((i % 256) as f32 - 127.5, (i / 256) as f32 - 127.5);
            x * x + y * y < 90.0 * 90.0
        })
        .map(|i| px[i])
        .collect();
    let mut lumas: Vec<f32> = on_ball.iter().map(|c| luma(*c)).collect();
    lumas.sort_by(f32::total_cmp);
    let median = lumas[lumas.len() / 2];
    eprintln!("METRIC viewer steel median {median:.3}");
    assert!(
        (0.3..0.62).contains(&median),
        "blasted steel on the viewer: median {median:.3}"
    );

    let gold = viewer::create_material(
        &mut r,
        &lib.look("gold-yellow-18k", Some("finish-polish")).expect("gold"),
        regia_looks::StoneSize::default(),
    )
    .expect("material");
    r.set_draw_material(draw, gold).expect("swap");
    let px = pixels(&capture_still(&mut r, 256, 256).expect("still"));
    let mids: Vec<[f32; 3]> = px
        .iter()
        .copied()
        .filter(|c| (0.3..0.8).contains(&luma(*c)) && chroma(*c) > 0.05)
        .collect();
    let mean = |k: usize| mids.iter().map(|c| c[k]).sum::<f32>() / mids.len().max(1) as f32;
    let (m_r, m_g, m_b) = (mean(0), mean(1), mean(2));
    eprintln!("METRIC viewer gold {m_r:.3} {m_g:.3} {m_b:.3}");
    assert!(
        m_r > m_g && m_g > m_b && (m_r - m_b) / m_r > 0.4,
        "gold on the viewer: {m_r:.3} {m_g:.3} {m_b:.3}"
    );
}

/// The pale quartz on the viewer: a yellow center, not a dark one or an olive one.
#[test]
fn a_yellow_round_on_the_viewer_is_light_in_the_center() {
    let Some(mut r) = looks_renderer_with_keys() else {
        return;
    };
    let lib = Library::builtin().expect("library");
    let cut = GemCut::new(CutType::RoundBrilliant, [6.0, 6.0]);
    let mesh = r.upload_mesh(&viewer::mesh_data(&cut.mesh())).expect("mesh");
    let m = viewer::create_material(
        &mut r,
        &lib.look("citrine", None).expect("stone"),
        cut.stone_size(),
    )
    .expect("material");
    r.add_draw(mesh, m, &[Affine3A::from_rotation_x(-22f32.to_radians())])
        .expect("draw");
    r.set_camera(&ViewerCamera::front(40.0, 100.0, 19.3))
        .expect("camera");
    let px = pixels(&capture_still(&mut r, 256, 256).expect("still"));
    let (lo, hi) = (100usize, 156usize);
    let center: Vec<[f32; 3]> = (lo..hi)
        .flat_map(|y| (lo..hi).map(move |x| y * 256 + x))
        .map(|i| px[i])
        .collect();
    let n = center.len() as f32;
    let mean = |k: usize| center.iter().map(|c| c[k]).sum::<f32>() / n;
    let (m_r, m_g, m_b) = (mean(0), mean(1), mean(2));
    eprintln!("METRIC viewer citrine center {m_r:.3} {m_g:.3} {m_b:.3}");
    assert!(m_r > 0.6, "dark center: {m_r:.3} {m_g:.3} {m_b:.3}");
    assert!(
        m_r > m_g && m_g > m_b && m_r - m_b > 0.25,
        "still yellow: {m_r:.3} {m_g:.3} {m_b:.3}"
    );
}

/// A princess table on the viewer, across the tilts where a softbox lies in it.
#[test]
fn a_princess_table_on_the_viewer_keeps_its_pattern() {
    let Some(mut r) = looks_renderer_with_keys() else {
        return;
    };
    let lib = Library::builtin().expect("library");
    let cut = GemCut::new(CutType::Princess, [6.0, 6.0]);
    let mesh = r.upload_mesh(&viewer::mesh_data(&cut.mesh())).expect("mesh");
    let m = viewer::create_material(
        &mut r,
        &lib.look("emerald-green", None).expect("stone"),
        cut.stone_size(),
    )
    .expect("material");
    let draw = r.add_draw(mesh, m, &[Affine3A::IDENTITY]).expect("draw");
    r.set_camera(&ViewerCamera::front(40.0, 100.0, 19.3))
        .expect("camera");
    let (lo, hi) = (82usize, 174usize);
    for tilt in [8.0f32, 12.0, 16.0, 17.0, 22.0] {
        r.set_draw_transforms(draw, &[Affine3A::from_rotation_x(-tilt.to_radians())])
            .expect("turn");
        let px = pixels(&capture_still(&mut r, 256, 256).expect("still"));
        let (mut edges, mut pairs) = (0u32, 0u32);
        for y in lo..hi {
            for x in lo..hi {
                let i = y * 256 + x;
                for j in [i + 1, i + 256] {
                    pairs += 1;
                    if (luma(px[i]) - luma(px[j])).abs() > 0.03 {
                        edges += 1;
                    }
                }
            }
        }
        let density = edges as f32 / pairs as f32;
        eprintln!("METRIC viewer princess tilt {tilt}: edges {density:.4}");
        assert!(
            density > 0.02,
            "tilt {tilt}: the table is one flat pane: {density:.4}"
        );
    }
}

fn looks_renderer_with_keys() -> Option<ViewerRenderer> {
    match regia_viewer::gpu::headless_device() {
        Ok((_, device, queue)) => Some(
            viewer::looks_renderer(
                device,
                queue,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                4,
                &Studio::default(),
            )
            .expect("renderer"),
        ),
        Err(e) => {
            assert!(
                std::env::var_os("REGIA_LOOKS_REQUIRE_GPU").is_none(),
                "no GPU: {e}"
            );
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn pixels(still: &regia_viewer::Still) -> Vec<[f32; 3]> {
    still
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        })
        .collect()
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn chroma(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}
