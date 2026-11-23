// SPDX-License-Identifier: MPL-2.0
//! Mirror reflection and far-side light of a studio environment against known answers
//! (`book/agents/studio-environment.md`, ADR-2000).
//!
//! The renderer's linear radiance is read back before the tonemap
//! (`FrameRenderer::request_hdr_capture`), so the numbers are scene-referred. The mirror's answer is the environment image itself; the diffuse answer is a small
//! deterministic path tracer of the same scene (a sphere on a plane under the same image).

use regia_render::forward_lite::ForwardLite;
use regia_render::{
    DebugViewMode, DirectionalLightUniform, EnvGroundPlane, EnvironmentAuthority, FrameQuality,
    FrameRenderer, FrameScene, HdrCapture, LabMaterial, LabMesh, LabScene, LocalCaptureDesc, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, StudioBackground, StudioEnvResolution, StudioEquirect,
    StudioGradient, box_mesh, translate_mesh, uv_sphere,
};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};

/// A renderer on which the installed environment is the sky and the image light of Desktop High's
/// frames too (`EnvironmentAuthority::Studio`, the opt-in of ADR-2000 (0), ADR-2016): these tests
/// measure that look. The default draws the physical atmosphere over it, as v0.2.8 did.
fn studio_renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_environment_authority(EnvironmentAuthority::Studio);
    renderer
}

const EV: f32 = 13.0;

/// A blue-grey sky over a sand-coloured lower half with a small hot disc at `sun` (unit vector).
/// Returns the radiance toward `d`.
fn studio_sky(d: [f32; 3], sun: [f32; 3], sun_cos: f32, sun_radiance: f32) -> [f32; 3] {
    let c = d[0] * sun[0] + d[1] * sun[1] + d[2] * sun[2];
    if c >= sun_cos {
        return [sun_radiance; 3];
    }
    if d[1] >= 0.0 {
        let t = d[1].clamp(0.0, 1.0);
        [900.0 + 500.0 * (1.0 - t), 1100.0 + 400.0 * (1.0 - t), 1600.0]
    } else {
        [1000.0, 850.0, 650.0]
    }
}

fn equirect_of(width: u32, f: &dyn Fn([f32; 3]) -> [f32; 3]) -> StudioEquirect {
    let height = width / 2;
    let mut texels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let u = (x as f32 + 0.5) / width as f32;
            let v = (y as f32 + 0.5) / height as f32;
            let phi = (u - 0.5) * std::f32::consts::TAU;
            let theta = v * std::f32::consts::PI;
            let (st, ct) = theta.sin_cos();
            texels.push(f([st * phi.cos(), ct, st * phi.sin()]));
        }
    }
    StudioEquirect::new(width, height, texels).expect("image")
}

/// The next frame's linear radiance (cd/m²), before the tonemap.
fn hdr_frame(device: &RenderDevice, renderer: &mut FrameRenderer, frame: &FrameScene<'_>) -> HdrCapture {
    renderer.render_offscreen(device, frame).expect("frame");
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, frame).expect("frame");
    renderer
        .take_hdr_capture(device)
        .expect("hdr read")
        .expect("a post stack frame")
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.ssr = false;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q
}

fn sphere_scene(
    size: u32,
    eye: [f32; 3],
    target: [f32; 3],
    fov_y: f32,
    material: LabMaterial,
    radius: f32,
    center: [f32; 3],
) -> LabScene {
    let (v, i) = uv_sphere(radius, 256, 128);
    LabScene {
        width: size,
        height: size,
        eye,
        target,
        fov_y,
        near: 0.1,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: EV,
            bloom: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![material],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, center),
            indices: i,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn mirror() -> LabMaterial {
    LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        metallic: 1.0,
        roughness: 0.0,
        ..LabMaterial::default()
    }
}

/// One device for the whole binary, used by one test at a time on a GPU (ADR-2113, ADR-7334,
/// ADR-7483). Its tests run on parallel threads: a test that created and dropped its own instance
/// and device changed the Vulkan loader's driver and device lists while another test's object
/// naming walked them (SIGSEGV in `loader_get_icd_and_device`, seen on the Intel Arc in the GPU
/// pre-gate), and on the shared device two tests compiling pipelines at once met the NVIDIA
/// driver's SPIR-V compiler fault (`libnvidia-glvkspirv` +0x363b2, RTX 5090, 615.71.09). The turn
/// lasts for the test; a test that panicked leaves it usable; a CPU adapter takes no turns.
fn device() -> Option<SerialGpu> {
    match serial_device() {
        Ok(d) => Some(d),
        Err(err) => {
            eprintln!("skip studio_measure: {err}");
            None
        }
    }
}

/// A hot disc the size of the sun (0.53°) behind the camera lands on the sphere's centre,
/// magnified by a long lens.
/// Prints the peak the mirror shows against the image's own radiance, in stops.
fn mirror_peak_ev(device: &RenderDevice, renderer: &mut FrameRenderer, quality: FrameQuality) -> (f32, f32) {
    let lab = sphere_scene(
        256,
        [0.0, 2.0, 20.0],
        [0.0, 2.0, 3.0],
        0.03,
        mirror(),
        3.0,
        [0.0, 2.0, 0.0],
    );
    let frame = lab.as_frame_scene_with_quality(quality);
    let hdr = hdr_frame(device, renderer, &frame);
    let peak = hdr.rgb.iter().map(|c| luminance(*c)).fold(0.0f32, f32::max);
    let mut sorted: Vec<f32> = hdr.rgb.iter().map(|c| luminance(*c)).collect();
    sorted.sort_by(f32::total_cmp);
    (peak, sorted[sorted.len() / 2])
}

#[test]
fn a_studio_base_keeps_a_small_hot_disc_in_a_mirror() {
    let Some(device) = device() else { return };
    let sun = [0.0, 0.0, 1.0];
    let sun_cos = 0.265f32.to_radians().cos();
    let sun_radiance = 2.0e6;
    let image = equirect_of(4096, &|d| studio_sky(d, sun, sun_cos, sun_radiance));
    let mut peaks = Vec::new();
    for resolution in [
        StudioEnvResolution::Standard,
        StudioEnvResolution::Studio,
        StudioEnvResolution::Source,
    ] {
        let mut renderer = studio_renderer();
        renderer.set_studio_env_resolution(resolution);
        assert!(renderer.set_studio_equirect(&device, Some(&image)));
        let info = renderer.studio_env_info().expect("installed");
        assert_eq!(info.source, Some((4096, 2048)));
        let (peak, median) = mirror_peak_ev(&device, &mut renderer, quality());
        let ev = (peak / sun_radiance).log2();
        eprintln!(
            "MEASURE mirror {resolution:?} base {}x{} ({} mips, {} MB): peak {peak:.0} cd/m2 vs image {sun_radiance:.0}: {ev:+.2} EV; median {median:.0}",
            info.base_width,
            info.base_height,
            info.mips,
            info.gpu_bytes / (1 << 20),
        );
        peaks.push(ev);
    }
    assert!(
        peaks[0] < -3.0,
        "the standard base loses the disc: {:+.2} EV",
        peaks[0]
    );
    // A 0.53° disc is 1.5 texels of a 1024 base: its peak averages with the sky around it.
    assert!(peaks[1] > -1.5, "the studio base keeps it: {:+.2} EV", peaks[1]);
    assert!(peaks[2] > -0.5, "the source base keeps it: {:+.2} EV", peaks[2]);
}

