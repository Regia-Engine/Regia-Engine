// SPDX-License-Identifier: MIT-0
//! Thermal and hydraulic erosion for the stand-in heightfields.
//!
//! Thermal transport follows Musgrave, Kolb and Mace, "The Synthesis and Rendering of
//! Eroded Fractal Terrains", SIGGRAPH 1989: material moves from a cell to a lower
//! neighbour when the slope exceeds a talus angle. Hydraulic transport is the particle
//! form of Mei, Decaudin and Hu, "Fast Hydraulic Erosion Simulation and Visualization
//! on GPU", Pacific Graphics 2007, run on the CPU with a fixed droplet budget: a drop
//! picks up sediment while below capacity and deposits it when the slope flattens.
//! Both are re-derived from the papers' descriptions. See `docs/provenance.md`.

/// One thermal pass. `talus` is the allowed height difference between 4-neighbours
/// (metres). `rate` is the fraction of the excess moved this pass, in `0..=1`.
///
/// Heights stay finite. The sum of heights is unchanged (no material leaves the grid).
pub fn thermal_pass(heights: &mut [f32], width: u32, height: u32, talus: f32, rate: f32) {
    let width = width as usize;
    let height = height as usize;
    if width < 2 || height < 2 || heights.len() != width * height {
        return;
    }
    let rate = rate.clamp(0.0, 1.0);
    let talus = talus.max(0.0);
    let mut next = heights.to_vec();
    for row in 0..height {
        for col in 0..width {
            let i = row * width + col;
            let h = heights[i];
            let mut excess = [0.0_f32; 4];
            let mut sum = 0.0_f32;
            let mut n = 0_usize;
            for (k, (nr, nc)) in neighbors(row, col, height, width).into_iter().enumerate() {
                let diff = h - heights[nr * width + nc];
                if diff > talus {
                    let over = diff - talus;
                    excess[k] = over;
                    sum += over;
                    n += 1;
                }
            }
            if n == 0 || sum <= 0.0 {
                continue;
            }
            // Share a fraction of the excess. Dividing by the neighbour count keeps one
            // pass from moving the cell below every neighbour (Musgrave's incremental form).
            let budget = sum * rate / n as f32;
            for (k, (nr, nc)) in neighbors(row, col, height, width).into_iter().enumerate() {
                if excess[k] <= 0.0 {
                    continue;
                }
                let mv = budget * (excess[k] / sum);
                next[i] -= mv;
                next[nr * width + nc] += mv;
            }
        }
    }
    heights.copy_from_slice(&next);
}

/// `iterations` thermal passes. No-op when the grid is smaller than 2×2.
pub fn thermal_erode(heights: &mut [f32], width: u32, height: u32, talus: f32, rate: f32, iterations: u32) {
    for _ in 0..iterations {
        thermal_pass(heights, width, height, talus, rate);
    }
}

/// Droplet hydraulic erosion. `drops` particles walk `steps` cells downhill.
///
/// `capacity_k` scales sediment capacity with slope and speed. Out-of-range drops
/// stop. Non-finite samples are left untouched.
pub fn hydraulic_erode(
    heights: &mut [f32],
    width: u32,
    height: u32,
    drops: u32,
    steps: u32,
    capacity_k: f32,
    seed: u32,
) {
    let width_u = width as usize;
    let height_u = height as usize;
    if width_u < 3 || height_u < 3 || heights.len() != width_u * height_u || drops == 0 {
        return;
    }
    let capacity_k = capacity_k.max(0.0);
    for drop in 0..drops {
        let u = regia_terrain::noise::hash2(drop as i32, 1, seed);
        let v = regia_terrain::noise::hash2(drop as i32, 2, seed);
        let mut x = 1.0 + u * (width as f32 - 3.0);
        let mut z = 1.0 + v * (height as f32 - 3.0);
        let mut dx = 0.0_f32;
        let mut dz = 0.0_f32;
        let mut water = 1.0_f32;
        let mut sediment = 0.0_f32;
        for _ in 0..steps {
            if water < 0.01 {
                break;
            }
            let col = x.floor() as usize;
            let row = z.floor() as usize;
            if col < 1 || row < 1 || col + 1 >= width_u || row + 1 >= height_u {
                break;
            }
            let hx = bilinear(heights, width_u, x + 1.0, z) - bilinear(heights, width_u, x - 1.0, z);
            let hz = bilinear(heights, width_u, x, z + 1.0) - bilinear(heights, width_u, x, z - 1.0);
            // Gradient points uphill; the drop follows the negation.
            dx = dx * 0.4 - hx * 0.6;
            dz = dz * 0.4 - hz * 0.6;
            let speed = dx.hypot(dz).max(0.05);
            dx /= speed;
            dz /= speed;
            let slope = hx.hypot(hz).max(0.01);
            let capacity = capacity_k * slope * speed * water;
            let delta = if sediment > capacity {
                let deposit = (sediment - capacity) * 0.4;
                sediment -= deposit;
                deposit
            } else {
                let erode = ((capacity - sediment) * 0.2).min(0.5);
                sediment += erode;
                -erode
            };
            deposit_bilinear(heights, width_u, height_u, x, z, delta);
            x += dx;
            z += dz;
            water *= 0.97;
        }
    }
}

