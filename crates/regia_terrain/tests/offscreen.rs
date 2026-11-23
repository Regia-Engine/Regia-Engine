// SPDX-License-Identifier: MPL-2.0
//! Offscreen GPU integration tests for the terrain crate (lavapipe in CI, any adapter locally).
//!
//! These exercise the whole hook end to end: height texture upload, LOD selection upload,
//! terrain + scatter + water draws, holes, deformation and time-of-day input — through
//! `regia_terrain::gpu::render_offscreen`, the reference consumer of the
//! [`regia_terrain::gpu::TerrainRenderer`] trait.
//!
//! The M13 round 3 shading tests pin the visual quality the lead requires: no blown-out or
//! crushed regions, at least three distinct hue clusters (green grass, brown dirt/sand, grey
//! rock/snow), and visible sun-direction lighting on a ridge. Round 4 adds the scatter
//! shading test: instance pixels (via a render-diff ID mask) must be lit and coloured like
//! the terrain, not black specks.

use std::collections::HashSet;
use std::sync::OnceLock;

use regia_asset::Heightfield;
use regia_terrain::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use regia_terrain::{
    CdlodConfig, Crater, HoleEllipse, HolesMask, LayerDesc, MinMaxPyramid, ScatterRule, TerrainLayers,
    TerrainMap, WaterPlane, apply_crater, place, select_nodes,
};

const W: u32 = 320;
const H: u32 = 180;
/// Dark neutral clear — the sky pass fills the background; this is fallback only.
const CLEAR: [f64; 4] = [0.02, 0.02, 0.05, 1.0];

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Island map: 257×257 samples over 256 m (1 m spacing), a 60 m mountain at the centre
/// falling to open water beyond ~105 m. 62·gauss(r/82) − 8 plus gentle ripples. The height
/// range needs the full u16 range: stored as `(h + 64) / 128 · 65535`.
fn island_map() -> TerrainMap {
    let n = 257u32;
    let width = 256.0f32;
    let mut samples = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            let r = (x * x + z * z).sqrt();
            let h = 62.0 * (-(r / 82.0).powi(2)).exp() - 8.0 + 2.6 * (x * 0.09).sin() * (z * 0.075).cos();
            samples.push(((h + 64.0) / 128.0 * 65535.0).round().clamp(0.0, 65535.0) as u16);
        }
    }
    let heightfield = Heightfield {
        width: n,
        height: n,
        samples,
    };
    TerrainMap::new(&heightfield, width, 128.0, -64.0).expect("valid island")
}

/// Ridge map for the lit-vs-shadowed test: a ridge running along X at z = 0, ~32 m tall
/// with steep faces, plus gentle ripples so both faces are non-uniform.
fn ridge_map() -> TerrainMap {
    let n = 257u32;
    let width = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            let h = 32.0 * (-(z / 21.0).powi(2)).exp() + 1.5 * (x * 0.1).sin() * (z * 0.13).sin();
            heights.push(h);
        }
    }
    TerrainMap::from_heights(n, n, heights, width, 44.0, -6.0).expect("valid ridge")
}

/// DO27-style height sample generator: 256×256 samples at 1 m spacing (255 m world
/// width), gentle hill + ripples, metre heights clamped to the DO27 0..32 m range
/// (65535/32 m per count).
fn do27_samples() -> Vec<u16> {
    let n = 256u32;
    let width = 255.0f32;
    let mut samples = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            let r2 = x * x + z * z;
            let h = 12.0 * (-r2 / (80.0 * 80.0)).exp()
                + 3.0 * (x * 0.04).sin() * (z * 0.05).cos()
                + 1.5 * (x * 0.12).sin() * (z * 0.11).sin();
            samples.push((h.clamp(0.0, 32.0) / 32.0 * 65535.0).round() as u16);
        }
    }
    samples
}

/// The island's five-layer stack: beach sand, grass, dirt, rock on steep slopes, snow on
/// the peak. Bands overlap so the weights cover every slope/altitude continuously.
fn layers() -> TerrainLayers {
    TerrainLayers {
        layers: vec![
            LayerDesc {
                name: "sand".into(),
                albedo: [0.42, 0.37, 0.26],
                roughness: 0.95,
                tiling: 6.0,
                slope_min: 0.0,
                slope_max: 0.5,
                altitude_min: -60.0,
                altitude_max: 5.5,
                blend_sharpness: 0.15,
                triplanar: false,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "grass".into(),
                albedo: [0.13, 0.28, 0.08],
                roughness: 0.95,
                tiling: 8.0,
                slope_min: 0.0,
                slope_max: 0.62,
                altitude_min: -60.0,
                altitude_max: 38.0,
                blend_sharpness: 0.2,
                triplanar: false,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "dirt".into(),
                albedo: [0.32, 0.22, 0.13],
                roughness: 0.95,
                tiling: 7.0,
                slope_min: 0.0,
                slope_max: 0.5,
                altitude_min: 6.0,
                altitude_max: 40.0,
                blend_sharpness: 0.25,
                triplanar: false,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "rock".into(),
                albedo: [0.40, 0.38, 0.36],
                roughness: 0.85,
                tiling: 4.0,
                slope_min: 0.42,
                slope_max: 1.6,
                altitude_min: -60.0,
                altitude_max: 90.0,
                blend_sharpness: 0.12,
                triplanar: true,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "snow".into(),
                albedo: [0.44, 0.44, 0.52],
                roughness: 0.8,
                tiling: 5.0,
                slope_min: 0.0,
                slope_max: 0.85,
                altitude_min: 32.0,
                altitude_max: 90.0,
                blend_sharpness: 0.18,
                triplanar: false,
                ..LayerDesc::ground()
            },
        ],
        ..Default::default()
    }
}

