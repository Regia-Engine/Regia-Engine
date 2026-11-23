// SPDX-License-Identifier: MPL-2.0
//! The `aerial_from_tables` plate (ADR-3294): the real outdoor composite (`outdoor_sky.wgsl`'s
//! sky, aerial perspective and weather veil, the cloud march when the weather has a deck) over a
//! flat ground disc around a raised eye, with the forward pass's part of the frame written by the
//! CPU mirror of its terms (`crate::horizon_seam`). Read back linear, with the sky mask, so the
//! horizon seam can be measured on it. Not used by any frame; tests and the owner's plates call it.

use crate::frame_work::counted::*;
use regia_core::glam::{Mat4, Vec3, Vec4};

use super::{OutdoorComposite, OutdoorState, TableAir, depth_tex, look, rgba_tex, submit};
use crate::clouds::CloudSettings;
use crate::horizon_seam::{HorizonSeam, forward_weather_inscatter, measure_horizon_seam, region_luminance};
use crate::{DirectionalLightUniform, RenderDevice, RenderError, TimeOfDay};

/// The weather of a [`HorizonPlate`]: the three presets the seam is reported for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HorizonWeather {
    /// No clouds, nothing falling.
    Clear,
    /// A full stratus deck, nothing falling.
    Overcast,
    /// The deck and heavy rain: 1 km visibility, σ = 3.912 / 1000 m (Koschmieder).
    HeavyRain,
}

impl HorizonWeather {
    /// Every preset, in report order.
    pub const ALL: [Self; 3] = [Self::Clear, Self::Overcast, Self::HeavyRain];

    /// Stable lower-case name (`"clear"`, `"overcast"`, `"heavy_rain"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Overcast => "overcast",
            Self::HeavyRain => "heavy_rain",
        }
    }

    /// The precipitation's extinction (1/m).
    #[must_use]
    pub const fn extinction_per_m(self) -> f32 {
        match self {
            Self::Clear | Self::Overcast => 0.0,
            Self::HeavyRain => 3.912e-3,
        }
    }

    /// The cloud layer.
    #[must_use]
    pub fn clouds(self) -> CloudSettings {
        match self {
            Self::Clear => CloudSettings::default(),
            Self::Overcast | Self::HeavyRain => CloudSettings {
                coverage: 1.0,
                cloud_type: 0.0,
                density: 0.04,
                altitude_bottom: 1200.0,
                altitude_top: 2400.0,
                ..CloudSettings::default()
            },
        }
    }
}

/// One plate: the sun clock, the weather, the mode and the camera over a flat ground disc.
#[derive(Clone, Debug)]
pub struct HorizonPlate {
    /// Sun clock (Earth).
    pub time: TimeOfDay,
    /// Weather preset.
    pub weather: HorizonWeather,
    /// The `aerial_from_tables` mode.
    pub mode: TableAir,
    /// Image size.
    pub size: (u32, u32),
    /// Vertical field of view (radians).
    pub fov_y: f32,
    /// Eye height above the ground (m).
    pub eye_height_m: f32,
    /// Radius of the ground disc around the eye (m); beyond it the cleared depth is sky.
    pub ground_radius_m: f32,
    /// Far plane (m), also how far a sky pixel's rain reaches.
    pub far_m: f32,
    /// View azimuth from the sun's azimuth (degrees; 0 looks toward the sun, 180 away).
    pub azimuth_from_sun_deg: f32,
    /// View pitch (degrees, negative looks down).
    pub pitch_deg: f32,
    /// The host's fog: the default mode's rain takes its colour from it.
    pub fog: crate::FogSettings,
    /// Draw the weather's cloud deck (the cloud march, one full march, no history).
    pub clouds: bool,
}