/// The physical atmosphere must not write over an installed environment: Desktop High and Ultra
/// frames bake it into the IBL maps every frame unless a studio environment is installed.
#[test]
fn a_studio_environment_survives_the_physical_atmosphere() {
    let Some(device) = device() else { return };
    let image = equirect_of(256, &|_| [800.0, 200.0, 100.0]);
    let lab = sphere_scene(
        128,
        SIDE_EYE,
        [0.0, 1.0, 0.0],
        SIDE_FOV,
        grey_sphere(),
        1.0,
        [0.0, 1.0, 0.0],
    );
    let mut with_atmosphere = quality();
    with_atmosphere.physical_atmosphere = true;
    let mut without = quality();
    without.physical_atmosphere = false;
    let mut renderer = studio_renderer();
    // A frame first, so the atmosphere has baked into the maps it owns.
    renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(with_atmosphere))
        .expect("frame");
    renderer.set_studio_equirect(&device, Some(&image));
    let on = hdr_frame(
        &device,
        &mut renderer,
        &lab.as_frame_scene_with_quality(with_atmosphere),
    );
    let off = hdr_frame(&device, &mut renderer, &lab.as_frame_scene_with_quality(without));
    let (on_sphere, on_corner) = sphere_and_corner(&on);
    let (off_sphere, off_corner) = sphere_and_corner(&off);
    eprintln!(
        "MEASURE atmosphere on: sphere {on_sphere:?} background {on_corner:?}; off: {off_sphere:?} {off_corner:?}"
    );
    // A uniform image: the sphere is albedo × radiance, and the background is the image.
    for c in 0..3 {
        assert!(
            (on_sphere[c] - off_sphere[c]).abs() < 0.03 * off_sphere[c],
            "{on_sphere:?} {off_sphere:?}"
        );
    }
    assert!((on_corner[0] - 800.0).abs() < 8.0, "{on_corner:?}");
    assert!(
        on_sphere[0] > 3.0 * on_sphere[2],
        "the image's colour lights it: {on_sphere:?}"
    );
}

/// The default authority keeps v0.2.8's frame (ADR-2016): with the physical atmosphere on, its
/// sky, not the installed image, is the background, and switching to the studio authority then
/// setting the same image again rebuilds it and shows the image.
#[test]
fn the_atmosphere_draws_over_the_environment_unless_the_studio_authority_is_set() {
    let Some(device) = device() else { return };
    let image = equirect_of(256, &|_| [800.0, 200.0, 100.0]);
    let lab = sphere_scene(
        128,
        SIDE_EYE,
        [0.0, 1.0, 0.0],
        SIDE_FOV,
        grey_sphere(),
        1.0,
        [0.0, 1.0, 0.0],
    );
    let mut with_atmosphere = quality();
    with_atmosphere.physical_atmosphere = true;
    let mut renderer = FrameRenderer::new();
    assert_eq!(renderer.environment_authority(), EnvironmentAuthority::Atmosphere);
    renderer
        .render_offscreen(&device, &lab.as_frame_scene_with_quality(with_atmosphere))
        .expect("frame");
    assert!(renderer.set_studio_equirect(&device, Some(&image)));
    let mixed = hdr_frame(
        &device,
        &mut renderer,
        &lab.as_frame_scene_with_quality(with_atmosphere),
    );
    let (_, mixed_corner) = sphere_and_corner(&mixed);
    eprintln!("MEASURE default authority, atmosphere on: background {mixed_corner:?}");
    assert!(
        (mixed_corner[0] - 800.0).abs() > 80.0,
        "the atmosphere's sky is the background by default: {mixed_corner:?}"
    );
    renderer.set_environment_authority(EnvironmentAuthority::Studio);
    assert!(
        renderer.set_studio_equirect(&device, Some(&image)),
        "maps the atmosphere wrote into rebuild after the switch"
    );
    assert!(
        !renderer.set_studio_equirect(&device, Some(&image)),
        "then they are kept"
    );
    let studio = hdr_frame(
        &device,
        &mut renderer,
        &lab.as_frame_scene_with_quality(with_atmosphere),
    );
    let (studio_sphere, studio_corner) = sphere_and_corner(&studio);
    eprintln!(
        "MEASURE studio authority after the switch: sphere {studio_sphere:?} background {studio_corner:?}"
    );
    assert!((studio_corner[0] - 800.0).abs() < 8.0, "{studio_corner:?}");
    assert!(
        studio_sphere[0] > 3.0 * studio_sphere[2],
        "the image's colour lights it: {studio_sphere:?}"
    );
}

#[test]
fn the_resolution_setting_rebuilds_and_the_read_reports_it() {
    let Some(device) = device() else { return };
    let image = equirect_of(2048, &|d| studio_sky(d, [0.0, 1.0, 0.0], 2.0, 0.0));
    let mut renderer = studio_renderer();
    assert!(renderer.set_studio_equirect(&device, Some(&image)));
    assert_eq!(renderer.environment_resolution(), Some((256, 128, 4)));
    assert!(!renderer.set_studio_equirect(&device, Some(&image)), "unchanged");
    renderer.set_studio_env_resolution(StudioEnvResolution::Studio);
    assert!(
        renderer.set_studio_equirect(&device, Some(&image)),
        "a new base rebuilds"
    );
    assert_eq!(renderer.environment_resolution(), Some((1024, 512, 6)));
    renderer.set_studio_env_budget(512);
    assert!(
        renderer.set_studio_equirect(&device, Some(&image)),
        "a lower budget rebuilds"
    );
    assert_eq!(renderer.environment_resolution(), Some((512, 256, 5)));
    let info = renderer.studio_env_info().expect("installed");
    assert_eq!(info.mips, 5);
    assert_eq!(info.gpu_bytes, (174_592 + 2560) * 8);
    // A gradient has no source; the two-tone sky has no record, and reports its maps.
    assert_eq!(
        renderer.set_studio_gradient(
            &device,
            Some(&StudioGradient::new([100.0; 3], [200.0; 3], [50.0; 3]))
        ),
        Ok(true)
    );
    assert_eq!(renderer.studio_env_info().and_then(|i| i.source), None);
    renderer.set_studio_sky(&device, Some(([100.0; 3], [50.0; 3])));
    assert_eq!(renderer.studio_env_info(), None);
    assert_eq!(renderer.environment_resolution(), Some((64, 32, 4)));
}

