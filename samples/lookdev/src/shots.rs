// SPDX-License-Identifier: MIT-0
//! Fixed shot list: eight cameras per planet, three quality profiles.
//!
//! Cameras are world-space and do not depend on the framebuffer. Profiles map onto
//! [`regia_render::RenderProfilePreset`]: Ultra, High, and Deck (called Deck-Medium
//! here — the spec has one Deck row, between Mobile High and Desktop Low).

use regia_render::{RenderProfile, RenderProfilePreset, horizontal_plus_fov_y};
use serde::{Deserialize, Serialize};

use crate::scene::{Climate, Scene};
use crate::{LookdevConfig, Planet};

/// The eight look-dev cameras.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraKind {
    /// One metre above the surface, looking along the road.
    Close1m,
    /// Fifty metres up, looking back at the same ground.
    Mid50m,
    /// A two-kilometre vista.
    Far2km,
    /// Low sun across the ground.
    LowSun,
    /// Sun near the zenith, looking down from fifty metres.
    Noon,
    /// Just outside the tunnel mouth, looking in.
    TunnelMouth,
    /// Eye height on the road bend.
    RoadBend,
    /// Pitched up so the sky fills most of the frame.
    SkyHeavy,
}

impl CameraKind {
    /// Every camera, in the order the shot list renders.
    pub const ALL: [CameraKind; 8] = [
        Self::Close1m,
        Self::Mid50m,
        Self::Far2km,
        Self::LowSun,
        Self::Noon,
        Self::TunnelMouth,
        Self::RoadBend,
        Self::SkyHeavy,
    ];

    /// File and JSON name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Close1m => "close_1m",
            Self::Mid50m => "mid_50m",
            Self::Far2km => "far_2km",
            Self::LowSun => "low_sun",
            Self::Noon => "noon",
            Self::TunnelMouth => "tunnel_mouth",
            Self::RoadBend => "road_bend",
            Self::SkyHeavy => "sky_heavy",
        }
    }
}

/// Quality profile for one shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileKind {
    /// [`RenderProfilePreset::DesktopUltra`].
    Ultra,
    /// [`RenderProfilePreset::DesktopHigh`].
    High,
    /// [`RenderProfilePreset::Deck`], the medium tier in the profile table.
    DeckMedium,
}

impl ProfileKind {
    /// Ultra, High, then Deck-Medium.
    pub const ALL: [ProfileKind; 3] = [Self::Ultra, Self::High, Self::DeckMedium];

    /// `ultra`, `high`, or `deck-medium`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Ultra => "ultra",
            Self::High => "high",
            Self::DeckMedium => "deck-medium",
        }
    }

    /// The engine preset this look-dev name selects.
    #[must_use]
    pub fn preset(self) -> RenderProfilePreset {
        match self {
            Self::Ultra => RenderProfilePreset::DesktopUltra,
            Self::High => RenderProfilePreset::DesktopHigh,
            Self::DeckMedium => RenderProfilePreset::Deck,
        }
    }

    /// CDLOD `range_factor` (higher keeps fine nodes out to a greater distance).
    #[must_use]
    pub fn range_factor(self) -> f32 {
        match self {
            Self::Ultra => 5.0,
            Self::High => 3.5,
            Self::DeckMedium => 2.5,
        }
    }

    /// Multiplier on the planet fog density.
    #[must_use]
    pub fn fog_scale(self) -> f32 {
        match self {
            Self::Ultra => 1.0,
            Self::High => 0.85,
            Self::DeckMedium => 0.5,
        }
    }

    /// Fraction of [`LookdevConfig::scatter_cap`] this profile uploads.
    #[must_use]
    pub fn scatter_scale(self) -> f32 {
        match self {
            Self::Ultra => 1.0,
            Self::High => 0.7,
            Self::DeckMedium => 0.35,
        }
    }

    /// Scatter fade distances, metres `(start, end)`.
    #[must_use]
    pub fn fade(self) -> (f32, f32) {
        match self {
            Self::Ultra => (320.0, 780.0),
            Self::High => (180.0, 420.0),
            Self::DeckMedium => (70.0, 180.0),
        }
    }

    /// Wetness multiplier (Deck is drier / cheaper).
    #[must_use]
    pub fn wetness_scale(self) -> f32 {
        match self {
            Self::DeckMedium => 0.5,
            _ => 1.0,
        }
    }
}

/// One entry in the shot list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShotDesc {
    /// Planet stand-in.
    pub planet: Planet,
    /// Camera.
    pub camera: CameraKind,
    /// Quality profile.
    pub profile: ProfileKind,
}

impl ShotDesc {
    /// `europa/close_1m/ultra`.
    #[must_use]
    pub fn id(self) -> String {
        format!(
            "{}/{}/{}",
            self.planet.name(),
            self.camera.name(),
            self.profile.name()
        )
    }

