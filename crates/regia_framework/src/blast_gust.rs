// SPDX-License-Identifier: MPL-2.0
//! The blast gust on the terrain grass (ADR-1497): a crash's blast front, published by
//! `regia_vfx` as [`BlastGusts`], bends the grass around the camera as it passes.
//!
//! [`BlastGustGrassPlugin`] (added by [`crate::shock_shake::ShockShakePlugin`], or on its own)
//! runs [`bend_grass_to_blast_gusts`] after `regia_vfx::crash::publish_blast_gusts`: it hands the
//! strongest front near the [`VfxView`] camera to the [`TerrainGrassHandle`]
//! (`regia_vfx::crash::grass_gust_for`), and clears the gust once the front's tail is past. The
//! grass shader leans each blade away from the crash behind the front with the blast wind's
//! Friedlander pulse, on the clock the camera shake uses, so the grass at the camera flicks on
//! the shake frame. Nothing happens without a grass handle, a VFX view or the VFX plugin.

use regia_app::prelude::*;
use regia_terrain::grass::{GrassGust, TerrainGrassHandle};
use regia_vfx::VfxView;
use regia_vfx::crash::{BlastGusts, VegetationGust, grass_gust_for};

/// Installs [`bend_grass_to_blast_gusts`].
pub struct BlastGustGrassPlugin;

impl Plugin for BlastGustGrassPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            bend_grass_to_blast_gusts.after(regia_vfx::crash::publish_blast_gusts),
        );
    }
}

/// The grass handle's gust for a vegetation gust.
#[must_use]
pub fn grass_gust(gust: VegetationGust) -> Option<GrassGust> {
    GrassGust {
        center: gust.center,
        radius: gust.radius,
        prev_radius: gust.prev_radius,
        bend: gust.bend,
        length: gust.length,
    }
    .sanitized()
}

/// Sets the terrain grass's gust from the strongest blast front near the camera, or clears it.
pub fn bend_grass_to_blast_gusts(
    gusts: Option<Res<BlastGusts>>,
    view: Option<Res<VfxView>>,
    grass: Option<Res<TerrainGrassHandle>>,
) {
    let Some(grass) = grass else { return };
    let gust = match (gusts, view) {
        (Some(gusts), Some(view)) => grass_gust_for(gusts.fronts(), view.camera.eye).and_then(grass_gust),
        _ => None,
    };
    if grass.gust() != gust {
        grass.set_gust(gust);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_vfx::crash::{CrashBurst, CrashSequence};

    /// The grass takes the front on the frame it detonates and lets go once its tail is past the
    /// camera; the bend is the vfx law's.
    #[test]
    fn the_grass_follows_the_blast_front_and_lets_go() {
        let mut app = App::new();
        app.add_plugins(regia_vfx::VfxPlugin)
            .add_plugins(BlastGustGrassPlugin);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        let eye = Vec3::new(68.6, 0.0, 0.0);
        app.insert_resource(VfxView {
            camera: regia_vfx::VfxCamera::looking_at(eye, Vec3::ZERO),
            ..VfxView::default()
        });
        let grass = TerrainGrassHandle::new_linked();
        app.insert_resource(grass.clone());
        app.update();
        let entity = app
            .world_mut()
            .spawn(CrashSequence::new(CrashBurst::default(), 0.1))
            .id();
        let mut bent = 0;
        let mut released = false;
        for _ in 0..900 {
            app.update();
            let detonated = app
                .world()
                .get::<CrashSequence>(entity)
                .is_some_and(CrashSequence::detonated);
            match grass.gust() {
                Some(g) => {
                    assert!(detonated, "no gust before the detonation");
                    assert!(g.bend > 0.0 && g.bend <= 0.7 && g.radius >= g.prev_radius);
                    bent += 1;
                }
                None if bent > 0 => {
                    released = true;
                    break;
                }
                None => {}
            }
        }
        assert!(bent > 10, "{bent} frames");
        assert!(released, "the gust clears once the tail is past");
    }

    #[test]
    fn a_missing_view_or_handle_is_a_no_op() {
        let mut app = App::new();
        app.add_plugins(BlastGustGrassPlugin);
        app.update();
        let grass = TerrainGrassHandle::new_linked();
        app.insert_resource(grass.clone());
        app.update();
        assert_eq!(grass.gust(), None);
    }
}