/// A small xorshift generator: the reference is deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32) / (1u64 << 24) as f32
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = dot(a, a).sqrt();
    [a[0] / l, a[1] / l, a[2] / l]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// A cosine-weighted direction around `n`.
fn cosine_dir(n: [f32; 3], rng: &mut Rng) -> [f32; 3] {
    let (u, v) = (rng.next(), rng.next());
    let (r, phi) = (u.sqrt(), std::f32::consts::TAU * v);
    let (s, c) = phi.sin_cos();
    let t = normalize(if n[0].abs() > 0.5 {
        cross(n, [0.0, 1.0, 0.0])
    } else {
        cross(n, [1.0, 0.0, 0.0])
    });
    let b = cross(n, t);
    let z = (1.0 - u).max(0.0).sqrt();
    let l = [r * c, r * s, z];
    normalize([
        t[0] * l[0] + b[0] * l[1] + n[0] * l[2],
        t[1] * l[0] + b[1] * l[1] + n[1] * l[2],
        t[2] * l[0] + b[2] * l[1] + n[2] * l[2],
    ])
}

/// The scene the reference traces: a diffuse sphere resting on a large diffuse plane.
struct Reference<'a> {
    env: &'a dyn Fn([f32; 3]) -> [f32; 3],
    radius: f32,
    sphere_albedo: f32,
    plane_albedo: f32,
    /// Plane radius around the origin (`f32::INFINITY`: unbounded).
    plane_radius: f32,
}

impl Reference<'_> {
    /// Radiance arriving at `o` along `d`, through up to `depth` diffuse bounces.
    fn radiance(&self, o: [f32; 3], d: [f32; 3], depth: u32, rng: &mut Rng) -> [f32; 3] {
        let c = [0.0, self.radius, 0.0];
        let oc = [o[0] - c[0], o[1] - c[1], o[2] - c[2]];
        let b = dot(oc, d);
        let disc = b * b - (dot(oc, oc) - self.radius * self.radius);
        let t_sphere = if disc > 0.0 {
            let t = -b - disc.sqrt();
            if t > 1e-4 { t } else { f32::INFINITY }
        } else {
            f32::INFINITY
        };
        let t_plane = if d[1] < -1e-6 && o[1] > 0.0 {
            let t = -o[1] / d[1];
            let (x, z) = (o[0] + d[0] * t, o[2] + d[2] * t);
            if x * x + z * z <= self.plane_radius * self.plane_radius {
                t
            } else {
                f32::INFINITY
            }
        } else {
            f32::INFINITY
        };
        if t_sphere.is_infinite() && t_plane.is_infinite() {
            return (self.env)(d);
        }
        if depth == 0 {
            return [0.0; 3];
        }
        let (t, albedo, n) = if t_sphere < t_plane {
            let h = [
                o[0] + d[0] * t_sphere,
                o[1] + d[1] * t_sphere,
                o[2] + d[2] * t_sphere,
            ];
            let n = normalize([h[0] - c[0], h[1] - c[1], h[2] - c[2]]);
            (t_sphere, self.sphere_albedo, n)
        } else {
            (t_plane, self.plane_albedo, [0.0, 1.0, 0.0])
        };
        let h = [
            o[0] + d[0] * t + n[0] * 1e-4,
            o[1] + d[1] * t + n[1] * 1e-4,
            o[2] + d[2] * t + n[2] * 1e-4,
        ];
        let l = cosine_dir(n, rng);
        self.radiance(h, l, depth - 1, rng).map(|v| v * albedo)
    }

    /// Outgoing radiance of the sphere's surface point with unit normal `n`.
    fn sphere_point(&self, n: [f32; 3], samples: u32, seed: u64) -> [f32; 3] {
        let mut rng = Rng(seed | 1);
        let p = [
            n[0] * (self.radius + 1e-3),
            self.radius + n[1] * (self.radius + 1e-3),
            n[2] * (self.radius + 1e-3),
        ];
        let mut sum = [0.0f32; 3];
        for _ in 0..samples {
            let l = cosine_dir(n, &mut rng);
            let e = self.radiance(p, l, 4, &mut rng);
            for (s, v) in sum.iter_mut().zip(e) {
                *s += v;
            }
        }
        sum.map(|v| v * self.sphere_albedo / samples as f32)
    }
}

/// The test's image: a sky, a bright lower half and a soft window on the +X side.
fn window_sky(d: [f32; 3]) -> [f32; 3] {
    let window = normalize([1.0, 0.5, 0.0]);
    let c = dot(d, window);
    let base = studio_sky(d, [0.0, 1.0, 0.0], 2.0, 0.0);
    if c > 0.85 { base.map(|v| v + 12000.0) } else { base }
}

const FAR_SIDE_NORMALS: [(&str, [f32; 3]); 5] = [
    ("far low", [-0.70, -0.50, 0.51]),
    ("far mid", [-0.85, 0.0, 0.527]),
    ("under", [0.0, -0.80, 0.60]),
    ("top", [0.0, 0.80, 0.60]),
    ("lit", [0.80, 0.20, 0.566]),
];

const SIDE_EYE: [f32; 3] = [0.0, 1.0, 7.0];
const SIDE_FOV: f32 = 0.5;

/// The pixel of the sphere point with normal `n` in the side view.
fn side_pixel(n: [f32; 3], radius: f32, size: u32) -> (u32, u32) {
    let p = [n[0] * radius, radius + n[1] * radius, n[2] * radius];
    let depth = SIDE_EYE[2] - p[2];
    let tan = (SIDE_FOV * 0.5).tan();
    let x = p[0] / depth / tan;
    let y = (p[1] - SIDE_EYE[1]) / depth / tan;
    (
        ((x * 0.5 + 0.5) * size as f32) as u32,
        ((0.5 - y * 0.5) * size as f32) as u32,
    )
}

fn grey_sphere() -> LabMaterial {
    LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        metallic: 0.0,
        roughness: 1.0,
        ..LabMaterial::default()
    }
}