fn neighbors(row: usize, col: usize, height: usize, width: usize) -> [(usize, usize); 4] {
    [
        (row.saturating_sub(1), col),
        (row.min(height - 2) + 1, col),
        (row, col.saturating_sub(1)),
        (row, col.min(width - 2) + 1),
    ]
}

fn bilinear(heights: &[f32], width: usize, x: f32, z: f32) -> f32 {
    let x0 = x.floor() as usize;
    let z0 = z.floor() as usize;
    let tx = x - x0 as f32;
    let tz = z - z0 as f32;
    let i00 = z0 * width + x0;
    let i10 = i00 + 1;
    let i01 = i00 + width;
    let i11 = i01 + 1;
    if i11 >= heights.len() {
        return heights[i00.min(heights.len() - 1)];
    }
    let a = heights[i00] + (heights[i10] - heights[i00]) * tx;
    let b = heights[i01] + (heights[i11] - heights[i01]) * tx;
    a + (b - a) * tz
}

fn deposit_bilinear(heights: &mut [f32], width: usize, height: usize, x: f32, z: f32, amount: f32) {
    if !amount.is_finite() || amount == 0.0 {
        return;
    }
    let x0 = x.floor() as usize;
    let z0 = z.floor() as usize;
    if x0 + 1 >= width || z0 + 1 >= height {
        return;
    }
    let tx = x - x0 as f32;
    let tz = z - z0 as f32;
    let w00 = (1.0 - tx) * (1.0 - tz);
    let w10 = tx * (1.0 - tz);
    let w01 = (1.0 - tx) * tz;
    let w11 = tx * tz;
    let i00 = z0 * width + x0;
    heights[i00] += amount * w00;
    heights[i00 + 1] += amount * w10;
    heights[i00 + width] += amount * w01;
    heights[i00 + width + 1] += amount * w11;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermal_lowers_a_spike_and_keeps_mass() {
        let n = 17_u32;
        let mut heights = vec![10.0_f32; (n * n) as usize];
        let mid = (n / 2) as usize;
        heights[mid * n as usize + mid] = 40.0;
        let before: f32 = heights.iter().sum();
        thermal_erode(&mut heights, n, n, 0.5, 0.5, 8);
        let after: f32 = heights.iter().sum();
        assert!((before - after).abs() < 1.0e-2, "mass {before} -> {after}");
        assert!(heights[mid * n as usize + mid] < 30.0, "spike should slump");
        assert!(heights.iter().all(|h| h.is_finite()));
    }

    #[test]
    fn hydraulic_lowers_a_mound() {
        let n = 24_u32;
        let mut heights = vec![0.0_f32; (n * n) as usize];
        for row in 0..n {
            for col in 0..n {
                let dx = col as f32 - n as f32 * 0.5;
                let dz = row as f32 - n as f32 * 0.5;
                let d = dx.hypot(dz);
                heights[(row * n + col) as usize] = (8.0 - d).max(0.0);
            }
        }
        let mid = (n / 2) as usize;
        let peak_before = heights[mid * n as usize + mid];
        // Drops spawn uniformly and then run downhill, so the summit is only
        // carved when a droplet starts on it. Eighty drops lower this peak by
        // ~0.07 m; a few hundred is what makes the ≥ 0.2 m claim observable.
        hydraulic_erode(&mut heights, n, n, 480, 24, 1.5, 7);
        let peak_after = heights[mid * n as usize + mid];
        assert!(
            peak_after < peak_before - 0.2,
            "peak {peak_before} -> {peak_after}"
        );
        assert!(heights.iter().all(|h| h.is_finite()));
    }
}
