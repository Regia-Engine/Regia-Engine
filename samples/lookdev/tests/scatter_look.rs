// SPDX-License-Identifier: MIT-0
//! The Mars stand-in rocks read like the UE plates on the `switchbacks_close` camera (lavapipe):
//! rocks darker than the dust they sit on, and stones at a plausible density around the eye.

use lookdev::scatter_shots::{ScatterOptions, ScatterShot, ScatterStage};
use lookdev::{LookdevConfig, Planet};

fn luma(p: &[u8]) -> f32 {
    0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])
}

fn median(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

#[test]
fn mars_rocks_are_darker_than_the_dust_and_dense_enough() {
    let device = regia_render::RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let config = LookdevConfig {
        height_resolution: 513,
        macro_resolution: 32,
        layer_height_resolution: 32,
        ..LookdevConfig::baseline()
    };
    let mut stage = ScatterStage::new(&device, Planet::Mars, &config).expect("stage");
    let options = ScatterOptions {
        width: 480,
        height: 270,
        frames: 4,
        scatter: true,
        // The contact pool darkens the dust beside rocks; judge the rocks alone.
        contact: false,
        gpu_timing: false,
    };
    let shot = ScatterShot::SwitchbacksClose;
    let on = stage.render(&device, shot, options).expect("on").pixels;
    let off = stage
        .render(
            &device,
            shot,
            ScatterOptions {
                scatter: false,
                ..options
            },
        )
        .expect("off")
        .pixels;
    let mut rock = Vec::new();
    let mut ground = Vec::new();
    for i in 0..(options.width * options.height) as usize {
        let d: i32 = (0..3)
            .map(|k| (i32::from(on[i * 4 + k]) - i32::from(off[i * 4 + k])).abs())
            .sum();
        // Rock cores: these stones are a few pixels wide, so weakly changed pixels are
        // anti-aliased mixes of rock and dust.
        if d > 60 {
            rock.push(luma(&on[i * 4..i * 4 + 4]));
            ground.push(luma(&off[i * 4..i * 4 + 4]));
        }
    }
    assert!(rock.len() > 150, "too few rock pixels to judge ({})", rock.len());
    let (rock_median, ground_median) = (median(rock), median(ground));
    eprintln!("rock median luma {rock_median:.1}, dust under them {ground_median:.1}");
    assert!(
        rock_median < ground_median * 0.8,
        "Mars rocks print brighter than the dust: {rock_median:.1} vs {ground_median:.1}"
    );

    // Stones (≥ 5 cm) within a 40 m square around the eye: UE shows a scatter of small
    // stones, not a carpet and not a bare floor.
    let (eye, _, _) = stage.camera(shot);
    stage.field.set_enabled(true);
    let mut count = 0usize;
    stage.field.footprints_in([eye[0], eye[2]], 20.0, |_| count += 1);
    let per_m2 = count as f32 / 1600.0;
    eprintln!("{count} rocks in 1600 m², {per_m2:.2} per m²");
    assert!((0.05..=1.5).contains(&per_m2), "{per_m2:.2} rocks per m²");
}
