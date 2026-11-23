// SPDX-License-Identifier: MPL-2.0
use super::*;

fn image(size: (u32, u32)) -> PtImage {
    let n = (size.0 * size.1) as usize;
    PtImage {
        width: size.0,
        height: size.1,
        beauty: vec![[10.0; 3]; n],
        albedo: vec![[0.5; 3]; n],
        normal: vec![[0.0, 1.0, 0.0]; n],
        roughness: vec![0.5; n],
        depth: vec![1.0; n],
        direct: vec![[4.0; 3]; n],
        indirect_diffuse: vec![[1.0, 2.0, 3.0]; n],
        indirect_specular: vec![[0.5, 0.0, 0.0]; n],
        emissive: vec![[0.0; 3]; n],
        volumetric: vec![[0.0; 3]; n],
        sky: vec![[0.0; 3]; n],
        shadow: vec![1.0; n],
        variance: vec![[0.0; 3]; n],
        spp: 1,
        spp_map: Vec::new(),
        metadata: regia_pt::output::PtMetadata::default(),
    }
}

fn capture(n: usize, value: f32) -> Capture {
    Capture {
        rgb: vec![[value; 3]; n],
        ms: 0.0,
    }
}

fn refused(result: Result<Prepared>, plane: &str) {
    let err = match result {
        Ok(_) => panic!("malformed {plane} reached image arithmetic"),
        Err(err) => err.to_string(),
    };
    assert!(err.starts_with("[image_shape]"), "{err}");
    assert!(err.contains(plane), "{err}");
}

#[test]
fn equal_counts_with_different_dimensions_are_refused() {
    let baked = [capture(6, 8.0)];
    refused(
        prepare(
            (3, 2),
            &baked,
            &capture(6, 6.0),
            &capture(6, 2.0),
            &image((2, 3)),
            None,
        ),
        "reference",
    );
    refused(
        prepare(
            (3, 2),
            &baked,
            &capture(6, 6.0),
            &capture(6, 2.0),
            &image((3, 2)),
            Some(&image((2, 3))),
        ),
        "emitter",
    );
}

#[test]
fn every_indirect_component_and_optional_emitter_plane_is_checked() {
    for delta in [-1, 1] {
        for component in 0..4 {
            let mut reference = image((3, 1));
            let mut emitter = image((3, 1));
            let (name, plane) = match component {
                0 => ("indirect_diffuse", &mut reference.indirect_diffuse),
                1 => ("indirect_specular", &mut reference.indirect_specular),
                2 => ("reference.direct", &mut reference.direct),
                _ => ("emitter.direct", &mut emitter.direct),
            };
            plane.resize((3 + delta) as usize, [0.0; 3]);
            refused(
                prepare(
                    (3, 1),
                    &[capture(3, 8.0)],
                    &capture(3, 6.0),
                    &capture(3, 2.0),
                    &reference,
                    Some(&emitter),
                ),
                name,
            );
        }
    }
}

#[test]
fn every_raster_and_reference_depth_or_beauty_length_is_checked() {
    for n in [2, 4] {
        for component in 0..6 {
            let mut baked = [capture(3, 8.0), capture(3, 9.0)];
            let mut ddgi = capture(3, 6.0);
            let mut direct = capture(3, 2.0);
            let mut reference = image((3, 1));
            let name = match component {
                0 => {
                    baked[0].rgb.resize(n, [0.0; 3]);
                    "baked[0]"
                }
                1 => {
                    baked[1].rgb.resize(n, [0.0; 3]);
                    "baked[1]"
                }
                2 => {
                    ddgi.rgb.resize(n, [0.0; 3]);
                    "ddgi"
                }
                3 => {
                    direct.rgb.resize(n, [0.0; 3]);
                    "direct"
                }
                4 => {
                    reference.beauty.resize(n, [0.0; 3]);
                    "beauty"
                }
                _ => {
                    reference.depth.resize(n, 0.0);
                    "depth"
                }
            };
            refused(prepare((3, 1), &baked, &ddgi, &direct, &reference, None), name);
        }
    }
}

