// SPDX-License-Identifier: MPL-2.0
//! Debug example: renders an island terrain with CDLOD and saves a PPM for visual inspection.
//! Run: `cargo run --example dbg -- output.ppm`

use regia_asset::Heightfield;
use regia_terrain::gpu::*;
use regia_terrain::*;

fn main() {
    flat_test();
    let device = regia_render::RenderDevice::headless().expect("device");
    let n = 257u32;
    let width = 256.0f32;
    let mut samples = Vec::new();
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            let r = (x * x + z * z).sqrt();
            let h = 34.0 * (-(r / 95.0).powi(2)).exp() - 6.0 + 2.2 * (x * 0.09).sin() * (z * 0.07).cos();
            samples.push(((h + 40.0) / 80.0 * 65535.0).round().clamp(0.0, 65535.0) as u16);
        }
    }
    let map = TerrainMap::new(
        &Heightfield {
            width: n,
            height: n,
            samples,
        },
        width,
        80.0,
        -40.0,
    )
    .expect("valid map");
    let stack = TerrainLayers {
        layers: vec![LayerDesc::ground()],
        ..Default::default()
    };
    let config = CdlodConfig::with_grid_size(16);
    let pyramid = MinMaxPyramid::build(&map, 4).expect("pyramid build");
    let mut gpu = TerrainGpu::new(&device, &map, &stack, &config, 4, None, None).expect("gpu");
    let cam = TerrainCamera {
        position: [0.0, 55.0, 130.0],
        target: [0.0, 6.0, 0.0],
        fov_y_radians: 1.05,
        aspect: 320.0 / 180.0,
        near: 0.5,
        far: 3000.0,
    };
    let frame = TerrainFrame::from_camera(&cam, &TimeOfDay::default(), 0.1, 1.7);
    let mut sel = select_nodes(&map, &pyramid, &config, 4, [0.0, 130.0], None);
    println!("nodes: {}", sel.nodes.len());
    if let Some(n) = std::env::args().nth(2).and_then(|a| a.parse::<usize>().ok()) {
        sel.nodes.truncate(n);
        println!("truncated to {}", sel.nodes.len());
    }
    let vp = cam.view_projection();
    {
        use regia_core::glam::Vec3;
        let view = regia_core::glam::camera::rh::view::look_at_mat4(
            Vec3::from_array(cam.position),
            Vec3::from_array(cam.target),
            Vec3::Y,
        );
        let fwd = Vec3::from_array(cam.target) - Vec3::from_array(cam.position);
        println!("fwd {fwd:?}");
        for (label, p) in [
            ("target", Vec3::new(0.0, 6.0, 0.0)),
            ("high_far", Vec3::new(0.0, 80.0, -128.0)),
        ] {
            let v = (view * p.extend(1.0)).truncate();
            println!("{label}: view {v:?}  (expect z<0 in front for RH)");
        }
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(
            cam.fov_y_radians,
            cam.aspect,
            cam.near,
            cam.far,
        );
        let pv = (proj * view) * Vec3::new(0.0, 80.0, -128.0).extend(1.0);
        println!("high_far clip {pv:?}");
    }
    for (label, p) in [
        ("target", [0.0f32, 6.0, 0.0, 1.0]),
        ("high_far", [0.0, 80.0, -128.0, 1.0]),
        ("below_cam", [0.0, 4.0, 128.0, 1.0]),
        ("behind", [0.0, 6.0, 200.0, 1.0]),
    ] {
        let mut clip = [0.0f32; 4];
        for (i, row) in vp.iter().enumerate() {
            clip[i] = row[0] * p[0] + row[1] * p[1] + row[2] * p[2] + row[3] * p[3];
        }
        // clip = M * p with M in column-major nested: clip[i] = sum_j M[j][i] * p[j]
        let mut clip2 = [0.0f32; 4];
        for (j, col) in vp.iter().enumerate() {
            for i in 0..4 {
                clip2[i] += col[i] * p[j];
            }
        }
        println!("{label}: rowmul {clip:?} colmul {clip2:?}");
    }
    let pixels =
        render_offscreen(&mut gpu, &device, &frame, &sel, 320, 180, [1.0, 0.0, 1.0, 1.0]).expect("render");
    for (label, x, y) in [
        ("top", 160, 2),
        ("upper", 160, 45),
        ("centre", 160, 90),
        ("lower", 160, 135),
        ("bottom", 160, 177),
    ] {
        let i = (y * 320 + x) * 4;
        println!("{label}: {:?}", &pixels[i..i + 4]);
    }
    let clear = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 240 && p[1] < 15 && p[2] > 240)
        .count();
    println!("clear px: {clear}/{}", 320 * 180);
    // Save PPM for inspection.
    let path = std::env::args().nth(1).unwrap_or_else(|| "dbg.ppm".into());
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path).expect("create"));
    std::io::Write::write_all(&mut out, "P6\n320 180\n255\n".as_bytes()).expect("hdr");
    let mut rgb = Vec::with_capacity(320 * 180 * 3);
    for px in pixels.as_chunks::<4>().0 {
        rgb.extend_from_slice(&px[..3]);
    }
    std::io::Write::write_all(&mut out, &rgb).expect("write");
    println!("wrote {path}");
}

