// SPDX-License-Identifier: MIT-0
//! `sky_lab`: renders a day cycle with a weather change headless and writes PNG frames and a
//! JSON report. See `book/agents/sky-lab.md`.

use std::process::ExitCode;

use regia_render::RenderDevice;
use sky_lab::lab::run;
use sky_lab::{Options, WeatherPreset};

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("sky_lab: {err}");
            eprintln!(
                "usage: sky_lab [--frames N] [--size WxH] [--from PRESET] [--to PRESET] [--start-hour H] [--end-hour H] [--out DIR] [--every N] [--list]"
            );
            return ExitCode::from(2);
        }
    };
    if options.list {
        for preset in WeatherPreset::ALL {
            println!("{}", preset.name());
        }
        return ExitCode::SUCCESS;
    }
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("sky_lab: no GPU device: {err}");
            return ExitCode::from(3);
        }
    };
    match run(&device, &options) {
        Ok(report) => {
            let (Some(first), Some(last)) = (report.frames.first(), report.frames.last()) else {
                return ExitCode::SUCCESS;
            };
            println!(
                "sky_lab: {} frames at {}x{}, hour {:.1} -> {:.1}, mean luma {:.1} -> {:.1}",
                report.frames.len(),
                report.width,
                report.height,
                first.hour,
                last.hour,
                first.mean_luma,
                last.mean_luma
            );
            if let Some(dir) = &options.out {
                println!("sky_lab: wrote frames and report.json under {}", dir.display());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("sky_lab: {err}");
            ExitCode::FAILURE
        }
    }
}
