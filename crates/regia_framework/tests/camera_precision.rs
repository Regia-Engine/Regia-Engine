// SPDX-License-Identifier: MPL-2.0
//! The s4 showcase camera through the whole camera pipeline (ADR-1488): shot sampling →
//! camera `Transform` / `GlobalTransform` → `extract_frame` → `ExtractedFrame::frame_scene` → the
//! view matrix every pass builds from it (`view_math::look_at_view`, as `FrameRenderer` does).
//!
//! The shot: a centripetal Catmull-Rom truck of ~1.5 m/s, 2 km from the world origin, looking at
//! a point 3.3 km away, 20° vertical field of view at 3840×2160. Its rendered view advanced in
//! single-frame steps of 1.8e-4 rad (1.1 px) every 42.6 frames, with 8e-5 rad (0.5 px) steps
//! every 23.5 frames between them, while the sampled rotation was smooth: the renderer kept the
//! view as a look-at point one unit ahead of the eye, and that point's `f32` rounding quantized
//! the direction to the coordinate grid at 2 km (1.2e-4 in x, 2.4e-4 in z).
//!
//! Set `REGIA_CAMERA_PRECISION_CSV` to a file path to get the per-frame table (eye, yaw, pitch,
//! turn) the test checks.

use std::fmt::Write as _;

use regia_core::glam::{DVec3, Vec3};
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_framework::shots::{
    CameraPath, Ease, LookAt, PathInterp, PathKey, SHOT_SEQUENCE_FORMAT_VERSION, ScalarKey, Shot, ShotCamera,
    ShotSequence, Track,
};
use regia_render::view_math::look_at_view;
use regia_render::{Camera, ExtractedFrame, extract_frame};
use regia_scene::{GlobalTransform, Transform};

/// The s4 output size.
const WIDTH: u32 = 3840;
const HEIGHT: u32 = 2160;
/// The s4 lens, degrees (vertical).
const FOV_Y_DEGREES: f32 = 20.0;
/// Frames of the s4 render (26 s at 60 fps).
const FRAMES: u32 = 1560;

/// The s4 shot of the look-4 showcase plan, as `camera.set_path` built it.
fn s4_sequence() -> ShotSequence {
    let keys = [
        (0.0, [-1752.0, 353.2, -2203.0]),
        (6.5, [-1758.69, 354.7, -2195.57]),
        (13.0, [-1765.38, 356.2, -2188.14]),
        (19.5, [-1772.07, 357.7, -2180.71]),
        (26.0, [-1778.77, 359.2, -2173.28]),
    ]
    .into_iter()
    .map(|(time, position)| PathKey { time, position })
    .collect();
    let sequence = ShotSequence {
        version: SHOT_SEQUENCE_FORMAT_VERSION,
        name: "s4".into(),
        fps: 60.0,
        shots: vec![Shot {
            name: "s4_full".into(),
            duration: 26.0,
            camera: ShotCamera {
                path: CameraPath {
                    interp: PathInterp::CatmullRom,
                    keys,
                    look_at: LookAt::Point([700.0, 530.0, 5.0]),
                },
                fov: Track(vec![ScalarKey {
                    time: 0.0,
                    value: FOV_Y_DEGREES,
                    ease: Ease::Linear,
                }]),
                roll: Track::default(),
                dof: None,
                rotation: Vec::new(),
            },
            fade_in: 0.0,
            fade_out: 0.0,
            fade_color: [0.0; 3],
        }],
    };
    sequence.validate().expect("the s4 sequence is valid");
    sequence
}

/// One frame as the renderer sees it.
struct RenderedView {
    /// The eye the frame renders from.
    eye: Vec3,
    /// The view direction of the view matrix (−row 2 of its rotation).
    forward: Vec3,
}

/// Moves the camera entity to `frame`'s sample, extracts the frame and builds its view matrix.
fn render_view(world: &mut World, camera: Entity, sequence: &ShotSequence, frame: u32) -> RenderedView {
    let sample = sequence.sample_frame(u64::from(frame)).expect("a sample");
    let transform = Transform {
        translation: sample.eye,
        rotation: sample.rotation,
        scale: Vec3::ONE,
    };
    world
        .entity_mut(camera)
        .insert((transform, GlobalTransform(transform.compute_affine())));
    world.run_system_once(extract_frame).expect("extract_frame runs");
    let extracted = world.resource::<ExtractedFrame>();
    let scene = extracted.frame_scene(0, WIDTH, HEIGHT);
    let eye = Vec3::from_array(scene.eye);
    let view = look_at_view(eye, Vec3::from_array(scene.target), Vec3::Y);
    RenderedView {
        eye,
        forward: -Vec3::new(view.x_axis.z, view.y_axis.z, view.z_axis.z),
    }
}