fn ground_quad(half: f32) -> LabMesh {
    let vertex = |x: f32, z: f32| regia_render::LabVertex {
        position: [x, 0.0, z],
        normal: [0.0, 1.0, 0.0],
        uv: [0.0; 2],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    };
    LabMesh {
        vertices: vec![
            vertex(-half, -half),
            vertex(-half, half),
            vertex(half, half),
            vertex(half, -half),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 1,
        ..LabMesh::unskinned()
    }
}

/// The diffuse sphere under the image; `plane` adds the ground as a mesh.
fn far_side_ratios(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    plane: bool,
    label: &str,
) -> Vec<f32> {
    far_side_ratios_with(device, renderer, plane, label, quality(), 0)
}

/// [`far_side_ratios`] at `quality`, after `warm_frames` extra frames (the probe GI converges
/// over frames).
fn far_side_ratios_with(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    plane: bool,
    label: &str,
    quality: FrameQuality,
    warm_frames: u32,
) -> Vec<f32> {
    const SIZE: u32 = 512;
    let mut lab = sphere_scene(
        SIZE,
        SIDE_EYE,
        [0.0, 1.0, 0.0],
        SIDE_FOV,
        grey_sphere(),
        1.0,
        [0.0, 1.0, 0.0],
    );
    if plane {
        lab.materials.push(LabMaterial {
            base_color: [0.18, 0.18, 0.18, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..LabMaterial::default()
        });
        lab.meshes.push(ground_quad(80.0));
    }
    let frame = lab.as_frame_scene_with_quality(quality);
    for _ in 0..warm_frames {
        renderer.render_offscreen(device, &frame).expect("frame");
    }
    let hdr = hdr_frame(device, renderer, &frame);
    let reference = Reference {
        env: &window_sky,
        radius: 1.0,
        sphere_albedo: 0.5,
        plane_albedo: 0.18,
        plane_radius: 80.0,
    };
    let mut ratios = Vec::new();
    for (i, (name, n)) in FAR_SIDE_NORMALS.iter().enumerate() {
        let n = normalize(*n);
        let (x, y) = side_pixel(n, 1.0, SIZE);
        let mut got = 0.0;
        for dy in 0..3 {
            for dx in 0..3 {
                let idx = ((y + dy - 1) * hdr.width + x + dx - 1) as usize;
                got += luminance(hdr.rgb[idx]) / 9.0;
            }
        }
        let want = luminance(reference.sphere_point(n, 16_000, 0x9E37_79B9 + i as u64));
        let ratio = got / want;
        eprintln!("MEASURE {label} {name}: regia {got:.0} reference {want:.0} ratio {ratio:.2}");
        ratios.push(ratio);
    }
    ratios
}

fn plane() -> EnvGroundPlane {
    EnvGroundPlane {
        albedo: [0.18; 3],
        radius: Some(80.0),
        ..EnvGroundPlane::default()
    }
}

#[test]
fn the_ground_plane_brings_far_sides_to_the_reference() {
    let Some(device) = device() else { return };
    let image = equirect_of(1024, &window_sky);
    let mut renderer = studio_renderer();
    assert!(renderer.set_studio_equirect(&device, Some(&image)));
    let alone = far_side_ratios(&device, &mut renderer, false, "image alone");
    let mesh = far_side_ratios(&device, &mut renderer, true, "with plane mesh");
    renderer.set_environment_ground(Some(plane())).expect("plane");
    assert!(renderer.environment_ground_active());
    let ground = far_side_ratios(&device, &mut renderer, true, "with plane mesh and ground plane");
    let worst = |r: &[f32]| r.iter().map(|v| v.max(1.0 / v)).fold(0.0f32, f32::max);
    eprintln!(
        "MEASURE worst ratio: image alone {:.2}, plane mesh {:.2}, ground plane {:.2}",
        worst(&alone),
        worst(&mesh),
        worst(&ground)
    );
    assert!(worst(&ground) < worst(&alone), "the plane must help");
    assert!(worst(&ground) < 1.5, "within 1.5x of the reference: {:?}", ground);
}

/// The same far sides on Desktop High as shipped, with the probe GI on (ADR-2000 (6)): the
/// probes' irradiance replaces the image's diffuse term where they cover a surface, so the ground
/// plane has to reach it as well. Since ADR-5716 the plane is in the probes' own world (a probe ray
/// that misses every surface lands on it) and the forward pass no longer swaps it into their
/// irradiance by a form factor.
///
/// Until that ADR this test compared the plane mesh alone with the plane mesh under the ground
/// plane and asked the second to be 0.3 nearer the reference. That held while the probes did not
/// trace the mesh (a quad without thickness, which the brick baker turned into a brick of +∞;
/// lavapipe: 2.29 against 1.54). Where they did trace it, on NVIDIA cards, the mesh alone was near
/// the reference already and the swap counted the plane twice (1.19 against 1.39, the far low side
/// at 0.72 of its reference). The 0.3 is now asked where the plane is the only ground the probes
/// have, against the image alone; with a mesh under it, the plane must not move the far sides.
#[test]
fn measure_the_far_sides_with_the_probe_gi_on() {
    let Some(device) = device() else { return };
    let image = equirect_of(1024, &window_sky);
    let mut probes = quality();
    probes.gi_probes = true;
    // A renderer of its own for each frame: the probes of a scene that changed keep the old
    // light for many updates.
    let measure = |plane_mesh: bool, ground: bool, label: &str| {
        let mut renderer = studio_renderer();
        assert!(renderer.set_studio_equirect(&device, Some(&image)));
        if ground {
            renderer.set_environment_ground(Some(plane())).expect("plane");
        }
        far_side_ratios_with(&device, &mut renderer, plane_mesh, label, probes, 24)
    };
    let alone = measure(false, false, "probes on, image alone");
    let ground = measure(false, true, "probes on, ground plane");
    let mesh = measure(true, false, "probes on, plane mesh");
    let both = measure(true, true, "probes on, plane mesh and ground plane");
    for r in alone.iter().chain(&ground).chain(&mesh).chain(&both) {
        assert!(r.is_finite() && *r > 0.0, "a lit, finite far side: {r}");
    }
    let worst = |r: &[f32]| r.iter().map(|v| v.max(1.0 / v)).fold(0.0f32, f32::max);
    eprintln!(
        "MEASURE worst ratio, probes on: image alone {:.2}, ground plane {:.2}, plane mesh {:.2}, plane mesh and ground plane {:.2}",
        worst(&alone),
        worst(&ground),
        worst(&mesh),
        worst(&both)
    );
    // The plane reaches the probes' irradiance too: without that it changed nothing here
    // (2.40 and 2.41 under the sphere, 10-31).
    assert!(
        worst(&ground) < worst(&alone) - 0.3,
        "the ground plane must help with the probe GI on: {ground:?} vs {alone:?}"
    );
    // The probes trace the plane mesh: it stands for the ground as the plane does.
    assert!(
        worst(&mesh) < worst(&alone) - 0.3,
        "the probes see the plane mesh: {mesh:?} vs {alone:?}"
    );
    // The frame this test has always measured, the plane over the plane mesh, keeps that gain.
    assert!(
        worst(&both) < worst(&alone) - 0.3,
        "the ground plane over the plane mesh must help with the probe GI on: {both:?} vs {alone:?}"
    );
    // A ground the probes trace is not counted again by the plane over it (ADR-5716).
    for (i, (name, _)) in FAR_SIDE_NORMALS.iter().enumerate() {
        let moved = both[i] / mesh[i];
        assert!(
            (0.9..=1.1).contains(&moved),
            "{name}: the ground plane over the plane mesh moves the far side by {moved:.3} ({} against {})",
            both[i],
            mesh[i]
        );
    }
}

/// ADR-5716: a probe ray that meets no surface lands on the image light's ground plane, whether the
/// probe stands over the plane, on it or under it. The layer of probes at the plane's own height
/// carries most of the weight of every point in the first cell over the plane: while its downward
/// rays read the image's lower half, the plane did not reach the underside of anything standing
/// on it.
///
/// The image's lower half is warm (red over blue, 1000 : 650), its upper half and the plane's
/// radiance are not, and the plane has one radiance: the rays that land on it hold one value.
#[test]
fn a_probe_ray_that_meets_no_surface_lands_on_the_ground_plane() {
    let Some(device) = device() else { return };
    let image = equirect_of(1024, &window_sky);
    let mut probes = quality();
    probes.gi_probes = true;
    // Per probe layer (a metre under the plane, on it, one and two metres over it), three metres
    // from the sphere: how many of the probe's 64 rays read the image's lower half, and the value
    // most of its other missed rays share with how many hold it.
    let layers = |ground: bool| -> Vec<(i32, usize, [u32; 3], usize)> {
        let mut renderer = studio_renderer();
        assert!(renderer.set_studio_equirect(&device, Some(&image)));
        if ground {
            renderer.set_environment_ground(Some(plane())).expect("plane");
        }
        let lab = sphere_scene(
            128,
            SIDE_EYE,
            [0.0, 1.0, 0.0],
            SIDE_FOV,
            grey_sphere(),
            1.0,
            [0.0, 1.0, 0.0],
        );
        let frame = lab.as_frame_scene_with_quality(probes);
        // Desktop High re-traces a probe every eighth frame: three rounds.
        for _ in 0..24 {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        let rb = renderer
            .ddgi_read_back(&device)
            .unwrap_or_else(|err| panic!("read back: {err}"))
            .unwrap_or_else(|| panic!("the probes did not run"));
        [-1, 0, 1, 2]
            .into_iter()
            .map(|y| {
                let cell = [3, y, 2];
                let slot = rb.slot(0, cell);
                let (offset, tag) = rb.probe(slot);
                assert_eq!(tag[..3], cell, "the probe is in the finest cascade");
                assert!(offset[3] > 0.5 && tag[3] > 0, "probe {cell:?} is active and traced");
                let missed: Vec<[f32; 4]> = rb.rays[slot as usize * 64..slot as usize * 64 + 64]
                    .iter()
                    .copied()
                    .filter(|r| r[3] >= 1.0e8)
                    .collect();
                let warm = missed.iter().filter(|r| r[0] > r[2]).count();
                let mut values: Vec<[u32; 3]> = missed
                    .iter()
                    .filter(|r| r[0] <= r[2])
                    .map(|r| [r[0].to_bits(), r[1].to_bits(), r[2].to_bits()])
                    .collect();
                values.sort_unstable();
                let (mut best, mut best_count) = ([0u32; 3], 0usize);
                for run in values.chunk_by(|a, b| a == b) {
                    if run.len() > best_count {
                        (best, best_count) = (run[0], run.len());
                    }
                }
                eprintln!(
                    "MEASURE probe layer {y} ({}): {} of 64 rays missed, {warm} read the image's lower half, {best_count} share {:?}",
                    if ground { "ground plane" } else { "image alone" },
                    missed.len(),
                    best.map(f32::from_bits)
                );
                (y, warm, best, best_count)
            })
            .collect()
    };
    // The control: without a plane the downward rays read the image's lower half.
    for (y, warm, _, _) in layers(false) {
        assert!(warm >= 24, "layer {y}: {warm} rays read the image's lower half");
    }
    let with = layers(true);
    for (y, warm, value, count) in &with {
        // A ray within a degree of the horizon may pass the disc's edge 80 m away.
        assert!(
            *warm <= 2,
            "layer {y}: {warm} rays still read the image's lower half under the plane"
        );
        assert!(
            *count >= 24,
            "layer {y}: only {count} rays hold the plane's radiance"
        );
        assert_eq!(*value, with[0].2, "layer {y}: the plane has one radiance");
    }
    let radiance = with[0].2.map(f32::from_bits);
    assert!(
        radiance.iter().all(|c| c.is_finite() && *c > 0.0),
        "a lit plane: {radiance:?}"
    );
}

/// Mean linear radiance of the frame's top-left pixels (the background) and at the sphere's centre.
fn sphere_and_corner(hdr: &HdrCapture) -> ([f32; 3], [f32; 3]) {
    let at = |x: u32, y: u32| hdr.rgb[(y * hdr.width + x) as usize];
    (at(hdr.width / 2, hdr.height / 2), at(4, 4))
}

#[test]
fn the_ground_plane_leaves_the_sun_alone() {
    let Some(device) = device() else { return };
    // A black image: every pixel of the sphere is the sun's direct light.
    let image = equirect_of(256, &|_| [0.0; 3]);
    let mut lab = sphere_scene(
        128,
        SIDE_EYE,
        [0.0, 1.0, 0.0],
        SIDE_FOV,
        grey_sphere(),
        1.0,
        [0.0, 1.0, 0.0],
    );
    lab.directional = DirectionalLightUniform::new([0.0, -1.0, -1.0], [1.0; 3], 100_000.0);
    let frame = lab.as_frame_scene_with_quality(quality());
    let mut renderer = studio_renderer();
    assert!(renderer.set_studio_equirect(&device, Some(&image)));
    let before = hdr_frame(&device, &mut renderer, &frame);
    renderer
        .set_environment_ground(Some(EnvGroundPlane {
            sun_bounce: false,
            ..plane()
        }))
        .expect("plane");
    let after = hdr_frame(&device, &mut renderer, &frame);
    let worst = before
        .rgb
        .iter()
        .zip(&after.rgb)
        .map(|(a, b)| (luminance(*a) - luminance(*b)).abs())
        .fold(0.0f32, f32::max);
    let peak = before.rgb.iter().map(|c| luminance(*c)).fold(0.0f32, f32::max);
    eprintln!("MEASURE ground plane vs sun: worst change {worst:.3} of peak {peak:.0}");
    assert!(peak > 1000.0, "the sun lights the sphere: {peak}");
    assert!(worst <= peak * 1e-3, "worst change {worst} of {peak}");
}

#[test]
fn the_ground_plane_turns_a_mirrors_lower_half_into_the_plane() {
    let Some(device) = device() else { return };
    let image = equirect_of(512, &window_sky);
    let lab = sphere_scene(
        256,
        [0.0, 2.0, 9.0],
        [0.0, 2.0, 0.0],
        0.75,
        mirror(),
        2.0,
        [0.0, 2.0, 0.0],
    );
    let frame = lab.as_frame_scene_with_quality(quality());
    let mut renderer = studio_renderer();
    renderer.set_studio_equirect(&device, Some(&image));
    let before = hdr_frame(&device, &mut renderer, &frame);
    let albedo = [0.5, 0.25, 0.1];
    renderer
        .set_environment_ground(Some(EnvGroundPlane {
            albedo,
            sun_bounce: false,
            ..EnvGroundPlane::default()
        }))
        .expect("plane");
    let after = hdr_frame(&device, &mut renderer, &frame);
    // A row below the centre reflects the floor; a row above it the sky, unchanged.
    let at = |h: &HdrCapture, y: u32| h.rgb[(y * h.width + h.width / 2) as usize];
    let (low, high) = (178, 96);
    eprintln!(
        "MEASURE mirror floor row before {:?} after {:?}; sky row before {:?} after {:?}",
        at(&before, low),
        at(&after, low),
        at(&before, high),
        at(&after, high)
    );
    let floor = at(&after, low);
    // Warm like the albedo, unlike the image's own lower half.
    assert!(floor[0] / floor[2] > 3.0, "{floor:?}");
    // The plane shows albedo × the light it receives, one near-white feed in all channels.
    let feed = [floor[0] / albedo[0], floor[1] / albedo[1], floor[2] / albedo[2]];
    assert!(
        feed[1] > 0.8 * feed[0] && feed[1] < 1.3 * feed[2],
        "the floor is the albedo over the sky's light: {feed:?}"
    );
    let (a, b) = (luminance(at(&before, high)), luminance(at(&after, high)));
    assert!((a - b).abs() < 0.01 * a, "the sky reflection stays: {a} {b}");
}

/// The lite forward shader has no ground plane term: a frame with one draws the full shader, so
/// the plane looks the same on a tier that asks for the lite path.
#[test]
fn a_ground_plane_keeps_the_full_shader_on_a_lite_tier() {
    let Some(device) = device() else { return };
    let image = equirect_of(256, &window_sky);
    let lab = sphere_scene(
        128,
        SIDE_EYE,
        [0.0, 1.0, 0.0],
        SIDE_FOV,
        grey_sphere(),
        1.0,
        [0.0, 1.0, 0.0],
    );
    let frame = lab.as_frame_scene_with_quality(quality());
    let mut full = studio_renderer();
    full.set_studio_equirect(&device, Some(&image));
    full.set_environment_ground(Some(plane())).expect("plane");
    let want = hdr_frame(&device, &mut full, &frame);
    let mut renderer = studio_renderer();
    renderer.set_forward_lite(Some(ForwardLite::Low));
    renderer.set_studio_equirect(&device, Some(&image));
    assert_eq!(renderer.frame_forward_tier(ForwardLite::Off), ForwardLite::Low);
    let lite = hdr_frame(&device, &mut renderer, &frame);
    renderer.set_environment_ground(Some(plane())).expect("plane");
    assert_eq!(renderer.frame_forward_tier(ForwardLite::Low), ForwardLite::Off);
    let got = hdr_frame(&device, &mut renderer, &frame);
    let worst = |a: &HdrCapture, b: &HdrCapture| {
        a.rgb
            .iter()
            .zip(&b.rgb)
            .map(|(p, q)| (luminance(*p) - luminance(*q)).abs())
            .fold(0.0f32, f32::max)
    };
    eprintln!(
        "MEASURE lite tier vs full with plane: lite path {:.2}, gated {:.2}",
        worst(&lite, &want),
        worst(&got, &want)
    );
    assert!(worst(&lite, &want) > 1.0, "the lite path shows no plane");
    assert!(
        worst(&got, &want) < 1.0,
        "the plane's frame draws the full shader"
    );
}

#[test]
fn the_debug_views_split_off_the_image_light() {
    let Some(device) = device() else { return };
    let image = equirect_of(256, &|d| studio_sky(d, [0.0, 1.0, 0.0], 2.0, 0.0));
    let mut lab = sphere_scene(
        128,
        SIDE_EYE,
        [0.0, 1.0, 0.0],
        SIDE_FOV,
        LabMaterial {
            emissive: [0.5, 0.0, 0.0],
            emissive_intensity: 50.0,
            ..grey_sphere()
        },
        1.0,
        [0.0, 1.0, 0.0],
    );
    lab.directional = DirectionalLightUniform::new([0.0, -1.0, -1.0], [1.0; 3], 100_000.0);
    let frame = lab.as_frame_scene_with_quality(quality());
    let mut renderer = studio_renderer();
    renderer.set_studio_equirect(&device, Some(&image));
    renderer.set_environment_ground(Some(plane())).expect("plane");
    let lit = hdr_frame(&device, &mut renderer, &frame);
    renderer.set_image_light_view(DebugViewMode::ImageLight);
    let light = hdr_frame(&device, &mut renderer, &frame);
    renderer.set_image_light_view(DebugViewMode::ImageLightOcclusion);
    let occlusion = hdr_frame(&device, &mut renderer, &frame);
    renderer.set_image_light_view(DebugViewMode::Lit);
    let back = hdr_frame(&device, &mut renderer, &frame);
    let (lit_c, _) = sphere_and_corner(&lit);
    let (light_c, _) = sphere_and_corner(&light);
    eprintln!("MEASURE debug centre lit {lit_c:?} image light {light_c:?}");
    assert!(luminance(light_c) > 0.0, "the image light reaches the sphere");
    assert!(
        luminance(light_c) < 0.5 * luminance(lit_c),
        "the view drops the sun and the emission"
    );
    assert!(
        (light_c[0] / light_c[2] - 1.0).abs() < 0.6,
        "no red emission in the view: {light_c:?}"
    );
    // Occlusion, over the exposure: the plane's share (blue) grows toward the underside.
    let exposure = regia_render::exposure_from_ev100(EV);
    let channel = |n: [f32; 3], c: usize| {
        let (x, y) = side_pixel(normalize(n), 1.0, 128);
        occlusion.rgb[(y * occlusion.width + x) as usize][c] * exposure
    };
    let (top, under) = (channel([0.0, 0.8, 0.6], 2), channel([0.0, -0.8, 0.6], 2));
    eprintln!("MEASURE occlusion plane share top {top:.3} under {under:.3}");
    // An unbounded plane fills (1 − n.y) / 2 of the hemisphere; this one is 80 m wide.
    assert!((top - 0.1).abs() < 0.03, "{top}");
    assert!((under - 0.9).abs() < 0.05, "{under}");
    let worst = lit
        .rgb
        .iter()
        .zip(&back.rgb)
        .map(|(a, b)| (luminance(*a) - luminance(*b)).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1.0, "lit comes back unchanged: {worst}");
}

#[test]
fn the_background_changes_the_background_and_nothing_else() {
    let Some(device) = device() else { return };
    let light = equirect_of(256, &window_sky);
    let second = equirect_of(64, &|_| [10.0, 500.0, 20.0]);
    let lab = sphere_scene(
        128,
        SIDE_EYE,
        [0.0, 1.0, 0.0],
        SIDE_FOV,
        LabMaterial {
            metallic: 1.0,
            roughness: 0.2,
            ..grey_sphere()
        },
        1.0,
        [0.0, 1.0, 0.0],
    );
    // Screen-space GI and the reflections on: nothing the frame derives may read the background.
    let mut q = quality();
    q.screen_space_gi = true;
    q.ssr = true;
    let frame = lab.as_frame_scene_with_quality(q);
    let mut renderer = studio_renderer();
    renderer.set_studio_equirect(&device, Some(&light));
    let shown = hdr_frame(&device, &mut renderer, &frame);
    let sphere_of = |h: &HdrCapture| sphere_and_corner(h).0;
    let reference = sphere_of(&shown);
    for (name, background, want) in [
        (
            "solid",
            StudioBackground::Solid([300.0, 40.0, 40.0]),
            [300.0, 40.0, 40.0],
        ),
        (
            "gradient",
            StudioBackground::Gradient(StudioGradient::new(
                [40.0, 300.0, 40.0],
                [40.0, 300.0, 40.0],
                [0.0; 3],
            )),
            [40.0, 300.0, 40.0],
        ),
        ("image", StudioBackground::Image(second), [10.0, 500.0, 20.0]),
    ] {
        renderer
            .set_studio_background(&device, background)
            .expect("background");
        assert_eq!(renderer.studio_background().name(), name);
        // A few frames, so a temporal or screen-space path could pick the background up.
        for _ in 0..3 {
            renderer.render_offscreen(&device, &frame).expect("frame");
        }
        let hdr = hdr_frame(&device, &mut renderer, &frame);
        let (sphere, corner) = sphere_and_corner(&hdr);
        eprintln!("MEASURE background {name}: corner {corner:?} sphere {sphere:?} vs {reference:?}");
        assert!(
            (0..3).all(|c| (corner[c] - want[c]).abs() <= 0.1 * want[c].max(50.0)),
            "{name}: {corner:?}"
        );
        assert!(
            (0..3).all(|c| (sphere[c] - reference[c]).abs() <= 0.01 * reference[c].max(1.0)),
            "{name}: the sphere moved: {sphere:?} vs {reference:?}"
        );
    }
    assert!(
        renderer
            .set_studio_background(&device, StudioBackground::Solid([f32::NAN, 0.0, 0.0]))
            .is_err()
    );
    renderer
        .set_studio_background(&device, StudioBackground::Environment)
        .expect("back");
    let hdr = hdr_frame(&device, &mut renderer, &frame);
    let (_, corner) = sphere_and_corner(&hdr);
    let (_, original) = sphere_and_corner(&shown);
    assert!(
        (0..3).all(|c| (corner[c] - original[c]).abs() <= 0.02 * original[c]),
        "the environment shows again: {corner:?} vs {original:?}"
    );
}

/// A red plinth beside a mirror ball under a plain grey environment, with no GI of any kind. The
/// plinth is diffuse and not emissive: an emissive one reaches metal through its fitted area light
/// (`emissive_lights.rs`) with no capture at all.
fn plinth_scene(profile: RenderProfilePreset) -> (LabScene, FrameQuality) {
    let mut lab = sphere_scene(
        256,
        [0.0, 1.0, 7.0],
        [0.0, 1.0, 0.0],
        0.6,
        mirror(),
        1.0,
        [0.0, 1.0, 0.0],
    );
    let (v, i) = box_mesh([0.6, 0.6, 0.6]);
    lab.meshes.push(LabMesh {
        vertices: translate_mesh(&v, [2.2, 0.6, 0.0]),
        indices: i,
        material: 1,
        ..LabMesh::unskinned()
    });
    lab.materials.push(LabMaterial {
        base_color: [1.0, 0.02, 0.02, 1.0],
        metallic: 0.0,
        roughness: 0.8,
        ..LabMaterial::default()
    });
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(profile));
    q.taau = false;
    q.ssr = false;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.reflection_probes = false;
    (lab, q)
}

/// How red the mirror's right side reads, as red over blue, and the mean red there.
fn right_side_red(hdr: &HdrCapture) -> (f32, f32) {
    let mut peak = 0.0f32;
    // Inside the sphere's silhouette (58 pixels across its radius): the plinth also stands in frame.
    for y in hdr.height / 2 - 48..hdr.height / 2 + 48 {
        for x in hdr.width / 2 + 4..hdr.width / 2 + 52 {
            let c = hdr.rgb[(y * hdr.width + x) as usize];
            peak = peak.max(c[0] - c[2]);
        }
    }
    (
        peak,
        hdr.rgb[(hdr.height / 2 * hdr.width + hdr.width / 2) as usize][2],
    )
}

#[test]
fn a_local_capture_shows_a_nearby_object_in_metal_without_a_probe_volume() {
    let Some(device) = device() else { return };
    let image = equirect_of(256, &|_| [800.0; 3]);
    for preset in [RenderProfilePreset::DesktopHigh, RenderProfilePreset::DesktopLow] {
        let (lab, q) = plinth_scene(preset);
        let frame = lab.as_frame_scene_with_quality(q);
        let mut renderer = studio_renderer();
        renderer.set_studio_equirect(&device, Some(&image));
        let before = hdr_frame(&device, &mut renderer, &frame);
        let (red_before, _) = right_side_red(&before);
        renderer
            .place_local_capture(LocalCaptureDesc::new(1, [0.0, 1.0, 0.0], [4.0, 3.0, 4.0]))
            .expect("place");
        let unbaked = hdr_frame(&device, &mut renderer, &frame);
        assert_eq!(right_side_red(&unbaked).0, red_before, "nothing until a bake");
        let info = renderer.bake_local_capture(&device, &frame, 1).expect("bake");
        assert!(info.baked && info.bakes == 1);
        assert!(
            info.coverage > 0.02 && info.coverage < 0.5,
            "it saw the plinth and not much else: {}",
            info.coverage
        );
        let after = hdr_frame(&device, &mut renderer, &frame);
        let (red_after, sky) = right_side_red(&after);
        eprintln!(
            "MEASURE capture {preset:?}: red {red_before:.0} -> {red_after:.0}, coverage {:.3}, sky {sky:.0}",
            info.coverage
        );
        assert!(red_before < 50.0, "no red before: {red_before}");
        assert!(red_after > 500.0, "the plinth shows in the metal: {red_after}");
        assert!(
            (sky - 800.0).abs() < 80.0,
            "the rest keeps the environment: {sky}"
        );
        // Weight 0 returns to the environment.
        renderer
            .place_local_capture(LocalCaptureDesc {
                weight: 0.0,
                ..LocalCaptureDesc::new(1, [0.0, 1.0, 0.0], [4.0, 3.0, 4.0])
            })
            .expect("move");
        let off = hdr_frame(&device, &mut renderer, &frame);
        assert!(right_side_red(&off).0 < 50.0);
    }
}

#[test]
fn box_projection_moves_a_reflection_with_the_surface() {
    let Some(device) = device() else { return };
    let image = equirect_of(256, &|_| [800.0; 3]);
    let (lab, q) = plinth_scene(RenderProfilePreset::DesktopHigh);
    let frame = lab.as_frame_scene_with_quality(q);
    let mean_x = |hdr: &HdrCapture| {
        let (mut sum, mut weight) = (0.0f32, 0.0f32);
        for y in 0..hdr.height {
            for x in hdr.width / 2..hdr.width / 2 + 52 {
                let c = hdr.rgb[(y * hdr.width + x) as usize];
                let red = (c[0] - c[2]).max(0.0);
                sum += red * x as f32;
                weight += red;
            }
        }
        sum / weight.max(1e-3)
    };
    let mut xs = Vec::new();
    for half in [3.0, 30.0] {
        let mut renderer = studio_renderer();
        renderer.set_studio_equirect(&device, Some(&image));
        renderer
            .place_local_capture(LocalCaptureDesc::new(1, [0.0, 1.0, 0.0], [half; 3]))
            .expect("place");
        renderer.bake_local_capture(&device, &frame, 1).expect("bake");
        xs.push(mean_x(&hdr_frame(&device, &mut renderer, &frame)));
    }
    eprintln!(
        "MEASURE box projection: red centre x {:.1} (3 m box) {:.1} (30 m box)",
        xs[0], xs[1]
    );
    assert!(
        (xs[0] - xs[1]).abs() > 1.0,
        "the box must bend the lookup: {xs:?}"
    );
}

#[test]
fn bakes_in_any_order_keep_their_layers() {
    let Some(device) = device() else { return };
    let image = equirect_of(256, &|_| [800.0; 3]);
    let (lab, q) = plinth_scene(RenderProfilePreset::DesktopHigh);
    let frame = lab.as_frame_scene_with_quality(q);
    let mut renderer = studio_renderer();
    renderer.set_studio_equirect(&device, Some(&image));
    renderer
        .place_local_capture(LocalCaptureDesc::new(1, [0.0, 1.0, 0.0], [4.0, 3.0, 4.0]))
        .expect("place");
    // A far capture that sees the plinth differently, baked first.
    renderer
        .place_local_capture(LocalCaptureDesc::new(2, [100.0, 1.0, 0.0], [1.0, 1.0, 1.0]))
        .expect("place");
    renderer.bake_local_capture(&device, &frame, 2).expect("bake");
    renderer.bake_local_capture(&device, &frame, 1).expect("bake");
    let hdr = hdr_frame(&device, &mut renderer, &frame);
    assert!(right_side_red(&hdr).0 > 500.0, "capture 1 reads its own layer");
    assert_eq!(renderer.request_local_capture_bake(None), Ok(2));
    assert_eq!(
        renderer
            .bake_pending_local_captures(&device, &frame)
            .expect("bake"),
        2
    );
    assert!(
        renderer
            .local_captures()
            .iter()
            .all(|c| c.baked && !c.pending && c.bakes == 2)
    );
    // `bake_pending_local_captures` ran the frame's selection (ADR-2860): a layer holds a selected
    // capture by id, not by slot, so a removal moves nothing, and nothing bakes or uploads again.
    let uploads = |renderer: &FrameRenderer| renderer.local_captures()[0].uploads;
    assert!(renderer.remove_local_capture(1));
    let before = uploads(&renderer);
    let left = renderer.local_captures();
    assert!(
        left.len() == 1 && !left[0].pending && left[0].baked && left[0].layer.is_some(),
        "the other capture keeps its layer and its map: {left:?}"
    );
    assert_eq!(
        renderer
            .bake_pending_local_captures(&device, &frame)
            .expect("no bake"),
        0,
        "nothing bakes again"
    );
    assert_eq!(uploads(&renderer), before, "nothing uploads again");
}

/// ADR-2000's contract for a host that never runs the selection (slot `i` is layer `i`): a removal
/// moves the later slots down a layer, so what was in a layer bakes again.
#[test]
fn before_the_selection_a_removal_bakes_the_others_again() {
    let Some(device) = device() else { return };
    let (lab, q) = plinth_scene(RenderProfilePreset::DesktopHigh);
    let frame = lab.as_frame_scene_with_quality(q);
    let mut renderer = studio_renderer();
    for (id, x) in [(1, 0.0), (2, 100.0)] {
        renderer
            .place_local_capture(LocalCaptureDesc::new(id, [x, 1.0, 0.0], [4.0, 3.0, 4.0]))
            .expect("place");
        renderer.bake_local_capture(&device, &frame, id).expect("bake");
    }
    assert!(renderer.remove_local_capture(1));
    let left = renderer.local_captures();
    assert!(
        left.len() == 1 && left[0].pending,
        "the other capture bakes again"
    );
}
