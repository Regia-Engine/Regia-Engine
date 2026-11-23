// SPDX-License-Identifier: MIT-0
//! `mem_report`: level load, RSS and GPU memory by category for one DO27 map at the tier
//! `LOOK_R2_TIER` selects, then the chase from a cold start as a streaming soak
//! (`lookdev::mem_budget`).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> [LOOK_R2_TIER=deck] mem_report --map switchbacks|bore_twelve
//!     [--cold] (drop the map's files from the page cache first) [--frames 600] [--warmup 30]
//!     [--size 2560x1440] [--csv <categories.csv>] [--frames-csv <per-frame.csv>] [--png <last frame.png>]
//! ```

use std::io::Write as _;
use std::process::ExitCode;

use lookdev::chase::ChaseOptions;
use lookdev::{LookdevError, Planet};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match main_inner(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mem_report: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn main_inner(args: &[String]) -> Result<(), LookdevError> {
    let content = lookdev::do27::content_dir()
        .ok_or_else(|| LookdevError::Config("set REGIA_DO27_CONTENT=<do27-content dir>".into()))?;
    let planet = match value(args, "--map") {
        Some("switchbacks") => Planet::Mars,
        Some("bore_twelve") => Planet::Europa,
        _ => return Err(LookdevError::Config("--map switchbacks|bore_twelve".into())),
    };
    let mut options = ChaseOptions {
        frames: 600,
        warmup: 30,
        ..ChaseOptions::default()
    };
    if let Some(v) = value(args, "--frames").and_then(|v| v.parse().ok()) {
        options.frames = v;
    }
    if let Some(v) = value(args, "--warmup").and_then(|v| v.parse().ok()) {
        options.warmup = v;
    }
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    options.png = value(args, "--png").map(std::path::PathBuf::from);
    if args.iter().any(|a| a == "--cold") {
        let map_dir = content.join("maps").join(lookdev::do27::map_name(planet));
        let evicted = lookdev::mem_budget::evict_page_cache(&map_dir)?;
        println!(
            "evicted {:.1} MiB from the page cache",
            evicted as f64 / 1048576.0
        );
    }
    // The tier's texture policy before the load, as a shell publishes it: images above its cap
    // are fitted at material load (`LOOK_R2_STREAM=off` publishes none).
    regia_render::set_resident_cap(lookdev::look_r2::tier_settings().profile.texture_streaming);
    let device = lookdev::mem_budget::runtime_device()?;
    println!("adapter: {}", device.adapter.get_info().name);
    let run = lookdev::mem_budget::measure(&device, &content, planet, &options)?;
    print!("{}", run.summary());
    if let Some(path) = value(args, "--frames-csv") {
        std::fs::write(path, run.frames_csv()).map_err(|source| LookdevError::Io {
            path: path.to_string(),
            source,
        })?;
        println!("wrote {path}");
    }
    if let Some(path) = value(args, "--csv") {
        std::fs::File::create(path)
            .and_then(|mut f| f.write_all(run.category_csv().as_bytes()))
            .map_err(|source| LookdevError::Io {
                path: path.to_string(),
                source,
            })?;
        println!("wrote {path}");
    }
    Ok(())
}
