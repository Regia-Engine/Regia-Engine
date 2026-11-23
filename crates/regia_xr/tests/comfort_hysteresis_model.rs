// SPDX-License-Identifier: MPL-2.0
//! Bounded sequence model for the documented snap-turn latch and cooldown contract.

use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::Pose;
use regia_xr::{ComfortSettings, SnapTurn, rig_turn};

#[derive(Clone, Copy, Debug)]
struct StickSample(f32);

const COOLDOWNS: [u32; 3] = [0, 1, 8];

#[derive(Default)]
struct TableState {
    locked: bool,
    remaining: u32,
    snaps: u64,
}

impl TableState {
    /// Apply one row of the contract table: consume one cooldown frame, then consider rearm/fire.
    fn row(&mut self, settings: &ComfortSettings, sample: StickSample) -> f32 {
        self.remaining = self.remaining.saturating_sub(1);
        let magnitude = sample.0.abs();
        if self.locked {
            if magnitude < settings.snap_rearm {
                self.locked = false;
            }
            return 0.0;
        }
        if magnitude >= settings.snap_threshold && self.remaining == 0 {
            self.locked = true;
            self.remaining = settings.snap_cooldown_frames;
            self.snaps += 1;
            return -sample.0.signum() * settings.snap_turn_deg.to_radians();
        }
        0.0
    }
}

#[test]
fn bounded_stick_histories_match_the_latch_cooldown_table_and_rig_turn() {
    let template = ComfortSettings::default();
    assert!(template.validate().is_ok());
    assert_eq!(template.snap_threshold, 0.6);
    assert_eq!(template.snap_rearm, 0.3);
    assert_eq!(template.snap_cooldown_frames, 8);

    // Exact boundaries and their neighbours, both polarities, plus neutral and full deflection.
    let samples = [
        -1.0, -0.601, -0.6, -0.599, -0.301, -0.3, -0.299, 0.0, 0.299, 0.3, 0.301, 0.599, 0.6, 0.601, 1.0,
    ]
    .map(StickSample);

    for cooldown in COOLDOWNS {
        let settings = ComfortSettings {
            snap_cooldown_frames: cooldown,
            ..template.clone()
        };
        assert!(settings.validate().is_ok());
        for length in 1..=4 {
            let trace_count = samples.len().pow(length as u32);
            for mut code in 0..trace_count {
                let mut trace = Vec::with_capacity(length);
                for _ in 0..length {
                    trace.push(samples[code % samples.len()]);
                    code /= samples.len();
                }
                check_trace(&settings, &trace);
            }
        }
    }

    // Rearm on neutral while cooldown remains; then hold the opposite direction until it fires.
    for cooldown in COOLDOWNS {
        let settings = ComfortSettings {
            snap_cooldown_frames: cooldown,
            ..template.clone()
        };
        let trace = [
            StickSample(0.8),
            StickSample(0.0),
            StickSample(-0.8),
            StickSample(-0.8),
            StickSample(-0.8),
            StickSample(-0.8),
            StickSample(-0.8),
            StickSample(-0.8),
            StickSample(-0.8),
            StickSample(-0.8),
        ];
        check_trace(&settings, &trace);
    }
}

fn check_trace(settings: &ComfortSettings, trace: &[StickSample]) {
    // Each trace starts with fresh state and validated, constant settings.
    assert!(settings.validate().is_ok());
    let mut actual = SnapTurn::default();
    let mut table = TableState::default();
    let initial_rig = Pose::new(
        Vec3::new(2.4, 0.2, -1.3),
        Quat::from_rotation_y(0.43) * Quat::from_rotation_x(-0.12),
    );
    let head = Pose::new(
        Vec3::new(0.35, 1.65, -0.42),
        Quat::from_rotation_y(-0.17) * Quat::from_rotation_x(0.08),
    );
    let fixed_head_position = initial_rig.compose(&head).position;
    let initial_head_orientation = initial_rig.compose(&head).orientation;
    let mut rig = initial_rig;
    let mut composed_yaw = 0.0;

    for (step, sample) in trace.iter().copied().enumerate() {
        let expected = table.row(settings, sample);
        let observed = actual.step(settings, sample.0, 1.0 / 60.0);
        assert!(
            (observed - expected).abs() <= 1.0e-6,
            "turn mismatch at step {step}, input {}: expected {expected}, got {observed}; trace={trace:?}, cooldown={}",
            sample.0,
            settings.snap_cooldown_frames
        );
        assert_eq!(
            actual.snaps(),
            table.snaps,
            "snap counter at step {step}; trace={trace:?}"
        );

        composed_yaw += expected;
        rig = rig_turn(&rig, &head, observed);
        let world_head = rig.compose(&head);
        assert!(
            (world_head.position - fixed_head_position).length() <= 1.0e-5,
            "head world position moved at step {step}: {:?} vs {:?}",
            world_head.position,
            fixed_head_position
        );
        let expected_orientation = Quat::from_rotation_y(composed_yaw) * initial_head_orientation;
        assert!(
            1.0 - world_head.orientation.dot(expected_orientation).abs() <= 2.0e-6,
            "head yaw composition exceeded quaternion dot tolerance at step {step}: trace={trace:?}"
        );
    }
}
