// SPDX-License-Identifier: MPL-2.0
use super::*;

fn safe() -> Rect {
    Rect {
        x: 96.0,
        y: 54.0,
        w: 1728.0,
        h: 972.0,
    }
}

fn input(id: u64, anchor: Option<[f32; 2]>) -> AnchorInput {
    AnchorInput {
        id: CaptionId(id),
        anchor,
        size: [400.0, 80.0],
    }
}

fn within(r: Rect, s: Rect) -> bool {
    r.x >= s.x - 1e-3 && r.y >= s.y - 1e-3 && r.x + r.w <= s.x + s.w + 1e-3 && r.y + r.h <= s.y + s.h + 1e-3
}

fn no_overlap(ps: &[AnchorPlacement]) {
    for (i, a) in ps.iter().enumerate() {
        for b in &ps[i + 1..] {
            assert!(!overlaps(a.rect, b.rect), "{:?} overlaps {:?}", a, b);
        }
    }
}

#[test]
fn on_screen_caption_sits_above_the_speaker() {
    let p = place_anchored(&[input(1, Some([960.0, 600.0]))], safe());
    assert_eq!(p[0].mode, AnchorMode::OnScreen);
    assert!(p[0].rect.y + p[0].rect.h <= 600.0);
    assert!((p[0].rect.x + p[0].rect.w * 0.5 - 960.0).abs() < 1e-3);
}

#[test]
fn stays_inside_the_safe_area_at_every_edge() {
    for a in [
        [100.0, 60.0],
        [1820.0, 60.0],
        [100.0, 1020.0],
        [1820.0, 1020.0],
        [960.0, 54.0],
        [96.0, 540.0],
    ] {
        let p = place_anchored(&[input(1, Some(a))], safe());
        assert!(within(p[0].rect, safe()), "{a:?} -> {:?}", p[0]);
    }
}

#[test]
fn off_screen_speaker_gets_an_edge_arrow_pointing_at_it() {
    let p = place_anchored(&[input(1, Some([3000.0, 540.0]))], safe());
    let AnchorMode::EdgeArrow(arrow) = p[0].mode else {
        panic!("{:?}", p[0])
    };
    assert!(within(p[0].rect, safe()));
    // Pointing right: 90 degrees clockwise from up, tip on the right edge.
    assert!((arrow.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-3);
    assert!((arrow.pos[0] - (safe().x + safe().w)).abs() < 1e-2);
    let p = place_anchored(&[input(1, Some([960.0, -500.0]))], safe());
    let AnchorMode::EdgeArrow(arrow) = p[0].mode else {
        panic!()
    };
    assert!(arrow.angle.abs() < 1e-3 || (arrow.angle - std::f32::consts::TAU).abs() < 1e-3);
}

#[test]
fn two_anchored_captions_never_overlap() {
    // Same anchor, and anchors one pixel apart, and many at once.
    for anchors in [
        vec![[960.0, 500.0], [960.0, 500.0]],
        vec![[960.0, 500.0], [961.0, 501.0]],
        (0..8).map(|i| [900.0 + i as f32, 300.0 + i as f32]).collect(),
        (0..5).map(|i| [3000.0 + i as f32, 500.0]).collect(),
    ] {
        let inputs: Vec<_> = anchors
            .iter()
            .enumerate()
            .map(|(i, a)| input(i as u64 + 1, Some(*a)))
            .collect();
        let p = place_anchored(&inputs, safe());
        assert_eq!(p.len(), inputs.len());
        no_overlap(&p);
        assert!(p.iter().all(|x| within(x.rect, safe())));
    }
}

#[test]
fn behind_the_camera_falls_back_to_the_band() {
    let p = place_anchored(&[input(1, None), input(2, None)], safe());
    assert!(p.iter().all(|x| x.mode == AnchorMode::BottomBand));
    no_overlap(&p);
    assert!(
        (p[0].rect.y + p[0].rect.h - (safe().y + safe().h)).abs() < 1e-3,
        "first at the bottom"
    );
    assert!(p[1].rect.y < p[0].rect.y, "second stacks upward");
    // A band caption blocks an anchored one at the same spot.
    let p = place_anchored(&[input(1, None), input(2, Some([960.0, 1000.0]))], safe());
    no_overlap(&p);
}

#[test]
fn no_free_spot_falls_back_to_the_band_without_overlap() {
    let tiny = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 200.0,
    };
    let inputs: Vec<_> = (1..=4).map(|i| input(i, Some([200.0, 100.0]))).collect();
    let p = place_anchored(&inputs, tiny);
    assert_eq!(p.len(), 4);
    assert!(p.iter().any(|x| x.mode == AnchorMode::BottomBand));
}

#[test]
fn projection_matches_a_known_camera_and_flags_behind() {
    let view =
        regia_core::glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        std::f32::consts::FRAC_PI_2,
        16.0 / 9.0,
        0.1,
        100.0,
    );
    let vp = proj * view;
    let center = project_point(&vp, (1920.0, 1080.0), Vec3::ZERO);
    assert!(!center.behind);
    assert!((center.px[0] - 960.0).abs() < 1e-2 && (center.px[1] - 540.0).abs() < 1e-2);
    let up = project_point(&vp, (1920.0, 1080.0), Vec3::new(0.0, 1.0, 0.0));
    assert!(up.px[1] < 540.0, "world up is screen up (y down)");
    let behind = project_point(&vp, (1920.0, 1080.0), Vec3::new(0.0, 0.0, 10.0));
    assert!(behind.behind);
}

#[test]
fn placement_names_round_trip() {
    for p in [CaptionPlacement::BottomBand, CaptionPlacement::Anchored] {
        assert_eq!(CaptionPlacement::parse(p.as_str()), Some(p));
    }
    assert_eq!(CaptionPlacement::parse("x"), None);
    let d = DialogueCaptionSettings::default();
    assert!(!d.karaoke && d.placement == CaptionPlacement::BottomBand);
}
