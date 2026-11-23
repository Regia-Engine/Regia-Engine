// SPDX-License-Identifier: MPL-2.0
//! A parked or hidden terrain stops drawing (spec 37 § 13, ADR-7200).
//!
//! A game draws its terrain through the renderer's shared draw (`TerrainSharedDraw`, installed
//! by the host with `FrameRenderer::set_shared_draw`), not through a query, so the renderer's
//! entity filter cannot see it. [`withhold_parked_terrain_draw`] closes that gap: while the world
//! has [`Terrain`] entities and none of them draws (each is a parked layer member or resolves
//! hidden), the shared draw is taken off the [`RuntimeRenderer`] and kept in
//! [`WithheldTerrainDraw`]; when one draws again the same draw goes back, with no rebuild. A host
//! that draws a terrain with no `Terrain` entity is left alone, and so is a draw the host
//! replaces while one is withheld (the withheld one is dropped).

use regia_app::prelude::*;
use regia_render::{RuntimeRenderer, SharedPassDraw};
use regia_scene::components::InheritedVisibility;
use regia_scene::layers::LayerParked;

use crate::Terrain;

/// The shared draw a parked or hidden terrain took off the renderer, kept to put back.
#[derive(Resource, Default)]
pub struct WithheldTerrainDraw {
    draw: Option<Box<dyn SharedPassDraw + Send + Sync>>,
    /// Times a draw was taken off (tests, diagnostics).
    pub withheld: u64,
    /// Times a withheld draw went back on.
    pub restored: u64,
}

impl WithheldTerrainDraw {
    /// Whether a draw is held off the renderer now.
    #[must_use]
    pub fn is_withheld(&self) -> bool {
        self.draw.is_some()
    }
}

/// Whether a terrain with these markers draws: not a parked layer member, and not hidden.
#[must_use]
pub fn terrain_drawn(parked: bool, inherited: Option<&InheritedVisibility>) -> bool {
    !parked && regia_render::is_drawn(inherited)
}

/// `Render`, before the views render: withholds the renderer's shared terrain draw while every
/// [`Terrain`] is parked or hidden, and puts it back when one draws (module docs).
pub fn withhold_parked_terrain_draw(
    terrains: Query<(Has<LayerParked>, Option<&InheritedVisibility>), With<Terrain>>,
    runtime: Option<ResMut<RuntimeRenderer>>,
    mut held: ResMut<WithheldTerrainDraw>,
) {
    let Some(mut runtime) = runtime else {
        return;
    };
    let any = !terrains.is_empty();
    let drawn = terrains
        .iter()
        .any(|(parked, inherited)| terrain_drawn(parked, inherited));
    if any && !drawn {
        if runtime.0.has_shared_draw()
            && let Some(draw) = runtime.0.take_shared_draw()
        {
            held.draw = Some(draw);
            held.withheld += 1;
        }
    } else if let Some(draw) = held.draw.take()
        && !runtime.0.has_shared_draw()
    {
        runtime.0.set_shared_draw(Some(draw));
        held.restored += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{FrameRenderer, SharedFrameInfo, SharedPass, SharedUpload};

    use crate::{CdlodConfig, TerrainLayers, TerrainMap};

    /// A shared draw that records nothing (the gate only moves the box).
    struct Nothing;

    impl SharedPassDraw for Nothing {
        fn prepare(&mut self, _info: &SharedFrameInfo, _upload: &mut SharedUpload<'_>) {}
        fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: SharedPass) {}
    }

    fn flat() -> Terrain {
        let n = 33u32;
        let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 128.0, 10.0, 0.0).expect("map");
        Terrain::new(
            &map.to_heightfield(),
            128.0,
            10.0,
            0.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    fn app(terrain: impl Bundle) -> (App, Entity) {
        let mut app = App::new();
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(Nothing)));
        app.insert_resource(RuntimeRenderer(renderer))
            .init_resource::<WithheldTerrainDraw>()
            .add_systems(Render, withhold_parked_terrain_draw);
        let entity = app.world_mut().spawn(terrain).id();
        (app, entity)
    }

    fn drawing(app: &App) -> bool {
        app.world().resource::<RuntimeRenderer>().0.has_shared_draw()
    }

    #[test]
    fn a_parked_terrain_draws_nothing_and_comes_back_when_live() {
        let (mut app, entity) = app(flat());
        app.update();
        assert!(drawing(&app), "a live terrain draws");
        app.world_mut().entity_mut(entity).insert(LayerParked {
            restore: regia_scene::components::Visibility::Inherited,
            inherited: false,
        });
        app.update();
        assert!(!drawing(&app), "a parked terrain draws nothing");
        assert!(app.world().resource::<WithheldTerrainDraw>().is_withheld());
        app.world_mut().entity_mut(entity).remove::<LayerParked>();
        app.update();
        assert!(drawing(&app), "live again: the same draw is back");
        let held = app.world().resource::<WithheldTerrainDraw>();
        assert_eq!((held.withheld, held.restored), (1, 1));
    }

    #[test]
    fn a_hidden_terrain_draws_nothing_and_a_host_draw_without_a_terrain_entity_stays() {
        let (mut app, entity) = app((flat(), InheritedVisibility(false)));
        app.update();
        assert!(!drawing(&app), "a hidden terrain draws nothing");
        app.world_mut().entity_mut(entity).despawn();
        app.update();
        assert!(drawing(&app), "no terrain entity: the host's draw is the host's");
    }
}
