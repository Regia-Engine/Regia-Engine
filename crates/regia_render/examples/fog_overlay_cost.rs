// SPDX-License-Identifier: MPL-2.0
//! Frame time with the fog of war overlay off and on (audit gap FW-1): an offscreen frame of a lit
//! ground seen from above, a 64 x 64 fog grid of mixed unexplored / explored / visible cells, timed
//! per frame with the readback wait (GPU plus submit and readback; the difference between the two
//! rows is the overlay's cost). Blocks of off and on frames alternate so drift hits both alike.
//!
//! ```text
//! cargo run --release -p regia_render --example fog_overlay_cost -- [width] [height] [frames] [softness]
//! ```
//!
//! Defaults: 1280 720 120 0.75. Select the card with `REGIA_GPU_INDEX` / `REGIA_GPU_NAME`.

use std::time::Instant;

use regia_render::{
    DirectionalLightUniform, FogLook, FogOverlayHandle, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice,
};

fn arg<T: std::str::FromStr>(i: usize, default: T) -> T {
    std::env::args()
        .nth(i)
        .and_then(|a| a.parse().ok())
        .unwrap_or(default)
}

fn scene(width: u32, height: u32) -> LabScene {
    let s = 400.0;
    let ground = LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-s, 0.0, -s], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([s, 0.0, -s], [0.0, 1.0, 0.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([s, 0.0, s], [0.0, 1.0, 0.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-s, 0.0, s], [0.0, 1.0, 0.0], [0.0, 1.0]),
        ],
        indices: vec![0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    };
    LabScene {
        width,
        height,
        eye: [0.0, 180.0, 160.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 1.0,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: true,
        post: Some(PostSettings::golden_deterministic()),
        directional: DirectionalLightUniform::new([0.2, -1.0, -0.3], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.45, 0.5, 0.35, 1.0],
            ..LabMaterial::default()
        }],
        meshes: vec![ground],
    }
}

fn median_ms(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    times.get(times.len() / 2).copied().unwrap_or(0.0)
}

fn main() {
    let (w, h, frames, softness) = (
        arg(1, 1280u32),
        arg(2, 720u32),
        arg(3, 120usize).max(8),
        arg(4, 0.75f32),
    );
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("no GPU adapter: {err}");
            std::process::exit(2);
        }
    };
    println!("adapter: {}", device.adapter.get_info().name);
    let lab = scene(w, h);
    let frame_scene = lab.as_frame_scene();
    let handle = FogOverlayHandle::default();
    let mut renderer = FrameRenderer::new();
    renderer.enable_fog_overlay(&handle);
    let cells = 64u32;
    handle.set_grid(cells, cells, [-512.0, -512.0], [1024.0, 1024.0]);
    let texels: Vec<u8> = (0..cells * cells)
        .map(|i| {
            let (x, z) = ((i % cells) as i32 - 32, (i / cells) as i32 - 32);
            match x * x + z * z {
                d if d < 100 => 255,
                d if d < 400 => 128,
                _ => 0,
            }
        })
        .collect();
    handle.write_rows(0, cells, &texels);
    let on = FogLook {
        softness,
        ..FogLook::on()
    };
    let off = FogLook::default();
    // Warm both variants (pipelines, texture).
    for look in [on, off] {
        handle.set_look(look);
        for _ in 0..4 {
            if let Err(err) = renderer.render_offscreen(&device, &frame_scene) {
                eprintln!("frame failed: {err}");
                std::process::exit(1);
            }
        }
    }
    let (mut t_off, mut t_on) = (Vec::new(), Vec::new());
    let block = 8;
    let mut done = 0;
    while done < frames {
        for (look, out) in [(off, &mut t_off), (on, &mut t_on)] {
            handle.set_look(look);
            for _ in 0..block {
                let start = Instant::now();
                if let Err(err) = renderer.render_offscreen(&device, &frame_scene) {
                    eprintln!("frame failed: {err}");
                    std::process::exit(1);
                }
                out.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        done += block;
    }
    let stats = handle.stats();
    let (off_ms, on_ms) = (median_ms(t_off), median_ms(t_on));
    println!("{w}x{h}, {cells}x{cells} fog cells, softness {softness}, {done} frames each");
    println!("overlay off: median {off_ms:.3} ms/frame");
    println!("overlay on:  median {on_ms:.3} ms/frame");
    println!("difference:  {:.3} ms/frame", on_ms - off_ms);
    println!(
        "overlay draws {}, texture creates {}, rows uploaded {}",
        stats.draws, stats.texture_creates, stats.rows_uploaded
    );
}
