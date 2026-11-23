// SPDX-License-Identifier: MPL-2.0
//! Binding refresh cost (ADR-1811, `docs/specs/81-ui-designer.md` § 9): one `update_bindings` over
//! a HUD with 200 bindings, with an unchanged model and with every value changing each frame.
//! The registry is padded with extra types, as a game's is. Headless, no GPU.
//! Run with `cargo bench -p regia_ui --bench bindings_200`; the budget is 0.1 ms.

use criterion::{Criterion, criterion_group, criterion_main};
use regia_app::prelude::*;
use regia_ecs::prelude::AppTypeRegistry;
use regia_ui::ViewBindings;
use regia_ui::binding::update_bindings;
use regia_ui::prelude::*;

const BINDINGS: usize = 200;

#[derive(Resource, Reflect, Default)]
#[reflect(Resource, Default)]
struct HudVm {
    health: f32,
    ammo: u32,
    name: String,
    dead: bool,
}

/// Registers 182 extra types (13 shapes of 14 base types): a game's registry is far from empty.
fn pad_registry(registry: &AppTypeRegistry) {
    macro_rules! pad {
        ($($t:ty),*) => {$(
            let mut r = registry.write();
            r.register::<$t>();
            r.register::<Option<$t>>();
            r.register::<Vec<$t>>();
            r.register::<($t, $t)>();
            r.register::<($t, u8)>();
            r.register::<($t, u16)>();
            r.register::<($t, u32)>();
            r.register::<($t, f32)>();
            r.register::<($t, bool)>();
            r.register::<($t, String)>();
            r.register::<[$t; 2]>();
            r.register::<[$t; 3]>();
            r.register::<[$t; 4]>();
            drop(r);
        )*};
    }
    pad!(
        u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, bool, String, usize, isize
    );
}

fn world() -> World {
    let mut world = World::new();
    let registry = AppTypeRegistry::default();
    registry.write().register::<HudVm>();
    pad_registry(&registry);
    world.insert_resource(registry);
    world.insert_resource(HudVm::default());
    for i in 0..BINDINGS {
        let (path, format) = match i % 3 {
            0 => ("HudVm.health", Some("{:.1}")),
            1 => ("HudVm.ammo", Some("{:03}")),
            _ => ("HudVm.name", None),
        };
        world.spawn((Text::default(), ViewBindings(vec![Bind::text(path, format)])));
    }
    update_bindings(&mut world);
    world
}

fn unchanged(c: &mut Criterion) {
    let mut world = world();
    c.bench_function("bindings_200_unchanged", |b| {
        b.iter(|| update_bindings(&mut world));
    });
}

fn all_changing(c: &mut Criterion) {
    let mut world = world();
    let mut n = 0u32;
    c.bench_function("bindings_200_all_changing", |b| {
        b.iter(|| {
            n = n.wrapping_add(1);
            let mut vm = world.resource_mut::<HudVm>();
            vm.health = n as f32;
            vm.ammo = n;
            vm.name.clear();
            vm.name.push_str(if n.is_multiple_of(2) { "even" } else { "odd" });
            update_bindings(&mut world);
        });
    });
}

criterion_group!(benches, unchanged, all_changing);
criterion_main!(benches);
