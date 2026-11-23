// SPDX-License-Identifier: MPL-2.0
//! What a fast flythrough of a dense forest costs on the CPU and how often it pops (ADR-1503): the
//! synthetic forest (two canopy classes and an understory, stand-in trees with three LODs and crown
//! parts), a canopy-level path carved collision-free first, then flown at each speed with and
//! without motion LOD, impostors on, wake sway on.
//!
//! ```text
//! cargo run --release -p regia_terrain --example forest_flythrough
//! FOREST_SPEEDS=50,100,150 FOREST_HEIGHT=10 cargo run --release -p regia_terrain --example forest_flythrough
//! # On the showcase heightfield (16-bit PNG, decode h = min + v / 65535 * (max - min)):
//! FOREST_HF=/path/heightfield.png FOREST_HF_SIZE=8191 FOREST_HF_MIN=27.05 FOREST_HF_MAX=421.25 \
//!   FOREST_ZONE=-1700,-1250,-300,250 cargo run --release -p regia_terrain --example forest_flythrough
//! ```
//!
//! Knobs (environment): `FOREST_SPEEDS` (m/s, default `50,100,150`), `FOREST_HEIGHT` (path height above the
//! ground, m, default 10), `FOREST_LENGTH` (path length, m, default 1200), `FOREST_FPS` (default 60),
//! `FOREST_HF*` (a real heightfield instead of the synthetic hills) and `FOREST_ZONE=x0,z0,x1,z1` (where the
//! forest grows on it). Each row prints the CPU time of the field's update + collect (+ sway) per frame
//! (median, 99th percentile, max, µs), the emitted instances and draws, frames that placed a needed tile on
//! the frame thread, and the pops (hard switches, pop-ins, vanishes over 2 px at 2160 p) plus the largest
//! projected radius any LOD switch began at.

use std::sync::Arc;

use regia_terrain::scatter::forest::{
    CameraPath, CorridorSpec, FlythroughSpec, ForestSpec, MotionLod, ScatterQuery, carve_corridor,
    corridor_report, flight_check, run_flythrough, synthetic_forest,
};
use regia_terrain::{DensityMap, ImpostorSettings, ScatterField, TerrainMap};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_list(name: &str, default: &[f32]) -> Vec<f32> {
    regia_core::brand::env_var(name)
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect::<Vec<f32>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_vec())
}

/// A 16-bit grayscale heightfield PNG as a terrain map.
fn load_heightfield(path: &str, size_m: f32, min: f32, max: f32) -> Result<TerrainMap, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(|e| format!("png {path}: {e}"))?;
    let size = reader.output_buffer_size().ok_or("png output size")?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png frame: {e}"))?;
    if info.bit_depth != png::BitDepth::Sixteen || info.color_type != png::ColorType::Grayscale {
        return Err(format!(
            "{path}: expected 16-bit grayscale, got {:?} {:?}",
            info.bit_depth, info.color_type
        ));
    }
    let heights: Vec<f32> = buf[..info.buffer_size()]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| min + f32::from(u16::from_be_bytes(*b)) / 65535.0 * (max - min))
        .collect();
    TerrainMap::from_heights(info.width, info.height, heights, size_m, 1.0, 0.0).map_err(|e| e.to_string())
}