fn config() -> CdlodConfig {
    CdlodConfig::with_grid_size(16)
}

fn camera() -> TerrainCamera {
    TerrainCamera {
        position: [0.0, 90.0, 175.0],
        target: [0.0, 12.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    }
}

fn default_tod() -> TimeOfDay {
    TimeOfDay::default()
}

fn frame(debug_view: u32) -> TerrainFrame {
    let mut frame = TerrainFrame::from_camera(&camera(), &default_tod(), 0.1, 1.7);
    frame.debug_view = debug_view;
    frame
}

fn frame_from(camera: &TerrainCamera, tod: &TimeOfDay) -> TerrainFrame {
    TerrainFrame::from_camera(camera, tod, 0.1, 1.7)
}

fn pyramid(map: &TerrainMap) -> MinMaxPyramid {
    MinMaxPyramid::build(map, 4).expect("pyramid")
}

fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = (y * W + x) as usize * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// Returns true if the pixel is sky (blue-dominant gradient).
fn is_sky(p: [u8; 4]) -> bool {
    // Sky gradient: blue > red + margin, blue > green + margin.
    let r = p[0] as u32;
    let g = p[1] as u32;
    let b = p[2] as u32;
    b > r + 30 && b > g + 15 && r < 180
}

/// Returns true if the pixel is sky, including the warm near-horizon band of the gradient
/// (pale, green ≥ red, very bright). Sunlit snow is redder than it is green and never
/// matches, so terrain pixels survive this mask.
fn is_sky_or_haze(p: [u8; 4]) -> bool {
    let r = p[0] as i32;
    let g = p[1] as i32;
    let b = p[2] as i32;
    let lum = (0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64) as i32;
    is_sky(p) || (lum > 200 && g > r + 4 && b > r)
}

fn luminance(p: [u8; 4]) -> f64 {
    0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64
}

/// Renders terrain for a given camera frame (with water).
fn render(map: &TerrainMap, mask: Option<&HolesMask>, frame: &TerrainFrame, scatter: bool) -> Vec<u8> {
    render_with(map, mask, frame, scatter, true)
}

/// Renders terrain for a given camera frame, optionally with the water plane.
fn render_with(
    map: &TerrainMap,
    mask: Option<&HolesMask>,
    frame: &TerrainFrame,
    scatter: bool,
    water: bool,
) -> Vec<u8> {
    let mut gpu = TerrainGpu::new(
        device(),
        map,
        &layers(),
        &config(),
        4,
        mask,
        water.then(WaterPlane::default).as_ref(),
    )
    .expect("terrain gpu");
    if scatter {
        let rule = ScatterRule {
            cell_size: 12.0,
            instances_per_cell: 2.0,
            height_min: 1.0,
            fade_start_distance: 400.0,
            fade_end_distance: 900.0,
            ..Default::default()
        };
        let instances = place(map, mask, None, &rule);
        gpu.set_scatter(&instances, rule.fade_start_distance, rule.fade_end_distance, 0.0);
    }
    let cam_xz = [frame.camera_pos[0], frame.camera_pos[2]];
    let selection = select_nodes(map, &pyramid(map), &config(), 4, cam_xz, None);
    render_offscreen(&mut gpu, device(), frame, &selection, W, H, CLEAR).expect("offscreen render")
}

/// Count terrain pixels (everything that is not sky).
fn count_terrain_pixels(pixels: &[u8]) -> usize {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| !is_sky([p[0], p[1], p[2], p[3]]))
        .count()
}

/// The terrain-only pixels of a render (sky excluded).
fn terrain_pixels(pixels: &[u8]) -> Vec<[u8; 4]> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [p[0], p[1], p[2], p[3]])
        .filter(|p| !is_sky(*p))
        .collect()
}

// ---------------------------------------------------------------------------
// Geometry / feature tests
// ---------------------------------------------------------------------------

#[test]
fn terrain_renders_ground_and_sky() {
    let map = island_map();
    let pixels = render(&map, None, &frame(0), false);

    // The sky pass covers the whole screen; terrain draws on top.
    // The majority of pixels should be terrain (not just sky).
    let terrain_count = count_terrain_pixels(&pixels);
    assert!(
        terrain_count > (W * H / 2) as usize,
        "expected substantial terrain coverage, got {terrain_count} terrain pixels"
    );

    // Terrain near the centre should have green-ish colour (grass layer dominates).
    let centre = pixel(&pixels, W / 2, H / 2);
    assert!(!is_sky(centre), "center pixel should be terrain, got {centre:?}");
    assert!(
        centre.iter().any(|&c| c > 30),
        "center pixel should not be near-black: {centre:?}"
    );
}

#[test]
fn hole_opens_the_surface() {
    let map = island_map();
    // Look down through the hole into the under-terrain void (sky shows through).
    let hole_camera = TerrainCamera {
        position: [0.0, 65.0, 45.0],
        target: [0.0, 30.0, -10.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    };
    let hole_frame = frame_from(&hole_camera, &default_tod());
    let mask = HolesMask::new(vec![HoleEllipse {
        center: [0.0, 0.0],
        radius: [35.0, 25.0],
        rotation_radians: 0.0,
    }])
    .expect("valid mask");

    let open = render(&map, Some(&mask), &hole_frame, false);
    let closed = render(&map, None, &hole_frame, false);
    assert!(!open.is_empty() && open.len() == closed.len());

    // The hole removes triangles; nearby pixels should differ from the no-hole render.
    let diff_count = (0..open.len())
        .filter(|&i| (open[i] as i16 - closed[i] as i16).unsigned_abs() > 10)
        .count();
    assert!(
        diff_count > 200,
        "hole must change the rendered surface (diff {diff_count} px)"
    );

    // Sky should show through the hole (terrain is removed, sky is behind).
    let sky_through_hole = open
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| is_sky([p[0], p[1], p[2], p[3]]))
        .count();
    let sky_no_hole = closed
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| is_sky([p[0], p[1], p[2], p[3]]))
        .count();
    assert!(
        sky_through_hole > sky_no_hole + 20,
        "hole must reveal more sky ({sky_through_hole} vs {sky_no_hole} sky px)"
    );
}

