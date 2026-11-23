// SPDX-License-Identifier: MPL-2.0
//! The baked lightmap in the forward pass (ADR-1623): a receiver quad takes its diffuse indirect
//! light from the installed lightmap through its UV2 stream and its entry's `scale_offset`, a
//! second quad that is not a receiver keeps the ambient light, and a frame without an enabled
//! lightmap is byte-identical to one without any. No golden image: every check compares pixels
//! of frames rendered here (lavapipe, deterministic).

use std::sync::Arc;

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice,
};

const W: u32 = 64;
const H: u32 = 64;
/// Object keys of the receiver (left) and the plain quad (right).
const RECEIVER: u64 = 0x0B1E_C7A1;
const PLAIN: u64 = 0x0B1E_C7A2;
/// Atlas texels: 8 × 2. Columns 0-3 belong to another entry (black), columns 4-5 hold `LOW`,
/// 6-7 `HIGH`; the receiver's entry is the right half (`scale_offset` `[0.5, 1, 0.5, 0]`).
const LOW: f32 = 2_000.0;
const HIGH: f32 = 20_000.0;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => {
            eprintln!("lightmap_forward adapter: {:?}", device.adapter.get_info());
            device
        }
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad facing the camera at `x`, UV `u` from 0 (left) to 1 (right).
fn quad(x: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([x - 0.3, -0.3, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, -0.3, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, 0.3, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.3, 0.3, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

/// No sun: the quads see only the flat ambient (or the lightmap).
fn lab(depth_prepass: bool) -> LabScene {
    let (vertices, indices) = quad(-0.5);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 10.0,
        depth_prepass,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

/// The lightmap: one entry `receiver` over the atlas's right half, unwrapped 1:1 from the quad
/// (UV2 = the quad's UV), `low` in its left columns and `high` in its right ones.
fn lightmap(low: f32, high: f32) -> Lightmap {
    let irr: Vec<[f32; 3]> = (0..2)
        .flat_map(|_| 0..8u32)
        .map(|x| match x {
            0..=3 => [0.0; 3],
            4 | 5 => [low; 3],
            _ => [high; 3],
        })
        .collect();
    Lightmap::from_irradiance(
        LightmapLayout {
            width: 8,
            height: 2,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "receiver".into(),
                source_vertices: 4,
                rect: [4, 0, 4, 2],
                scale_offset: [0.5, 1.0, 0.5, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 0.36,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &irr,
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap")
}

fn enabled() -> LightmapSettings {
    LightmapSettings {
        enabled: true,
        ..LightmapSettings::default()
    }
}

/// Luminance-ish sum of the RGB bytes at `(x, y)`.
fn px(img: &[u8], x: u32, y: u32) -> u32 {
    let i = ((y * W + x) * 4) as usize;
    u32::from(img[i]) + u32::from(img[i + 1]) + u32::from(img[i + 2])
}

/// Receiver pixels at entry-local u ≈ 0.25 and ≈ 0.75 (the left quad spans screen x 14-27 and
/// rows 25-38 of the 64 × 64 frame).
const RECEIVER_LEFT: (u32, u32) = (17, 32);
const RECEIVER_RIGHT: (u32, u32) = (24, 32);
/// A pixel of the plain quad (screen x 36-49).
const PLAIN_PX: (u32, u32) = (43, 32);

/// What the frame draws: no lightmap at all, or `map` under `settings` with the receiver table
/// `receivers`.
struct Run<'a> {
    map: Option<Arc<Lightmap>>,
    settings: LightmapSettings,
    receivers: &'a [(u64, u32)],
    depth_prepass: bool,
}

/// Renders the receiver quad (drawn from its entry's unwrap with the UV2 stream) and the plain
/// quad; returns the RGBA8 frame and the renderer's lightmap stats.
fn render(device: &RenderDevice, run: &Run<'_>) -> (Vec<u8>, regia_render::lightmap::LightmapForwardStats) {
    let lab = lab(run.depth_prepass);
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let (left_v, left_i) = quad(-0.5);
    let (right_v, right_i) = quad(0.5);
    let built = receiver_mesh(&lightmap(LOW, HIGH), 0, &left_v, &left_i).expect("the baked mesh");
    let receiver = FrameMesh {
        mesh_key: 0x0A11_00A1 << 3,
        object_key: RECEIVER,
        vertices: &built.vertices,
        indices: &built.indices,
        uv1: &built.uv2,
        ..template
    };
    let plain = FrameMesh {
        mesh_key: 0x0A11_00A2 << 3,
        object_key: PLAIN,
        vertices: &right_v,
        indices: &right_i,
        ..template
    };
    assert_eq!(built.indices, left_i, "a 1:1 unwrap keeps the triangles");
    let scene = FrameScene {
        meshes: vec![receiver, plain],
        ..base.clone()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_lightmap(run.map.clone(), run.settings);
    renderer.set_lightmap_receivers(run.receivers);
    let img = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    assert_eq!(img.len(), (W * H * 4) as usize);
    (img, renderer.lightmap_forward_stats())
}

#[test]
fn a_receiver_takes_its_entry_s_baked_light_and_nothing_else_changes() {
    let device = device();
    let none = Run {
        map: None,
        settings: LightmapSettings::default(),
        receivers: &[],
        depth_prepass: false,
    };
    let (plain_frame, stats) = render(&device, &none);
    assert!(!stats.active && stats.drawn == 0);
    let ambient = px(&plain_frame, RECEIVER_LEFT.0, RECEIVER_LEFT.1);
    assert!(ambient > 30, "the ambient lights the quads: {ambient}");
    assert!(
        px(&plain_frame, RECEIVER_RIGHT.0, RECEIVER_RIGHT.1).abs_diff(ambient) <= 3,
        "flat ambient on a flat quad"
    );

    // Installed but the flag off: the same bytes as no lightmap at all.
    let off = Run {
        map: Some(Arc::new(lightmap(LOW, HIGH))),
        settings: LightmapSettings::default(),
        receivers: &[(RECEIVER, 0)],
        depth_prepass: false,
    };
    let (off_frame, stats) = render(&device, &off);
    assert!(!stats.active && stats.drawn == 0 && stats.receivers == 1);
    assert!(off_frame == plain_frame, "a disabled lightmap changes no pixel");

    // Enabled: the receiver's left half reads LOW, its right half HIGH (through the entry's
    // scale_offset: without it the left half would read the black columns of the other entry).
    let on = Run {
        settings: enabled(),
        ..off
    };
    let (lit, stats) = render(&device, &on);
    assert!(stats.active, "{stats:?}");
    assert_eq!(
        (stats.drawn, stats.fallback, stats.unsupported),
        (1, 0, 0),
        "{stats:?}"
    );
    let (left, right) = (
        px(&lit, RECEIVER_LEFT.0, RECEIVER_LEFT.1),
        px(&lit, RECEIVER_RIGHT.0, RECEIVER_RIGHT.1),
    );
    assert!(
        left > 15,
        "LOW lights the left half (scale_offset applied): {left}"
    );
    assert!(
        left < ambient,
        "LOW (2 klx) is under the 5 klx ambient: {left} vs {ambient}"
    );
    assert!(
        right > ambient,
        "HIGH (20 klx) is over the 5 klx ambient: {right} vs {ambient}"
    );
    assert!(left < right, "UV2 reaches the lookup: {left} < {right}");
    // The plain quad is not a receiver: its pixels are the no-lightmap frame's.
    for y in 20..44 {
        for x in 34..W {
            assert_eq!(
                px(&lit, x, y),
                px(&plain_frame, x, y),
                "plain quad pixel ({x}, {y})"
            );
        }
    }

    // A black lightmap removes the ambient from the receiver only.
    let black = Run {
        map: Some(Arc::new(lightmap(0.0, 0.0))),
        ..on
    };
    let (dark, _) = render(&device, &black);
    assert!(px(&dark, RECEIVER_LEFT.0, RECEIVER_LEFT.1) <= 3);
    assert!(px(&dark, RECEIVER_RIGHT.0, RECEIVER_RIGHT.1) <= 3);
    assert_eq!(
        px(&dark, PLAIN_PX.0, PLAIN_PX.1),
        px(&plain_frame, PLAIN_PX.0, PLAIN_PX.1)
    );

    // With the depth prepass the receiver's colour pass still passes the depth-equal test.
    let prepass = Run {
        depth_prepass: true,
        ..on
    };
    let (pre, stats) = render(&device, &prepass);
    assert_eq!(stats.drawn, 1);
    for (x, y) in [RECEIVER_LEFT, RECEIVER_RIGHT] {
        assert!(
            px(&pre, x, y).abs_diff(px(&lit, x, y)) <= 6,
            "prepass frame at ({x}, {y}): {} vs {}",
            px(&pre, x, y),
            px(&lit, x, y)
        );
    }
}

#[test]
fn an_entry_the_lightmap_lacks_keeps_the_probe_light_and_is_counted() {
    let device = device();
    let none = Run {
        map: None,
        settings: LightmapSettings::default(),
        receivers: &[],
        depth_prepass: false,
    };
    let (plain_frame, _) = render(&device, &none);
    let bad = Run {
        map: Some(Arc::new(lightmap(LOW, HIGH))),
        settings: enabled(),
        receivers: &[(RECEIVER, 7)],
        depth_prepass: false,
    };
    let (frame, stats) = render(&device, &bad);
    assert_eq!((stats.drawn, stats.unsupported), (0, 1), "{stats:?}");
    assert!(
        frame == plain_frame,
        "an unsupported receiver draws as without the lightmap"
    );
}

#[test]
fn intensity_and_bilinear_reach_the_shader_without_a_rebuild() {
    let device = device();
    let lab = lab(false);
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let (left_v, left_i) = quad(-0.5);
    let map = Arc::new(lightmap(LOW, LOW));
    let built = receiver_mesh(&map, 0, &left_v, &left_i).expect("the baked mesh");
    let scene = FrameScene {
        meshes: vec![FrameMesh {
            mesh_key: 0x0A11_00B1 << 3,
            object_key: RECEIVER,
            vertices: &built.vertices,
            indices: &built.indices,
            uv1: &built.uv2,
            ..template
        }],
        ..base.clone()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
    renderer.set_lightmap(Some(map.clone()), enabled());
    let one = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
    // The same Arc with a higher intensity and bilinear filtering: parameters only.
    renderer.set_lightmap(
        Some(map.clone()),
        LightmapSettings {
            intensity: 2.0,
            bicubic: false,
            ..enabled()
        },
    );
    let two = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
    let (a, b) = (
        px(&one, RECEIVER_RIGHT.0, RECEIVER_RIGHT.1),
        px(&two, RECEIVER_RIGHT.0, RECEIVER_RIGHT.1),
    );
    assert!(b > a, "twice the intensity is brighter: {a} -> {b}");
    // Removing the lightmap: the receiver keeps the ambient again.
    renderer.set_lightmap(None, enabled());
    let back = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
    let mut fresh = FrameRenderer::new();
    let plain = fresh.render_offscreen(&device, &scene).expect("frame").pixels;
    assert!(back == plain, "no lightmap: the plain frame again");
    assert_eq!(renderer.lightmap_forward_stats().drawn, 0);
}

/// Renders the receiver quad alone with `map` installed under `settings`, the lab quality with
/// the GPU-driven and visibility-buffer switches as given; returns the frame and the stats.
fn render_receiver(
    device: &RenderDevice,
    map: &Arc<Lightmap>,
    settings: LightmapSettings,
    (gpu_driven, visibility_buffer): (bool, bool),
) -> (Vec<u8>, regia_render::lightmap::LightmapForwardStats) {
    let lab = lab(false);
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let (left_v, left_i) = quad(-0.5);
    let built = receiver_mesh(map, 0, &left_v, &left_i).expect("the baked mesh");
    let scene = FrameScene {
        quality: regia_render::FrameQuality {
            gpu_driven,
            visibility_buffer,
            ..base.quality
        },
        meshes: vec![FrameMesh {
            mesh_key: 0x0A11_00C1 << 3,
            object_key: RECEIVER,
            vertices: &built.vertices,
            indices: &built.indices,
            uv1: &built.uv2,
            ..template
        }],
        ..base.clone()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_lightmap(Some(map.clone()), settings);
    renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
    let img = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    (img, renderer.lightmap_forward_stats())
}

/// The double sun of ADR-1623's known issue (ADR-2024): a lightmap baked with `--direct-sun` holds
/// the sun's diffuse light, so a receiver drops the live sun's diffuse term and keeps only its
/// specular; everything that is not a receiver of that lightmap keeps the live sun whole.
#[test]
fn a_lightmap_holding_the_direct_sun_replaces_the_receiver_s_live_sun_diffuse() {
    /// Renders both quads under `sun_lux` overhead-facing light with `contents` in the lightmap.
    fn frame(device: &RenderDevice, sun_lux: f32, contents: u32, on: bool) -> Vec<u8> {
        let mut lab = lab(false);
        // Toward the quads' front: the sun is the bulk of the light on a rough dielectric.
        lab.directional = DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], sun_lux);
        lab.materials[0].roughness = 1.0;
        let base = lab.as_frame_scene();
        let template = base.meshes[0];
        let (left_v, left_i) = quad(-0.5);
        let (right_v, right_i) = quad(0.5);
        let mut map = lightmap(LOW, LOW);
        map.bake.contents = contents;
        let built = receiver_mesh(&map, 0, &left_v, &left_i).expect("the baked mesh");
        let scene = FrameScene {
            meshes: vec![
                FrameMesh {
                    mesh_key: 0x0A11_00D1 << 3,
                    object_key: RECEIVER,
                    vertices: &built.vertices,
                    indices: &built.indices,
                    uv1: &built.uv2,
                    ..template
                },
                FrameMesh {
                    mesh_key: 0x0A11_00D2 << 3,
                    object_key: PLAIN,
                    vertices: &right_v,
                    indices: &right_i,
                    ..template
                },
            ],
            ..base.clone()
        };
        let mut renderer = FrameRenderer::new();
        let settings = LightmapSettings {
            enabled: on,
            ..LightmapSettings::default()
        };
        renderer.set_lightmap(Some(Arc::new(map)), settings);
        renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
        renderer.render_offscreen(device, &scene).expect("frame").pixels
    }
    const WITH_SUN: u32 = LightmapContents::SKY | LightmapContents::SUN_DIRECT;
    let device = device();
    let live = frame(&device, 8_000.0, LightmapContents::SKY, true);
    let baked = frame(&device, 8_000.0, WITH_SUN, true);
    let no_sun = frame(&device, 0.0, LightmapContents::SKY, true);
    let at = |img: &[u8], p: (u32, u32)| px(img, p.0, p.1);

    // The test sees the problem: the live sun on top of the same lightmap is much brighter.
    assert!(
        at(&live, RECEIVER_LEFT) > at(&no_sun, RECEIVER_LEFT) + 30,
        "the sun must add visible light: {} vs {}",
        at(&live, RECEIVER_LEFT),
        at(&no_sun, RECEIVER_LEFT)
    );
    // With the sun baked the receiver is lit once: nothing but the small specular lobe stays.
    let once = at(&baked, RECEIVER_LEFT);
    assert!(
        once.abs_diff(at(&no_sun, RECEIVER_LEFT)) * 4 < at(&live, RECEIVER_LEFT) - at(&no_sun, RECEIVER_LEFT),
        "baked {once}, without the sun {}, with both {}",
        at(&no_sun, RECEIVER_LEFT),
        at(&live, RECEIVER_LEFT)
    );
    // A quad that is not a receiver keeps the live sun, byte for byte.
    assert_eq!(at(&baked, PLAIN_PX), at(&live, PLAIN_PX));
    assert!(at(&baked, PLAIN_PX) > at(&no_sun, PLAIN_PX) + 30);
    // With the lightmap switched off nothing is baked: the receiver is a plain sunlit surface.
    let off_baked = frame(&device, 8_000.0, WITH_SUN, false);
    let off_live = frame(&device, 8_000.0, LightmapContents::SKY, false);
    assert_eq!(off_baked, off_live, "a disabled lightmap changes nothing");
}

/// With directions, the lookup bends the baked light by the shading normal: light baked as
/// arriving along +x (`a = 1`: the bake normal faced it) on a quad facing +z gets
/// `(1 + n · d) / 2 = 1/2` of the irradiance; with the directional switch off it gets all of it.
#[test]
fn the_direction_section_and_the_shading_normal_reach_the_lookup() {
    let device = device();
    let mut map = lightmap(HIGH, HIGH);
    map.set_directions(&[[1.0, 0.0, 0.0, 1.0]; 16])
        .expect("one per texel");
    let map = Arc::new(map);
    let (bent, stats) = render_receiver(&device, &map, enabled(), (false, false));
    assert_eq!(stats.drawn, 1);
    let (flat, _) = render_receiver(
        &device,
        &map,
        LightmapSettings {
            directional: false,
            ..enabled()
        },
        (false, false),
    );
    let (b, f) = (
        px(&bent, RECEIVER_RIGHT.0, RECEIVER_RIGHT.1),
        px(&flat, RECEIVER_RIGHT.0, RECEIVER_RIGHT.1),
    );
    assert!(
        b + 20 < f,
        "a sideways baked direction halves the light: {b} vs {f}"
    );
}

/// In a frame that asks for the GPU-driven batches and the visibility buffer (no meshlets here, so
/// the batches: `tests/lightmap_paths.rs` covers both paths), a receiver takes the lightmap and looks
/// as in a plain frame.
#[test]
fn a_receiver_keeps_the_lightmap_in_gpu_driven_and_visibility_buffer_frames() {
    let device = device();
    let map = Arc::new(lightmap(LOW, HIGH));
    let (plain, stats) = render_receiver(&device, &map, enabled(), (false, false));
    assert_eq!(stats.drawn, 1);
    for switches in [(true, false), (true, true)] {
        let (img, stats) = render_receiver(&device, &map, enabled(), switches);
        assert_eq!(
            (stats.drawn, stats.fallback, stats.unsupported),
            (1, 0, 0),
            "gpu_driven, visbuffer {switches:?}: {stats:?}"
        );
        for (x, y) in [RECEIVER_LEFT, RECEIVER_RIGHT] {
            assert!(
                px(&img, x, y).abs_diff(px(&plain, x, y)) <= 6,
                "gpu_driven, visbuffer {switches:?} at ({x}, {y}): {} vs {}",
                px(&img, x, y),
                px(&plain, x, y)
            );
        }
    }
}

/// ADR-4762: a receiver takes its indirect specular from the lightmap. A metallic receiver has no
/// diffuse term and the lab frame has no image light, probes or reflections, so without the
/// specular its indirect light is black; with it, it reflects the baked light (the `HIGH` half
/// brighter than the `LOW` half), on the full shader and on both lite tiers. A rough dielectric
/// gray surface gains a little. The plain quad never changes, and with lightmaps off the flag does nothing.
#[test]
fn a_receiver_reflects_the_baked_light() {
    /// Both quads under the lightmap at `metallic`, the receivers' specular `specular`, on the
    /// lite tier `lite` (`None`: the full shader).
    fn frame(
        device: &RenderDevice,
        metallic: f32,
        settings: LightmapSettings,
        lite: Option<regia_render::forward_lite::ForwardLite>,
    ) -> Vec<u8> {
        let mut lab = lab(false);
        lab.materials[0].metallic = metallic;
        // A gray dielectric gains reflection after reserving its diffuse energy. A white
        // receiver stays constant, covered separately by the furnace regression below.
        if metallic == 0.0 {
            lab.materials[0].base_color = [0.5, 0.5, 0.5, 1.0];
        }
        lab.materials[0].roughness = 0.5;
        let base = lab.as_frame_scene();
        let template = base.meshes[0];
        let (left_v, left_i) = quad(-0.5);
        let (right_v, right_i) = quad(0.5);
        let map = lightmap(LOW, HIGH);
        let built = receiver_mesh(&map, 0, &left_v, &left_i).expect("the baked mesh");
        let scene = FrameScene {
            meshes: vec![
                FrameMesh {
                    mesh_key: 0x0A11_00E1 << 3,
                    object_key: RECEIVER,
                    vertices: &built.vertices,
                    indices: &built.indices,
                    uv1: &built.uv2,
                    ..template
                },
                FrameMesh {
                    mesh_key: 0x0A11_00E2 << 3,
                    object_key: PLAIN,
                    vertices: &right_v,
                    indices: &right_i,
                    ..template
                },
            ],
            ..base.clone()
        };
        let mut renderer = FrameRenderer::new();
        renderer.set_forward_lite(lite);
        renderer.set_lightmap(Some(Arc::new(map)), settings);
        renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
        renderer.render_offscreen(device, &scene).expect("frame").pixels
    }
    use regia_render::forward_lite::ForwardLite;
    let device = device();
    let on = enabled();
    let off = LightmapSettings {
        specular: false,
        ..on
    };
    let at = |img: &[u8], p: (u32, u32)| px(img, p.0, p.1);
    for lite in [None, Some(ForwardLite::High), Some(ForwardLite::Low)] {
        let metal_off = frame(&device, 1.0, off, lite);
        let metal_on = frame(&device, 1.0, on, lite);
        for p in [RECEIVER_LEFT, RECEIVER_RIGHT] {
            assert!(
                at(&metal_off, p) <= 6,
                "{lite:?}: a metal receiver without the specular has no indirect light: {}",
                at(&metal_off, p)
            );
        }
        assert!(
            at(&metal_on, RECEIVER_LEFT) > at(&metal_off, RECEIVER_LEFT) + 10,
            "{lite:?}: the metal reflects the baked LOW light: {} vs {}",
            at(&metal_on, RECEIVER_LEFT),
            at(&metal_off, RECEIVER_LEFT)
        );
        assert!(
            at(&metal_on, RECEIVER_RIGHT) > at(&metal_on, RECEIVER_LEFT) + 30,
            "{lite:?}: brighter where the bake is brighter: {} vs {}",
            at(&metal_on, RECEIVER_RIGHT),
            at(&metal_on, RECEIVER_LEFT)
        );
        assert_eq!(
            at(&metal_on, PLAIN_PX),
            at(&metal_off, PLAIN_PX),
            "{lite:?}: the plain quad is not a receiver"
        );
        let stone_off = frame(&device, 0.0, off, lite);
        let stone_on = frame(&device, 0.0, on, lite);
        assert!(
            at(&stone_on, RECEIVER_RIGHT) > at(&stone_off, RECEIVER_RIGHT),
            "{lite:?}: a dielectric receiver gains its specular: {} vs {}",
            at(&stone_on, RECEIVER_RIGHT),
            at(&stone_off, RECEIVER_RIGHT)
        );
        assert_eq!(at(&stone_on, PLAIN_PX), at(&stone_off, PLAIN_PX), "{lite:?}");
        let disabled = |specular| LightmapSettings {
            enabled: false,
            specular,
            ..on
        };
        assert_eq!(
            frame(&device, 1.0, disabled(true), lite),
            frame(&device, 1.0, disabled(false), lite),
            "{lite:?}: with lightmaps off the specular flag changes nothing"
        );
    }
}

/// A white receiver under uniform baked light returns the same radiance with the energy split
/// enabled. A black-baked metal keeps a local capture's partial coverage linearly, not squared.
#[test]
fn receiver_energy_and_partial_capture_shares_are_conserved() {
    use regia_render::forward_lite::ForwardLite;
    use regia_render::{EnvironmentAuthority, LocalCaptureBake, LocalCaptureDesc, StudioEquirect};
    let device = device();
    let frame = |specular, lite, metallic, roughness, multi_scatter, capture_weight: Option<f32>| {
        let mut lab = lab(false);
        lab.materials[0].base_color = [1.0; 4];
        lab.materials[0].metallic = metallic;
        lab.materials[0].roughness = roughness;
        lab.materials[0].hard_surface.multi_scatter = if multi_scatter { 1.0 } else { 0.0 };
        lab.ibl = capture_weight.is_some();
        let irradiance = if capture_weight.is_some() { 0.0 } else { LOW };
        let map = Arc::new(lightmap(irradiance, irradiance));
        let (vertices, indices) = quad(-0.5);
        let built = receiver_mesh(&map, 0, &vertices, &indices).expect("receiver");
        let mut scene = lab.as_frame_scene();
        scene.meshes[0] = FrameMesh {
            object_key: RECEIVER,
            vertices: &built.vertices,
            indices: &built.indices,
            uv1: &built.uv2,
            ..scene.meshes[0]
        };
        let mut renderer = FrameRenderer::new();
        renderer.set_forward_lite(lite);
        renderer.set_lightmap(
            Some(map),
            LightmapSettings {
                specular,
                ..enabled()
            },
        );
        renderer.set_lightmap_receivers(&[(RECEIVER, 0)]);
        if let Some(weight) = capture_weight {
            renderer.set_environment_authority(EnvironmentAuthority::Studio);
            let sky = StudioEquirect::new(64, 32, vec![[0.0; 3]; 64 * 32]).expect("black sky");
            assert!(renderer.set_studio_equirect(&device, Some(&sky)));
            let width = regia_render::LOCAL_CAPTURE_WIDTH;
            // Rgba16Float [0, 0, 0.25, 1]: uniform blue radiance, full surface coverage.
            let texel = [0, 0, 0, 0, 0, 0x34, 0, 0x3c];
            let bake = LocalCaptureBake {
                width,
                coverage: 1.0,
                mips: (0..regia_render::local_capture_mip_count())
                    .map(|m| texel.repeat(((width >> m).max(1) * ((width / 2) >> m).max(1)) as usize))
                    .collect(),
            };
            let mut desc = LocalCaptureDesc::new(1, [0.0; 3], [10.0; 3]);
            desc.weight = weight;
            desc.bake = Some(Arc::new(bake));
            renderer.place_local_capture(desc).expect("capture");
            renderer.select_local_captures(&device, scene.eye, 1);
        }
        renderer.render_offscreen(&device, &scene).expect("frame").pixels
    };
    for lite in [None, Some(ForwardLite::High), Some(ForwardLite::Low)] {
        for roughness in [0.1, 0.5, 1.0] {
            let off = frame(false, lite, 0.0, roughness, false, None);
            let on = frame(true, lite, 0.0, roughness, false, None);
            assert!(
                (30..700).contains(&px(&off, RECEIVER_LEFT.0, RECEIVER_LEFT.1)),
                "white-furnace reference must be visible and unsaturated"
            );
            for (x, y) in [RECEIVER_LEFT, RECEIVER_RIGHT] {
                assert!(
                    px(&on, x, y).abs_diff(px(&off, x, y)) <= 3,
                    "white furnace {lite:?}, roughness {roughness}: {} vs {}",
                    px(&on, x, y),
                    px(&off, x, y)
                );
            }
        }
    }
    // Multi-scatter returns the white conductor's missing energy and reserves its share
    // alongside single scatter for a dielectric. The bake works with image lighting off.
    for roughness in [0.1, 0.5, 1.0] {
        let reference = frame(false, None, 0.0, roughness, false, None);
        for metallic in [0.0, 1.0] {
            let compensated = frame(true, None, metallic, roughness, true, None);
            for (x, y) in [RECEIVER_LEFT, RECEIVER_RIGHT] {
                assert!(
                    px(&compensated, x, y).abs_diff(px(&reference, x, y)) <= 3,
                    "multi-scatter furnace, metallic {metallic}, roughness {roughness}: {} vs {}",
                    px(&compensated, x, y),
                    px(&reference, x, y)
                );
            }
        }
    }
    // Compare a half-weight capture with the legacy path, which already uses that weight once.
    // A metal has no diffuse term, and both the environment and baked irradiance are black.
    for weight in [0.25, 0.5, 1.0] {
        let off = frame(false, None, 1.0, 0.5, false, Some(weight));
        let on = frame(true, None, 1.0, 0.5, false, Some(weight));
        let blue = off[((RECEIVER_LEFT.1 * W + RECEIVER_LEFT.0) * 4 + 2) as usize];
        assert!(
            (20..240).contains(&blue),
            "capture must be visible and unsaturated: {blue}"
        );
        assert!(
            px(&off, RECEIVER_LEFT.0, RECEIVER_LEFT.1) > 0,
            "capture must be visible"
        );
        for (x, y) in [RECEIVER_LEFT, RECEIVER_RIGHT] {
            assert!(
                px(&on, x, y).abs_diff(px(&off, x, y)) <= 3,
                "capture weight {weight} must be applied once: {} vs {}",
                px(&on, x, y),
                px(&off, x, y)
            );
        }
    }
}
