// SPDX-License-Identifier: MPL-2.0
//! A view below the geometric horizon is the sunlit planet, not the clear colour.
//!
//! The sky-view LUT stores only air inscatter. A ray from a few metres up that looks
//! past a floor edge meets the planet in tens of metres, so that inscatter is nearly
//! black. The visible sky composites the Lambertian surface behind it.

use regia_render::{AtmosphereParameters, RenderDevice, SkyView, TimeOfDay, render_sky_view};

fn device() -> Option<RenderDevice> {
    match RenderDevice::shared_headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip horizon ground: {err}");
            None
        }
    }
}

fn luma(px: &[u8]) -> f32 {
    0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32
}

#[test]
fn below_horizon_sky_is_not_black() {
    let Some(device) = device() else { return };
    // Look ≈ −8.5°. A 50° vertical field spans about +16.5° to −33.5°, so the
    // horizon sits near a third of the way down and the lower half is the planet.
    let image = render_sky_view(
        &device,
        &SkyView {
            time: TimeOfDay::noon(),
            planet: AtmosphereParameters::earth(),
            body: None,
            size: (96, 64),
            fov_y: 50.0_f32.to_radians(),
            look_dir: [0.0, -0.15, 1.0],
            panorama: false,
        },
    )
    .expect("sky");
    let width = image.width;
    let height = image.height;
    let pixels = image.pixels.as_chunks::<4>().0;
    let y_sky0 = (0.05 * height as f32) as u32;
    let y_sky1 = (0.25 * height as f32) as u32;
    let y_gnd0 = (0.55 * height as f32) as u32;
    let y_gnd1 = (0.95 * height as f32) as u32;
    let mut sky_l = 0.0f32;
    let mut sky_r = 0.0f32;
    let mut sky_b = 0.0f32;
    let mut sky_n = 0.0f32;
    let mut gnd_l = 0.0f32;
    let mut gnd_n = 0.0f32;
    let mut gnd_dark = 0.0f32;
    for (index, px) in pixels.iter().enumerate() {
        let y = (index as u32) / width;
        if (y_sky0..y_sky1).contains(&y) {
            sky_l += luma(px);
            sky_r += px[0] as f32;
            sky_b += px[2] as f32;
            sky_n += 1.0;
        }
        if (y_gnd0..y_gnd1).contains(&y) {
            gnd_l += luma(px);
            gnd_n += 1.0;
            if px[0] < 8 && px[1] < 8 && px[2] < 8 {
                gnd_dark += 1.0;
            }
        }
    }
    let sky_n = sky_n.max(1.0);
    let gnd_n = gnd_n.max(1.0);
    assert!(
        sky_b > sky_r && sky_l / sky_n > 20.0,
        "the upper band should stay a blue sky, luma {} rgb ({}, …, {})",
        sky_l / sky_n,
        sky_r / sky_n,
        sky_b / sky_n
    );
    assert!(
        gnd_l / gnd_n > 18.0 && gnd_dark / gnd_n < 0.05,
        "below the horizon is black: mean luma {:.1}, dark fraction {:.2}",
        gnd_l / gnd_n,
        gnd_dark / gnd_n
    );
}

/// ADR-162: an airless world has no air to fade the planet surface into the horizon, so by
/// default (`OutdoorParams::planet_surface` = `None`) the sky leaves rays that miss the
/// scene black. Past the far edge of a finite map the uniform surface read as a flat band.
#[test]
fn airless_below_horizon_stays_space() {
    let Some(device) = device() else { return };
    let image = render_sky_view(
        &device,
        &SkyView {
            time: TimeOfDay::noon(),
            planet: AtmosphereParameters::airless(),
            body: None,
            size: (96, 64),
            fov_y: 50.0_f32.to_radians(),
            look_dir: [0.0, -0.15, 1.0],
            panorama: false,
        },
    )
    .expect("sky");
    let width = image.width;
    let height = image.height;
    let y0 = (0.55 * height as f32) as u32;
    let y1 = (0.95 * height as f32) as u32;
    let mut n = 0.0f32;
    let mut dark = 0.0f32;
    let mut peak = 0.0f32;
    for (index, px) in image.pixels.as_chunks::<4>().0.iter().enumerate() {
        let y = (index as u32) / width;
        if (y0..y1).contains(&y) {
            n += 1.0;
            peak = peak.max(luma(px));
            if px[0] < 8 && px[1] < 8 && px[2] < 8 {
                dark += 1.0;
            }
        }
    }
    assert!(
        dark / n.max(1.0) > 0.99,
        "an airless sky below the horizon should be black space: dark fraction {:.2}, peak luma {peak:.1}",
        dark / n.max(1.0)
    );
}
