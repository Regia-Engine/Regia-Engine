// SPDX-License-Identifier: MPL-2.0
//! Noise and Ramp sculpt modes (`docs/specs/82-terrain-editing.md` § 2, ADR-1853) through the
//! same stroke path the editor and `terrain.stroke` use.

use glam::Vec2;
use regia_terrain::edit::validate_brush;
use regia_terrain::{
    Brush, BrushMode, BrushShape, CdlodConfig, MinMaxPyramid, StrokeUndo, Terrain, TerrainEdits,
    TerrainLayers, TerrainMap, apply_stroke, ramp_mode,
};

/// A flat 129² map over 128 m (1 m samples) at `base` metres.
fn flat(base: f32) -> Terrain {
    let n = 129u32;
    let map = TerrainMap::from_heights(n, n, vec![base; (n * n) as usize], 128.0, 200.0, -50.0).expect("map");
    let minmax = MinMaxPyramid::build(&map, 3).expect("pyramid");
    Terrain {
        map,
        cdlod: CdlodConfig::default(),
        root_level: 3,
        minmax,
        layers: TerrainLayers::default(),
        holes: None,
        water: None,
    }
}

fn bits(t: &Terrain) -> Vec<u32> {
    t.map.heights().iter().map(|h| h.to_bits()).collect()
}

fn noise_brush(amount: f32, seed: u32) -> Brush {
    Brush {
        shape: BrushShape::Circle { radius: 40.0 },
        mode: BrushMode::Noise {
            amount,
            wavelength: 6.0,
            seed,
        },
        hardness: 1.0,
        ..Brush::default()
    }
}

fn stroke(t: &mut Terrain, brush: &Brush, path: &[Vec2]) -> StrokeUndo {
    apply_stroke(t, None, &mut TerrainEdits::default(), brush, path).expect("stroke")
}

#[test]
fn noise_is_bounded_roughly_zero_mean_and_seeded() {
    let mut a = flat(10.0);
    let undo = stroke(&mut a, &noise_brush(2.0, 7), &[Vec2::ZERO]);
    assert!(
        undo.len() > 3000,
        "a 40 m brush touches thousands of samples: {}",
        undo.len()
    );
    let StrokeUndo::Height(record) = &undo else {
        panic!("a height stroke");
    };
    let offsets: Vec<f32> = record.samples.iter().map(|s| s.after - s.before).collect();
    assert!(
        offsets.iter().all(|d| d.abs() <= 2.0 + 1e-4),
        "noise stays within ±amount"
    );
    assert!(
        offsets.iter().any(|&d| d > 0.5) && offsets.iter().any(|&d| d < -0.5),
        "both signs"
    );
    let mean = offsets.iter().sum::<f32>() / offsets.len() as f32;
    assert!(
        mean.abs() < 0.3,
        "value noise is close to zero-mean over a wide stamp: {mean}"
    );

    // The same seed reproduces the field bit for bit; another seed does not.
    let mut b = flat(10.0);
    stroke(&mut b, &noise_brush(2.0, 7), &[Vec2::ZERO]);
    assert_eq!(bits(&a), bits(&b));
    let mut c = flat(10.0);
    stroke(&mut c, &noise_brush(2.0, 8), &[Vec2::ZERO]);
    assert_ne!(bits(&a), bits(&c));

    // Inverted noise mirrors the field around the base height.
    let mut d = flat(10.0);
    let inverted = Brush {
        mode: noise_brush(2.0, 7).mode.inverted(),
        ..noise_brush(2.0, 7)
    };
    stroke(&mut d, &inverted, &[Vec2::ZERO]);
    for (x, y) in a.map.heights().iter().zip(d.map.heights()) {
        assert!(((x - 10.0) + (y - 10.0)).abs() < 1e-4);
    }

    // Undo restores the flat map bit for bit.
    record.undo(&mut a.map);
    assert_eq!(bits(&a), bits(&flat(10.0)));
}

#[test]
fn ramp_interpolates_along_its_axis_inside_the_brush_only() {
    let mut t = flat(0.0);
    let (from, to) = (Vec2::new(-30.0, 0.0), Vec2::new(30.0, 0.0));
    let brush = Brush {
        shape: BrushShape::Circle { radius: 6.0 },
        mode: ramp_mode(&t.map, from, to, Some(0.0), Some(12.0)),
        hardness: 1.0,
        ..Brush::default()
    };
    let undo = stroke(&mut t, &brush, &[from, to]);
    assert!(!undo.is_empty());
    for (x, want) in [
        (-30.0f32, 0.0f32),
        (-15.0, 3.0),
        (0.0, 6.0),
        (15.0, 9.0),
        (30.0, 12.0),
    ] {
        let got = t.map.sample_bilinear(x, 0.0);
        assert!(
            (got - want).abs() < 1e-3,
            "ramp height at x = {x}: {got}, want {want}"
        );
    }
    // Beyond the ends the target clamps, and outside the brush nothing moves.
    assert!(t.map.sample_bilinear(34.0, 0.0) > 11.99);
    assert_eq!(t.map.sample_bilinear(0.0, 20.0), 0.0);
    assert_eq!(t.map.sample_bilinear(-50.0, 0.0), 0.0);
}

#[test]
fn ramp_mode_takes_the_map_heights_by_default_and_inverts_by_swapping_ends() {
    let mut t = flat(0.0);
    let raise = Brush {
        shape: BrushShape::Circle { radius: 12.0 },
        mode: BrushMode::Assign { value: 20.0 },
        hardness: 1.0,
        ..Brush::default()
    };
    stroke(&mut t, &raise, &[Vec2::new(40.0, 40.0)]);
    let mode = ramp_mode(&t.map, Vec2::new(-40.0, -40.0), Vec2::new(40.0, 40.0), None, None);
    let BrushMode::Ramp {
        start_value,
        end_value,
        start,
        end,
    } = mode
    else {
        panic!("a ramp");
    };
    assert_eq!(start_value, 0.0);
    assert!((end_value - 20.0).abs() < 1e-4);
    assert_eq!(start, [24.0, 24.0]);
    assert_eq!(end, [104.0, 104.0]);
    assert_eq!(
        mode.inverted(),
        BrushMode::Ramp {
            start: end,
            end: start,
            start_value: end_value,
            end_value: start_value,
        }
    );
    assert_eq!(mode.name(), "ramp");
    assert_eq!(noise_brush(1.0, 0).mode.name(), "noise");
}

#[test]
fn bad_noise_and_ramp_parameters_are_refused() {
    let bad_wavelength = Brush {
        mode: BrushMode::Noise {
            amount: 1.0,
            wavelength: 0.1,
            seed: 0,
        },
        ..Brush::default()
    };
    assert!(validate_brush(&bad_wavelength).is_err());
    let bad_amount = Brush {
        mode: BrushMode::Noise {
            amount: f32::NAN,
            wavelength: 4.0,
            seed: 0,
        },
        ..Brush::default()
    };
    assert!(validate_brush(&bad_amount).is_err());
    let bad_ramp = Brush {
        mode: BrushMode::Ramp {
            start: [0.0, f32::INFINITY],
            end: [1.0, 1.0],
            start_value: 0.0,
            end_value: 1.0,
        },
        ..Brush::default()
    };
    assert!(validate_brush(&bad_ramp).is_err());
    assert!(validate_brush(&noise_brush(3.0, 1)).is_ok());
    // A degenerate ramp (both ends on one point) is valid and pulls toward the start value.
    assert_eq!(
        BrushMode::ramp_at(glam::IVec2::new(9, 9), [4.0, 4.0], [4.0, 4.0], 2.5, 9.0),
        2.5
    );
}
