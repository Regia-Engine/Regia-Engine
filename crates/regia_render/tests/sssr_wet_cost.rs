// SPDX-License-Identifier: MPL-2.0
//! Stochastic SSR on wet ground (ADR-2557; card-costs flag 4: wet surfaces cost +0.40 ms on the
//! DO27 chase, of which SSR is +0.30): the cost probe's counters, the wet ground scene that shows
//! the trace blow up on the base, the opt-in cost bound that caps it, and the look before and after.
//!
//! Counts, not times: the probe counts on the GPU, so lavapipe reads the numbers a card would for
//! the same frame. Its own test binary: the probe and the bound are process-wide knobs.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SSSR_MAX_STEPS, SssrCostBound,
    SssrCostCounters, SssrStats, SurfaceWeatherFrame, WeatherResponse,
};

const W: u32 = 256;
const H: u32 = 144;
const EV: f32 = 13.0;
/// A chase camera: 1.6 m over the road, looking down it.
const EYE: [f32; 3] = [0.0, 1.6, 6.0];
const TARGET: [f32; 3] = [0.0, 0.6, -12.0];
/// Frames per run: past the readback delay, before a held view would freeze (the camera moves).
const FRAMES: u32 = 10;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Serialises the GPU and the process-wide knobs across this binary's tests.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A flat quad over `x0..x1` × `z0..z1` at height `y`.
fn ground(x: [f32; 2], z: [f32; 2], y: f32, material: usize) -> LabMesh {
    let n = [0.0, 1.0, 0.0];
    let corners = [[x[0], y, z[0]], [x[1], y, z[0]], [x[1], y, z[1]], [x[0], y, z[1]]];
    LabMesh {
        vertices: corners
            .into_iter()
            .zip([[0.0, 0.0], [8.0, 0.0], [8.0, 8.0], [0.0, 8.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
        material,
        ..LabMesh::unskinned()
    }
}

fn block(size: [f32; 3], at: [f32; 3], material: usize) -> LabMesh {
    let (v, i) = regia_render::box_mesh(size);
    LabMesh {
        vertices: regia_render::translate_mesh(&v, at),
        indices: i,
        material,
        ..LabMesh::unskinned()
    }
}

/// Concrete ground (roughness 0.9, porous: dry, nothing on it traces; wet, about 0.46), a sealed
/// asphalt lane down the middle (roughness 0.6, porosity 0.15: wet, about 0.16), a painted car
/// ahead (roughness 0.3, so the pass runs on a dry frame too) and buildings on both sides for the
/// wet ground to mirror.
fn road() -> LabScene {
    let post = PostSettings {
        exposure_ev: EV,
        ..PostSettings::golden_deterministic()
    };
    let mut meshes = vec![
        ground([-200.0, 200.0], [-200.0, 200.0], 0.0, 0),
        ground([-3.0, 3.0], [-200.0, 20.0], 0.005, 3),
        block([0.9, 0.6, 2.0], [0.0, 0.6, -4.0], 1),
    ];
    for k in 0..6 {
        let z = -6.0 - 9.0 * k as f32;
        let h = 3.0 + (k % 3) as f32 * 2.0;
        meshes.push(block([2.0, h, 3.5], [-7.0, h, z], 2));
        meshes.push(block([2.0, h + 1.0, 3.5], [7.0, h + 1.0, z - 4.0], 2));
    }
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 400.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.3, -0.5, -0.8], [1.0, 0.97, 0.92], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.45, 0.45, 0.45, 1.0],
                roughness: 0.9,
                weather: WeatherResponse {
                    puddles: true,
                    ..WeatherResponse::DEFAULT
                },
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.6, 0.05, 0.04, 1.0],
                roughness: 0.3,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.7, 0.65, 0.55, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.12, 0.12, 0.13, 1.0],
                roughness: 0.6,
                weather: WeatherResponse {
                    porosity: Some(0.15),
                    puddles: true,
                    ..WeatherResponse::DEFAULT
                },
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

/// Desktop High at native scale, without clouds and fog.
fn desktop_high() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.render_scale_permille = 1000;
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q
}

/// The DO27 wet probe's surface state: a full film with the hollows half full.
const WET: SurfaceWeatherFrame = SurfaceWeatherFrame {
    wetness: 1.0,
    puddles: 0.5,
    ..SurfaceWeatherFrame::DRY
};