fn main() {
    let speeds = env_list("FOREST_SPEEDS", &[50.0, 100.0, 150.0]);
    let height: f32 = env_or("FOREST_HEIGHT", 10.0);
    let length: f32 = env_or("FOREST_LENGTH", 1200.0);
    let fps: f32 = env_or("FOREST_FPS", 60.0);
    let mut forest = match synthetic_forest(&ForestSpec::default()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("forest: {e}");
            std::process::exit(1);
        }
    };
    // The path runs along +X through the middle of the zone.
    let mut zone = [-length * 0.5 - 100.0, -300.0, length * 0.5 + 100.0, 300.0];
    if let Ok(path) = regia_core::brand::env_var("FOREST_HF") {
        let size: f32 = env_or("FOREST_HF_SIZE", 8191.0);
        let map = match load_heightfield(
            &path,
            size,
            env_or("FOREST_HF_MIN", 0.0),
            env_or("FOREST_HF_MAX", 400.0),
        ) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        let z = env_list("FOREST_ZONE", &[-1700.0, -1250.0, -300.0, 250.0]);
        if z.len() == 4 {
            zone = [z[0], z[1], z[2], z[3]];
        }
        // The forest grows only in the zone: a mask over the whole terrain on the canopy classes and the
        // understory.
        let side = 1024u32;
        let px = size / (side - 1) as f32;
        let mask: Vec<f32> = (0..side * side)
            .map(|i| {
                let (x, zz) = (
                    -size * 0.5 + (i % side) as f32 * px,
                    -size * 0.5 + (i / side) as f32 * px,
                );
                f32::from(u8::from(
                    x >= zone[0] && x <= zone[2] && zz >= zone[1] && zz <= zone[3],
                ))
            })
            .collect();
        let Ok(mask) = DensityMap::new(side, side, mask) else {
            eprintln!("zone mask");
            std::process::exit(1);
        };
        forest.source = regia_terrain::ScatterSource {
            density_maps: [("forest/zone.png".to_string(), mask)].into_iter().collect(),
            ..regia_terrain::ScatterSource::new(Arc::new(map))
        };
        for class in &mut forest.set.classes {
            class.density_map = Some("forest/zone.png".into());
        }
    }
    let map = Arc::clone(&forest.source.map);
    let zc = 0.5 * (zone[1] + zone[3]);
    let x0 = 0.5 * (zone[0] + zone[2]) - length * 0.5;
    let n = (length / 10.0).ceil() as usize;
    let points: Vec<[f32; 3]> = (0..=n)
        .map(|i| {
            let x = x0 + length * i as f32 / n as f32;
            // A gentle weave so the path crosses rows of trees at an angle.
            let z = zc + 25.0 * (x * 0.01).sin();
            [x, map.sample_bilinear(x, z) + height, z]
        })
        .collect();
    let path = CameraPath { points, times: None };
    let Ok(query) = ScatterQuery::new(&forest.set, forest.source.clone(), &forest.library) else {
        eprintln!("query");
        std::process::exit(1);
    };
    let spec = CorridorSpec {
        clearance_m: 2.5,
        ..CorridorSpec::default()
    };
    let before = corridor_report(&query, &path, &spec);
    let carve = carve_corridor(&query, &path, &spec);
    let mut set = forest.set.clone();
    carve.apply(&mut set);
    let carved_query = ScatterQuery::new(&set, forest.source.clone(), &forest.library);
    let after = carved_query.as_ref().map(|q| corridor_report(q, &path, &spec));
    println!(
        "corridor: {:.0} m at {height} m AGL, {} instances measured, {} violators in {} segments, min clearance {:?} m, canopy cover {:.2}; carve removes {} trees, after: {:?} violators",
        before.length_m,
        before.instances_measured,
        before.obstacle_violations,
        before.violating_segments,
        before.min_clearance_m,
        before.canopy_cover,
        carve.removed(),
        after.as_ref().map(|r| r.obstacle_violations).ok(),
    );
    println!(
        "speed,motion_lod,frames,cpu_us_p50,cpu_us_p99,cpu_us_max,mean_emitted,max_emitted,max_draws,hitch_frames,hard_switches,pop_ins,vanishes,soft_switches,max_switch_px,max_tilt_mrad"
    );
    for &speed in &speeds {
        let check = flight_check(
            &set,
            Some(&forest.library),
            &regia_terrain::scatter::forest::FlightPlan {
                speed_m_s: speed,
                fps,
                ..Default::default()
            },
        );
        for class in &check.classes {
            if !class.warnings.is_empty() {
                println!(
                    "# {speed} m/s {}: {:?} (band {:.2} s, prefetch lead {:.2} s, LOD 0 ends {:?} m)",
                    class.class,
                    class.warnings,
                    class.band_crossing_s,
                    class.prefetch_lead_s,
                    class.lod0_end_m
                );
            }
        }
        for motion in [false, true] {
            let mut field = match ScatterField::new(&set, forest.source.clone(), &forest.library, 0x1503) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("field: {e}");
                    std::process::exit(1);
                }
            };
            field.set_impostors(Some(ImpostorSettings::default()));
            if motion {
                field.set_motion_lod(Some(MotionLod::for_flight(speed, fps)));
            }
            // Level load: everything around the start in place before the first frame.
            if let Some(start) = path.points.first() {
                field.settle_streaming(*start);
            }
            let report = run_flythrough(
                &mut field,
                &path,
                &FlythroughSpec {
                    fps,
                    speed_m_s: speed,
                    sway: true,
                    measure_time: true,
                    ..FlythroughSpec::default()
                },
            );
            let [p50, p99, max] = report.cpu_us.unwrap_or([0; 3]);
            println!(
                "{speed},{motion},{},{p50},{p99},{max},{:.0},{},{},{},{},{},{},{},{:.0},{}",
                report.frames.len(),
                report.mean_emitted,
                report.max_emitted,
                report.max_draws,
                report.hitch_frames,
                report.pops.hard_switches,
                report.pops.pop_ins,
                report.pops.vanishes,
                report.pops.soft_switches,
                report.pops.max_switch_px,
                report.max_tilt_mrad,
            );
        }
    }
}