impl HorizonPlate {
    /// The report's plate: 192×108, 40° tall, the eye 150 m over a 120 km disc, far 150 km,
    /// looking 90° from the sun and 1.84° down, `sky_inscatter` fog, the deck drawn. The pitch puts
    /// the horizon a quarter of a row above a row boundary, so the farthest ground row is about
    /// 0.1° under the horizon, about 86 km out (a far ridge seen from a hill): past the tables'
    /// 32 km, where the default seam shows.
    #[must_use]
    pub fn new(time: TimeOfDay, weather: HorizonWeather, mode: TableAir) -> Self {
        Self {
            time,
            weather,
            mode,
            size: (192, 108),
            fov_y: 40f32.to_radians(),
            eye_height_m: 150.0,
            ground_radius_m: 120_000.0,
            far_m: 150_000.0,
            azimuth_from_sun_deg: 90.0,
            pitch_deg: -1.84,
            fog: crate::FogSettings {
                sky_inscatter: true,
                ..crate::FogSettings::default()
            },
            clouds: true,
        }
    }
}

/// A [`HorizonPlate`] read back.
#[derive(Clone, Debug)]
pub struct HorizonPlateImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Linear radiance (cd/m²), row-major from the top.
    pub rgb: Vec<[f32; 3]>,
    /// Pixels with the cleared depth (sky).
    pub sky: Vec<bool>,
    /// Ground distance per pixel (m), infinite on sky pixels.
    pub distance_m: Vec<f32>,
    /// The horizon seam ([`measure_horizon_seam`]); `None` without a sky-over-ground edge.
    pub seam: Option<HorizonSeam>,
    /// Mean luminance of the sky pixels and of the ground pixels.
    pub region_luminance: (f32, f32),
    /// Sky passes the state drew with the table entries (1 when the mode is on).
    pub table_air_frames: u64,
    /// Weather veils drawn (1 when the mode is on and something falls).
    pub weather_veils: u64,
}

const DEPTH_WGSL: &str = "@group(0) @binding(0) var depth_in: texture_2d<f32>;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let c = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(c * 2.0 - 1.0, 0.0, 1.0);
}
@fragment fn fs(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    return textureLoad(depth_in, vec2<i32>(p.xy), 0).r;
}
";

/// Renders `plate`: the CPU writes what the forward pass would (the ground's Lambert radiance
/// from the atmosphere, plus the default mode's analytic rain term) and the ground's depth; the
/// real outdoor composite then draws the sky, the aerial perspective, the clouds and, in a table
/// mode under rain, the weather veil.
///
/// # Errors
/// A device, submit or readback failure.
pub fn render_horizon_plate(
    device: &RenderDevice,
    plate: &HorizonPlate,
) -> Result<HorizonPlateImage, RenderError> {
    render_plate_with_interiors(device, plate, &[], None)
}

