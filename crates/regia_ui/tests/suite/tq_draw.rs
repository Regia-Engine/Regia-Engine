// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): draw-quad geometry: rect intersection, clipping, shadow margins,
//! nine-slice partitioning, GPU instance conversion and batching. Pure data; no GPU, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_test_utils::fuzz::FuzzRng;
use regia_ui::UiColor;
use regia_ui::draw::{
    BatchRange, NO_CLIP, QuadInstance, TextureRef, UiQuad, batch, batch_ranges, instance_of, intersect,
    nine_slice,
};

fn quad(pos: [f32; 2], size: [f32; 2]) -> UiQuad {
    UiQuad::fill(pos, size, UiColor::WHITE)
}

fn frac(rng: &mut FuzzRng) -> f32 {
    rng.below(1001) as f32 / 1000.0
}

#[test]
fn intersection_edges() {
    assert_eq!(
        intersect([0.0, 0.0, 10.0, 10.0], [5.0, 5.0, 10.0, 10.0]),
        [5.0, 5.0, 5.0, 5.0]
    );
    assert_eq!(
        intersect([0.0, 0.0, 10.0, 10.0], [2.0, 3.0, 4.0, 5.0]),
        [2.0, 3.0, 4.0, 5.0],
        "contained"
    );
    let touching = intersect([0.0, 0.0, 10.0, 10.0], [10.0, 0.0, 10.0, 10.0]);
    assert_eq!(&touching[2..], &[0.0, 10.0], "touching edges have no width");
    let apart = intersect([0.0, 0.0, 10.0, 10.0], [50.0, 50.0, 10.0, 10.0]);
    assert_eq!(
        &apart[2..],
        &[0.0, 0.0],
        "disjoint rects give zero size, never negative"
    );
    let all = intersect(NO_CLIP, [3.0, 4.0, 5.0, 6.0]);
    assert_eq!(all, [3.0, 4.0, 5.0, 6.0], "NO_CLIP clips nothing");
    let zero = intersect([0.0, 0.0, 0.0, 0.0], [0.0, 0.0, 10.0, 10.0]);
    assert_eq!(&zero[2..], &[0.0, 0.0]);
    let mut rng = FuzzRng::new(12);
    for _ in 0..500 {
        let a = [
            frac(&mut rng) * 100.0,
            frac(&mut rng) * 100.0,
            frac(&mut rng) * 100.0,
            frac(&mut rng) * 100.0,
        ];
        let b = [
            frac(&mut rng) * 100.0,
            frac(&mut rng) * 100.0,
            frac(&mut rng) * 100.0,
            frac(&mut rng) * 100.0,
        ];
        let r = intersect(a, b);
        assert_eq!(r, intersect(b, a), "commutative");
        assert!(r[2] >= 0.0 && r[3] >= 0.0);
        if r[2] > 0.0 && r[3] > 0.0 {
            for rect in [a, b] {
                assert!(r[0] >= rect[0] - 1e-4 && r[1] >= rect[1] - 1e-4);
                assert!(r[0] + r[2] <= rect[0] + rect[2] + 1e-4 && r[1] + r[3] <= rect[1] + rect[3] + 1e-4);
            }
        }
    }
}

#[test]
fn shadow_margins_and_bounds() {
    let mut q = quad([10.0, 10.0], [20.0, 20.0]);
    assert_eq!(q.shadow_margin(), 0.0);
    assert_eq!(q.bounds(), [10.0, 10.0, 20.0, 20.0]);
    for (blur, margin) in [(0.1, 1.0), (1.0, 2.0), (10.0, 15.0), (10.1, 16.0)] {
        q.blur = blur;
        assert_eq!(q.shadow_margin(), margin, "blur {blur}");
    }
    q.blur = 10.0;
    assert_eq!(q.bounds(), [-5.0, -5.0, 50.0, 50.0]);
    q.blur = -3.0;
    assert_eq!(q.shadow_margin(), 0.0, "a negative blur is no shadow");
    // Only rect quads carry shadows.
    q.blur = 10.0;
    q.texture = TextureRef::MonoAtlas(0);
    assert_eq!(q.shadow_margin(), 0.0);
}

