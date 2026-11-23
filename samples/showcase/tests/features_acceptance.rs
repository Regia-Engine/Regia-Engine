// SPDX-License-Identifier: MIT-0
//! The orchestrator's scene with the optional features on: a frame-time table per feature (one run
//! per configuration) and two runs of the all-on configuration that must be hash-identical.
//!
//! ```text
//! SHOWCASE_TEST_SCENE=<build-dir> SHOWCASE_SIZE=3840x2160 SHOWCASE_PROFILE=Cinematic \
//!   SHOWCASE_FRAMES=120 [SHOWCASE_CAMERA=ex,ey,ez,ax,ay,az] [SHOWCASE_KEEP_DIR=<dir>] REGIA_GPU_UUID=<free card> \
//!   cargo test --release -p showcase --test features_acceptance -- --ignored --nocapture
//! ```

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{JobSpec, Overlay, overlay_scene, run, scene_dir};
use regia_render::RenderProfilePreset;
use showcase::sky::SkyFile;

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn size() -> (u32, u32) {
    std::env::var("SHOWCASE_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1920, 1080))
}

fn profile() -> Option<RenderProfilePreset> {
    std::env::var("SHOWCASE_PROFILE")
        .ok()
        .and_then(|v| RenderProfilePreset::from_name(&v))
}

/// The camera eye and look-at point. `SHOWCASE_CAMERA=ex,ey,ez,ax,ay,az` overrides the default pose, which is below
/// the ground of the orchestrator's scene (terrain 71.5 m at the eye; ADR-1440).
fn camera() -> ([f32; 3], [f32; 3]) {
    let parsed = std::env::var("SHOWCASE_CAMERA").ok().and_then(|v| {
        let n: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        (n.len() == 6).then(|| ([n[0], n[1], n[2]], [n[3], n[4], n[5]]))
    });
    parsed.unwrap_or(([60.0, 46.0, -170.0], [0.0, 36.0, -100.0]))
}

fn spec(scene: std::path::PathBuf, contact_ao: bool) -> JobSpec {
    let (width, height) = size();
    JobSpec {
        width,
        height,
        frames: env_number("SHOWCASE_FRAMES", 120),
        warmup_frames: 8,
        plain: false,
        scatter_lod: 1,
        scene: Some(scene),
        camera: Some(camera()),
        gpu_driven: true,
        profile: profile(),
        contact_ao,
        ..JobSpec::default()
    }
}

/// `SHOWCASE_KEEP_DIR=<dir>` keeps the first and last frame of each configuration in `<dir>/<config>` (for a look
/// and for comparing two builds).
fn keep_dir(config: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("SHOWCASE_KEEP_DIR").map(|dir| std::path::PathBuf::from(dir).join(config))
}

/// Prints a configuration's frame hashes (first, last, the job's sequence hash) so two builds can be compared.
fn print_hashes(config: &str, out: &common::Outcome) {
    eprintln!(
        "hashes ({config}): frames {} first {} last {} sequence {}",
        out.hashes.len(),
        out.hashes.first().map_or("-", String::as_str),
        out.hashes.last().map_or("-", String::as_str),
        out.sequence
    );
    if let Some(dir) = keep_dir(config) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("hashes.txt"), out.hashes.join("\n"));
    }
}

#[test]
#[ignore = "needs a real GPU and the orchestrator's scene; run by the lead on buildhost"]
fn frame_time_per_feature_and_two_identical_runs() {
    let src = scene_dir();
    let none = Overlay::default();
    let configs: [(&str, Overlay, bool); 7] = [
        ("baseline", none, false),
        ("grass", Overlay { grass: true, ..none }, false),
        ("mist", Overlay { mist: true, ..none }, false),
        ("water", Overlay { water: true, ..none }, false),
        ("entry", Overlay { entry: true, ..none }, false),
        ("contact_ao", none, true),
        (
            "all",
            Overlay {
                grass: true,
                mist: true,
                water: true,
                entry: true,
            },
            true,
        ),
    ];
    eprintln!("| config | job ms | ms/frame | load ms | RSS MiB |");
    eprintln!("|---|---|---|---|---|");
    let mut all_hashes = Vec::new();
    for (index, (name, overlay, contact)) in configs.into_iter().enumerate() {
        let dir = overlay_scene(&src, &format!("showcase-features-{name}"), overlay);
        let job = JobSpec {
            keep: keep_dir(name),
            ..spec(dir.clone(), contact)
        };
        let frames = job.frames;
        let out = run(index as u32 + 1, &job);
        print_hashes(name, &out);
        let stats = out.stats.as_ref().expect("stats");
        assert_eq!(
            stats.grass_installed, overlay.grass,
            "{name}: grass {:?}",
            stats.grass_error
        );
        assert_eq!(stats.contact_installed, contact, "{name}");
        eprintln!(
            "| {name} | {} | {:.2} | {} | {} |",
            out.job_ms,
            out.job_ms as f64 / frames as f64,
            out.load_ms,
            out.peak_rss_kib / 1024
        );
        eprintln!("gpu timings ({name}): {}", out.gpu_timings);
        if name == "all" {
            all_hashes.push((out.hashes, out.sequence));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
    let dir = overlay_scene(
        &src,
        "showcase-features-all-b",
        Overlay {
            grass: true,
            mist: true,
            water: true,
            entry: true,
        },
    );
    let again = run(
        20,
        &JobSpec {
            keep: keep_dir("all-b"),
            ..spec(dir.clone(), true)
        },
    );
    print_hashes("all-b", &again);
    let _ = std::fs::remove_dir_all(dir);
    let (first_hashes, first_sequence) = all_hashes.pop().expect("the all-on run");
    assert_eq!(first_hashes, again.hashes, "two all-on runs give the same frames");
    assert_eq!(first_sequence, again.sequence);
}

/// The configurations without mist strip the source's `mist:` entry whole, however many lines it
/// spans: the orchestrator's scene writes it over two (`grid_cell_m` on the second), and leaving
/// that line behind made every no-mist configuration fail to load (`sky_malformed`). Runs without
/// a GPU or the orchestrator's scene.
#[test]
fn the_no_mist_overlay_strips_a_mist_entry_over_several_lines() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene");
    let base = std::fs::read_to_string(example.join("sky.ron")).expect("example sky.ron");
    let end = base.rfind(')').expect("a closing paren");
    let mut misty = base.clone();
    misty.insert_str(
        end,
        "    mist: Some((density: 0.0025, max_height_m: 35.0, wind_m_per_s: (0.9, 0.3),\n                grid_cell_m: 32.0, concavity_radius_m: 300.0)),\n",
    );
    let path = std::path::Path::new("sky.ron");
    let parsed = SkyFile::from_ron(&misty, path).expect("the misty sky parses");
    assert!(parsed.mist.is_some());
    let stripped = common::strip_mist(&misty);
    let parsed = SkyFile::from_ron(&stripped, path).unwrap_or_else(|err| panic!("{err:?}\n{stripped}"));
    assert!(parsed.mist.is_none(), "{stripped}");
    assert!(!stripped.contains("grid_cell_m"), "{stripped}");
    // A sky without mist is left as it was.
    assert_eq!(common::strip_mist(&base), base);
}