struct Run {
    stats: SssrStats,
    hdr: Vec<[f32; 3]>,
}

impl Run {
    fn cost(&self) -> SssrCostCounters {
        match self.stats.cost {
            Some(cost) => cost,
            None => panic!(
                "the probe was on, but no cost counters were read back: {:?}",
                self.stats
            ),
        }
    }
}

/// `REGIA_SSR_WET_DUMP=<dir>` writes the last frame of every run there (`<label>.png`) for review by
/// eye.
fn dump(label: &str, ldr: &regia_render::ForwardResult) {
    let Some(dir) = regia_core::brand::env_var_os("REGIA_SSR_WET_DUMP") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump dir");
    let name: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("dump file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), ldr.width, ldr.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&ldr.pixels))
        .expect("dump png");
}

/// `FRAMES` frames of the road with the camera driving forward (0.15 m a frame, so no held-view
/// reuse applies), with `weather`, the cost `bound` and the probe as given. Returns the newest
/// counters read back and the last frame's linear HDR.
fn drive(
    device: &RenderDevice,
    weather: SurfaceWeatherFrame,
    bound: Option<SssrCostBound>,
    probe: bool,
) -> Run {
    drive_labeled(device, weather, bound, probe, None)
}

/// [`drive`], dumping the last frame as `label` (see [`dump`]).
fn drive_labeled(
    device: &RenderDevice,
    weather: SurfaceWeatherFrame,
    bound: Option<SssrCostBound>,
    probe: bool,
    label: Option<&str>,
) -> Run {
    regia_render::set_sssr_cost_bound(bound);
    regia_render::set_sssr_cost_probe(probe);
    let lab = road();
    let mut frame = lab.as_frame_scene_with_quality(desktop_high());
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(weather);
    for i in 0..FRAMES {
        frame.eye[2] = EYE[2] - 0.15 * i as f32;
        frame.target[2] = TARGET[2] - 0.15 * i as f32;
        if i + 1 == FRAMES {
            renderer.request_hdr_capture();
        }
        let ldr = renderer.render_offscreen(device, &frame).expect("frame");
        if i + 1 == FRAMES
            && let Some(label) = label
        {
            dump(label, &ldr);
        }
    }
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("the post stack ran");
    let stats = renderer
        .sssr_stats()
        .expect("the pass ran and its counters came back");
    regia_render::set_sssr_cost_probe(false);
    regia_render::set_sssr_cost_bound(None);
    Run { stats, hdr: hdr.rgb }
}

fn flip(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    let (report, _) =
        regia_render::compare::compare_linear(a, b, W, H, EV, regia_render::compare::DEFAULT_PPD);
    report.flip_mean
}

fn line(label: &str, c: &SssrCostCounters) -> String {
    format!(
        "{label:<26} rays {:>6} steps {:>8} ({:>5.1}/ray, max {:>2}) exhausted {:>5} half-res {:>5} \
         hits {:>5} shared {:>5} variance {:>5} dropped {:>5} | mirror {:>5} glossy {:>5} skipped {:>5}",
        c.rays,
        c.steps,
        c.steps_per_ray(),
        c.max_steps,
        c.exhausted_rays,
        c.half_res_rays,
        c.hits,
        c.shared_pixels,
        c.variance_rays,
        c.budget_dropped_rays,
        c.mirror_pixels,
        c.glossy_pixels,
        c.skipped_pixels,
    )
}

/// The probe counts every pixel of the frame into one class, its trace counts agree with each
/// other, and it changes no pixel: the frame with the probe is the frame without it, bit for bit.
#[test]
fn the_cost_probe_counts_every_pixel_and_changes_nothing() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    for weather in [SurfaceWeatherFrame::DRY, WET] {
        let probed = drive(&device, weather, None, true);
        let plain = drive(&device, weather, None, false);
        let c = probed.cost();
        eprintln!("{}", line(if weather.is_dry() { "dry" } else { "wet" }, &c));
        assert_eq!(
            c.classified_pixels(),
            u64::from(W * H),
            "every pixel classified once"
        );
        assert!(c.rays > 0, "the car traces");
        assert!(c.steps >= c.rays, "every ray takes a step");
        assert!(c.hits <= c.rays && c.exhausted_rays <= c.rays && c.half_res_rays <= c.rays);
        assert!(c.max_steps <= SSSR_MAX_STEPS);
        assert!(probed.stats.ray_quads > 0 && c.rays >= probed.stats.ray_quads);
        assert_eq!(c.budget_dropped_rays, 0, "no bound, nothing dropped");
        assert_eq!(plain.stats.cost, None, "probe off: no counters");
        let differ = probed.hdr.iter().zip(&plain.hdr).filter(|(a, b)| a != b).count();
        assert_eq!(differ, 0, "the probe changed {differ} pixels");
    }
}