#[test]
fn clipping_keeps_the_geometry_and_drops_what_is_fully_outside() {
    let q = quad([10.0, 10.0], [20.0, 20.0]);
    let inside = q.clipped([0.0, 0.0, 100.0, 100.0]).expect("inside");
    assert_eq!((inside.pos, inside.size), (q.pos, q.size));
    assert_eq!(inside.clip, [0.0, 0.0, 100.0, 100.0]);
    let part = q.clipped([20.0, 0.0, 100.0, 100.0]).expect("partly inside");
    assert_eq!(
        (part.pos, part.size),
        (q.pos, q.size),
        "geometry is untouched; the GPU clips"
    );
    assert_eq!(part.clip, [20.0, 0.0, 100.0, 100.0]);
    assert!(q.clipped([40.0, 40.0, 10.0, 10.0]).is_none(), "entirely outside");
    assert!(
        q.clipped([30.0, 10.0, 10.0, 10.0]).is_none(),
        "only touching an edge"
    );
    assert!(
        q.clipped([0.0, 0.0, 0.0, 0.0]).is_none(),
        "an empty clip hides everything"
    );
    // Clipping twice intersects the clips.
    let twice = part.clipped([0.0, 0.0, 25.0, 100.0]).expect("still visible");
    assert_eq!(twice.clip, [20.0, 0.0, 5.0, 100.0]);
    // A shadow just outside the clip still shows through its blur margin.
    let mut shadow = quad([10.0, 10.0], [20.0, 20.0]);
    shadow.blur = 10.0;
    assert!(
        shadow.clipped([32.0, 10.0, 10.0, 20.0]).is_some(),
        "within the 15 px margin"
    );
    assert!(shadow.clipped([46.0, 10.0, 10.0, 20.0]).is_none(), "beyond it");
    // A zero-size quad has nothing to draw.
    assert!(quad([5.0, 5.0], [0.0, 0.0]).clipped(NO_CLIP).is_none());
}

#[test]
fn nine_slices_partition_the_rect_exactly() {
    let base = quad([10.0, 20.0], [100.0, 60.0]);
    let slices = nine_slice(base, 7, [0.0, 0.0, 1.0, 1.0], [8.0, 8.0, 8.0, 8.0], [32, 32], 1.0);
    assert_eq!(slices.len(), 9);
    let area: f32 = slices.iter().map(|s| s.size[0] * s.size[1]).sum();
    assert_eq!(area, 100.0 * 60.0);
    assert!(slices.iter().all(|s| s.texture == TextureRef::Image(7)));
    // Corners keep their size, scaled; the centre takes the rest.
    assert_eq!(slices[0].size, [8.0, 8.0]);
    assert_eq!(slices[4].size, [84.0, 44.0]);
    let doubled = nine_slice(base, 7, [0.0, 0.0, 1.0, 1.0], [8.0, 8.0, 8.0, 8.0], [32, 32], 2.0);
    assert_eq!(doubled[0].size, [16.0, 16.0]);
    // No insets: one quad that fills the rect.
    let one = nine_slice(base, 7, [0.25, 0.25, 0.75, 0.75], [0.0; 4], [32, 32], 1.0);
    assert_eq!(one.len(), 1);
    assert_eq!(
        (one[0].pos, one[0].size, one[0].uv),
        ([10.0, 20.0], [100.0, 60.0], [0.25, 0.25, 0.75, 0.75])
    );
    // The shared state (tint, alpha, z, clip) is kept and decoration is dropped.
    let mut tinted = base;
    tinted.color = UiColor::linear(1.0, 0.0, 0.0, 1.0);
    tinted.alpha = 0.5;
    tinted.z = 3.0;
    tinted.clip = [0.0, 0.0, 50.0, 50.0];
    tinted.corner_radius = [4.0; 4];
    tinted.border_width = [2.0; 4];
    for s in nine_slice(tinted, 1, [0.0, 0.0, 1.0, 1.0], [4.0; 4], [16, 16], 1.0) {
        assert_eq!(
            (s.color, s.alpha, s.z, s.clip),
            (tinted.color, 0.5, 3.0, [0.0, 0.0, 50.0, 50.0])
        );
        assert_eq!((s.corner_radius, s.border_width), ([0.0; 4], [0.0; 4]));
    }
}