#[test]
fn water_renders_over_submerged_ground() {
    let map = island_map();
    let water_camera = TerrainCamera {
        position: [90.0, 12.0, 80.0],
        target: [90.0, -1.0, 0.0],
        fov_y_radians: 0.8,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 1000.0,
    };
    let water_frame = frame_from(&water_camera, &default_tod());

    let with_water = render(&map, None, &water_frame, false);
    let without_water = render_with(&map, None, &water_frame, false, false);

    let diff = (0..without_water.len())
        .filter(|&i| (without_water[i] as i16 - with_water[i] as i16).unsigned_abs() > 8)
        .count();
    assert!(
        diff > 100,
        "water must differ from terrain-only render in submerged areas ({diff} px differ)"
    );

    // Water colour is blue-ish in deep areas.
    let deep_probe = pixel(&with_water, W / 2, H - 8);
    assert!(
        deep_probe[2] > deep_probe[0],
        "deep water should be blue-dominant (b > r), got {deep_probe:?}"
    );
}

/// `TerrainGpu::set_water` swaps the plane on a built terrain: adding it changes the frame,
/// removing it restores the dry frame (no byte moves past rasterizer noise), and an invalid
/// plane is refused with the old one kept.
#[test]
fn set_water_replaces_the_plane() {
    let map = island_map();
    let water_camera = TerrainCamera {
        position: [90.0, 12.0, 80.0],
        target: [90.0, -1.0, 0.0],
        fov_y_radians: 0.8,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 1000.0,
    };
    let frame = frame_from(&water_camera, &default_tod());
    let selection = select_nodes(&map, &pyramid(&map), &config(), 4, [90.0, 80.0], None);
    let mut gpu = TerrainGpu::new(device(), &map, &layers(), &config(), 4, None, None).expect("terrain gpu");
    assert!(!gpu.has_water());
    let draw = |gpu: &mut TerrainGpu| {
        render_offscreen(gpu, device(), &frame, &selection, W, H, CLEAR).expect("offscreen render")
    };
    // Bytes that moved by more than rasterizer noise.
    fn changed(a: &[u8], b: &[u8]) -> usize {
        (0..a.len())
            .filter(|&i| (a[i] as i16 - b[i] as i16).unsigned_abs() > 8)
            .count()
    }
    let dry = draw(&mut gpu);
    gpu.set_water(Some(&WaterPlane::default())).expect("valid plane");
    assert!(gpu.has_water());
    let wet = draw(&mut gpu);
    let shown = changed(&dry, &wet);
    assert!(shown > 100, "the plane must show ({shown} bytes changed)");
    let bad = WaterPlane {
        alpha: f32::NAN,
        ..WaterPlane::default()
    };
    assert!(gpu.set_water(Some(&bad)).is_err());
    assert!(gpu.has_water(), "a refused plane keeps the old one");
    assert_eq!(
        changed(&draw(&mut gpu), &wet),
        0,
        "the refused plane changed the frame"
    );
    gpu.set_water(None).expect("remove");
    assert!(!gpu.has_water());
    assert_eq!(
        changed(&draw(&mut gpu), &dry),
        0,
        "removing the plane must restore the dry frame"
    );
}

/// Mean luminance of the 9×9 patch at the centre of a frame.
fn centre_luma(pixels: &[u8]) -> f64 {
    let mut sum = 0.0;
    for y in H / 2 - 4..=H / 2 + 4 {
        for x in W / 2 - 4..=W / 2 + 4 {
            sum += luminance(pixel(pixels, x, y));
        }
    }
    sum / 81.0
}

/// Mean per-channel difference of the 9×9 centre patches of two frames.
fn centre_diff(a: &[u8], b: &[u8]) -> f64 {
    let mut sum = 0.0;
    for y in H / 2 - 4..=H / 2 + 4 {
        for x in W / 2 - 4..=W / 2 + 4 {
            let (pa, pb) = (pixel(a, x, y), pixel(b, x, y));
            sum += (0..3).map(|c| (pa[c] as f64 - pb[c] as f64).abs()).sum::<f64>();
        }
    }
    sum / (81.0 * 3.0)
}

/// ADR-1237: the water reflects the sky by Fresnel. Seen at a grazing angle almost all of
/// what the surface shows is sky, seen from above about 2 %; so brightening the sky moves a
/// grazing view of the water much more than a view from above (where the water body and the
/// bed under it carry the picture). Before the fix both views alpha-blended the same body
/// color and moved alike.
#[test]
fn grazing_water_reflects_the_sky() {
    let map = island_map();
    let water = WaterPlane::default();
    // The camera looks north along x = -100 from 2 m above the water, 40 m short of the
    // target: the center patch (±4 px at 0.0044 rad a pixel) sees the water 30 to 64 m away,
    // all more than 117 m from the peak, so open water, at N·V ≈ 0.03..0.07.
    let target = [-100.0f32, water.level, -65.0];
    assert!(
        water.probe(&map, target[0], target[2], 0.0).depth > 2.0,
        "deep water at the target"
    );
    let grazing = TerrainCamera {
        position: [target[0], water.level + 2.0, target[2] - 40.0],
        target,
        fov_y_radians: 0.8,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 1000.0,
    };
    let above = TerrainCamera {
        position: [target[0], water.level + 36.0, target[2] + 5.0],
        target,
        ..grazing
    };
    let sky_response = |camera: &TerrainCamera| {
        let dim = default_tod();
        let bright = TimeOfDay {
            ambient: dim.ambient.map(|a| a * 2.0),
            ..dim
        };
        let lo = render(&map, None, &frame_from(camera, &dim), false);
        let hi = render(&map, None, &frame_from(camera, &bright), false);
        centre_luma(&hi) - centre_luma(&lo)
    };
    let grazing_gain = sky_response(&grazing);
    let above_gain = sky_response(&above);
    assert!(
        grazing_gain > 5.0 && grazing_gain > 1.5 * above_gain,
        "a brighter sky must show in grazing water ({grazing_gain:.1} levels) much more than in water seen from above ({above_gain:.1})"
    );
}

