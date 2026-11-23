// SPDX-License-Identifier: MPL-2.0
//! Water evidence on real stream data (ADR-1399): crops a raw heightfield around one point of a
//! stream centerline, builds the ground as a mesh, spawns the stream as a `WaterBody` ribbon (three
//! rocks added as obstacles), and renders it from about 2 m and about 200 m at two fixed-step ticks
//! each, under a low side sun and a low back sun.
//!
//! Inputs (not in the repository): a directory holding `heightfield.json` (`size`, `cell_m`;
//! centered layout, `x = col − (size − 1)/2`, row 0 at −Z), `heightfield.f32` (little-endian `f32`
//! meters, row by row) and `stream.json` (`points: [{ x, z, water_y, width_m, depth_m }]`, source
//! first).
//!
//! ```text
//! cargo run -p regia_terrain --example water_stream_data -- <terrain dir> <out dir> [--at 600] [--size 2560x1440]
//! ```

use std::error::Error;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice, ShadowCast,
};
use regia_terrain::{RibbonPoint, WaterBody, WaterClock, WaterLook, WaterObstacle, WaterRenderer, WaterTier};

const SKY: ([f32; 3], [f32; 3]) = ([4200.0, 5200.0, 6900.0], [900.0, 850.0, 700.0]);

/// The value after `"key":` in `text`, from `from` on.
fn number_after(text: &str, key: &str, from: usize) -> Option<(f64, usize)> {
    let at = text[from..].find(&format!("\"{key}\""))? + from;
    let colon = text[at..].find(':')? + at + 1;
    let rest = &text[colon..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some((rest[..end].trim().parse().ok()?, colon + end))
}

struct Grid {
    size: usize,
    cell: f32,
    file: std::fs::File,
}

impl Grid {
    fn open(dir: &Path) -> Result<Self, Box<dyn Error>> {
        let meta = std::fs::read_to_string(dir.join("heightfield.json"))?;
        let size = number_after(&meta, "size", 0)
            .ok_or("heightfield.json: no size")?
            .0 as usize;
        let cell = number_after(&meta, "cell_m", 0).map_or(1.0, |(v, _)| v as f32);
        Ok(Self {
            size,
            cell,
            file: std::fs::File::open(dir.join("heightfield.f32"))?,
        })
    }

    /// Heights of the window of `n × n` samples whose first sample is (`col0`, `row0`).
    fn window(&mut self, col0: usize, row0: usize, n: usize) -> Result<Vec<f32>, Box<dyn Error>> {
        let mut out = vec![0.0f32; n * n];
        let mut row = vec![0u8; n * 4];
        for j in 0..n {
            let r = (row0 + j).min(self.size - 1);
            let offset = ((r * self.size + col0.min(self.size - n)) * 4) as u64;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.read_exact(&mut row)?;
            for (i, chunk) in row.as_chunks::<4>().0.iter().enumerate() {
                out[j * n + i] = f32::from_le_bytes(*chunk);
            }
        }
        Ok(out)
    }
}

/// The ground around `center` (half size `half` m) as a mesh with `step` m cells (bilinear between
/// samples).
fn ground_mesh(
    grid: &mut Grid,
    center: [f32; 2],
    half: f32,
    step: f32,
) -> Result<(Vec<LabVertex>, Vec<u32>), Box<dyn Error>> {
    let mid = (grid.size as f32 - 1.0) * 0.5;
    let cell = grid.cell;
    let col0 = (((center[0] - half) / cell + mid).floor().max(0.0)) as usize;
    let row0 = (((center[1] - half) / cell + mid).floor().max(0.0)) as usize;
    let n = ((2.0 * half / cell).ceil() as usize + 3).min(grid.size);
    let heights = grid.window(col0, row0, n)?;
    let height = |x: f32, z: f32| {
        let fc = (x / cell + mid - col0 as f32).clamp(0.0, (n - 2) as f32);
        let fr = (z / cell + mid - row0 as f32).clamp(0.0, (n - 2) as f32);
        let (c, r) = (fc.floor() as usize, fr.floor() as usize);
        let (tc, tr) = (fc - c as f32, fr - r as f32);
        let at = |cc: usize, rr: usize| heights[rr * n + cc];
        let top = at(c, r) * (1.0 - tc) + at(c + 1, r) * tc;
        let bottom = at(c, r + 1) * (1.0 - tc) + at(c + 1, r + 1) * tc;
        top * (1.0 - tr) + bottom * tr
    };
    let cells = (2.0 * half / step) as u32;
    let mut vertices = Vec::new();
    for j in 0..=cells {
        for i in 0..=cells {
            let x = center[0] - half + i as f32 * step;
            let z = center[1] - half + j as f32 * step;
            let e = step.max(0.25);
            let dx = (height(x + e, z) - height(x - e, z)) / (2.0 * e);
            let dz = (height(x, z + e) - height(x, z - e)) / (2.0 * e);
            let normal = Vec3::new(-dx, 1.0, -dz).normalize();
            vertices.push(LabVertex::from_pos_normal_uv(
                [x, height(x, z), z],
                normal.to_array(),
                [x, z],
            ));
        }
    }
    let row = cells + 1;
    let mut indices = Vec::new();
    for j in 0..cells {
        for i in 0..cells {
            let a = j * row + i;
            indices.extend_from_slice(&[a, a + row, a + 1, a + 1, a + row, a + row + 1]);
        }
    }
    Ok((vertices, indices))
}

fn stream_points(dir: &Path) -> Result<Vec<RibbonPoint>, Box<dyn Error>> {
    let text = std::fs::read_to_string(dir.join("stream.json"))?;
    let mut points = Vec::new();
    let mut from = text.find("\"points\"").ok_or("stream.json: no points")?;
    while let Some((x, after_x)) = number_after(&text, "x", from) {
        let (z, at) = number_after(&text, "z", after_x).ok_or("stream.json: point without z")?;
        let (y, at) = number_after(&text, "water_y", at).ok_or("stream.json: point without water_y")?;
        let (w, at) = number_after(&text, "width_m", at).ok_or("stream.json: point without width_m")?;
        let (d, at) = number_after(&text, "depth_m", at).ok_or("stream.json: point without depth_m")?;
        points.push(RibbonPoint {
            position: [x as f32, y as f32, z as f32],
            width: w as f32,
            depth: d as f32,
            speed: None,
        });
        from = at;
    }
    Ok(points)
}

/// A flattened octahedron (a rock).
fn rock() -> (Vec<LabVertex>, Vec<u32>) {
    let p = [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z];
    let faces = [
        [0, 2, 4],
        [4, 2, 1],
        [1, 2, 5],
        [5, 2, 0],
        [4, 3, 0],
        [1, 3, 4],
        [5, 3, 1],
        [0, 3, 5],
    ];
    let mut vertices = Vec::new();
    for f in faces {
        let (a, b, c) = (p[f[0]], p[f[1]], p[f[2]]);
        let n = (b - a).cross(c - a).normalize();
        for v in [a, b, c] {
            vertices.push(LabVertex::from_pos_normal_uv(
                v.to_array(),
                n.to_array(),
                [0.0, 0.0],
            ));
        }
    }
    let indices = (0..vertices.len() as u32).collect();
    (vertices, indices)
}

struct Shot {
    name: String,
    eye: Vec3,
    target: Vec3,
    fov: f32,
    tick: u64,
    sun: [f32; 3],
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), Box<dyn Error>> {
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(pixels)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut at = 600usize;
    let mut size = (2560u32, 1440u32);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--at" => at = args.next().ok_or("--at needs an index")?.parse()?,
            "--size" => {
                let text = args.next().ok_or("--size needs WxH")?;
                let (w, h) = text.split_once('x').ok_or("--size needs WxH")?;
                size = (w.parse()?, h.parse()?);
            }
            other => positional.push(PathBuf::from(other)),
        }
    }
    let [dir, out] = <[PathBuf; 2]>::try_from(positional)
        .map_err(|_| "usage: water_stream_data <terrain dir> <out dir> [--at N] [--size WxH]")?;
    std::fs::create_dir_all(&out)?;
    let mut grid = Grid::open(&dir)?;
    let all = stream_points(&dir)?;
    if all.len() < 20 {
        return Err("stream.json has fewer than 20 points".into());
    }
    let at = at.clamp(10, all.len() - 10);
    let here = Vec3::from_array(all[at].position);
    let ahead = Vec3::from_array(all[at + 1].position);
    let tangent = Vec3::new(ahead.x - here.x, 0.0, ahead.z - here.z).normalize_or(Vec3::X);
    let right = Vec3::new(-tangent.z, 0.0, tangent.x);
    println!(
        "point {at}: ({:.1}, {:.2}, {:.1}) width {:.2} m depth {:.2} m",
        here.x, here.y, here.z, all[at].width, all[at].depth
    );

    let device = RenderDevice::headless()?;
    println!("adapter: {}", device.adapter.get_info().name);
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 14.0,
        fxaa: false,
        taa: false,
        ..PostSettings::default()
    };
    let ground_mat = LabMaterial {
        base_color: [0.3, 0.27, 0.2, 1.0],
        roughness: 0.95,
        ..LabMaterial::EMPTY
    };
    let rock_mat = LabMaterial {
        base_color: [0.4, 0.38, 0.35, 1.0],
        roughness: 0.8,
        ..LabMaterial::EMPTY
    };
    let (rock_v, rock_i) = rock();
    // Three rocks in the water a few meters downstream.
    let rocks: Vec<(Vec3, f32)> = [
        (at + 3, 0.15f32, 0.22f32),
        (at + 6, -0.25, 0.16),
        (at + 10, 0.1, 0.25),
    ]
    .iter()
    .map(|&(i, side, r)| {
        let p = Vec3::from_array(all[i].position);
        (p + right * side * all[i].width - Vec3::Y * 0.05, r)
    })
    .collect();
    let sun_side = [
        right.x * 0.7 - tangent.x * 0.3,
        -0.18,
        right.z * 0.7 - tangent.z * 0.3,
    ];
    let sun_back = [-tangent.x, -0.17, -tangent.z];

    for (label, half, step, shots) in [
        (
            "near",
            40.0f32,
            0.25f32,
            vec![
                (0u64, sun_side, "side"),
                (15, sun_side, "side"),
                (0, sun_back, "back"),
                (15, sun_back, "back"),
            ],
        ),
        (
            "far",
            320.0,
            1.0,
            vec![
                (0, sun_side, "side"),
                (30, sun_side, "side"),
                (0, sun_back, "back"),
            ],
        ),
    ] {
        let (ground_v, ground_i) = ground_mesh(&mut grid, [here.x, here.z], half, step)?;
        let first = all
            .iter()
            .position(|p| (Vec3::from_array(p.position) - here).length() < half - 4.0)
            .unwrap_or(0);
        let last = all
            .iter()
            .rposition(|p| (Vec3::from_array(p.position) - here).length() < half - 4.0)
            .unwrap_or(all.len() - 1);
        let mut stream = WaterBody::ribbon(all[first..=last].to_vec(), WaterLook::clear_stream());
        stream.obstacles = rocks
            .iter()
            .map(|(p, r)| WaterObstacle {
                position: p.to_array(),
                radius: *r,
                strength: 1.0,
            })
            .collect();
        let (eye, target, fov) = if label == "near" {
            (
                here - tangent * 1.0 + right * 1.7 + Vec3::Y * 1.4,
                Vec3::from_array(all[at + 5].position),
                0.9,
            )
        } else {
            (here + right * 170.0 - tangent * 60.0 + Vec3::Y * 85.0, here, 0.3)
        };
        let shots: Vec<Shot> = shots
            .into_iter()
            .map(|(tick, sun, sun_name)| Shot {
                name: format!("stream_{label}_{sun_name}_t{tick:03}"),
                eye,
                target,
                fov,
                tick,
                sun,
            })
            .collect();
        for shot in shots {
            let water = WaterRenderer::new(&device);
            water.set_tier(WaterTier::High);
            water.set_bodies(&[&stream]);
            water.set_ambient_lux(15_000.0);
            water.set_clock(WaterClock {
                tick: shot.tick,
                ..WaterClock::default()
            });
            let mut renderer = FrameRenderer::new();
            renderer.set_studio_sky(&device, Some(SKY));
            renderer.set_translucent_layers(vec![water.layer()]);
            let mut meshes = vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &ground_v,
                indices: &ground_i,
                material: &ground_mat,
                model: Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }];
            for (k, (p, r)) in rocks.iter().enumerate() {
                meshes.push(FrameMesh {
                    is_static: false,
                    mesh_key: 2,
                    material_key: 2,
                    object_key: 10 + k as u64,
                    vertices: &rock_v,
                    indices: &rock_i,
                    material: &rock_mat,
                    model: (Mat4::from_translation(*p) * Mat4::from_scale(Vec3::new(*r, *r * 0.9, *r)))
                        .to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                });
            }
            let frame = FrameScene {
                width: size.0,
                height: size.1,
                eye: shot.eye.to_array(),
                target: shot.target.to_array(),
                fov_y: shot.fov,
                near: 0.1,
                far: 4000.0,
                depth_prepass: true,
                directional_shadows: true,
                ibl: true,
                exposure_ev: 13.5,
                ambient_lux: 15_000.0,
                double_sided: false,
                post: Some(&post),
                fog: FogSettings::default(),
                directional: DirectionalLightUniform::new(shot.sun, [1.0, 0.9, 0.78], 60_000.0),
                local_lights: &[],
                quality: FrameQuality::LAB,
                meshes,
            };
            let pixels = renderer.render_offscreen(&device, &frame)?.pixels;
            write_png(&out.join(format!("{}.png", shot.name)), size.0, size.1, &pixels)?;
            println!("{}", shot.name);
        }
    }
    Ok(())
}