#[test]
fn nine_slices_tile_without_gaps_or_overlap_for_any_inset_and_scale() {
    let mut rng = FuzzRng::new(0x915);
    for case in 0..1000 {
        let pos = [frac(&mut rng) * 50.0 - 25.0, frac(&mut rng) * 50.0 - 25.0];
        let size = [1.0 + frac(&mut rng) * 200.0, 1.0 + frac(&mut rng) * 200.0];
        let insets = [
            frac(&mut rng) * 60.0,
            frac(&mut rng) * 60.0,
            frac(&mut rng) * 60.0,
            frac(&mut rng) * 60.0,
        ];
        let scale = 0.25 + frac(&mut rng) * 3.0;
        let slices = nine_slice(quad(pos, size), 1, [0.0, 0.0, 1.0, 1.0], insets, [64, 64], scale);
        assert!(slices.len() <= 9 && !slices.is_empty(), "case {case}");
        let (x0, y0) = (pos[0].round(), pos[1].round());
        let (x1, y1) = ((pos[0] + size[0]).round(), (pos[1] + size[1]).round());
        let mut area = 0.0;
        for (i, a) in slices.iter().enumerate() {
            assert!(
                a.size[0] > 0.0 && a.size[1] > 0.0,
                "case {case}: empty slice kept"
            );
            assert!(
                a.pos[0] >= x0 && a.pos[1] >= y0 && a.pos[0] + a.size[0] <= x1 && a.pos[1] + a.size[1] <= y1,
                "case {case}"
            );
            assert_eq!(
                a.pos[0].fract() + a.pos[1].fract() + a.size[0].fract() + a.size[1].fract(),
                0.0,
                "case {case}: whole pixels"
            );
            area += a.size[0] * a.size[1];
            for b in &slices[i + 1..] {
                let apart = a.pos[0] + a.size[0] <= b.pos[0]
                    || b.pos[0] + b.size[0] <= a.pos[0]
                    || a.pos[1] + a.size[1] <= b.pos[1]
                    || b.pos[1] + b.size[1] <= a.pos[1];
                assert!(apart, "case {case}: overlap {a:?} {b:?}");
            }
        }
        assert_eq!(
            area,
            (x1 - x0) * (y1 - y0),
            "case {case}: the slices cover the rect"
        );
    }
}

#[test]
fn nine_slice_survives_degenerate_numbers() {
    let base = quad([0.0, 0.0], [10.0, 10.0]);
    for scale in [0.0, -2.0, f32::NAN, f32::INFINITY] {
        for insets in [[4.0; 4], [f32::NAN; 4], [-4.0; 4], [1e9; 4]] {
            let slices = nine_slice(base, 1, [0.0, 0.0, 1.0, 1.0], insets, [0, 0], scale);
            assert!(slices.len() <= 9);
            assert!(
                slices.iter().all(|s| s.size[0] > 0.0
                    && s.size[1] > 0.0
                    && s.pos.iter().chain(&s.uv).all(|v| v.is_finite())),
                "scale {scale} insets {insets:?}"
            );
        }
    }
    // An inverted or empty destination gives no slices rather than negative ones.
    for size in [[0.0, 0.0], [-10.0, 10.0], [10.0, -10.0], [0.4, 0.4]] {
        let slices = nine_slice(
            quad([0.0, 0.0], size),
            1,
            [0.0, 0.0, 1.0, 1.0],
            [2.0; 4],
            [8, 8],
            1.0,
        );
        assert!(
            slices.iter().all(|s| s.size[0] > 0.0 && s.size[1] > 0.0),
            "{size:?}"
        );
    }
}

#[test]
fn instances_carry_kind_layer_alpha_and_clip() {
    let mut q = quad([1.0, 2.0], [30.0, 40.0]);
    q.color = UiColor::linear(1.0, 0.5, 0.25, 0.8);
    q.color2 = UiColor::linear(0.0, 0.0, 1.0, 1.0);
    q.alpha = 0.5;
    q.clip = [5.0, 6.0, 7.0, 8.0];
    let rect = instance_of(&q);
    assert_eq!(rect.kind_layer, [0.0, 0.0, 0.0, 0.0]);
    assert_eq!(rect.color, [1.0, 0.5, 0.25, 0.4], "opacity folds into alpha only");
    assert_eq!(rect.color2[3], 0.5);
    assert_eq!(rect.clip, [5.0, 6.0, 12.0, 14.0], "x0, y0, x1, y1");
    q.gradient = true;
    q.angle = 90.0;
    assert_eq!(instance_of(&q).kind_layer, [4.0, 0.0, 90.0, 0.0]);
    q.gradient = false;
    q.texture = TextureRef::MonoAtlas(3);
    assert_eq!(instance_of(&q).kind_layer[..2], [1.0, 3.0]);
    q.texture = TextureRef::ColorAtlas(2);
    assert_eq!(instance_of(&q).kind_layer[..2], [2.0, 2.0]);
    q.texture = TextureRef::Image(99);
    assert_eq!(instance_of(&q).kind_layer[..2], [3.0, 0.0]);
    // A shadow expands by its margin, carries its offset in the uv and half the blur as sigma.
    let mut s = quad([10.0, 10.0], [20.0, 20.0]);
    s.blur = 10.0;
    s.shadow_offset = [3.0, 4.0];
    let inst = instance_of(&s);
    assert_eq!((inst.pos, inst.size), ([-5.0, -5.0], [50.0, 50.0]));
    assert_eq!(inst.uv, [3.0, 4.0, 0.0, 0.0]);
    assert_eq!(inst.kind_layer, [5.0, 15.0, 0.0, 5.0]);
    assert_eq!(QuadInstance::SIZE as usize, std::mem::size_of::<QuadInstance>());
    assert_eq!(
        QuadInstance::SIZE % 16,
        0,
        "instances stay 16-byte aligned for the GPU"
    );
}

