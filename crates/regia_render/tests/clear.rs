// SPDX-License-Identifier: MPL-2.0
//! M1: the clear pass produces the requested colour on a real (or software) GPU.
//! CI runs this on Mesa lavapipe (Linux) and WARP (Windows).

use regia_render::{RenderDevice, clear_to_pixels};

#[test]
fn clear_color_offscreen() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let (w, h) = (64, 32);
    let pixels = match clear_to_pixels(&device, [0.25, 0.5, 0.75, 1.0], w, h) {
        Ok(p) => p,
        Err(err) => panic!("offscreen clear failed: {err}"),
    };
    assert_eq!(pixels.len(), (w * h * 4) as usize);
    // Unorm8 of 0.25/0.5/0.75/1.0 is 63.75/127.5/191.25/255; GPUs may round the halves either way.
    let expected = [64i32, 128, 191, 255];
    for (i, px) in pixels.as_chunks::<4>().0.iter().enumerate() {
        for c in 0..4 {
            let delta = (i32::from(px[c]) - expected[c]).abs();
            assert!(
                delta <= 1,
                "pixel {i} channel {c}: got {}, expected {}",
                px[c],
                expected[c]
            );
        }
    }
}