#[test]
fn empty_images_are_refused() {
    refused(
        prepare(
            (0, 0),
            &[capture(0, 0.0)],
            &capture(0, 0.0),
            &capture(0, 0.0),
            &image((0, 0)),
            None,
        ),
        "0x0",
    );
}

#[test]
fn odd_valid_shapes_preserve_all_indirect_outputs() {
    let mut emitter = image((3, 5));
    emitter.direct.fill([3.0, 5.0, 4.0]);
    for emitter in [None, Some(&emitter)] {
        let prepared = prepare(
            (3, 5),
            &[capture(15, 8.0)],
            &capture(15, 6.0),
            &capture(15, 2.0),
            &image((3, 5)),
            emitter,
        )
        .expect("valid planes");
        assert_eq!(prepared.baked_ind, vec![[6.0; 3]; 15]);
        assert_eq!(prepared.ddgi_ind, vec![[4.0; 3]; 15]);
        assert_eq!(
            prepared.ref_ind,
            vec![
                if emitter.is_some() {
                    [2.5, 2.0, 3.0]
                } else {
                    [1.5, 2.0, 3.0]
                };
                15
            ]
        );
    }
}

#[test]
fn capture_rejects_transposed_dimensions_and_short_or_long_rgb() {
    assert!(check_capture((3, 2), (2, 3), &[[0.0; 3]; 6]).is_err());
    for n in [5, 7] {
        assert!(check_capture((3, 2), (3, 2), &vec![[0.0; 3]; n]).is_err());
    }
}

#[test]
fn scoring_rejects_each_short_or_long_plane_before_sky_fill() {
    for n in [2, 4] {
        let rgb = vec![[1.0; 3]; n];
        assert!(crate::gi_compare::score(&rgb, &[[1.0; 3]; 3], &[1.0; 3], (3, 1), 0.0).is_err());
        assert!(crate::gi_compare::score(&[[1.0; 3]; 3], &rgb, &[1.0; 3], (3, 1), 0.0).is_err());
        assert!(
            crate::gi_compare::score(&[[1.0; 3]; 3], &[[1.0; 3]; 3], &vec![1.0; n], (3, 1), 0.0).is_err()
        );
    }
}

#[test]
fn leak_rejects_each_short_or_long_plane_including_mask() {
    for n in [2, 4] {
        let rgb = vec![[1.0; 3]; n];
        let good = [[1.0; 3]; 3];
        let cases = [
            checked_leak((3, 1), &rgb, &good, &good, &[1.0; 3], &[true; 3]),
            checked_leak((3, 1), &good, &rgb, &good, &[1.0; 3], &[true; 3]),
            checked_leak((3, 1), &good, &good, &rgb, &[1.0; 3], &[true; 3]),
            checked_leak((3, 1), &good, &good, &good, &vec![1.0; n], &[true; 3]),
            checked_leak((3, 1), &good, &good, &good, &[1.0; 3], &vec![true; n]),
        ];
        for result in cases {
            let err = result.expect_err("malformed leak input");
            assert!(err.to_string().starts_with("[image_shape]"), "{err}");
        }
    }
}

#[test]
fn valid_black_reference_keeps_an_undefined_leak_denominator() {
    let dark = [[0.0; 3]; 15];
    let leak = checked_leak((3, 5), &[[1.0; 3]; 15], &dark, &dark, &[1.0; 15], &[true; 15])
        .expect("valid shape")
        .expect("mask selects geometry");
    assert_eq!(leak.pixels, 15);
    assert_eq!(leak.reference_mean, 0.0);
    assert_eq!(leak.floor, 0.0);
    assert_eq!(leak.leak, None);
    assert_eq!(
        checked_leak((3, 5), &dark, &dark, &dark, &[1.0; 15], &[false; 15]).expect("valid empty selection"),
        None
    );
}

