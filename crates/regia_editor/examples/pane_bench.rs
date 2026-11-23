// SPDX-License-Identifier: MPL-2.0
//! Per-frame cost of the live viewport pane path: GPU texture (no readback) against the CPU
//! readback fallback (`cargo run -p regia_editor --example pane_bench --release -- [WxH] [frames]`).
//! Honours `REGIA_GPU_*`; prints the adapter and a markdown table (`regia_editor::pane_bench`).

use regia_editor::pane_bench::{BenchSpec, markdown, measure};
use regia_editor::pane_texture::PanePath;

fn main() {
    let mut spec = BenchSpec::default();
    for arg in std::env::args().skip(1) {
        if let Some((w, h)) = arg.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                spec.size = [w, h];
            }
        } else if let Ok(frames) = arg.parse() {
            spec.frames = frames;
        }
    }
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("pane_bench: no GPU adapter: {err}");
            std::process::exit(1);
        }
    };
    let reports = [PanePath::Gpu, PanePath::Cpu].map(|path| measure(&device, path, &spec));
    print!("{}", markdown(&device.adapter.get_info().name, &reports));
}
