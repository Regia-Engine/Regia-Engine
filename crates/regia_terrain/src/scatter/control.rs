// SPDX-License-Identifier: MPL-2.0
//! The agent-facing scatter switchboard: a [`ScatterControl`] resource the agent protocol reads
//! (`terrain.scatter`) and writes (`terrain.set_scatter`), and the host applies to its
//! [`ScatterField`] / [`ContactMap`] once per frame (`book/agents/terrain-scatter.md`).

use regia_app::prelude::*;
use serde::Serialize;

use super::caster::ScatterCasterHint;
use super::contact::ContactMap;
use super::detail::ScatterDetail;
use super::field::{ScatterField, ScatterFrameStats};
use super::impostor::ImpostorSettings;
use super::season::ScatterSeason;

/// One class as the agent sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScatterClassReport {
    /// Class name from the `.scatter.ron`.
    pub name: String,
    /// Drawn this frame.
    pub enabled: bool,
    /// Instances resident (streamed in around the camera).
    pub resident: u32,
    /// The subsurface profile the class's materials carry (`off` or `foliage`,
    /// [`ScatterField::set_class_subsurface`], ADR-3170).
    pub subsurface: String,
    /// Why the class asks to scatter and cannot, a code of
    /// [`regia_render::SubsurfaceUnsupported`] (`material_graph` for a swaying class); `None` when
    /// it does not ask or can ([`ScatterField::class_subsurface_unsupported`]).
    pub subsurface_unsupported: Option<&'static str>,
    /// The class's shadow casters past a drawn LOD ([`ScatterField::set_class_caster_hint`], ADR-3146).
    pub caster: Option<ScatterCasterHint>,
    /// The class's season ([`ScatterField::set_class_season`], ADR-3146).
    pub season: Option<ScatterSeason>,
}

/// The last frame's scatter, recorded by the host with [`ScatterControl::record`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScatterReport {
    /// A field is installed and recorded at least once.
    pub installed: bool,
    /// Per-class placement.
    pub classes: Vec<ScatterClassReport>,
    /// Instances resident in total.
    pub resident: u32,
    /// Tiles in view and range / total tiles.
    pub tiles_visible: u32,
    /// Tiles in the field.
    pub tiles: u32,
    /// Instances handed to the frame (the GPU then culls per instance: frustum + Hi-Z).
    pub emitted: u32,
    /// Emitted per LOD.
    pub per_lod: Vec<u32>,
    /// Emitted instances shrinking in their fade band.
    pub fading: u32,
    /// The impostor far field is on in the field (ADR-116).
    pub impostors: bool,
    /// Meshes dithered out against their impostor this frame (the hand-over band).
    pub impostor_band: u32,
    /// The detail limits are on in the field (screen-size fade, small casters; [`ScatterDetail`]).
    pub detail: bool,
    /// Triangles emitted into shadow maps last frame ([`ScatterFrameStats::caster_triangles`]).
    pub caster_triangles: u64,
    /// Emitted instances without their leaves last frame ([`ScatterFrameStats::leafless`]).
    pub leafless: u32,
}

/// Requested scatter state plus the last report. Inserted by [`crate::TerrainPlugin`].
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct ScatterControl {
    /// Draw scatter at all.
    pub enabled: bool,
    /// Draw the contact grounding (AO pool + ground tint) under instances.
    pub contact: bool,
    /// Per-class overrides (`name`, `enabled`); classes not listed keep their state.
    pub classes: Vec<(String, bool)>,
    /// Draw far scatter as octahedral impostors where the host bound an impostor layer
    /// ([`ScatterControl::apply_impostors`]); off by default (opt-in, ADR-116).
    pub impostors: bool,
    /// Apply the host's detail limits ([`ScatterControl::apply_detail`]): instances shrink
    /// out by their projected size and tiny stones cast no shadow-map shadow. Off by default
    /// (opt-in: the frame changes).
    pub detail: bool,
    /// Field-wide shadow LOD hint for scatter instances ([`ScatterField::set_shadow_lod_hint`]);
    /// `None` (the default) leaves every instance's own shadow LOD. Applied by
    /// [`ScatterControl::apply`].
    pub shadow_lod_hint: Option<regia_render::ShadowLodHint>,
    /// What the host drew last frame.
    pub report: ScatterReport,
    /// Per-class foliage subsurface overrides (`name`, on): on puts the foliage profile on the
    /// class's materials ([`ScatterField::set_class_subsurface`], ADR-3170), off restores them.
    /// They scatter only under a render profile that asks (`RenderProfile::subsurface_scattering`,
    /// off in every preset). Classes not listed keep their state.
    pub foliage_subsurface: Vec<(String, bool)>,
    /// Per-class caster overrides (`name`, hint; `None` clears it,
    /// [`ScatterField::set_class_caster_hint`], ADR-3146). Classes not listed keep their state.
    pub class_casters: Vec<(String, Option<ScatterCasterHint>)>,
    /// Per-class season overrides (`name`, season; `None` clears it, [`ScatterField::set_class_season`],
    /// ADR-3146). Classes not listed keep their state.
    pub class_seasons: Vec<(String, Option<ScatterSeason>)>,
}