/// ADR-1237: the water body's opacity follows Beer–Lambert over the refracted path, so the
/// bed shows through shallow water and deep water shows its own color. Before the fix the
/// opacity was `alpha` at any depth past the shoreline.
#[test]
fn shallow_water_is_clearer_than_deep_water() {
    let map = island_map();
    let water = WaterPlane::default();
    // One point under 0.6 to 1.2 m of water on the gentle west shore, clear of the foam band
    // (the center patch spans about ±0.5 m), and one deep point east of the peak. `dir` picks
    // the side of the island the scan walks out along, on z = 0.
    let find = |dir: f32, pick: &dyn Fn(&regia_terrain::WaterProbe) -> bool| {
        (0..700)
            .map(|i| dir * (90.0 + i as f32 * 0.05))
            .find(|&x| pick(&water.probe(&map, x, 0.0, 0.0)))
            .unwrap_or_else(|| panic!("no such point on the island's shore"))
    };
    let shallow_x = find(-1.0, &|p| {
        (0.6..=1.2).contains(&p.depth) && p.shore_distance > water.foam_width + 1.4
    });
    let deep_x = find(1.0, &|p| p.depth > 6.0);
    let diff_at = |x: f32| {
        let camera = TerrainCamera {
            position: [x, water.level + 25.0, 6.0],
            target: [x, water.level, 0.0],
            fov_y_radians: 0.8,
            aspect: W as f32 / H as f32,
            near: 0.5,
            far: 1000.0,
        };
        let frame = frame_from(&camera, &default_tod());
        let with_water = render(&map, None, &frame, false);
        let without_water = render_with(&map, None, &frame, false, false);
        centre_diff(&with_water, &without_water)
    };
    let shallow = diff_at(shallow_x);
    let deep = diff_at(deep_x);
    assert!(
        deep > 8.0 && shallow * 2.0 < deep,
        "shallow water (x = {shallow_x}) must hide the bed much less than deep water (x = {deep_x}): {shallow:.1} vs {deep:.1} levels"
    );
}

/// ADR-1237: screen-space refraction. Looking straight down at still water the refracted ray
/// stays vertical, so what shows through is the frame straight behind: refraction on and off
/// agree at the center. Seen at 45° the bed shows from where the bent ray meets it, so the
/// two differ.
#[test]
fn refraction_follows_the_view_angle() {
    let map = island_map();
    let still = WaterPlane {
        wave_amplitude: 0.0,
        ..WaterPlane::default()
    };
    let deep_x = (0..700)
        .map(|i| 90.0 + i as f32 * 0.05)
        .find(|&x| still.probe(&map, x, 0.0, 0.0).depth > 6.0)
        .unwrap_or_else(|| panic!("no deep water east of the peak"));
    let render_both = |camera: &TerrainCamera| {
        let frame = frame_from(camera, &default_tod());
        let eye_xz = [camera.position[0], camera.position[2]];
        let selection = select_nodes(&map, &pyramid(&map), &config(), 4, eye_xz, None);
        let mut gpu = TerrainGpu::new(device(), &map, &layers(), &config(), 4, None, Some(&still))
            .expect("terrain gpu");
        assert!(gpu.water_refraction(), "refraction is on by default");
        let on = render_offscreen(&mut gpu, device(), &frame, &selection, W, H, CLEAR).expect("render");
        gpu.set_water_refraction(false);
        let off = render_offscreen(&mut gpu, device(), &frame, &selection, W, H, CLEAR).expect("render");
        (on, off)
    };
    let down = TerrainCamera {
        position: [deep_x, still.level + 30.0, 0.5],
        target: [deep_x, still.level, 0.0],
        fov_y_radians: 0.8,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 1000.0,
    };
    let (on, off) = render_both(&down);
    let centre = centre_diff(&on, &off);
    assert!(
        centre < 2.0,
        "straight down, refraction must not move the bed ({centre:.2} levels at the center)"
    );
    let oblique = TerrainCamera {
        position: [deep_x - 20.0, still.level + 20.0, 0.0],
        ..down
    };
    let (on, off) = render_both(&oblique);
    let moved = on.iter().zip(&off).filter(|(a, b)| a.abs_diff(**b) > 4).count();
    assert!(
        moved > 50,
        "at 45° the refracted bed must differ from the straight-through one ({moved} bytes moved)"
    );
}

