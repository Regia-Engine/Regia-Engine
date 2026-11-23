// SPDX-License-Identifier: MPL-2.0
//! The DO27 lighting evidence scene: one heightfield island, scatter that casts a sun
//! shadow, distance/height fog, and three sun angles.
//!
//! `samples/terrain_lab` and the lavapipe golden both call [`render_gap_shot`], so the
//! picture under `docs/evidence/do27-gaps/` is the frame the test compares.

use regia_asset::Heightfield;
use regia_render::{FogSettings, RenderDevice};

use crate::TerrainError;
use crate::cdlod::{CdlodConfig, MinMaxPyramid, select_nodes};
use crate::deform::{Crater, apply_crater};
use crate::gpu::{TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, render_offscreen};
use crate::holes::{HoleEllipse, HolesMask};
use crate::layers::{LayerDesc, TerrainLayers};
use crate::sampler::TerrainMap;
use crate::water::WaterPlane;

/// Which sun the evidence frame uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapSun {
    /// Low sun from the east, warm and dimmer.
    Morning,
    /// Default high sun.
    Noon,
    /// Low sun from the west.
    Evening,
}

/// What [`render_gap_shot`] draws. The evidence defaults are all on except fog,
/// which the caller sets.
#[derive(Clone, Copy, Debug)]
pub struct GapShot {
    /// Sun angle.
    pub sun: GapSun,
    /// Outdoor haze when true; fog off when false.
    pub fog: bool,
    /// Shoreline water plane.
    pub water: bool,
    /// Scatter rocks (they cast the sun cascade).
    pub scatter: bool,
    /// Elliptical hole in the heightfield.
    pub hole: bool,
    /// A crater stamped into the heightfield.
    pub crater: bool,
}

impl Default for GapShot {
    fn default() -> Self {
        Self {
            sun: GapSun::Noon,
            fog: false,
            water: true,
            scatter: true,
            hole: true,
            crater: true,
        }
    }
}

impl GapSun {
    fn time_of_day(self) -> TimeOfDay {
        match self {
            Self::Morning => TimeOfDay {
                azimuth_radians: 0.6,
                elevation_radians: 0.35,
                sun_color: [1.35, 1.05, 0.75],
                illuminance_lux: 55_000.0,
                ambient: [0.35, 0.40, 0.55],
            },
            Self::Evening => TimeOfDay {
                azimuth_radians: -2.4,
                elevation_radians: 0.28,
                sun_color: [1.45, 0.95, 0.65],
                illuminance_lux: 40_000.0,
                ambient: [0.30, 0.32, 0.48],
            },
            Self::Noon => TimeOfDay::default(),
        }
    }
}

/// The island used by the DO27 evidence frames: 257² samples over 256 m.
fn island_map() -> Result<(TerrainMap, TerrainLayers), TerrainError> {
    let n = 257u32;
    let width = 256.0;
    let mut samples = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
            let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
            let r = (x * x + z * z).sqrt();
            let h = 62.0 * (-(r / 82.0).powi(2)).exp() - 8.0 + 2.6 * (x * 0.09).sin() * (z * 0.075).cos();
            samples.push(((h + 64.0) / 128.0 * 65535.0).round().clamp(0.0, 65535.0) as u16);
        }
    }
    let map = TerrainMap::new(
        &Heightfield {
            width: n,
            height: n,
            samples,
        },
        width,
        128.0,
        -64.0,
    )?;
    let layers = TerrainLayers {
        layers: vec![
            LayerDesc {
                name: "sand".into(),
                albedo: [0.42, 0.37, 0.26],
                roughness: 0.95,
                tiling: 6.0,
                slope_min: 0.0,
                slope_max: 0.5,
                altitude_min: -60.0,
                altitude_max: 5.5,
                blend_sharpness: 0.15,
                triplanar: false,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "grass".into(),
                albedo: [0.13, 0.28, 0.08],
                roughness: 0.95,
                tiling: 8.0,
                slope_min: 0.0,
                slope_max: 0.62,
                altitude_min: -60.0,
                altitude_max: 38.0,
                blend_sharpness: 0.2,
                triplanar: false,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "dirt".into(),
                albedo: [0.32, 0.22, 0.13],
                roughness: 0.95,
                tiling: 7.0,
                slope_min: 0.0,
                slope_max: 0.5,
                altitude_min: 6.0,
                altitude_max: 40.0,
                blend_sharpness: 0.25,
                triplanar: false,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "rock".into(),
                albedo: [0.40, 0.38, 0.36],
                roughness: 0.85,
                tiling: 4.0,
                slope_min: 0.42,
                slope_max: 1.6,
                altitude_min: -60.0,
                altitude_max: 90.0,
                blend_sharpness: 0.12,
                triplanar: true,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: "snow".into(),
                albedo: [0.44, 0.44, 0.52],
                roughness: 0.8,
                tiling: 5.0,
                slope_min: 0.0,
                slope_max: 0.85,
                altitude_min: 32.0,
                altitude_max: 90.0,
                blend_sharpness: 0.18,
                triplanar: false,
                ..LayerDesc::ground()
            },
        ],
        ..Default::default()
    };
    Ok((map, layers))
}

