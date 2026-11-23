// SPDX-License-Identifier: MPL-2.0
//! Renders the look board still: stand-in gems (every cut, clear and colored stones, frosted,
//! cabochon, opal), stones set in bezel cups, metal ring bands and anodized beads, under the fixed
//! studio, headless (lavapipe on CI hosts). Writes a PNG.
//!
//! ```text
//! cargo run -p regia_looks --features gpu --example look_still -- out.png [width height] [tilt_deg]
//! ```

use std::error::Error;
use std::path::PathBuf;

use regia_looks::Studio;
use regia_looks::gpu::{LookRenderer, headless_device};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = PathBuf::from(args.first().map_or("look-still.png", String::as_str));
    let width: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1920);
    let height: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1200);
    let tilt: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(22.0);
    let scene = regia_looks::board::look_board(tilt)?;
    let (device, queue, info) = headless_device()?;
    eprintln!("adapter: {} ({:?})", info.name, info.backend);
    let mut renderer = LookRenderer::new(
        device,
        queue,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        4,
        Studio::default(),
    );
    // Supersample 2x for the evidence still; the viewer itself draws at 1x with MSAA 4.
    let (sw, sh) = (width * 2, height * 2);
    let big = renderer.render_still(&scene, sw, sh)?;
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            for c in 0..4 {
                let at =
                    |dx: usize, dy: usize| u32::from(big[((y * 2 + dy) * sw as usize + x * 2 + dx) * 4 + c]);
                rgba[(y * width as usize + x) * 4 + c] =
                    ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
            }
        }
    }
    write_png(&out, width, height, &rgba)?;
    eprintln!(
        "wrote {} ({width}x{height}, {} pipelines, {} draws)",
        out.display(),
        renderer.pipelines_created(),
        renderer.draws_last_frame()
    );
    Ok(())
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), Box<dyn Error>> {
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(rgba)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