/// ADR-1237: seen from below, the surface is a mirror past the critical angle (about 48.6°
/// from straight up) and a window to the sky inside it. A camera 2 m under the surface sees
/// the opaque water body when it looks 10° up, and the bright sky through the surface when
/// it looks 70° up. Before the fix both views blended the same body color at the same alpha.
#[test]
fn from_below_the_surface_mirrors_past_the_critical_angle() {
    let map = island_map();
    let water = WaterPlane::default();
    let eye = [120.0f32, water.level - 2.0, 0.0];
    assert!(
        water.probe(&map, eye[0], eye[2], 0.0).ground_height < eye[1] - 2.0,
        "the camera is in open water"
    );
    let looking_up = |elevation: f32| {
        let camera = TerrainCamera {
            position: eye,
            target: [eye[0], eye[1] + elevation.tan() * 10.0, eye[2] - 10.0],
            fov_y_radians: 0.5,
            aspect: W as f32 / H as f32,
            near: 0.1,
            far: 1000.0,
        };
        render(&map, None, &frame_from(&camera, &default_tod()), false)
    };
    let mirror = looking_up(10f32.to_radians());
    let window = looking_up(70f32.to_radians());
    let (mirror_luma, window_luma) = (centre_luma(&mirror), centre_luma(&window));
    let centre = pixel(&mirror, W / 2, H / 2);
    assert!(
        centre[2] > centre[0],
        "past the critical angle the surface shows the blue water body: {centre:?}"
    );
    assert!(
        window_luma > mirror_luma + 10.0,
        "the sky must show through the surface inside the window ({window_luma:.1}) more than past the critical angle ({mirror_luma:.1})"
    );
}

