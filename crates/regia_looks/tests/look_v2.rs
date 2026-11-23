// SPDX-License-Identifier: MPL-2.0
//! The look corrections of ADR-2587, measured on rendered pixels (lavapipe on CI hosts, the same
//! renderer the look board uses). No goldens: each bar is a property of the image. The bars were
//! set against the numbers the previous look gave; the lane report lists both.
#![cfg(feature = "gpu")]

use glam::{Mat4, Vec3};
use regia_looks::gpu::{LookCamera, LookItem, LookRenderer, LookScene, headless_device};
use regia_looks::{CutType, GemCut, Library, LookMesh, StoneSize, Studio};

const SIZE: u32 = 256;

fn renderer() -> Option<LookRenderer> {
    match headless_device() {
        Ok((device, queue, _)) => Some(LookRenderer::new(
            device,
            queue,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            4,
            Studio::default(),
        )),
        Err(e) => {
            assert!(
                std::env::var_os("REGIA_LOOKS_REQUIRE_GPU").is_none(),
                "no GPU: {e}"
            );
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn camera() -> LookCamera {
    LookCamera {
        eye: Vec3::new(0.0, 0.0, 40.0),
        target: Vec3::ZERO,
        fov_y_deg: 11.0,
    }
}

fn stone(cut_type: CutType, id: &str, tilt_deg: f32) -> LookScene {
    let lib = Library::builtin().expect("library");
    let cut = GemCut::new(cut_type, [6.0, 6.0]);
    LookScene {
        camera: camera(),
        items: vec![LookItem {
            mesh: cut.mesh(),
            material: lib.look(id, None).expect("stone"),
            model: Mat4::from_rotation_x(-tilt_deg.to_radians()),
            stone: cut.stone_size(),
        }],
        exposure: 1.0,
    }
}

fn opal(id: &str, tilt_deg: f32) -> LookScene {
    let lib = Library::builtin().expect("library");
    LookScene {
        camera: camera(),
        items: vec![LookItem {
            mesh: LookMesh::cabochon(6.0, 2.0, 64, 20),
            material: lib.look(id, None).expect("opal"),
            model: Mat4::from_rotation_x(-tilt_deg.to_radians()),
            stone: StoneSize::round(6.0, 2.0),
        }],
        exposure: 1.0,
    }
}

fn ball(id: &str, finish: &str) -> LookScene {
    let lib = Library::builtin().expect("library");
    LookScene {
        camera: camera(),
        items: vec![LookItem {
            mesh: LookMesh::ball(6.0, 64, 32),
            material: lib.look(id, Some(finish)).expect("metal"),
            model: Mat4::IDENTITY,
            stone: StoneSize::default(),
        }],
        exposure: 1.0,
    }
}

/// Encoded RGB in 0..1, row by row.
fn rgb(img: &[u8]) -> Vec<[f32; 3]> {
    img.as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        })
        .collect()
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn chroma(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

/// The pixels the scene drew on: different from the empty scene's background.
fn mask(r: &mut LookRenderer, scene: &LookScene, pixels: &[[f32; 3]]) -> Vec<bool> {
    let empty = LookScene {
        camera: scene.camera,
        items: Vec::new(),
        exposure: scene.exposure,
    };
    let bg = rgb(&r.render_still(&empty, SIZE, SIZE).expect("background"));
    pixels
        .iter()
        .zip(&bg)
        .map(|(a, b)| (0..3).any(|k| (a[k] - b[k]).abs() > 0.02))
        .collect()
}

/// Indices of the centered square covering `half` of the image's half width on each side.
fn center(half: f32) -> impl Iterator<Item = usize> {
    let s = SIZE as usize;
    let lo = ((0.5 - half * 0.5) * SIZE as f32) as usize;
    let hi = ((0.5 + half * 0.5) * SIZE as f32) as usize;
    (lo..hi).flat_map(move |y| (lo..hi).map(move |x| y * s + x))
}

fn percentile(mut v: Vec<f32>, p: f32) -> f32 {
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * p) as usize]
}

/// Hue in degrees and saturation of an encoded color.
fn hue_sat(c: [f32; 3]) -> (f32, f32) {
    let max = c[0].max(c[1]).max(c[2]);
    let d = chroma(c);
    if d < 1e-4 {
        return (0.0, 0.0);
    }
    let h = if max == c[0] {
        ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if max == c[1] {
        (c[2] - c[0]) / d + 2.0
    } else {
        (c[0] - c[1]) / d + 4.0
    };
    (h * 60.0, d / max)
}

/// The share of neighboring pixel pairs inside the center with a luma step: how much facet pattern
/// a region shows.
fn edge_density(px: &[[f32; 3]], half: f32) -> f32 {
    let s = SIZE as usize;
    let (mut edges, mut pairs) = (0u32, 0u32);
    for i in center(half) {
        for j in [i + 1, i + s] {
            pairs += 1;
            if (luma(px[i]) - luma(px[j])).abs() > 0.06 {
                edges += 1;
            }
        }
    }
    edges as f32 / pairs as f32
}

/// Bright pixels over 12 frames of a tilt drag and how many are single-frame pops, as the sparkle
/// test of `gpu_looks` counts them.
fn drag_pops(
    r: &mut LookRenderer,
    frame: impl Fn(f32) -> LookScene,
    step: f32,
    bright: f32,
    dark: f32,
) -> (usize, usize) {
    let frames: Vec<Vec<f32>> = (0..12)
        .map(|k| {
            let img = r
                .render_still(&frame(8.0 + k as f32 * step), SIZE, SIZE)
                .expect("frame");
            rgb(&img).into_iter().map(luma).collect()
        })
        .collect();
    let (mut maxima, mut single) = (0, 0);
    for k in 1..frames.len() - 1 {
        for (p, &v) in frames[k].iter().enumerate() {
            if v > bright {
                maxima += 1;
                if frames[k - 1][p] < dark && frames[k + 1][p] < dark {
                    single += 1;
                }
            }
        }
    }
    (maxima, single)
}

#[test]
fn princess_shows_pavilion_facets_through_the_table() {
    let Some(mut r) = renderer() else { return };
    let px = rgb(&r
        .render_still(&stone(CutType::Princess, "emerald-green", 12.0), SIZE, SIZE)
        .expect("frame"));
    let lumas: Vec<f32> = center(0.36).map(|i| luma(px[i])).collect();
    let spread = percentile(lumas.clone(), 0.9) - percentile(lumas, 0.1);
    let edges = edge_density(&px, 0.36);
    eprintln!("METRIC princess spread {spread:.3} edges {edges:.4}");
    assert!(
        spread > 0.25,
        "the table is one flat value: p90 - p10 = {spread:.3}"
    );
    assert!(edges > 0.04, "no facet pattern under the table: {edges:.4}");
}

/// The largest connected patch of near-constant color inside the center, as a share of the center.
fn largest_flat_patch(px: &[[f32; 3]], half: f32) -> f32 {
    let s = SIZE as usize;
    let cells: Vec<usize> = center(half).collect();
    let inside: std::collections::HashSet<usize> = cells.iter().copied().collect();
    let mut seen = std::collections::HashSet::new();
    let mut largest = 0usize;
    for &start in &cells {
        if !seen.insert(start) {
            continue;
        }
        let mut stack = vec![start];
        let mut size = 0usize;
        while let Some(i) = stack.pop() {
            size += 1;
            for j in [i + 1, i.wrapping_sub(1), i + s, i.wrapping_sub(s)] {
                if inside.contains(&j)
                    && !seen.contains(&j)
                    && (0..3).all(|k| (px[i][k] - px[j][k]).abs() < 0.015)
                {
                    seen.insert(j);
                    stack.push(j);
                }
            }
        }
        largest = largest.max(size);
    }
    largest as f32 / cells.len() as f32
}

/// The reported fault: one dark flat pane fills the table, so the stone reads as a glass block.
#[test]
fn princess_table_is_not_one_flat_pane() {
    let Some(mut r) = renderer() else { return };
    let mut worst = 0.0f32;
    for tilt in [8.0, 12.0, 16.0, 22.0] {
        let px = rgb(&r
            .render_still(&stone(CutType::Princess, "emerald-green", tilt), SIZE, SIZE)
            .expect("frame"));
        let flat = largest_flat_patch(&px, 0.36);
        eprintln!("METRIC princess flat patch tilt {tilt}: {flat:.4}");
        worst = worst.max(flat);
    }
    assert!(worst < 0.12, "one flat pane covers {worst:.4} of the table");
}

#[test]
fn princess_facets_flash_as_the_view_turns() {
    let Some(mut r) = renderer() else { return };
    let frames: Vec<Vec<[f32; 3]>> = (0..9)
        .map(|k| {
            rgb(&r
                .render_still(
                    &stone(CutType::Princess, "emerald-green", 8.0 + k as f32),
                    SIZE,
                    SIZE,
                )
                .expect("frame"))
        })
        .collect();
    let mut changed = 0.0;
    for pair in frames.windows(2) {
        let n = center(0.36).count() as f32;
        changed += center(0.36)
            .filter(|&i| (luma(pair[0][i]) - luma(pair[1][i])).abs() > 0.12)
            .count() as f32
            / n;
    }
    let changed = changed / (frames.len() - 1) as f32;
    eprintln!("METRIC princess flash {changed:.4}");
    assert!(changed > 0.12, "too little changes per degree: {changed:.4}");
}

#[test]
fn princess_stays_stable_during_a_slow_drag() {
    let Some(mut r) = renderer() else { return };
    let (maxima, single) = drag_pops(
        &mut r,
        |t| stone(CutType::Princess, "emerald-green", t),
        0.25,
        0.55,
        0.3,
    );
    let ratio = single as f64 / maxima.max(1) as f64;
    eprintln!("METRIC princess drag maxima {maxima} single {single} ratio {ratio:.4}");
    assert!(maxima > 100, "the stone flashes at all: {maxima}");
    assert!(
        ratio <= 0.10,
        "single-frame ratio {ratio:.4} > 0.10 (ADR-2514's bar)"
    );
}

/// Connected components of colorful stone pixels: (largest as a share of the stone, how many).
fn fleck_components(px: &[[f32; 3]], stone_px: &[bool]) -> (f32, usize) {
    let s = SIZE as usize;
    let colorful: Vec<bool> = px
        .iter()
        .zip(stone_px)
        .map(|(c, m)| *m && chroma(*c) > 0.25)
        .collect();
    let mut seen = vec![false; colorful.len()];
    let (mut largest, mut count) = (0usize, 0usize);
    for start in 0..colorful.len() {
        if !colorful[start] || seen[start] {
            continue;
        }
        let mut stack = vec![start];
        seen[start] = true;
        let mut size = 0usize;
        while let Some(i) = stack.pop() {
            size += 1;
            let mut visit = |j: usize, stack: &mut Vec<usize>| {
                if colorful[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if i % s + 1 < s {
                visit(i + 1, &mut stack);
            }
            if i % s > 0 {
                visit(i - 1, &mut stack);
            }
            if i + s < colorful.len() {
                visit(i + s, &mut stack);
            }
            if i >= s {
                visit(i - s, &mut stack);
            }
        }
        largest = largest.max(size);
        if size >= 3 {
            count += 1;
        }
    }
    let total = stone_px.iter().filter(|m| **m).count().max(1);
    (largest as f32 / total as f32, count)
}

#[test]
fn dark_opal_flecks_are_small_and_many() {
    let Some(mut r) = renderer() else { return };
    let scene = opal("opal-dark", 22.0);
    let px = rgb(&r.render_still(&scene, SIZE, SIZE).expect("frame"));
    let m = mask(&mut r, &scene, &px);
    let (largest, count) = fleck_components(&px, &m);
    eprintln!("METRIC dark opal largest {largest:.4} count {count}");
    assert!(
        largest < 0.006,
        "big flat confetti: largest fleck is {largest:.4} of the stone"
    );
    assert!(count >= 60, "too few flecks: {count}");
}

#[test]
fn opal_color_follows_the_view() {
    let Some(mut r) = renderer() else { return };
    let a = rgb(&r.render_still(&opal("opal-dark", 18.0), SIZE, SIZE).expect("a"));
    let b = rgb(&r.render_still(&opal("opal-dark", 26.0), SIZE, SIZE).expect("b"));
    let lit: Vec<usize> = (0..a.len()).filter(|&i| chroma(a[i]) > 0.25).collect();
    let shifted = lit
        .iter()
        .filter(|&&i| {
            let d = (0..3).map(|k| (a[i][k] - b[i][k]).abs()).fold(0.0, f32::max);
            d > 0.15
        })
        .count();
    let share = shifted as f32 / lit.len().max(1) as f32;
    eprintln!("METRIC opal view shift {share:.4} of {} lit", lit.len());
    assert!(lit.len() > 300, "the play is visible at all: {}", lit.len());
    assert!(share > 0.7, "color does not follow the view: {share:.4}");
    // The hue itself moves, not only the brightness: pixels lit in both views.
    let both: Vec<f32> = lit
        .iter()
        .filter(|&&i| chroma(b[i]) > 0.25)
        .map(|&i| {
            let d = (hue_sat(a[i]).0 - hue_sat(b[i]).0).abs();
            d.min(360.0 - d)
        })
        .collect();
    let hue = percentile(both.clone(), 0.5);
    eprintln!("METRIC opal hue shift median {hue:.1} of {}", both.len());
    assert!(
        both.len() > 50,
        "too few pixels stay lit to compare: {}",
        both.len()
    );
    assert!(hue > 25.0, "the hue barely moves over 8 degrees: {hue:.1}");
}

#[test]
fn opal_stays_stable_during_a_slow_drag() {
    let Some(mut r) = renderer() else { return };
    for id in ["opal-dark", "opal-white"] {
        let (maxima, single) = drag_pops(&mut r, |t| opal(id, t), 0.25, 0.55, 0.3);
        let ratio = single as f64 / maxima.max(1) as f64;
        eprintln!("METRIC {id} drag maxima {maxima} single {single} ratio {ratio:.4}");
        assert!(ratio <= 0.10, "{id}: single-frame ratio {ratio:.4} > 0.10");
    }
}

#[test]
fn white_opal_is_not_flat() {
    let Some(mut r) = renderer() else { return };
    let scene = opal("opal-white", 22.0);
    let px = rgb(&r.render_still(&scene, SIZE, SIZE).expect("frame"));
    let m = mask(&mut r, &scene, &px);
    let stone: Vec<[f32; 3]> = px.iter().zip(&m).filter(|(_, m)| **m).map(|(c, _)| *c).collect();
    let colorful = stone.iter().filter(|c| chroma(**c) > 0.12).count() as f32 / stone.len() as f32;
    let median = percentile(stone.iter().map(|c| luma(*c)).collect(), 0.5);
    eprintln!("METRIC white opal colorful {colorful:.4} median {median:.3}");
    assert!(colorful > 0.12, "the play is lost in the body: {colorful:.4}");
    assert!(median > 0.55, "a white opal stays light: {median:.3}");
    assert!(
        median < 0.9,
        "the body must not sit at the top of the tone curve: {median:.3}"
    );
}

#[test]
fn yellow_gold_reads_warm_and_saturated() {
    let Some(mut r) = renderer() else { return };
    let px = rgb(&r
        .render_still(&ball("gold-yellow-18k", "finish-polish"), SIZE, SIZE)
        .expect("frame"));
    let mids: Vec<[f32; 3]> = px
        .iter()
        .copied()
        .filter(|c| (0.3..0.8).contains(&luma(*c)) && chroma(*c) > 0.05)
        .collect();
    let hue = percentile(mids.iter().map(|c| hue_sat(*c).0).collect(), 0.5);
    let sat = percentile(mids.iter().map(|c| hue_sat(*c).1).collect(), 0.5);
    eprintln!("METRIC gold hue {hue:.1} sat {sat:.3} of {}", mids.len());
    assert!((35.0..52.0).contains(&hue), "yellow gold hue {hue:.1}");
    assert!(sat > 0.5, "pale gold: saturation {sat:.3}");
}

#[test]
fn sandblasted_steel_is_a_dark_rough_metal() {
    let Some(mut r) = renderer() else { return };
    let scene = ball("steel", "finish-sandblast");
    let px = rgb(&r.render_still(&scene, SIZE, SIZE).expect("frame"));
    let m = mask(&mut r, &scene, &px);
    let stone: Vec<[f32; 3]> = px.iter().zip(&m).filter(|(_, m)| **m).map(|(c, _)| *c).collect();
    let lumas: Vec<f32> = stone.iter().map(|c| luma(*c)).collect();
    let median = percentile(lumas.clone(), 0.5);
    let contrast = percentile(lumas.clone(), 0.95) - percentile(lumas, 0.05);
    let sat = percentile(stone.iter().map(|c| hue_sat(*c).1).collect(), 0.5);
    eprintln!("METRIC steel median {median:.3} contrast {contrast:.3} sat {sat:.3}");
    assert!(
        (0.35..0.62).contains(&median),
        "white ceramic or black: median {median:.3}"
    );
    assert!(contrast > 0.2, "flat, no metallic lobe: {contrast:.3}");
    assert!(sat < 0.12, "steel stays neutral: {sat:.3}");
}

/// A blasted ball turning slowly, near (grains resolved) and far (grains below a pixel, faded out):
/// the grain moves with the piece and does not sparkle on and off.
#[test]
fn blasted_grain_is_stable_while_turning() {
    let Some(mut r) = renderer() else { return };
    for (name, eye_z) in [("near", 40.0f32), ("far", 220.0)] {
        let frame = |t: f32| {
            let mut scene = ball("steel", "finish-sandblast");
            scene.camera.eye = Vec3::new(0.0, 0.0, eye_z);
            scene.items[0].model = Mat4::from_rotation_x(-t.to_radians());
            scene
        };
        let (maxima, single) = drag_pops(&mut r, frame, 0.25, 0.6, 0.35);
        let ratio = single as f64 / maxima.max(1) as f64;
        eprintln!("METRIC blasted {name} drag maxima {maxima} single {single} ratio {ratio:.4}");
        assert!(maxima > 50, "{name}: lit at all: {maxima}");
        assert!(ratio <= 0.10, "{name}: single-frame ratio {ratio:.4} > 0.10");
    }
}

#[test]
fn yellow_round_is_light_in_the_center() {
    let Some(mut r) = renderer() else { return };
    let px = rgb(&r
        .render_still(&stone(CutType::RoundBrilliant, "citrine", 22.0), SIZE, SIZE)
        .expect("frame"));
    let center_px: Vec<[f32; 3]> = center(0.3).map(|i| px[i]).collect();
    let n = center_px.len() as f32;
    let mean = |k: usize| center_px.iter().map(|c| c[k]).sum::<f32>() / n;
    let (m_r, m_g, m_b) = (mean(0), mean(1), mean(2));
    let l = luma([m_r, m_g, m_b]);
    eprintln!("METRIC citrine center luma {l:.3} rgb {m_r:.3} {m_g:.3} {m_b:.3}");
    assert!(l > 0.5, "dark center: mean luma {l:.3}");
    // Dark yellow reads as olive: the center has to reach a real yellow, not only a middling luma.
    assert!(m_r > 0.7, "olive center: red {m_r:.3}");
    assert!(
        m_r > m_g && m_g > m_b && m_r - m_b > 0.25,
        "still yellow: {m_r:.3} {m_g:.3} {m_b:.3}"
    );
}

/// The extra lift for a pale quartz stays off a deep one: the amethyst keeps dark zones. Without the
/// gate on absorption this reads 0.083; with it, the same 0.063 as before the lift.
#[test]
fn a_deep_quartz_keeps_its_dark_zones() {
    let Some(mut r) = renderer() else { return };
    let scene = stone(CutType::RoundBrilliant, "amethyst", 22.0);
    let px = rgb(&r.render_still(&scene, SIZE, SIZE).expect("frame"));
    let on = mask(&mut r, &scene, &px);
    let mut lumas: Vec<f32> = px
        .iter()
        .zip(&on)
        .filter(|(_, m)| **m)
        .map(|(c, _)| luma(*c))
        .collect();
    lumas.sort_by(f32::total_cmp);
    let p10 = lumas[lumas.len() / 10];
    eprintln!("METRIC amethyst darkest decile luma {p10:.3}");
    assert!(p10 < 0.075, "amethyst dark zones washed out: {p10:.3}");
}

/// A look that already reads well keeps its pixels: only the marked materials change.
#[test]
fn polished_metals_and_round_stones_stay_put() {
    let Some(mut r) = renderer() else { return };
    let polished = r
        .render_still(&ball("platinum", "finish-polish"), 64, 64)
        .expect("a");
    let brushed = r
        .render_still(&ball("platinum", "finish-brushed"), 64, 64)
        .expect("b");
    assert_ne!(polished, brushed, "finishes still differ");
    let again = r
        .render_still(&ball("platinum", "finish-polish"), 64, 64)
        .expect("c");
    assert_eq!(polished, again, "a still view renders the same bytes");
    let blasted = stone(CutType::Princess, "emerald-green", 12.0);
    let a = r.render_still(&blasted, 64, 64).expect("d");
    let b = r.render_still(&blasted, 64, 64).expect("e");
    assert_eq!(a, b, "the step pavilion has no noise and no history");
}
