// SPDX-License-Identifier: MPL-2.0
//! The scene-wind-to-sea bridge (ADR-2342, `docs/specs/100-water-extras.md` § 3):
//! [`SeaWeatherPlugin`] copies the scene's surface wind ([`SceneWind`], spec 99) into
//! `regia_terrain`'s [`SeaWind`], so every plane body carrying a
//! [`regia_terrain::water_body::WeatherSea`] gets the swell of the sea-state table for that wind,
//! and a body with an FFT ocean gets that wind on its ocean.
//!
//! ```text
//! Update: sync_sea_wind (SceneWind → SeaWind) → apply_weather_sea (SeaWind → swell / ocean wind)
//! ```
//!
//! [`SceneWind`] is already the wind 10 m above the ground (the weather writes it in
//! `SceneWindMode::Weather`; a host or an agent may hold it in `Manual`), which is the wind the
//! sea-state table is made for, so it is copied unscaled. The sea follows the mean wind: a gust
//! does not raise a fully developed sea.

use regia_app::prelude::*;
use regia_scene::weather::{SceneWind, SceneWindMode};
use regia_terrain::water_body::{SeaWind, apply_weather_sea};

/// Adds [`sync_sea_wind`] before `regia_terrain`'s `apply_weather_sea`. Needs
/// `regia_terrain::WaterPlugin` (which registers the latter). Without a [`SceneWind`], or with
/// it in `SceneWindMode::Off`, the [`SeaWind`] stays as the host set it.
#[derive(Default)]
pub struct SeaWeatherPlugin;

impl Plugin for SeaWeatherPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SeaWind>()
            .add_systems(Update, sync_sea_wind.before(apply_weather_sea));
    }
}

/// The wind the sea follows under `scene`: its mean wind (no gusts), or `None` in
/// `SceneWindMode::Off`.
#[must_use]
pub fn sea_wind_of(scene: &SceneWind) -> Option<[f32; 2]> {
    (scene.mode != SceneWindMode::Off).then_some(scene.mean_m_per_s)
}

/// Writes the scene's mean surface wind into [`SeaWind`] (only when it changed).
pub fn sync_sea_wind(scene: Option<Res<SceneWind>>, mut wind: ResMut<SeaWind>) {
    let Some(next) = scene.as_deref().and_then(sea_wind_of) else {
        return;
    };
    if next.iter().all(|v| v.is_finite()) && wind.wind_m_per_s != next {
        wind.wind_m_per_s = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::water_body::ocean::FftOcean;
    use regia_terrain::water_body::{WaterLook, WeatherSea};
    use regia_terrain::{WaterBody, WaterPlugin};

    fn height(app: &App, e: Entity) -> f32 {
        app.world()
            .get::<WaterBody>(e)
            .and_then(|b| b.swell.as_ref())
            .map_or(0.0, |s| {
                s.waves.iter().map(|w| w.height * w.height).sum::<f32>().sqrt()
            })
    }

    /// A storm wind in the scene raises the sea of a `WeatherSea` plane, a calm one flattens it, a
    /// body without the component is left alone, and the wind is copied unscaled.
    #[test]
    fn scene_wind_drives_the_swell() {
        let mut app = App::new();
        app.add_plugins((WaterPlugin, SeaWeatherPlugin));
        app.insert_resource(SceneWind::manual([3.0, 0.0]));
        let sea = app
            .world_mut()
            .spawn((
                WaterBody::plane([0.0, 0.0, 0.0], [200.0, 200.0], WaterLook::lake()),
                WeatherSea::default(),
            ))
            .id();
        let other = app
            .world_mut()
            .spawn(WaterBody::plane(
                [0.0, 0.0, 0.0],
                [200.0, 200.0],
                WaterLook::lake(),
            ))
            .id();
        app.update();
        assert_eq!(app.world().resource::<SeaWind>().wind_m_per_s, [3.0, 0.0]);
        let light = height(&app, sea);
        *app.world_mut().resource_mut::<SceneWind>() = SceneWind::manual([0.0, 20.0]);
        app.update();
        let storm = height(&app, sea);
        assert!(storm > light + 1.0, "light {light} storm {storm}");
        assert_eq!(height(&app, other), 0.0);
        *app.world_mut().resource_mut::<SceneWind>() = SceneWind::CALM;
        app.update();
        assert_eq!(height(&app, sea), 0.0, "no wind, no sea");
    }

    /// The sea follows the mean wind, not the gusts; `SceneWindMode::Off` leaves the host's
    /// `SeaWind`; without a `SceneWind` nothing is written.
    #[test]
    fn mean_wind_off_mode_and_no_scene_wind() {
        let mut app = App::new();
        app.add_plugins((WaterPlugin, SeaWeatherPlugin));
        app.world_mut().resource_mut::<SeaWind>().wind_m_per_s = [1.5, 2.5];
        app.update();
        assert_eq!(app.world().resource::<SeaWind>().wind_m_per_s, [1.5, 2.5]);
        app.insert_resource(SceneWind {
            mode: SceneWindMode::Weather,
            velocity_m_per_s: [14.0, 0.0],
            mean_m_per_s: [9.0, 0.0],
        });
        app.update();
        assert_eq!(app.world().resource::<SeaWind>().wind_m_per_s, [9.0, 0.0]);
        app.world_mut().resource_mut::<SceneWind>().mode = SceneWindMode::Off;
        app.world_mut().resource_mut::<SeaWind>().wind_m_per_s = [4.0, 4.0];
        app.update();
        assert_eq!(app.world().resource::<SeaWind>().wind_m_per_s, [4.0, 4.0]);
    }

    /// A `WeatherSea` on a body with an FFT ocean sets the ocean's wind (quantized) and leaves its
    /// swell alone; the same body without the component keeps its authored ocean.
    #[test]
    fn scene_wind_drives_an_ocean_body() {
        let mut app = App::new();
        app.add_plugins((WaterPlugin, SeaWeatherPlugin));
        app.insert_resource(SceneWind::manual([0.0, 17.2]));
        let mut body = WaterBody::plane([0.0, 0.0, 0.0], [500.0, 500.0], WaterLook::lake());
        body.ocean = Some(FftOcean::default());
        let driven = app.world_mut().spawn((body.clone(), WeatherSea::default())).id();
        let authored = app.world_mut().spawn(body).id();
        app.update();
        let ocean = |e: Entity| app_ocean(&app, e);
        let o = ocean(driven);
        assert_eq!(o.wind_speed, 17.0);
        assert!(o.wind_direction[0].abs() < 1.0e-6 && (o.wind_direction[1] - 1.0).abs() < 1.0e-6);
        assert!(o.validate().is_ok());
        assert!(
            app.world()
                .get::<WaterBody>(driven)
                .is_some_and(|b| b.swell.is_none())
        );
        assert_eq!(ocean(authored), FftOcean::default());
    }

    fn app_ocean(app: &App, e: Entity) -> FftOcean {
        app.world()
            .get::<WaterBody>(e)
            .and_then(|b| b.ocean)
            .unwrap_or_default()
    }
}
