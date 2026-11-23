// SPDX-License-Identifier: MIT-0
//! Incremental layout and subtree-cached extract on the dense HUD (lane ui-look): with a bar
//! animating and the timer ticking every frame, the incrementally maintained layout and draw list
//! must equal a forced full relayout and full rebuild after every frame. (This caught layout
//! racing the change-marking system and lagging a frame.)

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use regia::ecs::ecs::system::RunSystemOnce;
use regia::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::{ComputedNode, LayoutDirty, UiDamage, UiDamageSet, UiQuad};
use ui_gallery::hud::{HudClock, HudSample};
use ui_gallery::shots::{headless_app, resize};

fn layouts(app: &mut App) -> HashMap<Entity, ComputedNode> {
    let world = app.world_mut();
    world
        .query::<(Entity, &ComputedNode)>()
        .iter(world)
        .map(|(e, c)| (e, *c))
        .collect()
}

fn quads(app: &App) -> Vec<UiQuad> {
    app.world().resource::<SharedUiRenderer>().lock().quads().to_vec()
}

#[test]
fn incremental_hud_matches_full_rebuild_every_frame() {
    let mut app = headless_app(HudSample);
    app.world_mut().resource_mut::<HudClock>().live = true;
    // 1280×800 (Deck): a fractional UI scale exercises rounding paths too.
    resize(&mut app, 1280, 800, None);
    for frame in 0..60 {
        app.update();
        let incremental_layout = layouts(&mut app);
        let incremental_quads = quads(&app);
        {
            let world = app.world_mut();
            world.resource_mut::<LayoutDirty>().mark();
            regia_ui::run_ui_layout(world).expect("full layout");
            // A frame's layout is followed by the visual transforms (the minimap's blips use them).
            world
                .run_system_once(regia_ui::tween::apply_ui_transforms)
                .expect("transforms");
        }
        let full_layout = layouts(&mut app);
        let moved: Vec<_> = full_layout
            .iter()
            .filter(|(e, c)| incremental_layout.get(e) != Some(c))
            .map(|(e, c)| (e, incremental_layout.get(e).map(|i| i.rect), c.rect))
            .take(4)
            .collect();
        assert!(
            moved.is_empty(),
            "frame {frame}: incremental layout differs: {moved:?}"
        );
        {
            let world = app.world_mut();
            world.resource_mut::<UiDamageSet>().full = true;
            world.resource_mut::<UiDamage>().0 = true;
            regia_ui::extract::extract_ui(world);
        }
        assert_eq!(
            incremental_quads,
            quads(&app),
            "frame {frame}: subtree-cached draw list differs from a full rebuild"
        );
    }
}
