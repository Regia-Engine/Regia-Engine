// SPDX-License-Identifier: MPL-2.0
//! JW-L5 acceptance: no input sequence moves the view past the tilt cone, the pan bounds or the
//! zoom limit. Seeded, deterministic fuzzing of both layers (raw pointers through the recognizer,
//! and view inputs straight into the controller), including non-finite and huge values, with
//! random ranges, viewports and frame times. The checks read the pose the renderer would get
//! (the view matrix), not the controller's own state.

use glam::{Vec2, Vec3};
use regia_viewer_view::{CameraDefaults, DragView, PointerKind, SpinMode, ViewInput, ViewRange, ViewState};

/// SplitMix64: a tiny deterministic generator (public domain, Vigna 2015).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    /// Mostly ordinary values, sometimes extreme or not finite.
    fn wild(&mut self, scale: f32) -> f32 {
        match self.below(40) {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => 1.0e30,
            4 => -1.0e30,
            5 => 0.0,
            _ => self.range(-scale, scale),
        }
    }
}

fn random_range(rng: &mut Rng) -> ViewRange {
    let spin = match rng.below(4) {
        0 => SpinMode::None,
        1 => SpinMode::Rock,
        2 => SpinMode::AboutFrontAxis,
        _ => SpinMode::Free,
    };
    let axis = loop {
        let v = Vec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0));
        if v.length() > 0.1 {
            break v.normalize();
        }
    };
    let front_axis = if rng.below(3) == 0 { axis } else { Vec3::Z };
    let px = rng.range(0.0, 8.0);
    let py = rng.range(0.0, 8.0);
    ViewRange {
        front_axis,
        tilt_max_deg: if rng.below(10) == 0 {
            0.0
        } else {
            rng.range(0.0, 89.0)
        },
        pan_bounds_mm: [[-px, rng.range(0.0, 8.0)], [-py, rng.range(0.0, 8.0)]],
        spin,
        rock_max_deg: (spin == SpinMode::Rock).then(|| rng.range(0.0, 180.0)),
        max_px_per_mm: rng.range(1.0, 2000.0),
        back: "plausible".into(),
    }
}

/// Every invariant, read from what the renderer would draw. Checks both the shown and the target
/// view (the target is where a lazy loop would end up).
fn check(view: &DragView, pivot: Vec3, step: usize, seed: u64) {
    let c = &view.controller;
    let r = c.range();
    for (which, s) in [("shown", c.shown()), ("target", c.target())] {
        let ctx = || format!("seed {seed} step {step} {which} {s:?} range {r:?}");
        let pose = c.pose_of(&s);
        let vm = pose.view_matrix();
        assert!(vm.is_finite(), "view matrix not finite: {}", ctx());
        assert!(
            pose.projection(c.viewport().x / c.viewport().y).is_finite(),
            "{}",
            ctx()
        );
        // Camera forward is -Z of the camera frame; the eye-to-look-at direction is its negation.
        let inv = vm.inverse();
        let forward = -inv.z_axis.truncate().normalize();
        let eye = inv.w_axis.truncate();
        let view_dir = -forward;
        let angle = view_dir.dot(r.front_axis).clamp(-1.0, 1.0).acos().to_degrees();
        let limit = r.max_view_angle_deg();
        assert!(angle <= limit + 0.05, "angle {angle} > {limit}: {}", ctx());
        if r.spin != SpinMode::Free {
            // The vertical part of the tilt never exceeds tilt_max_deg, whatever the rock allows.
            let (_, up, _) = c.basis();
            let vertical = view_dir.dot(up).clamp(-1.0, 1.0).asin().to_degrees().abs();
            assert!(
                vertical <= r.tilt_max_deg + 0.05,
                "vertical {vertical}: {}",
                ctx()
            );
        } else {
            assert!(s.tilt_deg[1].abs() <= 85.0 + 1e-3, "{}", ctx());
        }
        if matches!(r.spin, SpinMode::None | SpinMode::Rock) {
            assert_eq!(s.spin_deg, 0.0, "{}", ctx());
        }
        // Pan, from the pose: the look-at point minus the pivot, in the front plane.
        let (_, up, right) = c.basis();
        let off = pose.look_at_mm - pivot;
        let pan = Vec2::new(off.dot(right), off.dot(up));
        for (p, [lo, hi]) in pan.to_array().into_iter().zip(r.pan_bounds_mm) {
            assert!(p >= lo - 1e-3 && p <= hi + 1e-3, "pan {pan}: {}", ctx());
        }
        assert!(
            off.dot(r.front_axis).abs() < 1e-3,
            "pan left the front plane: {}",
            ctx()
        );
        // Distance is fixed; zoom only narrows the lens, so the camera never enters the piece.
        let dist = (eye - pose.look_at_mm).length();
        assert!(
            (dist - c.camera().distance_mm).abs() < 1e-2 * dist.max(1.0),
            "{}",
            ctx()
        );
        assert!(
            s.zoom >= 1.0 - 1e-6 && s.zoom <= c.zoom_max() * (1.0 + 1e-5),
            "zoom: {}",
            ctx()
        );
        let px_mm = c.px_per_mm(s.zoom);
        assert!(
            px_mm <= r.max_px_per_mm.max(c.px_per_mm(1.0)) * (1.0 + 1e-4),
            "px/mm {px_mm}: {}",
            ctx()
        );
    }
}

fn pointer_kind(rng: &mut Rng) -> PointerKind {
    match rng.below(3) {
        0 => PointerKind::Touch,
        1 => PointerKind::MousePrimary,
        _ => PointerKind::MouseSecondary,
    }
}