fn render_plate_with_interiors(
    device: &RenderDevice,
    plate: &HorizonPlate,
    interiors: &[crate::InteriorVolume],
    ground: Option<[f32; 3]>,
) -> Result<HorizonPlateImage, RenderError> {
    let (width, height) = (plate.size.0.max(2), plate.size.1.max(2));
    let mut state = OutdoorState::default();
    state.params.time = Some(plate.time);
    state.params.interiors = interiors.to_vec();
    state.params.precip_extinction_per_m = plate.weather.extinction_per_m();
    state.params.clouds = plate.weather.clouds();
    let eye_y = plate.eye_height_m.max(0.5);
    let sun = state.apply_sun(
        true,
        DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1.0),
        eye_y,
    );
    let to_sun = [-sun.direction[0], -sun.direction[1], -sun.direction[2]];
    let sun_irradiance = sun.color.map(|c| c * sun.illuminance);
    let surface = ground.unwrap_or_else(|| {
        state
            .luts
            .as_ref()
            .map_or([0.0; 3], crate::atmosphere::ground_radiance)
    });
    state.set_table_air(plate.mode);
    state.ensure_gpu(&device.device, device.shared_pipelines())?;

    // The camera: azimuth from the sun's, pitch down a little.
    let sun_az = to_sun[0].atan2(to_sun[2]);
    let az = sun_az + plate.azimuth_from_sun_deg.to_radians();
    let pitch = plate.pitch_deg.to_radians();
    let dir = Vec3::new(az.sin() * pitch.cos(), pitch.sin(), az.cos() * pitch.cos());
    let eye = Vec3::new(0.0, eye_y, 0.0);
    let near = 0.5;
    let view = look(
        eye,
        eye + dir,
        Vec3::Y,
        plate.fov_y,
        width as f32 / height as f32,
        near,
        plate.far_m,
    );
    let view_proj = Mat4::from_cols_array_2d(&view.view_proj);
    let inv = Mat4::from_cols_array_2d(&view.inv_view_proj);

    // The forward pass's image and depth, per pixel.
    let count = (width * height) as usize;
    let mut rgb = vec![[0.0f32; 3]; count];
    let mut depth = vec![1.0f32; count];
    let mut distance = vec![f32::INFINITY; count];
    let sigma = plate.weather.extinction_per_m();
    for y in 0..height {
        for x in 0..width {
            let ndc_x = (x as f32 + 0.5) / width as f32 * 2.0 - 1.0;
            let ndc_y = 1.0 - (y as f32 + 0.5) / height as f32 * 2.0;
            let far_h = inv * Vec4::new(ndc_x, ndc_y, 1.0, 1.0);
            let near_h = inv * Vec4::new(ndc_x, ndc_y, 0.0, 1.0);
            let ray = (far_h.truncate() / far_h.w - near_h.truncate() / near_h.w).normalize_or(Vec3::Z);
            if ray.y >= -1e-7 {
                continue;
            }
            let t = -eye_y / ray.y;
            let hit = eye + ray * t;
            if Vec3::new(hit.x, 0.0, hit.z).length() > plate.ground_radius_m || t > plate.far_m {
                continue;
            }
            let clip = view_proj * hit.extend(1.0);
            let z = clip.z / clip.w;
            if !(0.0..1.0).contains(&z) {
                continue;
            }
            let i = (y * width + x) as usize;
            depth[i] = z;
            distance[i] = t;
            rgb[i] = if plate.mode.is_on() || sigma <= 0.0 {
                surface
            } else {
                // `lab_forward.wgsl`: `weather_extinction` with `weather_inscatter`.
                let rain = forward_weather_inscatter(ray.to_array(), to_sun, sun_irradiance, &plate.fog, 1.0);
                let tw = (-sigma * t).exp();
                std::array::from_fn(|c| surface[c] * tw + rain[c] * (1.0 - tw))
            };
        }
    }

    let d = &device.device;
    let hdr = rgba_tex(
        d,
        width,
        height,
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        "horizon plate hdr",
    );
    let half = crate::ibl::rgb_to_rgba16f(&rgb);
    device.queue.write_texture(
        hdr.texture.as_image_copy(),
        &half,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 8),
            rows_per_image: Some(height),
        },
        super::extent(width, height),
    );
    let depth_in = rgba_tex(
        d,
        width,
        height,
        wgpu::TextureFormat::R32Float,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        "horizon plate depth values",
    );
    let depth_bytes: Vec<u8> = depth.iter().flat_map(|v| v.to_le_bytes()).collect();
    device.queue.write_texture(
        depth_in.texture.as_image_copy(),
        &depth_bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        super::extent(width, height),
    );
    let scene_depth = depth_tex(d, width, height, "horizon plate depth");
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("horizon plate depth"),
        source: wgpu::ShaderSource::Wgsl(DEPTH_WGSL.into()),
    });
    let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("horizon plate depth"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        }],
    });
    let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("horizon plate depth"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("horizon plate depth"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            targets: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("horizon plate depth"),
        layout: &layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&depth_in.view),
        }],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("horizon plate"),
    });
    {
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("horizon plate depth"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &scene_depth.view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
        pass.counted_set_pipeline(&pipeline);
        pass.counted_set_bind_group(0, &bind, &[]);
        pass.counted_draw(0..3, 0..1);
    }
    let shot = OutdoorComposite {
        prepared_sky: None,
        depth_is_scene: true,
        pre_exposure: 1.0,
        hdr: &hdr.view,
        depth: &scene_depth.view,
        shadow: None,
        width,
        height,
        format: wgpu::TextureFormat::Rgba16Float,
        inv_view_proj: view.inv_view_proj,
        unjittered_inv_view_proj: view.inv_view_proj,
        prev_view_proj: view.view_proj,
        eye: view.eye,
        near,
        far: plate.far_m,
        sun_travel: sun.direction,
        sun_color: sun.color,
        sun_illuminance: sun.illuminance,
        light_vp: view.view_proj,
        shadow_size: 0.0,
        atmosphere: true,
        fog: false,
        clouds: plate.clouds,
        unjittered_view_proj: view.view_proj,
        cloud_reprojection: false,
        cloud_step_percent: 100,
        cloud_cone_light: false,
        cloud_march: crate::CloudMarch::REALTIME,
        local_shadow: None,
        fog_light_shadows: [[[0.0; 4]; 6]; 4],
    };
    let queue = crate::device::TrackedQueue::observe(&device.queue);
    state.composite(
        d,
        &queue,
        device.shared_pipelines(),
        &mut encoder,
        &shot,
        |_, _, _| {},
    )?;
    submit(device, encoder)?;
    let halves = super::cloud_probe::read_half(device, &hdr.texture, width, height, 4)?;
    let rgb: Vec<[f32; 3]> = halves
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    let sky: Vec<bool> = depth.iter().map(|z| *z >= 1.0).collect();
    Ok(HorizonPlateImage {
        width,
        height,
        seam: measure_horizon_seam(&rgb, &sky, width, height),
        region_luminance: region_luminance(&rgb, &sky),
        rgb,
        sky,
        distance_m: distance,
        table_air_frames: state.table_air_frames(),
        weather_veils: state.weather_veils(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_entries_and_rain_respect_interiors_and_leave_outdoors_exact() {
        if let Some(path) = std::env::var_os("REGIA_AERIAL_BASELINE_SHADER") {
            let source = std::fs::read_to_string(path).expect("parent shader evidence input");
            let swap = crate::shader_reload::stage_shader_edit("outdoor_sky.wgsl", &source)
                .expect("parent shader validates");
            assert!(swap.validated > 0, "the override must validate real entries");
        }
        let device = RenderDevice::headless().expect("headless device");
        let room = crate::InteriorVolume::with_floor([0.0, 2.0, 0.0], [10.0, 3.0, 10.0]);
        let behind = crate::InteriorVolume::with_floor([1.0e6, 2.0, 0.0], [10.0, 3.0, 10.0]);
        let mut failures = Vec::new();
        for mode in [TableAir::Full, TableAir::Lite] {
            for weather in [HorizonWeather::Clear, HorizonWeather::HeavyRain] {
                let mut plate = HorizonPlate::new(TimeOfDay::noon(), weather, mode);
                plate.size = (64, 48);
                plate.eye_height_m = 2.0;
                plate.ground_radius_m = 100.0;
                plate.far_m = 1000.0;
                plate.pitch_deg = -60.0;
                plate.clouds = false;
                let open =
                    render_plate_with_interiors(&device, &plate, &[], Some([0.0; 3])).expect("open air");
                assert_eq!(open.table_air_frames, 1, "must actually use the table entry");
                assert_eq!(
                    open.weather_veils,
                    u64::from(weather == HorizonWeather::HeavyRain)
                );
                assert!(open.sky.iter().all(|sky| !sky), "all rays must hit the ground");
                let irrelevant = render_plate_with_interiors(&device, &plate, &[behind], Some([0.0; 3]))
                    .expect("irrelevant volume");
                assert_eq!(open.rgb, irrelevant.rgb, "outside must remain bit-identical");
                if let Some(dir) = std::env::var_os("REGIA_AERIAL_OUTDOOR_CAPTURE") {
                    let dir = std::path::PathBuf::from(dir);
                    std::fs::create_dir_all(&dir).expect("evidence directory");
                    let bytes: Vec<u8> = open.rgb.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
                    let path = dir.join(format!("tables_{mode:?}_{}.rgb32le", weather.name()));
                    regia_test_utils::write_atomic(&path, &bytes).expect("atomic evidence");
                }
                let closed = render_plate_with_interiors(&device, &plate, &[room], Some([0.0; 3]))
                    .expect("closed air");
                let a = open.region_luminance.1;
                let b = closed.region_luminance.1;
                eprintln!("table {mode:?} {weather:?}: {a} -> {b}");
                assert!(a > 0.0, "the control must scatter light");
                if b > a * 0.001 {
                    failures.push(format!("{mode:?} {weather:?}: {a} -> {b}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "roofed air scattered light: {}",
            failures.join(", ")
        );
    }
}