/// The base trace on wet ground: the film takes the road from roughness 0.9 (skipped) to about
/// 0.16 (glossy), so the frame traces many times the rays and the steps of the dry frame.
#[test]
fn wet_ground_multiplies_the_trace_on_the_base() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let dry = drive(&device, SurfaceWeatherFrame::DRY, None, true).cost();
    let wet = drive(&device, WET, None, true).cost();
    eprintln!("{}", line("dry, base", &dry));
    eprintln!("{}", line("wet, base", &wet));
    let rays = wet.rays as f32 / dry.rays.max(1) as f32;
    let steps = wet.steps as f32 / dry.steps.max(1) as f32;
    eprintln!("wet / dry: rays ×{rays:.1}, steps ×{steps:.1}");
    assert!(
        wet.glossy_pixels > 4 * dry.glossy_pixels,
        "the film makes the road glossy"
    );
    assert!(rays >= 4.0, "wet rays ×{rays:.2}");
    assert!(steps >= 4.0, "wet steps ×{steps:.2}");
}

/// The cost bound on the same wet frame, each part alone and all of [`SssrCostBound::WET`]: the
/// counters say what each part buys. The full bound traces at most `tile_ray_budget` rays per
/// glossy tile (by construction) and takes far fewer steps than the base.
#[test]
fn the_wet_bound_caps_rays_and_steps() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let base = drive(&device, WET, None, true).cost();
    eprintln!("{}", line("wet, base", &base));
    // Each part alone: everything else as the base.
    let none = SssrCostBound {
        variance_rays: true,
        tile_ray_budget: 0,
        min_steps: 0,
        step_falloff_depth_m: 0.0,
        half_res_roughness: regia_render::SSSR_HALF_RES_ROUGHNESS,
        block_roughness: 1.0,
    };
    let wet = SssrCostBound::WET;
    let parts = [
        (
            "no variance rays",
            SssrCostBound {
                variance_rays: false,
                ..none
            },
        ),
        (
            "tile budget 16",
            SssrCostBound {
                tile_ray_budget: wet.tile_ray_budget,
                ..none
            },
        ),
        (
            "steps by roughness (32)",
            SssrCostBound {
                min_steps: 32,
                ..none
            },
        ),
        (
            "steps by distance (30 m)",
            SssrCostBound {
                step_falloff_depth_m: 30.0,
                ..none
            },
        ),
        (
            "block rate from 0.3",
            SssrCostBound {
                block_roughness: 0.3,
                ..none
            }
            .sanitized(),
        ),
        (
            "block rate from 0.1",
            SssrCostBound {
                block_roughness: wet.block_roughness,
                ..none
            },
        ),
        (
            "half-res from 0.1",
            SssrCostBound {
                half_res_roughness: 0.1,
                ..none
            },
        ),
    ];
    // A bound with every part off is the base trace through the bound's code path (only the quad
    // sharing differs, where a quad's even-even pixel traces nothing: within 3 %).
    let neutral = drive(&device, WET, Some(none), true).cost();
    eprintln!("{}", line("bound, every part off", &neutral));
    let near = |a: u32, b: u32| (f64::from(a) - f64::from(b)).abs() <= 0.03 * f64::from(b.max(1));
    assert!(
        near(neutral.rays, base.rays),
        "a neutral bound traces the base rays"
    );
    assert!(near(neutral.steps, base.steps), "and takes the base steps");
    for (label, bound) in parts {
        let c = drive(&device, WET, Some(bound), true).cost();
        eprintln!("{}", line(label, &c));
        assert!(
            c.rays <= base.rays && c.steps <= base.steps,
            "{label} costs more than the base"
        );
    }
    let bounded = drive(&device, WET, Some(wet), true);
    let c = bounded.cost();
    eprintln!("{}", line("WET (all parts)", &c));
    let tiles = bounded.stats.tiles;
    assert!(
        c.rays <= wet.tile_ray_budget * tiles,
        "{} rays over {tiles} glossy tiles exceed the budget of {} per tile",
        c.rays,
        wet.tile_ray_budget
    );
    assert_eq!(c.variance_rays, 0, "no variance-guided rays under the bound");
    let steps = c.steps as f32 / base.steps.max(1) as f32;
    let rays = c.rays as f32 / base.rays.max(1) as f32;
    eprintln!("WET / base: rays ×{rays:.2}, steps ×{steps:.2}");
    assert!(rays <= 0.5, "the bound keeps {rays:.2} of the base rays");
    assert!(steps <= 0.5, "the bound keeps {steps:.2} of the base steps");
    assert_eq!(
        c.classified_pixels(),
        base.classified_pixels(),
        "the bound changes what is traced, not what is classified"
    );
}