/// Angle between two directions, radians (f64 cross product: exact for small angles).
fn angle(a: Vec3, b: Vec3) -> f64 {
    let (a, b) = (a.as_dvec3().normalize(), b.as_dvec3().normalize());
    a.cross(b).length().atan2(a.dot(b))
}

/// Pixels per radian at the frame centre for the s4 lens and height.
fn pixels_per_radian() -> f64 {
    f64::from(HEIGHT) * 0.5 / (f64::from(FOV_Y_DEGREES).to_radians() * 0.5).tan()
}

/// Writes the per-frame table to `REGIA_CAMERA_PRECISION_CSV` when it is set (tmp + rename).
fn write_table(views: &[RenderedView], turns: &[f64]) {
    let Some(path) = std::env::var_os("REGIA_CAMERA_PRECISION_CSV") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let px = pixels_per_radian();
    let mut csv = String::from("frame,eye_x,eye_y,eye_z,yaw_rad,pitch_rad,turn_rad,turn_px\n");
    for (frame, view) in views.iter().enumerate() {
        let f: DVec3 = view.forward.as_dvec3().normalize();
        let turn = if frame == 0 { 0.0 } else { turns[frame - 1] };
        writeln!(
            csv,
            "{frame},{},{},{},{:.9},{:.9},{:.4e},{:.5}",
            view.eye.x,
            view.eye.y,
            view.eye.z,
            f.x.atan2(-f.z),
            f.y.asin(),
            turn,
            turn * px,
        )
        .expect("format a row");
    }
    let tmp = path.with_extension("csv.tmp");
    std::fs::write(&tmp, csv).expect("write the table");
    std::fs::rename(&tmp, &path).expect("move the table into place");
}

#[test]
fn the_rendered_s4_view_turns_by_an_even_step_every_frame() {
    let sequence = s4_sequence();
    let mut world = World::new();
    world.insert_resource(ExtractedFrame::default());
    let camera = world.spawn((Camera::default(), Transform::default())).id();

    let views: Vec<RenderedView> = (0..FRAMES)
        .map(|frame| render_view(&mut world, camera, &sequence, frame))
        .collect();
    let turns: Vec<f64> = views
        .windows(2)
        .map(|pair| angle(pair[0].forward, pair[1].forward))
        .collect();
    write_table(&views, &turns);

    // The view turns ~7.8e-6 rad (0.048 px) a frame. Frame to frame the turn may change by f32
    // rounding of the rotation and the view matrix (a few 1e-7 rad), never by a coordinate
    // grid step: the old ticks changed it by 8e-5 to 1.8e-4 rad (0.5 to 1.1 px).
    let px = pixels_per_radian();
    let mut worst = (0usize, 0.0f64);
    for (i, pair) in turns.windows(2).enumerate() {
        let change = (pair[1] - pair[0]).abs();
        if change > worst.1 {
            worst = (i + 2, change);
        }
    }
    assert!(
        worst.1 < 3.0e-6,
        "frame {}: the rendered view's turn changed by {:e} rad ({:.3} px at {WIDTH}×{HEIGHT}, \
         {FOV_Y_DEGREES}°): the view direction is quantized",
        worst.0,
        worst.1,
        worst.1 * px,
    );
    let mean = turns.iter().sum::<f64>() / turns.len() as f64;
    assert!(
        (mean - 7.8e-6).abs() < 0.5e-6,
        "mean turn {mean:e} rad a frame ({:.4} px)",
        mean * px
    );

    // The view looks at the shot's point: the renderer's direction is the sampled one.
    for (frame, view) in views.iter().enumerate().step_by(97) {
        let to_point = Vec3::new(700.0, 530.0, 5.0) - view.eye;
        assert!(
            angle(view.forward, to_point) < 1.0e-6,
            "frame {frame}: the view is {:e} rad off the look-at point",
            angle(view.forward, to_point)
        );
    }

    // The eye is not on a grid either: it moves by the path's ~2.6 cm a frame, give or take the
    // f32 rounding of the spline at 2 km (a few 1e-4 m, measured up to 1.5e-3 m in the step
    // change; at 2 km that is 7e-7 rad of parallax). A snap to any grid of centimetres or more
    // would change the step by the grid size.
    for (frame, triple) in views.windows(3).enumerate() {
        let a = triple[1].eye.as_dvec3() - triple[0].eye.as_dvec3();
        let b = triple[2].eye.as_dvec3() - triple[1].eye.as_dvec3();
        assert!(
            (b - a).length() < 1.0e-2,
            "frame {}: the eye's step changed by {:e} m",
            frame + 2,
            (b - a).length()
        );
    }
}