#[test]
fn saved_reference_aovs_and_nonempty_adaptive_counts_have_the_same_shape() {
    for n in [2, 4] {
        for component in 0..9 {
            let mut reference = image((3, 1));
            let name = match component {
                0 => {
                    reference.albedo.resize(n, [0.0; 3]);
                    "albedo"
                }
                1 => {
                    reference.normal.resize(n, [0.0; 3]);
                    "normal"
                }
                2 => {
                    reference.direct.resize(n, [0.0; 3]);
                    "direct"
                }
                3 => {
                    reference.emissive.resize(n, [0.0; 3]);
                    "emissive"
                }
                4 => {
                    reference.volumetric.resize(n, [0.0; 3]);
                    "volumetric"
                }
                5 => {
                    reference.sky.resize(n, [0.0; 3]);
                    "sky"
                }
                6 => {
                    reference.variance.resize(n, [0.0; 3]);
                    "variance"
                }
                7 => {
                    reference.shadow.resize(n, 0.0);
                    "shadow"
                }
                _ => {
                    reference.spp_map.resize(n, 1.0);
                    "spp_map"
                }
            };
            refused(
                prepare(
                    (3, 1),
                    &[capture(3, 8.0)],
                    &capture(3, 6.0),
                    &capture(3, 2.0),
                    &reference,
                    None,
                ),
                name,
            );
        }
    }
    let mut adaptive = image((3, 1));
    adaptive.spp_map = vec![1.0, 2.0, 1.0];
    assert!(
        prepare(
            (3, 1),
            &[capture(3, 8.0)],
            &capture(3, 6.0),
            &capture(3, 2.0),
            &adaptive,
            None
        )
        .is_ok()
    );
}

#[test]
fn valid_scores_and_serialized_score_fields_are_bit_for_bit_unchanged() {
    let size = (3, 5);
    let reference: Vec<_> = (0..15).map(|i| [i as f32 + 1.0, 2.0, 0.5]).collect();
    let test: Vec<_> = (0..15).map(|i| [i as f32 * 0.5, 0.25, 1.0]).collect();
    let depth: Vec<_> = (0u32..15)
        .map(|i| if i.is_multiple_of(3) { 0.0 } else { 2.0 })
        .collect();
    let expected = regia_render::compare::compare_linear(
        &reference,
        &metrics::fill_sky(&test, &reference, &depth),
        size.0,
        size.1,
        0.0,
        regia_render::compare::DEFAULT_PPD,
    );
    let actual = crate::gi_compare::score(&reference, &test, &depth, size, 0.0).expect("valid shapes");
    assert_eq!(actual, expected);
    assert_eq!(
        serde_json::to_string(&crate::gi_compare::summary::Scores::from_report(&actual.0)).expect("scores"),
        serde_json::to_string(&crate::gi_compare::summary::Scores::from_report(&expected.0)).expect("scores"),
    );
}

#[test]
fn diagnostics_identify_expected_and_actual_shape_stably() {
    assert_eq!(
        check_capture((3, 2), (2, 3), &[[0.0; 3]; 6])
            .expect_err("transposed capture")
            .to_string(),
        "[render_failed] HDR capture is 2x3, the frame 3x2",
    );
    assert_eq!(
        check_capture((3, 2), (3, 2), &[[0.0; 3]; 5])
            .expect_err("short RGB")
            .to_string(),
        "[image_shape] HDR capture.rgb has 5 elements; expected 6 for 3x2",
    );
    assert_eq!(
        checked_leak(
            (3, 1),
            &[[0.0; 3]; 3],
            &[[0.0; 3]; 3],
            &[[0.0; 3]; 3],
            &[1.0; 3],
            &[false; 2]
        )
        .expect_err("empty selection still needs a complete mask")
        .to_string(),
        "[image_shape] leak.mask has 2 elements; expected 3 for 3x1",
    );
}

#[test]
fn empty_planes_with_nonempty_dimensions_are_refused() {
    let mut reference = image((3, 1));
    reference.beauty.clear();
    refused(
        prepare(
            (3, 1),
            &[capture(3, 8.0)],
            &capture(3, 6.0),
            &capture(3, 2.0),
            &reference,
            None,
        ),
        "beauty",
    );
}