fn flat_test() {
    let device = regia_render::RenderDevice::headless().expect("device");
    // Perfectly flat map at y = 0, 64x64 samples over 128 m.
    let map = TerrainMap::from_heights(64, 64, vec![5.0; 64 * 64], 128.0, 1.0, 0.0).expect("valid flat map");
    let stack = TerrainLayers::default();
    let config = CdlodConfig::with_grid_size(16);
    let pyramid = MinMaxPyramid::build(&map, 3).expect("flat pyramid");
    let mut gpu = TerrainGpu::new(&device, &map, &stack, &config, 3, None, None).expect("gpu");
    // Camera 10 m above ground, 60 m south, looking at the map centre.
    let cam = TerrainCamera {
        position: [0.0, 10.0, 64.0],
        target: [0.0, 0.0, 0.0],
        fov_y_radians: 1.0,
        aspect: 1.0,
        near: 0.5,
        far: 500.0,
    };
    let frame = TerrainFrame::from_camera(&cam, &TimeOfDay::default(), 0.0, 0.0);
    let mut sel = select_nodes(&map, &pyramid, &config, 3, [0.0, 64.0], None);
    let disable_morph = std::env::args().any(|a| a == "--no-morph");
    if disable_morph {
        for n in &mut sel.nodes {
            n.morph_start = 0.0;
            n.morph_end = 0.0;
        }
    }
    println!("morph disabled: {disable_morph}");
    println!(
        "flat nodes: {} first {:?}",
        sel.nodes.len(),
        sel.nodes.first().map(|n| (n.node, n.morph_start, n.morph_end))
    );
    // CPU-side ground truth for one vertex.
    if let Some(draw) = sel.nodes.first() {
        println!(
            "cpu vertex(0,0): {:?}",
            regia_terrain::cdlod::eval_node_vertex(&map, &config, draw.node, 3, 0, 0, 0.0)
        );
    }
    let pixels =
        render_offscreen(&mut gpu, &device, &frame, &sel, 80, 80, [1.0, 0.0, 1.0, 1.0]).expect("render");
    let clear = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 240 && p[1] < 15 && p[2] > 240)
        .count();
    println!(
        "flat summary: nodes {} clear {}/6400 strideinfo {}",
        sel.nodes.len(),
        clear,
        regia_terrain::cdlod::select_nodes(&map, &pyramid, &config, 3, [0.0, 64.0], None)
            .nodes
            .len()
    );
    for y in (0..80).step_by(4) {
        let mut row = String::new();
        for x in (0..80).step_by(2) {
            let j = (y * 80 + x) * 4;
            let (r, g, b) = (pixels[j], pixels[j + 1], pixels[j + 2]);
            if r > 240 && b > 240 && g < 15 {
                row.push('C');
            } else {
                let l = (u16::from(r) * 3 + u16::from(g) * 6 + u16::from(b)) / 10;
                let ramp = " .:-=+*#%@";
                row.push(ramp.as_bytes()[usize::min(9, usize::from(l) * 10 / 256)] as char);
            }
        }
        println!("{row}");
    }
}