#[test]
fn scatter_instances_are_drawn() {
    let map = island_map();
    let near_camera = TerrainCamera {
        position: [55.0, 22.0, 95.0],
        target: [55.0, 4.0, 55.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 500.0,
    };
    let near_frame = frame_from(&near_camera, &default_tod());

    let bare = render(&map, None, &near_frame, false);
    let mut gpu = TerrainGpu::new(
        device(),
        &map,
        &layers(),
        &config(),
        4,
        None,
        Some(&WaterPlane::default()),
    )
    .expect("terrain gpu");
    let rule = ScatterRule {
        cell_size: 8.0,
        instances_per_cell: 3.0,
        min_scale: 0.8,
        max_scale: 2.0,
        height_min: -10.0,
        fade_start_distance: 300.0,
        fade_end_distance: 600.0,
        ..Default::default()
    };
    let instances = place(&map, None, None, &rule);
    assert!(
        instances.len() > 100,
        "scatter must place many instances, got {}",
        instances.len()
    );
    gpu.set_scatter(&instances, rule.fade_start_distance, rule.fade_end_distance, 0.0);
    let cam_xz = [near_frame.camera_pos[0], near_frame.camera_pos[2]];
    let selection = select_nodes(&map, &pyramid(&map), &config(), 4, cam_xz, None);
    let dressed = render_offscreen(&mut gpu, device(), &near_frame, &selection, W, H, CLEAR).expect("render");

    let diff = (0..bare.len())
        .filter(|&i| (bare[i] as i16 - dressed[i] as i16).unsigned_abs() > 10)
        .count();
    assert!(
        diff > 100,
        "scatter rocks must change the render ({diff} px differ)"
    );
}

#[test]
fn time_of_day_changes_the_lighting() {
    let map = island_map();
    let noon = TerrainFrame::from_camera(&camera(), &default_tod(), 0.1, 1.7);
    let dusk_tod = TimeOfDay {
        elevation_radians: 0.12,
        azimuth_radians: -1.4,
        sun_color: [0.9, 0.45, 0.25],
        illuminance_lux: 5_000.0,
        ambient: [0.10, 0.10, 0.16],
    };
    let dusk = frame_from(&camera(), &dusk_tod);

    let a = render(&map, None, &noon, false);
    let b = render(&map, None, &dusk, false);
    let mut diff = 0usize;
    for i in 0..a.len() {
        if (a[i] as i16 - b[i] as i16).unsigned_abs() > 12 {
            diff += 1;
        }
    }
    assert!(
        diff > (W * H / 10) as usize,
        "time of day must relight the terrain ({diff} px)"
    );
}

#[test]
fn far_and_near_cameras_render_every_lod() {
    let map = island_map();
    let near = select_nodes(&map, &pyramid(&map), &config(), 4, [0.0, 175.0], None);
    let far = select_nodes(&map, &pyramid(&map), &config(), 4, [9000.0, 9000.0], None);
    assert!(near.nodes.len() > 40, "near camera should select many nodes");
    assert_eq!(far.nodes.len(), 1, "far camera selects the root only");

    let mut gpu = TerrainGpu::new(
        device(),
        &map,
        &layers(),
        &config(),
        4,
        None,
        Some(&WaterPlane::default()),
    )
    .expect("terrain gpu");
    let far_camera = TerrainCamera {
        position: [700.0, 500.0, 700.0],
        target: [0.0, 0.0, 0.0],
        fov_y_radians: 1.05,
        aspect: W as f32 / H as f32,
        near: 1.0,
        far: 5000.0,
    };
    let far_frame = TerrainFrame::from_camera(&far_camera, &default_tod(), 0.0, 0.0);
    let far_pixels = render_offscreen(&mut gpu, device(), &far_frame, &far, W, H, CLEAR).expect("far render");
    let ground = count_terrain_pixels(&far_pixels);
    assert!(
        ground > 200,
        "root-only LOD must still render terrain ({ground} px)"
    );
}

#[test]
fn crater_deformation_changes_the_render() {
    let map = island_map();
    let crater_camera = TerrainCamera {
        position: [0.0, 60.0, 110.0],
        target: [0.0, 10.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    };
    let crater_frame = frame_from(&crater_camera, &default_tod());

    let before = render(&map, None, &crater_frame, false);

    let mut gpu = TerrainGpu::new(
        device(),
        &map,
        &layers(),
        &config(),
        4,
        None,
        Some(&WaterPlane::default()),
    )
    .expect("terrain gpu");
    let mut deformed = map.clone();
    let dirty = apply_crater(
        &mut deformed,
        &Crater {
            center: [0.0, 20.0],
            radius: 30.0,
            depth: 14.0,
            rim_height: 3.0,
        },
    )
    .expect("crater");
    assert!(!dirty.is_empty());
    gpu.update_heights(&deformed, &dirty);
    let cam_xz = [crater_frame.camera_pos[0], crater_frame.camera_pos[2]];
    let def_pyr = MinMaxPyramid::build(&deformed, 4).expect("deformed pyramid");
    let selection = select_nodes(&deformed, &def_pyr, &config(), 4, cam_xz, None);
    let after = render_offscreen(&mut gpu, device(), &crater_frame, &selection, W, H, CLEAR).expect("render");

    let diff = (0..before.len())
        .filter(|&i| (before[i] as i16 - after[i] as i16).unsigned_abs() > 10)
        .count();
    assert!(
        diff > 100,
        "crater must change the rendered terrain ({diff} px differ)"
    );
}

// ---------------------------------------------------------------------------
// M13 Round 3 acceptance: shading quality the lead can see in the PNG
// ---------------------------------------------------------------------------

/// Fraction of terrain pixels whose channels all exceed/exund the extremes. Round 2's
/// image was 31% pure white + 24% flat dark grey; both must stay under 2% now.
#[test]
fn shading_extremes_bounded() {
    let map = island_map();
    let pixels = render(&map, None, &frame(0), false);
    let terrain = terrain_pixels(&pixels);
    assert!(
        terrain.len() > (W * H / 3) as usize,
        "need enough terrain pixels ({})",
        terrain.len()
    );

    let near_white = terrain
        .iter()
        .filter(|p| p[0] > 240 && p[1] > 240 && p[2] > 240)
        .count();
    let near_black = terrain
        .iter()
        .filter(|p| p[0] < 20 && p[1] < 20 && p[2] < 20)
        .count();
    let white_ratio = near_white as f64 / terrain.len() as f64;
    let black_ratio = near_black as f64 / terrain.len() as f64;
    assert!(
        white_ratio < 0.02,
        "near-white pixels ({near_white}, {white_ratio:.2}%) blow out the terrain"
    );
    assert!(
        black_ratio < 0.02,
        "near-black pixels ({near_black}, {black_ratio:.2}%) crush the terrain"
    );
}

/// Hue clusters among terrain pixels: green (grass), brown (dirt/sand), grey
/// (rock/snow/water shadow). Each must be meaningfully present.
#[test]
fn shading_has_three_hue_clusters() {
    let map = island_map();
    let pixels = render(&map, None, &frame(0), false);
    let terrain = terrain_pixels(&pixels);

    let mut green = 0usize;
    let mut brown = 0usize;
    let mut grey = 0usize;
    for p in &terrain {
        let r = p[0] as f64;
        let g = p[1] as f64;
        let b = p[2] as f64;
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let sat = if mx > 0.0 { (mx - mn) / mx } else { 0.0 };
        let hue = if mx - mn > 1e-9 {
            if mx == r {
                (60.0 * (g - b) / (mx - mn) + 360.0) % 360.0
            } else if mx == g {
                (60.0 * (2.0 + (b - r) / (mx - mn))) % 360.0
            } else {
                (60.0 * (4.0 + (r - g) / (mx - mn))) % 360.0
            }
        } else {
            0.0
        };
        let lum = luminance(*p);
        // Grey means display saturation below 0.10. (It was 0.15 under the old
        // linear-input AgX fit, which inflated saturation per channel; the corrected
        // transform prints this map's warm-lit rock and thin beach at 0.10–0.15, which
        // is still a visibly warm family, not grey.)
        if sat < 0.10 && (35.0..236.0).contains(&lum) {
            grey += 1;
        } else if (70.0..=170.0).contains(&hue) && g >= r {
            green += 1;
        } else if (15.0..=60.0).contains(&hue) && r > g {
            brown += 1;
        }
    }
    let n = terrain.len() as f64;
    let ratios = [
        ("green", green as f64 / n),
        ("brown", brown as f64 / n),
        ("grey", grey as f64 / n),
    ];
    for (name, ratio) in ratios {
        assert!(
            ratio > 0.02,
            "hue cluster {name} covers only {ratio:.2} of terrain (need > 2%): \
             green {:.2} brown {:.2} grey {:.2}",
            green as f64 / n,
            brown as f64 / n,
            grey as f64 / n
        );
    }
}

/// Round 4: scatter instances must be lit and coloured like the terrain — not black specks.
/// The instance mask is the diff between a bare render and the same render with instances
/// uploaded (identical GPU state otherwise), so the masked pixels are exactly the visible
/// instance fragments.
#[test]
fn scatter_instances_are_lit_and_colored() {
    let map = island_map();
    // Close enough that a 1.5–3.5 m rock spans 6–13 px — instances are measurable objects.
    let near_camera = TerrainCamera {
        position: [55.0, 22.0, 95.0],
        target: [55.0, 4.0, 55.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.1,
        far: 500.0,
    };
    let near_frame = frame_from(&near_camera, &default_tod());

    let mut gpu = TerrainGpu::new(
        device(),
        &map,
        &layers(),
        &config(),
        4,
        None,
        None, // no water plane: the mask must be pure terrain vs instance pixels
    )
    .expect("terrain gpu");
    let rule = ScatterRule {
        cell_size: 8.0,
        instances_per_cell: 3.0,
        min_scale: 1.5,
        max_scale: 3.5,
        height_min: -10.0,
        fade_start_distance: 300.0,
        fade_end_distance: 600.0,
        ..Default::default()
    };
    let instances = place(&map, None, None, &rule);
    assert!(
        instances.len() > 100,
        "scatter must place many instances, got {}",
        instances.len()
    );

    let cam_xz = [near_frame.camera_pos[0], near_frame.camera_pos[2]];
    let selection = select_nodes(&map, &pyramid(&map), &config(), 4, cam_xz, None);
    let bare =
        render_offscreen(&mut gpu, device(), &near_frame, &selection, W, H, CLEAR).expect("bare render");
    gpu.set_scatter(&instances, rule.fade_start_distance, rule.fade_end_distance, 0.0);
    let dressed =
        render_offscreen(&mut gpu, device(), &near_frame, &selection, W, H, CLEAR).expect("dressed render");

    // Instance mask: pixels the instance draw changed (any channel).
    let mask: Vec<usize> = (0..bare.len() / 4)
        .filter(|&i| {
            (0..4).any(|c| {
                let j = i * 4 + c;
                (bare[j] as i16 - dressed[j] as i16).unsigned_abs() > 10
            })
        })
        .collect();
    assert!(
        mask.len() > 500,
        "instances must cover measurable pixels ({} px)",
        mask.len()
    );

    let instance_px: Vec<[u8; 4]> = mask
        .iter()
        .map(|&i| {
            [
                dressed[i * 4],
                dressed[i * 4 + 1],
                dressed[i * 4 + 2],
                dressed[i * 4 + 3],
            ]
        })
        .collect();
    let n = instance_px.len() as f64;

    // The black-speck defect rendered instances at mean luminance ~30 (ambient-only after
    // AgX). Lit, terrain-coloured instances land in the 40..200 band.
    let mean = instance_px.iter().map(|p| luminance(*p)).sum::<f64>() / n;
    assert!(
        (40.0..=200.0).contains(&mean),
        "instance mean luminance {mean:.1} outside the sane band 40..200"
    );

    // Dark fragments are allowed (away-facing rock sides) but near-black must stay rare.
    let near_black = instance_px
        .iter()
        .filter(|p| p[0] < 20 && p[1] < 20 && p[2] < 20)
        .count();
    assert!(
        near_black as f64 / n < 0.05,
        "near-black instance pixels ({near_black}/{}) exceed 5%",
        mask.len()
    );

    // Visible lighting: the lit-face vs shaded-face spread proves N·L reaches the output
    // (the defect produced a flat ambient-only image, spread ~0).
    let mut lums: Vec<f64> = instance_px.iter().map(|p| luminance(*p)).collect();
    lums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p05 = lums[lums.len() / 20];
    let p95 = lums[lums.len() * 19 / 20];
    assert!(
        p95 - p05 > 25.0,
        "instances show no lighting variation (p05 {p05:.0}, p95 {p95:.0})"
    );
    eprintln!(
        "scatter shading: {} px, mean lum {mean:.1}, p05 {p05:.0}, p95 {p95:.0}, near-black {near_black}",
        mask.len()
    );
}

/// Round 2 shipped a two-colour image that still passed a "unique colours" check by a hair;
/// keep the check so quantised/binary output can never return.
#[test]
fn shading_has_many_unique_colors() {
    let map = island_map();
    let pixels = render(&map, None, &frame(0), false);
    let terrain = terrain_pixels(&pixels);
    let unique: HashSet<u32> = terrain
        .iter()
        .map(|p| ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | (p[2] as u32))
        .collect();
    assert!(
        unique.len() > 1_000,
        "only {} unique colors (need > 1,000 for a textured terrain with lighting)",
        unique.len()
    );
}

/// The sun-facing side of a ridge must render brighter than the shadowed side: mean
/// luminance of the sun-facing render > 1.3 × the mean of the far-side render.
#[test]
fn ridge_sun_side_brighter_than_shadow_side() {
    let map = ridge_map();
    // Sun from +Z (azimuth 0), moderately low: the +Z face is lit, the −Z face is not.
    let tod = TimeOfDay {
        azimuth_radians: 0.0,
        elevation_radians: 0.6,
        ..default_tod()
    };

    // Stand at the base of each face and look up at it: the frame is the face itself
    // (plus sky), so the means compare the two surfaces, not the shared plain.
    let sun_side_camera = TerrainCamera {
        position: [0.0, 16.0, 48.0],
        target: [0.0, 18.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    };
    let far_side_camera = TerrainCamera {
        position: [0.0, 16.0, -48.0],
        target: [0.0, 18.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    };
    let sun_side = frame_from(&sun_side_camera, &tod);
    let far_side = frame_from(&far_side_camera, &tod);

    let sun_pixels = render_with(&map, None, &sun_side, false, false);
    let far_pixels = render_with(&map, None, &far_side, false, false);

    let mean_of = |pixels: &[u8]| -> f64 {
        let terrain: Vec<[u8; 4]> = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2], p[3]])
            .filter(|p| !is_sky_or_haze(*p))
            .collect();
        assert!(
            terrain.len() > (W * H / 10) as usize,
            "ridge render must contain terrain ({} px)",
            terrain.len()
        );
        terrain.iter().map(|p| luminance(*p)).sum::<f64>() / terrain.len() as f64
    };
    let sun_mean = mean_of(&sun_pixels);
    let far_mean = mean_of(&far_pixels);
    eprintln!("ridge means: sun side {sun_mean:.1}, shadow side {far_mean:.1}");
    assert!(
        sun_mean > 1.3 * far_mean,
        "sun-facing ridge (mean lum {sun_mean:.1}) must out-brighten the shadow side \
         (mean lum {far_mean:.1}) by > 1.3×"
    );
}

// ---------------------------------------------------------------------------
// DO27-style check: 16-bit PNG heightfield, 1 m spacing, 65535/32 m per count
// ---------------------------------------------------------------------------

/// Encodes samples as a 16-bit grayscale PNG in memory (the format the asset pipeline's
/// PNG importer consumes).
fn encode_gray16_png(samples: &[u16], width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().expect("PNG header");
        let mut be = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            be.extend_from_slice(&s.to_be_bytes());
        }
        writer.write_image_data(&be).expect("PNG data");
        writer.finish().expect("PNG finish");
    }
    bytes
}

