// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 12.2 / § 18 test 9 (VFX row): the plugin moves world-space VFX state on an
//! `OriginShifted` message, in the fixed step of the rebase, exactly once per message. The pool's
//! own shift is unit-tested in `decal.rs`; this proves the plugin wiring that calls it.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::floating_origin::Rebase;
use regia_scene::streaming::OriginShifted;
use regia_vfx::VfxPlugin;
use regia_vfx::decal::{Decal, DecalPool};

fn decal_at(position: Vec3) -> Decal {
    Decal {
        position,
        rotation: Quat::IDENTITY,
        half_size: Vec3::ONE,
        color: [1.0; 4],
        age: 0.0,
        lifetime: None,
    }
}

#[test]
fn an_origin_shift_moves_decals_through_the_plugin_once() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(VfxPlugin);
    let mut pool = DecalPool::new(4);
    pool.spawn(decal_at(Vec3::new(5000.0, 2.0, -7000.0)));
    app.insert_resource(pool);

    // No message: the fixed step leaves positions alone.
    app.world_mut().run_schedule(FixedUpdate);
    assert_eq!(
        app.world().resource::<DecalPool>().decals()[0].position,
        Vec3::new(5000.0, 2.0, -7000.0)
    );

    app.world_mut().write_message(OriginShifted {
        rebase: Rebase {
            cell_delta: [4, 0, -6],
            shift: [4096.0, 0.0, -6144.0],
        },
        origin_cell: [4, 0, -6],
    });
    app.world_mut().run_schedule(FixedUpdate);
    let moved = app.world().resource::<DecalPool>().decals()[0].position;
    assert_eq!(moved, Vec3::new(904.0, 2.0, -856.0), "local = old local - shift");

    // The same message is not applied twice by a later fixed step.
    app.world_mut().run_schedule(FixedUpdate);
    assert_eq!(app.world().resource::<DecalPool>().decals()[0].position, moved);
}
