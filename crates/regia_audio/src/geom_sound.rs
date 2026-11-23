// SPDX-License-Identifier: MPL-2.0
//! Sound over cooked world geometry (plan row W-25, ADR-8416; `docs/specs/16-audio.md` § 3d):
//! [`GeomSoundField`] is a [`RockField`] built from a loaded `<map>.meshlayers`
//! ([`regia_world_geom::WorldGeom`]). Install it on [`crate::AudioRockField`] and:
//!
//! - a source inside a passage is heard from the mouth it leaves by, at the length of the path
//!   through the cooked portals ([`regia_world_geom::portal_sound::PortalSoundMap::path`]);
//! - a source with no portal path is occluded by the rock on the straight line, measured with the
//!   core's segment test (`WorldGeom::rock_between` and the ground segment);
//! - the section the listener stands in arms its baked reverb preset (`tube`, `gallery`, `open`).
//!
//! Every answer is a pure function of the cooked file and the two points: no wall clock, no
//! per-process state.

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_world_geom::portal_sound::{
    PortalSoundMap, PortalSoundSettings, SectionReverb, SoundCell, SoundPath, SoundPathKind,
};
use regia_world_geom::{GeomState, WorldGeom};

use crate::audio_zone::{ArmedZone, FieldRoute, PortalRoute, RockField};
use crate::reverb_zone::{AudioReverbZone, ReverbPreset, ReverbZoneShape};

/// A [`RockField`] over cooked world geometry (see the module docs).
#[derive(Clone)]
pub struct GeomSoundField {
    geom: Arc<WorldGeom>,
    state: GeomState,
    map: PortalSoundMap,
    settings: PortalSoundSettings,
}

impl std::fmt::Debug for GeomSoundField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeomSoundField")
            .field("sections", &self.geom.sections().len())
            .field("portals", &self.map.portal_count())
            .field("settings", &self.settings)
            .finish()
    }
}

fn arr(v: Vec3) -> [f32; 3] {
    [v.x, v.y, v.z]
}

fn vec3(a: [f32; 3]) -> Vec3 {
    Vec3::new(a[0], a[1], a[2])
}

impl GeomSoundField {
    /// The field of `geom` with the default settings and a fresh query state.
    pub fn new(geom: Arc<WorldGeom>) -> Self {
        let state = geom.new_state();
        let map = PortalSoundMap::new(&geom);
        Self {
            geom,
            state,
            map,
            settings: PortalSoundSettings::default(),
        }
    }

    /// The same field with `settings`.
    #[must_use]
    pub fn with_settings(mut self, settings: PortalSoundSettings) -> Self {
        self.settings = settings;
        self
    }

    /// The geometry.
    pub fn geom(&self) -> &WorldGeom {
        &self.geom
    }

    /// The portal graph built from the cooked file.
    pub fn map(&self) -> &PortalSoundMap {
        &self.map
    }

    /// The settings in effect.
    pub fn settings(&self) -> &PortalSoundSettings {
        &self.settings
    }

    /// The cell of `point`.
    pub fn cell_at(&self, point: Vec3) -> SoundCell {
        self.map
            .cell_at(&self.geom, &self.state, &self.settings, arr(point))
    }

    /// The full path from `emitter` to `listener`.
    pub fn path(&self, listener: Vec3, emitter: Vec3) -> SoundPath {
        let _span = tracing::trace_span!("geom_sound_path").entered();
        self.map.path(
            &self.geom,
            &self.state,
            &self.settings,
            arr(emitter),
            arr(listener),
        )
    }

    /// The baked reverb of the section `listener` stands in.
    pub fn section_reverb(&self, listener: Vec3) -> Option<&SectionReverb> {
        match self.cell_at(listener) {
            SoundCell::Section(s) => self.map.section_reverb(s),
            SoundCell::Outdoor | SoundCell::Rock => None,
        }
    }
}

impl RockField for GeomSoundField {
    /// Not used: [`RockField::rock_between`] answers every segment exactly.
    fn surface_height(&self, _x: f32, _z: f32) -> Option<f32> {
        None
    }

    fn rock_between(&self, from: Vec3, to: Vec3) -> Option<f32> {
        Some(
            self.map
                .occlusion_m(&self.geom, &self.state, &self.settings, arr(from), arr(to)),
        )
    }

    fn sound_route(&self, listener: Vec3, emitter: Vec3) -> Option<FieldRoute> {
        let path = self.path(listener, emitter);
        let route = (path.kind == SoundPathKind::Portal).then(|| {
            let mouth = path.points.iter().rev().nth(1).copied().map_or(emitter, vec3);
            PortalRoute {
                mouth,
                position: vec3(path.apparent),
                path_length: path.length_m,
            }
        });
        Some(FieldRoute {
            route,
            range_scale: path.range_scale,
        })
    }

    fn armed_zone(&self, listener: Vec3) -> Option<ArmedZone> {
        let reverb = self.section_reverb(listener)?;
        let preset = ReverbPreset::from_name(&reverb.preset)?;
        let mut zone = AudioReverbZone::with_preset(preset, ReverbZoneShape::Global);
        // A baked section beats a level-wide global reverb; an authored finite zone still wins.
        zone.priority = 1;
        Some(ArmedZone {
            section: u32::from(reverb.section),
            preset_id: reverb.preset.clone(),
            zone,
        })
    }

    fn as_geom(&self) -> Option<&GeomSoundField> {
        Some(self)
    }
}