fn fuzz_one(seed: u64, steps: usize) {
    let mut rng = Rng(seed);
    let range = random_range(&mut rng);
    let camera = CameraDefaults {
        distance_mm: rng.range(10.0, 200.0),
        focal_length_mm: rng.range(20.0, 200.0),
        sensor_mm: 36.0,
        f_stop: 11.0,
    };
    let mut view = DragView::new(range, camera);
    let pivot = Vec3::new(rng.range(-5.0, 5.0), rng.range(-5.0, 5.0), rng.range(-5.0, 5.0));
    view.controller.set_pivot(pivot);
    view.controller
        .set_viewport(rng.range(1.0, 4000.0), rng.range(1.0, 4000.0));
    let mut t = 0.0f64;
    for step in 0..steps {
        t += rng.range(0.0, 40.0) as f64;
        match rng.below(20) {
            0..=1 => {
                let id = rng.below(4);
                let kind = pointer_kind(&mut rng);
                let (x, y) = (rng.wild(3000.0), rng.wild(3000.0));
                view.pointer_down(id, kind, x, y, t);
            }
            2..=6 => {
                let id = rng.below(4);
                let (x, y) = (rng.wild(3000.0), rng.wild(3000.0));
                view.pointer_move(id, x, y, t);
            }
            7..=8 => {
                view.pointer_up(rng.below(4), t);
            }
            9 => {
                view.pointer_cancel(rng.below(4));
            }
            10 => {
                view.wheel(rng.wild(20.0));
            }
            11..=12 => {
                let input = match rng.below(9) {
                    0 => ViewInput::Orbit {
                        dx_px: rng.wild(1.0e4),
                        dy_px: rng.wild(1.0e4),
                    },
                    1 => ViewInput::Pan {
                        dx_px: rng.wild(1.0e4),
                        dy_px: rng.wild(1.0e4),
                    },
                    2 => ViewInput::Pinch {
                        scale: rng.wild(50.0),
                    },
                    3 => ViewInput::Scroll {
                        lines: rng.wild(100.0),
                    },
                    4 => ViewInput::Twist {
                        deg: rng.wild(1000.0),
                    },
                    5 => ViewInput::Release {
                        vx_px_s: rng.wild(1.0e6),
                        vy_px_s: rng.wild(1.0e6),
                    },
                    6 => ViewInput::Grab,
                    7 => ViewInput::DoubleTap,
                    _ => ViewInput::Orbit {
                        dx_px: rng.range(-30.0, 30.0),
                        dy_px: rng.range(-30.0, 30.0),
                    },
                };
                view.controller.input(input);
            }
            13 => {
                let s = ViewState {
                    tilt_deg: [rng.wild(500.0), rng.wild(500.0)],
                    spin_deg: rng.wild(1000.0),
                    pan_mm: [rng.wild(100.0), rng.wild(100.0)],
                    zoom: rng.wild(100.0),
                };
                let immediate = rng.below(2) == 0;
                view.controller.set_view(s, immediate);
            }
            14 => {
                if rng.below(4) == 0 {
                    // A new product: a different range, and a viewport resize.
                    let _ = view.controller.set_range(random_range(&mut rng));
                }
                view.controller.set_viewport(rng.wild(4000.0), rng.wild(4000.0));
            }
            _ => {
                let dt = match rng.below(12) {
                    0 => f32::NAN,
                    1 => -1.0,
                    2 => 10.0,
                    3 => 0.0,
                    _ => rng.range(0.001, 0.05),
                };
                view.update(dt);
            }
        }
        check(&view, pivot, step, seed);
    }
    // However it ended, it settles: a lazy loop goes idle.
    let mut n = 0;
    while view.is_animating() && n < 2000 {
        view.update(1.0 / 60.0);
        n += 1;
    }
    assert!(!view.is_animating(), "seed {seed} never settled");
    check(&view, pivot, steps, seed);
}

#[test]
fn no_input_sequence_leaves_the_view_range() {
    for seed in 0..400u64 {
        fuzz_one(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x5EED, 600);
    }
}

#[test]
fn long_sequences_stay_bounded() {
    for seed in 0..4u64 {
        fuzz_one(0xABCD_0000 + seed, 50_000);
    }
}

/// Hammering one direction for a long time (a stuck finger, a scripted agent) stays at the limit.
#[test]
fn pressing_against_every_limit() {
    for spin in [
        SpinMode::None,
        SpinMode::Rock,
        SpinMode::AboutFrontAxis,
        SpinMode::Free,
    ] {
        let range = ViewRange {
            spin,
            rock_max_deg: (spin == SpinMode::Rock).then_some(50.0),
            ..ViewRange::default()
        };
        let mut view = DragView::new(range, CameraDefaults::default());
        for dir in [(1.0, 0.0), (0.0, 1.0), (-1.0, -1.0), (0.7, -0.3)] {
            for _ in 0..2000 {
                view.controller.input(ViewInput::Orbit {
                    dx_px: dir.0 * 37.0,
                    dy_px: dir.1 * 37.0,
                });
                view.controller.input(ViewInput::Pan {
                    dx_px: dir.0 * 500.0,
                    dy_px: dir.1 * 500.0,
                });
                view.controller.input(ViewInput::Scroll { lines: 5.0 });
                view.update(1.0 / 60.0);
                check(&view, Vec3::ZERO, 0, 0);
            }
        }
    }
}