/// Decodes a 16-bit grayscale PNG back to u16 samples.
fn decode_gray16_png(bytes: &[u8]) -> (Vec<u16>, u32, u32) {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("PNG info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("PNG output size")];
    let info = reader.next_frame(&mut buf).expect("PNG frame");
    assert_eq!(info.color_type, png::ColorType::Grayscale);
    assert_eq!(info.bit_depth, png::BitDepth::Sixteen);
    let samples: Vec<u16> = buf[..info.buffer_size()]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .collect();
    (samples, info.width, info.height)
}

#[test]
fn do27_style_16bit_png_heightfield_renders() {
    // Generate a DO27-style heightfield: 256×256 at 1 m spacing, 65535/32 m per count.
    let samples = do27_samples();
    let png_bytes = encode_gray16_png(&samples, 256, 256);
    let (decoded, width, height) = decode_gray16_png(&png_bytes);
    assert_eq!((width, height), (256, 256));
    assert_eq!(decoded, samples, "16-bit PNG round trip must be exact");

    let heightfield = Heightfield {
        width,
        height,
        samples: decoded,
    };
    // 256×256 samples at 1 m spacing (world width 255 m), DO27 height scale 65535/32.
    let map = TerrainMap::new(&heightfield, 255.0, 32.0, 0.0).expect("valid do27 map");
    assert_eq!(map.width(), 256);
    assert_eq!(map.height(), 256);
    assert!((map.height_scale() - 32.0).abs() < 0.001, "DO27 scale");
    assert_eq!(map.height_offset(), 0.0);
    assert!((map.sample_spacing() - 1.0).abs() < 1e-4, "1 m spacing");

    let do27_camera = TerrainCamera {
        position: [0.0, 45.0, 90.0],
        target: [0.0, 6.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 3000.0,
    };
    let do27_frame = frame_from(&do27_camera, &default_tod());

    let mut gpu = TerrainGpu::new(device(), &map, &layers(), &config(), 4, None, None).expect("do27 gpu");
    let cam_xz = [do27_frame.camera_pos[0], do27_frame.camera_pos[2]];
    let pyr = MinMaxPyramid::build(&map, 4).expect("pyramid");
    let selection = select_nodes(&map, &pyr, &config(), 4, cam_xz, None);
    let pixels =
        render_offscreen(&mut gpu, device(), &do27_frame, &selection, W, H, CLEAR).expect("do27 render");

    // The render must contain terrain, and the shading must stay inside the extremes.
    let terrain = terrain_pixels(&pixels);
    assert!(
        terrain.len() > 100,
        "DO27 map must render terrain ({} terrain px)",
        terrain.len()
    );
    let near_white = terrain
        .iter()
        .filter(|p| p[0] > 240 && p[1] > 240 && p[2] > 240)
        .count();
    let near_black = terrain
        .iter()
        .filter(|p| p[0] < 20 && p[1] < 20 && p[2] < 20)
        .count();
    let n = terrain.len() as f64;
    assert!(
        (near_white as f64 / n) < 0.02,
        "DO27 map: near-white pixels ({near_white}) > 2% of terrain"
    );
    assert!(
        (near_black as f64 / n) < 0.02,
        "DO27 map: near-black pixels ({near_black}) > 2% of terrain"
    );
}

/// The sun cascade is cached: a held camera renders it once, the same pixels
/// come back, and a new sun direction (a new snapped view-projection) or new
/// scatter casters re-render it.
#[test]
fn held_camera_renders_the_sun_cascade_once() {
    let map = island_map();
    let mut gpu = TerrainGpu::new(device(), &map, &layers(), &config(), 4, None, None).expect("terrain gpu");
    let rule = ScatterRule {
        cell_size: 12.0,
        instances_per_cell: 2.0,
        height_min: 1.0,
        fade_start_distance: 400.0,
        fade_end_distance: 900.0,
        ..Default::default()
    };
    let instances = place(&map, None, None, &rule);
    gpu.set_scatter(&instances, rule.fade_start_distance, rule.fade_end_distance, 0.0);
    let held = frame(0);
    let cam_xz = [held.camera_pos[0], held.camera_pos[2]];
    let selection = select_nodes(&map, &pyramid(&map), &config(), 4, cam_xz, None);
    let first = render_offscreen(&mut gpu, device(), &held, &selection, W, H, CLEAR).expect("frame 1");
    assert_eq!(gpu.sun_shadow_renders(), 1);
    let mut last = Vec::new();
    for _ in 0..3 {
        last = render_offscreen(&mut gpu, device(), &held, &selection, W, H, CLEAR).expect("held frame");
    }
    assert_eq!(
        gpu.sun_shadow_renders(),
        1,
        "a held camera re-rendered the sun cascade"
    );
    assert_eq!(
        first, last,
        "the cached cascade shades differently from the rendered one"
    );
    assert_eq!(
        gpu.last_node_upload(),
        (0, 0),
        "a held camera re-uploaded node uniforms"
    );

    let mut moved_sun = held;
    moved_sun.sun_dir = [
        moved_sun.sun_dir[0] + 0.2,
        moved_sun.sun_dir[1],
        moved_sun.sun_dir[2],
    ];
    render_offscreen(&mut gpu, device(), &moved_sun, &selection, W, H, CLEAR).expect("new sun");
    assert_eq!(
        gpu.sun_shadow_renders(),
        2,
        "a new sun direction must re-render the cascade"
    );
    gpu.set_scatter(
        &instances[..instances.len() / 2],
        rule.fade_start_distance,
        rule.fade_end_distance,
        0.0,
    );
    render_offscreen(&mut gpu, device(), &moved_sun, &selection, W, H, CLEAR).expect("new casters");
    assert_eq!(
        gpu.sun_shadow_renders(),
        3,
        "new casters must re-render the cascade"
    );
}