/// The look: the wet road with the bound against the base after the same drive, by FLIP on the
/// linear HDR (stated in the lane report), and the dry frame likewise. Each part of the bound alone
/// is printed too, and against the frame without any reflection pass for scale.
#[test]
fn the_bound_keeps_the_wet_look() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let wet = SssrCostBound::WET;
    let none = SssrCostBound {
        variance_rays: true,
        tile_ray_budget: 0,
        min_steps: 0,
        step_falloff_depth_m: 0.0,
        half_res_roughness: regia_render::SSSR_HALF_RES_ROUGHNESS,
        block_roughness: 1.0,
    };
    for (label, weather, limit) in [("wet", WET, 0.02f32), ("dry", SurfaceWeatherFrame::DRY, 0.02)] {
        let base = drive_labeled(&device, weather, None, false, Some(&format!("{label}_base")));
        let bounded = drive_labeled(
            &device,
            weather,
            Some(wet),
            false,
            Some(&format!("{label}_bound_wet")),
        );
        let mean = flip(&base.hdr, &bounded.hdr);
        // The pass's own run-to-run spread on the base, for scale.
        let again = drive(&device, weather, None, false);
        let spread = flip(&base.hdr, &again.hdr);
        eprintln!("{label}: FLIP base vs bound {mean:.4} (base vs base {spread:.4})");
        if !weather.is_dry() {
            for (part, bound) in [
                (
                    "no variance rays",
                    SssrCostBound {
                        variance_rays: false,
                        ..none
                    },
                ),
                (
                    "half-res from 0.1",
                    SssrCostBound {
                        half_res_roughness: 0.1,
                        ..none
                    },
                ),
                (
                    "block rate from 0.3",
                    SssrCostBound {
                        block_roughness: 0.3,
                        ..none
                    }
                    .sanitized(),
                ),
                (
                    "block rate from 0.1",
                    SssrCostBound {
                        block_roughness: wet.block_roughness,
                        ..none
                    },
                ),
            ] {
                let run = drive_labeled(
                    &device,
                    weather,
                    Some(bound),
                    false,
                    Some(&format!("{label}_{part}")),
                );
                eprintln!("{label}: FLIP base vs {part} {:.4}", flip(&base.hdr, &run.hdr));
            }
            regia_render::set_sssr_cost_bound(None);
            let lab = road();
            let mut no_ssr = lab.as_frame_scene_with_quality(desktop_high());
            no_ssr.quality.ssr = false;
            let mut renderer = FrameRenderer::new();
            renderer.set_surface_weather(weather);
            for i in 0..FRAMES {
                no_ssr.eye[2] = EYE[2] - 0.15 * i as f32;
                no_ssr.target[2] = TARGET[2] - 0.15 * i as f32;
                if i + 1 == FRAMES {
                    renderer.request_hdr_capture();
                }
                renderer.render_offscreen(&device, &no_ssr).expect("frame");
            }
            let off = renderer
                .take_hdr_capture(&device)
                .expect("readback")
                .expect("the post stack ran");
            eprintln!(
                "{label}: FLIP base vs no reflection pass {:.4} (what the pass adds)",
                flip(&base.hdr, &off.rgb)
            );
        }
        assert!(mean <= limit, "{label}: FLIP {mean:.4} over {limit}");
    }
}