    /// `europa_close_1m_ultra`, safe as a file name.
    #[must_use]
    pub fn file_stem(self) -> String {
        format!(
            "{}_{}_{}",
            self.planet.name(),
            self.camera.name(),
            self.profile.name()
        )
    }
}

/// Every camera and profile for `planets`.
#[must_use]
pub fn all_shots(planets: &[Planet]) -> Vec<ShotDesc> {
    let mut shots = Vec::with_capacity(planets.len() * CameraKind::ALL.len() * ProfileKind::ALL.len());
    for planet in planets {
        for camera in CameraKind::ALL {
            for profile in ProfileKind::ALL {
                shots.push(ShotDesc {
                    planet: *planet,
                    camera,
                    profile,
                });
            }
        }
    }
    shots
}

/// Eye and target for one camera, in metres.
#[derive(Clone, Copy, Debug)]
pub struct LookCamera {
    /// World-space eye.
    pub position: [f32; 3],
    /// World-space look target.
    pub target: [f32; 3],
    /// Vertical field of view, radians, before horizontal-plus adjustment.
    pub fov_y: f32,
}

/// Places `kind` on `scene`.
#[must_use]
pub fn look_camera(scene: &Scene, kind: CameraKind) -> LookCamera {
    let height = |xz: [f32; 2]| scene.height_at(xz[0], xz[1]);
    let along = scene.road.bend_tangent;
    match kind {
        CameraKind::Close1m => {
            let xz = scene.landmarks.close;
            let h = height(xz);
            LookCamera {
                position: [xz[0], h + 1.0, xz[1]],
                target: [xz[0] + along[0] * 4.0, h + 0.25, xz[1] + along[1] * 4.0],
                fov_y: 0.85,
            }
        }
        CameraKind::Mid50m => {
            let xz = scene.landmarks.close;
            let h = height(xz);
            LookCamera {
                position: [xz[0] - along[0] * 18.0, h + 50.0, xz[1] - along[1] * 18.0],
                target: [xz[0] + along[0] * 20.0, h + 1.0, xz[1] + along[1] * 20.0],
                fov_y: 0.90,
            }
        }
        CameraKind::Far2km => {
            let eye = scene.landmarks.vista_eye;
            let target = scene.landmarks.vista_target;
            LookCamera {
                position: [eye[0], height(eye) + 80.0, eye[1]],
                target: [target[0], height(target) + 15.0, target[1]],
                fov_y: 0.85,
            }
        }
        CameraKind::LowSun => {
            let xz = scene.landmarks.low_sun;
            let h = height(xz);
            let at = scene.landmarks.close;
            LookCamera {
                position: [xz[0], h + 5.0, xz[1]],
                target: [at[0], height(at) + 1.2, at[1]],
                fov_y: 0.90,
            }
        }
        CameraKind::Noon => {
            let xz = scene.landmarks.close;
            let h = height(xz);
            LookCamera {
                position: [xz[0] + 4.0, h + 50.0, xz[1] - 6.0],
                target: [xz[0], h + 0.5, xz[1]],
                fov_y: 0.95,
            }
        }
        CameraKind::TunnelMouth => {
            // Back along the road, aimed at the middle of the portal the harness draws.
            let end = scene.road.end;
            let t = scene.road.end_tangent;
            let back = 28.0;
            let x = end[0] - t[0] * back;
            let z = end[1] - t[1] * back;
            let floor = height(end);
            LookCamera {
                position: [x, height([x, z]) + 2.0, z],
                target: [end[0] + t[0] * 2.0, floor + 4.2, end[1] + t[1] * 2.0],
                fov_y: 0.72,
            }
        }
        CameraKind::RoadBend => {
            // Eye height on the centerline, looking toward one bank so the trench
            // wall crosses the frame instead of running out to a flat horizon.
            let bend = scene.road.bend;
            let t = scene.road.bend_tangent;
            let side = [-t[1], t[0]];
            let ahead = [
                bend[0] + t[0] * 16.0 + side[0] * 4.5,
                bend[1] + t[1] * 16.0 + side[1] * 4.5,
            ];
            LookCamera {
                position: [bend[0], height(bend) + 1.65, bend[1]],
                target: [ahead[0], height(ahead) + 0.35, ahead[1]],
                fov_y: 1.0,
            }
        }
        CameraKind::SkyHeavy => {
            let xz = scene.landmarks.sky;
            let h = height(xz);
            LookCamera {
                position: [xz[0], h + 1.8, xz[1]],
                target: [xz[0] + 6.0, h + 45.0, xz[1] + 4.0],
                fov_y: 1.05,
            }
        }
    }
}