/// Renders the DO27 evidence heightfield at `width`×`height` and returns tightly packed RGBA8.
///
/// # Errors
///
/// Returns [`TerrainError`] when the map or the GPU device rejects the frame.
#[tracing::instrument(skip(device), target = "regia_terrain")]
pub fn render_gap_shot(
    device: &RenderDevice,
    shot: &GapShot,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, TerrainError> {
    let (mut map, layers) = island_map()?;
    let config = CdlodConfig::with_grid_size(16);
    let root_level = 4u32;
    let mask = if shot.hole {
        Some(HolesMask::new(vec![HoleEllipse {
            center: [8.0, -12.0],
            radius: [18.0, 14.0],
            rotation_radians: 0.3,
        }])?)
    } else {
        None
    };
    let water = if shot.water {
        Some(WaterPlane::default())
    } else {
        None
    };
    let mut gpu = TerrainGpu::new(
        device,
        &map,
        &layers,
        &config,
        root_level,
        mask.as_ref(),
        water.as_ref(),
    )?;
    if shot.crater {
        let dirty = apply_crater(
            &mut map,
            &Crater {
                center: [15.0, 5.0],
                radius: 14.0,
                depth: 8.0,
                rim_height: 1.5,
            },
        )?;
        gpu.update_heights(&map, &dirty);
    }
    if shot.scatter {
        let rule = crate::ScatterRule {
            cell_size: 10.0,
            instances_per_cell: 1.5,
            min_scale: 2.2,
            max_scale: 5.0,
            height_min: 1.0,
            fade_start_distance: 400.0,
            fade_end_distance: 900.0,
            ..Default::default()
        };
        let instances = crate::place(&map, mask.as_ref(), Some(&layers), &rule);
        gpu.set_scatter(&instances, rule.fade_start_distance, rule.fade_end_distance, 0.2);
    }
    let camera = TerrainCamera {
        position: [0.0, 90.0, 175.0],
        target: [0.0, 12.0, 0.0],
        fov_y_radians: 1.0,
        aspect: width as f32 / height.max(1) as f32,
        near: 0.5,
        far: 3000.0,
    };
    let tod = shot.sun.time_of_day();
    let mut frame = TerrainFrame::from_camera(&camera, &tod, 0.15, 2.3);
    frame.fog = if shot.fog {
        FogSettings::outdoor_haze()
    } else {
        FogSettings::default()
    };
    let pyramid = MinMaxPyramid::build(&map, root_level)?;
    let selection = select_nodes(&map, &pyramid, &config, root_level, [0.0, 175.0], None);
    render_offscreen(
        &mut gpu,
        device,
        &frame,
        &selection,
        width.max(1),
        height.max(1),
        [0.02, 0.02, 0.05, 1.0],
    )
}

#[cfg(test)]
mod tests {
    use super::GapSun;

    /// Morning is east (+X) and evening is west (−X). Azimuth 0 is +Z and grows toward +X.
    #[test]
    fn morning_sun_is_east_and_evening_sun_is_west() {
        let morning = GapSun::Morning.time_of_day().sun_direction();
        let evening = GapSun::Evening.time_of_day().sun_direction();
        assert!(morning[0] > 0.0, "morning {morning:?}");
        assert!(evening[0] < 0.0, "evening {evening:?}");
        assert!(
            morning[1] > 0.0 && evening[1] > 0.0,
            "both stay above the horizon"
        );
    }
}
