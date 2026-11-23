// SPDX-License-Identifier: MPL-2.0
//! The UI composites after tonemapping, in display space (lane ui-look): a real scene frame is
//! rendered through `FrameRenderer::render_into` (the path window frames take) at two exposures,
//! the UI overlay is drawn on the same target afterwards (as `render_windows` does), and every
//! pixel the UI covers opaquely must be bit-identical across exposures — never exposed, bloomed
//! or tonemapped — while the scene behind it visibly changes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget, LabMaterial,
    LabVertex, RenderDevice,
};
use regia_ui::atlas::PAGE_SIZE;
use regia_ui::draw::{TextureRef, UiQuad};
use regia_ui::text::{Text, TextSpan, TextStyle, TextSystem};
use regia_ui::{Localization, OffscreenTarget, UiColor, UiRenderer};

const W: u32 = 256;
const H: u32 = 160;

static GROUND: std::sync::LazyLock<Vec<LabVertex>> = std::sync::LazyLock::new(|| {
    let corners = [
        [-6.0, 0.0, -6.0],
        [6.0, 0.0, -6.0],
        [6.0, 0.0, 6.0],
        [-6.0, 0.0, 6.0],
    ];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    corners
        .into_iter()
        .zip(uvs)
        .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], uv))
        .collect()
});
static MATERIAL: LabMaterial = LabMaterial {
    base_color: [0.75, 0.6, 0.45, 1.0],
    roughness: 0.6,
    ..LabMaterial::EMPTY
};

fn scene(exposure_ev: f32) -> FrameScene<'static> {
    FrameScene {
        width: W,
        height: H,
        eye: [0.0, 4.0, 7.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.35, -0.85, 0.4], [1.0, 1.0, 1.0], 30_000.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &GROUND,
            indices: &[0, 2, 1, 0, 3, 2],
            material: &MATERIAL,
            model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

/// HUD-like quads: an opaque panel with bright text, a translucent strip, a white bar.
fn hud(renderer: &mut UiRenderer, sys: &mut TextSystem) -> Vec<UiQuad> {
    let mut panel = UiQuad::fill([16.0, 16.0], [150.0, 56.0], UiColor::from_hex("#0c1117").unwrap());
    panel.corner_radius = [6.0; 4];
    let mut quads = vec![
        panel,
        UiQuad::fill(
            [16.0, 100.0],
            [220.0, 40.0],
            UiColor::from_hex("#0c1117").unwrap().with_alpha(0.6),
        ),
        UiQuad::fill([24.0, 56.0], [120.0, 6.0], UiColor::WHITE),
    ];
    let text = Text {
        spans: vec![TextSpan::lit("12:47 SUPPLY").with_style(TextStyle {
            font_px: 22.0,
            color: UiColor::WHITE,
            bold: true,
            ..Default::default()
        })],
        ..Default::default()
    };
    let shaped = sys.shape(&text, 1.0, None, &Localization::default());
    for g in &shaped.glyphs {
        let key = g.cache_key;
        let Some((place, offset)) = renderer.atlas_mut().get_or_insert(key, || sys.glyph_image(key)) else {
            continue;
        };
        let (gx, gy) = (
            (24 + g.pen[0] + offset[0]) as f32,
            (20 + g.pen[1] + offset[1]) as f32,
        );
        quads.push(UiQuad {
            uv: [
                place.rect[0] as f32 / PAGE_SIZE as f32,
                place.rect[1] as f32 / PAGE_SIZE as f32,
                (place.rect[0] + place.rect[2]) as f32 / PAGE_SIZE as f32,
                (place.rect[1] + place.rect[3]) as f32 / PAGE_SIZE as f32,
            ],
            texture: TextureRef::MonoAtlas(place.page),
            ..UiQuad::fill(
                [gx, gy],
                [place.rect[2] as f32, place.rect[3] as f32],
                UiColor::WHITE,
            )
        });
    }
    quads
}

fn frame_with_ui(device: &RenderDevice, frame: &mut FrameRenderer, exposure_ev: f32) -> Vec<u8> {
    let target = OffscreenTarget::new(device, W, H);
    let scene = scene(exposure_ev);
    frame
        .render_into(
            device,
            &scene,
            FrameTarget::full(target.view(), OffscreenTarget::FORMAT, W, H),
        )
        .expect("scene render");
    let mut renderer = UiRenderer::default();
    let mut sys = TextSystem::embedded_only();
    let quads = hud(&mut renderer, &mut sys);
    renderer.set_quads(quads);
    target.draw_ui(device, &mut renderer);
    target.read(device).expect("readback")
}

#[test]
fn ui_pixels_ignore_scene_exposure() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut frame = FrameRenderer::new();
    let bright = frame_with_ui(&device, &mut frame, 13.0);
    let dark = frame_with_ui(&device, &mut frame, 16.0);
    let at = |p: &[u8], x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    };
    // The scene (outside every UI quad) changes with exposure.
    let ui = |x: u32, y: u32| {
        (16..166).contains(&x) && (16..72).contains(&y) || (16..236).contains(&x) && (100..140).contains(&y)
    };
    let mut scene_change = 0;
    for y in 0..H {
        for x in 0..W {
            if !ui(x, y) {
                let (b, d) = (at(&bright, x, y), at(&dark, x, y));
                scene_change = scene_change.max((i32::from(b[1]) - i32::from(d[1])).abs());
            }
        }
    }
    assert!(
        scene_change > 20,
        "exposure must visibly change the scene (max Δ {scene_change}); test setup is wrong"
    );
    if let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") {
        std::fs::create_dir_all(&dir).expect("evidence dir");
        for (tag, p) in [("ev13", &bright), ("ev16", &dark)] {
            let img = image::RgbaImage::from_raw(W, H, p.clone()).expect("pixels");
            img.save(std::path::Path::new(&dir).join(format!("exposure_{tag}.png")))
                .expect("save");
        }
    }
    // Every pixel inside the opaque panel (text, bar, panel fill) is identical.
    let mut diffs = 0;
    for y in 22..66 {
        for x in 22..160 {
            if at(&bright, x, y) != at(&dark, x, y) {
                diffs += 1;
            }
        }
    }
    assert_eq!(diffs, 0, "{diffs} UI pixels changed with scene exposure");
    // The opaque panel shows its authored colour exactly (#0c1117), not an exposed one.
    assert_eq!(at(&bright, 150, 66), [0x0c, 0x11, 0x17, 255]);
    // Bright UI white is display white, not tonemapped down.
    assert_eq!(at(&bright, 80, 58), [255, 255, 255, 255]);
}