fn img(id: u64) -> UiQuad {
    let mut q = quad([0.0, 0.0], [4.0, 4.0]);
    q.texture = TextureRef::Image(id);
    q
}

fn run(quads: &[UiQuad]) -> (Vec<QuadInstance>, Vec<BatchRange>) {
    let (mut instances, mut ranges) = (Vec::new(), Vec::new());
    batch_ranges(quads, &mut instances, &mut ranges);
    (instances, ranges)
}

#[test]
fn image_free_quads_ride_along_and_images_split_runs() {
    let rect = quad([0.0, 0.0], [1.0, 1.0]);
    let (instances, ranges) = run(&[]);
    assert!(instances.is_empty() && ranges.is_empty());
    let (_, ranges) = run(&[rect, rect, rect]);
    assert_eq!(
        ranges,
        [BatchRange {
            image: None,
            first: 0,
            count: 3
        }]
    );
    let (_, ranges) = run(&[rect, img(1), rect, img(1)]);
    assert_eq!(
        ranges,
        [BatchRange {
            image: Some(1),
            first: 0,
            count: 4
        }],
        "one run adopts the first image"
    );
    let (_, ranges) = run(&[img(1), rect, img(2), rect, img(2), img(1)]);
    assert_eq!(
        ranges,
        [
            BatchRange {
                image: Some(1),
                first: 0,
                count: 2
            },
            BatchRange {
                image: Some(2),
                first: 2,
                count: 3
            },
            BatchRange {
                image: Some(1),
                first: 5,
                count: 1
            },
        ]
    );
}

#[test]
fn batching_reuses_buffers_and_ranges_always_cover_every_instance() {
    let mut rng = FuzzRng::new(0xBA7C);
    let (mut instances, mut ranges) = (
        vec![instance_of(&quad([0.0; 2], [1.0; 2])); 5],
        vec![BatchRange {
            image: Some(9),
            first: 0,
            count: 5,
        }],
    );
    for _ in 0..200 {
        let quads: Vec<UiQuad> = (0..rng.below(40))
            .map(|_| match rng.below(4) {
                0 => img(1 + rng.below(3) as u64),
                1 => {
                    let mut q = quad([0.0; 2], [1.0; 2]);
                    q.texture = TextureRef::MonoAtlas(0);
                    q
                }
                _ => quad([0.0; 2], [1.0; 2]),
            })
            .collect();
        batch_ranges(&quads, &mut instances, &mut ranges);
        assert_eq!(instances.len(), quads.len(), "stale instances are cleared");
        let mut next = 0;
        for r in &ranges {
            assert_eq!(r.first, next, "ranges are contiguous");
            assert!(r.count > 0);
            next += r.count;
        }
        assert_eq!(next as usize, quads.len());
        // The older splitter agrees on the order and count of instances.
        let old: Vec<QuadInstance> = batch(&quads).into_iter().flat_map(|b| b.instances).collect();
        assert_eq!(old, instances);
        // Every image quad sits in a run bound to its own image.
        for (i, q) in quads.iter().enumerate() {
            if let TextureRef::Image(id) = q.texture {
                let run = ranges
                    .iter()
                    .find(|r| (r.first as usize..(r.first + r.count) as usize).contains(&i))
                    .unwrap();
                assert_eq!(run.image, Some(id));
            }
        }
    }
}
