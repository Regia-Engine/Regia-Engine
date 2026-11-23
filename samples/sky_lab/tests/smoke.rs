// SPDX-License-Identifier: MIT-0
//! Runs the sample headless for 120 frames: dawn to dusk, fair weather turning to a storm.
//! Without a GPU adapter (not even lavapipe) the tests print a note and pass.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::RenderDevice;
use regia_test_utils::unique_temp_dir;
use sky_lab::lab::{Report, run};
use sky_lab::{DEFAULT_FRAMES, Options};

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip sky_lab smoke: {err}");
            None
        }
    }
}

fn options(out: Option<std::path::PathBuf>) -> Options {
    Options {
        size: (160, 90),
        out,
        every: 40,
        ..Options::default()
    }
}

fn luma_at(report: &Report, frame: u32) -> f32 {
    report.frames[frame as usize].mean_luma
}

#[test]
fn a_day_of_120_frames_renders_and_the_sky_follows_the_sun() {
    let Some(device) = device() else { return };
    let report = run(&device, &options(None)).expect("120 frames render");
    assert_eq!(DEFAULT_FRAMES, 120);
    assert_eq!(report.frames.len(), 120, "one record per frame");
    for (i, frame) in report.frames.iter().enumerate() {
        assert_eq!(frame.frame as usize, i);
        assert!(frame.mean_luma.is_finite(), "frame {i} luma");
    }
    // The run starts and ends with the sun below the horizon (hour 5.5 and 18.5 on the
    // equinox at 45 N) and has it high at midday: the middle of the run is brighter than both
    // ends.
    let first = luma_at(&report, 0);
    let noon = luma_at(&report, 60);
    let last = luma_at(&report, 119);
    eprintln!("sky_lab smoke luma: dawn {first:.2} noon {noon:.2} dusk {last:.2}");
    assert!(
        noon > first + 1.0,
        "noon {noon} is brighter than the first frame {first}"
    );
    assert!(
        noon > last + 1.0,
        "noon {noon} is brighter than the last frame {last}"
    );
    // The weather blend is recorded and runs from the first preset to the last.
    assert!(report.frames[0].coverage < report.frames[119].coverage);
    // The frames are not one repeated image.
    let distinct = report
        .frames
        .windows(2)
        .filter(|w| (w[0].mean_luma - w[1].mean_luma).abs() > f32::EPSILON)
        .count();
    assert!(distinct > 60, "only {distinct} of 119 steps changed the image");
}

#[test]
fn a_run_writes_frames_and_a_report() {
    let Some(device) = device() else { return };
    let dir = unique_temp_dir("sky-lab-smoke");
    let opts = Options {
        scenario: sky_lab::Scenario {
            frames: 41,
            ..sky_lab::Scenario::default()
        },
        ..options(Some(dir.clone()))
    };
    let report = run(&device, &opts).expect("run");
    assert_eq!(report.frames.len(), 41);
    // Every 40th frame and the last: 0, 40.
    for frame in [0, 40] {
        let path = dir.join(format!("frame_{frame:04}.png"));
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{} is a PNG", path.display());
    }
    assert!(
        !dir.join("frame_0001.png").exists(),
        "only every 40th frame is written"
    );
    let text = std::fs::read_to_string(dir.join("report.json")).expect("report.json");
    let json: serde_json::Value = serde_json::from_str(&text).expect("report parses");
    assert_eq!(json["frames"].as_array().map(Vec::len), Some(41));
    assert_eq!(json["width"], 160);
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temp files left behind");
    let _ = std::fs::remove_dir_all(&dir);
}
