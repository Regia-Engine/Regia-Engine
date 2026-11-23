// SPDX-License-Identifier: MPL-2.0
//! `regia_app` does not depend on bevy crates: this proves `#[derive(Component, Resource,
//! Reflect)]` and `#[reflect(...)]` work with only the Regia Engine prelude in scope (specs/10 § 2.1).

use regia_app::prelude::*;
use regia_ecs::attrs::{Range, Units};
use regia_ecs::serialize::{from_ron_as, to_ron};

/// A component declared in a crate that only knows Regia Engine.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default, PartialEq)]
struct Speed {
    #[reflect(@Range(0.0..=10.0), @Units("m/s"))]
    value: f32,
}

#[derive(Resource, Reflect, Default)]
#[reflect(Resource, Default)]
struct Settings {
    volume: f32,
}

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
struct MySet;

#[test]
fn derives_and_reflection_work_through_the_facade() {
    let mut app = App::new();
    app.register_type::<Speed>().register_type::<Settings>();
    app.add_systems(
        Update,
        (|mut q: Query<&mut Speed>| {
            for mut s in &mut q {
                s.value += 1.0;
            }
        })
        .in_set(MySet),
    );
    app.world_mut().spawn(Speed { value: 1.0 });
    app.update();

    let registry = app.type_registry().read();
    let registration = registry.get(std::any::TypeId::of::<Speed>());
    assert!(registration.is_some_and(|r| r.data::<ReflectComponent>().is_some()));
    let text = to_ron(&Speed { value: 2.5 }, &registry).unwrap_or_default();
    assert_eq!(
        from_ron_as::<Speed>(&text, &registry).ok(),
        Some(Speed { value: 2.5 })
    );
    let attrs = regia_ecs::describe::field_attrs::<Speed>("value").unwrap_or_default();
    assert_eq!(attrs.range, Some(0.0..=10.0));
    let _ = Units("m");
    let _ = Range::new(0.0, 1.0);
}
