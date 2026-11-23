// SPDX-License-Identifier: MPL-2.0
//! Curve names are not heap-allocated again on the second sample.
//!
//! Sampling a one-curve clip and writing it into the parameter set must reuse the name from
//! the previous step.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{NamedCurve, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::hot_count::{self, curve_name_clones, set_float_allocs};
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_scene::Transform;

fn clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("plant", 1.0);
    clip.curves.push(NamedCurve {
        name: "FootPlant_L".into(),
        curve: TrackCurve::Static(1.0),
    });
    clip
}

#[test]
fn the_second_step_does_not_allocate_the_curve_name() {
    let _guard = hot_count::lock_for_test();
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.update();
    let skeleton = Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
        .expect("skeleton");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, Animator::single(clip(), true)))
        .id();
    app.update();
    let names = curve_name_clones();
    let sets = set_float_allocs();
    app.update();
    let name_delta = curve_name_clones() - names;
    let set_delta = set_float_allocs() - sets;
    assert_eq!(
        name_delta, 0,
        "the second step cloned the curve name {name_delta} times"
    );
    assert_eq!(
        set_delta, 0,
        "the second step allocated a parameter key {set_delta} times"
    );
    let value = app
        .world()
        .get::<Animator>(rig)
        .expect("animator")
        .parameters
        .float("FootPlant_L");
    assert!(
        (value - 1.0).abs() < 1e-6,
        "the curve still writes the parameter ({value})"
    );
}