/// Sun for this camera. Low-sun and noon override the planet default.
#[derive(Clone, Copy, Debug)]
pub struct ShotSun {
    /// Azimuth, radians.
    pub azimuth: f32,
    /// Elevation, radians.
    pub elevation: f32,
    /// Linear sun colour.
    pub color: [f32; 3],
    /// Illuminance, lux.
    pub lux: f32,
    /// Ambient added by the terrain shader.
    pub ambient: [f32; 3],
}

/// Lighting for `kind` on this planet.
///
/// The terrain shader exposes with EV100 14.5 and a π-normalised Lambert term, then
/// AgX. That curve clips display white once the linear lit value passes about 0.85,
/// and snow albedo is ~0.87, so a textbook 110 klx noon (plus the planet's sky
/// ambient) maps Europa to a flat white frame. Noon here is still a high sun, at a
/// lux that leaves snow under the shoulder. Low sun stays low (about 26°), with
/// enough elevation and fill that a flat foreground is not the black toe of AgX.
#[must_use]
pub fn shot_sun(climate: &Climate, kind: CameraKind) -> ShotSun {
    match kind {
        CameraKind::LowSun => ShotSun {
            azimuth: climate.sun_azimuth + 0.9,
            elevation: 0.46,
            color: [1.0, 0.70, 0.42],
            lux: 70_000.0,
            ambient: [0.34, 0.28, 0.22],
        },
        CameraKind::Noon => ShotSun {
            azimuth: climate.sun_azimuth,
            elevation: 1.15,
            color: [1.0, 0.98, 0.95],
            lux: 46_000.0,
            ambient: [0.16, 0.17, 0.20],
        },
        _ => ShotSun {
            azimuth: climate.sun_azimuth,
            elevation: climate.sun_elevation,
            color: climate.sun_color,
            lux: climate.sun_lux,
            ambient: climate.ambient,
        },
    }
}

/// Framebuffer size after the profile's render scale, at least 2×2.
#[must_use]
pub fn framebuffer_size(config: &LookdevConfig, profile: ProfileKind) -> (u32, u32) {
    let scaled = RenderProfile::from_preset(profile.preset()).scaled_extent(config.width, config.height);
    (scaled.0.max(2), scaled.1.max(2))
}

/// Vertical FOV after the profile's horizontal-plus rule.
#[must_use]
pub fn fov_y(profile: ProfileKind, authored: f32, aspect: f32) -> f32 {
    let preset = RenderProfile::from_preset(profile.preset());
    if preset.horizontal_plus {
        horizontal_plus_fov_y(authored, aspect)
    } else {
        authored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shot_list_is_two_planets_eight_cameras_three_profiles() {
        let shots = all_shots(Planet::both());
        assert_eq!(shots.len(), 48);
        let mut ids: Vec<_> = shots.iter().map(|s| s.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 48);
        assert!(ids.iter().any(|id| id == "europa/tunnel_mouth/deck-medium"));
        assert!(ids.iter().any(|id| id == "mars/far_2km/ultra"));
    }

    #[test]
    fn deck_medium_uses_the_deck_preset_and_a_smaller_framebuffer() {
        assert_eq!(ProfileKind::DeckMedium.preset(), RenderProfilePreset::Deck);
        let config = LookdevConfig::baseline();
        let (w, h) = framebuffer_size(&config, ProfileKind::DeckMedium);
        assert!(w < config.width);
        assert!(h < config.height);
        let (uw, uh) = framebuffer_size(&config, ProfileKind::Ultra);
        assert_eq!((uw, uh), (config.width, config.height));
    }

    #[test]
    fn noon_and_low_sun_stay_off_the_agx_clip_and_the_black_toe() {
        use crate::scene::Climate;
        let climate = Climate {
            fog_color: [0.6, 0.7, 0.8],
            fog_density: 0.0,
            fog_falloff: 0.0,
            wetness: 0.0,
            ambient: [0.42, 0.50, 0.62],
            sun_azimuth: 0.7,
            sun_elevation: 0.55,
            sun_color: [1.0, 1.0, 1.0],
            sun_lux: 80_000.0,
            base_height: 0.0,
        };
        let noon = shot_sun(&climate, CameraKind::Noon);
        assert!(noon.lux < 60_000.0, "noon lux {}", noon.lux);
        assert!(noon.elevation > 1.0, "noon should still be a high sun");
        assert!(noon.ambient[0] < 0.25 && noon.ambient[2] < 0.30);
        let low = shot_sun(&climate, CameraKind::LowSun);
        assert!(
            low.elevation > 0.35 && low.elevation < 0.7,
            "elevation {}",
            low.elevation
        );
        assert!(low.ambient[1] > 0.2, "low-sun fill {}", low.ambient[1]);
        assert!(low.lux > 40_000.0);
    }
}