impl Default for ScatterControl {
    fn default() -> Self {
        Self {
            enabled: true,
            contact: true,
            classes: Vec::new(),
            impostors: false,
            detail: false,
            shadow_lod_hint: None,
            report: ScatterReport::default(),
            foliage_subsurface: Vec::new(),
            class_casters: Vec::new(),
            class_seasons: Vec::new(),
        }
    }
}

impl ScatterControl {
    /// Sets one class override (replacing an earlier one for the same name).
    pub fn set_class(&mut self, name: &str, enabled: bool) {
        self.classes.retain(|(n, _)| n != name);
        self.classes.push((name.to_string(), enabled));
    }

    /// Sets one class's foliage subsurface override (replacing an earlier one for the same name).
    pub fn set_class_foliage_subsurface(&mut self, name: &str, on: bool) {
        self.foliage_subsurface.retain(|(n, _)| n != name);
        self.foliage_subsurface.push((name.to_string(), on));
    }

    /// Sets one class's caster hint (replacing an earlier one for the same name).
    pub fn set_class_caster(&mut self, name: &str, hint: Option<ScatterCasterHint>) {
        self.class_casters.retain(|(n, _)| n != name);
        self.class_casters.push((name.to_string(), hint));
    }

    /// Sets one class's season (replacing an earlier one for the same name).
    pub fn set_class_season(&mut self, name: &str, season: Option<ScatterSeason>) {
        self.class_seasons.retain(|(n, _)| n != name);
        self.class_seasons.push((name.to_string(), season));
    }

    /// Pushes the requested state into the host's field and contact map. Unknown class names
    /// are ignored (and reported by [`ScatterControl::unknown_classes`]).
    pub fn apply(&self, field: &mut ScatterField, contact: Option<&mut ContactMap>) {
        field.set_enabled(self.enabled);
        field.set_shadow_lod_hint(self.shadow_lod_hint);
        for (name, enabled) in &self.classes {
            field.set_class_enabled(name, *enabled);
        }
        for (name, on) in &self.foliage_subsurface {
            let factors = if *on {
                regia_render::SubsurfaceFactors::new(regia_render::SubsurfaceProfile::Foliage)
            } else {
                regia_render::SubsurfaceFactors::OFF
            };
            field.set_class_subsurface(name, factors);
        }
        for (name, hint) in &self.class_casters {
            field.set_class_caster_hint(name, *hint);
        }
        for (name, season) in &self.class_seasons {
            field.set_class_season(name, *season);
        }
        if let Some(contact) = contact {
            contact.set_enabled(self.contact);
        }
    }

    /// Pushes the impostor request into the field and the host's layer: on only where the host
    /// has a layer (`Some`), with the host's `settings`. Without a layer the field's far field
    /// stays off — meshes then shrink out as before instead of handing over to nothing.
    pub fn apply_impostors(
        &self,
        field: &mut ScatterField,
        layer: Option<&mut crate::gpu::ImpostorLayer>,
        settings: ImpostorSettings,
    ) {
        match layer {
            Some(layer) => {
                field.set_impostors(self.impostors.then_some(settings));
                layer.set_enabled(self.impostors);
            }
            None => field.set_impostors(None),
        }
    }

    /// Pushes the detail request into the field: the host's `settings` (its output height
    /// and field of view as the reference) when [`Self::detail`] is on, none otherwise.
    pub fn apply_detail(&self, field: &mut ScatterField, settings: ScatterDetail) {
        field.set_detail(self.detail.then_some(settings));
    }

    /// Override names that match no class of `field`.
    #[must_use]
    pub fn unknown_classes(&self, field: &ScatterField) -> Vec<String> {
        let known = field.classes();
        let mut unknown: Vec<String> = Vec::new();
        let names = self
            .classes
            .iter()
            .chain(&self.foliage_subsurface)
            .map(|(name, _)| name)
            .chain(self.class_casters.iter().map(|(name, _)| name))
            .chain(self.class_seasons.iter().map(|(name, _)| name));
        for name in names {
            // A class can be named by several override lists.
            if !known.iter().any(|(k, _)| k == name) && !unknown.contains(name) {
                unknown.push(name.clone());
            }
        }
        unknown
    }

    /// Records the field and this frame's [`ScatterField::collect`] stats for the agent.
    pub fn record(&mut self, field: &ScatterField, stats: ScatterFrameStats) {
        let resident = field.resident_per_class();
        self.report = ScatterReport {
            installed: true,
            classes: field
                .classes()
                .into_iter()
                .zip(resident)
                .map(|((name, enabled), resident)| ScatterClassReport {
                    caster: field.class_caster_hint(&name),
                    season: field.class_season(&name),
                    subsurface: field
                        .class_subsurface(&name)
                        .filter(regia_render::SubsurfaceFactors::is_on)
                        .map_or("off", |f| f.profile.name())
                        .to_owned(),
                    subsurface_unsupported: field
                        .class_subsurface_unsupported(&name)
                        .map(regia_render::SubsurfaceUnsupported::code),
                    name,
                    enabled: enabled && field.enabled(),
                    resident,
                })
                .collect(),
            resident: stats.resident,
            tiles_visible: stats.tiles_visible,
            tiles: stats.tiles,
            emitted: stats.emitted,
            per_lod: stats.per_lod.to_vec(),
            fading: stats.fading,
            impostors: field.impostors().is_some(),
            impostor_band: stats.impostor_band,
            detail: field.detail().is_some(),
            caster_triangles: stats.caster_triangles,
            leafless: stats.leafless,
        };
    }
}
